//! Runs (milestone 4), at `/v1/workspaces/{ws}/agents/{agent}/runs`,
//! `/v1/workspaces/{ws}/runs…` and `/v1/organizations/{org}/runs`. Console
//! members and API keys both read, start and cancel them; starting or
//! cancelling needs a developer or stricter. The Console doesn't start runs
//! in Production: the customer's product does that through the API.
//!
//! Approvals are decided in the customer's product and recorded by its
//! backend with an API key: a Console session only observes them. A
//! decision on the call the run waits for sets it running again and queues
//! it for the run engine.

use std::collections::BTreeMap;

use axum::Router;
use axum::extract::{Path, RawQuery, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::Response;
use axum::routing::{get, post};
use serde::Serialize;
use serde_json::{Value, json};
use zupflow_core::agents::{AgentError, AgentStatus};
use zupflow_core::environment::Environment;
use zupflow_core::error::ApiError;
use zupflow_core::ids::{AgentId, RunEventId, RunId, SessionId, ToolCallId, WorkspaceId};
use zupflow_core::organizations::MemberRole;
use zupflow_core::runs::{
    ApprovalDecision, CallOutcome, Failure as RunFailure, MAX_INPUT, NewRun, NewRunEvent, OrganizationRun,
    OrganizationRunFilter, Page, Run, RunError, RunEvent, RunEventType, RunFilter, RunStatus, RunTrigger, RunUsage,
};
use zupflow_core::time::Timestamp;

use crate::auth::{Access, Actor, AppState, Caller, WorkspaceAccess};
use crate::http::{Failure, envelope, envelope_with_meta};
use crate::input::{Body, Fields};
use crate::run_engine;
use crate::workspaces::{environment_mismatch, invalid_cursor};

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/v1/organizations/{org}/runs", get(list_organization_runs))
        .route("/v1/workspaces/{ws}/agents/{agent}/runs", post(start_run))
        .route("/v1/workspaces/{ws}/runs", get(list_runs))
        .route("/v1/workspaces/{ws}/runs/{run}", get(get_run))
        .route("/v1/workspaces/{ws}/runs/{run}/events", get(list_run_events))
        .route("/v1/workspaces/{ws}/runs/{run}/cancel", post(cancel_run))
        .route("/v1/workspaces/{ws}/runs/{run}/approvals", post(resolve_approval))
}

/// The longest reason a denial carries, in characters.
const MAX_REASON: usize = 500;

/// What a run whose agent was deleted names it.
const DELETED_AGENT: &str = "Deleted agent";

/// The contract's `Run`.
#[derive(Serialize)]
struct RunView {
    id: RunId,
    workspace_id: WorkspaceId,
    agent_id: AgentId,
    agent_name: String,
    session_id: SessionId,
    trigger: &'static str,
    status: &'static str,
    input: String,
    output: Option<String>,
    usage: RunUsage,
    error: Option<RunFailure>,
    created_at: Timestamp,
    started_at: Option<Timestamp>,
    finished_at: Option<Timestamp>,
}

fn run_view(run: Run) -> RunView {
    RunView {
        id: run.id,
        workspace_id: run.workspace_id,
        agent_id: run.agent_id,
        agent_name: run.agent_name.unwrap_or_else(|| DELETED_AGENT.to_owned()),
        session_id: run.session_id,
        trigger: run.trigger.as_str(),
        status: run.status.as_str(),
        input: run.input,
        output: run.output,
        usage: run.usage,
        error: run.error,
        created_at: run.created_at,
        started_at: run.started_at,
        finished_at: run.finished_at,
    }
}

#[derive(Serialize)]
struct WorkspaceRef {
    id: WorkspaceId,
    name: String,
    slug: String,
    environment: Environment,
}

/// The contract's `OrganizationRun`.
#[derive(Serialize)]
struct OrganizationRunView {
    #[serde(flatten)]
    run: RunView,
    workspace: WorkspaceRef,
}

fn organization_run_view(row: OrganizationRun) -> OrganizationRunView {
    OrganizationRunView {
        workspace: WorkspaceRef {
            id: row.run.workspace_id,
            name: row.workspace_name,
            slug: row.workspace_slug,
            environment: row.environment,
        },
        run: run_view(row.run),
    }
}

/// The contract's `RunEvent`.
#[derive(Serialize)]
struct RunEventView {
    id: RunEventId,
    run_id: RunId,
    workspace_id: WorkspaceId,
    sequence: u64,
    created_at: Timestamp,
    #[serde(rename = "type")]
    kind: &'static str,
    data: Value,
}

fn event_view(event: RunEvent) -> RunEventView {
    RunEventView {
        id: event.id,
        run_id: event.run_id,
        workspace_id: event.workspace_id,
        sequence: event.sequence,
        created_at: event.created_at,
        kind: event.kind.as_str(),
        data: event.data,
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

fn run_failure(error: RunError) -> Failure {
    match error {
        RunError::NotFound => ApiError::not_found("Run").into(),
        RunError::AgentNotFound => ApiError::not_found("Agent").into(),
        RunError::InvalidCursor => invalid_cursor(),
        RunError::Finished => finished(),
        RunError::NotWaiting => ApiError::new(
            409,
            "not_waiting",
            "This run isn't waiting for a decision on that call. Read the run's events for the call it waits for.",
        )
        .into(),
        // A step moved it first; the caller can read it again.
        RunError::Moved => ApiError::new(
            409,
            "run_changed",
            "The run changed while this request was handled. Read it again, then retry.",
        )
        .into(),
        // The store has logged the cause.
        RunError::Unavailable => unavailable(),
    }
}

fn finished() -> Failure {
    ApiError::new(409, "run_finished", "This run already finished.").into()
}

fn validation(field: &str, message: &str) -> Failure {
    Failure(ApiError {
        fields: Some(BTreeMap::from([(field.to_owned(), message.to_owned())])),
        ..ApiError::new(400, "validation_failed", message)
    })
}

/// A run's ID from the path; anything else is the same `404` as a run that isn't there.
fn run_id(run: &str) -> Result<RunId, Failure> {
    RunId::parse(run).map_err(|_| ApiError::not_found("Run").into())
}

async fn find(state: &AppState, access: &WorkspaceAccess, run: &str) -> Result<Run, Failure> {
    state
        .runs
        .find_run(access.access.organization_id, access.workspace.id, run_id(run)?)
        .await
        .map_err(run_failure)?
        .ok_or_else(|| ApiError::not_found("Run").into())
}

/// A list's query, first value of each name.
fn query(raw: Option<&str>) -> Result<BTreeMap<String, String>, Failure> {
    let pairs: Vec<(String, String)> = serde_urlencoded::from_str(raw.unwrap_or_default()).map_err(|_| {
        Failure(ApiError::new(
            400,
            "validation_failed",
            "The query string can't be read.",
        ))
    })?;
    let mut query = BTreeMap::new();
    for (name, value) in pairs {
        query.entry(name).or_insert(value);
    }
    Ok(query)
}

fn listed_status(query: &BTreeMap<String, String>) -> Result<Option<RunStatus>, Failure> {
    match query
        .get("status")
        .map(String::as_str)
        .filter(|value| !value.is_empty())
    {
        None => Ok(None),
        Some(value) => RunStatus::parse(value)
            .map(Some)
            .ok_or_else(|| validation("status", "Unknown status.")),
    }
}

/// 1 to 100, 25 by default; a value that isn't an integer means the default.
fn page_limit(query: &BTreeMap<String, String>) -> u32 {
    query
        .get("limit")
        .and_then(|value| value.trim().parse::<i64>().ok())
        .map_or(25, |limit| limit.clamp(1, i64::from(zupflow_core::runs::MAX_PAGE)))
        .try_into()
        .unwrap_or(25)
}

fn cursor(query: &BTreeMap<String, String>) -> Option<&str> {
    query
        .get("cursor")
        .map(String::as_str)
        .filter(|cursor| !cursor.is_empty())
}

fn paginated<T, V: Serialize>(page: Page<T>, view: impl Fn(T) -> V) -> Response {
    let meta = json!({ "pagination": { "next_cursor": page.next_cursor, "total": page.total } });
    envelope_with_meta(
        StatusCode::OK,
        page.items.into_iter().map(view).collect::<Vec<_>>(),
        meta,
    )
}

fn empty_page() -> Response {
    envelope_with_meta(
        StatusCode::OK,
        Vec::<Value>::new(),
        json!({ "pagination": { "next_cursor": null, "total": 0 } }),
    )
}

async fn list_runs(
    State(state): State<AppState>,
    caller: Caller,
    headers: HeaderMap,
    Path(ws): Path<String>,
    RawQuery(raw): RawQuery,
) -> Result<Response, Failure> {
    let access = WorkspaceAccess::resolve(&state, caller, &headers, &ws).await?;
    let query = query(raw.as_deref())?;
    let status = listed_status(&query)?;
    // A malformed agent ID matches nothing.
    let agent_id = match query.get("agent_id").filter(|value| !value.is_empty()) {
        None => None,
        Some(value) => match AgentId::parse(value) {
            Ok(agent_id) => Some(agent_id),
            Err(_) => return Ok(empty_page()),
        },
    };
    let page = state
        .runs
        .list_runs(
            access.access.organization_id,
            access.workspace.id,
            &RunFilter { agent_id, status },
            cursor(&query),
            page_limit(&query),
        )
        .await
        .map_err(run_failure)?;
    Ok(paginated(page, run_view))
}

async fn list_organization_runs(
    State(state): State<AppState>,
    caller: Caller,
    Path(org): Path<String>,
    RawQuery(raw): RawQuery,
) -> Result<Response, Failure> {
    let access = Access::organization(&state, caller, &org).await?;
    let query = query(raw.as_deref())?;
    let asked = match query.get("environment").filter(|value| !value.is_empty()) {
        None => None,
        Some(value) => {
            Some(Environment::parse(value).ok_or_else(|| validation("environment", "Unknown environment."))?)
        }
    };
    // A key sees its own environment only: asking for the other is `422`.
    let environment = match (access.environment(), asked) {
        (Some(own), Some(asked)) if own != asked => return Err(environment_mismatch()),
        (Some(own), _) => Some(own),
        (None, asked) => asked,
    };
    let status = listed_status(&query)?;
    let workspace_id = match query.get("workspace_id").filter(|value| !value.is_empty()) {
        None => None,
        Some(value) => match WorkspaceId::parse(value) {
            Ok(workspace_id) => Some(workspace_id),
            Err(_) => return Ok(empty_page()),
        },
    };
    let page = state
        .runs
        .list_organization_runs(
            access.organization_id,
            &OrganizationRunFilter {
                environment,
                workspace_id,
                status,
            },
            cursor(&query),
            page_limit(&query),
        )
        .await
        .map_err(run_failure)?;
    Ok(paginated(page, organization_run_view))
}

async fn get_run(
    State(state): State<AppState>,
    caller: Caller,
    headers: HeaderMap,
    Path((ws, run)): Path<(String, String)>,
) -> Result<Response, Failure> {
    let access = WorkspaceAccess::resolve(&state, caller, &headers, &ws).await?;
    let run = find(&state, &access, &run).await?;
    Ok(envelope(StatusCode::OK, run_view(run)))
}

/// `after`, as a number: events with a greater sequence. Missing or empty
/// is `0`; one that isn't a number matches nothing.
fn after(query: &BTreeMap<String, String>) -> Option<u64> {
    let Some(value) = query.get("after").map(|value| value.trim()) else {
        return Some(0);
    };
    if value.is_empty() {
        return Some(0);
    }
    let after: f64 = value.parse().ok().filter(|after: &f64| !after.is_nan())?;
    if after <= 0.0 {
        Some(0)
    } else if after >= u64::MAX as f64 {
        Some(u64::MAX)
    } else {
        // Sequences are integers: greater than 2.5 is greater than 2.
        Some(after.floor() as u64)
    }
}

async fn list_run_events(
    State(state): State<AppState>,
    caller: Caller,
    headers: HeaderMap,
    Path((ws, run)): Path<(String, String)>,
    RawQuery(raw): RawQuery,
) -> Result<Response, Failure> {
    let access = WorkspaceAccess::resolve(&state, caller, &headers, &ws).await?;
    let run = find(&state, &access, &run).await?;
    let query = query(raw.as_deref())?;
    let Some(after) = after(&query) else {
        return Ok(envelope(StatusCode::OK, Vec::<Value>::new()));
    };
    let events = state
        .runs
        .list_events(access.access.organization_id, access.workspace.id, run.id, after)
        .await
        .map_err(run_failure)?;
    Ok(envelope(
        StatusCode::OK,
        events.into_iter().map(event_view).collect::<Vec<_>>(),
    ))
}

async fn start_run(
    State(state): State<AppState>,
    caller: Caller,
    headers: HeaderMap,
    Path((ws, agent)): Path<(String, String)>,
    body: Body,
) -> Result<Response, Failure> {
    let access = WorkspaceAccess::resolve(&state, caller, &headers, &ws).await?;
    access.access.require(MemberRole::is_developer, "start runs")?;
    let trigger = match access.access.actor {
        Actor::Member(_) => RunTrigger::Console,
        Actor::ApiKey(_) => RunTrigger::Api,
    };
    if trigger == RunTrigger::Console && access.workspace.environment == Environment::Production {
        return Err(ApiError::new(
            403,
            "console_read_only_in_production",
            "The Console can't start runs in Production. Your product does that through the API.",
        )
        .into());
    }
    let organization_id = access.access.organization_id;
    let workspace_id = access.workspace.id;
    let not_found = || Failure(ApiError::not_found("Agent"));
    let agent_id = AgentId::parse(&agent).map_err(|_| not_found())?;
    let agent = state
        .agents
        .find_agent(organization_id, workspace_id, agent_id)
        .await
        .map_err(|error| match error {
            AgentError::Unavailable => unavailable(),
            _ => not_found(),
        })?
        .ok_or_else(not_found)?;
    if agent.status == AgentStatus::Paused {
        return Err(ApiError::new(
            422,
            "agent_paused",
            format!("{} is paused. Resume it to start runs.", agent.name),
        )
        .into());
    }
    let body = body.object()?;
    let mut fields = Fields::default();
    let input = body.string("input", &mut fields).and_then(|input| {
        let input = input.trim().to_owned();
        if input.is_empty() {
            fields.invalid("input", "Write a message.");
            None
        } else if input.chars().count() > MAX_INPUT {
            fields.invalid("input", "Use 8,000 characters or fewer.");
            None
        } else {
            Some(input)
        }
    });
    fields.finish()?;
    let Some(input) = input else {
        return Err(ApiError::internal().into());
    };
    let run_id = RunId::generate();
    let Some(job) = run_engine::job(run_id) else {
        return Err(ApiError::internal().into());
    };
    let (run, events) = state
        .runs
        .create_run(
            organization_id,
            workspace_id,
            run_id,
            &NewRun {
                agent_id: agent.id,
                session_id: SessionId::generate(),
                trigger,
                input,
                model: agent.model,
                instructions: agent.instructions,
            },
            &[NewRunEvent::new(RunEventType::RunQueued, json!({}))],
            &job,
            Timestamp::now(),
        )
        .await
        .map_err(run_failure)?;
    run_engine::publish(
        &state,
        organization_id,
        workspace_id,
        access.workspace.environment,
        &events,
    )
    .await;
    Ok(envelope(StatusCode::CREATED, run_view(run)))
}

async fn cancel_run(
    State(state): State<AppState>,
    caller: Caller,
    headers: HeaderMap,
    Path((ws, run)): Path<(String, String)>,
) -> Result<Response, Failure> {
    let access = WorkspaceAccess::resolve(&state, caller, &headers, &ws).await?;
    access.access.require(MemberRole::is_developer, "cancel runs")?;
    let run = find(&state, &access, &run).await?;
    if run.status.is_terminal() {
        return Err(finished());
    }
    let cancelled_by = match &access.access.actor {
        Actor::Member(member) => member.user.name.clone(),
        Actor::ApiKey(key) => key.name.clone(),
    };
    let (run, events) = state
        .runs
        .cancel_run(
            access.access.organization_id,
            access.workspace.id,
            run.id,
            &[NewRunEvent::new(
                RunEventType::RunCancelled,
                json!({ "cancelled_by": cancelled_by }),
            )],
            Timestamp::now(),
        )
        .await
        .map_err(run_failure)?;
    run_engine::publish(
        &state,
        access.access.organization_id,
        access.workspace.id,
        access.workspace.environment,
        &events,
    )
    .await;
    Ok(envelope(StatusCode::OK, run_view(run)))
}

/// Records the decision the customer's product made on the call a run
/// waits for. API keys only: the person decides in the customer's product,
/// never in the Console.
async fn resolve_approval(
    State(state): State<AppState>,
    caller: Caller,
    headers: HeaderMap,
    Path((ws, run)): Path<(String, String)>,
    body: Body,
) -> Result<Response, Failure> {
    let access = WorkspaceAccess::resolve(&state, caller, &headers, &ws).await?;
    let run = find(&state, &access, &run).await?;
    let api_key_id = match &access.access.actor {
        Actor::ApiKey(key) => key.id,
        Actor::Member(_) => {
            return Err(ApiError::new(
                403,
                "approval_requires_api_key",
                "Approvals come from your backend with an API key, once your product has the decision.",
            )
            .into());
        }
    };
    let body = body.object()?;
    let mut fields = Fields::default();
    let call_id = body.string("call_id", &mut fields).and_then(|call_id| {
        let parsed = ToolCallId::parse(&call_id).ok();
        if parsed.is_none() {
            fields.invalid("call_id", "Not a call ID.");
        }
        parsed
    });
    let decision = body.string("decision", &mut fields).and_then(|decision| {
        let parsed = ApprovalDecision::parse(&decision);
        if parsed.is_none() {
            fields.invalid("decision", "Choose approved or denied.");
        }
        parsed
    });
    // A denial's reason is what the model reads back; bounded, and trimmed.
    let reason = body.optional_string("reason", &mut fields).and_then(|reason| {
        if reason.chars().count() > MAX_REASON {
            fields.invalid("reason", "Use 500 characters or fewer.");
            None
        } else {
            Some(reason.trim().to_owned()).filter(|reason| !reason.is_empty())
        }
    });
    fields.finish()?;
    let (Some(call_id), Some(decision)) = (call_id, decision) else {
        return Err(ApiError::internal().into());
    };
    let Some(job) = run_engine::job(run.id) else {
        return Err(ApiError::internal().into());
    };
    let (run, events) = state
        .runs
        .resolve_approval(
            access.access.organization_id,
            access.workspace.id,
            run.id,
            call_id,
            decision,
            api_key_id,
            &CallOutcome {
                content: match reason {
                    Some(reason) => format!("A person denied this call, so it wasn't made. Their reason: {reason}"),
                    None => run_engine::DENIED.to_owned(),
                },
                is_error: true,
            },
            &[NewRunEvent::new(
                RunEventType::ApprovalResolved,
                json!({ "call_id": call_id, "decision": decision.as_str(), "api_key_id": api_key_id }),
            )],
            &job,
            Timestamp::now(),
        )
        .await
        .map_err(run_failure)?;
    run_engine::publish(
        &state,
        access.access.organization_id,
        access.workspace.id,
        access.workspace.environment,
        &events,
    )
    .await;
    Ok(envelope(StatusCode::OK, run_view(run)))
}
