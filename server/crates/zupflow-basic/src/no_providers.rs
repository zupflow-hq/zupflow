//! [`NoProviders`]: the model provider catalog of a deployment that reaches
//! no providers. It lists nothing and checks no key: every call is
//! `NotConfigured`, so no connection can be made rather than one saved with
//! a key nobody checked.

use async_trait::async_trait;
use zupflow_core::model_providers::{CatalogError, ModelProviderCatalog, ProviderKey, ProviderKind, ProviderModel};

/// Recorded with failures.
pub const NAME: &str = "none";

#[derive(Debug, Clone, Copy, Default)]
pub struct NoProviders;

#[async_trait]
impl ModelProviderCatalog for NoProviders {
    fn name(&self) -> &'static str {
        NAME
    }

    async fn list_models(&self, _: ProviderKind, _: &ProviderKey) -> Result<Vec<ProviderModel>, CatalogError> {
        Err(CatalogError::NotConfigured)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn lists_nothing_and_checks_no_key() {
        let key = ProviderKey::new("sk-test-long-suite-0000-xx".to_owned());
        for kind in ProviderKind::ALL {
            assert_eq!(
                NoProviders.list_models(kind, &key).await,
                Err(CatalogError::NotConfigured)
            );
        }
    }
}
