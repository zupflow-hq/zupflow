//! [`ConfiguredModels`]: the model catalog as the deployment's configuration
//! lists it, in `ZUPFLOW_MODELS`. Nothing here calls a model.
//!
//! The value is a JSON array, one object per model, in the order to offer
//! them:
//!
//! ```json
//! [{ "id": "model-a", "name": "Model A", "provider": "Example", "description": "Balanced default." }]
//! ```
//!
//! - `id`: what an agent stores; 1 to 100 characters of letters, digits and
//!   `.`, `-`, `_`, `:`, `/`, `@`; each model's own.
//! - `name`: 1 to 100 characters; `provider`: 1 to 60.
//! - `description`: optional, at most 280 characters.
//!
//! Any other field, or any value outside these, is refused, so a mistyped
//! catalog stops startup instead of offering something else. Unset, or `[]`,
//! offers no models, and creating an agent is refused.

use async_trait::async_trait;
use serde::Deserialize;
use zupflow_core::models::{MAX_ID, Model, ModelCatalog, ModelError};

const MAX_NAME: usize = 100;
const MAX_PROVIDER: usize = 60;
const MAX_DESCRIPTION: usize = 280;

/// The catalog, fixed at startup.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ConfiguredModels {
    models: Vec<Model>,
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ModelsConfigError {
    #[error("must be a JSON array of models like [{{\"id\":…,\"name\":…,\"provider\":…,\"description\":…}}]: {0}")]
    Malformed(String),
    #[error("model {index}: {problem}")]
    Invalid { index: usize, problem: &'static str },
    #[error("model {index}: the ID {id} is listed twice")]
    Duplicate { index: usize, id: String },
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Entry {
    id: String,
    name: String,
    provider: String,
    #[serde(default)]
    description: String,
}

fn valid_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= MAX_ID
        && id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'-' | b'_' | b':' | b'/' | b'@'))
}

/// Text that is present, has no surrounding whitespace and fits.
fn valid_text(value: &str, max: usize) -> bool {
    !value.is_empty() && value.trim() == value && value.chars().count() <= max && !value.chars().any(char::is_control)
}

impl ConfiguredModels {
    /// The catalog `ZUPFLOW_MODELS` describes; see the module documentation.
    pub fn parse(value: &str) -> Result<Self, ModelsConfigError> {
        let entries: Vec<Entry> =
            serde_json::from_str(value).map_err(|error| ModelsConfigError::Malformed(error.to_string()))?;
        let mut models: Vec<Model> = Vec::with_capacity(entries.len());
        for (index, entry) in entries.into_iter().enumerate() {
            let invalid = |problem| ModelsConfigError::Invalid { index, problem };
            if !valid_id(&entry.id) {
                return Err(invalid("id must be 1 to 100 letters, digits and . - _ : / @"));
            }
            if !valid_text(&entry.name, MAX_NAME) {
                return Err(invalid("name must be 1 to 100 characters, with no surrounding spaces"));
            }
            if !valid_text(&entry.provider, MAX_PROVIDER) {
                return Err(invalid(
                    "provider must be 1 to 60 characters, with no surrounding spaces",
                ));
            }
            if !(entry.description.is_empty() || valid_text(&entry.description, MAX_DESCRIPTION)) {
                return Err(invalid(
                    "description must be at most 280 characters, with no surrounding spaces",
                ));
            }
            if models.iter().any(|model| model.id == entry.id) {
                return Err(ModelsConfigError::Duplicate { index, id: entry.id });
            }
            models.push(Model {
                id: entry.id,
                name: entry.name,
                provider: entry.provider,
                description: entry.description,
            });
        }
        Ok(Self { models })
    }

    /// No models: nothing can be offered, and no agent created.
    pub fn none() -> Self {
        Self::default()
    }
}

#[async_trait]
impl ModelCatalog for ConfiguredModels {
    async fn list_models(&self) -> Result<Vec<Model>, ModelError> {
        Ok(self.models.clone())
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;

    #[tokio::test]
    async fn conforms() {
        let models = ConfiguredModels::parse(
            r#"[
                {"id":"model-b","name":"Model B","provider":"Example","description":"Second."},
                {"id":"model-a","name":"Model A","provider":"Example"}
            ]"#,
        )
        .unwrap();
        zupflow_conformance::model_catalog::run(&models, &["model-b", "model-a"]).await;
        zupflow_conformance::model_catalog::run(&ConfiguredModels::none(), &[]).await;
        zupflow_conformance::model_catalog::run(&ConfiguredModels::parse("[]").unwrap(), &[]).await;
    }

    #[test]
    fn keeps_what_the_configuration_says_in_its_order() {
        let models = ConfiguredModels::parse(
            r#"[{"id":"vendor/model-1@2026","name":"One","provider":"Example","description":"The first."}]"#,
        )
        .unwrap();
        assert_eq!(
            models.models,
            [Model {
                id: "vendor/model-1@2026".to_owned(),
                name: "One".to_owned(),
                provider: "Example".to_owned(),
                description: "The first.".to_owned(),
            }]
        );
    }

    #[test]
    fn refuses_anything_it_would_have_to_guess_at() {
        let model = |fields: &str| format!(r#"[{{{fields}}}]"#);
        for bad in [
            String::new(),
            "{}".to_owned(),
            "[1]".to_owned(),
            r#"["model-a"]"#.to_owned(),
            model(r#""id":"a","name":"A""#),
            model(r#""id":"a","name":"A","provider":"P","price":1"#),
            model(r#""id":"","name":"A","provider":"P""#),
            model(r#""id":"has space","name":"A","provider":"P""#),
            model(&format!(r#""id":"{}","name":"A","provider":"P""#, "a".repeat(101))),
            model(r#""id":"a","name":"","provider":"P""#),
            model(r#""id":"a","name":" A","provider":"P""#),
            model(r#""id":"a","name":"A","provider":"""#),
            model(&format!(
                r#""id":"a","name":"A","provider":"P","description":"{}""#,
                "d".repeat(281)
            )),
            r#"[{"id":"a","name":"A","provider":"P"},{"id":"a","name":"B","provider":"P"}]"#.to_owned(),
        ] {
            assert!(ConfiguredModels::parse(&bad).is_err(), "{bad}");
        }
    }
}
