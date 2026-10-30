//! What agents remember, at `/v1/workspaces/{ws}/agents/{agent}/memories…`:
//! an agent's memories in its workspace, listed one memory type at a time
//! (the agent's own, or the one `memory_type` names), and read, saved,
//! changed and forgotten through the kind of each type
//! ([`MemoryProvider`]). Every member and the workspace's API keys read
//! them; owners, admins, developers and API keys save, change and forget
//! them, the Console in Sandbox only (`403 console_read_only_in_production`).
//!
//! A memory is found by its ID among the agent's memories in the workspace,
//! in the organization's types whose kind this server serves: one of a kind
//! it doesn't serve is the same `404` as one that isn't there. A list or a
//! save names its type, so there such a kind is `422
//! memory_kind_not_served`. A change or a forget finds the memory first,
//! then checks what its kind lets people do ([`Manage`]), then the text.
//!
//! Each change is recorded with it, in the transaction the kind makes it in
//! ([`zupflow_core::memory::Record`]): its webhook event (`memory.saved`,
//! `memory.updated` or `memory.forgotten`, naming the memory and never its
//! text), with a delivery for each endpoint of the workspace's environment
//! that subscribes to it, and its audit event, which names a memory by its
//! agent and short ID. A request cut off leaves the change and its records,
//! or neither. The text a memory holds already changes nothing, so nothing
//! is sent or recorded for it.
//!
//! Forgetting everything is an erasure: in each type of a kind this server
//! serves, whatever the kind lets people do, the agent's memories go at
//! once, each with its `memory.forgotten`, audited once for the type, as
//! `agent.memory_forgotten` with how many.

use std::collections::BTreeMap;
use std::sync::Arc;

use axum::Router;
use axum::extract::{Path, RawQuery, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use serde::Serialize;
use serde_json::{Value, json};
use zupflow_core::agents::Agent;
use zupflow_core::audit::{AuditAction, AuditEntry, AuditObject, AuditPlace, AuditTarget};
use zupflow_core::error::ApiError;
use zupflow_core::ids::{AgentId, ChannelConversationId, MemoryId, MemoryTypeId, WebhookEventId, WorkspaceId};
use zupflow_core::installations::ShortId;
use zupflow_core::memory::type_store::{self, MemoryType};
use zupflow_core::memory::{
    AgentMemory, DEFAULT_PAGE, MAX_PAGE, Manage, Memory, MemoryChange, MemoryError, MemoryFilter, MemoryProvider,
    MemoryScope, Origin, Recorded, Settings,
};
use zupflow_core::organizations::MemberRole;
use zupflow_core::time::Timestamp;
use zupflow_core::webhooks::{NewWebhookEvent, WebhookEventType, event_body, memory_event_data};

use crate::auth::{Access, Actor, AppState, Caller, WorkspaceAccess};
use crate::channels::console_may_act;
use crate::http::{Failure, envelope, envelope_with_meta};
use crate::input::{Body, Object};
use crate::runs::query;
use crate::workspaces::{clamped_int, invalid_cursor};

pub fn routes() -> Router<AppState> {
    Router::new()
        .route(
            "/v1/workspaces/{ws}/agents/{agent}/memories",
            get(list_memories).post(create_memory).delete(delete_memories),
        )
        .route(
            "/v1/workspaces/{ws}/agents/{agent}/memories/{memory}",
            get(get_memory).patch(update_memory).delete(delete_memory),
        )
}

/// The contract's `Memory`: never its organization's ID, which the request
/// already names.
#[derive(Serialize)]
struct MemoryView<'a> {
    id: MemoryId,
    workspace_id: WorkspaceId,
    agent_id: AgentId,
    memory_type_id: MemoryTypeId,
    short_id: &'a ShortId,
    text: &'a str,
    channel_conversation_id: Option<ChannelConversationId>,
    origin: Origin,
    created_at: Timestamp,
    updated_at: Timestamp,
}

fn memory_view(memory: &Memory) -> MemoryView<'_> {
    MemoryView {
        id: memory.id,
        workspace_id: memory.workspace_id,
        agent_id: memory.agent_id,
        memory_type_id: memory.memory_type_id,
        short_id: &memory.short_id,
        text: &memory.text,
        channel_conversation_id: memory.channel_conversation_id,
        origin: memory.origin,
        created_at: memory.created_at,
        updated_at: memory.updated_at,
    }
}

/// What people do with a kind's memories, as its [`Manage`] allows each.
#[derive(Debug, Clone, Copy)]
enum Use {
    List,
    Add,
    Change,
    Forget,
}

impl Use {
    fn allowed(self, manage: Manage) -> bool {
        match self {
            Self::List => manage.list,
            Self::Add => manage.add,
            Self::Change => manage.change,
            Self::Forget => manage.forget,
        }
    }

    fn verb(self) -> &'static str {
        match self {
            Self::List => "list",
            Self::Add => "add",
            Self::Change => "change",
            Self::Forget => "forget",
        }
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

fn memory_not_found() -> Failure {
    ApiError::not_found("Memory").into()
}

fn agent_not_found() -> Failure {
    ApiError::not_found("Agent").into()
}

/// A refusal naming one field, with the same words for it.
fn field_refusal(status: u16, code: &'static str, field: &str, message: &str) -> Failure {
    ApiError {
        fields: Some(BTreeMap::from([(field.to_owned(), message.to_owned())])),
        ..ApiError::new(status, code, message)
    }
    .into()
}

/// A type `field` names that isn't one of the organization's.
fn unknown_type(field: &str) -> Failure {
    field_refusal(
        422,
        "unknown_memory_type",
        field,
        "Choose a memory type of this organization.",
    )
}

fn not_served(memory_type: &MemoryType) -> Failure {
    ApiError::new(
        422,
        "memory_kind_not_served",
        format!(
            "This server doesn't serve {}, the memory kind of {}.",
            memory_type.kind, memory_type.name
        ),
    )
    .into()
}

fn unsupported(memory_type: &MemoryType, what: Use) -> Failure {
    ApiError::new(
        422,
        "memory_kind_unsupported",
        format!(
            "{}'s memory kind, {}, can't {} memories from the API or the Console.",
            memory_type.name,
            memory_type.kind,
            what.verb()
        ),
    )
    .into()
}

/// What a kind's refusal answers, for a memory of `memory_type` it was asked
/// to `what`. A kind that's unavailable has logged the cause.
fn kind_failure(error: MemoryError, memory_type: &MemoryType, provider: &dyn MemoryProvider, what: Use) -> Failure {
    match error {
        MemoryError::Unavailable => unavailable(),
        MemoryError::NotFound => memory_not_found(),
        MemoryError::InvalidText => field_refusal(422, "invalid_memory_text", "text", &provider.text_rule()),
        MemoryError::InvalidCursor => invalid_cursor(),
        MemoryError::Unsupported => unsupported(memory_type, what),
        MemoryError::NotServed => not_served(memory_type),
        // Only a save finds no room, and that route names it.
        MemoryError::Full => ApiError::new(422, "memory_full", "The memory is full.").into(),
    }
}

/// The type store failed; it has logged the cause.
fn types_failure(_: type_store::MemoryTypeError) -> Failure {
    unavailable()
}

/// Who writes a memory's text: the API key, or the member from the Console.
fn origin_of(access: &Access) -> Origin {
    match &access.actor {
        Actor::Member(member) => Origin::Member {
            user_id: member.user.id,
        },
        Actor::ApiKey(key) => Origin::ApiKey { api_key_id: key.id },
    }
}

/// The agent the path names, in the request's workspace: another
/// workspace's, environment's or organization's is the same `404`.
async fn find_agent(state: &AppState, access: &WorkspaceAccess, agent: &str) -> Result<Agent, Failure> {
    let agent_id = AgentId::parse(agent).map_err(|_| agent_not_found())?;
    state
        .agents
        .find_agent(access.access.organization_id, access.workspace.id, agent_id)
        .await
        .map_err(|_| unavailable())?
        .ok_or_else(agent_not_found)
}

/// Where the agent keeps its memories in `memory_type`: built from the
/// request's organization and workspace, never from what a kind answers.
fn scope_of(access: &WorkspaceAccess, agent: &Agent, memory_type: &MemoryType) -> MemoryScope {
    MemoryScope {
        organization_id: access.access.organization_id,
        workspace_id: access.workspace.id,
        agent_id: agent.id,
        memory_type_id: memory_type.id,
    }
}

/// A type a request names by ID, the organization's or none: a malformed ID
/// names none.
async fn named_type(state: &AppState, access: &WorkspaceAccess, id: &str) -> Result<Option<MemoryType>, Failure> {
    let Ok(memory_type_id) = MemoryTypeId::parse(id) else {
        return Ok(None);
    };
    state
        .memory_types
        .find_type(access.access.organization_id, memory_type_id)
        .await
        .map_err(types_failure)
}

/// The type the agent uses now: its own, the organization's default for one
/// on the default, or none while its memory is off or there's no default.
async fn current_type(
    state: &AppState,
    access: &WorkspaceAccess,
    agent: &Agent,
) -> Result<Option<MemoryType>, Failure> {
    let organization_id = access.access.organization_id;
    match agent.memory {
        AgentMemory::Type(memory_type_id) => state
            .memory_types
            .find_type(organization_id, memory_type_id)
            .await
            .map_err(types_failure),
        AgentMemory::Default => Ok(state
            .memory_types
            .default_type(organization_id)
            .await
            .map_err(types_failure)?
            .memory_type),
        AgentMemory::Off => Ok(None),
    }
}

/// The provider of a type a list or a save names, when this server serves
/// its kind and the kind lets people `what` its memories.
fn provider_for(state: &AppState, memory_type: &MemoryType, what: Use) -> Result<Arc<dyn MemoryProvider>, Failure> {
    if !state.memory_providers.serves(&memory_type.kind) {
        return Err(not_served(memory_type));
    }
    let provider = state.memory_providers.provider(&memory_type.kind);
    allows(&*provider, memory_type, what)?;
    Ok(provider)
}

fn allows(provider: &dyn MemoryProvider, memory_type: &MemoryType, what: Use) -> Result<(), Failure> {
    if what.allowed(provider.describe().manage) {
        Ok(())
    } else {
        Err(unsupported(memory_type, what))
    }
}

/// Every type of the organization whose kind this server serves, by name.
async fn served_types(state: &AppState, access: &WorkspaceAccess) -> Result<Vec<MemoryType>, Failure> {
    let mut served = Vec::new();
    let mut cursor: Option<String> = None;
    loop {
        let page = state
            .memory_types
            .list_types(access.access.organization_id, cursor.as_deref(), type_store::MAX_PAGE)
            .await
            .map_err(types_failure)?;
        served.extend(
            page.items
                .into_iter()
                .filter(|memory_type| state.memory_providers.serves(&memory_type.kind)),
        );
        match page.next_cursor {
            Some(next) => cursor = Some(next),
            None => return Ok(served),
        }
    }
}

/// A memory the path names, with the type that keeps it and that type's
/// provider.
struct Found {
    memory: Memory,
    memory_type: MemoryType,
    provider: Arc<dyn MemoryProvider>,
    scope: MemoryScope,
}

/// One of the agent's memories in the workspace, by its ID, in whichever of
/// the organization's types of a served kind keeps it: a malformed ID, an
/// unknown one, another agent's, workspace's or organization's, and one of a
/// kind this server doesn't serve are the same `404`.
async fn find_memory(
    state: &AppState,
    access: &WorkspaceAccess,
    agent: &Agent,
    memory: &str,
) -> Result<Found, Failure> {
    let memory_id = MemoryId::parse(memory).map_err(|_| memory_not_found())?;
    for memory_type in served_types(state, access).await? {
        let provider = state.memory_providers.provider(&memory_type.kind);
        let scope = scope_of(access, agent, &memory_type);
        match provider.get(&scope, memory_id).await {
            Ok(memory) => {
                return Ok(Found {
                    memory,
                    memory_type,
                    provider,
                    scope,
                });
            }
            Err(MemoryError::NotFound | MemoryError::NotServed | MemoryError::Unsupported) => {}
            // The kind has logged the cause.
            Err(_) => return Err(unavailable()),
        }
    }
    Err(memory_not_found())
}

/// A memory as the audit log names it: by its agent and short ID, never by
/// its text.
fn memory_target(agent: &Agent, memory: &Memory) -> AuditTarget {
    AuditTarget::new(
        AuditObject::Memory(memory.id),
        format!("{} · {}", agent.name, memory.short_id.as_str()),
    )
}

fn workspace_audit(access: &WorkspaceAccess, action: AuditAction, target: AuditTarget) -> AuditEntry {
    AuditEntry::new(
        access.access.audit_actor(),
        action,
        target,
        AuditPlace::Workspace(access.workspace.id),
    )
}

/// A change's webhook event, for the endpoints of the workspace's
/// environment that subscribe to it: it names the memory, never its text.
fn memory_event(access: &WorkspaceAccess, change: &MemoryChange) -> NewWebhookEvent {
    let kind = WebhookEventType::of_memory_change(change);
    let id = WebhookEventId::generate();
    let now = Timestamp::now();
    let workspace = &access.workspace;
    NewWebhookEvent {
        id,
        environment: workspace.environment,
        workspace_id: Some(workspace.id),
        kind: kind.as_str().to_owned(),
        body: event_body(
            id,
            kind.as_str(),
            now,
            workspace.environment,
            access.access.organization_id,
            Some((workspace.id, workspace.external_id.as_deref())),
            memory_event_data(change.memory()),
        ),
        created_at: now,
    }
}

/// What one change records with it ([`zupflow_core::memory::Record`]): its
/// webhook event, and its audit event, by the caller.
fn recorded(access: &WorkspaceAccess, agent: &Agent, change: &MemoryChange) -> Recorded {
    let action = match change {
        MemoryChange::Saved(_) => AuditAction::MemorySaved,
        MemoryChange::Updated(_) => AuditAction::MemoryUpdated,
        MemoryChange::Forgotten(_) => AuditAction::MemoryForgotten,
    };
    Recorded {
        audit: Some(workspace_audit(access, action, memory_target(agent, change.memory()))),
        events: vec![memory_event(access, change)],
    }
}

/// An empty page: what an agent without a type to list keeps.
fn empty_page() -> Response {
    envelope_with_meta(
        StatusCode::OK,
        Vec::<MemoryView<'_>>::new(),
        json!({ "pagination": { "next_cursor": null, "total": 0 } }),
    )
}

/// The agent's memories in one type, newest first: the one `memory_type`
/// names, or else the agent's own. An agent without one (its memory off, or
/// on a default the organization hasn't chosen) lists an empty page.
async fn list_memories(
    State(state): State<AppState>,
    caller: Caller,
    headers: HeaderMap,
    Path((ws, agent)): Path<(String, String)>,
    RawQuery(raw): RawQuery,
) -> Result<Response, Failure> {
    let access = WorkspaceAccess::resolve(&state, caller, &headers, &ws).await?;
    let query = query(raw.as_deref())?;
    let agent = find_agent(&state, &access, &agent).await?;
    let memory_type = match query.get("memory_type") {
        Some(named) => Some(
            named_type(&state, &access, named)
                .await?
                .ok_or_else(|| unknown_type("memory_type"))?,
        ),
        None => current_type(&state, &access, &agent).await?,
    };
    let Some(memory_type) = memory_type else {
        return Ok(empty_page());
    };
    let provider = provider_for(&state, &memory_type, Use::List)?;
    let limit = clamped_int(
        query.get("limit").map(String::as_str),
        i64::from(DEFAULT_PAGE),
        i64::from(MAX_PAGE),
    );
    let filter = MemoryFilter {
        q: query.get("q").cloned(),
        cursor: query.get("cursor").cloned(),
        limit: u32::try_from(limit).unwrap_or(DEFAULT_PAGE),
    };
    let page = provider
        .list(&scope_of(&access, &agent, &memory_type), &filter)
        .await
        .map_err(|error| kind_failure(error, &memory_type, &*provider, Use::List))?;
    let items: Vec<MemoryView<'_>> = page.items.iter().map(memory_view).collect();
    Ok(envelope_with_meta(
        StatusCode::OK,
        items,
        json!({ "pagination": { "next_cursor": page.next_cursor, "total": page.total } }),
    ))
}

/// `400 validation_failed` for a memory's text that isn't a string.
fn text_required() -> Failure {
    ApiError::validation(BTreeMap::from([("text".to_owned(), "Write the memory.".to_owned())])).into()
}

/// `text`, a string, and `memory_type_id`, a string when sent (`null` too is
/// refused): each field that isn't is named.
fn create_input(body: &Object) -> Result<(&str, Option<&str>), Failure> {
    let text = match body.0.get("text") {
        Some(Value::String(text)) => Some(text.as_str()),
        _ => None,
    };
    let named = match body.0.get("memory_type_id") {
        None => Ok(None),
        Some(Value::String(id)) => Ok(Some(id.as_str())),
        Some(_) => Err(()),
    };
    match (text, named) {
        (Some(text), Ok(named)) => Ok((text, named)),
        (text, named) => {
            let mut fields = BTreeMap::new();
            if text.is_none() {
                fields.insert("text".to_owned(), "Write the memory.".to_owned());
            }
            if named.is_err() {
                fields.insert(
                    "memory_type_id".to_owned(),
                    "Choose a memory type of this organization.".to_owned(),
                );
            }
            Err(ApiError::validation(fields).into())
        }
    }
}

/// Saves a memory for the whole workspace, written by the caller, in the
/// type the body names, or else the agent's own: even a type the agent
/// doesn't use, and even while its memory is off.
async fn create_memory(
    State(state): State<AppState>,
    caller: Caller,
    headers: HeaderMap,
    Path((ws, agent)): Path<(String, String)>,
    body: Body,
) -> Result<Response, Failure> {
    let access = WorkspaceAccess::resolve(&state, caller, &headers, &ws).await?;
    access.access.require(MemberRole::is_developer, "save memories")?;
    console_may_act(&access, "save memories")?;
    let agent = find_agent(&state, &access, &agent).await?;
    let body = body.object()?;
    let (text, named) = create_input(&body)?;
    let memory_type = match named {
        Some(named) => named_type(&state, &access, named)
            .await?
            .ok_or_else(|| unknown_type("memory_type_id"))?,
        None => current_type(&state, &access, &agent).await?.ok_or_else(|| {
            Failure(ApiError::new(
                422,
                "memory_off",
                format!(
                    "{} has no memory to save to: its memory is off, or the organization has no default memory \
                     type. Name a memory type.",
                    agent.name
                ),
            ))
        })?,
    };
    let provider = provider_for(&state, &memory_type, Use::Add)?;
    let settings = stored_settings(&*provider, &access, &memory_type)?;
    let record = |change: &MemoryChange| recorded(&access, &agent, change);
    let saved = provider
        .add(
            &scope_of(&access, &agent, &memory_type),
            MemoryId::generate(),
            &origin_of(&access.access),
            &settings,
            text,
            Timestamp::now(),
            &record,
        )
        .await;
    match saved {
        Ok(saved) => Ok(envelope(StatusCode::CREATED, memory_view(&saved))),
        Err(MemoryError::Full) => Err(ApiError::new(
            422,
            "memory_full",
            format!(
                "{} keeps {} memories at most in {}. Forget one, or change one instead.",
                agent.name,
                provider.limit(&settings),
                memory_type.name
            ),
        )
        .into()),
        Err(MemoryError::NotFound) => Err(gone_meanwhile(&state, &access, &agent).await),
        Err(error) => Err(kind_failure(error, &memory_type, &*provider, Use::Add)),
    }
}

/// What a save answers when its kind found nowhere to keep the memory: its
/// agent went meanwhile (`404`), or else its type, which is then no type of
/// the organization's.
async fn gone_meanwhile(state: &AppState, access: &WorkspaceAccess, agent: &Agent) -> Failure {
    match state
        .agents
        .find_agent(access.access.organization_id, access.workspace.id, agent.id)
        .await
    {
        Ok(Some(_)) => unknown_type("memory_type_id"),
        Ok(None) => agent_not_found(),
        Err(_) => unavailable(),
    }
}

/// The type's settings as stored, checked again by its kind, which kept
/// them when the type was saved: a refusal now is the server's to fix.
fn stored_settings(
    provider: &dyn MemoryProvider,
    access: &WorkspaceAccess,
    memory_type: &MemoryType,
) -> Result<Settings, Failure> {
    provider.check_settings(&memory_type.settings).map_err(|refusal| {
        tracing::error!(
            event = "memories.settings_refused",
            organization_id = %access.access.organization_id,
            memory_type_id = %memory_type.id,
            kind = memory_type.kind.as_str(),
            settings = ?refusal.fields.keys().collect::<Vec<_>>(),
        );
        unavailable()
    })
}

/// One of the agent's memories, as it is now: what a webhook's consumer
/// reads after an event names it.
async fn get_memory(
    State(state): State<AppState>,
    caller: Caller,
    headers: HeaderMap,
    Path((ws, agent, memory)): Path<(String, String, String)>,
) -> Result<Response, Failure> {
    let access = WorkspaceAccess::resolve(&state, caller, &headers, &ws).await?;
    let agent = find_agent(&state, &access, &agent).await?;
    let found = find_memory(&state, &access, &agent, &memory).await?;
    allows(&*found.provider, &found.memory_type, Use::List)?;
    Ok(envelope(StatusCode::OK, memory_view(&found.memory)))
}

/// Replaces a memory's text, written by the caller now. The text it holds
/// already, once kept as its kind keeps it, changes nothing: the memory is
/// answered as it was.
async fn update_memory(
    State(state): State<AppState>,
    caller: Caller,
    headers: HeaderMap,
    Path((ws, agent, memory)): Path<(String, String, String)>,
    body: Body,
) -> Result<Response, Failure> {
    let access = WorkspaceAccess::resolve(&state, caller, &headers, &ws).await?;
    access.access.require(MemberRole::is_developer, "change memories")?;
    console_may_act(&access, "change memories")?;
    let agent = find_agent(&state, &access, &agent).await?;
    // The memory first, then what its kind lets people do, then the text.
    let found = find_memory(&state, &access, &agent, &memory).await?;
    allows(&*found.provider, &found.memory_type, Use::Change)?;
    let body = body.object()?;
    let Some(Value::String(text)) = body.0.get("text") else {
        return Err(text_required());
    };
    let record = |change: &MemoryChange| recorded(&access, &agent, change);
    let rewritten = found
        .provider
        .change(
            &found.scope,
            &origin_of(&access.access),
            found.memory.id,
            text,
            Timestamp::now(),
            &record,
        )
        .await
        .map_err(|error| kind_failure(error, &found.memory_type, &*found.provider, Use::Change))?;
    Ok(envelope(StatusCode::OK, memory_view(&rewritten.memory)))
}

async fn delete_memory(
    State(state): State<AppState>,
    caller: Caller,
    headers: HeaderMap,
    Path((ws, agent, memory)): Path<(String, String, String)>,
) -> Result<Response, Failure> {
    let access = WorkspaceAccess::resolve(&state, caller, &headers, &ws).await?;
    access.access.require(MemberRole::is_developer, "forget memories")?;
    console_may_act(&access, "forget memories")?;
    let agent = find_agent(&state, &access, &agent).await?;
    let found = find_memory(&state, &access, &agent, &memory).await?;
    allows(&*found.provider, &found.memory_type, Use::Forget)?;
    let record = |change: &MemoryChange| recorded(&access, &agent, change);
    found
        .provider
        .forget(&found.scope, found.memory.id, &record)
        .await
        .map_err(|error| kind_failure(error, &found.memory_type, &*found.provider, Use::Forget))?;
    Ok(StatusCode::NO_CONTENT.into_response())
}

/// Forgets everything the agent keeps in the workspace, in every type of a
/// kind this server serves, whatever the kind lets people do: an erasure.
/// Each type at once, in one call to its kind, which commits with it a
/// `memory.forgotten` for each memory and, when there was one, an
/// `agent.memory_forgotten` naming the type and how many. A type that fails
/// stops the rest; the types before stay forgotten, with their records.
async fn delete_memories(
    State(state): State<AppState>,
    caller: Caller,
    headers: HeaderMap,
    Path((ws, agent)): Path<(String, String)>,
) -> Result<Response, Failure> {
    let access = WorkspaceAccess::resolve(&state, caller, &headers, &ws).await?;
    access.access.require(MemberRole::is_developer, "forget memories")?;
    console_may_act(&access, "forget memories")?;
    let agent = find_agent(&state, &access, &agent).await?;
    // Each memory's event; the type's audit event counts them.
    let record = |change: &MemoryChange| Recorded {
        audit: None,
        events: vec![memory_event(&access, change)],
    };
    for memory_type in served_types(&state, &access).await? {
        let provider = state.memory_providers.provider(&memory_type.kind);
        let counted = |count: &u64| {
            let count = if *count == 1 {
                "1 memory".to_owned()
            } else {
                format!("{count} memories")
            };
            let target = AuditTarget::new(
                AuditObject::Agent(agent.id),
                format!("{} · {} · {count}", agent.name, memory_type.name),
            );
            workspace_audit(&access, AuditAction::AgentMemoryForgotten, target)
        };
        provider
            .forget_scope(&scope_of(&access, &agent, &memory_type), &record, &counted)
            .await
            .map_err(|error| kind_failure(error, &memory_type, &*provider, Use::Forget))?;
    }
    Ok(StatusCode::NO_CONTENT.into_response())
}
