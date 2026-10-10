//! [`SqliteRunStore`]: runs in `runs`, their timelines in `run_events` and
//! their tool calls in `run_tool_calls`. Every statement repeats
//! `organization_id` and `workspace_id` beside the run's ID; the
//! organization-wide list joins `workspaces` on the organization too.
//!
//! Each step is one immediate transaction that reads the run's state, checks
//! it, writes, and appends the step's events after the run's last sequence,
//! so two workers holding the same run (a lease that ran out) can't both take
//! a step: the second finds the run moved on. A run's job is queued in the
//! transaction that needs it, through the queue's own table, and so is the
//! announcement of every event a webhook names (the outbox, decision 0017).

use async_trait::async_trait;
use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use serde_json::{Map, Value};
use sqlx::sqlite::SqliteRow;
use sqlx::{QueryBuilder, Row, Sqlite, Transaction};
use uuid::Uuid;
use zupflow_core::environment::Environment;
use zupflow_core::ids::{
    AgentId, ApiKeyId, InstallationId, OrganizationId, RunEventId, RunId, SessionId, ToolCallId, WorkspaceId,
};
use zupflow_core::jobs::JobId;
use zupflow_core::runs::{
    AgentActivity, ApprovalDecision, CallOutcome, CallState, CallTarget, Ending, Failure, NewRun, NewRunEvent, NewTurn,
    OrganizationRun, OrganizationRunFilter, Page, QueuedJob, RecordedDecision, Run, RunError, RunEvent, RunEventType,
    RunFilter, RunProgress, RunStatus, RunStore, RunToolCall, RunTrigger, RunTurn, RunUsage, UsageDay,
};
use zupflow_core::scope::Scope;
use zupflow_core::time::Timestamp;
use zupflow_core::webhooks::Announcement;

use crate::Database;
use crate::job_queue::insert_job;

pub struct SqliteRunStore {
    database: Database,
}

impl SqliteRunStore {
    pub fn new(database: Database) -> Self {
        Self { database }
    }

    async fn immediate(&self, operation: &'static str) -> Result<Transaction<'static, Sqlite>, RunError> {
        self.database
            .pool()
            .begin_with("BEGIN IMMEDIATE")
            .await
            .map_err(failed(operation))
    }
}

fn failed(operation: &'static str) -> impl FnOnce(sqlx::Error) -> RunError {
    move |error| {
        tracing::error!(event = "runs.database_error", operation, %error);
        RunError::Unavailable
    }
}

fn read(error: impl std::fmt::Display) -> RunError {
    tracing::error!(event = "runs.unreadable_row", %error);
    RunError::Unavailable
}

/// A run's columns and its agent's name, from `runs r`.
macro_rules! run_columns {
    () => {
        "r.id, r.organization_id, r.workspace_id, r.agent_id, a.name AS agent_name, r.session_id, r.trigger, \
         r.status, r.input, r.output, r.input_tokens, r.output_tokens, r.tool_calls, r.error_code, r.error_message, \
         r.created_at, r.started_at, r.finished_at"
    };
}

/// The agent a run names, in the run's own workspace; none once it's deleted.
const AGENT_JOIN: &str = " LEFT JOIN agents a ON a.organization_id = r.organization_id \
                          AND a.workspace_id = r.workspace_id AND a.id = r.agent_id";

const FIND_RUN: &str = concat!(
    "SELECT ",
    run_columns!(),
    ", r.model, r.instructions, r.turns FROM runs r LEFT JOIN agents a ON a.organization_id = r.organization_id \
     AND a.workspace_id = r.workspace_id AND a.id = r.agent_id \
     WHERE r.organization_id = ? AND r.workspace_id = ? AND r.id = ?"
);

const RUN_STATE: &str = "SELECT status, turns FROM runs WHERE organization_id = ? AND workspace_id = ? AND id = ?";

macro_rules! call_columns {
    () => {
        "id, turn, position, model_call_id, name, installation_id, installation_label, app_name, tool, arguments, \
         state, verdict, policy, mode, rule, reason, approval_requested, approval, approval_api_key_id, outcome, \
         outcome_is_error, started_at, finished_at"
    };
}

const LIST_CALLS: &str = concat!(
    "SELECT ",
    call_columns!(),
    " FROM run_tool_calls WHERE organization_id = ? AND workspace_id = ? AND run_id = ? ORDER BY turn, position"
);

const FIND_CALL: &str = concat!(
    "SELECT ",
    call_columns!(),
    " FROM run_tool_calls WHERE organization_id = ? AND workspace_id = ? AND run_id = ? AND id = ?"
);

fn millis_opt(row: &SqliteRow, column: &str) -> Result<Option<Timestamp>, RunError> {
    Ok(row
        .try_get::<Option<i64>, _>(column)
        .map_err(read)?
        .map(Timestamp::from_millis))
}

fn count(row: &SqliteRow, column: &str) -> Result<u64, RunError> {
    u64::try_from(row.try_get::<i64, _>(column).map_err(read)?).map_err(read)
}

fn run_from(row: &SqliteRow) -> Result<Run, RunError> {
    let trigger: String = row.try_get("trigger").map_err(read)?;
    let status: String = row.try_get("status").map_err(read)?;
    let error_code: Option<String> = row.try_get("error_code").map_err(read)?;
    let error_message: Option<String> = row.try_get("error_message").map_err(read)?;
    Ok(Run {
        id: RunId::from_uuid(row.try_get("id").map_err(read)?),
        organization_id: OrganizationId::from_uuid(row.try_get("organization_id").map_err(read)?),
        workspace_id: WorkspaceId::from_uuid(row.try_get("workspace_id").map_err(read)?),
        agent_id: AgentId::from_uuid(row.try_get("agent_id").map_err(read)?),
        agent_name: row.try_get("agent_name").map_err(read)?,
        session_id: SessionId::from_uuid(row.try_get("session_id").map_err(read)?),
        trigger: RunTrigger::parse(&trigger).ok_or_else(|| read("an unknown trigger"))?,
        status: RunStatus::parse(&status).ok_or_else(|| read("an unknown run status"))?,
        input: row.try_get("input").map_err(read)?,
        output: row.try_get("output").map_err(read)?,
        usage: RunUsage {
            input_tokens: count(row, "input_tokens")?,
            output_tokens: count(row, "output_tokens")?,
            tool_calls: count(row, "tool_calls")?,
        },
        error: match (error_code, error_message) {
            (Some(code), Some(message)) => Some(Failure { code, message }),
            _ => None,
        },
        created_at: Timestamp::from_millis(row.try_get("created_at").map_err(read)?),
        started_at: millis_opt(row, "started_at")?,
        finished_at: millis_opt(row, "finished_at")?,
    })
}

fn event_from(row: &SqliteRow) -> Result<RunEvent, RunError> {
    let kind: String = row.try_get("type").map_err(read)?;
    let data: String = row.try_get("data").map_err(read)?;
    Ok(RunEvent {
        id: RunEventId::from_uuid(row.try_get("id").map_err(read)?),
        organization_id: OrganizationId::from_uuid(row.try_get("organization_id").map_err(read)?),
        workspace_id: WorkspaceId::from_uuid(row.try_get("workspace_id").map_err(read)?),
        run_id: RunId::from_uuid(row.try_get("run_id").map_err(read)?),
        sequence: count(row, "sequence")?,
        kind: RunEventType::parse(&kind).ok_or_else(|| read("an unknown event type"))?,
        data: serde_json::from_str(&data).map_err(read)?,
        created_at: Timestamp::from_millis(row.try_get("created_at").map_err(read)?),
    })
}

fn call_from(row: &SqliteRow) -> Result<RunToolCall, RunError> {
    let text = |column: &str| row.try_get::<Option<String>, _>(column).map_err(read);
    let installation: Option<Uuid> = row.try_get("installation_id").map_err(read)?;
    let target = match (
        installation,
        text("installation_label")?,
        text("app_name")?,
        text("tool")?,
    ) {
        (Some(installation_id), Some(installation_label), Some(app_name), Some(tool)) => Some(CallTarget {
            installation_id: InstallationId::from_uuid(installation_id),
            installation_label,
            app_name,
            tool,
        }),
        _ => None,
    };
    let decision = match (text("verdict")?, text("policy")?, text("rule")?) {
        (Some(verdict), Some(policy), Some(rule)) => Some(RecordedDecision {
            verdict,
            policy,
            mode: text("mode")?,
            rule,
            reason: text("reason")?,
        }),
        _ => None,
    };
    let approval_key: Option<Uuid> = row.try_get("approval_api_key_id").map_err(read)?;
    let approval = match (text("approval")?, approval_key) {
        (Some(decision), Some(key)) => Some((
            ApprovalDecision::parse(&decision).ok_or_else(|| read("an unknown approval"))?,
            ApiKeyId::from_uuid(key),
        )),
        _ => None,
    };
    let outcome = match (
        text("outcome")?,
        row.try_get::<Option<i64>, _>("outcome_is_error").map_err(read)?,
    ) {
        (Some(content), Some(is_error)) => Some(CallOutcome {
            content,
            is_error: is_error == 1,
        }),
        _ => None,
    };
    let arguments: String = row.try_get("arguments").map_err(read)?;
    let arguments: Map<String, Value> = serde_json::from_str(&arguments).map_err(read)?;
    let state: String = row.try_get("state").map_err(read)?;
    Ok(RunToolCall {
        id: ToolCallId::from_uuid(row.try_get("id").map_err(read)?),
        turn: u32::try_from(row.try_get::<i64, _>("turn").map_err(read)?).map_err(read)?,
        position: u32::try_from(row.try_get::<i64, _>("position").map_err(read)?).map_err(read)?,
        model_call_id: row.try_get("model_call_id").map_err(read)?,
        name: row.try_get("name").map_err(read)?,
        target,
        arguments,
        state: CallState::parse(&state).ok_or_else(|| read("an unknown call state"))?,
        decision,
        approval_requested: row.try_get::<i64, _>("approval_requested").map_err(read)? == 1,
        approval,
        outcome,
        started_at: millis_opt(row, "started_at")?,
        finished_at: millis_opt(row, "finished_at")?,
    })
}

/// A position in a list: newest first, then by ID.
struct Position {
    created_at: i64,
    id: Uuid,
}

impl Position {
    /// The run's prefixed ID, never its bare UUID: a cursor leaves the server.
    fn encode(&self) -> String {
        URL_SAFE_NO_PAD.encode(format!("{}\n{}", self.created_at, RunId::from_uuid(self.id)))
    }

    fn decode(cursor: &str) -> Option<Self> {
        let raw = String::from_utf8(URL_SAFE_NO_PAD.decode(cursor).ok()?).ok()?;
        let (created_at, id) = raw.split_once('\n')?;
        Some(Self {
            created_at: created_at.parse().ok()?,
            id: RunId::parse(id).ok()?.uuid(),
        })
    }
}

fn after(cursor: Option<&str>) -> Result<Option<Position>, RunError> {
    cursor
        .map(|cursor| Position::decode(cursor).ok_or(RunError::InvalidCursor))
        .transpose()
}

fn push_after(query: &mut QueryBuilder<Sqlite>, after: &Option<Position>) {
    if let Some(after) = after {
        query.push(" AND (r.created_at < ");
        query.push_bind(after.created_at);
        query.push(" OR (r.created_at = ");
        query.push_bind(after.created_at);
        query.push(" AND r.id < ");
        query.push_bind(after.id);
        query.push("))");
    }
}

/// Keeps the page to `limit`, and the next page's cursor when more follow.
fn paged<T>(mut items: Vec<T>, limit: u32, position: impl Fn(&T) -> Position) -> (Vec<T>, Option<String>) {
    let limit = limit as usize;
    if items.len() > limit {
        items.truncate(limit);
        let next = items.last().map(|last| position(last).encode());
        (items, next)
    } else {
        (items, None)
    }
}

/// The run's status and turns, read in the step's transaction.
async fn state_of(
    transaction: &mut Transaction<'static, Sqlite>,
    organization_id: OrganizationId,
    workspace_id: WorkspaceId,
    run_id: RunId,
) -> Result<Option<(RunStatus, Vec<RunTurn>)>, RunError> {
    let row = sqlx::query(RUN_STATE)
        .bind(organization_id.uuid())
        .bind(workspace_id.uuid())
        .bind(run_id.uuid())
        .fetch_optional(&mut **transaction)
        .await
        .map_err(failed("state_of"))?;
    let Some(row) = row else { return Ok(None) };
    let status: String = row.try_get("status").map_err(read)?;
    let turns: String = row.try_get("turns").map_err(read)?;
    Ok(Some((
        RunStatus::parse(&status).ok_or_else(|| read("an unknown run status"))?,
        serde_json::from_str(&turns).map_err(read)?,
    )))
}

async fn call_in(
    transaction: &mut Transaction<'static, Sqlite>,
    organization_id: OrganizationId,
    workspace_id: WorkspaceId,
    run_id: RunId,
    call_id: ToolCallId,
) -> Result<Option<RunToolCall>, RunError> {
    let row = sqlx::query(FIND_CALL)
        .bind(organization_id.uuid())
        .bind(workspace_id.uuid())
        .bind(run_id.uuid())
        .bind(call_id.uuid())
        .fetch_optional(&mut **transaction)
        .await
        .map_err(failed("call_in"))?;
    row.as_ref().map(call_from).transpose()
}

/// Appends events after the run's last, in order, and answers them.
async fn append(
    transaction: &mut Transaction<'static, Sqlite>,
    organization_id: OrganizationId,
    workspace_id: WorkspaceId,
    run_id: RunId,
    events: &[NewRunEvent],
    now: Timestamp,
) -> Result<Vec<RunEvent>, RunError> {
    let last: i64 = sqlx::query_scalar(
        "SELECT coalesce(max(sequence), 0) FROM run_events WHERE organization_id = ? AND workspace_id = ? AND run_id = ?",
    )
    .bind(organization_id.uuid())
    .bind(workspace_id.uuid())
    .bind(run_id.uuid())
    .fetch_one(&mut **transaction)
    .await
    .map_err(failed("append"))?;
    let mut appended = Vec::with_capacity(events.len());
    for (offset, event) in events.iter().enumerate() {
        let sequence = last + 1 + i64::try_from(offset).map_err(read)?;
        let id = RunEventId::generate();
        let data = if event.data.is_object() {
            event.data.clone()
        } else {
            Value::Object(Map::new())
        };
        sqlx::query(
            "INSERT INTO run_events (id, organization_id, workspace_id, run_id, sequence, type, data, created_at) \
             VALUES (?, ?, ?, ?, ?, ?, ?, ?)",
        )
        .bind(id.uuid())
        .bind(organization_id.uuid())
        .bind(workspace_id.uuid())
        .bind(run_id.uuid())
        .bind(sequence)
        .bind(event.kind.as_str())
        .bind(data.to_string())
        .bind(now.millis())
        .execute(&mut **transaction)
        .await
        .map_err(failed("append"))?;
        // The outbox: a webhook this event causes is announced with it.
        if let Some(announcement) = Announcement::of_run_event(run_id, event.kind, now) {
            let Some(job) = announcement.job() else {
                return Err(RunError::Unavailable);
            };
            queue(transaction, &Scope::Workspace(organization_id, workspace_id), &job).await?;
        }
        appended.push(RunEvent {
            id,
            organization_id,
            workspace_id,
            run_id,
            sequence: u64::try_from(sequence).map_err(read)?,
            kind: event.kind,
            data,
            created_at: now,
        });
    }
    Ok(appended)
}

async fn find_in(
    transaction: &mut Transaction<'static, Sqlite>,
    organization_id: OrganizationId,
    workspace_id: WorkspaceId,
    run_id: RunId,
) -> Result<Run, RunError> {
    let row = sqlx::query(FIND_RUN)
        .bind(organization_id.uuid())
        .bind(workspace_id.uuid())
        .bind(run_id.uuid())
        .fetch_optional(&mut **transaction)
        .await
        .map_err(failed("find_in"))?
        .ok_or(RunError::NotFound)?;
    run_from(&row)
}

async fn queue(
    transaction: &mut Transaction<'static, Sqlite>,
    scope: &Scope,
    job: &QueuedJob,
) -> Result<JobId, RunError> {
    insert_job(&mut **transaction, scope, &job.kind, job.payload.clone(), job.options)
        .await
        .map_err(|error| {
            tracing::error!(event = "runs.job_not_queued", %error);
            RunError::Unavailable
        })
}

/// Sets a run completed or failed.
async fn end(
    transaction: &mut Transaction<'static, Sqlite>,
    organization_id: OrganizationId,
    workspace_id: WorkspaceId,
    run_id: RunId,
    ending: &Ending,
    now: Timestamp,
) -> Result<(), RunError> {
    let (status, output, error) = match ending {
        Ending::Completed { output } => (RunStatus::Completed, Some(output.as_str()), None),
        Ending::Failed(failure) => (RunStatus::Failed, None, Some(failure)),
    };
    sqlx::query(
        "UPDATE runs SET status = ?, output = ?, error_code = ?, error_message = ?, finished_at = ? \
         WHERE organization_id = ? AND workspace_id = ? AND id = ?",
    )
    .bind(status.as_str())
    .bind(output)
    .bind(error.map(|error| error.code.as_str()))
    .bind(error.map(|error| error.message.as_str()))
    .bind(now.millis())
    .bind(organization_id.uuid())
    .bind(workspace_id.uuid())
    .bind(run_id.uuid())
    .execute(&mut **transaction)
    .await
    .map_err(failed("end"))?;
    Ok(())
}

fn verdict_of(decision: &RecordedDecision) -> &str {
    decision.verdict.as_str()
}

#[async_trait]
impl RunStore for SqliteRunStore {
    async fn create_run(
        &self,
        organization_id: OrganizationId,
        workspace_id: WorkspaceId,
        run_id: RunId,
        run: &NewRun,
        events: &[NewRunEvent],
        job: &QueuedJob,
        now: Timestamp,
    ) -> Result<(Run, Vec<RunEvent>), RunError> {
        let mut transaction = self.immediate("create_run").await?;
        let agent = sqlx::query("SELECT 1 FROM agents WHERE organization_id = ? AND workspace_id = ? AND id = ?")
            .bind(organization_id.uuid())
            .bind(workspace_id.uuid())
            .bind(run.agent_id.uuid())
            .fetch_optional(&mut *transaction)
            .await
            .map_err(failed("create_run"))?;
        if agent.is_none() {
            return Err(RunError::AgentNotFound);
        }
        let job_id = queue(&mut transaction, &Scope::Workspace(organization_id, workspace_id), job).await?;
        sqlx::query(
            "INSERT INTO runs (id, organization_id, workspace_id, agent_id, session_id, trigger, job_id, status, \
             input, model, instructions, turns, input_tokens, output_tokens, tool_calls, created_at) \
             VALUES (?, ?, ?, ?, ?, ?, ?, 'queued', ?, ?, ?, '[]', 0, 0, 0, ?)",
        )
        .bind(run_id.uuid())
        .bind(organization_id.uuid())
        .bind(workspace_id.uuid())
        .bind(run.agent_id.uuid())
        .bind(run.session_id.uuid())
        .bind(run.trigger.as_str())
        .bind(job_id.uuid())
        .bind(&run.input)
        .bind(&run.model)
        .bind(&run.instructions)
        .bind(now.millis())
        .execute(&mut *transaction)
        .await
        .map_err(failed("create_run"))?;
        let appended = append(&mut transaction, organization_id, workspace_id, run_id, events, now).await?;
        let created = find_in(&mut transaction, organization_id, workspace_id, run_id).await?;
        transaction.commit().await.map_err(failed("create_run"))?;
        Ok((created, appended))
    }

    async fn find_run(
        &self,
        organization_id: OrganizationId,
        workspace_id: WorkspaceId,
        run_id: RunId,
    ) -> Result<Option<Run>, RunError> {
        let row = sqlx::query(FIND_RUN)
            .bind(organization_id.uuid())
            .bind(workspace_id.uuid())
            .bind(run_id.uuid())
            .fetch_optional(self.database.pool())
            .await
            .map_err(failed("find_run"))?;
        row.as_ref().map(run_from).transpose()
    }

    async fn list_runs(
        &self,
        organization_id: OrganizationId,
        workspace_id: WorkspaceId,
        filter: &RunFilter,
        cursor: Option<&str>,
        limit: u32,
    ) -> Result<Page<Run>, RunError> {
        let after = after(cursor)?;
        let limit = limit.clamp(1, zupflow_core::runs::MAX_PAGE);
        let push_filter = |query: &mut QueryBuilder<Sqlite>| {
            query.push(" WHERE r.organization_id = ");
            query.push_bind(organization_id.uuid());
            query.push(" AND r.workspace_id = ");
            query.push_bind(workspace_id.uuid());
            if let Some(agent_id) = filter.agent_id {
                query.push(" AND r.agent_id = ");
                query.push_bind(agent_id.uuid());
            }
            if let Some(status) = filter.status {
                query.push(" AND r.status = ");
                query.push_bind(status.as_str());
            }
        };
        let mut transaction = self.database.pool().begin().await.map_err(failed("list_runs"))?;
        let mut total = QueryBuilder::<Sqlite>::new("SELECT count(*) FROM runs r");
        push_filter(&mut total);
        let total: i64 = total
            .build_query_scalar()
            .fetch_one(&mut *transaction)
            .await
            .map_err(failed("list_runs"))?;
        let mut query = QueryBuilder::<Sqlite>::new(concat!("SELECT ", run_columns!(), " FROM runs r"));
        query.push(AGENT_JOIN);
        push_filter(&mut query);
        push_after(&mut query, &after);
        query.push(" ORDER BY r.created_at DESC, r.id DESC LIMIT ");
        query.push_bind(i64::from(limit) + 1);
        let rows = query
            .build()
            .fetch_all(&mut *transaction)
            .await
            .map_err(failed("list_runs"))?;
        transaction.commit().await.map_err(failed("list_runs"))?;
        let runs = rows.iter().map(run_from).collect::<Result<Vec<_>, _>>()?;
        let (items, next_cursor) = paged(runs, limit, |run| Position {
            created_at: run.created_at.millis(),
            id: run.id.uuid(),
        });
        Ok(Page {
            items,
            next_cursor,
            total: u64::try_from(total).map_err(read)?,
        })
    }

    async fn list_organization_runs(
        &self,
        organization_id: OrganizationId,
        filter: &OrganizationRunFilter,
        cursor: Option<&str>,
        limit: u32,
    ) -> Result<Page<OrganizationRun>, RunError> {
        let after = after(cursor)?;
        let limit = limit.clamp(1, zupflow_core::runs::MAX_PAGE);
        let push_filter = |query: &mut QueryBuilder<Sqlite>| {
            query.push(
                " JOIN workspaces w ON w.organization_id = r.organization_id AND w.id = r.workspace_id \
                 WHERE r.organization_id = ",
            );
            query.push_bind(organization_id.uuid());
            if let Some(environment) = filter.environment {
                query.push(" AND w.environment = ");
                query.push_bind(environment.as_str());
            }
            if let Some(workspace_id) = filter.workspace_id {
                query.push(" AND r.workspace_id = ");
                query.push_bind(workspace_id.uuid());
            }
            if let Some(status) = filter.status {
                query.push(" AND r.status = ");
                query.push_bind(status.as_str());
            }
        };
        let mut transaction = self
            .database
            .pool()
            .begin()
            .await
            .map_err(failed("list_organization_runs"))?;
        let mut total = QueryBuilder::<Sqlite>::new("SELECT count(*) FROM runs r");
        push_filter(&mut total);
        let total: i64 = total
            .build_query_scalar()
            .fetch_one(&mut *transaction)
            .await
            .map_err(failed("list_organization_runs"))?;
        let mut query = QueryBuilder::<Sqlite>::new(concat!(
            "SELECT ",
            run_columns!(),
            ", w.name AS workspace_name, w.slug AS workspace_slug, w.environment FROM runs r",
        ));
        query.push(AGENT_JOIN);
        push_filter(&mut query);
        push_after(&mut query, &after);
        query.push(" ORDER BY r.created_at DESC, r.id DESC LIMIT ");
        query.push_bind(i64::from(limit) + 1);
        let rows = query
            .build()
            .fetch_all(&mut *transaction)
            .await
            .map_err(failed("list_organization_runs"))?;
        transaction.commit().await.map_err(failed("list_organization_runs"))?;
        let mut runs = Vec::with_capacity(rows.len());
        for row in &rows {
            let environment: String = row.try_get("environment").map_err(read)?;
            runs.push(OrganizationRun {
                run: run_from(row)?,
                workspace_name: row.try_get("workspace_name").map_err(read)?,
                workspace_slug: row.try_get("workspace_slug").map_err(read)?,
                environment: Environment::parse(&environment).ok_or_else(|| read("an unknown environment"))?,
            });
        }
        let (items, next_cursor) = paged(runs, limit, |item| Position {
            created_at: item.run.created_at.millis(),
            id: item.run.id.uuid(),
        });
        Ok(Page {
            items,
            next_cursor,
            total: u64::try_from(total).map_err(read)?,
        })
    }

    async fn list_events(
        &self,
        organization_id: OrganizationId,
        workspace_id: WorkspaceId,
        run_id: RunId,
        after: u64,
    ) -> Result<Vec<RunEvent>, RunError> {
        let mut transaction = self.database.pool().begin().await.map_err(failed("list_events"))?;
        if state_of(&mut transaction, organization_id, workspace_id, run_id)
            .await?
            .is_none()
        {
            return Err(RunError::NotFound);
        }
        let rows = sqlx::query(
            "SELECT id, organization_id, workspace_id, run_id, sequence, type, data, created_at FROM run_events \
             WHERE organization_id = ? AND workspace_id = ? AND run_id = ? AND sequence > ? ORDER BY sequence",
        )
        .bind(organization_id.uuid())
        .bind(workspace_id.uuid())
        .bind(run_id.uuid())
        .bind(i64::try_from(after).unwrap_or(i64::MAX))
        .fetch_all(&mut *transaction)
        .await
        .map_err(failed("list_events"))?;
        transaction.commit().await.map_err(failed("list_events"))?;
        rows.iter().map(event_from).collect()
    }

    async fn agent_activity(
        &self,
        organization_id: OrganizationId,
        workspace_id: WorkspaceId,
        since: Timestamp,
    ) -> Result<Vec<AgentActivity>, RunError> {
        let rows = sqlx::query(
            "SELECT agent_id, sum(created_at >= ?) AS runs_since, max(created_at) AS last_run_at FROM runs \
             WHERE organization_id = ? AND workspace_id = ? GROUP BY agent_id",
        )
        .bind(since.millis())
        .bind(organization_id.uuid())
        .bind(workspace_id.uuid())
        .fetch_all(self.database.pool())
        .await
        .map_err(failed("agent_activity"))?;
        rows.iter()
            .map(|row| {
                Ok(AgentActivity {
                    agent_id: AgentId::from_uuid(row.try_get("agent_id").map_err(read)?),
                    runs_since: count(row, "runs_since")?,
                    last_run_at: millis_opt(row, "last_run_at")?,
                })
            })
            .collect()
    }

    async fn usage(
        &self,
        organization_id: OrganizationId,
        environment: Environment,
        since: Timestamp,
    ) -> Result<Vec<UsageDay>, RunError> {
        let rows = sqlx::query(
            "SELECT r.created_at / 86400000 AS day, count(*) AS runs, sum(r.status = 'failed') AS failed_runs, \
             sum(r.tool_calls) AS tool_calls FROM runs r \
             JOIN workspaces w ON w.organization_id = r.organization_id AND w.id = r.workspace_id \
             WHERE r.organization_id = ? AND w.organization_id = ? AND w.environment = ? AND r.created_at >= ? \
             GROUP BY day ORDER BY day",
        )
        .bind(organization_id.uuid())
        .bind(organization_id.uuid())
        .bind(environment.as_str())
        .bind(since.millis().max(0))
        .fetch_all(self.database.pool())
        .await
        .map_err(failed("usage"))?;
        rows.iter()
            .map(|row| {
                Ok(UsageDay {
                    day: row.try_get("day").map_err(read)?,
                    runs: count(row, "runs")?,
                    failed_runs: count(row, "failed_runs")?,
                    tool_calls: count(row, "tool_calls")?,
                })
            })
            .collect()
    }

    async fn find_progress(
        &self,
        organization_id: OrganizationId,
        workspace_id: WorkspaceId,
        run_id: RunId,
    ) -> Result<Option<RunProgress>, RunError> {
        let mut transaction = self.database.pool().begin().await.map_err(failed("find_progress"))?;
        let row = sqlx::query(FIND_RUN)
            .bind(organization_id.uuid())
            .bind(workspace_id.uuid())
            .bind(run_id.uuid())
            .fetch_optional(&mut *transaction)
            .await
            .map_err(failed("find_progress"))?;
        let Some(row) = row else {
            return Ok(None);
        };
        let calls = sqlx::query(LIST_CALLS)
            .bind(organization_id.uuid())
            .bind(workspace_id.uuid())
            .bind(run_id.uuid())
            .fetch_all(&mut *transaction)
            .await
            .map_err(failed("find_progress"))?;
        transaction.commit().await.map_err(failed("find_progress"))?;
        let turns: String = row.try_get("turns").map_err(read)?;
        Ok(Some(RunProgress {
            run: run_from(&row)?,
            model: row.try_get("model").map_err(read)?,
            instructions: row.try_get("instructions").map_err(read)?,
            turns: serde_json::from_str(&turns).map_err(read)?,
            calls: calls.iter().map(call_from).collect::<Result<_, _>>()?,
        }))
    }

    async fn start_run(
        &self,
        organization_id: OrganizationId,
        workspace_id: WorkspaceId,
        run_id: RunId,
        events: &[NewRunEvent],
        now: Timestamp,
    ) -> Result<Vec<RunEvent>, RunError> {
        let mut transaction = self.immediate("start_run").await?;
        match state_of(&mut transaction, organization_id, workspace_id, run_id).await? {
            None => return Err(RunError::NotFound),
            Some((RunStatus::Queued, _)) => {}
            Some(_) => return Err(RunError::Moved),
        }
        sqlx::query(
            "UPDATE runs SET status = 'running', started_at = ? WHERE organization_id = ? AND workspace_id = ? AND id = ?",
        )
        .bind(now.millis())
        .bind(organization_id.uuid())
        .bind(workspace_id.uuid())
        .bind(run_id.uuid())
        .execute(&mut *transaction)
        .await
        .map_err(failed("start_run"))?;
        let appended = append(&mut transaction, organization_id, workspace_id, run_id, events, now).await?;
        transaction.commit().await.map_err(failed("start_run"))?;
        Ok(appended)
    }

    async fn record_turn(
        &self,
        organization_id: OrganizationId,
        workspace_id: WorkspaceId,
        run_id: RunId,
        index: u32,
        turn: &NewTurn,
        events: &[NewRunEvent],
        now: Timestamp,
    ) -> Result<Vec<RunEvent>, RunError> {
        let mut transaction = self.immediate("record_turn").await?;
        let mut turns = match state_of(&mut transaction, organization_id, workspace_id, run_id).await? {
            None => return Err(RunError::NotFound),
            Some((RunStatus::Running, turns)) if turns.len() == index as usize => turns,
            Some(_) => return Err(RunError::Moved),
        };
        for (position, call) in turn.calls.iter().enumerate() {
            let target = call.target.as_ref();
            let decision = call.decision.as_ref();
            let settled = call.state.is_settled().then_some(now.millis());
            sqlx::query(
                "INSERT INTO run_tool_calls (id, organization_id, workspace_id, run_id, turn, position, model_call_id, \
                 name, installation_id, installation_label, app_name, tool, arguments, state, verdict, policy, mode, \
                 rule, reason, approval_requested, outcome, outcome_is_error, finished_at) \
                 VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, 0, ?, ?, ?)",
            )
            .bind(call.id.uuid())
            .bind(organization_id.uuid())
            .bind(workspace_id.uuid())
            .bind(run_id.uuid())
            .bind(i64::from(index))
            .bind(i64::try_from(position).map_err(read)?)
            .bind(&call.model_call_id)
            .bind(&call.name)
            .bind(target.map(|target| target.installation_id.uuid()))
            .bind(target.map(|target| target.installation_label.as_str()))
            .bind(target.map(|target| target.app_name.as_str()))
            .bind(target.map(|target| target.tool.as_str()))
            .bind(Value::Object(call.arguments.clone()).to_string())
            .bind(call.state.as_str())
            .bind(decision.map(verdict_of))
            .bind(decision.map(|decision| decision.policy.as_str()))
            .bind(decision.and_then(|decision| decision.mode.as_deref()))
            .bind(decision.map(|decision| decision.rule.as_str()))
            .bind(decision.and_then(|decision| decision.reason.as_deref()))
            .bind(call.outcome.as_ref().map(|outcome| outcome.content.as_str()))
            .bind(call.outcome.as_ref().map(|outcome| i64::from(outcome.is_error)))
            .bind(settled)
            .execute(&mut *transaction)
            .await
            .map_err(failed("record_turn"))?;
        }
        turns.push(RunTurn {
            text: turn.text.clone(),
            calls: turn.calls.iter().map(|call| call.id).collect(),
        });
        let turns = serde_json::to_string(&turns).map_err(read)?;
        sqlx::query(
            "UPDATE runs SET turns = ?, input_tokens = input_tokens + ?, output_tokens = output_tokens + ? \
             WHERE organization_id = ? AND workspace_id = ? AND id = ?",
        )
        .bind(turns)
        .bind(i64::try_from(turn.usage.input_tokens).unwrap_or(i64::MAX))
        .bind(i64::try_from(turn.usage.output_tokens).unwrap_or(i64::MAX))
        .bind(organization_id.uuid())
        .bind(workspace_id.uuid())
        .bind(run_id.uuid())
        .execute(&mut *transaction)
        .await
        .map_err(failed("record_turn"))?;
        if let Some(ending) = &turn.finish {
            end(&mut transaction, organization_id, workspace_id, run_id, ending, now).await?;
        }
        let appended = append(&mut transaction, organization_id, workspace_id, run_id, events, now).await?;
        transaction.commit().await.map_err(failed("record_turn"))?;
        Ok(appended)
    }

    async fn request_approval(
        &self,
        organization_id: OrganizationId,
        workspace_id: WorkspaceId,
        run_id: RunId,
        call_id: ToolCallId,
        events: &[NewRunEvent],
        now: Timestamp,
    ) -> Result<Vec<RunEvent>, RunError> {
        let mut transaction = self.immediate("request_approval").await?;
        match state_of(&mut transaction, organization_id, workspace_id, run_id).await? {
            None => return Err(RunError::NotFound),
            Some((RunStatus::Running, _)) => {}
            Some(_) => return Err(RunError::Moved),
        }
        let call = call_in(&mut transaction, organization_id, workspace_id, run_id, call_id).await?;
        if call.is_none_or(|call| call.state != CallState::AwaitingApproval) {
            return Err(RunError::Moved);
        }
        sqlx::query(
            "UPDATE run_tool_calls SET approval_requested = 1 \
             WHERE organization_id = ? AND workspace_id = ? AND run_id = ? AND id = ?",
        )
        .bind(organization_id.uuid())
        .bind(workspace_id.uuid())
        .bind(run_id.uuid())
        .bind(call_id.uuid())
        .execute(&mut *transaction)
        .await
        .map_err(failed("request_approval"))?;
        sqlx::query(
            "UPDATE runs SET status = 'waiting_for_approval' WHERE organization_id = ? AND workspace_id = ? AND id = ?",
        )
        .bind(organization_id.uuid())
        .bind(workspace_id.uuid())
        .bind(run_id.uuid())
        .execute(&mut *transaction)
        .await
        .map_err(failed("request_approval"))?;
        let appended = append(&mut transaction, organization_id, workspace_id, run_id, events, now).await?;
        transaction.commit().await.map_err(failed("request_approval"))?;
        Ok(appended)
    }

    async fn resolve_approval(
        &self,
        organization_id: OrganizationId,
        workspace_id: WorkspaceId,
        run_id: RunId,
        call_id: ToolCallId,
        decision: ApprovalDecision,
        api_key_id: ApiKeyId,
        denial: &CallOutcome,
        events: &[NewRunEvent],
        job: &QueuedJob,
        now: Timestamp,
    ) -> Result<(Run, Vec<RunEvent>), RunError> {
        let mut transaction = self.immediate("resolve_approval").await?;
        match state_of(&mut transaction, organization_id, workspace_id, run_id).await? {
            None => return Err(RunError::NotFound),
            Some((RunStatus::WaitingForApproval, _)) => {}
            Some(_) => return Err(RunError::NotWaiting),
        }
        let call = call_in(&mut transaction, organization_id, workspace_id, run_id, call_id).await?;
        if call.is_none_or(|call| call.state != CallState::AwaitingApproval || !call.approval_requested) {
            return Err(RunError::NotWaiting);
        }
        let (state, outcome, finished_at) = match decision {
            ApprovalDecision::Approved => (CallState::Approved, None, None),
            ApprovalDecision::Denied => (CallState::Denied, Some(denial), Some(now.millis())),
        };
        sqlx::query(
            "UPDATE run_tool_calls SET state = ?, approval = ?, approval_api_key_id = ?, outcome = ?, \
             outcome_is_error = ?, finished_at = ? WHERE organization_id = ? AND workspace_id = ? AND run_id = ? AND id = ?",
        )
        .bind(state.as_str())
        .bind(decision.as_str())
        .bind(api_key_id.uuid())
        .bind(outcome.map(|outcome| outcome.content.as_str()))
        .bind(outcome.map(|outcome| i64::from(outcome.is_error)))
        .bind(finished_at)
        .bind(organization_id.uuid())
        .bind(workspace_id.uuid())
        .bind(run_id.uuid())
        .bind(call_id.uuid())
        .execute(&mut *transaction)
        .await
        .map_err(failed("resolve_approval"))?;
        let job_id = queue(&mut transaction, &Scope::Workspace(organization_id, workspace_id), job).await?;
        sqlx::query(
            "UPDATE runs SET status = 'running', job_id = ? WHERE organization_id = ? AND workspace_id = ? AND id = ?",
        )
        .bind(job_id.uuid())
        .bind(organization_id.uuid())
        .bind(workspace_id.uuid())
        .bind(run_id.uuid())
        .execute(&mut *transaction)
        .await
        .map_err(failed("resolve_approval"))?;
        let appended = append(&mut transaction, organization_id, workspace_id, run_id, events, now).await?;
        let run = find_in(&mut transaction, organization_id, workspace_id, run_id).await?;
        transaction.commit().await.map_err(failed("resolve_approval"))?;
        Ok((run, appended))
    }

    async fn begin_call(
        &self,
        organization_id: OrganizationId,
        workspace_id: WorkspaceId,
        run_id: RunId,
        call_id: ToolCallId,
        events: &[NewRunEvent],
        now: Timestamp,
    ) -> Result<Vec<RunEvent>, RunError> {
        let mut transaction = self.immediate("begin_call").await?;
        match state_of(&mut transaction, organization_id, workspace_id, run_id).await? {
            None => return Err(RunError::NotFound),
            Some((RunStatus::Running, _)) => {}
            Some(_) => return Err(RunError::Moved),
        }
        let call = call_in(&mut transaction, organization_id, workspace_id, run_id, call_id).await?;
        if call.is_none_or(|call| !matches!(call.state, CallState::Allowed | CallState::Approved)) {
            return Err(RunError::Moved);
        }
        sqlx::query(
            "UPDATE run_tool_calls SET state = 'started', started_at = ? \
             WHERE organization_id = ? AND workspace_id = ? AND run_id = ? AND id = ?",
        )
        .bind(now.millis())
        .bind(organization_id.uuid())
        .bind(workspace_id.uuid())
        .bind(run_id.uuid())
        .bind(call_id.uuid())
        .execute(&mut *transaction)
        .await
        .map_err(failed("begin_call"))?;
        sqlx::query(
            "UPDATE runs SET tool_calls = tool_calls + 1 WHERE organization_id = ? AND workspace_id = ? AND id = ?",
        )
        .bind(organization_id.uuid())
        .bind(workspace_id.uuid())
        .bind(run_id.uuid())
        .execute(&mut *transaction)
        .await
        .map_err(failed("begin_call"))?;
        let appended = append(&mut transaction, organization_id, workspace_id, run_id, events, now).await?;
        transaction.commit().await.map_err(failed("begin_call"))?;
        Ok(appended)
    }

    async fn settle_call(
        &self,
        organization_id: OrganizationId,
        workspace_id: WorkspaceId,
        run_id: RunId,
        call_id: ToolCallId,
        from: &[CallState],
        to: CallState,
        outcome: &CallOutcome,
        events: &[NewRunEvent],
        now: Timestamp,
    ) -> Result<Vec<RunEvent>, RunError> {
        if !to.is_settled() {
            return Err(RunError::Moved);
        }
        let mut transaction = self.immediate("settle_call").await?;
        if state_of(&mut transaction, organization_id, workspace_id, run_id)
            .await?
            .is_none()
        {
            return Err(RunError::NotFound);
        }
        let call = call_in(&mut transaction, organization_id, workspace_id, run_id, call_id).await?;
        if call.is_none_or(|call| !from.contains(&call.state)) {
            return Err(RunError::Moved);
        }
        sqlx::query(
            "UPDATE run_tool_calls SET state = ?, outcome = ?, outcome_is_error = ?, finished_at = ? \
             WHERE organization_id = ? AND workspace_id = ? AND run_id = ? AND id = ?",
        )
        .bind(to.as_str())
        .bind(&outcome.content)
        .bind(i64::from(outcome.is_error))
        .bind(now.millis())
        .bind(organization_id.uuid())
        .bind(workspace_id.uuid())
        .bind(run_id.uuid())
        .bind(call_id.uuid())
        .execute(&mut *transaction)
        .await
        .map_err(failed("settle_call"))?;
        let appended = append(&mut transaction, organization_id, workspace_id, run_id, events, now).await?;
        transaction.commit().await.map_err(failed("settle_call"))?;
        Ok(appended)
    }

    async fn decide_call(
        &self,
        organization_id: OrganizationId,
        workspace_id: WorkspaceId,
        run_id: RunId,
        call_id: ToolCallId,
        from: &[CallState],
        to: CallState,
        decision: &RecordedDecision,
        outcome: Option<&CallOutcome>,
        events: &[NewRunEvent],
        now: Timestamp,
    ) -> Result<Vec<RunEvent>, RunError> {
        let settled = match (to, outcome) {
            (CallState::AwaitingApproval, None) => None,
            (CallState::Refused, Some(outcome)) => Some(outcome),
            _ => return Err(RunError::Moved),
        };
        let mut transaction = self.immediate("decide_call").await?;
        match state_of(&mut transaction, organization_id, workspace_id, run_id).await? {
            None => return Err(RunError::NotFound),
            Some((RunStatus::Running, _)) => {}
            Some(_) => return Err(RunError::Moved),
        }
        let call = call_in(&mut transaction, organization_id, workspace_id, run_id, call_id).await?;
        if call.is_none_or(|call| !from.contains(&call.state)) {
            return Err(RunError::Moved);
        }
        sqlx::query(
            "UPDATE run_tool_calls SET state = ?, verdict = ?, policy = ?, mode = ?, rule = ?, reason = ?, \
             outcome = ?, outcome_is_error = ?, finished_at = ? \
             WHERE organization_id = ? AND workspace_id = ? AND run_id = ? AND id = ?",
        )
        .bind(to.as_str())
        .bind(verdict_of(decision))
        .bind(&decision.policy)
        .bind(decision.mode.as_deref())
        .bind(&decision.rule)
        .bind(decision.reason.as_deref())
        .bind(settled.map(|outcome| outcome.content.as_str()))
        .bind(settled.map(|outcome| i64::from(outcome.is_error)))
        .bind(settled.map(|_| now.millis()))
        .bind(organization_id.uuid())
        .bind(workspace_id.uuid())
        .bind(run_id.uuid())
        .bind(call_id.uuid())
        .execute(&mut *transaction)
        .await
        .map_err(failed("decide_call"))?;
        let appended = append(&mut transaction, organization_id, workspace_id, run_id, events, now).await?;
        transaction.commit().await.map_err(failed("decide_call"))?;
        Ok(appended)
    }

    async fn finish_run(
        &self,
        organization_id: OrganizationId,
        workspace_id: WorkspaceId,
        run_id: RunId,
        turns: u32,
        ending: &Ending,
        events: &[NewRunEvent],
        now: Timestamp,
    ) -> Result<Vec<RunEvent>, RunError> {
        let mut transaction = self.immediate("finish_run").await?;
        match state_of(&mut transaction, organization_id, workspace_id, run_id).await? {
            None => return Err(RunError::NotFound),
            Some((status, _)) if status.is_terminal() => return Err(RunError::Finished),
            Some((_, recorded)) if recorded.len() != turns as usize => return Err(RunError::Moved),
            Some(_) => {}
        }
        end(&mut transaction, organization_id, workspace_id, run_id, ending, now).await?;
        let appended = append(&mut transaction, organization_id, workspace_id, run_id, events, now).await?;
        transaction.commit().await.map_err(failed("finish_run"))?;
        Ok(appended)
    }

    /// **Cross-tenant**, by design: the sweep serves every organization. It
    /// reads `runs` and `jobs` only, matching each run's job within the
    /// run's own organization and workspace, and answers each run's scope.
    async fn abandoned_runs(&self, limit: u32) -> Result<Vec<(OrganizationId, WorkspaceId, RunId)>, RunError> {
        let rows = sqlx::query(
            "SELECT r.organization_id, r.workspace_id, r.id FROM runs r \
             WHERE r.status IN ('queued', 'running') AND NOT EXISTS (SELECT 1 FROM jobs j \
             WHERE j.organization_id = r.organization_id AND j.workspace_id = r.workspace_id \
             AND j.id = r.job_id AND j.state IN ('queued', 'running')) \
             ORDER BY r.created_at, r.id LIMIT ?",
        )
        .bind(i64::from(limit.clamp(1, zupflow_core::runs::MAX_PAGE)))
        .fetch_all(self.database.pool())
        .await
        .map_err(failed("abandoned_runs"))?;
        rows.iter()
            .map(|row| {
                Ok((
                    OrganizationId::from_uuid(row.try_get("organization_id").map_err(read)?),
                    WorkspaceId::from_uuid(row.try_get("workspace_id").map_err(read)?),
                    RunId::from_uuid(row.try_get("id").map_err(read)?),
                ))
            })
            .collect()
    }

    async fn cancel_run(
        &self,
        organization_id: OrganizationId,
        workspace_id: WorkspaceId,
        run_id: RunId,
        events: &[NewRunEvent],
        now: Timestamp,
    ) -> Result<(Run, Vec<RunEvent>), RunError> {
        let mut transaction = self.immediate("cancel_run").await?;
        match state_of(&mut transaction, organization_id, workspace_id, run_id).await? {
            None => return Err(RunError::NotFound),
            Some((status, _)) if status.is_terminal() => return Err(RunError::Finished),
            Some(_) => {}
        }
        sqlx::query(
            "UPDATE runs SET status = 'cancelled', finished_at = ? WHERE organization_id = ? AND workspace_id = ? AND id = ?",
        )
        .bind(now.millis())
        .bind(organization_id.uuid())
        .bind(workspace_id.uuid())
        .bind(run_id.uuid())
        .execute(&mut *transaction)
        .await
        .map_err(failed("cancel_run"))?;
        let appended = append(&mut transaction, organization_id, workspace_id, run_id, events, now).await?;
        let run = find_in(&mut transaction, organization_id, workspace_id, run_id).await?;
        transaction.commit().await.map_err(failed("cancel_run"))?;
        Ok((run, appended))
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use std::sync::Arc;

    use zupflow_conformance::run_store::{Prepare, Stores, Tenant};

    use super::*;
    use crate::schema::{Temporary, temporary};
    use crate::{SqliteAgentStore, SqliteJobQueue};

    /// Makes the tenant's rows straight in the database: an organization
    /// with a Sandbox and a Production workspace.
    fn prepare(database: Database) -> Prepare {
        Arc::new(move |tenant: Tenant| {
            let database = database.clone();
            Box::pin(async move {
                let pool = database.pool();
                let org = tenant.organization_id.uuid();
                sqlx::query("INSERT INTO organizations (id, name, slug, created_at) VALUES (?, 'Test', ?, 0)")
                    .bind(org)
                    .bind(format!("org-{}", org.simple()))
                    .execute(pool)
                    .await
                    .unwrap();
                for (workspace, environment) in [(tenant.workspace, "sandbox"), (tenant.sibling, "production")] {
                    sqlx::query(
                        "INSERT INTO workspaces (id, organization_id, environment, name, slug, created_at) \
                         VALUES (?, ?, ?, 'W', ?, 0)",
                    )
                    .bind(workspace.uuid())
                    .bind(org)
                    .bind(environment)
                    .bind(format!("w-{}", workspace.uuid().simple()))
                    .execute(pool)
                    .await
                    .unwrap();
                }
            })
        })
    }

    #[tokio::test]
    async fn conforms() {
        let Temporary {
            database,
            directory: _directory,
        } = temporary().await;
        zupflow_conformance::run_store::run(
            Stores {
                runs: &SqliteRunStore::new(database.clone()),
                agents: &SqliteAgentStore::new(database.clone()),
                jobs: &SqliteJobQueue::new(database.clone()),
            },
            prepare(database),
        )
        .await;
    }

    /// The database itself keeps a run's events and calls in its workspace,
    /// and deleting the workspace takes them.
    #[tokio::test]
    async fn keeps_a_runs_rows_inside_its_workspace() {
        use zupflow_core::agents::{AgentStore, NewAgent};
        use zupflow_core::ids::AgentId;
        use zupflow_core::jobs::JobOptions;
        use zupflow_core::scope::Name;

        let Temporary {
            database,
            directory: _directory,
        } = temporary().await;
        let tenant = Tenant {
            organization_id: OrganizationId::generate(),
            workspace: WorkspaceId::generate(),
            sibling: WorkspaceId::generate(),
        };
        prepare(database.clone())(tenant).await;
        let org = tenant.organization_id;
        let agent = SqliteAgentStore::new(database.clone())
            .create_agent(
                org,
                tenant.workspace,
                AgentId::generate(),
                &NewAgent {
                    name: "A".to_owned(),
                    description: String::new(),
                    model: "m".to_owned(),
                    instructions: String::new(),
                },
            )
            .await
            .unwrap();
        let store = SqliteRunStore::new(database.clone());
        let id = RunId::generate();
        store
            .create_run(
                org,
                tenant.workspace,
                id,
                &NewRun {
                    agent_id: agent.id,
                    session_id: SessionId::generate(),
                    trigger: RunTrigger::Console,
                    input: "hi".to_owned(),
                    model: "m".to_owned(),
                    instructions: String::new(),
                },
                &[NewRunEvent::new(RunEventType::RunQueued, serde_json::json!({}))],
                &QueuedJob {
                    kind: Name::new("run_agent").unwrap(),
                    payload: Vec::new(),
                    options: JobOptions::default(),
                },
                Timestamp::now(),
            )
            .await
            .unwrap();
        let refused = sqlx::query(
            "INSERT INTO run_events (id, organization_id, workspace_id, run_id, sequence, type, data, created_at) \
             VALUES (?, ?, ?, ?, 9, 'run.started', '{}', 0)",
        )
        .bind(Uuid::now_v7())
        .bind(org.uuid())
        .bind(tenant.sibling.uuid())
        .bind(id.uuid())
        .execute(database.pool())
        .await;
        assert!(refused.is_err(), "an event in another workspace than its run's");
        sqlx::query("DELETE FROM workspaces WHERE organization_id = ? AND id = ?")
            .bind(org.uuid())
            .bind(tenant.workspace.uuid())
            .execute(database.pool())
            .await
            .unwrap();
        for (table, count) in [
            ("runs", "SELECT count(*) FROM runs"),
            ("run_events", "SELECT count(*) FROM run_events"),
            ("run_tool_calls", "SELECT count(*) FROM run_tool_calls"),
        ] {
            let left: i64 = sqlx::query_scalar(count).fetch_one(database.pool()).await.unwrap();
            assert_eq!(left, 0, "{table} goes with the workspace");
        }
    }
}
