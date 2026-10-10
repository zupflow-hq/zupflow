//! The models an agent can be set to use, as a deployment offers them.
//!
//! Only the list: an agent names a model by its ID, and creating or changing
//! one is refused for an ID the catalog doesn't offer. Calling a model, and
//! the providers and keys that takes, is another decision.

use async_trait::async_trait;
use serde::Serialize;

/// The longest model ID kept.
pub const MAX_ID: usize = 100;

/// A model an agent can use: the contract's `Model`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Model {
    /// What an agent stores in `model`.
    pub id: String,
    pub name: String,
    /// Who serves it, as people know them.
    pub provider: String,
    pub description: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum ModelError {
    #[error("the model catalog is unavailable")]
    Unavailable,
}

#[async_trait]
pub trait ModelCatalog: Send + Sync {
    /// Every model offered, in the order the deployment lists them. Empty
    /// when it offers none: then no agent can be created.
    async fn list_models(&self) -> Result<Vec<Model>, ModelError>;

    /// Whether `id` is one of [`ModelCatalog::list_models`], exactly.
    async fn offers(&self, id: &str) -> Result<bool, ModelError> {
        Ok(self.list_models().await?.iter().any(|model| model.id == id))
    }
}
