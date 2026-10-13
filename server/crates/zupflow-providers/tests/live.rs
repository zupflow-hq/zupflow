//! Live tests against the real providers, opt-in only: each runs when its
//! key is set, and passes without asking anything otherwise. Never set in
//! CI. They reach the provider through the real `SafeHttpClient` and run
//! the same suites the fake passes:
//!
//! - `ZUPFLOW_LIVE_OPENAI_KEY` (and `ZUPFLOW_LIVE_OPENAI_MODEL`, default `gpt-5-mini`)
//! - `ZUPFLOW_LIVE_ANTHROPIC_KEY` (and `ZUPFLOW_LIVE_ANTHROPIC_MODEL`, default `claude-haiku-4-5`)
//! - `ZUPFLOW_LIVE_OPENROUTER_KEY` (and `ZUPFLOW_LIVE_OPENROUTER_MODEL`, default `openai/gpt-5-mini`)
//!
//! A call costs a few cents at most. Run one with
//! `ZUPFLOW_LIVE_OPENROUTER_KEY=… cargo test -p zupflow-providers --test live -- --nocapture`.

#![allow(clippy::unwrap_used)]

use std::sync::Arc;

use zupflow_core::model_providers::{ProviderKey, ProviderKind};
use zupflow_core::models::ProviderAccess;
use zupflow_providers::{HttpModelProvider, HttpProviderCatalog};

/// A key no provider accepts, of the shape they take.
const REFUSED: &str = "sk-zupflow-live-test-refused-0000000000";

async fn live(kind: ProviderKind, default_model: &str) {
    let name = kind.as_str().to_uppercase();
    let Ok(key) = std::env::var(format!("ZUPFLOW_LIVE_{name}_KEY")) else {
        eprintln!(
            "ZUPFLOW_LIVE_{name}_KEY is unset: the live {} test asks nothing",
            kind.name()
        );
        return;
    };
    let model = std::env::var(format!("ZUPFLOW_LIVE_{name}_MODEL")).unwrap_or_else(|_| default_model.to_owned());
    let outbound = Arc::new(zupflow_outbound::SafeHttpClient::new().unwrap());
    let key = ProviderKey::new(key.trim().to_owned());
    let refused = ProviderKey::new(REFUSED.to_owned());
    zupflow_conformance::model_provider_catalog::run_kind(
        &HttpProviderCatalog::new(outbound.clone()),
        kind,
        &key,
        &refused,
    )
    .await;
    zupflow_conformance::model_provider::run(
        &HttpModelProvider::new(outbound),
        &ProviderAccess { kind, key: &key },
        &refused,
        &model,
    )
    .await;
}

#[tokio::test]
async fn openai() {
    live(ProviderKind::OpenAi, "gpt-5-mini").await;
}

#[tokio::test]
async fn anthropic() {
    live(ProviderKind::Anthropic, "claude-haiku-4-5").await;
}

#[tokio::test]
async fn openrouter() {
    live(ProviderKind::OpenRouter, "openai/gpt-5-mini").await;
}
