//! Refuses requests a browser was tricked into sending.
//!
//! Loopback isn't private from the browser: any page the user visits can send
//! requests to `127.0.0.1`, and DNS rebinding can make the browser treat the
//! server as the attacker's own origin and read the answers. That matters most
//! with `local` sign-in, where every request is the owner. So:
//!
//! - **Host.** With `local`, a request must name the server by a loopback name
//!   and its own port (`localhost`, `127.0.0.1` or `[::1]`). A rebound name
//!   (`evil.example` resolving to 127.0.0.1) is refused. The one exception is
//!   the host of `ZUPFLOW_PUBLIC_URL` (a tunnel, for trying Slack locally),
//!   admitted only for the routes that are public by design and authorized
//!   by what the request carries, never by the owner: hosted Connect (the
//!   link's reference, or the OAuth `state`) and channels' inbound endpoint
//!   (the provider's signature). Each is matched exactly, by method and by
//!   path segment ([`PUBLIC_HOST_ROUTES`]). Such a request is marked
//!   [`PublicHost`], and no caller is ever resolved for it (`auth.rs`), so it
//!   can't reach the local owner even through a route that asks for one.
//! - **Origin.** A browser request (one with `Origin`) must come from an
//!   allowed origin: one in `ZUPFLOW_ALLOWED_ORIGINS`, or, with `local`, the
//!   server's own loopback origin. The server's public origin
//!   (`ZUPFLOW_PUBLIC_URL`, where the hosted Connect page is served) is
//!   admitted for hosted Connect alone: the page, its files and the hosted
//!   Connect API (`/connect/…`, `/v1/connect/sessions/…`), never a Console
//!   or workspace route.
//! - **Content type.** Request bodies must be `application/json` (checked
//!   where a body is read, in `input.rs`), so a cross-site form or `text/plain`
//!   post can't carry one without a preflight.
//!
//! **CORS** is for the Console alone (`ZUPFLOW_CONSOLE_ORIGIN`): that one
//! origin is admitted, its preflights are answered here, and its responses
//! say it may read them. No other origin gets a CORS header, so a page
//! elsewhere can neither preflight a request nor read an answer. Credentials
//! are bearer tokens the Console sends itself, never cookies, so CORS never
//! allows credentials.

use std::sync::Arc;

use axum::extract::{Request, State};
use axum::http::header::{
    ACCESS_CONTROL_ALLOW_HEADERS, ACCESS_CONTROL_ALLOW_METHODS, ACCESS_CONTROL_ALLOW_ORIGIN, ACCESS_CONTROL_MAX_AGE,
    ACCESS_CONTROL_REQUEST_METHOD, HOST, ORIGIN, VARY,
};
use axum::http::{HeaderMap, HeaderValue, Method, StatusCode};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use zupflow_core::error::ApiError;

use crate::http::Failure;

#[derive(Clone, Debug, Default)]
pub struct RequestGuard {
    /// The only `Host` values accepted, lowercased; `None` accepts any.
    hosts: Option<Arc<[String]>>,
    /// With `hosts`, the `Host` values of the server's public URL when it
    /// isn't one of them, lowercased: admitted for [`PUBLIC_HOST_ROUTES`]
    /// only, and marked [`PublicHost`].
    public_hosts: Option<Arc<[String]>>,
    /// Origins a browser request may come from, lowercased, no trailing slash.
    origins: Arc<[String]>,
    /// The server's public origin, as `origins` holds them, when it isn't one
    /// of them: admitted for hosted Connect's paths only.
    public: Option<Arc<str>>,
    /// The Console's origin, as `origins` holds it: the one origin CORS
    /// answers. `None` answers none.
    console: Option<Arc<str>>,
}

/// The methods the API serves, for a preflight.
const ALLOWED_METHODS: &str = "GET, POST, PUT, PATCH, DELETE";
/// The request headers the Console sends beyond the CORS-safelisted ones:
/// its token, JSON bodies, `Idempotency-Key` on every `POST` and the
/// organization a workspace request acts in.
const ALLOWED_HEADERS: &str = "authorization, content-type, idempotency-key, zupflow-organization";
/// How long a browser may reuse a preflight's answer, in seconds.
const PREFLIGHT_MAX_AGE: &str = "600";

impl RequestGuard {
    /// For `local` sign-in on `port`: loopback hosts only, and browser
    /// requests from the server's own loopback origins or `origins`.
    pub fn local(port: u16, origins: &[String]) -> Self {
        let hosts: Vec<String> = ["localhost", "127.0.0.1", "[::1]"]
            .iter()
            .map(|host| format!("{host}:{port}"))
            .collect();
        let mut allowed: Vec<String> = hosts.iter().map(|host| format!("http://{host}")).collect();
        allowed.extend(origins.iter().map(|origin| normalized(origin)));
        Self {
            hosts: Some(hosts.into()),
            public_hosts: None,
            origins: allowed.into(),
            public: None,
            console: None,
        }
    }

    /// Any host; browser requests only from `origins`.
    pub fn with_origins(origins: &[String]) -> Self {
        Self {
            hosts: None,
            public_hosts: None,
            origins: origins.iter().map(|origin| normalized(origin)).collect(),
            public: None,
            console: None,
        }
    }

    /// Admits the server's own public origin, `ZUPFLOW_PUBLIC_URL`, for
    /// hosted Connect's paths only: the page is served there and calls the
    /// hosted Connect API from it. Same-origin, so no CORS.
    ///
    /// With `local`, also admits its host for [`PUBLIC_HOST_ROUTES`] alone,
    /// unless it's already one of the loopback hosts (the default
    /// `http://localhost:{port}`), which changes nothing.
    #[must_use]
    pub fn with_public_url(self, public_url: &str) -> Self {
        let public_hosts = match &self.hosts {
            Some(hosts) => {
                let named: Vec<String> = host_values(public_url)
                    .into_iter()
                    .filter(|host| !hosts.contains(host))
                    .collect();
                (!named.is_empty()).then(|| named.into())
            }
            None => None,
        };
        let origin = normalized(public_url);
        // Already allowed everywhere (the loopback origin with `local`, say).
        let public = (!self.origins.contains(&origin)).then(|| origin.into());
        Self {
            public_hosts,
            public,
            ..self
        }
    }

    /// Admits the Console's origin and answers CORS for it, and for no other.
    #[must_use]
    pub fn with_console(self, origin: &str) -> Self {
        let origin = normalized(origin);
        let mut origins = self.origins.to_vec();
        if !origins.contains(&origin) {
            origins.push(origin.clone());
        }
        Self {
            origins: origins.into(),
            console: Some(origin.into()),
            ..self
        }
    }
}

fn normalized(origin: &str) -> String {
    origin.trim_end_matches('/').to_ascii_lowercase()
}

/// The `Host` values a browser or provider sends for `public_url`'s origin:
/// its host and port, and its bare host when the port is the scheme's
/// default. Lowercased; none when it isn't a URL.
fn host_values(public_url: &str) -> Vec<String> {
    let Ok(url) = url::Url::parse(public_url) else {
        return Vec::new();
    };
    let (Some(host), Some(port)) = (url.host_str(), url.port_or_known_default()) else {
        return Vec::new();
    };
    let host = host.to_ascii_lowercase();
    let mut values = vec![format!("{host}:{port}")];
    if url.port().is_none() {
        values.push(host);
    }
    values
}

/// Hosted Connect's paths: the page and its files, the OAuth callback, and
/// the hosted Connect API, which the link's reference authorizes.
fn hosted_connect(path: &str) -> bool {
    path.starts_with("/connect/") || path.starts_with("/v1/connect/sessions/")
}

/// The routes the public host reaches under `local` sign-in, by method and
/// template exactly as the router declares them: each is public by design
/// and authorized by what the request carries, never by the local owner.
///
/// - Hosted Connect (`connect.rs`, `connect_page.rs`): the page and its
///   files, the hosted API, which the link's reference authorizes, and the
///   OAuth callback, which the single-use `state` does.
/// - Channels' inbound endpoint (`channel_inbound.rs`), which the provider's
///   signature over the body, with the app's stored secret, authorizes.
///
/// `GET` admits `HEAD` too, as the router does.
pub const PUBLIC_HOST_ROUTES: [(&str, &str); 10] = [
    ("GET", "/connect/{reference}"),
    ("GET", "/connect/assets/page.js"),
    ("GET", "/connect/assets/page.css"),
    ("GET", "/connect/assets/icon.svg"),
    ("GET", "/connect/oauth/callback"),
    ("GET", "/v1/connect/sessions/{reference}"),
    ("POST", "/v1/connect/sessions/{reference}/cancel"),
    ("POST", "/v1/connect/sessions/{reference}/credentials"),
    ("POST", "/v1/connect/sessions/{reference}/authorize"),
    ("POST", "/channels/inbound/{inbound_ref}"),
];

/// The longest path parameter the public host passes on. References are 32
/// characters; this leaves room without letting anything else through.
const PARAM_MAX: usize = 128;

/// Whether `method` and `path` name one of [`PUBLIC_HOST_ROUTES`]: the same
/// segments, literal ones exactly (case included), each parameter one
/// non-empty segment of letters, digits, `-` and `_`. So a path the router
/// would match elsewhere can't pass for one of these: no encoded slash or
/// `%` at all, no `.` or `..`, no empty segment from a doubled or trailing
/// slash, no extra segment.
fn public_route(method: &Method, path: &str) -> bool {
    let method = if *method == Method::HEAD {
        "GET"
    } else {
        method.as_str()
    };
    PUBLIC_HOST_ROUTES
        .iter()
        .any(|(allowed, template)| *allowed == method && matches_template(template, path))
}

fn matches_template(template: &str, path: &str) -> bool {
    template.split('/').count() == path.split('/').count()
        && template.split('/').zip(path.split('/')).all(|(want, got)| {
            if want.starts_with('{') {
                (1..=PARAM_MAX).contains(&got.len())
                    && got
                        .bytes()
                        .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
            } else {
                want == got
            }
        })
}

/// Marks a request the guard admitted on the server's public host under
/// `local` sign-in. Only the guard adds it; `auth.rs` resolves no caller,
/// and so never the local owner, for a request that carries it.
#[derive(Clone, Copy, Debug)]
pub struct PublicHost;

/// How the guard let a request through.
enum Admitted {
    /// Addressed as the guard requires everywhere.
    Everywhere,
    /// On the public host, for one of [`PUBLIC_HOST_ROUTES`].
    PublicHost,
}

pub async fn guard(State(guard): State<RequestGuard>, mut request: Request, next: Next) -> Response {
    // Decided before awaiting: the request is borrowed only here.
    let uri = request.uri();
    match admission(
        &guard,
        request.method(),
        request.headers(),
        uri.authority().map(|authority| authority.as_str()),
        uri.path(),
    ) {
        Err(refusal) => return refusal.into_response(),
        Ok(Admitted::PublicHost) => {
            request.extensions_mut().insert(PublicHost);
        }
        Ok(Admitted::Everywhere) => {}
    }
    let Some(console) = guard.console.clone() else {
        return next.run(request).await;
    };
    // Admitted already, so an `Origin` here is the Console's or another allowed one.
    let from_console = request
        .headers()
        .get(ORIGIN)
        .and_then(|value| value.to_str().ok())
        .is_some_and(|origin| normalized(origin) == *console);
    let preflight =
        request.method() == Method::OPTIONS && request.headers().contains_key(ACCESS_CONTROL_REQUEST_METHOD);
    let mut response = if from_console && preflight {
        preflight_response()
    } else {
        next.run(request).await
    };
    let headers = response.headers_mut();
    // Answers differ by origin once CORS is on, so a cache must key on it.
    headers.append(VARY, HeaderValue::from_static("Origin"));
    if from_console && let Ok(origin) = HeaderValue::from_str(&console) {
        headers.insert(ACCESS_CONTROL_ALLOW_ORIGIN, origin);
    }
    response
}

/// The Console may send any of the API's methods with the headers it uses.
fn preflight_response() -> Response {
    let mut response = StatusCode::NO_CONTENT.into_response();
    let headers = response.headers_mut();
    headers.insert(ACCESS_CONTROL_ALLOW_METHODS, HeaderValue::from_static(ALLOWED_METHODS));
    headers.insert(ACCESS_CONTROL_ALLOW_HEADERS, HeaderValue::from_static(ALLOWED_HEADERS));
    headers.insert(ACCESS_CONTROL_MAX_AGE, HeaderValue::from_static(PREFLIGHT_MAX_AGE));
    headers.append(
        VARY,
        HeaderValue::from_static("Access-Control-Request-Method, Access-Control-Request-Headers"),
    );
    response
}

fn admission(
    guard: &RequestGuard,
    method: &Method,
    headers: &HeaderMap,
    authority: Option<&str>,
    path: &str,
) -> Result<Admitted, Failure> {
    let header = |name| {
        headers
            .get(name)
            .map(|value| value.to_str().map(str::to_ascii_lowercase))
    };
    let mut admitted = Admitted::Everywhere;
    if let Some(hosts) = &guard.hosts {
        // Exactly one `Host`, and an absolute-form target (or HTTP/2's
        // `:authority`) naming the same one: a proxy or tunnel in front may
        // route on a different copy than the one checked here.
        let mut hosts_sent = headers.get_all(HOST).iter();
        let host = match (hosts_sent.next(), hosts_sent.next()) {
            (Some(host), None) => host.to_str().ok().map(str::to_ascii_lowercase),
            _ => None,
        }
        .filter(|host| authority.is_none_or(|authority| authority.eq_ignore_ascii_case(host)));
        let public = |host: &String| {
            guard
                .public_hosts
                .as_deref()
                .is_some_and(|public| public.contains(host))
        };
        match host {
            Some(host) if hosts.contains(&host) => {}
            Some(host) if public(&host) && public_route(method, path) => admitted = Admitted::PublicHost,
            _ => {
                tracing::warn!(event = "guard.host_refused");
                return Err(Failure(ApiError::new(
                    403,
                    "host_not_allowed",
                    "This server only answers requests addressed to localhost, 127.0.0.1 or [::1] and its own port.",
                )));
            }
        }
    }
    match header(ORIGIN) {
        None => Ok(admitted),
        Some(Ok(origin)) if guard.origins.contains(&normalized(&origin)) => Ok(admitted),
        Some(Ok(origin)) if guard.public.as_deref() == Some(normalized(&origin).as_str()) && hosted_connect(path) => {
            Ok(admitted)
        }
        Some(origin) => {
            // Which origin and what kind of route, so a refusal can be traced
            // to its sender. Never the path: hosted Connect's carries the
            // session's reference, a bearer link.
            tracing::warn!(
                event = "guard.origin_refused",
                origin = %loggable_origin(origin.as_deref().ok()),
                method = loggable_method(method),
                route = route_kind(path),
            );
            Err(Failure(ApiError::new(
                403,
                "origin_not_allowed",
                "Requests from this origin aren't allowed. Set ZUPFLOW_CONSOLE_ORIGIN to it if the Console is served there, or add it to ZUPFLOW_ALLOWED_ORIGINS.",
            )))
        }
    }
}

/// An `Origin` header as it may be logged: the browser's serialized origin
/// (scheme, host and port, or `null`), so printable ASCII and short. Anything
/// else is replaced rather than echoed into the log.
fn loggable_origin(origin: Option<&str>) -> String {
    match origin {
        Some(origin)
            if !origin.is_empty() && origin.len() <= 255 && origin.bytes().all(|byte| byte.is_ascii_graphic()) =>
        {
            origin.to_owned()
        }
        _ => "<unreadable>".to_owned(),
    }
}

/// A method as it may be logged: the standard ones by name, any extension
/// method (of any length a client sends) as `other`.
fn loggable_method(method: &Method) -> &'static str {
    match *method {
        Method::GET => "GET",
        Method::HEAD => "HEAD",
        Method::POST => "POST",
        Method::PUT => "PUT",
        Method::PATCH => "PATCH",
        Method::DELETE => "DELETE",
        Method::OPTIONS => "OPTIONS",
        _ => "other",
    }
}

/// The kind of route a request names, for logs that must not carry its path.
fn route_kind(path: &str) -> &'static str {
    if hosted_connect(path) {
        "hosted_connect"
    } else if path.starts_with("/channels/") {
        "channels"
    } else if path.starts_with("/v1/") {
        "api"
    } else {
        "other"
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn logs_origins_only_as_browsers_serialize_them() {
        assert_eq!(loggable_origin(Some("https://app.example")), "https://app.example");
        assert_eq!(loggable_origin(Some("null")), "null");
        assert_eq!(loggable_origin(Some("https://a.example\nforged=1")), "<unreadable>");
        assert_eq!(loggable_origin(Some("https://a.example evil")), "<unreadable>");
        assert_eq!(loggable_origin(Some(&"a".repeat(255))), "a".repeat(255));
        assert_eq!(loggable_origin(Some(&"a".repeat(256))), "<unreadable>");
        assert_eq!(loggable_origin(Some("")), "<unreadable>");
        assert_eq!(loggable_origin(None), "<unreadable>");
    }

    #[test]
    fn logs_extension_methods_as_other() {
        assert_eq!(loggable_method(&Method::POST), "POST");
        let long = Method::from_bytes("X".repeat(4096).as_bytes()).expect("a token is a method");
        assert_eq!(loggable_method(&long), "other");
    }

    #[test]
    fn names_route_kinds_without_their_paths() {
        let reference = "Ab3dEf6hIj9kLm2nOp5qRs8tUv1wXy4z";
        assert_eq!(route_kind(&format!("/connect/{reference}")), "hosted_connect");
        assert_eq!(
            route_kind(&format!("/v1/connect/sessions/{reference}/authorize")),
            "hosted_connect"
        );
        assert_eq!(route_kind("/channels/inbound/abc"), "channels");
        assert_eq!(route_kind("/v1/me"), "api");
        assert_eq!(route_kind("/health"), "other");
    }

    #[test]
    fn matches_each_public_route_exactly() {
        let reference = "Ab3dEf6hIj9kLm2nOp5qRs8tUv1wXy4z";
        for path in [
            format!("/connect/{reference}"),
            "/connect/assets/page.js".to_owned(),
            "/connect/oauth/callback".to_owned(),
            format!("/v1/connect/sessions/{reference}"),
        ] {
            assert!(public_route(&Method::GET, &path), "{path}");
            assert!(public_route(&Method::HEAD, &path), "{path}");
            assert!(!public_route(&Method::POST, &path), "{path}");
        }
        for path in [
            format!("/v1/connect/sessions/{reference}/cancel"),
            format!("/v1/connect/sessions/{reference}/credentials"),
            format!("/v1/connect/sessions/{reference}/authorize"),
            "/channels/inbound/abcdefghijklmnopqrstuvwxyz234567".to_owned(),
        ] {
            assert!(public_route(&Method::POST, &path), "{path}");
            assert!(!public_route(&Method::GET, &path), "{path}");
            assert!(!public_route(&Method::DELETE, &path), "{path}");
        }
        for path in [
            "/",
            "/connect",
            "/connect/",
            "/connect//x",
            "/connect/x/",
            "//connect/x",
            "/connect/.",
            "/connect/..",
            "/connect/a.b",
            "/connect/..%2Fv1%2Fme",
            "/connect/%2e%2e",
            "/CONNECT/x",
            "/Connect/assets/page.js",
            "/connect/assets/PAGE.JS",
            "/connect/oauth/callback/x",
            "/v1/me",
            "/v1/connect/sessions",
            "/v1/connect/sessions/",
            "/v1/connect/sessions/x/../../../me",
            "/v1/connect/sessions/x%2F..%2F..%2Fme",
            "/v1%2Fconnect%2Fsessions%2Fx",
            "/v1/workspaces/ws_x/connect-sessions",
            "/channels/inbound",
            "/channels/inbound/",
            "/channels/inbound/x/y",
            "/channels//inbound/x",
            "/channels/inbound/x/",
            "/Channels/inbound/x",
        ] {
            for method in [Method::GET, Method::POST] {
                assert!(!public_route(&method, path), "{method} {path}");
            }
        }
        assert!(!public_route(
            &Method::GET,
            &format!("/connect/{}", "a".repeat(PARAM_MAX + 1))
        ));
    }

    #[test]
    fn names_the_public_url_as_a_host_sends_it() {
        assert_eq!(
            host_values("https://Tunnel.Example"),
            ["tunnel.example:443", "tunnel.example"]
        );
        assert_eq!(host_values("http://tunnel.example:8081"), ["tunnel.example:8081"]);
        assert_eq!(host_values("http://[::1]:9000"), ["[::1]:9000"]);
        assert!(host_values("not a url").is_empty());
    }

    #[test]
    fn admits_the_public_host_only_with_local_and_only_when_it_isnt_loopback() {
        let local = RequestGuard::local(8080, &[]);
        assert!(
            local
                .clone()
                .with_public_url("http://localhost:8080")
                .public_hosts
                .is_none()
        );
        assert!(
            RequestGuard::with_origins(&[])
                .with_public_url("https://tunnel.example")
                .public_hosts
                .is_none()
        );
        let tunnel = local
            .with_public_url("https://tunnel.example")
            .with_console("http://localhost:5180");
        assert_eq!(
            tunnel.public_hosts.as_deref(),
            Some(&["tunnel.example:443".to_owned(), "tunnel.example".to_owned()][..])
        );
    }
}
