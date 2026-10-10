//! [`NoModelProvider`]: what answers runs when a deployment configured no
//! model provider. It answers none: every turn is `NotConfigured`, so a run
//! fails rather than being answered by something that isn't a model.

use async_trait::async_trait;
use zupflow_core::models::{Completion, ModelProvider, ProviderError, Turn};

/// Recorded with failures.
pub const NAME: &str = "none";

#[derive(Debug, Clone, Copy, Default)]
pub struct NoModelProvider;

#[async_trait]
impl ModelProvider for NoModelProvider {
    fn name(&self) -> &'static str {
        NAME
    }

    async fn complete(&self, _: &Turn<'_>) -> Result<Completion, ProviderError> {
        Err(ProviderError::NotConfigured)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn answers_no_turn() {
        let messages = [zupflow_core::models::Message::User { text: "hi".to_owned() }];
        let turn = Turn {
            model: "model-a",
            instructions: "",
            messages: &messages,
            tools: &[],
        };
        assert_eq!(NoModelProvider.complete(&turn).await, Err(ProviderError::NotConfigured));
    }
}
