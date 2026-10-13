//! [`ScriptedProviders`]: the basic [`ModelProviderCatalog`], for tests,
//! local runs and the contract's reference server. It calls no provider: each
//! kind lists a fixed catalog, the same one the Console's mock API lists, and
//! a key is checked by a fixed rule.
//!
//! - A key containing [`REFUSED_KEY_MARKER`], in any case, is refused, as a
//!   provider refuses a revoked key. Every other key is accepted.
//! - OpenAI's and Anthropic's lists name no prices; OpenRouter's names both
//!   prices and context sizes. OpenAI and OpenRouter each list models that
//!   can't call tools.
//!
//! [`ScriptedProviders::tool_models`] names every model that can call tools,
//! so the scripted model can serve each one an organization may add.

use async_trait::async_trait;
use zupflow_core::model_providers::{
    CatalogError, ModelPricing, ModelProviderCatalog, ProviderKey, ProviderKind, ProviderModel,
};

/// Recorded with failures.
pub const NAME: &str = "scripted";

/// The text that makes the scripted providers refuse a key, in any case.
pub const REFUSED_KEY_MARKER: &str = "invalid";

#[derive(Debug, Clone, Copy, Default)]
pub struct ScriptedProviders;

fn price(input: f64, output: f64) -> Option<ModelPricing> {
    Some(ModelPricing {
        input_per_million_usd: input,
        output_per_million_usd: output,
    })
}

fn entry(
    provider_model_id: &str,
    name: &str,
    author: &str,
    context_window: Option<u64>,
    supports_tools: bool,
    pricing: Option<ModelPricing>,
) -> ProviderModel {
    ProviderModel {
        provider_model_id: provider_model_id.to_owned(),
        name: name.to_owned(),
        author: author.to_owned(),
        context_window,
        supports_tools,
        pricing,
    }
}

/// What each provider lists, in its own order.
pub fn catalog(kind: ProviderKind) -> Vec<ProviderModel> {
    match kind {
        ProviderKind::OpenAi => openai(),
        ProviderKind::Anthropic => anthropic(),
        ProviderKind::OpenRouter => openrouter(),
    }
}

fn openai() -> Vec<ProviderModel> {
    vec![
        entry("gpt-5", "GPT-5", "openai", Some(400_000), true, None),
        entry("gpt-5-mini", "GPT-5 mini", "openai", Some(400_000), true, None),
        entry("gpt-5-nano", "GPT-5 nano", "openai", Some(400_000), true, None),
        entry("gpt-4.1", "GPT-4.1", "openai", Some(1_047_576), true, None),
        entry("gpt-4.1-mini", "GPT-4.1 mini", "openai", Some(1_047_576), true, None),
        entry("gpt-4o", "GPT-4o", "openai", Some(128_000), true, None),
        entry("gpt-4o-mini", "GPT-4o mini", "openai", Some(128_000), true, None),
        entry("o3", "o3", "openai", Some(200_000), true, None),
        entry("o4-mini", "o4-mini", "openai", Some(200_000), true, None),
        entry(
            "text-embedding-3-large",
            "Text embedding 3 large",
            "openai",
            Some(8_191),
            false,
            None,
        ),
        entry("gpt-image-1", "GPT Image 1", "openai", None, false, None),
    ]
}

fn anthropic() -> Vec<ProviderModel> {
    vec![
        entry(
            "claude-sonnet-4-5",
            "Claude Sonnet 4.5",
            "anthropic",
            Some(200_000),
            true,
            None,
        ),
        entry(
            "claude-haiku-4-5",
            "Claude Haiku 4.5",
            "anthropic",
            Some(200_000),
            true,
            None,
        ),
        entry(
            "claude-opus-4-1",
            "Claude Opus 4.1",
            "anthropic",
            Some(200_000),
            true,
            None,
        ),
        entry(
            "claude-opus-4-0",
            "Claude Opus 4",
            "anthropic",
            Some(200_000),
            true,
            None,
        ),
        entry(
            "claude-sonnet-4-0",
            "Claude Sonnet 4",
            "anthropic",
            Some(200_000),
            true,
            None,
        ),
        entry(
            "claude-3-7-sonnet-latest",
            "Claude Sonnet 3.7",
            "anthropic",
            Some(200_000),
            true,
            None,
        ),
        entry(
            "claude-3-5-haiku-latest",
            "Claude Haiku 3.5",
            "anthropic",
            Some(200_000),
            true,
            None,
        ),
    ]
}

fn openrouter() -> Vec<ProviderModel> {
    vec![
        entry(
            "openai/gpt-5",
            "GPT-5",
            "openai",
            Some(400_000),
            true,
            price(1.25, 10.0),
        ),
        entry(
            "openai/gpt-5-mini",
            "GPT-5 mini",
            "openai",
            Some(400_000),
            true,
            price(0.25, 2.0),
        ),
        entry(
            "openai/gpt-5-nano",
            "GPT-5 nano",
            "openai",
            Some(400_000),
            true,
            price(0.05, 0.4),
        ),
        entry(
            "openai/gpt-4.1",
            "GPT-4.1",
            "openai",
            Some(1_047_576),
            true,
            price(2.0, 8.0),
        ),
        entry(
            "openai/gpt-4o-mini",
            "GPT-4o mini",
            "openai",
            Some(128_000),
            true,
            price(0.15, 0.6),
        ),
        entry("openai/o3", "o3", "openai", Some(200_000), true, price(2.0, 8.0)),
        entry(
            "openai/o4-mini",
            "o4-mini",
            "openai",
            Some(200_000),
            true,
            price(1.1, 4.4),
        ),
        entry(
            "anthropic/claude-sonnet-4.5",
            "Claude Sonnet 4.5",
            "anthropic",
            Some(1_000_000),
            true,
            price(3.0, 15.0),
        ),
        entry(
            "anthropic/claude-haiku-4.5",
            "Claude Haiku 4.5",
            "anthropic",
            Some(200_000),
            true,
            price(1.0, 5.0),
        ),
        entry(
            "anthropic/claude-opus-4.1",
            "Claude Opus 4.1",
            "anthropic",
            Some(200_000),
            true,
            price(15.0, 75.0),
        ),
        entry(
            "anthropic/claude-sonnet-4",
            "Claude Sonnet 4",
            "anthropic",
            Some(1_000_000),
            true,
            price(3.0, 15.0),
        ),
        entry(
            "anthropic/claude-3.5-haiku",
            "Claude Haiku 3.5",
            "anthropic",
            Some(200_000),
            true,
            price(0.8, 4.0),
        ),
        entry(
            "google/gemini-2.5-pro",
            "Gemini 2.5 Pro",
            "google",
            Some(1_048_576),
            true,
            price(1.25, 10.0),
        ),
        entry(
            "google/gemini-2.5-flash",
            "Gemini 2.5 Flash",
            "google",
            Some(1_048_576),
            true,
            price(0.3, 2.5),
        ),
        entry(
            "google/gemini-2.5-flash-lite",
            "Gemini 2.5 Flash Lite",
            "google",
            Some(1_048_576),
            true,
            price(0.1, 0.4),
        ),
        entry(
            "google/gemma-3-27b-it",
            "Gemma 3 27B",
            "google",
            Some(131_072),
            false,
            price(0.07, 0.26),
        ),
        entry("x-ai/grok-4", "Grok 4", "x-ai", Some(256_000), true, price(3.0, 15.0)),
        entry(
            "x-ai/grok-code-fast-1",
            "Grok Code Fast 1",
            "x-ai",
            Some(256_000),
            true,
            price(0.2, 1.5),
        ),
        entry(
            "meta-llama/llama-4-maverick",
            "Llama 4 Maverick",
            "meta-llama",
            Some(1_048_576),
            true,
            price(0.15, 0.6),
        ),
        entry(
            "meta-llama/llama-3.3-70b-instruct",
            "Llama 3.3 70B Instruct",
            "meta-llama",
            Some(131_072),
            true,
            price(0.13, 0.39),
        ),
        entry(
            "mistralai/mistral-medium-3.1",
            "Mistral Medium 3.1",
            "mistralai",
            Some(131_072),
            true,
            price(0.4, 2.0),
        ),
        entry(
            "mistralai/devstral-medium",
            "Devstral Medium",
            "mistralai",
            Some(131_072),
            true,
            price(0.4, 2.0),
        ),
        entry(
            "mistralai/mistral-7b-instruct",
            "Mistral 7B Instruct",
            "mistralai",
            Some(32_768),
            false,
            price(0.03, 0.05),
        ),
        entry(
            "deepseek/deepseek-chat-v3.1",
            "DeepSeek V3.1",
            "deepseek",
            Some(163_840),
            true,
            price(0.2, 0.8),
        ),
        entry(
            "deepseek/deepseek-r1-0528",
            "DeepSeek R1 0528",
            "deepseek",
            Some(163_840),
            true,
            price(0.4, 1.75),
        ),
        entry(
            "qwen/qwen3-coder",
            "Qwen3 Coder",
            "qwen",
            Some(262_144),
            true,
            price(0.22, 0.95),
        ),
        entry(
            "qwen/qwen3-235b-a22b-2507",
            "Qwen3 235B A22B",
            "qwen",
            Some(262_144),
            true,
            price(0.13, 0.6),
        ),
        entry(
            "moonshotai/kimi-k2-0905",
            "Kimi K2",
            "moonshotai",
            Some(262_144),
            true,
            price(0.38, 1.52),
        ),
        entry(
            "z-ai/glm-4.5",
            "GLM 4.5",
            "z-ai",
            Some(131_072),
            true,
            price(0.35, 1.55),
        ),
        entry(
            "nousresearch/hermes-3-llama-3.1-405b",
            "Hermes 3 405B",
            "nousresearch",
            Some(131_072),
            false,
            price(1.0, 1.0),
        ),
    ]
}

impl ScriptedProviders {
    /// Whether the scripted providers accept `key`.
    pub fn accepts(key: &ProviderKey) -> bool {
        !key.expose().to_lowercase().contains(REFUSED_KEY_MARKER)
    }

    /// Every provider model ID, of every kind, that can call tools: what the
    /// scripted model serves.
    pub fn tool_models() -> Vec<String> {
        let mut ids: Vec<String> = ProviderKind::ALL
            .into_iter()
            .flat_map(catalog)
            .filter(|model| model.supports_tools)
            .map(|model| model.provider_model_id)
            .collect();
        ids.sort_unstable();
        ids.dedup();
        ids
    }
}

#[async_trait]
impl ModelProviderCatalog for ScriptedProviders {
    fn name(&self) -> &'static str {
        NAME
    }

    async fn list_models(&self, kind: ProviderKind, key: &ProviderKey) -> Result<Vec<ProviderModel>, CatalogError> {
        if Self::accepts(key) {
            Ok(catalog(kind))
        } else {
            Err(CatalogError::KeyRefused)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn passes_the_catalog_suite() {
        zupflow_conformance::model_provider_catalog::run(
            &ScriptedProviders,
            &ProviderKey::new("sk-test-long-suite-0000-xx".to_owned()),
            &ProviderKey::new("sk-test-long-INVALID-0000".to_owned()),
        )
        .await;
    }

    #[test]
    fn lists_what_the_mock_lists() {
        assert_eq!(
            ProviderKind::ALL.map(|kind| catalog(kind).len()),
            [11, 7, 30],
            "the mock's catalogs, entry for entry"
        );
        let toolless: Vec<String> = ProviderKind::ALL
            .into_iter()
            .flat_map(catalog)
            .filter(|model| !model.supports_tools)
            .map(|model| model.provider_model_id)
            .collect();
        assert_eq!(
            toolless,
            [
                "text-embedding-3-large",
                "gpt-image-1",
                "google/gemma-3-27b-it",
                "mistralai/mistral-7b-instruct",
                "nousresearch/hermes-3-llama-3.1-405b"
            ]
        );
        assert!(
            catalog(ProviderKind::OpenAi)
                .iter()
                .all(|model| model.pricing.is_none())
        );
        assert!(
            catalog(ProviderKind::OpenRouter)
                .iter()
                .all(|model| model.pricing.is_some())
        );
        let tools = ScriptedProviders::tool_models();
        assert!(tools.contains(&"claude-sonnet-4-5".to_owned()));
        assert!(tools.contains(&"openai/gpt-5".to_owned()));
        assert!(!tools.contains(&"gpt-image-1".to_owned()));
    }
}
