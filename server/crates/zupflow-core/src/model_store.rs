//! Organizations' models (decisions 0049, 0050): connections to model
//! providers with the organization's own key, the models added from them,
//! and the default model of each environment.
//!
//! Everything here belongs to one organization and one environment. A
//! connection's models and the default live in its environment, and nothing
//! falls back to the other's. An agent may override the default with a model
//! of its workspace's environment; a run resolves its model once, when it's
//! admitted ([`ModelStore::run_model`]), and the run engine checks the route
//! again before each model attempt ([`ModelStore::route_state`]).
//!
//! A connection's key is sealed under its `model_provider_key` context, one
//! value per key version; only its last four characters are ever shown.
//!
//! Every changing method takes the acting [`ModelManager`] and checks, inside
//! its own transaction, that they may still manage models: an active owner
//! or admin, or an unrevoked API key of the environment. The rules that hold
//! across concurrent changes (a model in use can't be removed, a default is a
//! model of its environment) are checked there too.

use async_trait::async_trait;
use serde::Serialize;

use crate::audit::Audit;
use crate::environment::Environment;
use crate::ids::{ApiKeyId, MemberId, ModelConnectionId, ModelId, OrganizationId, UserId, WorkspaceId};
use crate::keys::Sealed;
use crate::model_providers::ProviderKind;
use crate::organizations::User;
use crate::time::Timestamp;
use crate::workspaces::Page;

/// The longest label or model name, in characters.
pub const MAX_NAME: usize = 60;
/// The longest provider model ID kept, in characters.
pub const MAX_PROVIDER_MODEL_ID: usize = 200;
/// How many agents a refusal names before it counts the rest.
pub const NAMED_HOLDERS: usize = 5;
/// The most models one page holds, and how many it holds by default.
pub const MAX_PAGE: u32 = 100;
pub const DEFAULT_PAGE: u32 = 25;

/// `invalid_key`: the provider refused the key; replacing it makes the
/// connection `active` again.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ConnectionStatus {
    Active,
    InvalidKey,
}

impl ConnectionStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Active => "active",
            Self::InvalidKey => "invalid_key",
        }
    }

    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "active" => Some(Self::Active),
            "invalid_key" => Some(Self::InvalidKey),
            _ => None,
        }
    }
}

/// The contract's `SecretState` for a connection's key: always set.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct KeyState {
    pub set: bool,
    pub last4: Option<String>,
    pub updated_at: Option<Timestamp>,
}

/// The contract's `ModelConnection`. Never the key.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ModelConnection {
    pub id: ModelConnectionId,
    pub organization_id: OrganizationId,
    pub environment: Environment,
    pub kind: ProviderKind,
    pub label: String,
    pub api_key: KeyState,
    pub key_version: u32,
    pub status: ConnectionStatus,
    /// Models added from it, counted when read.
    pub model_count: u64,
    pub created_at: Timestamp,
    pub updated_at: Timestamp,
}

/// A connection's key as stored, for the code that opens it: sealed under
/// `EncryptionContext::model_provider_key` at `version`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConnectionKey {
    pub version: u32,
    pub sealed: Sealed,
}

/// The first key version; each replacement seals the next.
pub const FIRST_KEY_VERSION: u32 = 1;

/// A key to store: sealed at the version it's stored as, with its last four
/// characters for display.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SealedKey {
    pub sealed: Sealed,
    pub last4: String,
}

/// What a connection is created with, checked by the caller; its key sealed
/// at [`FIRST_KEY_VERSION`].
#[derive(Debug, Clone)]
pub struct NewConnection {
    pub environment: Environment,
    pub kind: ProviderKind,
    pub label: String,
    pub key: SealedKey,
}

/// A replacement key, sealed at `expected_version + 1`: stored only while
/// the connection's key is still at `expected_version`.
#[derive(Debug, Clone)]
pub struct KeyReplacement {
    pub expected_version: u32,
    pub key: SealedKey,
}

/// The contract's `ModelStatus`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ModelStatus {
    Available,
    Unavailable,
}

impl ModelStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Available => "available",
            Self::Unavailable => "unavailable",
        }
    }

    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "available" => Some(Self::Available),
            "unavailable" => Some(Self::Unavailable),
            _ => None,
        }
    }
}

/// The contract's `ModelRoute`: the connection a model is reached through.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ModelRoute {
    pub connection_id: ModelConnectionId,
    pub kind: ProviderKind,
    pub label: String,
}

/// The contract's `Model`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Model {
    pub id: ModelId,
    pub organization_id: OrganizationId,
    pub environment: Environment,
    pub route: ModelRoute,
    pub provider_model_id: String,
    pub name: String,
    pub author: String,
    pub context_window: Option<u64>,
    pub supports_tools: bool,
    pub status: ModelStatus,
    /// The environment's default, read with it.
    pub is_default: bool,
    /// Agents that override the default with it, counted when read.
    pub agent_count: u64,
    pub created_at: Timestamp,
    pub updated_at: Timestamp,
}

/// A model to add from a connection, as the provider lists it, checked by
/// the caller.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NewModel {
    pub connection_id: ModelConnectionId,
    pub provider_model_id: String,
    pub name: String,
    pub author: String,
    pub context_window: Option<u64>,
    pub supports_tools: bool,
}

/// The contract's `DefaultModel`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct DefaultModel {
    pub environment: Environment,
    pub model: Option<Model>,
    pub updated_at: Option<Timestamp>,
    /// `None` before anyone chose, when an API key did, or once the person is gone.
    pub updated_by: Option<User>,
}

/// What a list of models asks for.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ModelFilter {
    pub environment: Option<Environment>,
    pub connection_id: Option<ModelConnectionId>,
    /// Matches the name, the provider's model ID or the author, any case.
    pub q: Option<String>,
    /// A filter naming something that can't exist (a malformed ID): an empty list.
    pub matches_nothing: bool,
}

/// An authenticated caller whose current authority a change rechecks
/// inside its transaction. The caller cannot supply this through the API.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ModelManager {
    Member(MemberId),
    ApiKey(ApiKeyId),
}

/// An agent holding a model, by the names people know.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HoldingAgent {
    pub agent: String,
    pub workspace: String,
}

/// What keeps models from being removed.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ModelHolders {
    /// The environment's default, by name, when it's one of them.
    pub default_model: Option<String>,
    /// The first [`NAMED_HOLDERS`] agents overriding the default with one of
    /// them, by name, then workspace and agent ID.
    pub agents: Vec<HoldingAgent>,
    /// How many agents in all.
    pub agent_total: u64,
}

impl ModelHolders {
    pub fn is_empty(&self) -> bool {
        self.default_model.is_none() && self.agent_total == 0
    }
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ModelStoreError {
    #[error("the store is unavailable")]
    Unavailable,
    /// The connection isn't in this organization and environment.
    #[error("model connection not found")]
    ConnectionNotFound,
    /// The model isn't in this organization and environment.
    #[error("model not found")]
    ModelNotFound,
    /// The manager can no longer manage models here.
    #[error("the caller can no longer manage models")]
    NotAllowed,
    /// The connection already has this provider model.
    #[error("the model is already added from this connection")]
    ModelExists,
    /// A default naming no model of its organization and environment.
    #[error("not a model of this environment")]
    UnknownModel,
    /// The models are in use.
    #[error("the models are in use")]
    InUse(ModelHolders),
    /// The key was replaced since it was read.
    #[error("the key changed meanwhile")]
    KeyChanged,
    #[error("not a cursor from this list")]
    InvalidCursor,
}

/// The model a run is admitted with: the organization's model, the
/// connection it's reached through, and the provider's ID sent to it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RunModel {
    pub model_id: ModelId,
    pub connection_id: ModelConnectionId,
    pub provider_model_id: String,
}

/// Whether a run's model can be called now.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, serde::Deserialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum RouteState {
    /// Both are there, and the key is at this version.
    Usable { key_version: u32 },
    /// The model or its connection was removed, or the model is now another
    /// connection's.
    Removed,
    /// The provider refused the connection's key.
    KeyInvalid { connection_label: String },
}

/// Connections, models and defaults. Every method takes the organization
/// first and reaches nothing outside it; a method of one environment's
/// objects takes the environment next, and finds nothing of the other's.
/// Every change records its [`Audit`] in its own transaction.
#[async_trait]
pub trait ModelStore: Send + Sync {
    /// The organization's connections, oldest first; only `environment`'s when given.
    async fn list_connections(
        &self,
        organization_id: OrganizationId,
        environment: Option<Environment>,
    ) -> Result<Vec<ModelConnection>, ModelStoreError>;

    /// The connection, if it's the organization's and, when `environment`
    /// is given, of that environment.
    async fn find_connection(
        &self,
        organization_id: OrganizationId,
        environment: Option<Environment>,
        connection_id: ModelConnectionId,
    ) -> Result<Option<ModelConnection>, ModelStoreError>;

    /// The connection's current key, sealed, for the code that opens it.
    async fn connection_key(
        &self,
        organization_id: OrganizationId,
        environment: Environment,
        connection_id: ModelConnectionId,
    ) -> Result<Option<ConnectionKey>, ModelStoreError>;

    /// Creates an `active` connection with its key at [`FIRST_KEY_VERSION`].
    async fn create_connection(
        &self,
        organization_id: OrganizationId,
        connection_id: ModelConnectionId,
        manager: ModelManager,
        connection: &NewConnection,
        now: Timestamp,
        audit: &Audit<'_, ModelConnection>,
    ) -> Result<ModelConnection, ModelStoreError>;

    #[allow(clippy::too_many_arguments)]
    async fn rename_connection(
        &self,
        organization_id: OrganizationId,
        environment: Environment,
        connection_id: ModelConnectionId,
        manager: ModelManager,
        label: &str,
        now: Timestamp,
        audit: &Audit<'_, ModelConnection>,
    ) -> Result<ModelConnection, ModelStoreError>;

    /// Stores the next key version, forgets the earlier ones and makes the
    /// connection `active`. `KeyChanged` unless the key is still at
    /// `expected_version`: nothing changes then.
    #[allow(clippy::too_many_arguments)]
    async fn replace_key(
        &self,
        organization_id: OrganizationId,
        environment: Environment,
        connection_id: ModelConnectionId,
        manager: ModelManager,
        replacement: &KeyReplacement,
        now: Timestamp,
        audit: &Audit<'_, ModelConnection>,
    ) -> Result<ModelConnection, ModelStoreError>;

    /// Records that the provider refused the key at `key_version`: the
    /// connection becomes `invalid_key`, unless its key was replaced since.
    /// What Zupflow observed, not a person's change: no audit.
    async fn mark_key_refused(
        &self,
        organization_id: OrganizationId,
        environment: Environment,
        connection_id: ModelConnectionId,
        key_version: u32,
    ) -> Result<(), ModelStoreError>;

    /// Deletes the connection with its keys and models, as one, recording
    /// `audit` of it as it was. `InUse` while one of its models is the
    /// default or an agent's.
    async fn delete_connection(
        &self,
        organization_id: OrganizationId,
        environment: Environment,
        connection_id: ModelConnectionId,
        manager: ModelManager,
        audit: &Audit<'_, ModelConnection>,
    ) -> Result<(), ModelStoreError>;

    /// A page of models, by name (any case), then ID. A cursor is only a
    /// position in that order (`InvalidCursor` when it isn't one): given
    /// with other filters, or another organization's, it pages this
    /// organization's models matching these filters from that position.
    async fn list_models(
        &self,
        organization_id: OrganizationId,
        filter: &ModelFilter,
        cursor: Option<&str>,
        limit: u32,
    ) -> Result<Page<Model>, ModelStoreError>;

    /// The model, if it's the organization's and, when `environment` is
    /// given, of that environment.
    async fn find_model(
        &self,
        organization_id: OrganizationId,
        environment: Option<Environment>,
        model_id: ModelId,
    ) -> Result<Option<Model>, ModelStoreError>;

    /// The connection's models: each provider model ID with the model added.
    async fn added_models(
        &self,
        organization_id: OrganizationId,
        environment: Environment,
        connection_id: ModelConnectionId,
    ) -> Result<Vec<(String, ModelId)>, ModelStoreError>;

    /// Adds a model from a connection of this environment
    /// (`ConnectionNotFound` otherwise), once per connection and provider
    /// model (`ModelExists`).
    #[allow(clippy::too_many_arguments)]
    async fn create_model(
        &self,
        organization_id: OrganizationId,
        environment: Environment,
        model_id: ModelId,
        manager: ModelManager,
        model: &NewModel,
        now: Timestamp,
        audit: &Audit<'_, Model>,
    ) -> Result<Model, ModelStoreError>;

    #[allow(clippy::too_many_arguments)]
    async fn rename_model(
        &self,
        organization_id: OrganizationId,
        environment: Environment,
        model_id: ModelId,
        manager: ModelManager,
        name: &str,
        now: Timestamp,
        audit: &Audit<'_, Model>,
    ) -> Result<Model, ModelStoreError>;

    /// Removes the model, recording `audit` of it as it was. `InUse` while
    /// it's the default or an agent's.
    async fn delete_model(
        &self,
        organization_id: OrganizationId,
        environment: Environment,
        model_id: ModelId,
        manager: ModelManager,
        audit: &Audit<'_, Model>,
    ) -> Result<(), ModelStoreError>;

    async fn default_model(
        &self,
        organization_id: OrganizationId,
        environment: Environment,
    ) -> Result<DefaultModel, ModelStoreError>;

    /// Sets the environment's default to a model of it (`UnknownModel`
    /// otherwise), or clears it, as written by `updated_by` (`None` for an
    /// API key).
    #[allow(clippy::too_many_arguments)]
    async fn set_default_model(
        &self,
        organization_id: OrganizationId,
        environment: Environment,
        manager: ModelManager,
        model_id: Option<ModelId>,
        updated_by: Option<UserId>,
        now: Timestamp,
        audit: &Audit<'_, DefaultModel>,
    ) -> Result<DefaultModel, ModelStoreError>;

    /// The model a new run of an agent in this workspace uses: its override
    /// (`model_id`), or else the default of the workspace's environment.
    /// `None` when it has neither: the run is refused, never given a model.
    async fn run_model(
        &self,
        organization_id: OrganizationId,
        workspace_id: WorkspaceId,
        model_id: Option<ModelId>,
    ) -> Result<Option<RunModel>, ModelStoreError>;

    /// Whether a run's model, reached through its connection, can be called
    /// now, both found in the workspace's environment.
    async fn route_state(
        &self,
        organization_id: OrganizationId,
        workspace_id: WorkspaceId,
        model_id: ModelId,
        connection_id: ModelConnectionId,
    ) -> Result<RouteState, ModelStoreError>;
}
