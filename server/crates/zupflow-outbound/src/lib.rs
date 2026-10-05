//! [`SafeHttpClient`]: the basic [`OutboundHttp`], on reqwest with rustls.
//!
//! - **Only HTTPS**, to an absolute URL with no user name or password in it.
//! - **Only public addresses.** An IP-literal host is checked as written; a
//!   name is resolved by our own resolver, which refuses the whole name if any
//!   address it resolves to isn't public (`zupflow_core::outbound::is_public`),
//!   and hands the connector only the addresses it checked. So the connection
//!   goes exactly where the check looked: a name that resolves to a public
//!   address for the check and a private one for the connection (DNS
//!   rebinding) can't slip between the two. No proxy is used, so nothing else
//!   resolves names on our behalf.
//! - **No redirects.** A `3xx` answer is [`OutboundError::Redirected`].
//! - **Bounded:** the whole exchange within `Limits::timeout`, the body read
//!   up to `Limits::max_response_bytes` and no further.
//! - **Failures by kind,** never by the other side's words.
//! - **The credential** is set after every other header, and every exact
//!   occurrence of it is scrubbed from the body and header values returned.

use std::error::Error as _;
use std::net::{IpAddr, SocketAddr};

use async_trait::async_trait;
use reqwest::dns::{Addrs, Name, Resolve, Resolving};
use reqwest::header::{HeaderName, HeaderValue};
use reqwest::{Client, Url};
use zupflow_core::outbound::{
    Limits, MAX_CREDENTIAL_BYTES, Method, OutboundError, OutboundHttp, OutboundRequest, OutboundResponse, Refusal,
    is_public, scrub,
};

/// Raised by the resolver; found again in the error chain to say why.
#[derive(Debug)]
struct NotPublic;

impl std::fmt::Display for NotPublic {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("resolves to a non-public address")
    }
}

impl std::error::Error for NotPublic {}

#[derive(Debug)]
struct Unresolvable;

impl std::fmt::Display for Unresolvable {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("doesn't resolve")
    }
}

impl std::error::Error for Unresolvable {}

/// Which destinations are allowed. Production allows public HTTPS only; the
/// tests below also reach plain HTTP on loopback, which nothing else can ask for.
#[derive(Debug, Clone, Copy)]
struct Policy {
    loopback_for_tests: bool,
}

impl Policy {
    fn allows(self, ip: IpAddr) -> bool {
        is_public(ip) || (self.loopback_for_tests && ip.is_loopback())
    }
}

/// Whether a name may be called: only if every address it resolves to is
/// allowed. One bad address refuses the name: a host that resolves to both
/// public and private addresses is an attempt, not an accident.
fn all_allowed(policy: Policy, addresses: &[SocketAddr]) -> bool {
    addresses.iter().all(|address| policy.allows(address.ip()))
}

/// Headers a caller can't set: the connection's own, and `Accept-Encoding`,
/// because nothing here decodes, and a compressed echo of the credential
/// couldn't be scrubbed.
const RESERVED_HEADERS: [&str; 10] = [
    "host",
    "accept-encoding",
    "connection",
    "keep-alive",
    "proxy-authorization",
    "proxy-connection",
    "te",
    "trailer",
    "transfer-encoding",
    "upgrade",
];

/// Whether a header is one only the client sets. `HeaderName` is lowercase.
fn reserved(name: &HeaderName) -> bool {
    RESERVED_HEADERS.contains(&name.as_str()) || name.as_str() == "content-length"
}

/// Resolves a name and refuses it unless every address it resolves to is allowed.
struct CheckedResolver {
    policy: Policy,
}

impl Resolve for CheckedResolver {
    fn resolve(&self, name: Name) -> Resolving {
        let policy = self.policy;
        let host = name.as_str().to_owned();
        Box::pin(async move {
            let addresses: Vec<SocketAddr> = tokio::net::lookup_host((host.as_str(), 0))
                .await
                .map_err(|_| Box::new(Unresolvable) as Box<dyn std::error::Error + Send + Sync>)?
                .collect();
            if addresses.is_empty() {
                return Err(Box::new(Unresolvable) as Box<dyn std::error::Error + Send + Sync>);
            }
            if !all_allowed(policy, &addresses) {
                tracing::warn!(event = "outbound.not_public", host = %host);
                return Err(Box::new(NotPublic) as Box<dyn std::error::Error + Send + Sync>);
            }
            Ok(Box::new(addresses.into_iter()) as Addrs)
        })
    }
}

pub struct SafeHttpClient {
    client: Client,
    policy: Policy,
}

impl SafeHttpClient {
    pub fn new() -> Result<Self, ClientError> {
        Self::with_policy(Policy {
            loopback_for_tests: false,
        })
    }

    fn with_policy(policy: Policy) -> Result<Self, ClientError> {
        let client = Client::builder()
            .no_proxy()
            .redirect(reqwest::redirect::Policy::none())
            .https_only(!policy.loopback_for_tests)
            .dns_resolver(CheckedResolver { policy })
            .build()
            .map_err(|_| ClientError)?;
        Ok(Self { client, policy })
    }

    /// Checks the URL before anything is resolved or sent.
    fn checked_url(&self, url: &str) -> Result<Url, OutboundError> {
        let url = Url::parse(url).map_err(|_| OutboundError::Refused(Refusal::InvalidUrl))?;
        if !url.username().is_empty() || url.password().is_some() {
            return Err(OutboundError::Refused(Refusal::InvalidUrl));
        }
        let allowed_scheme = url.scheme() == "https" || (self.policy.loopback_for_tests && url.scheme() == "http");
        if !allowed_scheme {
            return Err(OutboundError::Refused(Refusal::NotHttps));
        }
        let Some(host) = url.host_str() else {
            return Err(OutboundError::Refused(Refusal::InvalidUrl));
        };
        // A literal address is never resolved, so it's checked here.
        let literal = host.trim_start_matches('[').trim_end_matches(']').parse::<IpAddr>();
        match literal {
            Ok(ip) if !self.policy.allows(ip) => Err(OutboundError::Refused(Refusal::NotPublic)),
            _ => Ok(url),
        }
    }
}

/// The client couldn't be built (no TLS roots, say).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ClientError;

impl std::fmt::Display for ClientError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("can't build the HTTP client")
    }
}

impl std::error::Error for ClientError {}

/// What to scrub for a credential: its whole value and, for a value with a
/// scheme (`Bearer abc…`), the token alone, which is what a provider that
/// echoes it is likely to repeat.
fn secrets(value: &str) -> Vec<&[u8]> {
    let mut secrets = vec![value.as_bytes()];
    if let Some((_, token)) = value.rsplit_once(' ')
        && token.len() >= 8
    {
        secrets.push(token.as_bytes());
    }
    secrets
}

/// What a reqwest error was, by kind, never by message.
fn classify(error: &reqwest::Error) -> OutboundError {
    let mut source: Option<&(dyn std::error::Error + 'static)> = error.source();
    while let Some(cause) = source {
        if cause.is::<NotPublic>() {
            return OutboundError::Refused(Refusal::NotPublic);
        }
        if cause.is::<Unresolvable>() {
            return OutboundError::Refused(Refusal::Unresolvable);
        }
        source = cause.source();
    }
    if error.is_timeout() {
        OutboundError::TimedOut
    } else if error.is_connect() {
        OutboundError::Unreachable
    } else {
        OutboundError::Failed
    }
}

#[async_trait]
impl OutboundHttp for SafeHttpClient {
    async fn send(&self, request: OutboundRequest, limits: Limits) -> Result<OutboundResponse, OutboundError> {
        let url = self.checked_url(&request.url)?;
        let method = match request.method {
            Method::Get => reqwest::Method::GET,
            Method::Post => reqwest::Method::POST,
            Method::Put => reqwest::Method::PUT,
            Method::Patch => reqwest::Method::PATCH,
            Method::Delete => reqwest::Method::DELETE,
        };
        let mut headers = reqwest::header::HeaderMap::new();
        let invalid = || OutboundError::Refused(Refusal::InvalidHeader);
        for (name, value) in &request.headers {
            let name = HeaderName::try_from(name.as_str()).map_err(|_| invalid())?;
            if reserved(&name) {
                continue;
            }
            let value = HeaderValue::try_from(value.as_str()).map_err(|_| invalid())?;
            headers.append(name, value);
        }
        // Last, replacing any header of the same name. The `HeaderValue` copy
        // reqwest keeps isn't zeroized; the `Credential` is.
        if let Some(credential) = &request.credential {
            if credential.value.is_empty() || credential.value.len() > MAX_CREDENTIAL_BYTES {
                return Err(invalid());
            }
            let name = HeaderName::try_from(credential.header.as_str()).map_err(|_| invalid())?;
            if reserved(&name) {
                return Err(invalid());
            }
            let mut value = HeaderValue::try_from(credential.value.as_str()).map_err(|_| invalid())?;
            value.set_sensitive(true);
            headers.insert(name, value);
        }
        let mut builder = self
            .client
            .request(method, url)
            .headers(headers)
            .timeout(limits.timeout);
        if let Some(body) = request.body {
            builder = builder.body(body);
        }

        let exchange = async {
            let mut response = builder.send().await.map_err(|error| classify(&error))?;
            if [301, 302, 303, 307, 308].contains(&response.status().as_u16()) {
                return Err(OutboundError::Redirected);
            }
            if response
                .content_length()
                .is_some_and(|length| length > limits.max_response_bytes as u64)
            {
                return Err(OutboundError::TooLarge);
            }
            let status = response.status().as_u16();
            let headers: Vec<(String, String)> = response
                .headers()
                .iter()
                .map(|(name, value)| {
                    (
                        name.as_str().to_owned(),
                        String::from_utf8_lossy(value.as_bytes()).into_owned(),
                    )
                })
                .collect();
            let mut body = Vec::new();
            while let Some(chunk) = response.chunk().await.map_err(|error| classify(&error))? {
                if body.len() + chunk.len() > limits.max_response_bytes {
                    return Err(OutboundError::TooLarge);
                }
                body.extend_from_slice(&chunk);
            }
            Ok(OutboundResponse { status, headers, body })
        };
        let mut response = tokio::time::timeout(limits.timeout, exchange)
            .await
            .map_err(|_| OutboundError::TimedOut)??;

        if let Some(credential) = &request.credential {
            for secret in secrets(&credential.value) {
                // A header named after the credential is dropped: names can't be rewritten.
                let lowered = String::from_utf8_lossy(secret).to_ascii_lowercase();
                response.headers.retain(|(name, _)| !name.contains(lowered.as_str()));
                scrub(&mut response.body, secret);
                for (_, value) in &mut response.headers {
                    let mut bytes = std::mem::take(value).into_bytes();
                    scrub(&mut bytes, secret);
                    *value = String::from_utf8_lossy(&bytes).into_owned();
                }
            }
        }
        Ok(response)
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use std::sync::{Arc, Mutex};
    use std::time::Duration;

    use axum::body::Body;
    use axum::http::{HeaderMap, StatusCode};
    use axum::response::IntoResponse;
    use axum::routing::{get, post};
    use zeroize::Zeroizing;
    use zupflow_core::outbound::Credential;

    use super::*;

    fn request(method: Method, url: &str) -> OutboundRequest {
        OutboundRequest {
            method,
            url: url.to_owned(),
            headers: Vec::new(),
            body: None,
            credential: None,
        }
    }

    #[tokio::test]
    async fn refuses_every_destination_that_isnt_public_https() {
        let client = SafeHttpClient::new().unwrap();
        let refused = [
            ("not a url", Refusal::InvalidUrl),
            ("/relative", Refusal::InvalidUrl),
            ("https://user:secret@api.example/", Refusal::InvalidUrl),
            ("http://api.example/", Refusal::NotHttps),
            ("ftp://api.example/", Refusal::NotHttps),
            ("https://10.0.0.1/", Refusal::NotPublic),
            ("https://127.0.0.1:8080/", Refusal::NotPublic),
            ("https://[::1]/", Refusal::NotPublic),
            ("https://169.254.169.254/latest/meta-data/", Refusal::NotPublic),
            ("https://[::ffff:127.0.0.1]/", Refusal::NotPublic),
            ("https://[fd00::1]/", Refusal::NotPublic),
            // Shorthand IPv4 forms, normalised by the URL parser and then refused.
            ("https://2130706433/", Refusal::NotPublic),
            ("https://0x7f.1/", Refusal::NotPublic),
            ("https://127.1/", Refusal::NotPublic),
            ("https://0177.0.0.1/", Refusal::NotPublic),
            // Names are checked after resolution.
            ("https://localhost:9/", Refusal::NotPublic),
            ("https://nowhere.invalid/", Refusal::Unresolvable),
        ];
        for (url, refusal) in refused {
            assert_eq!(
                client.send(request(Method::Get, url), Limits::default()).await,
                Err(OutboundError::Refused(refusal)),
                "{url}"
            );
        }
    }

    #[test]
    fn refuses_a_name_with_any_address_that_isnt_public() {
        let production = Policy {
            loopback_for_tests: false,
        };
        let address = |text: &str| -> SocketAddr { text.parse().unwrap() };
        assert!(all_allowed(
            production,
            &[address("8.8.8.8:443"), address("1.1.1.1:443")]
        ));
        assert!(
            !all_allowed(production, &[address("8.8.8.8:443"), address("10.0.0.1:443")]),
            "mixed"
        );
        assert!(
            !all_allowed(production, &[address("[::1]:443"), address("8.8.8.8:443")]),
            "mixed, private first"
        );
    }

    #[tokio::test]
    async fn refuses_bad_headers_and_oversized_credentials() {
        let client = SafeHttpClient::new().unwrap();
        let mut bad_name = request(Method::Get, "https://api.example/");
        bad_name.headers.push(("bad header".to_owned(), "x".to_owned()));
        let mut huge = request(Method::Get, "https://api.example/");
        huge.credential = Some(Credential {
            header: "authorization".to_owned(),
            value: Zeroizing::new("x".repeat(MAX_CREDENTIAL_BYTES + 1)),
        });
        let mut empty = request(Method::Get, "https://api.example/");
        empty.credential = Some(Credential {
            header: "authorization".to_owned(),
            value: Zeroizing::new(String::new()),
        });
        let mut as_host = request(Method::Get, "https://api.example/");
        as_host.credential = Some(Credential {
            header: "Host".to_owned(),
            value: Zeroizing::new("tok_test_s3cret0123".to_owned()),
        });
        for call in [bad_name, huge, empty, as_host] {
            assert_eq!(
                client.send(call, Limits::default()).await,
                Err(OutboundError::Refused(Refusal::InvalidHeader))
            );
        }
    }

    /// A loopback server, and a client allowed to reach it over plain HTTP.
    async fn server() -> (SafeHttpClient, String, Arc<Mutex<Vec<String>>>) {
        let seen = Arc::new(Mutex::new(Vec::new()));
        let recorded = seen.clone();
        let app = axum::Router::new()
            .route(
                "/echo",
                post(move |headers: HeaderMap, body: String| async move {
                    let authorization = headers
                        .get("authorization")
                        .map(|value| value.to_str().unwrap().to_owned())
                        .unwrap_or_default();
                    recorded.lock().unwrap().push(authorization.clone());
                    let token = authorization.rsplit(' ').next().unwrap_or_default().to_owned();
                    let host = headers
                        .get("host")
                        .map(|value| value.to_str().unwrap().to_owned())
                        .unwrap_or_default();
                    let encoding = headers.get("accept-encoding").is_some();
                    let mut answer = HeaderMap::new();
                    for (name, value) in [
                        ("x-echo".to_owned(), format!("you sent {authorization}")),
                        (format!("x-{}", token.to_ascii_lowercase()), "named after it".to_owned()),
                        ("x-host".to_owned(), host),
                        ("x-encoding".to_owned(), encoding.to_string()),
                    ] {
                        answer.insert(
                            axum::http::HeaderName::try_from(name).unwrap(),
                            axum::http::HeaderValue::try_from(value).unwrap(),
                        );
                    }
                    (answer, format!("{body} / {authorization} / just {token}"))
                }),
            )
            .route(
                "/redirect",
                get(|| async { (StatusCode::FOUND, [("location", "https://elsewhere.example/")]) }),
            )
            .route("/big", get(|| async { vec![b'x'; 2 << 20] }))
            .route(
                "/big-stream",
                get(|| async {
                    let chunks = (0..64).map(|_| Ok::<_, std::io::Error>(vec![b'y'; 64 << 10]));
                    Body::from_stream(futures_util::stream::iter(chunks))
                }),
            )
            .route(
                "/slow",
                get(|| async {
                    tokio::time::sleep(Duration::from_secs(3)).await;
                    "late"
                }),
            )
            .route("/not-modified", get(|| async { StatusCode::NOT_MODIFIED }))
            .route(
                "/error",
                get(|| async { (StatusCode::INTERNAL_SERVER_ERROR, "the provider's own words").into_response() }),
            );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://127.0.0.1:{}", listener.local_addr().unwrap().port());
        tokio::spawn(async move { axum::serve(listener, app).await });
        let client = SafeHttpClient::with_policy(Policy {
            loopback_for_tests: true,
        })
        .unwrap();
        (client, base, seen)
    }

    #[tokio::test]
    async fn injects_the_credential_last_and_scrubs_it_from_the_answer() {
        let (client, base, seen) = server().await;
        let mut call = request(Method::Post, &format!("{base}/echo"));
        call.body = Some(b"hello".to_vec());
        // The caller's own header of the same name never wins.
        call.headers
            .push(("authorization".to_owned(), "Bearer not-the-credential".to_owned()));
        // Headers a caller can't set are dropped.
        call.headers.push(("host".to_owned(), "internal.example".to_owned()));
        call.headers.push(("accept-encoding".to_owned(), "gzip".to_owned()));
        call.credential = Some(Credential {
            header: "authorization".to_owned(),
            value: Zeroizing::new("Bearer tok_test_s3cret0123".to_owned()),
        });
        let response = client.send(call, Limits::default()).await.unwrap();
        assert_eq!(
            seen.lock().unwrap().as_slice(),
            ["Bearer tok_test_s3cret0123"],
            "sent once, last"
        );
        assert_eq!(response.status, 200);
        assert_eq!(
            String::from_utf8(response.body).unwrap(),
            "hello / [redacted] / just [redacted]",
            "the whole value and the bare token"
        );
        let header = |name: &str| {
            response
                .headers
                .iter()
                .find(|(header, _)| header == name)
                .map(|(_, value)| value.as_str())
        };
        assert_eq!(header("x-echo"), Some("you sent [redacted]"));
        assert!(
            response.headers.iter().all(|(name, _)| !name.contains("s3cret")),
            "a header named after the credential is dropped"
        );
        assert!(
            header("x-host").unwrap().starts_with("127.0.0.1"),
            "the caller's Host isn't sent"
        );
        assert_eq!(header("x-encoding"), Some("false"), "nor its Accept-Encoding");
    }

    #[tokio::test]
    async fn bounds_what_comes_back() {
        let (client, base, _) = server().await;
        let call = |path: &str| request(Method::Get, &format!("{base}{path}"));
        assert_eq!(
            client.send(call("/redirect"), Limits::default()).await,
            Err(OutboundError::Redirected)
        );
        assert_eq!(
            client.send(call("/big"), Limits::default()).await,
            Err(OutboundError::TooLarge)
        );
        assert_eq!(
            client.send(call("/big-stream"), Limits::default()).await,
            Err(OutboundError::TooLarge),
            "a body with no length is cut off as it streams"
        );
        let quick = Limits {
            timeout: Duration::from_millis(300),
            ..Limits::default()
        };
        assert_eq!(client.send(call("/slow"), quick).await, Err(OutboundError::TimedOut));
        let unchanged = client.send(call("/not-modified"), Limits::default()).await.unwrap();
        assert_eq!(unchanged.status, 304, "a 304 is an answer, not a redirect");
        let error = client.send(call("/error"), Limits::default()).await.unwrap();
        assert_eq!(error.status, 500, "any status is an answer for the caller to classify");
        assert_eq!(
            client
                .send(request(Method::Get, "http://127.0.0.1:1/"), Limits::default())
                .await,
            Err(OutboundError::Unreachable)
        );
    }
}
