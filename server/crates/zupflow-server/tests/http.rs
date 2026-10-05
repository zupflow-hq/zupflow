//! The HTTP surface outside any domain: health, readiness, and the envelope
//! for paths and methods the API doesn't serve.

use axum::body::Body;
use axum::http::{Method, Request, StatusCode};
use http_body_util::BodyExt;
use serde_json::{Value, json};
use tower::ServiceExt;

async fn call(method: Method, uri: &str) -> (StatusCode, Value) {
    let request = Request::builder()
        .method(method)
        .uri(uri)
        .body(Body::empty())
        .expect("a valid request");
    let response = zupflow_server::app()
        .oneshot(request)
        .await
        .expect("the router always answers");
    let status = response.status();
    let bytes = response
        .into_body()
        .collect()
        .await
        .expect("a readable body")
        .to_bytes();
    (status, serde_json::from_slice(&bytes).expect("a JSON body"))
}

#[tokio::test]
async fn reports_health_and_readiness() {
    assert_eq!(
        call(Method::GET, "/health").await,
        (StatusCode::OK, json!({ "status": "ok" }))
    );
    assert_eq!(
        call(Method::GET, "/ready").await,
        (StatusCode::OK, json!({ "status": "ok" }))
    );
}

#[tokio::test]
async fn answers_unknown_paths_with_the_error_envelope() {
    for uri in ["/v1/nothing", "/", "/v1/workspaces/ws_031DCp7XMZ9AHIB2uF6QgS"] {
        let (status, body) = call(Method::GET, uri).await;
        assert_eq!(status, StatusCode::NOT_FOUND, "{uri}");
        assert_eq!(
            body,
            json!({
                "data": null,
                "meta": null,
                "error": { "code": "not_found", "message": "There's nothing at this path.", "fields": null },
            }),
        );
    }
}

#[tokio::test]
async fn answers_an_unsupported_method_with_the_error_envelope() {
    let (status, body) = call(Method::POST, "/health").await;
    assert_eq!(status, StatusCode::METHOD_NOT_ALLOWED);
    assert_eq!(body["error"]["code"], "method_not_allowed");
    assert_eq!(body["data"], Value::Null);
}
