//! Model providers: bring your own key (decisions 0049, 0050), at
//! `/v1/organizations/{org}/model-connections…`, `/models…` and
//! `/default-models/{env}`. Read by every member and the organization's API
//! keys; changed by owners, admins and API keys. Everything is
//! organization-scoped and environment-bound: an API key reaches only its own
//! environment's connections, models and default (another's is `404`, and
//! naming the other environment is `422 environment_mismatch`), and nothing
//! falls back from one environment to the other.
//!
//! A connection's key is checked with its provider, by listing its models
//! through the [`ModelProviderCatalog`], before anything is saved; then it's
//! sealed under the connection's `model_provider_key` context, one value per
//! key version, and opened only to ask the provider again. Responses, errors,
//! logs and the audit log never hold it: a connection shows whether its key
//! is set and its last four characters. The provider's list is kept for ten
//! minutes per connection and key version.
//!
//! Removing a model the environment's default or an agent holds is
//! `409 model_in_use`, naming what holds it; the store checks it in the
//! transaction that removes it.

use std::collections::HashMap;
use std::time::Duration;

use axum::Router;
use axum::extract::{Path, RawQuery, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::{get, put};
use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use serde::Serialize;
use serde_json::{Value, json};
use zupflow_core::audit::{AuditAction, AuditEntry, AuditObject, AuditPlace, AuditTarget};
use zupflow_core::environment::Environment;
use zupflow_core::error::ApiError;
use zupflow_core::ids::{ModelConnectionId, ModelId};
use zupflow_core::keys::{EncryptionContext, open, seal};
use zupflow_core::model_providers::{CatalogError, MAX_KEY, MIN_KEY, ProviderKey, ProviderKind, ProviderModel};
use zupflow_core::model_store::{
    DEFAULT_PAGE, DefaultModel, FIRST_KEY_VERSION, KeyReplacement, MAX_NAME, MAX_PAGE, MAX_PROVIDER_MODEL_ID, Model,
    ModelConnection, ModelFilter, ModelHolders, ModelManager, ModelStoreError, NAMED_HOLDERS, NewConnection, NewModel,
    SealedKey,
};
use zupflow_core::organizations::MemberRole;
use zupflow_core::scope::{Name, Scope};
use zupflow_core::time::Timestamp;

use crate::auth::{Access, Actor, AppState, Caller};
use crate::http::{Failure, envelope, envelope_with_meta};
use crate::input::{Body, Fields, Object};
use crate::runs::query;
use crate::workspaces::{clamped_int, environment_mismatch, invalid_cursor};

pub fn routes() -> Router<AppState> {
    Router::new()
        .route(
            "/v1/organizations/{org}/model-connections",
            get(list_connections).post(create_connection),
        )
        .route(
            "/v1/organizations/{org}/model-connections/{connection}",
            get(get_connection).patch(update_connection).delete(delete_connection),
        )
        .route(
            "/v1/organizations/{org}/model-connections/{connection}/api-key",
            put(replace_key),
        )
        .route(
            "/v1/organizations/{org}/model-connections/{connection}/provider-models",
            get(list_provider_models),
        )
        .route("/v1/organizations/{org}/models", get(list_models).post(create_model))
        .route(
            "/v1/organizations/{org}/models/{model}",
            get(get_model).patch(update_model).delete(delete_model),
        )
        .route(
            "/v1/organizations/{org}/default-models/{env}",
            get(get_default_model).put(update_default_model),
        )
}

/// How long a provider's list is kept, per connection and key version.
const PROVIDER_LIST_TTL: Duration = Duration::from_secs(10 * 60);

/// Provider models on one page, by default and at most.
const PROVIDER_PAGE: i64 = 25;
const MAX_PROVIDER_PAGE: i64 = 100;

fn unavailable() -> Failure {
    ApiError::new(
        503,
        "unavailable",
        "The service is unavailable right now. Try again shortly.",
    )
    .into()
}

fn connection_not_found() -> Failure {
    ApiError::not_found("Model connection").into()
}

fn model_not_found() -> Failure {
    ApiError::not_found("Model").into()
}

fn field_error(status: u16, code: &'static str, message: String, field: &str, field_message: String) -> Failure {
    ApiError {
        fields: Some([(field.to_owned(), field_message)].into()),
        ..ApiError::new(status, code, message)
    }
    .into()
}

/// `model_id` names no model of the organization in this environment.
fn unknown_model(environment: Environment) -> Failure {
    let message = format!("Choose a model added in {}.", environment.name());
    field_error(422, "unknown_model", message.clone(), "model_id", message)
}

/// What a store refusal answers, where the route has nothing more to say.
fn store_failure(error: ModelStoreError) -> Failure {
    match error {
        ModelStoreError::Unavailable => unavailable(),
        ModelStoreError::ConnectionNotFound => connection_not_found(),
        ModelStoreError::ModelNotFound => model_not_found(),
        ModelStoreError::NotAllowed => ApiError::forbidden("You can no longer manage model providers.").into(),
        ModelStoreError::ModelExists => {
            ApiError::new(409, "model_exists", "That model is already added from this connection.").into()
        }
        ModelStoreError::UnknownModel => {
            ApiError::new(422, "unknown_model", "Choose a model added in this environment.").into()
        }
        ModelStoreError::InUse(holders) => in_use(&holders, None, "these models"),
        ModelStoreError::KeyChanged => ApiError::new(
            409,
            "key_changed",
            "The key was replaced at the same time. Check the connection, then try again.",
        )
        .into(),
        ModelStoreError::InvalidCursor => invalid_cursor(),
    }
}

/// `409 model_in_use`, naming what holds `what`: the environment's default,
/// then the agents, the first [`NAMED_HOLDERS`] by name and workspace.
fn in_use(holders: &ModelHolders, environment: Option<Environment>, what: &str) -> Failure {
    let mut reasons = Vec::new();
    let mut fields = std::collections::BTreeMap::new();
    if let Some(name) = &holders.default_model {
        let place = environment.map_or("this environment".to_owned(), |environment| {
            format!("{}'s", environment.name())
        });
        let place = if environment.is_some() {
            place
        } else {
            "this environment's".to_owned()
        };
        reasons.push(format!("{name} is {place} default model; choose another default first"));
        fields.insert("default_model".to_owned(), name.clone());
    }
    let mut names: Vec<String> = holders
        .agents
        .iter()
        .map(|holder| format!("{} ({})", holder.agent, holder.workspace))
        .collect();
    let named = u64::try_from(NAMED_HOLDERS).unwrap_or(5);
    if holders.agent_total > named {
        names.push(format!("{} more", holders.agent_total - named));
    }
    if !names.is_empty() {
        let names = names.join(", ");
        reasons.push(format!("agents use it: {names}; choose another model for them first"));
        fields.insert("agents".to_owned(), names);
    }
    ApiError {
        fields: Some(fields),
        ..ApiError::new(
            409,
            "model_in_use",
            format!("Can't remove {what}: {}.", reasons.join(". ")),
        )
    }
    .into()
}

/// Who the store rechecks a change for, inside its transaction.
fn manager(access: &Access) -> ModelManager {
    match &access.actor {
        Actor::Member(member) => ModelManager::Member(member.member.id),
        Actor::ApiKey(key) => ModelManager::ApiKey(key.id),
    }
}

/// An environment a request names, held to an API key's own: a key naming
/// the other is `422`; naming none is its own.
fn required_environment(access: &Access, asked: Option<Environment>) -> Result<Option<Environment>, Failure> {
    match (access.environment(), asked) {
        (Some(own), Some(asked)) if own != asked => Err(environment_mismatch()),
        (Some(own), _) => Ok(Some(own)),
        (None, asked) => Ok(asked),
    }
}

/// An `environment` query parameter: one of the two, or absent.
fn environment_query(value: Option<&String>) -> Result<Option<Environment>, Failure> {
    match value.map(String::as_str).filter(|value| !value.is_empty()) {
        None => Ok(None),
        Some(value) => Environment::parse(value).map(Some).ok_or_else(|| {
            field_error(
                400,
                "validation_failed",
                "Unknown environment.".to_owned(),
                "environment",
                "Unknown environment.".to_owned(),
            )
        }),
    }
}

/// The environment a `default-models/{env}` path names; another value is
/// the same `404` as one that isn't there.
fn environment_path(access: &Access, env: &str) -> Result<Environment, Failure> {
    let environment = Environment::parse(env).ok_or_else(|| Failure(ApiError::not_found("Environment")))?;
    required_environment(access, Some(environment))?;
    Ok(environment)
}

/// The connection a path or body names, in the caller's organization and, for
/// an API key, its environment; anything else is the same `404`.
async fn find_connection(state: &AppState, access: &Access, connection: &str) -> Result<ModelConnection, Failure> {
    let connection_id = ModelConnectionId::parse(connection).map_err(|_| connection_not_found())?;
    state
        .model_store
        .find_connection(access.organization_id, access.environment(), connection_id)
        .await
        .map_err(store_failure)?
        .ok_or_else(connection_not_found)
}

/// The model a path names, as [`find_connection`] finds a connection.
async fn find_model(state: &AppState, access: &Access, model: &str) -> Result<Model, Failure> {
    let model_id = ModelId::parse(model).map_err(|_| model_not_found())?;
    state
        .model_store
        .find_model(access.organization_id, access.environment(), model_id)
        .await
        .map_err(store_failure)?
        .ok_or_else(model_not_found)
}

/// A label or a model's name: trimmed, 1 to 60 characters.
fn checked_name(value: String, field: &str, fields: &mut Fields) -> Option<String> {
    let name = value.trim().to_owned();
    if name.is_empty() {
        fields.invalid(field, "Required.");
        None
    } else if name.chars().count() > MAX_NAME {
        fields.invalid(field, "Use 60 characters or fewer.");
        None
    } else {
        Some(name)
    }
}

/// A provider key as pasted: trimmed, one unbroken token of 20 to 500
/// characters. Never echoed: a refusal names what's wrong, not the key.
fn checked_key(body: &Object, fields: &mut Fields) -> Option<ProviderKey> {
    let key = ProviderKey::new(body.string("api_key", fields)?.trim().to_owned());
    let length = key.expose().chars().count();
    if length < MIN_KEY {
        fields.invalid("api_key", "Paste the whole key.");
        None
    } else if length > MAX_KEY {
        fields.invalid("api_key", "Use 500 characters or fewer.");
        None
    } else if key.expose().chars().any(char::is_whitespace) {
        fields.invalid("api_key", "A key has no spaces.");
        None
    } else {
        Some(key)
    }
}

/// A catalog failure other than a refused key: Zupflow couldn't ask. A
/// deployment configured with no providers never can, so that's its own,
/// permanent `501`, not a `503` to retry.
fn catalog_unavailable(kind: ProviderKind, error: CatalogError) -> Failure {
    match error {
        CatalogError::NotConfigured => ApiError::new(
            501,
            "model_providers_not_configured",
            format!(
                "This deployment isn't set up to reach model providers, so it can't use {} keys. \
                 Ask its operator to configure ZUPFLOW_MODEL_PROVIDER.",
                kind.name()
            ),
        )
        .into(),
        // A refused key is answered by its own error before it gets here.
        CatalogError::KeyRefused | CatalogError::Forbidden | CatalogError::Unavailable => ApiError::new(
            503,
            "unavailable",
            format!("{} didn't answer. Try again shortly.", kind.name()),
        )
        .into(),
    }
}

/// Checks a new key with its provider, by listing its models with it. A key
/// the provider refuses is `422 invalid_api_key`.
async fn check_key(state: &AppState, kind: ProviderKind, key: &ProviderKey) -> Result<(), Failure> {
    match state.model_catalog.list_models(kind, key).await {
        Ok(_) => Ok(()),
        Err(CatalogError::KeyRefused) => Err(field_error(
            422,
            "invalid_api_key",
            format!(
                "{} refused this key. Check it was copied whole and is still active.",
                kind.name()
            ),
            "api_key",
            format!("{} refused this key.", kind.name()),
        )),
        Err(CatalogError::Forbidden) => Err(field_error(
            422,
            "invalid_api_key",
            cannot_list(kind),
            "api_key",
            format!("{} won't list models with this key.", kind.name()),
        )),
        Err(error) => {
            tracing::warn!(event = "model_providers.check_failed", catalog = state.model_catalog.name(), kind = kind.as_str(), %error);
            Err(catalog_unavailable(kind, error))
        }
    }
}

/// Seals a key under its connection's context at `version`.
async fn sealed_key(
    state: &AppState,
    access: &Access,
    environment: Environment,
    connection_id: ModelConnectionId,
    version: u32,
    key: &ProviderKey,
) -> Result<SealedKey, Failure> {
    let organization_id = access.organization_id;
    let context = EncryptionContext::model_provider_key(organization_id, environment, connection_id, version);
    let sealed = seal(state.keys.as_ref(), &context, key.expose().as_bytes())
        .await
        .map_err(|error| {
            tracing::error!(event = "model_providers.seal_failed", %organization_id, %error);
            unavailable()
        })?;
    Ok(SealedKey {
        sealed,
        last4: key.last4(),
    })
}

/// Why a stored key can't be used to ask the provider.
enum ListFailure {
    /// The provider refused it: the connection is now `invalid_key`.
    Refused,
    Failed(Failure),
}

/// What the connection's provider lists for its key: kept for ten minutes
/// per key version, else asked again with the key opened. A key the
/// provider refuses marks the connection `invalid_key`.
async fn provider_models(state: &AppState, connection: &ModelConnection) -> Result<Vec<ProviderModel>, ListFailure> {
    let organization_id = connection.organization_id;
    let environment = connection.environment;
    let scope = Scope::Environment(organization_id, environment);
    let cache_key = Name::new(&format!(
        "provider-models.{}.v{}",
        connection.id, connection.key_version
    ))
    .map_err(|_| ListFailure::Failed(unavailable()))?;
    if let Ok(Some(bytes)) = state.cache.get(&scope, &cache_key).await
        && let Ok(models) = serde_json::from_slice::<Vec<ProviderModel>>(&bytes)
    {
        return Ok(models);
    }
    let stored = state
        .model_store
        .connection_key(organization_id, environment, connection.id)
        .await
        .map_err(|error| ListFailure::Failed(store_failure(error)))?
        .ok_or_else(|| ListFailure::Failed(connection_not_found()))?;
    let context = EncryptionContext::model_provider_key(organization_id, environment, connection.id, stored.version);
    let opened = open(state.keys.as_ref(), &context, &stored.sealed)
        .await
        .map_err(|error| {
            tracing::error!(event = "model_providers.open_failed", %organization_id, %error);
            ListFailure::Failed(unavailable())
        })?;
    let key = ProviderKey::from_bytes(opened.expose()).ok_or_else(|| ListFailure::Failed(unavailable()))?;
    match state.model_catalog.list_models(connection.kind, &key).await {
        Ok(models) => {
            if let Ok(bytes) = serde_json::to_vec(&models)
                && let Err(error) = state.cache.put(&scope, &cache_key, bytes, PROVIDER_LIST_TTL).await
            {
                tracing::warn!(event = "model_providers.cache_failed", %organization_id, %error);
            }
            Ok(models)
        }
        Err(CatalogError::KeyRefused) => {
            if let Err(error) = state
                .model_store
                .mark_key_refused(organization_id, environment, connection.id, stored.version)
                .await
            {
                tracing::error!(event = "model_providers.mark_refused_failed", %organization_id, %error);
            }
            Err(ListFailure::Refused)
        }
        // The key works, but not for listing: nothing to mark.
        Err(CatalogError::Forbidden) => Err(ListFailure::Failed(
            ApiError::new(422, "invalid_api_key", cannot_list(connection.kind)).into(),
        )),
        Err(error) => {
            tracing::warn!(event = "model_providers.list_failed", catalog = state.model_catalog.name(), kind = connection.kind.as_str(), %error);
            Err(ListFailure::Failed(catalog_unavailable(connection.kind, error)))
        }
    }
}

/// A key its provider accepts but won't list models with: every key must
/// be able to, since that's how a key is checked and models are added.
fn cannot_list(kind: ProviderKind) -> String {
    format!(
        "{} accepted this key but won't list models with it. Use a key that may read models \
         (an OpenAI restricted key needs read access to Models).",
        kind.name()
    )
}

/// The connection's key was refused: `then` says what to do about it.
fn key_refused(kind: ProviderKind, then: &str) -> Failure {
    ApiError::new(
        422,
        "invalid_api_key",
        format!("{} refused this connection's key. {then}", kind.name()),
    )
    .into()
}

fn connection_target(connection: &ModelConnection) -> AuditTarget {
    AuditTarget::new(AuditObject::ModelConnection(connection.id), &connection.label)
}

fn model_target(model: &Model) -> AuditTarget {
    AuditTarget::new(AuditObject::Model(model.id), &model.name)
}

fn page_meta(next_cursor: Option<String>, total: u64) -> Value {
    json!({ "pagination": { "next_cursor": next_cursor, "total": total } })
}

// ---------------------------------------------------------------------------
// Connections

async fn list_connections(
    State(state): State<AppState>,
    caller: Caller,
    Path(org): Path<String>,
    RawQuery(raw): RawQuery,
) -> Result<Response, Failure> {
    let access = Access::organization(&state, caller, &org).await?;
    let query = query(raw.as_deref())?;
    let environment = required_environment(&access, environment_query(query.get("environment"))?)?;
    let connections = state
        .model_store
        .list_connections(access.organization_id, environment)
        .await
        .map_err(store_failure)?;
    Ok(envelope(StatusCode::OK, connections))
}

async fn create_connection(
    State(state): State<AppState>,
    caller: Caller,
    Path(org): Path<String>,
    body: Body,
) -> Result<Response, Failure> {
    let access = Access::organization(&state, caller, &org).await?;
    access.require(MemberRole::is_admin, "connect model providers")?;
    let body = body.object()?;
    let mut fields = Fields::default();
    let environment = body
        .string("environment", &mut fields)
        .and_then(|value| match Environment::parse(&value) {
            Some(environment) => Some(environment),
            None => {
                fields.invalid("environment", "Choose sandbox or production.");
                None
            }
        });
    let kind = body
        .string("kind", &mut fields)
        .and_then(|value| match ProviderKind::parse(&value) {
            Some(kind) => Some(kind),
            None => {
                fields.invalid("kind", "Choose a provider.");
                None
            }
        });
    let label = body
        .string("label", &mut fields)
        .and_then(|label| checked_name(label, "label", &mut fields));
    let key = checked_key(&body, &mut fields);
    fields.finish()?;
    let (Some(environment), Some(kind), Some(label), Some(key)) = (environment, kind, label, key) else {
        return Err(ApiError::internal().into());
    };
    // A key connects providers in its own environment only.
    required_environment(&access, Some(environment))?;
    check_key(&state, kind, &key).await?;
    let connection_id = ModelConnectionId::generate();
    let sealed = sealed_key(&state, &access, environment, connection_id, FIRST_KEY_VERSION, &key).await?;
    drop(key);
    let connection = state
        .model_store
        .create_connection(
            access.organization_id,
            connection_id,
            manager(&access),
            &NewConnection {
                environment,
                kind,
                label,
                key: sealed,
            },
            Timestamp::now(),
            &|connection: &ModelConnection| {
                AuditEntry::new(
                    access.audit_actor(),
                    AuditAction::ModelConnectionCreated,
                    connection_target(connection),
                    AuditPlace::Environment(connection.environment),
                )
            },
        )
        .await
        .map_err(store_failure)?;
    Ok(envelope(StatusCode::CREATED, connection))
}

async fn get_connection(
    State(state): State<AppState>,
    caller: Caller,
    Path((org, connection)): Path<(String, String)>,
) -> Result<Response, Failure> {
    let access = Access::organization(&state, caller, &org).await?;
    let connection = find_connection(&state, &access, &connection).await?;
    Ok(envelope(StatusCode::OK, connection))
}

async fn update_connection(
    State(state): State<AppState>,
    caller: Caller,
    Path((org, connection)): Path<(String, String)>,
    body: Body,
) -> Result<Response, Failure> {
    let access = Access::organization(&state, caller, &org).await?;
    access.require(MemberRole::is_admin, "rename model provider connections")?;
    let connection = find_connection(&state, &access, &connection).await?;
    let body = body.object()?;
    let mut fields = Fields::default();
    let label = body
        .string("label", &mut fields)
        .and_then(|label| checked_name(label, "label", &mut fields));
    fields.finish()?;
    let Some(label) = label else {
        return Err(ApiError::internal().into());
    };
    let renamed = state
        .model_store
        .rename_connection(
            access.organization_id,
            connection.environment,
            connection.id,
            manager(&access),
            &label,
            Timestamp::now(),
            &|connection: &ModelConnection| {
                AuditEntry::new(
                    access.audit_actor(),
                    AuditAction::ModelConnectionUpdated,
                    connection_target(connection),
                    AuditPlace::Environment(connection.environment),
                )
            },
        )
        .await
        .map_err(store_failure)?;
    Ok(envelope(StatusCode::OK, renamed))
}

async fn replace_key(
    State(state): State<AppState>,
    caller: Caller,
    Path((org, connection)): Path<(String, String)>,
    body: Body,
) -> Result<Response, Failure> {
    let access = Access::organization(&state, caller, &org).await?;
    access.require(MemberRole::is_admin, "replace model provider keys")?;
    let connection = find_connection(&state, &access, &connection).await?;
    let body = body.object()?;
    let mut fields = Fields::default();
    let key = checked_key(&body, &mut fields);
    fields.finish()?;
    let Some(key) = key else {
        return Err(ApiError::internal().into());
    };
    // A refused key changes nothing: the connection keeps the key it had.
    check_key(&state, connection.kind, &key).await?;
    let next = connection.key_version.checked_add(1).ok_or_else(unavailable)?;
    let sealed = sealed_key(&state, &access, connection.environment, connection.id, next, &key).await?;
    drop(key);
    let replaced = state
        .model_store
        .replace_key(
            access.organization_id,
            connection.environment,
            connection.id,
            manager(&access),
            &KeyReplacement {
                expected_version: connection.key_version,
                key: sealed,
            },
            Timestamp::now(),
            &|connection: &ModelConnection| {
                AuditEntry::new(
                    access.audit_actor(),
                    AuditAction::ModelConnectionKeyReplaced,
                    connection_target(connection),
                    AuditPlace::Environment(connection.environment),
                )
            },
        )
        .await
        .map_err(store_failure)?;
    Ok(envelope(StatusCode::OK, replaced))
}

async fn delete_connection(
    State(state): State<AppState>,
    caller: Caller,
    Path((org, connection)): Path<(String, String)>,
) -> Result<Response, Failure> {
    let access = Access::organization(&state, caller, &org).await?;
    access.require(MemberRole::is_admin, "delete model provider connections")?;
    let connection = find_connection(&state, &access, &connection).await?;
    state
        .model_store
        .delete_connection(
            access.organization_id,
            connection.environment,
            connection.id,
            manager(&access),
            &|connection: &ModelConnection| {
                AuditEntry::new(
                    access.audit_actor(),
                    AuditAction::ModelConnectionDeleted,
                    connection_target(connection),
                    AuditPlace::Environment(connection.environment),
                )
            },
        )
        .await
        .map_err(|error| match error {
            ModelStoreError::InUse(holders) => in_use(
                &holders,
                Some(connection.environment),
                &format!("{} while its models are in use", connection.label),
            ),
            error => store_failure(error),
        })?;
    Ok(StatusCode::NO_CONTENT.into_response())
}

/// The contract's `ProviderModel`: what the provider lists, with the model
/// already added from this connection.
#[derive(Serialize)]
struct ProviderModelView<'a> {
    #[serde(flatten)]
    model: &'a ProviderModel,
    added_model_id: Option<ModelId>,
}

/// A page of a list held in memory: the position after the last item shown.
fn offset_of(cursor: Option<&String>) -> Result<usize, Failure> {
    let Some(cursor) = cursor else { return Ok(0) };
    let decoded = URL_SAFE_NO_PAD.decode(cursor).map_err(|_| invalid_cursor())?;
    std::str::from_utf8(&decoded)
        .ok()
        .and_then(|text| text.strip_prefix("offset:"))
        .and_then(|offset| offset.parse().ok())
        .ok_or_else(invalid_cursor)
}

async fn list_provider_models(
    State(state): State<AppState>,
    caller: Caller,
    Path((org, connection)): Path<(String, String)>,
    RawQuery(raw): RawQuery,
) -> Result<Response, Failure> {
    let access = Access::organization(&state, caller, &org).await?;
    let connection = find_connection(&state, &access, &connection).await?;
    let then = "Replace the key to search its models.";
    if connection.status == zupflow_core::model_store::ConnectionStatus::InvalidKey {
        return Err(key_refused(connection.kind, then));
    }
    let query = query(raw.as_deref())?;
    let offset = offset_of(query.get("cursor"))?;
    let limit = usize::try_from(clamped_int(
        query.get("limit").map(String::as_str),
        PROVIDER_PAGE,
        MAX_PROVIDER_PAGE,
    ))
    .unwrap_or(25);
    let listed = provider_models(&state, &connection)
        .await
        .map_err(|failure| match failure {
            ListFailure::Refused => key_refused(connection.kind, then),
            ListFailure::Failed(failure) => failure,
        })?;
    let added: HashMap<String, ModelId> = state
        .model_store
        .added_models(access.organization_id, connection.environment, connection.id)
        .await
        .map_err(store_failure)?
        .into_iter()
        .collect();
    let q = query.get("q").map(String::as_str).unwrap_or_default();
    let matches: Vec<&ProviderModel> = listed.iter().filter(|model| model.matches(q)).collect();
    let page: Vec<ProviderModelView<'_>> = matches
        .iter()
        .skip(offset)
        .take(limit)
        .map(|model| ProviderModelView {
            model,
            added_model_id: added.get(&model.provider_model_id).copied(),
        })
        .collect();
    let next = offset.saturating_add(limit);
    let next_cursor = (next < matches.len()).then(|| URL_SAFE_NO_PAD.encode(format!("offset:{next}")));
    let total = u64::try_from(matches.len()).unwrap_or(u64::MAX);
    Ok(envelope_with_meta(StatusCode::OK, page, page_meta(next_cursor, total)))
}

// ---------------------------------------------------------------------------
// Models

async fn list_models(
    State(state): State<AppState>,
    caller: Caller,
    Path(org): Path<String>,
    RawQuery(raw): RawQuery,
) -> Result<Response, Failure> {
    let access = Access::organization(&state, caller, &org).await?;
    let query = query(raw.as_deref())?;
    let environment = required_environment(&access, environment_query(query.get("environment"))?)?;
    let mut filter = ModelFilter {
        environment,
        q: query.get("q").cloned(),
        ..ModelFilter::default()
    };
    // A malformed or unknown connection matches nothing.
    if let Some(connection) = query.get("connection_id").filter(|value| !value.is_empty()) {
        filter.connection_id = ModelConnectionId::parse(connection).ok();
        filter.matches_nothing = filter.connection_id.is_none();
    }
    let limit = clamped_int(
        query.get("limit").map(String::as_str),
        i64::from(DEFAULT_PAGE),
        i64::from(MAX_PAGE),
    );
    let page = state
        .model_store
        .list_models(
            access.organization_id,
            &filter,
            query.get("cursor").map(String::as_str),
            u32::try_from(limit).unwrap_or(DEFAULT_PAGE),
        )
        .await
        .map_err(store_failure)?;
    Ok(envelope_with_meta(
        StatusCode::OK,
        page.items,
        page_meta(page.next_cursor, page.total),
    ))
}

async fn create_model(
    State(state): State<AppState>,
    caller: Caller,
    Path(org): Path<String>,
    body: Body,
) -> Result<Response, Failure> {
    let access = Access::organization(&state, caller, &org).await?;
    access.require(MemberRole::is_admin, "add models")?;
    let body = body.object()?;
    let mut fields = Fields::default();
    let connection = body.string("connection_id", &mut fields);
    let provider_model_id = body.string("provider_model_id", &mut fields).and_then(|id| {
        let id = id.trim().to_owned();
        if id.is_empty() {
            fields.invalid("provider_model_id", "Choose a model.");
            None
        } else {
            Some(id)
        }
    });
    let name = body
        .optional_string("name", &mut fields)
        .and_then(|name| checked_name(name, "name", &mut fields));
    let named = body.has("name");
    fields.finish()?;
    let (Some(connection), Some(provider_model_id)) = (connection, provider_model_id) else {
        return Err(ApiError::internal().into());
    };
    if named && name.is_none() {
        return Err(ApiError::internal().into());
    }
    let connection = find_connection(&state, &access, &connection).await?;
    let provider = connection.kind.name();
    let then = "Replace the key before adding models.";
    if connection.status == zupflow_core::model_store::ConnectionStatus::InvalidKey {
        return Err(key_refused(connection.kind, then));
    }
    let listed = provider_models(&state, &connection)
        .await
        .map_err(|failure| match failure {
            ListFailure::Refused => key_refused(connection.kind, then),
            ListFailure::Failed(failure) => failure,
        })?;
    let Some(listed) = listed
        .into_iter()
        .find(|model| model.provider_model_id == provider_model_id)
        .filter(|model| model.provider_model_id.chars().count() <= MAX_PROVIDER_MODEL_ID)
    else {
        return Err(field_error(
            422,
            "unknown_provider_model",
            format!("{provider} doesn't list that model for this key."),
            "provider_model_id",
            format!("{provider} doesn't list this model."),
        ));
    };
    if !listed.supports_tools {
        return Err(field_error(
            422,
            "tools_unsupported",
            format!("{} can't call tools, so an agent can't use it.", listed.name),
            "provider_model_id",
            "This model can't call tools.".to_owned(),
        ));
    }
    // A provider's own name may be longer than a model's: keep what fits.
    let name = name.unwrap_or_else(|| listed.name.chars().take(MAX_NAME).collect());
    let created = state
        .model_store
        .create_model(
            access.organization_id,
            connection.environment,
            ModelId::generate(),
            manager(&access),
            &NewModel {
                connection_id: connection.id,
                provider_model_id: listed.provider_model_id.clone(),
                name,
                author: listed.author.chars().take(100).collect(),
                context_window: listed.context_window,
                supports_tools: listed.supports_tools,
            },
            Timestamp::now(),
            &|model: &Model| {
                AuditEntry::new(
                    access.audit_actor(),
                    AuditAction::ModelCreated,
                    model_target(model),
                    AuditPlace::Environment(model.environment),
                )
            },
        )
        .await
        .map_err(|error| match error {
            ModelStoreError::ModelExists => field_error(
                409,
                "model_exists",
                format!("{} is already added from {}.", listed.name, connection.label),
                "provider_model_id",
                "Already added from this connection.".to_owned(),
            ),
            error => store_failure(error),
        })?;
    Ok(envelope(StatusCode::CREATED, created))
}

async fn get_model(
    State(state): State<AppState>,
    caller: Caller,
    Path((org, model)): Path<(String, String)>,
) -> Result<Response, Failure> {
    let access = Access::organization(&state, caller, &org).await?;
    let model = find_model(&state, &access, &model).await?;
    Ok(envelope(StatusCode::OK, model))
}

async fn update_model(
    State(state): State<AppState>,
    caller: Caller,
    Path((org, model)): Path<(String, String)>,
    body: Body,
) -> Result<Response, Failure> {
    let access = Access::organization(&state, caller, &org).await?;
    access.require(MemberRole::is_admin, "rename models")?;
    let model = find_model(&state, &access, &model).await?;
    let body = body.object()?;
    let mut fields = Fields::default();
    let name = body
        .string("name", &mut fields)
        .and_then(|name| checked_name(name, "name", &mut fields));
    fields.finish()?;
    let Some(name) = name else {
        return Err(ApiError::internal().into());
    };
    let renamed = state
        .model_store
        .rename_model(
            access.organization_id,
            model.environment,
            model.id,
            manager(&access),
            &name,
            Timestamp::now(),
            &|model: &Model| {
                AuditEntry::new(
                    access.audit_actor(),
                    AuditAction::ModelUpdated,
                    model_target(model),
                    AuditPlace::Environment(model.environment),
                )
            },
        )
        .await
        .map_err(store_failure)?;
    Ok(envelope(StatusCode::OK, renamed))
}

async fn delete_model(
    State(state): State<AppState>,
    caller: Caller,
    Path((org, model)): Path<(String, String)>,
) -> Result<Response, Failure> {
    let access = Access::organization(&state, caller, &org).await?;
    access.require(MemberRole::is_admin, "remove models")?;
    let model = find_model(&state, &access, &model).await?;
    state
        .model_store
        .delete_model(
            access.organization_id,
            model.environment,
            model.id,
            manager(&access),
            &|model: &Model| {
                AuditEntry::new(
                    access.audit_actor(),
                    AuditAction::ModelDeleted,
                    model_target(model),
                    AuditPlace::Environment(model.environment),
                )
            },
        )
        .await
        .map_err(|error| match error {
            ModelStoreError::InUse(holders) => in_use(&holders, Some(model.environment), &model.name),
            error => store_failure(error),
        })?;
    Ok(StatusCode::NO_CONTENT.into_response())
}

// ---------------------------------------------------------------------------
// Default models

async fn get_default_model(
    State(state): State<AppState>,
    caller: Caller,
    Path((org, env)): Path<(String, String)>,
) -> Result<Response, Failure> {
    let access = Access::organization(&state, caller, &org).await?;
    let environment = environment_path(&access, &env)?;
    let default = state
        .model_store
        .default_model(access.organization_id, environment)
        .await
        .map_err(store_failure)?;
    Ok(envelope(StatusCode::OK, default))
}

async fn update_default_model(
    State(state): State<AppState>,
    caller: Caller,
    Path((org, env)): Path<(String, String)>,
    body: Body,
) -> Result<Response, Failure> {
    let access = Access::organization(&state, caller, &org).await?;
    access.require(MemberRole::is_admin, "choose the default model")?;
    let environment = environment_path(&access, &env)?;
    let body = body.object()?;
    let model_id = match body.0.get("model_id") {
        None => {
            let mut fields = Fields::default();
            fields.invalid("model_id", "Required.");
            fields.finish()?;
            return Err(ApiError::internal().into());
        }
        Some(Value::Null) => None,
        Some(Value::String(id)) => Some(ModelId::parse(id).map_err(|_| unknown_model(environment))?),
        Some(_) => {
            let mut fields = Fields::default();
            fields.invalid("model_id", "Must be a string.");
            fields.finish()?;
            return Err(ApiError::internal().into());
        }
    };
    let updated_by = match &access.actor {
        Actor::Member(member) => Some(member.user.id),
        Actor::ApiKey(_) => None,
    };
    let default = state
        .model_store
        .set_default_model(
            access.organization_id,
            environment,
            manager(&access),
            model_id,
            updated_by,
            Timestamp::now(),
            &|default: &DefaultModel| {
                let model = default.model.as_ref();
                AuditEntry::new(
                    access.audit_actor(),
                    AuditAction::DefaultModelUpdated,
                    AuditTarget::new(
                        AuditObject::DefaultModel(model.map(|model| model.id)),
                        model.map_or("None", |model| model.name.as_str()),
                    ),
                    AuditPlace::Environment(default.environment),
                )
            },
        )
        .await
        .map_err(|error| match error {
            ModelStoreError::UnknownModel => unknown_model(environment),
            error => store_failure(error),
        })?;
    Ok(envelope(StatusCode::OK, default))
}
