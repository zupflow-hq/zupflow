//! Sign-in through an MCP server (decision 0039, after MCP's authorization
//! spec): find the server's authorization server, check its metadata, and
//! register a client with it. Every call goes through [`OutboundHttp`],
//! without a credential.
//!
//! [`discover`] asks the MCP server unauthenticated and reads
//! `resource_metadata` from its `401`'s `WWW-Authenticate`; otherwise, or if
//! that URL answers nothing usable, it reads the protected-resource metadata
//! (RFC 9728) at the well-known path suffixed with the server's path, then at
//! the root. The first `authorization_servers` entry is the issuer. Its
//! metadata (RFC 8414) is read at the path-suffixed well-known path first,
//! then by OpenID Connect discovery. The metadata must name that issuer
//! exactly, use HTTPS for every endpoint, support `S256` and offer a
//! registration endpoint.
//!
//! [`register`] registers a public client (RFC 7591) for the deployment's
//! one redirect URI. If the server issues a secret anyway, the client
//! authenticates with it.
//!
//! Both take a deadline, which the caller sets once for discovery and
//! registration together: each request gets what's left of it, up to its
//! own limit, and one that would run past it is cut off as unavailable.
//!
//! Failures say what was wrong, never what the other side wrote: an
//! unreachable or overloaded server is [`SignInError::Unavailable`], and
//! anything else [`SignInError::Unsupported`].

use std::fmt;
use std::time::Duration;

use serde_json::{Map, Value, json};
use tokio::time::Instant;
use url::Url;
use zeroize::Zeroizing;
use zupflow_core::apps::{AuthorizationServer, ClientAuthentication};
use zupflow_core::outbound::{Limits, Method, OutboundError, OutboundHttp, OutboundRequest, OutboundResponse, Refusal};

#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum SignInError {
    /// No answer, a timeout, a `429` or a `5xx`: worth trying again later.
    #[error("the server is unavailable")]
    Unavailable,
    /// The server offers no sign-in this client can use, with what was wrong.
    #[error("the server offers no usable sign-in: {0}")]
    Unsupported(&'static str),
}

/// Metadata is small: a server that answers more is not answering metadata.
const METADATA_LIMITS: Limits = Limits {
    timeout: Duration::from_secs(10),
    max_response_bytes: 64 << 10,
};

/// How long discovery and registration may take together: well under the
/// minute an idempotency key holds its request.
pub const SIGN_IN_BUDGET: Duration = Duration::from_secs(30);

/// Sends `request` with what's left before `deadline`, up to the metadata
/// limit; past the deadline, it's cut off as timed out.
async fn send(
    http: &dyn OutboundHttp,
    request: OutboundRequest,
    deadline: Instant,
) -> Result<OutboundResponse, OutboundError> {
    let left = deadline.saturating_duration_since(Instant::now());
    if left.is_zero() {
        return Err(OutboundError::TimedOut);
    }
    let limits = Limits {
        timeout: left.min(METADATA_LIMITS.timeout),
        ..METADATA_LIMITS
    };
    tokio::time::timeout_at(deadline, http.send(request, limits))
        .await
        .unwrap_or(Err(OutboundError::TimedOut))
}

/// The longest URL, client ID or issued value kept.
const MAX_URL: usize = 2048;
const MAX_CLIENT_ID: usize = 512;
const MAX_ISSUED: usize = 4096;

/// Finds and checks the authorization server of the MCP server at `server_url`.
pub async fn discover(
    http: &dyn OutboundHttp,
    server_url: &str,
    deadline: Instant,
) -> Result<AuthorizationServer, SignInError> {
    let server = https(server_url).ok_or(SignInError::Unsupported("a server URL that isn't HTTPS"))?;
    let mut candidates = Vec::new();
    if let Some(url) = challenge(http, server_url, deadline).await? {
        candidates.push(url);
    }
    for url in well_known(&server, "oauth-protected-resource") {
        if !candidates.contains(&url) {
            candidates.push(url);
        }
    }
    let mut resource = None;
    for url in &candidates {
        if let Some(metadata) = metadata(http, url, deadline).await? {
            resource = Some(metadata);
            break;
        }
    }
    let resource = resource.ok_or(SignInError::Unsupported("no protected-resource metadata"))?;
    // RFC 9728, section 3.3: the metadata must be for the URL it was
    // fetched for, or it may name another resource's authorization server.
    let named = resource
        .get("resource")
        .and_then(Value::as_str)
        .ok_or(SignInError::Unsupported(
            "protected-resource metadata without its resource",
        ))?;
    if comparable(named) != comparable(server_url) {
        return Err(SignInError::Unsupported("metadata for another resource"));
    }
    let issuer = resource
        .get("authorization_servers")
        .and_then(Value::as_array)
        .and_then(|servers| servers.first())
        .and_then(Value::as_str)
        .ok_or(SignInError::Unsupported("no authorization server named"))?
        .to_owned();
    let issuer_url = https(&issuer).ok_or(SignInError::Unsupported("an authorization server that isn't HTTPS"))?;

    let mut found = None;
    for url in authorization_server_metadata(&issuer_url, &issuer) {
        if let Some(metadata) = metadata(http, &url, deadline).await? {
            found = Some(metadata);
            break;
        }
    }
    let metadata = found.ok_or(SignInError::Unsupported("no authorization server metadata"))?;
    checked(&issuer, &metadata)
}

/// A resource URL as compared with another: its scheme and authority in
/// lowercase and one trailing slash dropped, nothing else normalized.
fn comparable(url: &str) -> String {
    let url = url.strip_suffix('/').unwrap_or(url);
    let Some((scheme, rest)) = url.split_once("://") else {
        return url.to_owned();
    };
    let end = rest.find(['/', '?', '#']).unwrap_or(rest.len());
    let (authority, tail) = rest.split_at(end);
    format!(
        "{}://{}{tail}",
        scheme.to_ascii_lowercase(),
        authority.to_ascii_lowercase()
    )
}

/// The metadata checked as decision 0039 requires.
fn checked(issuer: &str, metadata: &Map<String, Value>) -> Result<AuthorizationServer, SignInError> {
    if metadata.get("issuer").and_then(Value::as_str) != Some(issuer) {
        return Err(SignInError::Unsupported("metadata for another issuer"));
    }
    let endpoint = |key: &str| {
        metadata
            .get(key)
            .and_then(Value::as_str)
            .filter(|value| https(value).is_some())
            .map(str::to_owned)
    };
    let authorization_endpoint =
        endpoint("authorization_endpoint").ok_or(SignInError::Unsupported("no HTTPS authorization endpoint"))?;
    let token_endpoint = endpoint("token_endpoint").ok_or(SignInError::Unsupported("no HTTPS token endpoint"))?;
    let registration_endpoint = match metadata.get("registration_endpoint") {
        None | Some(Value::Null) => return Err(SignInError::Unsupported("no client registration")),
        Some(_) => {
            endpoint("registration_endpoint").ok_or(SignInError::Unsupported("no HTTPS registration endpoint"))?
        }
    };
    let s256 = metadata
        .get("code_challenge_methods_supported")
        .and_then(Value::as_array)
        .is_some_and(|methods| methods.iter().any(|method| method.as_str() == Some("S256")));
    if !s256 {
        return Err(SignInError::Unsupported("no S256 code challenge"));
    }
    Ok(AuthorizationServer {
        issuer: issuer.to_owned(),
        response_iss_supported: metadata
            .get("authorization_response_iss_parameter_supported")
            .and_then(Value::as_bool)
            .unwrap_or(false),
        authorization_endpoint,
        token_endpoint,
        registration_endpoint,
    })
}

/// What [`register`] was issued. The secret and the registration access
/// token are wiped on drop and never printed.
pub struct Registered {
    pub client_id: String,
    pub client_secret: Option<Zeroizing<String>>,
    pub registration_access_token: Option<Zeroizing<String>>,
    pub authentication: ClientAuthentication,
}

impl fmt::Debug for Registered {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Registered")
            .field("client_id", &self.client_id)
            .field("authentication", &self.authentication)
            .finish_non_exhaustive()
    }
}

/// The name a registered client shows on the server's consent screen.
pub const CLIENT_NAME: &str = "Zupflow";

/// Registers a public client with `server` for `redirect_uri`, asking for
/// the authorization code and refresh token grants.
pub async fn register(
    http: &dyn OutboundHttp,
    server: &AuthorizationServer,
    redirect_uri: &str,
    deadline: Instant,
) -> Result<Registered, SignInError> {
    let body = json!({
        "client_name": CLIENT_NAME,
        "redirect_uris": [redirect_uri],
        "grant_types": ["authorization_code", "refresh_token"],
        "response_types": ["code"],
        "token_endpoint_auth_method": "none",
    });
    let request = OutboundRequest {
        method: Method::Post,
        url: server.registration_endpoint.clone(),
        headers: vec![
            ("Content-Type".to_owned(), "application/json".to_owned()),
            ("Accept".to_owned(), "application/json".to_owned()),
        ],
        body: Some(body.to_string().into_bytes()),
        credential: None,
    };
    let response = send(http, request, deadline).await.map_err(failed)?;
    let answer = Zeroizing::new(response.body);
    match response.status {
        200 | 201 => {}
        429 | 500..=599 => return Err(SignInError::Unavailable),
        _ => return Err(SignInError::Unsupported("the registration was refused")),
    }
    let issued: Map<String, Value> =
        serde_json::from_slice(&answer).map_err(|_| SignInError::Unsupported("a registration that isn't JSON"))?;
    let client_id = issued
        .get("client_id")
        .and_then(Value::as_str)
        .filter(|id| {
            !id.is_empty() && id.len() <= MAX_CLIENT_ID && id.bytes().all(|byte| (0x20..=0x7e).contains(&byte))
        })
        .ok_or(SignInError::Unsupported("a registration without a usable client ID"))?
        .to_owned();
    let secret = |key: &str| -> Result<Option<Zeroizing<String>>, SignInError> {
        match issued.get(key) {
            None | Some(Value::Null) => Ok(None),
            Some(Value::String(value)) if value.is_empty() => Ok(None),
            Some(Value::String(value)) if value.len() <= MAX_ISSUED => Ok(Some(Zeroizing::new(value.clone()))),
            Some(_) => Err(SignInError::Unsupported("an issued value this client can't keep")),
        }
    };
    let client_secret = secret("client_secret")?;
    let registration_access_token = secret("registration_access_token")?;
    if let Some(Value::Array(uris)) = issued.get("redirect_uris")
        && !uris.iter().any(|uri| uri.as_str() == Some(redirect_uri))
    {
        return Err(SignInError::Unsupported("a registration for another redirect URI"));
    }
    let authentication = match issued.get("token_endpoint_auth_method").and_then(Value::as_str) {
        None => {
            if client_secret.is_some() {
                ClientAuthentication::ClientSecretBasic
            } else {
                ClientAuthentication::None
            }
        }
        Some("none") => ClientAuthentication::None,
        Some("client_secret_basic") if client_secret.is_some() => ClientAuthentication::ClientSecretBasic,
        Some(_) => {
            return Err(SignInError::Unsupported(
                "a client authentication this client doesn't use",
            ));
        }
    };
    Ok(Registered {
        client_id,
        client_secret,
        registration_access_token,
        authentication,
    })
}

/// An absolute HTTPS URL with a host, no user name or password and no fragment.
fn https(value: &str) -> Option<Url> {
    if value.len() > MAX_URL {
        return None;
    }
    let url = Url::parse(value).ok()?;
    (url.scheme() == "https"
        && url.host_str().is_some_and(|host| !host.is_empty())
        && url.username().is_empty()
        && url.password().is_none()
        && url.fragment().is_none())
    .then_some(url)
}

fn failed(error: OutboundError) -> SignInError {
    match error {
        OutboundError::Refused(Refusal::Unresolvable)
        | OutboundError::TimedOut
        | OutboundError::Unreachable
        | OutboundError::Failed => SignInError::Unavailable,
        OutboundError::Refused(_) => SignInError::Unsupported("an address that was refused"),
        OutboundError::Redirected => SignInError::Unsupported("a redirect, which isn't followed"),
        OutboundError::TooLarge => SignInError::Unsupported("an answer larger than allowed"),
    }
}

/// Asks the MCP server unauthenticated, as a client starting a session
/// would, and reads `resource_metadata` from a `401`'s `WWW-Authenticate`.
async fn challenge(
    http: &dyn OutboundHttp,
    server_url: &str,
    deadline: Instant,
) -> Result<Option<String>, SignInError> {
    let body = json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "initialize",
        "params": {
            "protocolVersion": crate::PROTOCOL_VERSIONS[0],
            "capabilities": {},
            "clientInfo": { "name": "zupflow", "version": env!("CARGO_PKG_VERSION") },
        },
    });
    let request = OutboundRequest {
        method: Method::Post,
        url: server_url.to_owned(),
        headers: vec![
            ("Content-Type".to_owned(), "application/json".to_owned()),
            ("Accept".to_owned(), "application/json, text/event-stream".to_owned()),
        ],
        body: Some(body.to_string().into_bytes()),
        credential: None,
    };
    let response = match send(http, request, deadline).await {
        Ok(response) => response,
        // A server that answers too much or redirects still may have metadata.
        Err(OutboundError::TooLarge | OutboundError::Redirected) => return Ok(None),
        Err(error) => return Err(failed(error)),
    };
    match response.status {
        401 => Ok(header(&response, "www-authenticate")
            .and_then(resource_metadata)
            .filter(|url| https(url).is_some())),
        429 | 500..=599 => Err(SignInError::Unavailable),
        _ => Ok(None),
    }
}

fn header<'a>(response: &'a OutboundResponse, name: &str) -> Option<&'a str> {
    response
        .headers
        .iter()
        .find(|(header, _)| header.eq_ignore_ascii_case(name))
        .map(|(_, value)| value.as_str())
}

/// The `resource_metadata` parameter of a `WWW-Authenticate` value (RFC
/// 9728, section 5.1), quoted or not.
fn resource_metadata(value: &str) -> Option<String> {
    const NAME: &str = "resource_metadata";
    let lower = value.to_ascii_lowercase();
    let mut from = 0;
    while let Some(found) = lower[from..].find(NAME) {
        let start = from + found;
        from = start + NAME.len();
        // A parameter's own name: not the tail of another's.
        let before = value[..start].chars().next_back();
        if before.is_some_and(|c| !(c == ' ' || c == ',' || c == '\t')) {
            continue;
        }
        let rest = value[from..].trim_start();
        let Some(rest) = rest.strip_prefix('=') else {
            continue;
        };
        let rest = rest.trim_start();
        if let Some(quoted) = rest.strip_prefix('"') {
            let mut out = String::new();
            let mut chars = quoted.chars();
            while let Some(c) = chars.next() {
                match c {
                    '\\' => out.push(chars.next()?),
                    '"' => return Some(out),
                    c => out.push(c),
                }
            }
            return None;
        }
        let token: String = rest.chars().take_while(|c| !(*c == ',' || c.is_whitespace())).collect();
        return (!token.is_empty()).then_some(token);
    }
    None
}

/// `{origin}/.well-known/{name}{path}` when the URL has a path, then
/// `{origin}/.well-known/{name}`.
fn well_known(url: &Url, name: &str) -> Vec<String> {
    let origin = url.origin().ascii_serialization();
    let path = url.path().trim_end_matches('/');
    let mut urls = Vec::new();
    if !path.is_empty() {
        urls.push(format!("{origin}/.well-known/{name}{path}"));
    }
    urls.push(format!("{origin}/.well-known/{name}"));
    urls
}

/// Where an issuer's metadata may be, in order (RFC 8414, section 3.1, then
/// OpenID Connect discovery, path-inserted and path-appended). An issuer
/// with a path never takes the root's metadata: it would name another issuer.
fn authorization_server_metadata(url: &Url, issuer: &str) -> Vec<String> {
    let origin = url.origin().ascii_serialization();
    let path = url.path().trim_end_matches('/');
    if path.is_empty() {
        vec![
            format!("{origin}/.well-known/oauth-authorization-server"),
            format!("{origin}/.well-known/openid-configuration"),
        ]
    } else {
        vec![
            format!("{origin}/.well-known/oauth-authorization-server{path}"),
            format!("{origin}/.well-known/openid-configuration{path}"),
            format!("{}/.well-known/openid-configuration", issuer.trim_end_matches('/')),
        ]
    }
}

/// A JSON object at `url`, `None` if it isn't there, or an error worth
/// stopping for: the server unavailable, or an address refused.
async fn metadata(
    http: &dyn OutboundHttp,
    url: &str,
    deadline: Instant,
) -> Result<Option<Map<String, Value>>, SignInError> {
    let request = OutboundRequest {
        method: Method::Get,
        url: url.to_owned(),
        headers: vec![("Accept".to_owned(), "application/json".to_owned())],
        body: None,
        credential: None,
    };
    let response = match send(http, request, deadline).await {
        Ok(response) => response,
        Err(OutboundError::TooLarge | OutboundError::Redirected) => return Ok(None),
        Err(error) => return Err(failed(error)),
    };
    match response.status {
        200..=299 => Ok(serde_json::from_slice(&response.body).ok()),
        429 | 500..=599 => Err(SignInError::Unavailable),
        _ => Ok(None),
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use std::sync::Mutex;

    use async_trait::async_trait;

    use super::*;

    /// One request: its method, URL, JSON body, and whether it carried a credential.
    type Sent = (Method, String, Option<Value>, bool);

    /// A change to the authorization server's metadata.
    type Change<'a> = &'a dyn Fn(&mut Map<String, Value>);

    /// Answers each URL with what the test scripted for it, `404` otherwise,
    /// and records what was sent.
    struct Server {
        answers: Vec<(String, Result<OutboundResponse, OutboundError>)>,
        sent: Mutex<Vec<Sent>>,
    }

    impl Server {
        fn new(answers: Vec<(&str, Result<OutboundResponse, OutboundError>)>) -> Self {
            Self {
                answers: answers
                    .into_iter()
                    .map(|(url, answer)| (url.to_owned(), answer))
                    .collect(),
                sent: Mutex::new(Vec::new()),
            }
        }

        fn urls(&self) -> Vec<String> {
            self.sent
                .lock()
                .unwrap()
                .iter()
                .map(|(_, url, _, _)| url.clone())
                .collect()
        }
    }

    #[async_trait]
    impl OutboundHttp for Server {
        async fn send(&self, request: OutboundRequest, _: Limits) -> Result<OutboundResponse, OutboundError> {
            self.sent.lock().unwrap().push((
                request.method,
                request.url.clone(),
                request
                    .body
                    .as_deref()
                    .map(|body| serde_json::from_slice(body).unwrap()),
                request.credential.is_some(),
            ));
            self.answers
                .iter()
                .find(|(url, _)| *url == request.url)
                .map(|(_, answer)| answer.clone())
                .unwrap_or_else(|| status(404))
        }
    }

    fn status(status: u16) -> Result<OutboundResponse, OutboundError> {
        Ok(OutboundResponse {
            status,
            headers: Vec::new(),
            body: Vec::new(),
        })
    }

    fn json_answer(status: u16, body: Value) -> Result<OutboundResponse, OutboundError> {
        Ok(OutboundResponse {
            status,
            headers: vec![("content-type".to_owned(), "application/json".to_owned())],
            body: body.to_string().into_bytes(),
        })
    }

    fn unauthorized(www_authenticate: &str) -> Result<OutboundResponse, OutboundError> {
        Ok(OutboundResponse {
            status: 401,
            headers: vec![("WWW-Authenticate".to_owned(), www_authenticate.to_owned())],
            body: Vec::new(),
        })
    }

    /// A deadline no test reaches unless it means to.
    fn later() -> Instant {
        Instant::now() + SIGN_IN_BUDGET
    }

    const MCP: &str = "https://tools.example/v1/mcp";
    const ISSUER: &str = "https://auth.example/tenant";

    fn resource() -> Value {
        json!({ "resource": MCP, "authorization_servers": [ISSUER, "https://other.example"] })
    }

    fn server_metadata() -> Value {
        json!({
            "issuer": ISSUER,
            "authorization_endpoint": "https://auth.example/tenant/authorize",
            "token_endpoint": "https://auth.example/tenant/token",
            "registration_endpoint": "https://auth.example/tenant/register",
            "code_challenge_methods_supported": ["plain", "S256"],
        })
    }

    fn expected() -> AuthorizationServer {
        AuthorizationServer {
            response_iss_supported: false,
            issuer: ISSUER.to_owned(),
            authorization_endpoint: "https://auth.example/tenant/authorize".to_owned(),
            token_endpoint: "https://auth.example/tenant/token".to_owned(),
            registration_endpoint: "https://auth.example/tenant/register".to_owned(),
        }
    }

    #[tokio::test]
    async fn follows_the_challenge_to_the_resource_metadata() {
        let http = Server::new(vec![
            (
                MCP,
                unauthorized(r#"Bearer error="invalid_token", resource_metadata="https://meta.example/prm""#),
            ),
            ("https://meta.example/prm", json_answer(200, resource())),
            (
                "https://auth.example/.well-known/oauth-authorization-server/tenant",
                json_answer(200, server_metadata()),
            ),
        ]);
        assert_eq!(discover(&http, MCP, later()).await, Ok(expected()));
        assert_eq!(
            http.urls(),
            [
                MCP,
                "https://meta.example/prm",
                "https://auth.example/.well-known/oauth-authorization-server/tenant"
            ]
        );
        let sent = http.sent.lock().unwrap();
        assert!(
            sent.iter().all(|(_, _, _, credential)| !credential),
            "nothing authenticated"
        );
        assert_eq!(sent[0].0, Method::Post);
        assert_eq!(sent[0].2.as_ref().unwrap()["method"], "initialize");
    }

    #[tokio::test]
    async fn falls_back_to_the_well_known_paths_in_order() {
        // No challenge: the protected resource's path-suffixed metadata, then
        // the root's; the issuer's path-suffixed metadata, then OIDC's.
        let http = Server::new(vec![
            (MCP, status(405)),
            (
                "https://tools.example/.well-known/oauth-protected-resource",
                json_answer(200, resource()),
            ),
            (
                "https://auth.example/tenant/.well-known/openid-configuration",
                json_answer(200, server_metadata()),
            ),
        ]);
        assert_eq!(discover(&http, MCP, later()).await, Ok(expected()));
        assert_eq!(
            http.urls(),
            [
                MCP,
                "https://tools.example/.well-known/oauth-protected-resource/v1/mcp",
                "https://tools.example/.well-known/oauth-protected-resource",
                "https://auth.example/.well-known/oauth-authorization-server/tenant",
                "https://auth.example/.well-known/openid-configuration/tenant",
                "https://auth.example/tenant/.well-known/openid-configuration",
            ]
        );

        // An issuer without a path: the root's metadata.
        let root = "https://auth.example";
        let http = Server::new(vec![
            (MCP, unauthorized("Bearer realm=\"tools\"")),
            (
                "https://tools.example/.well-known/oauth-protected-resource/v1/mcp",
                json_answer(200, json!({ "resource": MCP, "authorization_servers": [root] })),
            ),
            (
                "https://auth.example/.well-known/oauth-authorization-server",
                json_answer(200, {
                    let mut metadata = metadata_fields();
                    metadata.insert("issuer".into(), json!(root));
                    Value::Object(metadata)
                }),
            ),
        ]);
        assert_eq!(
            discover(&http, MCP, later()).await.map(|server| server.issuer),
            Ok(root.to_owned())
        );
    }

    /// The metadata's endpoints without its issuer, for building variants.
    fn metadata_fields() -> serde_json::Map<String, Value> {
        let Value::Object(mut fields) = server_metadata() else {
            unreachable!()
        };
        fields.remove("issuer");
        fields
    }

    #[tokio::test]
    async fn refuses_metadata_it_cannot_trust_or_use() {
        let with = |change: &dyn Fn(&mut Map<String, Value>)| {
            let Value::Object(mut metadata) = server_metadata() else {
                unreachable!()
            };
            change(&mut metadata);
            Server::new(vec![
                (
                    MCP,
                    unauthorized(r#"Bearer resource_metadata="https://meta.example/prm""#),
                ),
                ("https://meta.example/prm", json_answer(200, resource())),
                (
                    "https://auth.example/.well-known/oauth-authorization-server/tenant",
                    json_answer(200, Value::Object(metadata)),
                ),
            ])
        };
        let cases: [(Change, &str); 6] = [
            (
                &|m| {
                    m.insert("issuer".into(), json!("https://auth.example/other"));
                },
                "metadata for another issuer",
            ),
            (
                &|m| {
                    m.insert("token_endpoint".into(), json!("http://auth.example/tenant/token"));
                },
                "no HTTPS token endpoint",
            ),
            (
                &|m| {
                    m.insert("authorization_endpoint".into(), json!("https://user:pw@auth.example/a"));
                },
                "no HTTPS authorization endpoint",
            ),
            (
                &|m| {
                    m.insert("code_challenge_methods_supported".into(), json!(["plain"]));
                },
                "no S256 code challenge",
            ),
            (
                &|m| {
                    m.remove("registration_endpoint");
                },
                "no client registration",
            ),
            (
                &|m| {
                    m.insert("registration_endpoint".into(), json!("http://auth.example/register"));
                },
                "no HTTPS registration endpoint",
            ),
        ];
        for (change, why) in cases {
            assert_eq!(
                discover(&with(change), MCP, later()).await,
                Err(SignInError::Unsupported(why))
            );
        }

        // Nothing to find, or no authorization server named.
        let nothing = Server::new(vec![]);
        assert_eq!(
            discover(&nothing, MCP, later()).await,
            Err(SignInError::Unsupported("no protected-resource metadata"))
        );
        let unnamed = Server::new(vec![(
            "https://tools.example/.well-known/oauth-protected-resource",
            json_answer(200, json!({ "resource": MCP, "authorization_servers": [] })),
        )]);
        assert_eq!(
            discover(&unnamed, MCP, later()).await,
            Err(SignInError::Unsupported("no authorization server named"))
        );
        let plain = Server::new(vec![(
            "https://tools.example/.well-known/oauth-protected-resource",
            json_answer(
                200,
                json!({ "resource": MCP, "authorization_servers": ["http://auth.example"] }),
            ),
        )]);
        assert_eq!(
            discover(&plain, MCP, later()).await,
            Err(SignInError::Unsupported("an authorization server that isn't HTTPS"))
        );
    }

    #[tokio::test]
    async fn refuses_metadata_for_another_resource() {
        let answering = |metadata: Value| {
            Server::new(vec![
                (
                    MCP,
                    unauthorized(r#"Bearer resource_metadata="https://meta.example/prm""#),
                ),
                ("https://meta.example/prm", json_answer(200, metadata)),
                (
                    "https://auth.example/.well-known/oauth-authorization-server/tenant",
                    json_answer(200, server_metadata()),
                ),
            ])
        };
        for (named, why) in [
            (json!("https://other.example/v1/mcp"), "metadata for another resource"),
            (
                json!("https://tools.example/v1/mcp/other"),
                "metadata for another resource",
            ),
            (json!("https://tools.example/V1/MCP"), "metadata for another resource"),
            (
                json!("https://tools.example:8443/v1/mcp"),
                "metadata for another resource",
            ),
            (json!("https://tools.example/v1/mcp//"), "metadata for another resource"),
            (Value::Null, "protected-resource metadata without its resource"),
        ] {
            let http = answering(json!({ "resource": named, "authorization_servers": [ISSUER] }));
            assert_eq!(
                discover(&http, MCP, later()).await,
                Err(SignInError::Unsupported(why)),
                "{named}"
            );
            assert_eq!(http.urls().len(), 2, "no authorization server asked: {named}");
        }
        let http = answering(json!({ "authorization_servers": [ISSUER] }));
        assert_eq!(
            discover(&http, MCP, later()).await,
            Err(SignInError::Unsupported(
                "protected-resource metadata without its resource"
            ))
        );

        // Only the scheme's and host's case and a trailing slash may differ.
        for named in ["HTTPS://Tools.Example/v1/mcp/", "https://tools.example/v1/mcp/"] {
            let http = answering(json!({ "resource": named, "authorization_servers": [ISSUER] }));
            assert_eq!(discover(&http, MCP, later()).await, Ok(expected()), "{named}");
        }
    }

    /// Answers as `inner` does, `delay` later. A server that `keeps_time`
    /// gives up at the request's timeout; one that doesn't, never does.
    struct Slow {
        inner: Server,
        delay: Duration,
        keeps_time: bool,
        timeouts: Mutex<Vec<Duration>>,
    }

    impl Slow {
        fn new(inner: Server, delay: Duration, keeps_time: bool) -> Self {
            Self {
                inner,
                delay,
                keeps_time,
                timeouts: Mutex::new(Vec::new()),
            }
        }
    }

    #[async_trait]
    impl OutboundHttp for Slow {
        async fn send(&self, request: OutboundRequest, limits: Limits) -> Result<OutboundResponse, OutboundError> {
            self.timeouts.lock().unwrap().push(limits.timeout);
            if self.keeps_time && self.delay > limits.timeout {
                tokio::time::sleep(limits.timeout).await;
                return Err(OutboundError::TimedOut);
            }
            tokio::time::sleep(self.delay).await;
            self.inner.send(request, limits).await
        }
    }

    /// A server with everything sign-in needs, found through its challenge.
    fn signing_in() -> Server {
        Server::new(vec![
            (
                MCP,
                unauthorized(r#"Bearer resource_metadata="https://meta.example/prm""#),
            ),
            ("https://meta.example/prm", json_answer(200, resource())),
            (
                "https://auth.example/.well-known/oauth-authorization-server/tenant",
                json_answer(200, server_metadata()),
            ),
            (
                "https://auth.example/tenant/register",
                json_answer(201, json!({ "client_id": "client-1" })),
            ),
        ])
    }

    #[tokio::test(start_paused = true)]
    async fn discovery_and_registration_share_one_deadline() {
        let seconds = Duration::from_secs;

        // Each answer takes 8 s: discovery's three take 24, and registration
        // gets the 6 left, which isn't enough.
        let http = Slow::new(signing_in(), seconds(8), true);
        let begun = Instant::now();
        let deadline = begun + SIGN_IN_BUDGET;
        let server = discover(&http, MCP, deadline).await.unwrap();
        assert_eq!(begun.elapsed(), seconds(24));
        assert_eq!(
            register(&http, &server, REDIRECT, deadline).await.map(|_| ()),
            Err(SignInError::Unavailable)
        );
        assert_eq!(begun.elapsed(), SIGN_IN_BUDGET);
        assert_eq!(
            *http.timeouts.lock().unwrap(),
            [seconds(10), seconds(10), seconds(10), seconds(6)]
        );

        // Quick enough, it registers within the same deadline.
        let http = Slow::new(signing_in(), seconds(7), true);
        let deadline = Instant::now() + SIGN_IN_BUDGET;
        let server = discover(&http, MCP, deadline).await.unwrap();
        let registered = register(&http, &server, REDIRECT, deadline).await.unwrap();
        assert_eq!(registered.client_id, "client-1");

        // A server that ignores its timeout is cut off at the deadline all the same.
        let http = Slow::new(signing_in(), seconds(60), false);
        let begun = Instant::now();
        assert_eq!(
            discover(&http, MCP, begun + SIGN_IN_BUDGET).await,
            Err(SignInError::Unavailable)
        );
        assert_eq!(begun.elapsed(), SIGN_IN_BUDGET);

        // Past the deadline, nothing is sent.
        let http = signing_in();
        assert_eq!(
            register(&http, &expected(), REDIRECT, Instant::now()).await.map(|_| ()),
            Err(SignInError::Unavailable)
        );
        assert!(http.urls().is_empty());
    }

    #[tokio::test]
    async fn says_when_the_server_is_unavailable() {
        let down = Server::new(vec![(MCP, Err(OutboundError::Unreachable))]);
        assert_eq!(discover(&down, MCP, later()).await, Err(SignInError::Unavailable));
        let overloaded = Server::new(vec![(MCP, status(503))]);
        assert_eq!(discover(&overloaded, MCP, later()).await, Err(SignInError::Unavailable));
        let metadata_down = Server::new(vec![
            (MCP, status(401)),
            (
                "https://tools.example/.well-known/oauth-protected-resource/v1/mcp",
                json_answer(200, resource()),
            ),
            (
                "https://auth.example/.well-known/oauth-authorization-server/tenant",
                status(502),
            ),
        ]);
        assert_eq!(
            discover(&metadata_down, MCP, later()).await,
            Err(SignInError::Unavailable)
        );
        let private = Server::new(vec![(MCP, Err(OutboundError::Refused(Refusal::NotPublic)))]);
        assert_eq!(
            discover(&private, MCP, later()).await,
            Err(SignInError::Unsupported("an address that was refused"))
        );
    }

    #[test]
    fn reads_resource_metadata_from_a_challenge() {
        for (value, expected) in [
            (
                r#"Bearer resource_metadata="https://a.example/x""#,
                Some("https://a.example/x"),
            ),
            (
                r#"Bearer realm="r", Resource_Metadata = "https://a.example/\"q\"""#,
                Some(r#"https://a.example/"q""#),
            ),
            (
                "Bearer resource_metadata=https://a.example/x, scope=read",
                Some("https://a.example/x"),
            ),
            (r#"Bearer not_resource_metadata="https://evil.example""#, None),
            ("Bearer realm=\"r\"", None),
            (r#"Bearer resource_metadata="unterminated"#, None),
        ] {
            assert_eq!(resource_metadata(value).as_deref(), expected, "{value}");
        }
    }

    const REDIRECT: &str = "https://zupflow.example/connect/oauth/callback";

    #[tokio::test]
    async fn registers_a_public_client() {
        let http = Server::new(vec![(
            "https://auth.example/tenant/register",
            json_answer(201, json!({ "client_id": "client-1", "redirect_uris": [REDIRECT] })),
        )]);
        let registered = register(&http, &expected(), REDIRECT, later()).await.unwrap();
        assert_eq!(
            (registered.client_id.as_str(), registered.authentication),
            ("client-1", ClientAuthentication::None)
        );
        assert!(registered.client_secret.is_none() && registered.registration_access_token.is_none());
        let sent = http.sent.lock().unwrap();
        assert_eq!(
            sent[0].2,
            Some(json!({
                "client_name": "Zupflow",
                "redirect_uris": [REDIRECT],
                "grant_types": ["authorization_code", "refresh_token"],
                "response_types": ["code"],
                "token_endpoint_auth_method": "none",
            }))
        );
        assert!(!sent[0].3, "unauthenticated");
    }

    #[tokio::test]
    async fn authenticates_with_a_secret_the_server_issued() {
        let http = Server::new(vec![(
            "https://auth.example/tenant/register",
            json_answer(
                201,
                json!({ "client_id": "client-2", "client_secret": "s3cret", "registration_access_token": "rat" }),
            ),
        )]);
        let registered = register(&http, &expected(), REDIRECT, later()).await.unwrap();
        assert_eq!(registered.authentication, ClientAuthentication::ClientSecretBasic);
        assert_eq!(registered.client_secret.as_deref().map(String::as_str), Some("s3cret"));
        assert_eq!(
            registered.registration_access_token.as_deref().map(String::as_str),
            Some("rat")
        );
        assert!(!format!("{registered:?}").contains("s3cret"), "never printed");
    }

    #[tokio::test]
    async fn refuses_a_registration_it_cannot_use() {
        for (answer, want) in [
            (status(400), SignInError::Unsupported("the registration was refused")),
            (status(503), SignInError::Unavailable),
            (Err(OutboundError::TimedOut), SignInError::Unavailable),
            (
                json_answer(201, json!({ "client_secret": "s" })),
                SignInError::Unsupported("a registration without a usable client ID"),
            ),
            (
                json_answer(
                    201,
                    json!({ "client_id": "c", "token_endpoint_auth_method": "client_secret_post", "client_secret": "s" }),
                ),
                SignInError::Unsupported("a client authentication this client doesn't use"),
            ),
            (
                json_answer(
                    201,
                    json!({ "client_id": "c", "token_endpoint_auth_method": "client_secret_basic" }),
                ),
                SignInError::Unsupported("a client authentication this client doesn't use"),
            ),
            (
                json_answer(
                    201,
                    json!({ "client_id": "c", "redirect_uris": ["https://elsewhere.example/cb"] }),
                ),
                SignInError::Unsupported("a registration for another redirect URI"),
            ),
        ] {
            let http = Server::new(vec![("https://auth.example/tenant/register", answer)]);
            assert_eq!(
                register(&http, &expected(), REDIRECT, later()).await.map(|_| ()),
                Err(want)
            );
        }
    }
}
