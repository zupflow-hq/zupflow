//! Refuses requests a browser was tricked into sending.
//!
//! Loopback isn't private from the browser: any page the user visits can send
//! requests to `127.0.0.1`, and DNS rebinding can make the browser treat the
//! server as the attacker's own origin and read the answers. That matters most
//! with `local` sign-in, where every request is the owner. So:
//!
//! - **Host.** With `local`, a request must name the server by a loopback name
//!   and its own port (`localhost`, `127.0.0.1` or `[::1]`). A rebound name
//!   (`evil.example` resolving to 127.0.0.1) is refused.
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
            origins: allowed.into(),
            public: None,
            console: None,
        }
    }

    /// Any host; browser requests only from `origins`.
    pub fn with_origins(origins: &[String]) -> Self {
        Self {
            hosts: None,
            origins: origins.iter().map(|origin| normalized(origin)).collect(),
            public: None,
            console: None,
        }
    }

    /// Admits the server's own public origin, `ZUPFLOW_PUBLIC_URL`, for
    /// hosted Connect's paths only: the page is served there and calls the
    /// hosted Connect API from it. Same-origin, so no CORS.
    #[must_use]
    pub fn with_public_url(self, public_url: &str) -> Self {
        let origin = normalized(public_url);
        // Already allowed everywhere (the loopback origin with `local`, say).
        if self.origins.contains(&origin) {
            return self;
        }
        Self {
            public: Some(origin.into()),
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
            hosts: self.hosts,
            origins: origins.into(),
            public: self.public,
            console: Some(origin.into()),
        }
    }
}

fn normalized(origin: &str) -> String {
    origin.trim_end_matches('/').to_ascii_lowercase()
}

/// Hosted Connect's paths: the page and its files, the OAuth callback, and
/// the hosted Connect API, which the link's reference authorizes.
fn hosted_connect(path: &str) -> bool {
    path.starts_with("/connect/") || path.starts_with("/v1/connect/sessions/")
}

pub async fn guard(State(guard): State<RequestGuard>, request: Request, next: Next) -> Response {
    // Decided before awaiting: the request is borrowed only here.
    if let Some(refusal) = refusal(&guard, request.headers(), request.uri().path()) {
        return refusal.into_response();
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

fn refusal(guard: &RequestGuard, headers: &HeaderMap, path: &str) -> Option<Failure> {
    let header = |name| {
        headers
            .get(name)
            .map(|value| value.to_str().map(str::to_ascii_lowercase))
    };
    if let Some(hosts) = &guard.hosts {
        let allowed = matches!(header(HOST), Some(Ok(host)) if hosts.contains(&host));
        if !allowed {
            tracing::warn!(event = "guard.host_refused");
            return Some(Failure(ApiError::new(
                403,
                "host_not_allowed",
                "This server only answers requests addressed to localhost, 127.0.0.1 or [::1] and its own port.",
            )));
        }
    }
    match header(ORIGIN) {
        None => None,
        Some(Ok(origin)) if guard.origins.contains(&normalized(&origin)) => None,
        Some(Ok(origin)) if guard.public.as_deref() == Some(normalized(&origin).as_str()) && hosted_connect(path) => {
            None
        }
        Some(_) => {
            tracing::warn!(event = "guard.origin_refused");
            Some(Failure(ApiError::new(
                403,
                "origin_not_allowed",
                "Requests from this origin aren't allowed. Set ZUPFLOW_CONSOLE_ORIGIN to it if the Console is served there, or add it to ZUPFLOW_ALLOWED_ORIGINS.",
            )))
        }
    }
}
