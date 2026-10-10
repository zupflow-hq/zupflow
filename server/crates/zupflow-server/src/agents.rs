//! Agents and their grants (decision 0040), at
//! `/v1/workspaces/{ws}/agents…`, and the models an agent can use, at
//! `/v1/models`. Console members and API keys both call them; every change
//! needs a developer or stricter.
//!
//! A grant names tools in its installation's registry: one that isn't there,
//! or is gone, is refused with a field error, checked by the store in the
//! transaction that writes the grant. A grant of a catalog app's
//! installation is also held inside the organization's levels: a tool it
//! denies is refused, and one it sets to approve must require approval. The
//! Console isn't kept out of
//! Production here: agents and grants are configuration, not acting on a
//! customer's accounts, and the mock, the API's specification, allows them.

use std::collections::BTreeMap;

use axum::Router;
use axum::extract::{Path, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use serde::Serialize;
use serde_json::Value;
use zupflow_core::agents::{Agent, AgentChanges, AgentError, AgentStatus, Grant, GrantTools, NewAgent};
use zupflow_core::error::ApiError;
use zupflow_core::ids::{AgentId, GrantId, InstallationId, WorkspaceId};
use zupflow_core::models::ModelError;
use zupflow_core::organizations::MemberRole;
use zupflow_core::time::Timestamp;

use crate::auth::{AppState, Caller, WorkspaceAccess};
use crate::http::{Failure, envelope};
use crate::input::{Body, Fields, Object};

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/v1/models", get(list_models))
        .route("/v1/workspaces/{ws}/agents", get(list_agents).post(create_agent))
        .route(
            "/v1/workspaces/{ws}/agents/{agent}",
            get(get_agent).patch(update_agent).delete(delete_agent),
        )
        .route(
            "/v1/workspaces/{ws}/agents/{agent}/grants",
            get(list_grants).post(create_grant),
        )
        .route(
            "/v1/workspaces/{ws}/agents/{agent}/grants/{grant}",
            axum::routing::patch(update_grant).delete(delete_grant),
        )
}

const NAME_MAX: usize = 60;
const DESCRIPTION_MAX: usize = 280;
const INSTRUCTIONS_MAX: usize = 20_000;

/// The contract's `Agent`.
#[derive(Serialize)]
struct AgentView {
    id: AgentId,
    workspace_id: WorkspaceId,
    name: String,
    description: String,
    model: String,
    instructions: String,
    status: AgentStatus,
    grant_count: u64,
    runs_last_24h: u64,
    last_run_at: Option<Timestamp>,
    created_at: Timestamp,
    updated_at: Timestamp,
}

fn agent_view(agent: Agent) -> AgentView {
    AgentView {
        id: agent.id,
        workspace_id: agent.workspace_id,
        name: agent.name,
        description: agent.description,
        model: agent.model,
        instructions: agent.instructions,
        status: agent.status,
        grant_count: agent.grant_count,
        // Runs arrive with the run engine (milestone 4, step 3); until then an
        // agent has run none.
        runs_last_24h: 0,
        last_run_at: None,
        created_at: agent.created_at,
        updated_at: agent.updated_at,
    }
}

/// The contract's `Grant`.
#[derive(Serialize)]
struct GrantView {
    id: GrantId,
    workspace_id: WorkspaceId,
    agent_id: AgentId,
    installation_id: InstallationId,
    tools: Vec<String>,
    approval_required: Vec<String>,
    created_at: Timestamp,
    updated_at: Timestamp,
}

fn grant_view(grant: Grant) -> GrantView {
    GrantView {
        id: grant.id,
        workspace_id: grant.workspace_id,
        agent_id: grant.agent_id,
        installation_id: grant.installation_id,
        tools: grant.tools,
        approval_required: grant.approval_required,
        created_at: grant.created_at,
        updated_at: grant.updated_at,
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

/// A refusal about one field, with the message under it too.
fn field_error(status: u16, code: &'static str, field: &str, message: String) -> Failure {
    Failure(ApiError {
        fields: Some(BTreeMap::from([(field.to_owned(), message.clone())])),
        ..ApiError::new(status, code, message)
    })
}

/// The store's refusal as the API answers it. `installation` names the
/// grant's installation in a tool's refusal.
fn agent_failure(error: AgentError, installation: &str) -> Failure {
    match error {
        AgentError::AgentNotFound => ApiError::not_found("Agent").into(),
        AgentError::GrantNotFound => ApiError::not_found("Grant").into(),
        AgentError::InstallationNotFound => ApiError::not_found("Installation").into(),
        AgentError::GrantExists => ApiError::new(
            409,
            "grant_exists",
            "This agent already has access to that installation. Edit the existing grant.",
        )
        .into(),
        AgentError::UnknownTool(name) => field_error(
            422,
            "unknown_tool",
            "tools",
            format!("{name} isn't a tool of {installation}. Refresh its tools if the app added it."),
        ),
        AgentError::ToolGone(name) => field_error(
            422,
            "tool_gone",
            "tools",
            format!("{name} is no longer offered by {installation}. Remove it from the grant."),
        ),
        AgentError::ApprovalOutsideGrant(name) => field_error(
            422,
            "approval_outside_grant",
            "approval_required",
            format!("{name} requires approval but isn't granted."),
        ),
        AgentError::ToolDenied(name) => field_error(
            422,
            "tool_denied",
            "tools",
            format!("Your organization blocks {name}. Remove it from the grant."),
        ),
        AgentError::ApprovalRequiredByPolicy(name) => field_error(
            422,
            "approval_required_by_policy",
            "approval_required",
            format!("Your organization requires approval for {name}. Add it to approval required."),
        ),
        // The store has logged the cause.
        AgentError::Unavailable => unavailable(),
    }
}

fn models_unavailable(_: ModelError) -> Failure {
    unavailable()
}

/// An agent ID from the path; anything else is the same `404` as an agent that isn't there.
fn agent_id(agent: &str) -> Result<AgentId, Failure> {
    AgentId::parse(agent).map_err(|_| ApiError::not_found("Agent").into())
}

fn grant_id(grant: &str) -> Result<GrantId, Failure> {
    GrantId::parse(grant).map_err(|_| ApiError::not_found("Grant").into())
}

async fn find(state: &AppState, access: &WorkspaceAccess, agent: &str) -> Result<Agent, Failure> {
    state
        .agents
        .find_agent(access.access.organization_id, access.workspace.id, agent_id(agent)?)
        .await
        .map_err(|error| agent_failure(error, "this installation"))?
        .ok_or_else(|| ApiError::not_found("Agent").into())
}

/// The models agents can use. Any caller reads them.
async fn list_models(State(state): State<AppState>, _caller: Caller) -> Result<Response, Failure> {
    let models = state.models.list_models().await.map_err(models_unavailable)?;
    Ok(envelope(StatusCode::OK, models))
}

async fn list_agents(
    State(state): State<AppState>,
    caller: Caller,
    headers: HeaderMap,
    Path(ws): Path<String>,
) -> Result<Response, Failure> {
    let access = WorkspaceAccess::resolve(&state, caller, &headers, &ws).await?;
    let agents = state
        .agents
        .list_agents(access.access.organization_id, access.workspace.id)
        .await
        .map_err(|error| agent_failure(error, "this installation"))?;
    Ok(envelope(
        StatusCode::OK,
        agents.into_iter().map(agent_view).collect::<Vec<_>>(),
    ))
}

async fn get_agent(
    State(state): State<AppState>,
    caller: Caller,
    headers: HeaderMap,
    Path((ws, agent)): Path<(String, String)>,
) -> Result<Response, Failure> {
    let access = WorkspaceAccess::resolve(&state, caller, &headers, &ws).await?;
    let agent = find(&state, &access, &agent).await?;
    Ok(envelope(StatusCode::OK, agent_view(agent)))
}

/// A name: trimmed, 1 to 60 characters.
fn checked_name(value: String, fields: &mut Fields) -> Option<String> {
    let name = value.trim().to_owned();
    if name.is_empty() {
        fields.invalid("name", "Required.");
        None
    } else if name.chars().count() > NAME_MAX {
        fields.invalid("name", "Use 60 characters or fewer.");
        None
    } else {
        Some(name)
    }
}

fn checked_description(value: String, fields: &mut Fields) -> Option<String> {
    let description = value.trim().to_owned();
    if description.chars().count() > DESCRIPTION_MAX {
        fields.invalid("description", "Use 280 characters or fewer.");
        None
    } else {
        Some(description)
    }
}

/// Instructions are kept exactly as written.
fn checked_instructions(value: String, fields: &mut Fields) -> Option<String> {
    if value.chars().count() > INSTRUCTIONS_MAX {
        fields.invalid("instructions", "Use 20,000 characters or fewer.");
        None
    } else {
        Some(value)
    }
}

/// A model the catalog offers.
async fn checked_model(state: &AppState, value: String, fields: &mut Fields) -> Result<Option<String>, Failure> {
    if state.models.offers(&value).await.map_err(models_unavailable)? {
        Ok(Some(value))
    } else {
        fields.invalid("model", "Choose a model.");
        Ok(None)
    }
}

async fn create_agent(
    State(state): State<AppState>,
    caller: Caller,
    headers: HeaderMap,
    Path(ws): Path<String>,
    body: Body,
) -> Result<Response, Failure> {
    let access = WorkspaceAccess::resolve(&state, caller, &headers, &ws).await?;
    access.access.require(MemberRole::is_developer, "create agents")?;
    let body = body.object()?;
    let mut fields = Fields::default();
    let name = body
        .string("name", &mut fields)
        .and_then(|name| checked_name(name, &mut fields));
    let description = body
        .string("description", &mut fields)
        .and_then(|description| checked_description(description, &mut fields));
    let model = match body.string("model", &mut fields) {
        Some(model) => checked_model(&state, model, &mut fields).await?,
        None => None,
    };
    let instructions = body
        .string("instructions", &mut fields)
        .and_then(|instructions| checked_instructions(instructions, &mut fields));
    fields.finish()?;
    let (Some(name), Some(description), Some(model), Some(instructions)) = (name, description, model, instructions)
    else {
        return Err(ApiError::internal().into());
    };
    let agent = state
        .agents
        .create_agent(
            access.access.organization_id,
            access.workspace.id,
            AgentId::generate(),
            &NewAgent {
                name,
                description,
                model,
                instructions,
            },
        )
        .await
        .map_err(|error| agent_failure(error, "this installation"))?;
    Ok(envelope(StatusCode::CREATED, agent_view(agent)))
}

async fn update_agent(
    State(state): State<AppState>,
    caller: Caller,
    headers: HeaderMap,
    Path((ws, agent)): Path<(String, String)>,
    body: Body,
) -> Result<Response, Failure> {
    let access = WorkspaceAccess::resolve(&state, caller, &headers, &ws).await?;
    access.access.require(MemberRole::is_developer, "change agents")?;
    let agent = find(&state, &access, &agent).await?;
    let body = body.object()?;
    let mut fields = Fields::default();
    let name = body
        .optional_string("name", &mut fields)
        .and_then(|name| checked_name(name, &mut fields));
    let description = body
        .optional_string("description", &mut fields)
        .and_then(|description| checked_description(description, &mut fields));
    let model = match body.optional_string("model", &mut fields) {
        Some(model) => checked_model(&state, model, &mut fields).await?,
        None => None,
    };
    let instructions = body
        .optional_string("instructions", &mut fields)
        .and_then(|instructions| checked_instructions(instructions, &mut fields));
    let status = body.optional_string("status", &mut fields).and_then(|status| {
        let parsed = AgentStatus::parse(&status);
        if parsed.is_none() {
            fields.invalid("status", "Choose active or paused.");
        }
        parsed
    });
    fields.finish()?;
    let updated = state
        .agents
        .update_agent(
            access.access.organization_id,
            access.workspace.id,
            agent.id,
            &AgentChanges {
                name,
                description,
                model,
                instructions,
                status,
            },
        )
        .await
        .map_err(|error| agent_failure(error, "this installation"))?;
    Ok(envelope(StatusCode::OK, agent_view(updated)))
}

async fn delete_agent(
    State(state): State<AppState>,
    caller: Caller,
    headers: HeaderMap,
    Path((ws, agent)): Path<(String, String)>,
) -> Result<Response, Failure> {
    let access = WorkspaceAccess::resolve(&state, caller, &headers, &ws).await?;
    access.access.require(MemberRole::is_developer, "delete agents")?;
    let agent = find(&state, &access, &agent).await?;
    state
        .agents
        .delete_agent(access.access.organization_id, access.workspace.id, agent.id)
        .await
        .map_err(|error| agent_failure(error, "this installation"))?;
    Ok(StatusCode::NO_CONTENT.into_response())
}

async fn list_grants(
    State(state): State<AppState>,
    caller: Caller,
    headers: HeaderMap,
    Path((ws, agent)): Path<(String, String)>,
) -> Result<Response, Failure> {
    let access = WorkspaceAccess::resolve(&state, caller, &headers, &ws).await?;
    let grants = state
        .agents
        .list_grants(access.access.organization_id, access.workspace.id, agent_id(&agent)?)
        .await
        .map_err(|error| agent_failure(error, "this installation"))?;
    Ok(envelope(
        StatusCode::OK,
        grants.into_iter().map(grant_view).collect::<Vec<_>>(),
    ))
}

/// A list of tool names; `None` (and a field error) when it isn't one.
fn names(body: &Object, field: &str, fields: &mut Fields) -> Option<Vec<String>> {
    match body.0.get(field) {
        None => {
            fields.invalid(field, "Required.");
            None
        }
        Some(Value::Array(items)) => {
            let names: Option<Vec<String>> = items.iter().map(|item| item.as_str().map(str::to_owned)).collect();
            if names.is_none() {
                fields.invalid(field, "Must be a list of tool names.");
            }
            names
        }
        Some(_) => {
            fields.invalid(field, "Must be a list of tool names.");
            None
        }
    }
}

/// A grant's tools and approvals, as a body names them; at least one tool.
fn grant_tools(body: &Object, fields: &mut Fields, empty: &str) -> Option<GrantTools> {
    let tools = names(body, "tools", fields).filter(|tools| {
        if tools.is_empty() {
            fields.invalid("tools", empty);
        }
        !tools.is_empty()
    });
    let approval_required = names(body, "approval_required", fields);
    Some(GrantTools {
        tools: tools?,
        approval_required: approval_required?,
    })
}

/// What a tool's refusal calls the installation: its label, when it can be read.
async fn installation_label(state: &AppState, access: &WorkspaceAccess, installation: InstallationId) -> String {
    match state
        .installations
        .find_installation(access.access.organization_id, access.workspace.id, installation)
        .await
    {
        Ok(Some(installation)) => installation.label,
        _ => "this installation".to_owned(),
    }
}

async fn grant_failure(
    state: &AppState,
    access: &WorkspaceAccess,
    installation: InstallationId,
    error: AgentError,
) -> Failure {
    let label = match error {
        AgentError::UnknownTool(_) | AgentError::ToolGone(_) => installation_label(state, access, installation).await,
        _ => String::new(),
    };
    agent_failure(error, &label)
}

async fn create_grant(
    State(state): State<AppState>,
    caller: Caller,
    headers: HeaderMap,
    Path((ws, agent)): Path<(String, String)>,
    body: Body,
) -> Result<Response, Failure> {
    let access = WorkspaceAccess::resolve(&state, caller, &headers, &ws).await?;
    access.access.require(MemberRole::is_developer, "grant access")?;
    let agent = find(&state, &access, &agent).await?;
    let body = body.object()?;
    let mut fields = Fields::default();
    let installation = body.string("installation_id", &mut fields);
    let tools = grant_tools(&body, &mut fields, "Choose at least one tool.");
    fields.finish()?;
    let (Some(installation), Some(tools)) = (installation, tools) else {
        return Err(ApiError::internal().into());
    };
    // A malformed ID matches nothing: the same 404 as one that isn't here.
    let installation =
        InstallationId::parse(&installation).map_err(|_| Failure(ApiError::not_found("Installation")))?;
    let grant = match state
        .agents
        .create_grant(
            access.access.organization_id,
            access.workspace.id,
            agent.id,
            GrantId::generate(),
            installation,
            &tools,
        )
        .await
    {
        Ok(grant) => grant,
        Err(error) => return Err(grant_failure(&state, &access, installation, error).await),
    };
    Ok(envelope(StatusCode::CREATED, grant_view(grant)))
}

async fn update_grant(
    State(state): State<AppState>,
    caller: Caller,
    headers: HeaderMap,
    Path((ws, agent, grant)): Path<(String, String, String)>,
    body: Body,
) -> Result<Response, Failure> {
    let access = WorkspaceAccess::resolve(&state, caller, &headers, &ws).await?;
    access.access.require(MemberRole::is_developer, "change access")?;
    let agent = find(&state, &access, &agent).await?;
    let grant = grant_id(&grant)?;
    let existing = state
        .agents
        .list_grants(access.access.organization_id, access.workspace.id, agent.id)
        .await
        .map_err(|error| agent_failure(error, "this installation"))?
        .into_iter()
        .find(|existing| existing.id == grant)
        .ok_or_else(|| Failure(ApiError::not_found("Grant")))?;
    let body = body.object()?;
    let mut fields = Fields::default();
    let tools = grant_tools(&body, &mut fields, "Choose at least one tool, or remove the grant.");
    fields.finish()?;
    let Some(tools) = tools else {
        return Err(ApiError::internal().into());
    };
    let updated = match state
        .agents
        .update_grant(
            access.access.organization_id,
            access.workspace.id,
            agent.id,
            grant,
            &tools,
        )
        .await
    {
        Ok(updated) => updated,
        Err(error) => return Err(grant_failure(&state, &access, existing.installation_id, error).await),
    };
    Ok(envelope(StatusCode::OK, grant_view(updated)))
}

async fn delete_grant(
    State(state): State<AppState>,
    caller: Caller,
    headers: HeaderMap,
    Path((ws, agent, grant)): Path<(String, String, String)>,
) -> Result<Response, Failure> {
    let access = WorkspaceAccess::resolve(&state, caller, &headers, &ws).await?;
    access.access.require(MemberRole::is_developer, "remove access")?;
    let agent = find(&state, &access, &agent).await?;
    state
        .agents
        .delete_grant(
            access.access.organization_id,
            access.workspace.id,
            agent.id,
            grant_id(&grant)?,
        )
        .await
        .map_err(|error| agent_failure(error, "this installation"))?;
    Ok(StatusCode::NO_CONTENT.into_response())
}
