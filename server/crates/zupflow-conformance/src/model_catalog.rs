//! The `ModelCatalog` suite: the models listed are the ones configured, in
//! their order, each ID once; `offers` answers for exactly those IDs, compared
//! as they are; and an empty catalog offers nothing.

use zupflow_core::models::ModelCatalog;

/// `expected`: the IDs the catalog was configured with, in order.
pub async fn run(catalog: &dyn ModelCatalog, expected: &[&str]) {
    let models = catalog.list_models().await.unwrap();
    let ids: Vec<&str> = models.iter().map(|model| model.id.as_str()).collect();
    assert_eq!(ids, expected, "lists exactly the configured models, in order");
    for model in &models {
        assert!(!model.name.is_empty(), "{} has a name", model.id);
        assert!(!model.provider.is_empty(), "{} names its provider", model.id);
        assert!(catalog.offers(&model.id).await.unwrap(), "offers {}", model.id);
        // IDs are compared exactly: no case folding, no trimming, no prefixes.
        for near in [
            model.id.to_uppercase(),
            format!(" {}", model.id),
            format!("{} ", model.id),
            format!("{}x", model.id),
            model
                .id
                .chars()
                .take(model.id.chars().count().saturating_sub(1))
                .collect(),
        ] {
            if near != model.id && !expected.contains(&near.as_str()) {
                assert!(!catalog.offers(&near).await.unwrap(), "doesn't offer {near:?}");
            }
        }
    }
    let mut unique = ids.clone();
    unique.sort_unstable();
    unique.dedup();
    assert_eq!(unique.len(), ids.len(), "each ID once");
    assert!(!catalog.offers("").await.unwrap(), "an empty ID is never offered");
    assert!(
        !catalog.offers("not-a-configured-model").await.unwrap(),
        "an ID it wasn't given is never offered"
    );
    assert_eq!(catalog.list_models().await.unwrap(), models, "the same list every time");
}
