//! Runs (milestone 4), at `/v1/workspaces/{ws}/agents/{agent}/runs`,
//! `/v1/workspaces/{ws}/runs…` and `/v1/organizations/{org}/runs`, and the
//! usage they add up to, at `/v1/organizations/{org}/usage`. Console
//! members and API keys both read, start and cancel them; starting or
//! cancelling needs a developer or stricter. The Console doesn't start runs
//! in Production: the customer's product does that through the API.
//!
//! Approvals are decided in the customer's product and recorded by its
//! backend with an API key: a Console session only observes them. A
//! decision on the call the run waits for sets it running again and queues
//! it for the run engine.
//!
//! Starting, cancelling and deciding are each one transition of the run
//! (decision 0046), built by the reducer from the run as committed and
//! applied by the store with its events, audit row and job. A cancel or a
//! decision races the run's worker on the run's revision: when the worker
//! moves first, it's built again from the run as it is then.

use std::collections::BTreeMap;

use axum::Router;
use axum::extract::{Path, RawQuery, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::Response;
use axum::routing::{get, post};
use serde::Serialize;
use serde_json::{Value, json};
use uuid::Uuid;
use zupflow_core::agents::{AgentError, AgentStatus};
use zupflow_core::environment::Environment;
use zupflow_core::error::ApiError;
use zupflow_core::ids::{AgentId, OrganizationId, RunEventId, RunId, SessionId, ToolCallId, WorkspaceId};
use zupflow_core::organizations::MemberRole;
use zupflow_core::runs::reducer::{self, Approval, ControlError};
use zupflow_core::runs::state::{RequestId, Snapshot};
use zupflow_core::runs::store::{Applied, Receipt, StoreError};
use zupflow_core::runs::transition::TransitionRequest;
use zupflow_core::runs::{
    ApprovalDecision, Failure as RunFailure, MAX_INPUT, NewRun, OrganizationRun, OrganizationRunFilter, Page, Run,
    RunEvent, RunFilter, RunStatus, RunTrigger, RunUsage, day_of,
};
use zupflow_core::time::Timestamp;

use crate::auth::{Access, Actor, AppState, Caller, WorkspaceAccess};
use crate::http::{Failure, envelope, envelope_with_meta};
use crate::input::{Body, Fields};
use crate::run_observe;
use crate::workspaces::{clamped_int, environment_mismatch, invalid_cursor};

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/v1/organizations/{org}/runs", get(list_organization_runs))
        .route("/v1/organizations/{org}/usage", get(get_usage))
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
    /// Where a channel run came from. Always null: this server starts no channel runs yet.
    channel: Option<()>,
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
        channel: None,
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

fn run_failure(error: StoreError) -> Failure {
    match error {
        StoreError::NotFound => ApiError::not_found("Run").into(),
        StoreError::AgentNotFound => ApiError::not_found("Agent").into(),
        StoreError::InvalidCursor => invalid_cursor(),
        // The store has logged the cause.
        StoreError::Unavailable => unavailable(),
        // A request the store refused is a bug of ours, never the caller's.
        StoreError::Refused(why) => {
            tracing::error!(event = "runs.request_refused", why);
            ApiError::internal().into()
        }
        StoreError::Unsupported => {
            tracing::error!(event = "runs.run_unsupported");
            ApiError::internal().into()
        }
    }
}

fn not_waiting() -> Failure {
    ApiError::new(
        409,
        "not_waiting",
        "This run isn't waiting for a decision on that call. Read the run's events for the call it waits for.",
    )
    .into()
}

/// The workers kept moving the run while this request was handled.
fn run_changed() -> Failure {
    ApiError::new(
        409,
        "run_changed",
        "The run changed while this request was handled. Read it again, then retry.",
    )
    .into()
}

/// How many times a control request is built again after the run's worker
/// moved it first.
const CONTROL_ATTEMPTS: usize = 8;

/// Applies the control request `build` makes of the run as committed, built
/// again from the run as it is whenever another transition got there first.
/// `None` from `build` for a run this engine doesn't advance.
async fn control(
    state: &AppState,
    organization_id: OrganizationId,
    workspace_id: WorkspaceId,
    run_id: RunId,
    build: impl Fn(Option<&Snapshot>) -> Result<TransitionRequest, Failure>,
) -> Result<(TransitionRequest, Receipt), Failure> {
    for _ in 0..CONTROL_ATTEMPTS {
        let snapshot = match state.runs.find_snapshot(organization_id, workspace_id, run_id).await {
            Ok(Some(snapshot)) => Some(snapshot),
            Ok(None) => return Err(ApiError::not_found("Run").into()),
            Err(StoreError::Unsupported) => None,
            Err(error) => return Err(run_failure(error)),
        };
        let request = build(snapshot.as_ref())?;
        match state
            .runs
            .apply(organization_id, workspace_id, &request)
            .await
            .map_err(run_failure)?
        {
            Applied::Committed(receipt) => return Ok((request, receipt)),
            Applied::Conflict(_) => continue,
        }
    }
    Err(run_changed())
}

/// A fresh identity for one caller's control request.
fn request_id() -> RequestId {
    RequestId::from_uuid(Uuid::now_v7())
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
pub(crate) fn query(raw: Option<&str>) -> Result<BTreeMap<String, String>, Failure> {
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
    let max = i64::from(zupflow_core::runs::MAX_PAGE);
    u32::try_from(clamped_int(query.get("limit").map(String::as_str), 25, max)).unwrap_or(25)
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

/// The contract's `UsageDay`.
#[derive(Serialize, Default, Clone, Copy)]
struct UsageCounts {
    runs: u64,
    failed_runs: u64,
    tool_calls: u64,
    input_tokens: u64,
    output_tokens: u64,
    cache_read_input_tokens: u64,
    cache_write_input_tokens: u64,
}

impl UsageCounts {
    fn of(day: &zupflow_core::runs::UsageDay) -> Self {
        Self {
            runs: day.runs,
            failed_runs: day.failed_runs,
            tool_calls: day.tool_calls,
            input_tokens: day.input_tokens,
            output_tokens: day.output_tokens,
            cache_read_input_tokens: day.cache_read_input_tokens,
            cache_write_input_tokens: day.cache_write_input_tokens,
        }
    }

    fn add(&mut self, other: &Self) {
        self.runs = self.runs.saturating_add(other.runs);
        self.failed_runs = self.failed_runs.saturating_add(other.failed_runs);
        self.tool_calls = self.tool_calls.saturating_add(other.tool_calls);
        self.input_tokens = self.input_tokens.saturating_add(other.input_tokens);
        self.output_tokens = self.output_tokens.saturating_add(other.output_tokens);
        self.cache_read_input_tokens = self
            .cache_read_input_tokens
            .saturating_add(other.cache_read_input_tokens);
        self.cache_write_input_tokens = self
            .cache_write_input_tokens
            .saturating_add(other.cache_write_input_tokens);
    }
}

#[derive(Serialize)]
struct UsageDayView {
    date: String,
    #[serde(flatten)]
    counts: UsageCounts,
}

/// The contract's `UsageReport`.
#[derive(Serialize)]
struct UsageReport {
    environment: Environment,
    days: Vec<UsageDayView>,
    totals: UsageCounts,
}

/// The most days a report covers, and how many it covers by default.
const MAX_USAGE_DAYS: i64 = 90;
const USAGE_DAYS: i64 = 14;

/// Runs, failed runs, tool calls and tokens per UTC day in one environment: the last
/// `days` days, today included, each listed even when it had no runs. Every
/// member reads it; an API key, its own environment's only.
async fn get_usage(
    State(state): State<AppState>,
    caller: Caller,
    Path(org): Path<String>,
    RawQuery(raw): RawQuery,
) -> Result<Response, Failure> {
    let access = Access::organization(&state, caller, &org).await?;
    let query = query(raw.as_deref())?;
    let Some(environment) = query.get("environment").and_then(|value| Environment::parse(value)) else {
        return Err(Failure(ApiError {
            fields: Some(BTreeMap::from([("environment".to_owned(), "Required.".to_owned())])),
            ..ApiError::new(400, "validation_failed", "Choose an environment.")
        }));
    };
    if access.environment().is_some_and(|own| own != environment) {
        return Err(environment_mismatch());
    }
    let days = clamped_int(query.get("days").map(String::as_str), USAGE_DAYS, MAX_USAGE_DAYS);
    let today = day_of(Timestamp::now());
    let first = today - (days - 1);
    let counted = state
        .runs
        .usage(
            access.organization_id,
            environment,
            Timestamp::from_millis(first * 86_400_000),
        )
        .await
        .map_err(run_failure)?;
    let mut totals = UsageCounts::default();
    let mut list = Vec::new();
    for day in first..=today {
        let counts = counted
            .iter()
            .find(|counted| counted.day == day)
            .map_or_else(UsageCounts::default, UsageCounts::of);
        totals.add(&counts);
        let date = Timestamp::from_millis(day * 86_400_000).to_string();
        list.push(UsageDayView {
            date: date[..10].to_owned(),
            counts,
        });
    }
    Ok(envelope(
        StatusCode::OK,
        UsageReport {
            environment,
            days: list,
            totals,
        },
    ))
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
    // Resolved once, now: the agent's model, or else its environment's
    // default. The run keeps it even if either changes before it starts.
    let model = state
        .model_store
        .run_model(organization_id, workspace_id, agent.model_id)
        .await
        .map_err(|_| unavailable())?
        .ok_or_else(|| {
            Failure(ApiError::new(
                422,
                "model_not_configured",
                format!(
                    "{} has no model and {} has no default model. Choose a default model under Model providers, or a model for the agent.",
                    agent.name,
                    access.workspace.environment.name()
                ),
            ))
        })?;
    let run_id = RunId::generate();
    let request = reducer::admit(
        organization_id,
        workspace_id,
        run_id,
        &NewRun {
            agent_id: agent.id,
            session_id: SessionId::generate(),
            trigger,
            input,
            model,
            instructions: agent.instructions,
        },
    );
    let receipt = match state
        .runs
        .apply(organization_id, workspace_id, &request)
        .await
        .map_err(|error| match error {
            StoreError::AgentNotFound => not_found(),
            error => run_failure(error),
        })? {
        Applied::Committed(receipt) => receipt,
        // A new run's ID is fresh: nothing else starts from its revision 0.
        Applied::Conflict(_) => return Err(ApiError::internal().into()),
    };
    run_observe::committed(
        &state,
        organization_id,
        workspace_id,
        access.workspace.environment,
        &request,
        &receipt,
    )
    .await;
    // The run as it committed: never read again, so a failed read can't turn
    // a committed start into an error a retry would start again.
    Ok(envelope(StatusCode::CREATED, run_view(*receipt.run)))
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
    let source = match access.access.actor {
        Actor::Member(_) => RunTrigger::Console,
        Actor::ApiKey(_) => RunTrigger::Api,
    };
    let actor = access.access.audit_actor();
    let (organization_id, workspace_id) = (access.access.organization_id, access.workspace.id);
    let (request, receipt) = control(&state, organization_id, workspace_id, run.id, |snapshot| {
        // Only a run this engine never advances has no snapshot, and every
        // such run is finished (migration 0029).
        let Some(snapshot) = snapshot else {
            return Err(finished());
        };
        reducer::cancel(
            organization_id,
            workspace_id,
            snapshot,
            request_id(),
            source,
            actor.clone(),
        )
        .map_err(|error| match error {
            ControlError::Finished => finished(),
            ControlError::NotFound => ApiError::not_found("Run").into(),
            error => {
                tracing::error!(event = "runs.cancel_unplanned", %error);
                ApiError::internal().into()
            }
        })
    })
    .await?;
    run_observe::committed(
        &state,
        organization_id,
        workspace_id,
        access.workspace.environment,
        &request,
        &receipt,
    )
    .await;
    Ok(envelope(StatusCode::OK, run_view(*receipt.run)))
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
    let (api_key_id, api_key_name) = match &access.access.actor {
        Actor::ApiKey(key) => (key.id, key.name.clone()),
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
    let (organization_id, workspace_id) = (access.access.organization_id, access.workspace.id);
    let (request, receipt) = control(&state, organization_id, workspace_id, run.id, |snapshot| {
        let Some(snapshot) = snapshot else {
            return Err(not_waiting());
        };
        reducer::resolve_approval(
            organization_id,
            workspace_id,
            snapshot,
            request_id(),
            Approval {
                call_id,
                decision,
                api_key_id,
                api_key_name: &api_key_name,
                reason: reason.as_deref(),
            },
        )
        .map_err(|error| match error {
            ControlError::NotWaiting | ControlError::Finished | ControlError::UnsupportedVersion(_) => not_waiting(),
            ControlError::NotFound => ApiError::not_found("Run").into(),
            error => {
                tracing::error!(event = "runs.approval_unplanned", %error);
                ApiError::internal().into()
            }
        })
    })
    .await?;
    run_observe::committed(
        &state,
        organization_id,
        workspace_id,
        access.workspace.environment,
        &request,
        &receipt,
    )
    .await;
    Ok(envelope(StatusCode::OK, run_view(*receipt.run)))
}
