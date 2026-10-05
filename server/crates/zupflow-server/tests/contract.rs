//! What the server takes from the contract, held equal to the committed
//! `openapi.json`.

#![allow(clippy::unwrap_used)]

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
