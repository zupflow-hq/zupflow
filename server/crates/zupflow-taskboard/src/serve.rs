//! Serving Taskboard over HTTPS on loopback, with a certificate made for
//! its name at start.

use std::sync::Arc;

use axum::body::{Body, Bytes};
use axum::extract::{FromRequest, Request, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use hyper_util::rt::{TokioExecutor, TokioIo};
use hyper_util::server::conn::auto;
use hyper_util::service::TowerToHyperService;
use rustls::pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer};
use tokio::net::TcpListener;
use tokio::task::JoinHandle;
use tokio_rustls::TlsAcceptor;

use crate::{Options, Taskboard, WebhookRequest, mcp, oauth, pages};

/// Taskboard, serving. Dropping it stops it.
pub struct Running {
    pub taskboard: Taskboard,
    pub port: u16,
    /// The certificate it serves (self-signed, for its name alone), as PEM:
    /// what a client trusts to reach it.
    pub certificate_pem: String,
    task: JoinHandle<()>,
}

impl Drop for Running {
    fn drop(&mut self) {
        self.task.abort();
    }
}

#[derive(Debug)]
pub enum StartError {
    /// Not a lowercase name under `.example`.
    NotReserved(String),
    Certificate(String),
    Listen(std::io::Error),
}

impl std::fmt::Display for StartError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NotReserved(host) => write!(
                f,
                "{host} isn't a lowercase name under .example: Taskboard only ever uses the reserved domain"
            ),
            Self::Certificate(why) => write!(f, "can't make a certificate: {why}"),
            Self::Listen(error) => write!(f, "can't listen on loopback: {error}"),
        }
    }
}

impl std::error::Error for StartError {}

/// A DNS name ending in `.example`: lowercase letters, digits and inner
/// hyphens, at most 253 bytes. The same rule as `zupflow-outbound`'s for
/// local apps, so any name Taskboard serves under, the backend can reach.
fn reserved(host: &str) -> bool {
    host.strip_suffix(".example").is_some_and(|rest| {
        !rest.is_empty()
            && host.len() <= 253
            && rest.split('.').all(|label| {
                !label.is_empty()
                    && label.len() <= 63
                    && !label.starts_with('-')
                    && !label.ends_with('-')
                    && label
                        .bytes()
                        .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-')
            })
    })
}

fn router(taskboard: Taskboard) -> Router {
    Router::new()
        .route("/", get(pages::home))
        .route("/dev/expire-access-tokens", post(pages::expire_access_tokens))
        .route("/dev/revoke-tokens", post(pages::revoke_tokens))
        .route("/.well-known/oauth-protected-resource", get(oauth::protected_resource))
        .route(
            "/.well-known/oauth-protected-resource/mcp",
            get(oauth::protected_resource),
        )
        .route(
            "/.well-known/oauth-authorization-server",
            get(oauth::authorization_server),
        )
        .route("/webhooks", post(webhook))
        .route("/register", post(oauth::register))
        .route("/authorize", get(oauth::authorize).post(oauth::decide))
        .route("/token", post(oauth::token))
        .route("/me", get(oauth::me))
        .route("/mcp", post(mcp::post).get(mcp::get).delete(mcp::delete))
        .fallback(oauth::not_found)
        .with_state(taskboard)
}

async fn webhook(State(board): State<Taskboard>, request: Request) -> Response {
    let headers = request.headers().clone();
    let body = match Bytes::from_request(request, &board).await {
        Ok(body) => body,
        Err(error) => return error.into_response(),
    };
    // Keep the exact bytes for signature checks while retaining Json's
    // content-type and parse validation before recording a request.
    let mut request = Request::new(Body::from(body.clone()));
    *request.headers_mut() = headers.clone();
    let Json(event) = match Json::<serde_json::Value>::from_request(request, &board).await {
        Ok(event) => event,
        Err(error) => return error.into_response(),
    };
    let header = |name| {
        headers
            .get(name)
            .and_then(|value| value.to_str().ok())
            .map(str::to_owned)
    };
    let mut state = board.state();
    state.webhooks.push(event);
    state.webhook_requests.push(WebhookRequest {
        body: body.to_vec(),
        signature: header("zupflow-signature"),
        event_id: header("zupflow-event-id"),
        event_type: header("zupflow-event-type"),
        attempt: header("zupflow-delivery-attempt"),
    });
    state
        .webhook_status
        .and_then(|status| StatusCode::from_u16(status).ok())
        .unwrap_or(StatusCode::NO_CONTENT)
        .into_response()
}

/// Starts Taskboard on `127.0.0.1`, reached at `https://{host}:{port}`.
pub async fn start(options: Options) -> Result<Running, StartError> {
    if !reserved(&options.host) {
        return Err(StartError::NotReserved(options.host));
    }
    let certified = rcgen::generate_simple_self_signed(vec![options.host.clone()])
        .map_err(|error| StartError::Certificate(error.to_string()))?;
    let certificate_pem = certified.cert.pem();
    let chain = vec![CertificateDer::from(certified.cert.der().to_vec())];
    let key = PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(certified.signing_key.serialize_der()));
    let config = rustls::ServerConfig::builder_with_provider(Arc::new(rustls::crypto::aws_lc_rs::default_provider()))
        .with_safe_default_protocol_versions()
        .map_err(|error| StartError::Certificate(error.to_string()))?
        .with_no_client_auth()
        .with_single_cert(chain, key)
        .map_err(|error| StartError::Certificate(error.to_string()))?;
    let acceptor = TlsAcceptor::from(Arc::new(config));

    let listener = TcpListener::bind(("127.0.0.1", options.port))
        .await
        .map_err(StartError::Listen)?;
    let port = listener.local_addr().map_err(StartError::Listen)?.port();
    let taskboard = Taskboard::new(format!("https://{}:{port}", options.host), options.access_token_ttl);
    let app = router(taskboard.clone());
    let task = tokio::spawn(async move {
        loop {
            let Ok((stream, _)) = listener.accept().await else {
                continue;
            };
            let acceptor = acceptor.clone();
            let app = app.clone();
            tokio::spawn(async move {
                // A client that doesn't trust the certificate just goes away.
                let Ok(stream) = acceptor.accept(stream).await else {
                    return;
                };
                let _ = auto::Builder::new(TokioExecutor::new())
                    .serve_connection(TokioIo::new(stream), TowerToHyperService::new(app))
                    .await;
            });
        }
    });
    Ok(Running {
        taskboard,
        port,
        certificate_pem,
        task,
    })
}

#[cfg(test)]
mod tests {
    use super::reserved;

    #[test]
    fn uses_only_the_reserved_example_domain() {
        for (host, ok) in [
            ("taskboard.example", true),
            ("tasks.dev.example", true),
            ("example", false),
            ("taskboard.example.com", false),
            ("taskboard.example.", false),
            (format!("{}example", "a.".repeat(123)).as_str(), true),
            (format!("{}example", "a.".repeat(124)).as_str(), false),
            ("taskboard.localhost", false),
            ("Taskboard.example", false),
        ] {
            assert_eq!(reserved(host), ok, "{host}");
        }
    }
}
