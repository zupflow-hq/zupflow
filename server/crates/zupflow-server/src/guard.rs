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
//!   server's own loopback origin.
//! - **Content type.** Request bodies must be `application/json` (checked
//!   where a body is read, in `input.rs`), so a cross-site form or `text/plain`
//!   post can't carry one without a preflight.

use std::sync::Arc;

use axum::extract::{Request, State};
use axum::http::HeaderMap;
use axum::http::header::{HOST, ORIGIN};
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
}

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
        }
    }

    /// Any host; browser requests only from `origins`.
    pub fn with_origins(origins: &[String]) -> Self {
        Self {
            hosts: None,
            origins: origins.iter().map(|origin| normalized(origin)).collect(),
        }
    }
}

fn normalized(origin: &str) -> String {
    origin.trim_end_matches('/').to_ascii_lowercase()
}

pub async fn guard(State(guard): State<RequestGuard>, request: Request, next: Next) -> Response {
    // Decided before awaiting: the request is borrowed only here.
    if let Some(refusal) = refusal(&guard, request.headers()) {
        return refusal.into_response();
    }
    next.run(request).await
}

fn refusal(guard: &RequestGuard, headers: &HeaderMap) -> Option<Failure> {
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
        Some(_) => {
            tracing::warn!(event = "guard.origin_refused");
            Some(Failure(ApiError::new(
                403,
                "origin_not_allowed",
                "Requests from this origin aren't allowed. Add it to ZUPFLOW_ALLOWED_ORIGINS.",
            )))
        }
    }
}
