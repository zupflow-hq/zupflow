//! Model providers (decisions 0049, 0050): the services an organization
//! brings its own API key for, and what each one lists for a key.
//!
//! [`ModelProviderCatalog`] asks a provider, with a connection's opened key,
//! for the models it lists. That is also how a key is checked before it's
//! saved: a provider that refuses to list is refusing the key. A deployment's
//! adapters reach providers through `OutboundHttp`; this repository ships
//! only a scripted catalog (`zupflow-basic`), which calls nothing.
//!
//! A key is held as a [`ProviderKey`]: wiped on drop, never printed, and
//! given to an adapter only to send.

use std::fmt;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use zeroize::Zeroizing;

/// The providers a connection can reach: the contract's `MODEL_PROVIDER_KINDS`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProviderKind {
    #[serde(rename = "openai")]
    OpenAi,
    Anthropic,
    #[serde(rename = "openrouter")]
    OpenRouter,
}

impl ProviderKind {
    pub const ALL: [Self; 3] = [Self::OpenAi, Self::Anthropic, Self::OpenRouter];

    pub fn as_str(self) -> &'static str {
        match self {
            Self::OpenAi => "openai",
            Self::Anthropic => "anthropic",
            Self::OpenRouter => "openrouter",
        }
    }

    pub fn parse(value: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|kind| kind.as_str() == value)
    }

    /// The provider's name, as people know it.
    pub fn name(self) -> &'static str {
        match self {
            Self::OpenAi => "OpenAI",
            Self::Anthropic => "Anthropic",
            Self::OpenRouter => "OpenRouter",
        }
    }

    /// Serves other authors' models: a model added through it is shown "via" it.
    pub fn routes_to_others(self) -> bool {
        matches!(self, Self::OpenRouter)
    }
}

/// The shortest key accepted, in characters: anything shorter wasn't pasted whole.
pub const MIN_KEY: usize = 20;
/// The longest key accepted, in characters.
pub const MAX_KEY: usize = 500;

/// A provider's API key, as pasted (trimmed). Wiped from memory when
/// dropped, and never printed: `Debug` shows its length only.
#[derive(Clone)]
pub struct ProviderKey(Zeroizing<String>);

impl ProviderKey {
    pub fn new(key: String) -> Self {
        Self(Zeroizing::new(key))
    }

    /// From an opened sealed value. `None` unless it's UTF-8.
    pub fn from_bytes(bytes: &[u8]) -> Option<Self> {
        std::str::from_utf8(bytes).ok().map(|key| Self::new(key.to_owned()))
    }

    /// The key itself, for the adapter that sends it and for sealing.
    pub fn expose(&self) -> &str {
        &self.0
    }

    /// Its last four characters, which responses show.
    pub fn last4(&self) -> String {
        let chars: Vec<char> = self.0.chars().collect();
        chars[chars.len().saturating_sub(4)..].iter().collect()
    }
}

impl fmt::Debug for ProviderKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "ProviderKey({} characters)", self.0.chars().count())
    }
}

/// Dollars per million tokens, as the provider lists them.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct ModelPricing {
    pub input_per_million_usd: f64,
    pub output_per_million_usd: f64,
}

/// A model a provider lists for a key: the contract's `ProviderModel`,
/// without what the organization added.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ProviderModel {
    /// The provider's own ID, sent to it as is.
    pub provider_model_id: String,
    pub name: String,
    /// Who made it, whoever serves it.
    pub author: String,
    /// Tokens; `None` when the provider doesn't say.
    pub context_window: Option<u64>,
    pub supports_tools: bool,
    /// `None` when the provider doesn't list prices.
    pub pricing: Option<ModelPricing>,
}

impl ProviderModel {
    /// Whether `q` (trimmed, any case) is in its ID, name or author; an empty
    /// `q` matches every model.
    pub fn matches(&self, q: &str) -> bool {
        let needle = q.trim().to_lowercase();
        needle.is_empty()
            || [&self.provider_model_id, &self.name, &self.author]
                .iter()
                .any(|value| value.to_lowercase().contains(&needle))
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum CatalogError {
    /// The provider refused the key (it answered `401` or `403`).
    #[error("the provider refused the key")]
    KeyRefused,
    /// No answer, or not one worth reading: worth trying again later.
    #[error("the provider is unavailable")]
    Unavailable,
    /// This deployment reaches no providers.
    #[error("no model provider catalog is configured")]
    NotConfigured,
}

/// Asks a provider what it lists for a key. Replaceable whole: a deployment
/// brings one adapter per kind, each through `OutboundHttp`; the basic
/// implementation is scripted.
///
/// Every implementation lists the same models, in the same order, for the
/// same kind and an accepted key; refuses a key the provider refuses
/// (`KeyRefused`), whatever the kind; and never puts the key in what it
/// answers or logs.
#[async_trait]
pub trait ModelProviderCatalog: Send + Sync {
    /// Recorded with failures.
    fn name(&self) -> &'static str;

    /// Every model the provider lists for `key`, in its own order. A key it
    /// refuses is `KeyRefused`: this is how a key is checked before it's saved.
    async fn list_models(&self, kind: ProviderKind, key: &ProviderKey) -> Result<Vec<ProviderModel>, CatalogError>;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_each_provider_as_the_contract_does() {
        for kind in ProviderKind::ALL {
            assert_eq!(ProviderKind::parse(kind.as_str()), Some(kind));
            assert_eq!(
                serde_json::to_value(kind).ok(),
                Some(serde_json::Value::String(kind.as_str().to_owned()))
            );
        }
        assert_eq!(ProviderKind::parse("OpenAI"), None);
        assert!(ProviderKind::OpenRouter.routes_to_others());
        assert!(!ProviderKind::Anthropic.routes_to_others());
    }

    #[test]
    fn never_prints_a_key() {
        let key = ProviderKey::new("sk-secret-value-1234".to_owned());
        assert_eq!(format!("{key:?}"), "ProviderKey(20 characters)");
        assert_eq!(key.last4(), "1234");
        assert_eq!(ProviderKey::new("ab".to_owned()).last4(), "ab");
    }

    #[test]
    fn searches_id_name_and_author() {
        let model = ProviderModel {
            provider_model_id: "openai/gpt-5".to_owned(),
            name: "GPT-5".to_owned(),
            author: "openai".to_owned(),
            context_window: None,
            supports_tools: true,
            pricing: None,
        };
        assert!(model.matches(""));
        assert!(model.matches("  gpt "));
        assert!(model.matches("OPENAI"));
        assert!(!model.matches("claude"));
    }
}
