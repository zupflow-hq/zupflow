//! The backend's ID registry is the contract's: the same prefixes, from the
//! generated `openapi.json` that `@zupflow/contract` commits. So are the
//! webhook event types an endpoint subscribes to.

use std::collections::BTreeSet;
use std::path::Path;

use zupflow_core::ids::REGISTRY;
use zupflow_core::webhooks::WebhookEventType;

fn openapi() -> serde_json::Value {
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../../packages/contract/openapi.json");
    let text = std::fs::read_to_string(&path).expect("the contract's openapi.json is committed beside the server");
    serde_json::from_str(&text).expect("openapi.json is JSON")
}

fn pascal(name: &str) -> String {
    name.split('_')
        .map(|part| {
            let mut chars = part.chars();
            chars
                .next()
                .map(|c| c.to_ascii_uppercase().to_string() + chars.as_str())
                .unwrap_or_default()
        })
        .collect()
}

#[test]
fn registers_exactly_the_contract_prefixes() {
    let doc = openapi();
    let pattern = doc["components"]["schemas"]["AnyId"]["pattern"]
        .as_str()
        .expect("AnyId has a pattern");
    // `^(org|ws|…)_[0-9A-Za-z]{22}$`
    let list = pattern
        .strip_prefix("^(")
        .and_then(|rest| rest.split_once(")_"))
        .map(|(list, _)| list)
        .expect("AnyId's pattern lists the prefixes");
    let contract: BTreeSet<&str> = list.split('|').collect();
    let ours: BTreeSet<&str> = REGISTRY.iter().map(|(_, prefix)| *prefix).collect();
    assert_eq!(ours, contract);
}

#[test]
fn names_each_kind_as_the_contract_does() {
    let doc = openapi();
    let schemas = doc["components"]["schemas"].as_object().expect("components.schemas");
    let mut checked = 0;
    for (name, prefix) in REGISTRY {
        // Kinds not yet referenced by any endpoint have no component of their own.
        let Some(schema) = schemas.get(&format!("{}Id", pascal(name))) else {
            continue;
        };
        let expected = format!("^{prefix}_[0-9A-Za-z]{{22}}$");
        assert_eq!(schema["pattern"].as_str(), Some(expected.as_str()), "{name}");
        checked += 1;
    }
    assert!(checked >= 17, "only {checked} kinds had a component to compare with");
}

#[test]
fn subscribes_to_exactly_the_contract_event_types() {
    let doc = openapi();
    let listed = doc["components"]["schemas"]["WebhookEndpoint"]["properties"]["events"]["items"]["enum"]
        .as_array()
        .expect("an endpoint's events list the contract's types");
    let contract: Vec<&str> = listed.iter().filter_map(serde_json::Value::as_str).collect();
    let ours: Vec<&str> = WebhookEventType::ALL.iter().map(|kind| kind.as_str()).collect();
    assert_eq!(ours, contract, "the same types, in the contract's order");
}
