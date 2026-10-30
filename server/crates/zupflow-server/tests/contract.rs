//! What the server takes from the contract, held equal to the committed
//! `openapi.json`.

#![allow(clippy::unwrap_used)]

mod common;

use std::collections::BTreeSet;
use std::path::Path;

/// The routes that never save their body for an idempotent retry are exactly
/// the ones the contract marks `x-zupflow-shows-secret-once`.
#[test]
fn knows_every_route_that_shows_a_secret_once() {
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../../packages/contract/openapi.json");
    let document: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap();
    let marked: BTreeSet<String> = document["paths"]
        .as_object()
        .unwrap()
        .iter()
        .filter(|(_, operations)| operations["post"]["x-zupflow-shows-secret-once"] == true)
        .map(|(path, _)| path.clone())
        .collect();
    let known: BTreeSet<String> = zupflow_server::idempotency::SHOWS_SECRET_ONCE
        .iter()
        .map(|path| (*path).to_owned())
        .collect();
    assert_eq!(known, marked);
}

/// The routes whose answer is never kept for an idempotent retry, as it holds
/// what people wrote, are exactly the ones the contract marks
/// `x-zupflow-answer-never-kept`.
#[test]
fn knows_every_route_whose_answer_is_never_kept() {
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../../packages/contract/openapi.json");
    let document: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap();
    let marked: BTreeSet<String> = document["paths"]
        .as_object()
        .unwrap()
        .iter()
        .filter(|(_, operations)| operations["post"]["x-zupflow-answer-never-kept"] == true)
        .map(|(path, _)| path.clone())
        .collect();
    let known: BTreeSet<String> = zupflow_server::idempotency::ANSWER_NEVER_KEPT
        .iter()
        .map(|path| (*path).to_owned())
        .collect();
    assert!(!known.is_empty());
    assert_eq!(known, marked);
}

/// The `POST` routes an API key passes straight through are exactly the ones
/// the contract doesn't open to API keys.
#[test]
fn knows_every_post_route_an_api_key_cannot_call() {
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../../packages/contract/openapi.json");
    let document: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap();
    let not_for_keys: BTreeSet<String> = document["paths"]
        .as_object()
        .unwrap()
        .iter()
        .filter(|(_, operations)| {
            operations["post"]["x-zupflow-callers"]
                .as_array()
                .is_some_and(|callers| !callers.iter().any(|caller| caller == "api_key"))
        })
        .map(|(path, _)| path.clone())
        .collect();
    let known: BTreeSet<String> = zupflow_server::idempotency::NOT_FOR_API_KEYS
        .iter()
        .map(|path| (*path).to_owned())
        .collect();
    assert_eq!(known, not_for_keys);
}

/// The routes the middleware leaves to their provider are exactly the ones
/// only a provider calls.
#[test]
fn knows_every_route_a_provider_calls() {
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../../packages/contract/openapi.json");
    let document: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap();
    let provider: BTreeSet<String> = document["paths"]
        .as_object()
        .unwrap()
        .iter()
        .filter(|(_, operations)| {
            operations["post"]["x-zupflow-callers"]
                .as_array()
                .is_some_and(|callers| callers.iter().all(|caller| caller == "provider"))
        })
        .map(|(path, _)| path.clone())
        .collect();
    let known: BTreeSet<String> = zupflow_server::idempotency::PROVIDER_ROUTES
        .iter()
        .map(|path| (*path).to_owned())
        .collect();
    assert_eq!(known, provider);
}

/// `SERVED_OPERATIONS` is exactly what the router answers: asked without a
/// credential, a served route answers 401 (or, for hosted Connect, its own
/// 404, and for a public route, 200) and any other contract route the API's 404 or 405, so the list can't claim a route that isn't there or miss
/// one that is.
#[tokio::test]
async fn serves_exactly_the_operations_it_lists() {
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../../packages/contract/openapi.json");
    let document: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap();
    let server = common::Server::new().await;
    let listed: BTreeSet<&str> = zupflow_server::SERVED_OPERATIONS.into_iter().collect();
    let mut answered = BTreeSet::new();
    let mut every = BTreeSet::new();
    for (template, operations) in document["paths"].as_object().unwrap() {
        for (method, operation) in operations.as_object().unwrap() {
            let id = operation["operationId"].as_str().unwrap().to_owned();
            every.insert(id.clone());
            // Any value fills a parameter: the credential is checked before IDs are read.
            let uri: String = template
                .split('/')
                .map(|segment| if segment.starts_with('{') { "x" } else { segment })
                .collect::<Vec<_>>()
                .join("/");
            let request = axum::http::Request::builder()
                .method(method.to_uppercase().as_str())
                .uri(&uri)
                .body(axum::body::Body::empty())
                .unwrap();
            let response = tower::ServiceExt::oneshot(server.app.clone(), request).await.unwrap();
            let status = response.status().as_u16();
            let body = http_body_util::BodyExt::collect(response.into_body())
                .await
                .unwrap()
                .to_bytes();
            let message = serde_json::from_slice::<serde_json::Value>(&body).unwrap_or_default()["error"]["message"]
                .as_str()
                .unwrap_or_default()
                .to_owned();
            let public = operation["x-zupflow-callers"] == serde_json::json!(["public"]);
            match status {
                401 => {
                    answered.insert(id);
                }
                // A public route needs no credential at all: it answers.
                200 if public => {
                    answered.insert(id);
                }
                // Hosted Connect needs no credential: its route answers that
                // the reference names no session, not the router's 404.
                404 if message != "There's nothing at this path." => {
                    answered.insert(id);
                }
                404 | 405 => {}
                other => panic!("{method} {uri} answered {other}"),
            }
        }
    }
    let answered: BTreeSet<&str> = answered.iter().map(String::as_str).collect();
    assert_eq!(listed, answered);
    assert!(
        listed.iter().all(|id| every.contains(*id)),
        "every listed operation is in the contract"
    );
}
