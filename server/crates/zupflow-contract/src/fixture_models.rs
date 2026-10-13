//! The fixture's model providers, as the mock seeds them: each
//! organization's connections, the models added from them and each
//! environment's default, made through the model store with keys sealed as
//! the server seals them. Keys are fictional, of the shape the scripted
//! providers accept; the refused connection's key contains `invalid` and is
//! recorded as refused, as the server records it once the provider says so.

use zupflow_core::audit::{AuditAction, AuditActor, AuditEntry, AuditObject, AuditPlace, AuditTarget};
use zupflow_core::environment::Environment;
use zupflow_core::ids::{MemberId, ModelConnectionId, ModelId, OrganizationId, UserId};
use zupflow_core::keys::{EncryptionContext, KeyService, seal};
use zupflow_core::model_providers::{ProviderKey, ProviderKind};
use zupflow_core::model_store::{
    DefaultModel, FIRST_KEY_VERSION, Model, ModelConnection, ModelManager, ModelStore, NewConnection, NewModel,
    SealedKey,
};
use zupflow_core::time::Timestamp;

/// One connection and the provider models added from it.
pub struct ConnectionSeed {
    pub environment: Environment,
    pub kind: ProviderKind,
    pub label: &'static str,
    /// The key's last four characters, as the mock shows them.
    pub last4: &'static str,
    /// The provider refused the key.
    pub refused: bool,
    pub models: &'static [&'static str],
}

/// The models seeded, by environment, connection label and provider model ID.
pub struct Seeded(Vec<(Environment, &'static str, &'static str, ModelId)>);

impl Seeded {
    pub fn model(&self, environment: Environment, label: &str, provider_model_id: &str) -> Result<ModelId, String> {
        self.0
            .iter()
            .find(|(each, each_label, each_id, _)| {
                *each == environment && *each_label == label && *each_id == provider_model_id
            })
            .map(|(.., id)| *id)
            .ok_or_else(|| format!("no seeded {provider_model_id} from {label} in {}", environment.name()))
    }
}

fn by_fixture(action: AuditAction, target: AuditTarget, environment: Environment) -> AuditEntry {
    AuditEntry::new(
        AuditActor::System {
            name: "Fixture".to_owned(),
        },
        action,
        target,
        AuditPlace::Environment(environment),
    )
}

/// Connects `connections` in `organization_id` as its owner `manager`, and
/// adds their models.
pub async fn seed(
    store: &dyn ModelStore,
    sealing: &dyn KeyService,
    organization_id: OrganizationId,
    manager: MemberId,
    connections: &[ConnectionSeed],
) -> Result<Seeded, String> {
    let fail = |what: &str, error: &dyn std::fmt::Debug| format!("{what}: {error:?}");
    let catalog = zupflow_basic::scripted_providers::catalog;
    let mut seeded = Vec::new();
    for spec in connections {
        let marker = if spec.refused { "invalid" } else { "fixture" };
        let key = ProviderKey::new(format!("sk-{marker}-{}-000000{}", spec.kind.as_str(), spec.last4));
        let connection_id = ModelConnectionId::generate();
        let context =
            EncryptionContext::model_provider_key(organization_id, spec.environment, connection_id, FIRST_KEY_VERSION);
        let sealed = seal(sealing, &context, key.expose().as_bytes())
            .await
            .map_err(|error| fail(spec.label, &error))?;
        let connection = store
            .create_connection(
                organization_id,
                connection_id,
                ModelManager::Member(manager),
                &NewConnection {
                    environment: spec.environment,
                    kind: spec.kind,
                    label: spec.label.to_owned(),
                    key: SealedKey {
                        sealed,
                        last4: key.last4(),
                    },
                },
                Timestamp::now(),
                &|created: &ModelConnection| {
                    by_fixture(
                        AuditAction::ModelConnectionCreated,
                        AuditTarget::new(AuditObject::ModelConnection(created.id), &created.label),
                        created.environment,
                    )
                },
            )
            .await
            .map_err(|error| fail(spec.label, &error))?;
        for provider_model_id in spec.models {
            let listed = catalog(spec.kind)
                .into_iter()
                .find(|listed| listed.provider_model_id == *provider_model_id)
                .ok_or_else(|| format!("{} lists no {provider_model_id}", spec.kind.as_str()))?;
            let model = store
                .create_model(
                    organization_id,
                    spec.environment,
                    ModelId::generate(),
                    ModelManager::Member(manager),
                    &NewModel {
                        connection_id: connection.id,
                        provider_model_id: listed.provider_model_id,
                        name: listed.name,
                        author: listed.author,
                        context_window: listed.context_window,
                        supports_tools: listed.supports_tools,
                    },
                    Timestamp::now(),
                    &|added: &Model| {
                        by_fixture(
                            AuditAction::ModelCreated,
                            AuditTarget::new(AuditObject::Model(added.id), &added.name),
                            added.environment,
                        )
                    },
                )
                .await
                .map_err(|error| fail(provider_model_id, &error))?;
            seeded.push((spec.environment, spec.label, *provider_model_id, model.id));
        }
        if spec.refused {
            store
                .mark_key_refused(organization_id, spec.environment, connection.id, FIRST_KEY_VERSION)
                .await
                .map_err(|error| fail(spec.label, &error))?;
        }
    }
    Ok(Seeded(seeded))
}

/// Makes `model_id` the default of `environment`, chosen by its owner.
pub async fn set_default(
    store: &dyn ModelStore,
    organization_id: OrganizationId,
    environment: Environment,
    manager: MemberId,
    owner: UserId,
    model_id: ModelId,
) -> Result<(), String> {
    store
        .set_default_model(
            organization_id,
            environment,
            ModelManager::Member(manager),
            Some(model_id),
            Some(owner),
            Timestamp::now(),
            &|default: &DefaultModel| {
                let model = default.model.as_ref();
                by_fixture(
                    AuditAction::DefaultModelUpdated,
                    AuditTarget::new(
                        AuditObject::DefaultModel(model.map(|model| model.id)),
                        model.map_or("None", |model| model.name.as_str()),
                    ),
                    environment,
                )
            },
        )
        .await
        .map(drop)
        .map_err(|error| format!("default of {}: {error:?}", environment.name()))
}
