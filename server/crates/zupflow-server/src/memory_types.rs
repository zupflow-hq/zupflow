//! Memory types, at `/v1/organizations/{org}/memory-kinds`, `/memory-types…`
//! and `/default-memory-type`: the memory kinds this server serves, the
//! organization's memory types, and the one type its agents on the default
//! use. Types and the default are the organization's, not an environment's.
//! Every member and the organization's API keys, of either environment, read
//! them; owners and admins change them, from the Console only (an API key is
//! `403 console_only`).
//!
//! A type is made of a kind this server serves (`422 unknown_memory_kind`),
//! with settings that kind checks, its defaults filled in (`422
//! invalid_memory_settings`, naming each setting refused). A name is the
//! organization's once (`409 memory_type_name_taken`), and a type that's the
//! default or an agent's isn't deleted (`409 memory_type_in_use`, naming what
//! holds it): the store checks each, and that the member may still change
//! memory types, in the transaction that makes the change. A type of a kind
//! this server doesn't serve still reads, as not `served`: its agents run
//! without memory. A deleted type's memories go with it, in every
//! workspace, without a `memory.forgotten` for each: the deletion queues,
//! in its own transaction, the job that has the type's kind forget them
//! ([`crate::memory_forgetting`]).

use std::collections::BTreeMap;

use axum::Router;
use axum::extract::{Path, RawQuery, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use serde::Serialize;
use serde_json::{Map, Value, json};
use zupflow_core::audit::{AuditAction, AuditEntry, AuditObject, AuditPlace, AuditTarget};
use zupflow_core::error::ApiError;
use zupflow_core::ids::{MemoryTypeId, OrganizationId};
use zupflow_core::memory::registry::MemoryProviders;
use zupflow_core::memory::type_store::{
    DEFAULT_PAGE, DefaultMemoryType, MAX_NAME, MAX_PAGE, MemoryType, MemoryTypeChanges, MemoryTypeError,
    MemoryTypeHolders, NewMemoryType,
};
use zupflow_core::memory::{MemoryKind, MemoryWrites, Settings};
use zupflow_core::organizations::{MemberRole, User};
use zupflow_core::time::Timestamp;

use crate::auth::{Access, AppState, Caller, ConsoleUser, OrganizationAccess};
use crate::http::{Failure, envelope, envelope_with_meta};
use crate::input::{Body, Fields, Object};
use crate::runs::query;
use crate::workspaces::{clamped_int, invalid_cursor};

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/v1/organizations/{org}/memory-kinds", get(list_kinds))
        .route(
            "/v1/organizations/{org}/memory-types",
            get(list_types).post(create_type),
        )
        .route(
            "/v1/organizations/{org}/memory-types/{memory_type}",
            get(get_type).patch(update_type).delete(delete_type),
        )
        .route(
            "/v1/organizations/{org}/default-memory-type",
            get(get_default).put(update_default),
        )
}

/// The contract's `MemoryType`: the type as stored, and whether this server
/// serves its kind. Never its organization's ID.
#[derive(Serialize)]
struct MemoryTypeView {
    id: MemoryTypeId,
    name: String,
    kind: MemoryKind,
    settings: Map<String, Value>,
    writes: MemoryWrites,
    served: bool,
    agent_count: u64,
    is_default: bool,
    created_at: Timestamp,
    updated_at: Timestamp,
}

fn type_view(providers: &MemoryProviders, memory_type: MemoryType) -> MemoryTypeView {
    MemoryTypeView {
        served: providers.serves(&memory_type.kind),
        id: memory_type.id,
        name: memory_type.name,
        kind: memory_type.kind,
        settings: memory_type.settings,
        writes: memory_type.writes,
        agent_count: memory_type.agent_count,
        is_default: memory_type.is_default,
        created_at: memory_type.created_at,
        updated_at: memory_type.updated_at,
    }
}

/// The contract's `DefaultMemoryType`.
#[derive(Serialize)]
struct DefaultView {
    memory_type: Option<MemoryTypeView>,
    updated_at: Option<Timestamp>,
    updated_by: Option<User>,
}

fn default_view(providers: &MemoryProviders, default: DefaultMemoryType) -> DefaultView {
    DefaultView {
        memory_type: default.memory_type.map(|memory_type| type_view(providers, memory_type)),
        updated_at: default.updated_at,
        updated_by: default.updated_by,
    }
}

fn unavailable() -> Failure {
    ApiError::new(
        503,
        "unavailable",
        "The service is unavailable right now. Try again shortly.",
    )
    .into()
}

fn not_found() -> Failure {
    ApiError::not_found("Memory type").into()
}

/// A refusal naming one field.
fn field_error(status: u16, code: &'static str, message: &str, field: &str, field_message: &str) -> Failure {
    ApiError {
        fields: Some(BTreeMap::from([(field.to_owned(), field_message.to_owned())])),
        ..ApiError::new(status, code, message)
    }
    .into()
}

/// `400 validation_failed` for one field.
fn invalid(field: &str, message: &str) -> Failure {
    ApiError::validation(BTreeMap::from([(field.to_owned(), message.to_owned())])).into()
}

/// A `memory_type_id` that names none of the organization's types.
fn unknown_type() -> Failure {
    let message = "Choose a memory type of this organization.";
    field_error(422, "unknown_memory_type", message, "memory_type_id", message)
}

/// What a store refusal answers, where the route has nothing more to say.
fn store_failure(error: MemoryTypeError) -> Failure {
    match error {
        // The store has logged the cause.
        MemoryTypeError::Unavailable => unavailable(),
        MemoryTypeError::NotFound => not_found(),
        MemoryTypeError::NotAllowed => ApiError::forbidden("You can no longer manage memory types.").into(),
        MemoryTypeError::NameTaken => field_error(
            409,
            "memory_type_name_taken",
            "Another memory type has that name.",
            "name",
            "Already taken.",
        ),
        MemoryTypeError::UnknownType => unknown_type(),
        // Only deleting a type is refused for what holds it, and that route
        // names the holders.
        MemoryTypeError::InUse(_) => ApiError::new(409, "memory_type_in_use", "The memory type is in use.").into(),
        MemoryTypeError::InvalidCursor => invalid_cursor(),
    }
}

/// `409 memory_type_in_use`, naming what holds the type called `name`: the
/// organization's default, then the agents that name it, the first few by
/// name and workspace, oldest first, and how many more there are.
fn in_use(name: &str, holders: &MemoryTypeHolders) -> Failure {
    let mut reasons = Vec::new();
    let mut fields = BTreeMap::new();
    if holders.is_default {
        reasons.push(format!(
            "{name} is the organization's default memory type; choose another default first"
        ));
        fields.insert("default_memory_type".to_owned(), name.to_owned());
    }
    let mut names: Vec<String> = holders
        .agents
        .iter()
        .map(|holder| format!("{} ({})", holder.agent, holder.workspace))
        .collect();
    let more = holders
        .agent_total
        .saturating_sub(u64::try_from(holders.agents.len()).unwrap_or(u64::MAX));
    if more > 0 {
        names.push(format!("{more} more"));
    }
    if !names.is_empty() {
        let names = names.join(", ");
        reasons.push(format!("agents use it: {names}; choose another memory for them first"));
        fields.insert("agents".to_owned(), names);
    }
    ApiError {
        fields: Some(fields),
        ..ApiError::new(
            409,
            "memory_type_in_use",
            format!("Can't delete {name}: {}.", reasons.join(". ")),
        )
    }
    .into()
}

fn type_target(memory_type: &MemoryType) -> AuditTarget {
    AuditTarget::new(AuditObject::MemoryType(memory_type.id), &memory_type.name)
}

/// The default as the audit log names it: by the type chosen, or `None`
/// once cleared.
fn default_target(default: &DefaultMemoryType) -> AuditTarget {
    let chosen = default.memory_type.as_ref();
    AuditTarget::new(
        AuditObject::DefaultMemoryType(chosen.map(|memory_type| memory_type.id)),
        chosen.map_or("None", |memory_type| memory_type.name.as_str()),
    )
}

/// The type a path names, in the caller's organization: a malformed ID, an
/// unknown one and another organization's are the same `404`.
async fn find_type(
    state: &AppState,
    organization_id: OrganizationId,
    memory_type: &str,
) -> Result<MemoryType, Failure> {
    let memory_type_id = MemoryTypeId::parse(memory_type).map_err(|_| not_found())?;
    state
        .memory_types
        .find_type(organization_id, memory_type_id)
        .await
        .map_err(store_failure)?
        .ok_or_else(not_found)
}

/// A type's name: trimmed, 1 to 60 characters.
fn checked_name(value: String, fields: &mut Fields) -> Option<String> {
    let name = value.trim().to_owned();
    if name.is_empty() {
        fields.invalid("name", "Required.");
        None
    } else if name.chars().count() > MAX_NAME {
        fields.invalid("name", "Use 60 characters or fewer.");
        None
    } else {
        Some(name)
    }
}

/// `settings` as sent, an object; `None` when the body leaves it out. Their
/// kind checks what's in it.
fn settings_input(body: &Object, fields: &mut Fields) -> Option<Map<String, Value>> {
    match body.0.get("settings")? {
        Value::Object(settings) => Some(settings.clone()),
        _ => {
            fields.invalid("settings", "Use an object of the kind's settings.");
            None
        }
    }
}

/// `writes` as sent; `None` when the body leaves it out.
fn writes_input(body: &Object, fields: &mut Fields) -> Option<MemoryWrites> {
    let writes = body.0.get("writes")?.as_str().and_then(MemoryWrites::parse);
    if writes.is_none() {
        fields.invalid("writes", "Choose allow, approve or deny.");
    }
    writes
}

/// The kind a body names, when this server serves it; any other, a key of
/// the wrong form included, is `422 unknown_memory_kind`.
fn served_kind(providers: &MemoryProviders, kind: &str) -> Result<MemoryKind, Failure> {
    let message = "Choose a memory kind this server serves.";
    MemoryKind::parse(kind)
        .filter(|kind| providers.serves(kind))
        .ok_or_else(|| field_error(422, "unknown_memory_kind", message, "kind", message))
}

/// Settings as `kind` checks them, its defaults filled in; each one it
/// refuses is named as `settings.<key>` (`422 invalid_memory_settings`). A
/// kind this server doesn't serve keeps them as sent: nothing here can
/// check them.
fn checked_settings(
    providers: &MemoryProviders,
    kind: &MemoryKind,
    settings: &Map<String, Value>,
) -> Result<Settings, Failure> {
    providers.provider(kind).check_settings(settings).map_err(|refusal| {
        let fields = refusal
            .fields
            .into_iter()
            .map(|(key, why)| (format!("settings.{key}"), why))
            .collect();
        ApiError {
            fields: Some(fields),
            ..ApiError::new(422, "invalid_memory_settings", "Some settings are invalid.")
        }
        .into()
    })
}

async fn list_kinds(
    State(state): State<AppState>,
    caller: Caller,
    Path(org): Path<String>,
) -> Result<Response, Failure> {
    Access::organization(&state, caller, &org).await?;
    Ok(envelope(StatusCode::OK, state.memory_providers.descriptions()))
}

async fn list_types(
    State(state): State<AppState>,
    caller: Caller,
    Path(org): Path<String>,
    RawQuery(raw): RawQuery,
) -> Result<Response, Failure> {
    let access = Access::organization(&state, caller, &org).await?;
    let query = query(raw.as_deref())?;
    let limit = clamped_int(
        query.get("limit").map(String::as_str),
        i64::from(DEFAULT_PAGE),
        i64::from(MAX_PAGE),
    );
    let page = state
        .memory_types
        .list_types(
            access.organization_id,
            query.get("cursor").map(String::as_str),
            u32::try_from(limit).unwrap_or(DEFAULT_PAGE),
        )
        .await
        .map_err(store_failure)?;
    let items: Vec<MemoryTypeView> = page
        .items
        .into_iter()
        .map(|memory_type| type_view(&state.memory_providers, memory_type))
        .collect();
    Ok(envelope_with_meta(
        StatusCode::OK,
        items,
        json!({ "pagination": { "next_cursor": page.next_cursor, "total": page.total } }),
    ))
}

async fn create_type(
    State(state): State<AppState>,
    ConsoleUser(user): ConsoleUser,
    Path(org): Path<String>,
    body: Body,
) -> Result<Response, Failure> {
    let access = OrganizationAccess::resolve(&state, user, &org).await?;
    access.require(MemberRole::is_admin, "create memory types")?;
    let body = body.object()?;
    let mut fields = Fields::default();
    let name = body
        .string("name", &mut fields)
        .and_then(|name| checked_name(name, &mut fields));
    let kind = body.string("kind", &mut fields);
    let settings = settings_input(&body, &mut fields);
    let writes = writes_input(&body, &mut fields);
    fields.finish()?;
    let (Some(name), Some(kind)) = (name, kind) else {
        return Err(ApiError::internal().into());
    };
    // In order, as the mock checks them: the kind, its settings, then the
    // name, which the store holds to the organization's other types.
    let kind = served_kind(&state.memory_providers, &kind)?;
    let settings = checked_settings(&state.memory_providers, &kind, &settings.unwrap_or_default())?;
    let created = state
        .memory_types
        .create_type(
            access.organization.id,
            MemoryTypeId::generate(),
            access.member.id,
            &NewMemoryType {
                name,
                kind,
                settings,
                writes: writes.unwrap_or_default(),
            },
            Timestamp::now(),
            &|created: &MemoryType| {
                AuditEntry::new(
                    access.audit_actor(),
                    AuditAction::MemoryTypeCreated,
                    type_target(created),
                    AuditPlace::Organization,
                )
            },
        )
        .await
        .map_err(store_failure)?;
    Ok(envelope(
        StatusCode::CREATED,
        type_view(&state.memory_providers, created),
    ))
}

async fn get_type(
    State(state): State<AppState>,
    caller: Caller,
    Path((org, memory_type)): Path<(String, String)>,
) -> Result<Response, Failure> {
    let access = Access::organization(&state, caller, &org).await?;
    let memory_type = find_type(&state, access.organization_id, &memory_type).await?;
    Ok(envelope(
        StatusCode::OK,
        type_view(&state.memory_providers, memory_type),
    ))
}

/// Changes what the body sends of `name`, `settings` and `writes`; the kind
/// never changes, and anything else sent is ignored. Settings sent replace
/// all of the type's, checked by its own kind: one left out takes the kind's
/// default again.
async fn update_type(
    State(state): State<AppState>,
    ConsoleUser(user): ConsoleUser,
    Path((org, memory_type)): Path<(String, String)>,
    body: Body,
) -> Result<Response, Failure> {
    let access = OrganizationAccess::resolve(&state, user, &org).await?;
    access.require(MemberRole::is_admin, "change memory types")?;
    let memory_type = find_type(&state, access.organization.id, &memory_type).await?;
    let body = body.object()?;
    let mut fields = Fields::default();
    let name = body
        .optional_string("name", &mut fields)
        .and_then(|name| checked_name(name, &mut fields));
    let settings = settings_input(&body, &mut fields);
    let writes = writes_input(&body, &mut fields);
    fields.finish()?;
    let settings = settings
        .map(|settings| checked_settings(&state.memory_providers, &memory_type.kind, &settings))
        .transpose()?;
    let updated = state
        .memory_types
        .update_type(
            access.organization.id,
            memory_type.id,
            access.member.id,
            &MemoryTypeChanges { name, settings, writes },
            Timestamp::now(),
            &|updated: &MemoryType| {
                AuditEntry::new(
                    access.audit_actor(),
                    AuditAction::MemoryTypeUpdated,
                    type_target(updated),
                    AuditPlace::Organization,
                )
            },
        )
        .await
        .map_err(store_failure)?;
    Ok(envelope(StatusCode::OK, type_view(&state.memory_providers, updated)))
}

async fn delete_type(
    State(state): State<AppState>,
    ConsoleUser(user): ConsoleUser,
    Path((org, memory_type)): Path<(String, String)>,
) -> Result<Response, Failure> {
    let access = OrganizationAccess::resolve(&state, user, &org).await?;
    access.require(MemberRole::is_admin, "delete memory types")?;
    let memory_type = find_type(&state, access.organization.id, &memory_type).await?;
    state
        .memory_types
        .delete_type(
            access.organization.id,
            memory_type.id,
            access.member.id,
            &|deleted: &MemoryType| {
                AuditEntry::new(
                    access.audit_actor(),
                    AuditAction::MemoryTypeDeleted,
                    type_target(deleted),
                    AuditPlace::Organization,
                )
            },
        )
        .await
        .map_err(|error| match error {
            MemoryTypeError::InUse(holders) => in_use(&memory_type.name, &holders),
            error => store_failure(error),
        })?;
    Ok(StatusCode::NO_CONTENT.into_response())
}

async fn get_default(
    State(state): State<AppState>,
    caller: Caller,
    Path(org): Path<String>,
) -> Result<Response, Failure> {
    let access = Access::organization(&state, caller, &org).await?;
    let default = state
        .memory_types
        .default_type(access.organization_id)
        .await
        .map_err(store_failure)?;
    Ok(envelope(StatusCode::OK, default_view(&state.memory_providers, default)))
}

/// Chooses one of the organization's types as its default, or none with
/// `null`. The type itself doesn't change.
async fn update_default(
    State(state): State<AppState>,
    ConsoleUser(user): ConsoleUser,
    Path(org): Path<String>,
    body: Body,
) -> Result<Response, Failure> {
    let access = OrganizationAccess::resolve(&state, user, &org).await?;
    access.require(MemberRole::is_admin, "choose the default memory type")?;
    let body = body.object()?;
    let chosen = match body.0.get("memory_type_id") {
        Some(Value::Null) => None,
        // An ID that can't be a type's names none of the organization's.
        Some(Value::String(id)) => Some(MemoryTypeId::parse(id).map_err(|_| unknown_type())?),
        _ => return Err(invalid("memory_type_id", "Choose a memory type, or null for none.")),
    };
    let default = state
        .memory_types
        .set_default_type(
            access.organization.id,
            access.member.id,
            chosen,
            Timestamp::now(),
            &|default: &DefaultMemoryType| {
                AuditEntry::new(
                    access.audit_actor(),
                    AuditAction::DefaultMemoryTypeUpdated,
                    default_target(default),
                    AuditPlace::Organization,
                )
            },
        )
        .await
        .map_err(store_failure)?;
    Ok(envelope(StatusCode::OK, default_view(&state.memory_providers, default)))
}

#[cfg(test)]
mod tests {
    use zupflow_core::model_store::HoldingAgent;

    use super::*;

    fn holders(is_default: bool, agents: usize, agent_total: u64) -> MemoryTypeHolders {
        MemoryTypeHolders {
            is_default,
            agents: (0..agents)
                .map(|i| HoldingAgent {
                    agent: format!("Agent {i}"),
                    workspace: "QA".to_owned(),
                })
                .collect(),
            agent_total,
        }
    }

    #[test]
    fn names_what_holds_a_type() {
        let Failure(error) = in_use("Notes", &holders(true, 5, 6));
        assert_eq!((error.status, error.code), (409, "memory_type_in_use"));
        let agents = "Agent 0 (QA), Agent 1 (QA), Agent 2 (QA), Agent 3 (QA), Agent 4 (QA), 1 more";
        assert_eq!(
            error.fields,
            Some(BTreeMap::from([
                ("agents".to_owned(), agents.to_owned()),
                ("default_memory_type".to_owned(), "Notes".to_owned()),
            ]))
        );
        assert_eq!(
            error.message,
            format!(
                "Can't delete Notes: Notes is the organization's default memory type; choose another default \
                 first. agents use it: {agents}; choose another memory for them first."
            )
        );
        let Failure(error) = in_use("Notes", &holders(false, 1, 1));
        assert_eq!(
            error.fields,
            Some(BTreeMap::from([("agents".to_owned(), "Agent 0 (QA)".to_owned())]))
        );
        let Failure(error) = in_use("Notes", &holders(true, 0, 0));
        assert_eq!(
            error.fields,
            Some(BTreeMap::from([("default_memory_type".to_owned(), "Notes".to_owned())]))
        );
    }
}
