//! Live tests against the real providers, opt-in only: each runs when its
//! key is set, and passes without asking anything otherwise. Never set in
//! CI. They reach the provider through the real `SafeHttpClient` and run
//! the same suites the fake passes:
//!
//! - `ZUPFLOW_LIVE_OPENAI_KEY` (and `ZUPFLOW_LIVE_OPENAI_MODEL`, default `gpt-5-mini`)
//! - `ZUPFLOW_LIVE_ANTHROPIC_KEY` (and `ZUPFLOW_LIVE_ANTHROPIC_MODEL`, default `claude-haiku-4-5`)
//! - `ZUPFLOW_LIVE_OPENROUTER_KEY` (and `ZUPFLOW_LIVE_OPENROUTER_MODEL`, default `openai/gpt-5-mini`)
//!
//! Each also sends one conversation twice, the second time one turn longer,
//! with instructions long enough for every provider to cache, and prints
//! the tokens each turn read from the cache; for Anthropic and Claude
//! through OpenRouter, which cache at the breakpoints the adapters mark,
//! the second turn must read the first from the cache.
//!
//! A call costs a few cents at most. Run one with
//! `ZUPFLOW_LIVE_OPENROUTER_KEY=… cargo test -p zupflow-providers --test live -- --nocapture`.

#![allow(clippy::unwrap_used)]

use std::sync::Arc;

use zupflow_core::model_providers::{ProviderKey, ProviderKind};
use zupflow_core::models::{Message, ModelProvider, ProviderAccess, Reply, ToolSpec, Turn};
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
    let provider = HttpModelProvider::new(outbound);
    zupflow_conformance::model_provider::run(&provider, &ProviderAccess { kind, key: &key }, &refused, &model).await;
    caches(&provider, kind, &key, &model).await;
}

/// Two turns of one conversation: the second reads the first from the
/// cache where the adapter marks breakpoints.
async fn caches(provider: &HttpModelProvider, kind: ProviderKind, key: &ProviderKey, model: &str) {
    // Past every provider's least cacheable prompt (4,096 tokens at most).
    let instructions: String = (0..1200)
        .map(|n| format!("Rule {n}: answer in one short sentence.\n"))
        .collect();
    let tools = [ToolSpec {
        name: "lookup".to_owned(),
        description: "Looks a word up.".to_owned(),
        input_schema: serde_json::json!({ "type": "object", "properties": { "word": { "type": "string" } } }),
    }];
    let cache_key = "zf_live0000000000000000000000000000";
    let mut messages = vec![Message::User {
        text: "Say hello.".to_owned(),
    }];
    let mut usages = Vec::new();
    for _ in 0..2 {
        let turn = Turn {
            model,
            instructions: &instructions,
            messages: &messages,
            tools: &tools,
            cache_key: Some(cache_key),
        };
        let answer = provider.complete(&ProviderAccess { kind, key }, &turn).await.unwrap();
        usages.push(answer.usage);
        let text = match answer.reply {
            Reply::Text(text) => Some(text),
            Reply::ToolCalls { text, .. } => text,
        };
        messages.push(Message::Assistant {
            text: Some(text.unwrap_or_else(|| "Hello.".to_owned())),
            tool_calls: Vec::new(),
        });
        messages.push(Message::User {
            text: "Say it again.".to_owned(),
        });
    }
    eprintln!("{} {model}: {usages:?}", kind.name());
    let at_breakpoints =
        kind == ProviderKind::Anthropic || (kind == ProviderKind::OpenRouter && model.starts_with("anthropic/"));
    if at_breakpoints {
        assert!(
            usages[1].cache_read_input_tokens > 0,
            "the second turn read the first from the cache: {usages:?}"
        );
    }
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
