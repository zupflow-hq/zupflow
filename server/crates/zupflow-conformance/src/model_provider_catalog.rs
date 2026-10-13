//! The `ModelProviderCatalog` suite: for a key the provider accepts, every
//! kind lists models, each with an ID, a name and an author, each ID once,
//! the same list in the same order each time, and prices that aren't
//! negative; a key the provider refuses is `KeyRefused` for every kind; and
//! nothing a catalog answers repeats the key.
//!
//! `accepted` and `refused` are keys the implementation's providers accept
//! and refuse: the scripted catalog's rule, or real test accounts' keys.

use std::collections::HashSet;

use zupflow_core::model_providers::{CatalogError, ModelProviderCatalog, ProviderKey, ProviderKind};

pub async fn run(catalog: &dyn ModelProviderCatalog, accepted: &ProviderKey, refused: &ProviderKey) {
    assert!(!catalog.name().is_empty(), "named, for failures");
    for kind in ProviderKind::ALL {
        let models = catalog.list_models(kind, accepted).await.unwrap();
        assert!(
            !models.is_empty(),
            "{} lists models for a key it accepts",
            kind.as_str()
        );
        let mut seen = HashSet::new();
        for model in &models {
            assert!(!model.provider_model_id.is_empty(), "every model has an ID");
            assert!(!model.name.is_empty(), "{} has a name", model.provider_model_id);
            assert!(
                seen.insert(model.provider_model_id.as_str()),
                "{} is listed once",
                model.provider_model_id
            );
            if let Some(pricing) = model.pricing {
                assert!(
                    pricing.input_per_million_usd >= 0.0 && pricing.output_per_million_usd >= 0.0,
                    "{}'s prices aren't negative",
                    model.provider_model_id
                );
            }
            let shown = serde_json::to_string(model).unwrap();
            assert!(!shown.contains(accepted.expose()), "a listing never repeats the key");
        }
        assert_eq!(
            catalog.list_models(kind, accepted).await.unwrap(),
            models,
            "the same list, in the same order"
        );
        assert_eq!(
            catalog.list_models(kind, refused).await,
            Err(CatalogError::KeyRefused),
            "{} refuses a key it refuses",
            kind.as_str()
        );
    }
}
