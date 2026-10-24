//! What the request guard logs when it refuses a browser's origin. In its own
//! binary: the log is captured with a thread-local subscriber, and tracing
//! caches each callsite's interest process-wide, so another test reaching the
//! same callsite first could hide the line from it.

#![allow(clippy::unwrap_used)]

mod common;

use std::io::Write;
use std::sync::{Arc, Mutex};

use axum::body::Body;
use axum::http::{Request, StatusCode};
use common::Server;
use tower::ServiceExt;
use zupflow_server::RequestGuard;

#[derive(Clone, Default)]
struct Captured(Arc<Mutex<Vec<u8>>>);

impl Write for Captured {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(bytes);
        Ok(bytes.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

/// A refusal names the origin, the method and the kind of route, never the
/// path: hosted Connect's carries the session's reference, a bearer link.
#[tokio::test]
async fn names_the_refused_origin_and_never_the_link() {
    let captured = Captured::default();
    let writer = captured.clone();
    let subscriber = tracing_subscriber::fmt()
        .json()
        .with_max_level(tracing::Level::TRACE)
        .with_writer(move || writer.clone())
        .finish();
    let _default = tracing::subscriber::set_default(subscriber);

    let server = Server::new().await;
    let app = zupflow_server::app(
        server.state.clone(),
        RequestGuard::with_origins(&[]).with_public_url("https://connect.northwind.example"),
    );
    let reference = "Ab3dEf6hIj9kLm2nOp5qRs8tUv1wXy4z";
    let request = Request::builder()
        .method("POST")
        .uri(format!("/v1/connect/sessions/{reference}/cancel"))
        .header("Origin", "https://elsewhere.example")
        .body(Body::empty())
        .unwrap();
    let refused = app.oneshot(request).await.unwrap();
    assert_eq!(refused.status(), StatusCode::FORBIDDEN);

    let logged = String::from_utf8(captured.0.lock().unwrap().clone()).unwrap();
    let refusal = logged
        .lines()
        .find(|line| line.contains("guard.origin_refused"))
        .expect("the refusal is logged");
    assert!(refusal.contains(r#""origin":"https://elsewhere.example""#), "{refusal}");
    assert!(refusal.contains(r#""route":"hosted_connect""#), "{refusal}");
    assert!(refusal.contains(r#""method":"POST""#), "{refusal}");
    assert!(!logged.contains(reference), "{logged}");
}
