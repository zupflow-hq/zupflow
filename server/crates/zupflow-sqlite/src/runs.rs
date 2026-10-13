//! [`SqliteRunStore`]: runs (decision 0046), in the same database as the
//! job queue: runs in `runs`, their timelines in `run_events`, their tool
//! calls in `run_tool_calls`, and the engine's own records in
//! `run_transitions`, `run_turns`, `run_model_attempts` and
//! `run_call_attempts`.
//!
//! Every `apply` is one `BEGIN IMMEDIATE` transaction: it takes SQLite's
//! write lock first, then finds an earlier commit of the same request, reads
//! the time from the store's clock, reads the run as committed, checks the
//! request's guards and that its operation fits the run, and writes the run,
//! its turns, calls and attempts, the events the operation derives (numbered
//! after the run's last), their webhook announcements (the outbox), the
//! audit row of a control request, the run's job and the transition itself.
//! Any failure rolls all of it back.
//!
//! Every statement repeats `organization_id` and `workspace_id` beside the
//! run's ID, and the organization-wide list joins `workspaces` on the
//! organization too; the one that doesn't is the sweep's, named as
//! cross-tenant.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use serde_json::{Map, Value, json};
use sqlx::error::ErrorKind;
use sqlx::sqlite::SqliteRow;
use sqlx::{QueryBuilder, Row, Sqlite, SqliteConnection};
use uuid::Uuid;
use zupflow_core::audit::{AuditAction, AuditActor, AuditEntry, AuditObject, AuditPlace, AuditTarget};
use zupflow_core::environment::Environment;
use zupflow_core::ids::{
    AgentId, ApiKeyId, InstallationId, ModelConnectionId, ModelId, OrganizationId, RunEventId, RunId, SessionId,
    ToolCallId, WorkspaceId,
};
use zupflow_core::models::MAX_RETRY_AFTER;
use zupflow_core::runs::digest::{Sha256Digest, call_digest};
use zupflow_core::runs::names::Offer;
use zupflow_core::runs::reducer::{INSTALLATION_DISCONNECTED, INSTALLATION_REMOVED};
use zupflow_core::runs::state::{
    BoundTarget, CallAttempt, CallDecision, CallRecord, CallVerdict, ClaimantId, CommittedTurn, Continuation,
    EXECUTION_VERSION, InvocationId, ModelAttempt, ModelAttemptState, RunControl, Snapshot, supports,
};
use zupflow_core::runs::store::{Applied, Durability, MODEL_RETRY_STEP, Receipt, RunStore, StoreError, run_job};
use zupflow_core::runs::transition::{
    Admit, Authorization, AuthorizeCall, Cancel, Claim, Closing, CommitTurn, Conflict, FailAbandoned, Finish, Guards,
    ModelAttemptRef, NewCall, Operation, Park, PrepareModel, RecordOutcome, RefuseWaiting, ResolveApproval, RunEnding,
    SendLease, SendOutcome, SettleUnknown, TransitionId, TransitionRequest, Yield, YieldReason,
};
use zupflow_core::runs::{
    AgentActivity, ApprovalDecision, CallOutcome, CallState, Failure, OrganizationRun, OrganizationRunFilter, Page,
    Run, RunEvent, RunEventType, RunFilter, RunStatus, RunTrigger, RunUsage, UsageDay,
};
use zupflow_core::scope::Scope;
use zupflow_core::time::{Clock, SystemClock, Timestamp};
use zupflow_core::webhooks::Announcement;

use crate::Database;
use crate::job_queue::insert_job_at;

pub struct SqliteRunStore {
    database: Database,
    clock: Arc<dyn Clock>,
}

impl SqliteRunStore {
    pub fn new(database: Database) -> Self {
        Self::with_clock(database, Arc::new(SystemClock))
    }

    /// A store whose leases, events, jobs and audit rows take their time from
    /// `clock`: the system's in a deployment, one a test moves itself.
    pub fn with_clock(database: Database, clock: Arc<dyn Clock>) -> Self {
        Self { database, clock }
    }
}

/// A write the database's constraints refuse is the request's fault, and
/// changes nothing; anything else is the store being unavailable.
fn failed(operation: &'static str) -> impl FnOnce(sqlx::Error) -> StoreError {
    move |error| {
        let refused = error.as_database_error().is_some_and(|database| {
            matches!(
                database.kind(),
                ErrorKind::UniqueViolation
                    | ErrorKind::ForeignKeyViolation
                    | ErrorKind::NotNullViolation
                    | ErrorKind::CheckViolation
            )
        });
        if refused {
            tracing::warn!(event = "runs.write_refused", operation, %error);
            StoreError::Refused("a write the store's constraints refuse")
        } else {
            tracing::error!(event = "runs.database_error", operation, %error);
            StoreError::Unavailable
        }
    }
}

/// A stored value this build can't read. The error itself isn't logged: a
/// parser's message can quote what it read, and rows hold customer content.
fn unreadable<E>(_error: E) -> StoreError {
    tracing::error!(event = "runs.unreadable_row");
    StoreError::Unavailable
}

/// A stored value that isn't what the store writes: `what` names its kind.
fn corrupt(what: &'static str) -> StoreError {
    tracing::error!(event = "runs.unreadable_row", what);
    StoreError::Unavailable
}

/// A stored count or position.
fn int(value: u64) -> Result<i64, StoreError> {
    i64::try_from(value).map_err(|_| StoreError::Refused("a number larger than the store keeps"))
}

fn uint(row: &SqliteRow, column: &str) -> Result<u64, StoreError> {
    u64::try_from(row.try_get::<i64, _>(column).map_err(unreadable)?).map_err(unreadable)
}

fn small(row: &SqliteRow, column: &str) -> Result<u32, StoreError> {
    u32::try_from(row.try_get::<i64, _>(column).map_err(unreadable)?).map_err(unreadable)
}

fn instant(row: &SqliteRow, column: &str) -> Result<Option<Timestamp>, StoreError> {
    Ok(row
        .try_get::<Option<i64>, _>(column)
        .map_err(unreadable)?
        .map(Timestamp::from_millis))
}

fn digest(row: &SqliteRow, column: &str) -> Result<Option<Sha256Digest>, StoreError> {
    row.try_get::<Option<String>, _>(column)
        .map_err(unreadable)?
        .map(|text| Sha256Digest::parse(&text).ok_or_else(|| corrupt("a digest")))
        .transpose()
}

fn after_ms(now: Timestamp, ms: u64) -> Timestamp {
    Timestamp::from_millis(now.millis().saturating_add(i64::try_from(ms).unwrap_or(i64::MAX)))
}

fn model_attempt_state(state: ModelAttemptState) -> &'static str {
    match state {
        ModelAttemptState::Prepared => "prepared",
        ModelAttemptState::Answered => "answered",
        ModelAttemptState::Abandoned => "abandoned",
        ModelAttemptState::Unanswered => "unanswered",
    }
}

fn parse_model_attempt_state(state: &str) -> Option<ModelAttemptState> {
    [
        ModelAttemptState::Prepared,
        ModelAttemptState::Answered,
        ModelAttemptState::Abandoned,
        ModelAttemptState::Unanswered,
    ]
    .into_iter()
    .find(|candidate| model_attempt_state(*candidate) == state)
}

/// A decision as its columns: verdict, reason, policy, mode, rule.
fn decision_columns(decision: &CallDecision) -> (&'static str, Option<&str>) {
    match &decision.verdict {
        CallVerdict::Allow => ("allow", None),
        CallVerdict::RequireApproval { reason } => ("require_approval", Some(reason.as_str())),
        CallVerdict::Deny { reason } => ("deny", Some(reason.as_str())),
    }
}

/// The run's row, as `apply` and the snapshot read it.
struct RunRow {
    status: RunStatus,
    continuation: Option<String>,
    revision: u64,
    owner_epoch: u64,
    owner_claimant: Option<Uuid>,
    lease_until: Option<Timestamp>,
    cancel_requested_at: Option<Timestamp>,
    execution_version: u32,
    job_id: Uuid,
    agent_id: AgentId,
    model: String,
    model_id: Option<Uuid>,
    model_connection_id: Option<Uuid>,
    instructions: String,
    input: String,
    usage: RunUsage,
}

async fn run_row(
    connection: &mut SqliteConnection,
    organization_id: OrganizationId,
    workspace_id: WorkspaceId,
    run_id: RunId,
) -> Result<Option<RunRow>, StoreError> {
    let row = sqlx::query(
        "SELECT status, continuation, revision, owner_epoch, owner_claimant, lease_until, cancel_requested_at, \
         execution_version, job_id, agent_id, model, model_id, model_connection_id, instructions, input, \
         input_tokens, output_tokens, tool_calls \
         FROM runs WHERE organization_id = ? AND workspace_id = ? AND id = ?",
    )
    .bind(organization_id.uuid())
    .bind(workspace_id.uuid())
    .bind(run_id.uuid())
    .fetch_optional(&mut *connection)
    .await
    .map_err(failed("run_row"))?;
    let Some(row) = row else { return Ok(None) };
    let status: String = row.try_get("status").map_err(unreadable)?;
    Ok(Some(RunRow {
        status: RunStatus::parse(&status).ok_or_else(|| corrupt("a run status"))?,
        continuation: row.try_get("continuation").map_err(unreadable)?,
        revision: uint(&row, "revision")?,
        owner_epoch: uint(&row, "owner_epoch")?,
        owner_claimant: row.try_get("owner_claimant").map_err(unreadable)?,
        lease_until: instant(&row, "lease_until")?,
        cancel_requested_at: instant(&row, "cancel_requested_at")?,
        execution_version: small(&row, "execution_version")?,
        job_id: row.try_get("job_id").map_err(unreadable)?,
        agent_id: AgentId::from_uuid(row.try_get("agent_id").map_err(unreadable)?),
        model: row.try_get("model").map_err(unreadable)?,
        model_id: row.try_get("model_id").map_err(unreadable)?,
        model_connection_id: row.try_get("model_connection_id").map_err(unreadable)?,
        instructions: row.try_get("instructions").map_err(unreadable)?,
        input: row.try_get("input").map_err(unreadable)?,
        usage: RunUsage {
            input_tokens: uint(&row, "input_tokens")?,
            output_tokens: uint(&row, "output_tokens")?,
            tool_calls: uint(&row, "tool_calls")?,
        },
    }))
}

/// A run as committed, with what the snapshot leaves out.
struct Loaded {
    snapshot: Snapshot,
    job_id: Uuid,
    agent_id: AgentId,
    usage: RunUsage,
    /// When each send claim was authorized, by call and attempt.
    authorized: HashMap<(ToolCallId, u32), Timestamp>,
}

fn call_from(row: &SqliteRow) -> Result<CallRecord, StoreError> {
    let text = |column: &str| row.try_get::<Option<String>, _>(column).map_err(unreadable);
    let installation: Option<Uuid> = row.try_get("installation_id").map_err(unreadable)?;
    let definition = digest(row, "definition_sha256")?;
    let target = match (
        installation,
        text("installation_short_id")?,
        text("installation_label")?,
        text("app_key")?,
        text("app_name")?,
        text("tool")?,
        definition,
    ) {
        (
            Some(installation_id),
            Some(installation_short_id),
            Some(installation_label),
            Some(app_key),
            Some(app_name),
            Some(tool),
            Some(definition_sha256),
        ) => Some(BoundTarget {
            installation_id: InstallationId::from_uuid(installation_id),
            installation_short_id,
            installation_label,
            app_key,
            app_name,
            tool,
            definition_sha256,
        }),
        (None, None, None, None, None, None, None) => None,
        _ => return Err(corrupt("a call's target")),
    };
    let decision = match (text("verdict")?, text("policy")?, text("rule")?) {
        (Some(verdict), Some(policy), Some(rule)) => {
            let reason = text("reason")?;
            let verdict = match (verdict.as_str(), reason) {
                ("allow", _) => CallVerdict::Allow,
                ("require_approval", Some(reason)) => CallVerdict::RequireApproval { reason },
                ("deny", Some(reason)) => CallVerdict::Deny { reason },
                _ => return Err(corrupt("a call's verdict")),
            };
            Some(CallDecision {
                verdict,
                policy,
                mode: text("mode")?,
                rule,
            })
        }
        (None, None, None) => None,
        _ => return Err(corrupt("a call's decision")),
    };
    let approval_key: Option<Uuid> = row.try_get("approval_api_key_id").map_err(unreadable)?;
    let approval = match (text("approval")?, approval_key) {
        (Some(decision), Some(key)) => Some((
            ApprovalDecision::parse(&decision).ok_or_else(|| corrupt("an approval"))?,
            ApiKeyId::from_uuid(key),
        )),
        _ => None,
    };
    let outcome = match (
        text("outcome")?,
        row.try_get::<Option<i64>, _>("outcome_is_error").map_err(unreadable)?,
    ) {
        (Some(content), Some(is_error)) => Some(CallOutcome {
            content,
            is_error: is_error == 1,
        }),
        _ => None,
    };
    let failure = match (text("failure_code")?, text("failure_message")?) {
        (Some(code), Some(message)) => Some(Failure { code, message }),
        _ => None,
    };
    let arguments: String = row.try_get("arguments").map_err(unreadable)?;
    let state: String = row.try_get("state").map_err(unreadable)?;
    Ok(CallRecord {
        id: ToolCallId::from_uuid(row.try_get("id").map_err(unreadable)?),
        turn: small(row, "turn")?,
        position: small(row, "position")?,
        model_call_id: row.try_get("model_call_id").map_err(unreadable)?,
        model_tool_name: row.try_get("name").map_err(unreadable)?,
        target,
        arguments: serde_json::from_str(&arguments).map_err(unreadable)?,
        state: CallState::parse(&state).ok_or_else(|| corrupt("a call state"))?,
        decision,
        call_digest: digest(row, "call_digest")?,
        approval,
        outcome,
        failure,
        attempts: Vec::new(),
    })
}

/// The run as committed, read on `connection` (inside a transaction, so
/// every part is of the same commit).
async fn load(
    connection: &mut SqliteConnection,
    organization_id: OrganizationId,
    workspace_id: WorkspaceId,
    run_id: RunId,
    row: RunRow,
) -> Result<Loaded, StoreError> {
    if !supports(row.execution_version) {
        return Err(StoreError::Unsupported);
    }
    let continuation = row
        .continuation
        .as_deref()
        .ok_or_else(|| corrupt("a run without a continuation"))
        .and_then(|json| Continuation::from_json(json).map_err(unreadable))?;
    let scope = (organization_id.uuid(), workspace_id.uuid(), run_id.uuid());

    let turns = sqlx::query(
        "SELECT turn, text FROM run_turns WHERE organization_id = ? AND workspace_id = ? AND run_id = ? ORDER BY turn",
    )
    .bind(scope.0)
    .bind(scope.1)
    .bind(scope.2)
    .fetch_all(&mut *connection)
    .await
    .map_err(failed("load_turns"))?
    .iter()
    .map(|row| {
        Ok(CommittedTurn {
            turn: small(row, "turn")?,
            text: row.try_get("text").map_err(unreadable)?,
        })
    })
    .collect::<Result<Vec<_>, StoreError>>()?;

    let mut calls = sqlx::query(
        "SELECT id, turn, position, model_call_id, name, installation_id, installation_short_id, installation_label, \
         app_key, app_name, tool, definition_sha256, arguments, state, verdict, policy, mode, rule, reason, \
         call_digest, approval, approval_api_key_id, outcome, outcome_is_error, failure_code, failure_message \
         FROM run_tool_calls WHERE organization_id = ? AND workspace_id = ? AND run_id = ? ORDER BY turn, position",
    )
    .bind(scope.0)
    .bind(scope.1)
    .bind(scope.2)
    .fetch_all(&mut *connection)
    .await
    .map_err(failed("load_calls"))?
    .iter()
    .map(call_from)
    .collect::<Result<Vec<_>, StoreError>>()?;

    let mut authorized = HashMap::new();
    let attempts = sqlx::query(
        "SELECT call_id, attempt, dispatch_epoch, invocation_id, authorized_at, send_lease_until, outcome_recorded_at \
         FROM run_call_attempts WHERE organization_id = ? AND workspace_id = ? AND run_id = ? ORDER BY call_id, attempt",
    )
    .bind(scope.0)
    .bind(scope.1)
    .bind(scope.2)
    .fetch_all(&mut *connection)
    .await
    .map_err(failed("load_call_attempts"))?;
    for row in &attempts {
        let call_id = ToolCallId::from_uuid(row.try_get("call_id").map_err(unreadable)?);
        let attempt = CallAttempt {
            attempt: small(row, "attempt")?,
            dispatch_epoch: uint(row, "dispatch_epoch")?,
            invocation_id: InvocationId::from_uuid(row.try_get("invocation_id").map_err(unreadable)?),
            send_lease_until: Timestamp::from_millis(row.try_get("send_lease_until").map_err(unreadable)?),
            outcome_recorded: instant(row, "outcome_recorded_at")?.is_some(),
        };
        let at = Timestamp::from_millis(row.try_get("authorized_at").map_err(unreadable)?);
        authorized.insert((call_id, attempt.attempt), at);
        calls
            .iter_mut()
            .find(|call| call.id == call_id)
            .ok_or_else(|| corrupt("an attempt of no call"))?
            .attempts
            .push(attempt);
    }

    let model_attempts = sqlx::query(
        "SELECT turn, attempt, state, request_digest, offer, timed_out FROM run_model_attempts \
         WHERE organization_id = ? AND workspace_id = ? AND run_id = ? ORDER BY turn, attempt",
    )
    .bind(scope.0)
    .bind(scope.1)
    .bind(scope.2)
    .fetch_all(&mut *connection)
    .await
    .map_err(failed("load_model_attempts"))?
    .iter()
    .map(|row| {
        let state: String = row.try_get("state").map_err(unreadable)?;
        let offer: String = row.try_get("offer").map_err(unreadable)?;
        Ok(ModelAttempt {
            turn: small(row, "turn")?,
            attempt: small(row, "attempt")?,
            state: parse_model_attempt_state(&state).ok_or_else(|| corrupt("a model attempt state"))?,
            request_digest: digest(row, "request_digest")?.ok_or_else(|| corrupt("a request digest"))?,
            offer: serde_json::from_str::<Offer>(&offer).map_err(unreadable)?,
            timed_out: row.try_get::<i64, _>("timed_out").map_err(unreadable)? != 0,
        })
    })
    .collect::<Result<Vec<_>, StoreError>>()?;

    Ok(Loaded {
        snapshot: Snapshot {
            organization_id,
            workspace_id,
            run_id,
            control: RunControl {
                status: row.status,
                continuation,
                revision: row.revision,
                owner_epoch: row.owner_epoch,
                owner_claimant: row.owner_claimant.map(ClaimantId::from_uuid),
                lease_until: row.lease_until,
                cancel_requested_at: row.cancel_requested_at,
                execution_version: row.execution_version,
            },
            model: row.model,
            model_id: row.model_id.map(ModelId::from_uuid),
            connection_id: row.model_connection_id.map(ModelConnectionId::from_uuid),
            instructions: row.instructions,
            input: row.input,
            turns,
            calls,
            model_attempts,
        },
        job_id: row.job_id,
        agent_id: row.agent_id,
        usage: row.usage,
        authorized,
    })
}

/// Calls sent with no outcome recorded yet, with their latest send claim.
fn in_flight(snapshot: &Snapshot) -> impl Iterator<Item = (&CallRecord, &CallAttempt)> {
    snapshot
        .calls
        .iter()
        .filter(|call| call.state == CallState::Started)
        .filter_map(|call| {
            call.attempts
                .iter()
                .max_by_key(|attempt| attempt.attempt)
                .filter(|attempt| !attempt.outcome_recorded)
                .map(|attempt| (call, attempt))
        })
}

/// The guards, on the store's clock: the first that fails, if any.
fn check_guards(snapshot: &Snapshot, guards: &Guards, now: Timestamp) -> Option<Conflict> {
    let control = &snapshot.control;
    let live = |until: Option<Timestamp>| until.is_some_and(|until| until > now);
    if guards.not_cancelled && control.cancel_requested_at.is_some() {
        return Some(Conflict::Cancelled);
    }
    if let Some(owner) = guards.owner
        && (owner.epoch != control.owner_epoch
            || control.owner_claimant != Some(owner.claimant)
            || !live(control.lease_until))
    {
        return Some(Conflict::Owner);
    }
    if guards.no_live_claim && live(control.lease_until) {
        return Some(Conflict::Claimed);
    }
    if guards.no_live_send && in_flight(snapshot).any(|(_, attempt)| attempt.send_lease_until > now) {
        return Some(Conflict::Claimed);
    }
    if let Some(send) = guards.send {
        let holds = snapshot
            .call(send.call_id)
            .and_then(|call| call.attempt(send.attempt))
            .is_some_and(|attempt| {
                attempt.dispatch_epoch == send.dispatch_epoch
                    && attempt.invocation_id == send.invocation_id
                    && !attempt.outcome_recorded
                    && (send.lease == SendLease::Any || attempt.send_lease_until <= now)
            });
        if !holds {
            return Some(Conflict::SendClaim);
        }
    }
    None
}

/// The guards each operation needs. A request without them is refused: the
/// store never applies a worker's step unfenced.
fn check_required(guards: &Guards, operation: &Operation) -> Result<(), StoreError> {
    let refused = StoreError::Refused;
    let worker = |keeps: bool| -> Result<(), StoreError> {
        let owner = guards.owner.ok_or(refused("a worker's step without its owner guard"))?;
        if !guards.not_cancelled {
            return Err(refused("a worker's step that doesn't stop at a cancel"));
        }
        if owner.renew_lease_ms.is_some() != keeps {
            return Err(refused(if keeps {
                "a step that keeps the claim without renewing it"
            } else {
                "a step that releases the claim but renews it"
            }));
        }
        Ok(())
    };
    match operation {
        Operation::Admit(_) => {
            if guards.owner.is_some() || guards.send.is_some() {
                return Err(refused("an admission guarded by a claim"));
            }
            Ok(())
        }
        Operation::Claim(_) => {
            if !(guards.not_cancelled && guards.no_live_claim) || guards.owner.is_some() {
                return Err(refused("a claim that doesn't wait for a live one or stop at a cancel"));
            }
            Ok(())
        }
        Operation::PrepareModel(_) => worker(true),
        Operation::CommitTurn(commit) => worker(commit.ending.is_none()),
        Operation::AuthorizeCall(_) => worker(true),
        Operation::Park(_) | Operation::Yield(_) | Operation::Finish(_) => worker(false),
        Operation::RecordOutcome(record) => match guards.send {
            Some(send)
                if send.call_id == record.call_id
                    && send.attempt == record.attempt
                    && send.invocation_id == record.invocation_id =>
            {
                Ok(())
            }
            _ => Err(refused("an outcome not fenced by its own send claim")),
        },
        Operation::SettleUnknown(settle) => match guards.send {
            Some(send)
                if send.call_id == settle.call_id
                    && send.attempt == settle.attempt
                    && send.lease == SendLease::Lapsed =>
            {
                Ok(())
            }
            _ => Err(refused("an unknown outcome not fenced by a lapsed send claim")),
        },
        Operation::FailAbandoned(_) => {
            if !(guards.no_live_claim && guards.no_live_send) {
                return Err(refused("the sweep's failure without its claim and send guards"));
            }
            Ok(())
        }
        Operation::ResolveApproval(_) | Operation::Cancel(_) => Ok(()),
        // The job holds no claim on the run, and no send claim.
        Operation::RefuseWaiting(_) => {
            if guards.owner.is_some() || guards.send.is_some() {
                return Err(refused("a refusal guarded by a claim"));
            }
            Ok(())
        }
    }
}

/// Whether `failure` says the call's installation went away (decision 0047):
/// the only reason a call waiting for approval is refused without a
/// decision.
fn installation_went(failure: &Failure) -> bool {
    failure.code == INSTALLATION_REMOVED || failure.code == INSTALLATION_DISCONNECTED
}

/// What a transition does to the run's job.
enum JobAction {
    Keep,
    /// A new job, due now.
    Queue,
    /// Due at this time, its attempt given back.
    Postpone(Timestamp),
    /// Tried again later (after the provider's own wait, if it gave one),
    /// using its attempt: dead once they run out.
    RetryModel(Option<Duration>),
    /// Due now, if it waits for later.
    BringForward,
    /// Finished: no worker needs it.
    Release,
}

/// What a finished run's job does: wait for the latest live send claim's
/// lease to lapse, so its call is settled, or go.
fn ended(sending: Option<Timestamp>) -> JobAction {
    sending.map_or(JobAction::Release, JobAction::Postpone)
}

/// The changes to the run's own row.
#[derive(Default)]
struct RowChange {
    started: bool,
    ending: Option<RunEnding>,
    add_input_tokens: u64,
    add_output_tokens: u64,
    add_tool_calls: u64,
}

/// One transition's writes, on its transaction.
struct Writer<'a> {
    connection: &'a mut SqliteConnection,
    organization_id: OrganizationId,
    workspace_id: WorkspaceId,
    run_id: RunId,
    now: Timestamp,
    events: Vec<(RunEventType, Value)>,
}

fn reference(call_id: ToolCallId, target: &BoundTarget) -> Map<String, Value> {
    let mut data = Map::new();
    data.insert("call_id".to_owned(), json!(call_id));
    data.insert("installation_id".to_owned(), json!(target.installation_id));
    data.insert("installation_label".to_owned(), json!(target.installation_label));
    data.insert("app_name".to_owned(), json!(target.app_name));
    data.insert("tool".to_owned(), json!(target.tool));
    data
}

fn with(mut data: Map<String, Value>, more: &[(&str, Value)]) -> Value {
    for (key, value) in more {
        data.insert((*key).to_owned(), value.clone());
    }
    Value::Object(data)
}

impl Writer<'_> {
    fn scope(&self) -> (Uuid, Uuid, Uuid) {
        (
            self.organization_id.uuid(),
            self.workspace_id.uuid(),
            self.run_id.uuid(),
        )
    }

    fn event(&mut self, kind: RunEventType, data: Value) {
        self.events.push((kind, data));
    }

    fn call_failed(&mut self, call: ToolCallId, target: &BoundTarget, failure: &Failure, duration_ms: i64) {
        self.event(
            RunEventType::ToolCallFailed,
            with(
                reference(call, target),
                &[("error", json!(failure)), ("duration_ms", json!(duration_ms.max(0)))],
            ),
        );
    }

    async fn insert_call(&mut self, turn: u32, call: &NewCall) -> Result<(), StoreError> {
        let fits = match call.state {
            CallState::Allowed | CallState::AwaitingApproval => {
                call.target.is_some() && call.decision.is_some() && call.outcome.is_none() && call.failure.is_none()
            }
            CallState::Refused => call.outcome.is_some(),
            _ => false,
        };
        if !fits || call.call_digest.is_some() != call.target.is_some() {
            return Err(StoreError::Refused("a call recorded in a state a turn can't give it"));
        }
        let (org, ws, run) = self.scope();
        let target = call.target.as_ref();
        let decision = call.decision.as_ref();
        let (verdict, reason) = decision.map(decision_columns).unzip();
        sqlx::query(
            "INSERT INTO run_tool_calls (id, organization_id, workspace_id, run_id, turn, position, model_call_id, \
             name, installation_id, installation_short_id, installation_label, app_key, app_name, tool, \
             definition_sha256, arguments, state, verdict, policy, mode, rule, reason, call_digest, \
             outcome, outcome_is_error, failure_code, failure_message, finished_at) \
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
        )
        .bind(call.id.uuid())
        .bind(org)
        .bind(ws)
        .bind(run)
        .bind(i64::from(turn))
        .bind(i64::from(call.position))
        .bind(&call.model_call_id)
        .bind(&call.model_tool_name)
        .bind(target.map(|target| target.installation_id.uuid()))
        .bind(target.map(|target| target.installation_short_id.as_str()))
        .bind(target.map(|target| target.installation_label.as_str()))
        .bind(target.map(|target| target.app_key.as_str()))
        .bind(target.map(|target| target.app_name.as_str()))
        .bind(target.map(|target| target.tool.as_str()))
        .bind(target.map(|target| target.definition_sha256.to_string()))
        .bind(Value::Object(call.arguments.clone()).to_string())
        .bind(call.state.as_str())
        .bind(verdict)
        .bind(decision.map(|decision| decision.policy.as_str()))
        .bind(decision.and_then(|decision| decision.mode.as_deref()))
        .bind(decision.map(|decision| decision.rule.as_str()))
        .bind(reason.flatten())
        .bind(call.call_digest.map(|digest| digest.to_string()))
        .bind(call.outcome.as_ref().map(|outcome| outcome.content.as_str()))
        .bind(call.outcome.as_ref().map(|outcome| i64::from(outcome.is_error)))
        .bind(call.failure.as_ref().map(|failure| failure.code.as_str()))
        .bind(call.failure.as_ref().map(|failure| failure.message.as_str()))
        .bind((call.state == CallState::Refused).then_some(self.now.millis()))
        .execute(&mut *self.connection)
        .await
        .map_err(failed("insert_call"))?;
        if call.state == CallState::Refused
            && let (Some(target), Some(failure)) = (target, &call.failure)
        {
            self.call_failed(call.id, target, failure, 0);
        }
        Ok(())
    }

    async fn decide(&mut self, call: ToolCallId, decision: &CallDecision) -> Result<(), StoreError> {
        let (org, ws, run) = self.scope();
        let (verdict, reason) = decision_columns(decision);
        sqlx::query(
            "UPDATE run_tool_calls SET verdict = ?, reason = ?, policy = ?, mode = ?, rule = ? \
             WHERE organization_id = ? AND workspace_id = ? AND run_id = ? AND id = ?",
        )
        .bind(verdict)
        .bind(reason)
        .bind(&decision.policy)
        .bind(decision.mode.as_deref())
        .bind(&decision.rule)
        .bind(org)
        .bind(ws)
        .bind(run)
        .bind(call.uuid())
        .execute(&mut *self.connection)
        .await
        .map_err(failed("decide"))?;
        Ok(())
    }

    /// Settles a call: never sent again.
    async fn settle(
        &mut self,
        call: ToolCallId,
        state: CallState,
        outcome: &CallOutcome,
        failure: Option<&Failure>,
    ) -> Result<(), StoreError> {
        let (org, ws, run) = self.scope();
        sqlx::query(
            "UPDATE run_tool_calls SET state = ?, outcome = ?, outcome_is_error = ?, failure_code = ?, \
             failure_message = ?, finished_at = ? \
             WHERE organization_id = ? AND workspace_id = ? AND run_id = ? AND id = ?",
        )
        .bind(state.as_str())
        .bind(&outcome.content)
        .bind(i64::from(outcome.is_error))
        .bind(failure.map(|failure| failure.code.as_str()))
        .bind(failure.map(|failure| failure.message.as_str()))
        .bind(self.now.millis())
        .bind(org)
        .bind(ws)
        .bind(run)
        .bind(call.uuid())
        .execute(&mut *self.connection)
        .await
        .map_err(failed("settle"))?;
        Ok(())
    }

    /// Closes every call `closing` names, never sent. They must be exactly
    /// the run's open calls.
    async fn close(&mut self, snapshot: &Snapshot, closing: Option<&Closing>) -> Result<(), StoreError> {
        let mut named: Vec<ToolCallId> = closing.map(|closing| closing.calls.clone()).unwrap_or_default();
        let mut open = snapshot.open_calls();
        named.sort();
        open.sort();
        if named != open {
            return Err(StoreError::Refused("a closing that isn't exactly the run's open calls"));
        }
        if let Some(closing) = closing {
            for call in &closing.calls {
                self.settle(*call, CallState::Refused, &closing.outcome, Some(&closing.failure))
                    .await?;
            }
        }
        Ok(())
    }

    async fn set_model_attempt(
        &mut self,
        turn: u32,
        attempt: u32,
        from: ModelAttemptState,
        to: ModelAttemptState,
        tokens: Option<(u64, u64)>,
    ) -> Result<(), StoreError> {
        let (org, ws, run) = self.scope();
        let (input, output) = tokens.map_or((None, None), |(input, output)| (Some(input), Some(output)));
        let changed = sqlx::query(
            "UPDATE run_model_attempts SET state = ?, input_tokens = ?, output_tokens = ?, updated_at = ? \
             WHERE organization_id = ? AND workspace_id = ? AND run_id = ? AND turn = ? AND attempt = ? AND state = ?",
        )
        .bind(model_attempt_state(to))
        .bind(input.map(int).transpose()?)
        .bind(output.map(int).transpose()?)
        .bind(self.now.millis())
        .bind(org)
        .bind(ws)
        .bind(run)
        .bind(i64::from(turn))
        .bind(i64::from(attempt))
        .bind(model_attempt_state(from))
        .execute(&mut *self.connection)
        .await
        .map_err(failed("set_model_attempt"))?;
        if changed.rows_affected() != 1 {
            return Err(StoreError::Refused("a model attempt that isn't where the request says"));
        }
        Ok(())
    }

    /// Records that an unanswered model attempt was sent and timed out.
    async fn mark_timed_out(&mut self, turn: u32, attempt: u32) -> Result<(), StoreError> {
        let (org, ws, run) = self.scope();
        let changed = sqlx::query(
            "UPDATE run_model_attempts SET timed_out = 1 \
             WHERE organization_id = ? AND workspace_id = ? AND run_id = ? AND turn = ? AND attempt = ? \
             AND state = 'unanswered'",
        )
        .bind(org)
        .bind(ws)
        .bind(run)
        .bind(i64::from(turn))
        .bind(i64::from(attempt))
        .execute(&mut *self.connection)
        .await
        .map_err(failed("mark_model_attempt_timed_out"))?;
        if changed.rows_affected() != 1 {
            return Err(StoreError::Refused("a model attempt that isn't where the request says"));
        }
        Ok(())
    }

    /// Marks the prepared attempt the run waits on abandoned: its answer will
    /// never be committed. It must be the one the run waits on.
    async fn abandon(&mut self, snapshot: &Snapshot, abandons: Option<ModelAttemptRef>) -> Result<(), StoreError> {
        let waiting = match snapshot.control.continuation {
            Continuation::AwaitModel { turn, attempt } => Some(ModelAttemptRef { turn, attempt }),
            _ => None,
        };
        if abandons != waiting {
            return Err(StoreError::Refused(
                "an abandoned model attempt the run doesn't wait on",
            ));
        }
        if let Some(ModelAttemptRef { turn, attempt }) = abandons {
            self.set_model_attempt(
                turn,
                attempt,
                ModelAttemptState::Prepared,
                ModelAttemptState::Abandoned,
                None,
            )
            .await?;
        }
        Ok(())
    }

    /// Appends the events after the run's last, in order, each with the
    /// webhook announcement it names (the outbox).
    async fn append(&mut self) -> Result<(u64, Vec<RunEvent>), StoreError> {
        let (org, ws, run) = self.scope();
        let last: i64 = sqlx::query_scalar(
            "SELECT coalesce(max(sequence), 0) FROM run_events \
             WHERE organization_id = ? AND workspace_id = ? AND run_id = ?",
        )
        .bind(org)
        .bind(ws)
        .bind(run)
        .fetch_one(&mut *self.connection)
        .await
        .map_err(failed("append"))?;
        let first = u64::try_from(last).map_err(unreadable)? + 1;
        let mut appended = Vec::with_capacity(self.events.len());
        for (offset, (kind, data)) in std::mem::take(&mut self.events).into_iter().enumerate() {
            let sequence = first + u64::try_from(offset).map_err(unreadable)?;
            let id = RunEventId::generate();
            sqlx::query(
                "INSERT INTO run_events (id, organization_id, workspace_id, run_id, sequence, type, data, created_at) \
                 VALUES (?, ?, ?, ?, ?, ?, ?, ?)",
            )
            .bind(id.uuid())
            .bind(org)
            .bind(ws)
            .bind(run)
            .bind(int(sequence)?)
            .bind(kind.as_str())
            .bind(data.to_string())
            .bind(self.now.millis())
            .execute(&mut *self.connection)
            .await
            .map_err(failed("append"))?;
            if let Some(announcement) = Announcement::of_run_event(self.run_id, kind, self.now) {
                let job = announcement
                    .job()
                    .ok_or(StoreError::Refused("an announcement that isn't a job"))?;
                insert_job_at(
                    &mut *self.connection,
                    &Scope::Workspace(self.organization_id, self.workspace_id),
                    &job.kind,
                    job.payload,
                    job.options,
                    self.now.millis(),
                )
                .await
                .map_err(|error| {
                    tracing::error!(event = "runs.announcement_not_queued", %error);
                    StoreError::Unavailable
                })?;
            }
            appended.push(RunEvent {
                id,
                organization_id: self.organization_id,
                workspace_id: self.workspace_id,
                run_id: self.run_id,
                sequence,
                kind,
                data,
                created_at: self.now,
            });
        }
        Ok((first, appended))
    }

    /// Queues a new job for the run, due at `at`; its ID.
    async fn queue_job(&mut self, at: Timestamp) -> Result<Uuid, StoreError> {
        let mut job = run_job(self.run_id).ok_or(StoreError::Refused("a run job that isn't a job"))?;
        job.options.delay =
            std::time::Duration::from_millis(u64::try_from(at.millis().saturating_sub(self.now.millis())).unwrap_or(0));
        let id = insert_job_at(
            &mut *self.connection,
            &Scope::Workspace(self.organization_id, self.workspace_id),
            &job.kind,
            job.payload,
            job.options,
            self.now.millis(),
        )
        .await
        .map_err(|error| {
            tracing::error!(event = "runs.job_not_queued", %error);
            StoreError::Unavailable
        })?;
        Ok(id.uuid())
    }

    /// Carries out `action` on the run's job; the job the run names after it.
    async fn job(&mut self, job_id: Uuid, action: JobAction) -> Result<Uuid, StoreError> {
        let (org, ws, _) = self.scope();
        let now = self.now.millis();
        match action {
            JobAction::Keep => Ok(job_id),
            JobAction::Queue => self.queue_job(self.now).await,
            JobAction::Postpone(at) => {
                let at = at.max(self.now);
                // A held job gives its attempt back, as a postponed one does.
                let postponed = sqlx::query(
                    "UPDATE jobs SET state = 'queued', attempts = CASE WHEN state = 'running' THEN attempts - 1 \
                     ELSE attempts END, run_at = ?, reason = NULL, lease_token = NULL, lease_expires_at = NULL \
                     WHERE organization_id = ? AND workspace_id = ? AND environment IS NULL AND id = ? \
                     AND state IN ('queued', 'running')",
                )
                .bind(at.millis())
                .bind(org)
                .bind(ws)
                .bind(job_id)
                .execute(&mut *self.connection)
                .await
                .map_err(failed("postpone_job"))?;
                if postponed.rows_affected() == 1 {
                    Ok(job_id)
                } else {
                    self.queue_job(at).await
                }
            }
            JobAction::RetryModel(after) => {
                let step = i64::try_from(MODEL_RETRY_STEP.as_millis()).unwrap_or(i64::MAX);
                // The provider's own wait replaces the growing step.
                let after = after.map(|after| i64::try_from(after.as_millis()).unwrap_or(i64::MAX));
                let retried = sqlx::query(
                    "UPDATE jobs SET \
                       state = CASE WHEN attempts >= max_attempts THEN 'dead' ELSE 'queued' END, \
                       reason = CASE WHEN attempts >= max_attempts THEN 'model_unavailable' ELSE NULL END, \
                       run_at = ? + coalesce(?, ? * max(attempts, 1)), lease_token = NULL, lease_expires_at = NULL \
                     WHERE organization_id = ? AND workspace_id = ? AND environment IS NULL AND id = ? \
                     AND state = 'running'",
                )
                .bind(now)
                .bind(after)
                .bind(step)
                .bind(org)
                .bind(ws)
                .bind(job_id)
                .execute(&mut *self.connection)
                .await
                .map_err(failed("retry_job"))?;
                if retried.rows_affected() == 1 || self.job_waits(job_id).await? {
                    Ok(job_id)
                } else {
                    self.queue_job(Timestamp::from_millis(now.saturating_add(after.unwrap_or(step))))
                        .await
                }
            }
            JobAction::BringForward => {
                sqlx::query(
                    "UPDATE jobs SET run_at = ? WHERE organization_id = ? AND workspace_id = ? \
                     AND environment IS NULL AND id = ? AND state = 'queued' AND run_at > ?",
                )
                .bind(now)
                .bind(org)
                .bind(ws)
                .bind(job_id)
                .bind(now)
                .execute(&mut *self.connection)
                .await
                .map_err(failed("bring_job_forward"))?;
                Ok(job_id)
            }
            JobAction::Release => {
                sqlx::query(
                    "DELETE FROM jobs WHERE organization_id = ? AND workspace_id = ? AND environment IS NULL \
                     AND id = ? AND state IN ('queued', 'running')",
                )
                .bind(org)
                .bind(ws)
                .bind(job_id)
                .execute(&mut *self.connection)
                .await
                .map_err(failed("release_job"))?;
                Ok(job_id)
            }
        }
    }

    async fn job_waits(&mut self, job_id: Uuid) -> Result<bool, StoreError> {
        let (org, ws, _) = self.scope();
        let waiting: Option<i64> = sqlx::query_scalar(
            "SELECT 1 FROM jobs WHERE organization_id = ? AND workspace_id = ? AND environment IS NULL AND id = ? \
             AND state = 'queued'",
        )
        .bind(org)
        .bind(ws)
        .bind(job_id)
        .fetch_optional(&mut *self.connection)
        .await
        .map_err(failed("job_waits"))?;
        Ok(waiting.is_some())
    }
}

/// The latest live send lease of the calls still being sent, leaving out
/// `except`, the call this transition settles.
fn sending_until(snapshot: &Snapshot, except: Option<ToolCallId>) -> Option<Timestamp> {
    in_flight(snapshot)
        .filter(|(call, _)| Some(call.id) != except)
        .map(|(_, attempt)| attempt.send_lease_until)
        .max()
}

/// What an operation did besides the events: the run's row and its job.
struct Effects {
    row: RowChange,
    job: JobAction,
}

impl Effects {
    fn job(job: JobAction) -> Self {
        Self {
            row: RowChange::default(),
            job,
        }
    }
}

/// The agent's name for the audit log: "Agent" once it's gone.
async fn agent_name(
    connection: &mut SqliteConnection,
    organization_id: OrganizationId,
    workspace_id: WorkspaceId,
    agent_id: AgentId,
) -> Result<String, StoreError> {
    let name: Option<String> =
        sqlx::query_scalar("SELECT name FROM agents WHERE organization_id = ? AND workspace_id = ? AND id = ?")
            .bind(organization_id.uuid())
            .bind(workspace_id.uuid())
            .bind(agent_id.uuid())
            .fetch_optional(&mut *connection)
            .await
            .map_err(failed("agent_name"))?;
    Ok(name.unwrap_or_else(|| "Agent".to_owned()))
}

async fn audit(
    writer: &mut Writer<'_>,
    agent_id: AgentId,
    actor: AuditActor,
    action: AuditAction,
) -> Result<(), StoreError> {
    let name = agent_name(writer.connection, writer.organization_id, writer.workspace_id, agent_id).await?;
    let entry = AuditEntry::new(
        actor,
        action,
        AuditTarget::new(AuditObject::Run(writer.run_id), name),
        AuditPlace::Workspace(writer.workspace_id),
    );
    crate::audit::record_at(writer.connection, writer.organization_id, &entry, writer.now)
        .await
        .map_err(failed("audit"))?;
    Ok(())
}

/// A call of an answer must be what the attempt's frozen offer makes of it:
/// the target of the name the model used, or none for a name it wasn't
/// offered, and the digest of exactly that target and its arguments.
fn bound_by_offer(offer: &Offer, call: &NewCall) -> Result<(), StoreError> {
    let offered = offer.find(&call.model_tool_name);
    let fits = match (offered, &call.target) {
        (None, None) => call.call_digest.is_none(),
        (Some(tool), Some(target)) => {
            target.installation_id == tool.installation_id
                && target.installation_short_id == tool.installation_short_id
                && target.installation_label == tool.installation_label
                && target.app_key == tool.app_key
                && target.app_name == tool.app_name
                && target.tool == tool.tool
                && target.definition_sha256 == tool.definition_sha256
                && call.call_digest
                    == Some(call_digest(
                        target.installation_id,
                        &target.app_key,
                        &target.tool,
                        &target.definition_sha256,
                        &call.arguments,
                    ))
        }
        _ => false,
    };
    if fits {
        Ok(())
    } else {
        Err(StoreError::Refused(
            "a call that isn't what the frozen offer makes of it",
        ))
    }
}

/// The call at the run's next position, as the request names it.
fn next_call(snapshot: &Snapshot, call_id: ToolCallId) -> Result<&CallRecord, StoreError> {
    let Continuation::NeedCall { turn, position } = snapshot.control.continuation else {
        return Err(StoreError::Refused("a call step while the run isn't at a call"));
    };
    snapshot
        .call_at(turn, position)
        .filter(|call| call.id == call_id)
        .ok_or(StoreError::Refused("a call step for a call the run isn't at"))
}

/// Where the run goes once `call` is settled: the next open call of its
/// turn, or the next turn.
fn next_after(snapshot: &Snapshot, call: &CallRecord) -> Option<Continuation> {
    let next = snapshot
        .calls
        .iter()
        .filter(|other| other.turn == call.turn && other.id != call.id && other.is_open())
        .map(|other| other.position)
        .min();
    match next {
        Some(position) => Some(Continuation::NeedCall {
            turn: call.turn,
            position,
        }),
        None => call.turn.checked_add(1).map(|turn| Continuation::NeedModel { turn }),
    }
}

/// The continuation a request moves the run to must be the one its
/// operation leads to: a run is never left at a step it can't take.
fn expect_next(asked: Continuation, expected: Option<Continuation>) -> Result<(), StoreError> {
    if Some(asked) == expected {
        Ok(())
    } else {
        Err(StoreError::Refused("a next step the operation doesn't lead to"))
    }
}

fn target_of(call: &CallRecord) -> Result<&BoundTarget, StoreError> {
    call.target
        .as_ref()
        .ok_or(StoreError::Refused("a call step for a call without a target"))
}

/// The worker steps, the sender's, and the control requests on a run that
/// exists. The run's revision, version and guards are already checked.
async fn operate(writer: &mut Writer<'_>, loaded: &Loaded, request: &TransitionRequest) -> Result<Effects, StoreError> {
    let snapshot = &loaded.snapshot;
    let control = &snapshot.control;
    let now = writer.now;
    let refused = StoreError::Refused;
    match request.operation() {
        Operation::Admit(_) => Err(refused("an admission of a run that exists")),
        Operation::Claim(Claim { epoch, starts, .. }) => {
            if Some(*epoch) != control.owner_epoch.checked_add(1) {
                return Err(refused("a claim that isn't the next epoch"));
            }
            match control.continuation {
                Continuation::Done | Continuation::AwaitApproval { .. } => {
                    return Err(refused("a claim of a run no worker takes forward"));
                }
                _ => {}
            }
            if *starts != (control.status == RunStatus::Queued) {
                return Err(refused("a claim that starts a run that isn't queued"));
            }
            if *starts {
                // The model the run was admitted with; `null` IDs only for a run
                // admitted before organizations had models.
                writer.event(
                    RunEventType::RunStarted,
                    json!({
                        "model": snapshot.model,
                        "model_id": snapshot.model_id,
                        "connection_id": snapshot.connection_id,
                    }),
                );
            }
            Ok(Effects {
                row: RowChange {
                    started: *starts,
                    ..RowChange::default()
                },
                job: JobAction::Keep,
            })
        }
        Operation::PrepareModel(PrepareModel {
            turn,
            attempt,
            abandons,
            request_digest,
            offer,
            key_version,
        }) => {
            if key_version.is_some() != snapshot.model_id.is_some() {
                return Err(refused("a model attempt without the key version of the run's model"));
            }
            let fits = match control.continuation {
                Continuation::NeedModel { turn: next } => next == *turn && abandons.is_none(),
                Continuation::AwaitModel {
                    turn: next,
                    attempt: waiting,
                } => next == *turn && *abandons == Some(waiting),
                _ => false,
            };
            let last = snapshot
                .model_attempts
                .iter()
                .filter(|recorded| recorded.turn == *turn)
                .map(|recorded| recorded.attempt)
                .max()
                .unwrap_or(0);
            if !fits || Some(*attempt) != last.checked_add(1) {
                return Err(refused("a model attempt the run isn't ready for"));
            }
            if let Some(abandoned) = abandons {
                writer
                    .set_model_attempt(
                        *turn,
                        *abandoned,
                        ModelAttemptState::Prepared,
                        ModelAttemptState::Abandoned,
                        None,
                    )
                    .await?;
            }
            let (org, ws, run) = writer.scope();
            let offer = serde_json::to_string(offer).map_err(|_| refused("an offer that isn't JSON"))?;
            sqlx::query(
                "INSERT INTO run_model_attempts (id, organization_id, workspace_id, run_id, turn, attempt, state, \
                 request_digest, offer, key_version, created_at, updated_at) \
                 VALUES (?, ?, ?, ?, ?, ?, 'prepared', ?, ?, ?, ?, ?)",
            )
            .bind(Uuid::now_v7())
            .bind(org)
            .bind(ws)
            .bind(run)
            .bind(i64::from(*turn))
            .bind(i64::from(*attempt))
            .bind(request_digest.to_string())
            .bind(offer)
            .bind(key_version.map(i64::from))
            .bind(now.millis())
            .bind(now.millis())
            .execute(&mut *writer.connection)
            .await
            .map_err(failed("prepare_model"))?;
            // The key version the run first used, kept as it started.
            if let Some(key_version) = key_version {
                sqlx::query(
                    "UPDATE runs SET model_key_version = coalesce(model_key_version, ?) \
                     WHERE organization_id = ? AND workspace_id = ? AND id = ?",
                )
                .bind(i64::from(*key_version))
                .bind(org)
                .bind(ws)
                .bind(run)
                .execute(&mut *writer.connection)
                .await
                .map_err(failed("prepare_model"))?;
            }
            Ok(Effects::job(JobAction::Keep))
        }
        Operation::CommitTurn(CommitTurn {
            turn,
            attempt,
            text,
            usage,
            calls,
            next,
            ending,
        }) => {
            if control.continuation
                != (Continuation::AwaitModel {
                    turn: *turn,
                    attempt: *attempt,
                })
                || usize::try_from(*turn).ok() != Some(snapshot.turns.len())
            {
                return Err(refused("an answer to a model attempt the run doesn't wait on"));
            }
            if ending.is_some() && !calls.is_empty() {
                return Err(refused("a final answer with calls"));
            }
            let expected = if ending.is_some() {
                Some(Continuation::Done)
            } else {
                match calls
                    .iter()
                    .filter(|call| call.state != CallState::Refused)
                    .map(|call| call.position)
                    .min()
                {
                    Some(position) => Some(Continuation::NeedCall { turn: *turn, position }),
                    None => turn.checked_add(1).map(|turn| Continuation::NeedModel { turn }),
                }
            };
            expect_next(*next, expected)?;
            let prepared = snapshot
                .model_attempt(*turn, *attempt)
                .ok_or(refused("an answer to a model attempt that isn't recorded"))?;
            for call in calls {
                bound_by_offer(&prepared.offer, call)?;
            }
            writer
                .set_model_attempt(
                    *turn,
                    *attempt,
                    ModelAttemptState::Prepared,
                    ModelAttemptState::Answered,
                    Some((usage.input_tokens, usage.output_tokens)),
                )
                .await?;
            let (org, ws, run) = writer.scope();
            sqlx::query(
                "INSERT INTO run_turns (id, organization_id, workspace_id, run_id, turn, model_attempt, text, \
                 input_tokens, output_tokens, created_at) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
            )
            .bind(Uuid::now_v7())
            .bind(org)
            .bind(ws)
            .bind(run)
            .bind(i64::from(*turn))
            .bind(i64::from(*attempt))
            .bind(text.as_deref())
            .bind(int(usage.input_tokens)?)
            .bind(int(usage.output_tokens)?)
            .bind(now.millis())
            .execute(&mut *writer.connection)
            .await
            .map_err(failed("commit_turn"))?;
            if let Some(text) = text.as_ref().filter(|text| !text.trim().is_empty()) {
                writer.event(RunEventType::MessageAssistant, json!({ "text": text }));
            }
            for call in calls {
                writer.insert_call(*turn, call).await?;
            }
            let row = RowChange {
                ending: ending.clone(),
                add_input_tokens: usage.input_tokens,
                add_output_tokens: usage.output_tokens,
                ..RowChange::default()
            };
            let job = if ending.is_some() {
                ended(sending_until(snapshot, None))
            } else {
                JobAction::Keep
            };
            Ok(Effects { row, job })
        }
        Operation::Park(Park { call_id, decision }) => {
            let call = next_call(snapshot, *call_id)?;
            let target = target_of(call)?;
            match (call.state, decision) {
                (CallState::AwaitingApproval, None) => {}
                (CallState::Allowed, Some(decision))
                    if matches!(decision.verdict, CallVerdict::RequireApproval { .. }) =>
                {
                    writer.decide(*call_id, decision).await?;
                }
                _ => return Err(refused("a park on a call that doesn't wait for approval")),
            }
            let (org, ws, run) = writer.scope();
            sqlx::query(
                "UPDATE run_tool_calls SET state = 'awaiting_approval' \
                 WHERE organization_id = ? AND workspace_id = ? AND run_id = ? AND id = ?",
            )
            .bind(org)
            .bind(ws)
            .bind(run)
            .bind(call_id.uuid())
            .execute(&mut *writer.connection)
            .await
            .map_err(failed("park"))?;
            writer.event(
                RunEventType::ApprovalRequested,
                with(
                    reference(*call_id, target),
                    &[("arguments", Value::Object(call.arguments.clone()))],
                ),
            );
            Ok(Effects::job(JobAction::Release))
        }
        Operation::ResolveApproval(ResolveApproval {
            call_id,
            call_digest,
            decision,
            api_key_id,
            api_key_name,
            outcome,
            next,
            ..
        }) => {
            let waiting = control.status == RunStatus::WaitingForApproval
                && control.continuation == Continuation::AwaitApproval { call_id: *call_id };
            let call = snapshot
                .call(*call_id)
                .filter(|call| waiting && call.state == CallState::AwaitingApproval)
                .ok_or(refused("a decision on a call the run doesn't wait for"))?;
            if call.call_digest != *call_digest {
                return Err(refused("a decision bound to another digest than the call's"));
            }
            expect_next(
                *next,
                match decision {
                    ApprovalDecision::Approved => Some(Continuation::NeedCall {
                        turn: call.turn,
                        position: call.position,
                    }),
                    ApprovalDecision::Denied => next_after(snapshot, call),
                },
            )?;
            let (state, settled) = match (decision, outcome) {
                (ApprovalDecision::Approved, None) => (CallState::Approved, None),
                (ApprovalDecision::Denied, Some(outcome)) => (CallState::Denied, Some(outcome)),
                _ => return Err(refused("a decision without what the model reads back")),
            };
            let (org, ws, run) = writer.scope();
            sqlx::query(
                "UPDATE run_tool_calls SET state = ?, approval = ?, approval_api_key_id = ?, approval_call_digest = ?, \
                 outcome = ?, outcome_is_error = ?, finished_at = ? \
                 WHERE organization_id = ? AND workspace_id = ? AND run_id = ? AND id = ?",
            )
            .bind(state.as_str())
            .bind(decision.as_str())
            .bind(api_key_id.uuid())
            .bind(call_digest.map(|digest| digest.to_string()))
            .bind(settled.map(|outcome| outcome.content.as_str()))
            .bind(settled.map(|outcome| i64::from(outcome.is_error)))
            .bind(settled.map(|_| now.millis()))
            .bind(org)
            .bind(ws)
            .bind(run)
            .bind(call_id.uuid())
            .execute(&mut *writer.connection)
            .await
            .map_err(failed("resolve_approval"))?;
            writer.event(
                RunEventType::ApprovalResolved,
                json!({ "call_id": call_id, "decision": decision.as_str(), "api_key_id": api_key_id }),
            );
            audit(
                writer,
                loaded.agent_id,
                AuditActor::ApiKey {
                    id: *api_key_id,
                    name: api_key_name.clone(),
                },
                AuditAction::ApprovalResolved,
            )
            .await?;
            Ok(Effects::job(JobAction::Queue))
        }
        Operation::RefuseWaiting(RefuseWaiting {
            call_id,
            installation_id,
            outcome,
            failure,
            next,
            ..
        }) => {
            let waiting = control.status == RunStatus::WaitingForApproval
                && control.continuation == Continuation::AwaitApproval { call_id: *call_id };
            let call = snapshot
                .call(*call_id)
                .filter(|call| waiting && call.state == CallState::AwaitingApproval)
                .ok_or(refused("a refusal of a call the run doesn't wait for"))?;
            let target = target_of(call)?;
            if target.installation_id != *installation_id {
                return Err(refused("a refusal for an installation the call isn't bound to"));
            }
            if !installation_went(failure) || !outcome.is_error {
                return Err(refused("a refusal that doesn't say the installation went"));
            }
            expect_next(*next, next_after(snapshot, call))?;
            writer
                .settle(*call_id, CallState::Refused, outcome, Some(failure))
                .await?;
            writer.call_failed(*call_id, target, failure, 0);
            Ok(Effects::job(JobAction::Queue))
        }
        Operation::AuthorizeCall(AuthorizeCall {
            call_id,
            decision,
            authorization,
            next,
        }) => {
            let call = next_call(snapshot, *call_id)?;
            // A call waiting for approval is refused, instead of parked on,
            // only because its installation went away.
            let fits = match (call.state, authorization) {
                (CallState::Allowed | CallState::Approved, _) => true,
                (CallState::AwaitingApproval, Authorization::Refuse { failure, .. }) => installation_went(failure),
                _ => false,
            };
            if !fits {
                return Err(refused("a send of a call that isn't allowed or approved"));
            }
            let target = target_of(call)?;
            writer.decide(*call_id, decision).await?;
            match authorization {
                Authorization::Send {
                    attempt,
                    dispatch_epoch,
                    invocation_id,
                    send_lease_ms,
                } => {
                    let last = call.attempts.iter().map(|recorded| recorded.attempt).max().unwrap_or(0);
                    // The owner guard already holds this epoch.
                    if *dispatch_epoch != control.owner_epoch {
                        return Err(refused("a send claim under another epoch than the owner's"));
                    }
                    if Some(*attempt) != last.checked_add(1) {
                        return Err(refused("a send claim that isn't the call's next attempt"));
                    }
                    expect_next(
                        *next,
                        Some(Continuation::AwaitCall {
                            call_id: *call_id,
                            attempt: *attempt,
                        }),
                    )?;
                    if matches!(decision.verdict, CallVerdict::Deny { .. })
                        || (call.state == CallState::Allowed
                            && matches!(decision.verdict, CallVerdict::RequireApproval { .. }))
                    {
                        return Err(refused("a send the decision doesn't allow"));
                    }
                    let (org, ws, run) = writer.scope();
                    sqlx::query(
                        "UPDATE run_tool_calls SET state = 'started', started_at = ? \
                         WHERE organization_id = ? AND workspace_id = ? AND run_id = ? AND id = ?",
                    )
                    .bind(now.millis())
                    .bind(org)
                    .bind(ws)
                    .bind(run)
                    .bind(call_id.uuid())
                    .execute(&mut *writer.connection)
                    .await
                    .map_err(failed("authorize_call"))?;
                    sqlx::query(
                        "INSERT INTO run_call_attempts (id, organization_id, workspace_id, run_id, call_id, attempt, \
                         dispatch_epoch, invocation_id, authorized_at, send_lease_until, late) \
                         VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, 0)",
                    )
                    .bind(Uuid::now_v7())
                    .bind(org)
                    .bind(ws)
                    .bind(run)
                    .bind(call_id.uuid())
                    .bind(i64::from(*attempt))
                    .bind(int(*dispatch_epoch)?)
                    .bind(invocation_id.uuid())
                    .bind(now.millis())
                    .bind(after_ms(now, *send_lease_ms).millis())
                    .execute(&mut *writer.connection)
                    .await
                    .map_err(failed("authorize_call"))?;
                    writer.event(
                        RunEventType::ToolCallStarted,
                        with(
                            reference(*call_id, target),
                            &[("arguments", Value::Object(call.arguments.clone()))],
                        ),
                    );
                    Ok(Effects {
                        row: RowChange {
                            add_tool_calls: 1,
                            ..RowChange::default()
                        },
                        job: JobAction::Keep,
                    })
                }
                Authorization::Refuse { outcome, failure } => {
                    expect_next(*next, next_after(snapshot, call))?;
                    writer
                        .settle(*call_id, CallState::Refused, outcome, Some(failure))
                        .await?;
                    writer.call_failed(*call_id, target, failure, 0);
                    Ok(Effects::job(JobAction::Keep))
                }
            }
        }
        Operation::RecordOutcome(RecordOutcome {
            call_id,
            attempt,
            outcome,
            settle,
            next,
            ..
        }) => {
            let call = snapshot
                .call(*call_id)
                .ok_or(refused("an outcome for a call this run doesn't have"))?;
            let latest = call.attempts.iter().map(|recorded| recorded.attempt).max() == Some(*attempt);
            let awaited = control.continuation
                == Continuation::AwaitCall {
                    call_id: *call_id,
                    attempt: *attempt,
                };
            match (call.state, settle) {
                (CallState::Started, Some(_)) if latest => {}
                (CallState::Unknown, None) => {}
                _ => return Err(refused("an outcome that doesn't fit the call as committed")),
            }
            match next {
                Some(next) => {
                    if !awaited || control.status.is_terminal() {
                        return Err(refused("an outcome that moves a run that doesn't wait on it"));
                    }
                    expect_next(*next, next_after(snapshot, call))?;
                }
                // A run that waits on this send goes on once it's settled.
                None if awaited && !control.status.is_terminal() && settle.is_some() => {
                    return Err(refused("an outcome that leaves its run waiting on a settled call"));
                }
                None => {}
            }
            let (org, ws, run) = writer.scope();
            let reported = serde_json::to_string(outcome).map_err(|_| refused("an outcome that isn't JSON"))?;
            sqlx::query(
                "UPDATE run_call_attempts SET outcome = ?, outcome_recorded_at = ?, late = ? \
                 WHERE organization_id = ? AND workspace_id = ? AND run_id = ? AND call_id = ? AND attempt = ? \
                 AND outcome IS NULL",
            )
            .bind(reported)
            .bind(now.millis())
            .bind(i64::from(settle.is_none()))
            .bind(org)
            .bind(ws)
            .bind(run)
            .bind(call_id.uuid())
            .bind(i64::from(*attempt))
            .execute(&mut *writer.connection)
            .await
            .map_err(failed("record_outcome"))?;
            let Some(settle) = settle else {
                // Late evidence: the call stays unknown to the model and the API.
                return Ok(Effects::job(JobAction::Keep));
            };
            let target = target_of(call)?;
            let authorized = loaded.authorized.get(&(*call_id, *attempt)).copied().unwrap_or(now);
            let duration = now.millis() - authorized.millis();
            match (settle.state, outcome) {
                (CallState::Completed, SendOutcome::Completed { result }) => {
                    writer.event(
                        RunEventType::ToolCallCompleted,
                        with(
                            reference(*call_id, target),
                            &[
                                ("result", Value::Object(result.clone())),
                                ("duration_ms", json!(duration.max(0))),
                            ],
                        ),
                    );
                }
                (CallState::Failed | CallState::Unknown | CallState::Refused, _) => {
                    let failure = settle
                        .failure
                        .as_ref()
                        .ok_or(refused("a failed call without its failure"))?;
                    writer.call_failed(*call_id, target, failure, duration);
                }
                _ => return Err(refused("a settlement that doesn't fit the outcome")),
            }
            writer
                .settle(*call_id, settle.state, &settle.outcome, settle.failure.as_ref())
                .await?;
            let job = if control.status.is_terminal() {
                ended(sending_until(snapshot, Some(*call_id)))
            } else if next.is_some() {
                JobAction::BringForward
            } else {
                JobAction::Keep
            };
            Ok(Effects::job(job))
        }
        Operation::SettleUnknown(SettleUnknown {
            call_id,
            attempt,
            outcome,
            failure,
            next,
        }) => {
            let call = snapshot
                .call(*call_id)
                .filter(|call| call.state == CallState::Started)
                .ok_or(refused("an unknown outcome for a call that isn't being sent"))?;
            let awaited = control.continuation
                == Continuation::AwaitCall {
                    call_id: *call_id,
                    attempt: *attempt,
                };
            let fits = match next {
                Some(_) => awaited && !control.status.is_terminal() && request.guards().owner.is_some(),
                None => control.status.is_terminal(),
            };
            if !fits {
                return Err(refused("an unknown outcome that doesn't fit the run"));
            }
            if let Some(next) = next {
                expect_next(*next, next_after(snapshot, call))?;
            }
            let target = target_of(call)?;
            let authorized = loaded.authorized.get(&(*call_id, *attempt)).copied().unwrap_or(now);
            writer
                .settle(*call_id, CallState::Unknown, outcome, Some(failure))
                .await?;
            writer.call_failed(*call_id, target, failure, now.millis() - authorized.millis());
            tracing::warn!(event = "runs.call_outcome_unknown", organization_id = %writer.organization_id, workspace_id = %writer.workspace_id, run_id = %writer.run_id, call_id = %call_id);
            let job = if control.status.is_terminal() {
                ended(sending_until(snapshot, Some(*call_id)))
            } else {
                JobAction::Keep
            };
            Ok(Effects::job(job))
        }
        Operation::Cancel(Cancel {
            actor,
            closing,
            abandons,
            ..
        }) => {
            if control.status.is_terminal() {
                return Err(refused("a cancel of a finished run"));
            }
            let closing = (!closing.calls.is_empty()).then_some(closing);
            writer.abandon(snapshot, *abandons).await?;
            writer.close(snapshot, closing).await?;
            writer.event(RunEventType::RunCancelled, json!({ "cancelled_by": actor.name() }));
            audit(writer, loaded.agent_id, actor.clone(), AuditAction::RunCancelled).await?;
            Ok(Effects::job(ended(sending_until(snapshot, None))))
        }
        Operation::Yield(Yield { why }) => {
            let job = match *why {
                YieldReason::StepBudget => {
                    if !control.continuation.needs_claim() {
                        return Err(refused("a yield of a run that doesn't need a worker"));
                    }
                    JobAction::Postpone(now)
                }
                YieldReason::SendInFlight { call_id, attempt } => {
                    if control.continuation != (Continuation::AwaitCall { call_id, attempt }) {
                        return Err(refused("a yield for a send the run doesn't wait on"));
                    }
                    let until = snapshot
                        .call(call_id)
                        .and_then(|call| call.attempt(attempt))
                        .map(|attempt| attempt.send_lease_until)
                        .ok_or(refused("a yield for a send that isn't recorded"))?;
                    JobAction::Postpone(until)
                }
                YieldReason::ModelUnavailable {
                    turn,
                    attempt,
                    timed_out,
                    retry_after_secs,
                } => {
                    if control.continuation != (Continuation::AwaitModel { turn, attempt }) {
                        return Err(refused("a yield for a model attempt the run doesn't wait on"));
                    }
                    writer
                        .set_model_attempt(
                            turn,
                            attempt,
                            ModelAttemptState::Prepared,
                            ModelAttemptState::Unanswered,
                            None,
                        )
                        .await?;
                    if timed_out {
                        writer.mark_timed_out(turn, attempt).await?;
                    }
                    let after = retry_after_secs.map(|secs| Duration::from_secs(u64::from(secs)).min(MAX_RETRY_AFTER));
                    JobAction::RetryModel(after)
                }
            };
            Ok(Effects::job(job))
        }
        Operation::Finish(Finish {
            ending,
            closing,
            abandons,
        }) => {
            if control.status != RunStatus::Running {
                return Err(refused("a finish of a run that isn't running"));
            }
            writer.abandon(snapshot, *abandons).await?;
            writer.close(snapshot, closing.as_ref()).await?;
            Ok(Effects {
                row: RowChange {
                    ending: Some(ending.clone()),
                    ..RowChange::default()
                },
                job: ended(sending_until(snapshot, None)),
            })
        }
        Operation::FailAbandoned(FailAbandoned {
            error,
            closing,
            abandons,
            ..
        }) => {
            if !matches!(control.status, RunStatus::Queued | RunStatus::Running) {
                return Err(refused(
                    "the sweep's failure of a run a job isn't meant to take forward",
                ));
            }
            writer.abandon(snapshot, *abandons).await?;
            writer.close(snapshot, closing.as_ref()).await?;
            Ok(Effects {
                row: RowChange {
                    ending: Some(RunEnding::Failed { error: error.clone() }),
                    ..RowChange::default()
                },
                job: ended(sending_until(snapshot, None)),
            })
        }
    }
}

/// The transition's row: its receipt, as a retry of the same request finds
/// it again.
#[allow(clippy::too_many_arguments)]
async fn record_transition(
    connection: &mut SqliteConnection,
    request: &TransitionRequest,
    to_revision: u64,
    status: RunStatus,
    continuation: Continuation,
    lease_until: Option<Timestamp>,
    first_sequence: u64,
    events: Vec<RunEvent>,
    now: Timestamp,
) -> Result<Receipt, StoreError> {
    sqlx::query(
        "INSERT INTO run_transitions (id, organization_id, workspace_id, run_id, transition_id, from_revision, \
         to_revision, operation, request_digest, status, continuation, lease_until, first_sequence, event_count, \
         committed_at) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
    )
    .bind(Uuid::now_v7())
    .bind(request.organization_id().uuid())
    .bind(request.workspace_id().uuid())
    .bind(request.run_id().uuid())
    .bind(request.id().as_bytes().to_vec())
    .bind(int(request.from_revision())?)
    .bind(int(to_revision)?)
    .bind(request.operation().name())
    .bind(request.digest().to_string())
    .bind(status.as_str())
    .bind(continuation.to_json())
    .bind(lease_until.map(Timestamp::millis))
    .bind(int(first_sequence)?)
    .bind(int(u64::try_from(events.len()).map_err(unreadable)?)?)
    .bind(now.millis())
    .execute(&mut *connection)
    .await
    .map_err(failed("record_transition"))?;
    let run = Box::new(run_in(connection, request).await?);
    Ok(Receipt {
        transition_id: request.id(),
        operation: request.operation().name().to_owned(),
        from_revision: request.from_revision(),
        to_revision,
        status,
        continuation,
        lease_until,
        committed_at: now,
        events,
        run,
    })
}

/// The run a request names, as the API shows it, read on `connection`.
async fn run_in(connection: &mut SqliteConnection, request: &TransitionRequest) -> Result<Run, StoreError> {
    let row = sqlx::query(FIND_RUN)
        .bind(request.organization_id().uuid())
        .bind(request.workspace_id().uuid())
        .bind(request.run_id().uuid())
        .fetch_optional(&mut *connection)
        .await
        .map_err(failed("run_in"))?
        .ok_or_else(|| corrupt("a transition of no run"))?;
    run_from(&row)
}

fn event_from(row: &SqliteRow) -> Result<RunEvent, StoreError> {
    let kind: String = row.try_get("type").map_err(unreadable)?;
    let data: String = row.try_get("data").map_err(unreadable)?;
    Ok(RunEvent {
        id: RunEventId::from_uuid(row.try_get("id").map_err(unreadable)?),
        organization_id: OrganizationId::from_uuid(row.try_get("organization_id").map_err(unreadable)?),
        workspace_id: WorkspaceId::from_uuid(row.try_get("workspace_id").map_err(unreadable)?),
        run_id: RunId::from_uuid(row.try_get("run_id").map_err(unreadable)?),
        sequence: uint(row, "sequence")?,
        kind: RunEventType::parse(&kind).ok_or_else(|| corrupt("an event type"))?,
        data: serde_json::from_str(&data).map_err(unreadable)?,
        created_at: Timestamp::from_millis(row.try_get("created_at").map_err(unreadable)?),
    })
}

/// The receipt of a transition that committed: `None` when none with this
/// ID did; with whether its digest is the request's.
async fn earlier(
    connection: &mut SqliteConnection,
    request: &TransitionRequest,
) -> Result<Option<(bool, Receipt)>, StoreError> {
    let (org, ws, run) = (
        request.organization_id().uuid(),
        request.workspace_id().uuid(),
        request.run_id().uuid(),
    );
    let row = sqlx::query(
        "SELECT operation, request_digest, from_revision, to_revision, status, continuation, lease_until, \
         first_sequence, event_count, committed_at FROM run_transitions \
         WHERE organization_id = ? AND workspace_id = ? AND run_id = ? AND transition_id = ?",
    )
    .bind(org)
    .bind(ws)
    .bind(run)
    .bind(request.id().as_bytes().to_vec())
    .fetch_optional(&mut *connection)
    .await
    .map_err(failed("earlier"))?;
    let Some(row) = row else { return Ok(None) };
    let same = digest(&row, "request_digest")? == Some(request.digest());
    let first = uint(&row, "first_sequence")?;
    let count = uint(&row, "event_count")?;
    let events = sqlx::query(
        "SELECT id, organization_id, workspace_id, run_id, sequence, type, data, created_at FROM run_events \
         WHERE organization_id = ? AND workspace_id = ? AND run_id = ? AND sequence >= ? AND sequence < ? \
         ORDER BY sequence",
    )
    .bind(org)
    .bind(ws)
    .bind(run)
    .bind(int(first)?)
    .bind(int(first.saturating_add(count))?)
    .fetch_all(&mut *connection)
    .await
    .map_err(failed("earlier"))?
    .iter()
    .map(event_from)
    .collect::<Result<Vec<_>, StoreError>>()?;
    let status: String = row.try_get("status").map_err(unreadable)?;
    let continuation: String = row.try_get("continuation").map_err(unreadable)?;
    let run = Box::new(run_in(connection, request).await?);
    Ok(Some((
        same,
        Receipt {
            transition_id: request.id(),
            operation: row.try_get("operation").map_err(unreadable)?,
            from_revision: uint(&row, "from_revision")?,
            to_revision: uint(&row, "to_revision")?,
            status: RunStatus::parse(&status).ok_or_else(|| corrupt("a run status"))?,
            continuation: Continuation::from_json(&continuation).map_err(unreadable)?,
            lease_until: instant(&row, "lease_until")?,
            committed_at: Timestamp::from_millis(row.try_get("committed_at").map_err(unreadable)?),
            events,
            run,
        },
    )))
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
    " FROM runs r LEFT JOIN agents a ON a.organization_id = r.organization_id \
     AND a.workspace_id = r.workspace_id AND a.id = r.agent_id \
     WHERE r.organization_id = ? AND r.workspace_id = ? AND r.id = ?"
);

/// The run as the API shows it.
fn run_from(row: &SqliteRow) -> Result<Run, StoreError> {
    let trigger: String = row.try_get("trigger").map_err(unreadable)?;
    let status: String = row.try_get("status").map_err(unreadable)?;
    let error_code: Option<String> = row.try_get("error_code").map_err(unreadable)?;
    let error_message: Option<String> = row.try_get("error_message").map_err(unreadable)?;
    Ok(Run {
        id: RunId::from_uuid(row.try_get("id").map_err(unreadable)?),
        organization_id: OrganizationId::from_uuid(row.try_get("organization_id").map_err(unreadable)?),
        workspace_id: WorkspaceId::from_uuid(row.try_get("workspace_id").map_err(unreadable)?),
        agent_id: AgentId::from_uuid(row.try_get("agent_id").map_err(unreadable)?),
        agent_name: row.try_get("agent_name").map_err(unreadable)?,
        session_id: SessionId::from_uuid(row.try_get("session_id").map_err(unreadable)?),
        trigger: RunTrigger::parse(&trigger).ok_or_else(|| corrupt("a trigger"))?,
        status: RunStatus::parse(&status).ok_or_else(|| corrupt("a run status"))?,
        input: row.try_get("input").map_err(unreadable)?,
        output: row.try_get("output").map_err(unreadable)?,
        usage: RunUsage {
            input_tokens: uint(row, "input_tokens")?,
            output_tokens: uint(row, "output_tokens")?,
            tool_calls: uint(row, "tool_calls")?,
        },
        error: match (error_code, error_message) {
            (Some(code), Some(message)) => Some(Failure { code, message }),
            _ => None,
        },
        created_at: Timestamp::from_millis(row.try_get("created_at").map_err(unreadable)?),
        started_at: instant(row, "started_at")?,
        finished_at: instant(row, "finished_at")?,
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

fn after(cursor: Option<&str>) -> Result<Option<Position>, StoreError> {
    cursor
        .map(|cursor| Position::decode(cursor).ok_or(StoreError::InvalidCursor))
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

/// A new run: queued, `run.queued`, its first job.
async fn admit(
    connection: &mut SqliteConnection,
    request: &TransitionRequest,
    admit: &Admit,
    now: Timestamp,
) -> Result<Receipt, StoreError> {
    let (organization_id, workspace_id, run_id) = (request.organization_id(), request.workspace_id(), request.run_id());
    if request.from_revision() != 0 {
        return Err(StoreError::Refused("an admission that doesn't start from revision 0"));
    }
    if admit.execution_version != EXECUTION_VERSION {
        return Err(StoreError::Refused(
            "an admission under an execution version this build doesn't advance",
        ));
    }
    let agent: Option<i64> =
        sqlx::query_scalar("SELECT 1 FROM agents WHERE organization_id = ? AND workspace_id = ? AND id = ?")
            .bind(organization_id.uuid())
            .bind(workspace_id.uuid())
            .bind(admit.agent_id.uuid())
            .fetch_optional(&mut *connection)
            .await
            .map_err(failed("admit"))?;
    if agent.is_none() {
        return Err(StoreError::AgentNotFound);
    }
    let mut writer = Writer {
        connection,
        organization_id,
        workspace_id,
        run_id,
        now,
        events: Vec::new(),
    };
    let job_id = writer.queue_job(now).await?;
    let continuation = Continuation::NeedModel { turn: 0 };
    sqlx::query(
        "INSERT INTO runs (id, organization_id, workspace_id, agent_id, session_id, trigger, job_id, status, input, \
         model, model_id, model_connection_id, instructions, input_tokens, output_tokens, tool_calls, created_at, \
         execution_version, revision, continuation) \
         VALUES (?, ?, ?, ?, ?, ?, ?, 'queued', ?, ?, ?, ?, ?, 0, 0, 0, ?, ?, 1, ?)",
    )
    .bind(run_id.uuid())
    .bind(organization_id.uuid())
    .bind(workspace_id.uuid())
    .bind(admit.agent_id.uuid())
    .bind(admit.session_id.uuid())
    .bind(admit.trigger.as_str())
    .bind(job_id)
    .bind(&admit.input)
    .bind(&admit.model)
    .bind(admit.model_id.uuid())
    .bind(admit.connection_id.uuid())
    .bind(&admit.instructions)
    .bind(now.millis())
    .bind(i64::from(admit.execution_version))
    .bind(continuation.to_json())
    .execute(&mut *writer.connection)
    .await
    .map_err(failed("admit"))?;
    writer.event(RunEventType::RunQueued, json!({}));
    let (first, events) = writer.append().await?;
    record_transition(
        writer.connection,
        request,
        1,
        RunStatus::Queued,
        continuation,
        None,
        first,
        events,
        now,
    )
    .await
}

impl SqliteRunStore {
    async fn apply_in(
        &self,
        connection: &mut SqliteConnection,
        request: &TransitionRequest,
    ) -> Result<Applied, StoreError> {
        let (organization_id, workspace_id, run_id) =
            (request.organization_id(), request.workspace_id(), request.run_id());
        if let Some((same, receipt)) = earlier(connection, request).await? {
            return Ok(if same {
                Applied::Committed(receipt)
            } else {
                Applied::Conflict(Conflict::Revision)
            });
        }
        // The time, read once the write lock is held.
        let now = self.clock.now();
        let Some(row) = run_row(connection, organization_id, workspace_id, run_id).await? else {
            return match request.operation() {
                Operation::Admit(operation) => {
                    check_required(request.guards(), request.operation())?;
                    admit(connection, request, operation, now).await.map(Applied::Committed)
                }
                _ => Err(StoreError::NotFound),
            };
        };
        if matches!(request.operation(), Operation::Admit(_)) || row.revision != request.from_revision() {
            return Ok(Applied::Conflict(Conflict::Revision));
        }
        let to_revision = request
            .to_revision()
            .ok_or(StoreError::Refused("the run's revisions are exhausted"))?;
        let loaded = load(connection, organization_id, workspace_id, run_id, row).await?;
        if let Some(conflict) = check_guards(&loaded.snapshot, request.guards(), now) {
            return Ok(Applied::Conflict(conflict));
        }
        check_required(request.guards(), request.operation())?;

        let mut writer = Writer {
            connection,
            organization_id,
            workspace_id,
            run_id,
            now,
            events: Vec::new(),
        };
        let effects = operate(&mut writer, &loaded, request).await?;
        let operation = request.operation();
        let control = &loaded.snapshot.control;
        let status = operation.status_after(control.status);
        let continuation = operation.continuation_after(control.continuation);
        let (owner_epoch, owner_claimant, mut lease_until) = match operation {
            Operation::Claim(claim) => (claim.epoch, Some(claim.claimant), Some(after_ms(now, claim.lease_ms))),
            _ => (
                control.owner_epoch,
                control.owner_claimant,
                match request.guards().owner {
                    Some(owner) => owner.renew_lease_ms.map(|ms| after_ms(now, ms)),
                    None => control.lease_until,
                },
            ),
        };
        if status.is_terminal() {
            lease_until = None;
        }
        let cancel_requested_at = match operation {
            Operation::Cancel(_) => Some(now),
            _ => control.cancel_requested_at,
        };
        let job_id = writer.job(loaded.job_id, effects.job).await?;

        // The run.completed or run.failed event carries the run's totals.
        let row = effects.row;
        let usage = RunUsage {
            input_tokens: loaded.usage.input_tokens.saturating_add(row.add_input_tokens),
            output_tokens: loaded.usage.output_tokens.saturating_add(row.add_output_tokens),
            tool_calls: loaded.usage.tool_calls.saturating_add(row.add_tool_calls),
        };
        let (output, error) = match &row.ending {
            Some(RunEnding::Completed { output }) => {
                writer.event(RunEventType::RunCompleted, json!({ "usage": usage }));
                (Some(output.as_str()), None)
            }
            Some(RunEnding::Failed { error }) => {
                writer.event(RunEventType::RunFailed, json!({ "error": error }));
                (None, Some(error))
            }
            None => (None, None),
        };
        if row.ending.is_some()
            != (status.is_terminal() && !control.status.is_terminal() && !matches!(operation, Operation::Cancel(_)))
        {
            return Err(StoreError::Refused("an ending that doesn't fit the run's status"));
        }
        let (org, ws, run) = writer.scope();
        let updated = sqlx::query(
            "UPDATE runs SET status = ?, continuation = ?, revision = ?, owner_epoch = ?, owner_claimant = ?, \
             lease_until = ?, cancel_requested_at = ?, started_at = coalesce(started_at, ?), \
             finished_at = coalesce(finished_at, ?), output = coalesce(output, ?), \
             error_code = coalesce(error_code, ?), error_message = coalesce(error_message, ?), \
             input_tokens = ?, output_tokens = ?, tool_calls = ?, job_id = ? \
             WHERE organization_id = ? AND workspace_id = ? AND id = ? AND revision = ?",
        )
        .bind(status.as_str())
        .bind(continuation.to_json())
        .bind(int(to_revision)?)
        .bind(int(owner_epoch)?)
        .bind(owner_claimant.map(|claimant| claimant.uuid()))
        .bind(lease_until.map(Timestamp::millis))
        .bind(cancel_requested_at.map(Timestamp::millis))
        .bind(row.started.then_some(now.millis()))
        .bind(status.is_terminal().then_some(now.millis()))
        .bind(output)
        .bind(error.map(|error| error.code.as_str()))
        .bind(error.map(|error| error.message.as_str()))
        .bind(int(usage.input_tokens)?)
        .bind(int(usage.output_tokens)?)
        .bind(int(usage.tool_calls)?)
        .bind(job_id)
        .bind(org)
        .bind(ws)
        .bind(run)
        .bind(int(request.from_revision())?)
        .execute(&mut *writer.connection)
        .await
        .map_err(failed("update_run"))?;
        if updated.rows_affected() != 1 {
            return Err(corrupt("a run that moved under the write lock"));
        }
        let (first, events) = writer.append().await?;
        let receipt = record_transition(
            writer.connection,
            request,
            to_revision,
            status,
            continuation,
            lease_until,
            first,
            events,
            now,
        )
        .await?;
        Ok(Applied::Committed(receipt))
    }
}

#[async_trait]
impl RunStore for SqliteRunStore {
    /// Opened with WAL and `synchronous = FULL` ([`Database::open`]).
    fn durability(&self) -> Durability {
        Durability::Synchronous
    }

    async fn apply(
        &self,
        organization_id: OrganizationId,
        workspace_id: WorkspaceId,
        request: &TransitionRequest,
    ) -> Result<Applied, StoreError> {
        // The request names its own scope; it must be exactly the caller's,
        // and its ID exactly the one that scope, its run and its revision give.
        if request.organization_id() != organization_id
            || request.workspace_id() != workspace_id
            || request.id()
                != TransitionId::derive(organization_id, workspace_id, request.run_id(), request.from_revision())
        {
            return Err(StoreError::NotFound);
        }
        let mut transaction = self
            .database
            .pool()
            .begin_with("BEGIN IMMEDIATE")
            .await
            .map_err(failed("apply"))?;
        let applied = self.apply_in(&mut transaction, request).await?;
        // A conflict, or a receipt found again, wrote nothing.
        transaction.commit().await.map_err(failed("apply"))?;
        Ok(applied)
    }

    async fn find_snapshot(
        &self,
        organization_id: OrganizationId,
        workspace_id: WorkspaceId,
        run_id: RunId,
    ) -> Result<Option<Snapshot>, StoreError> {
        let mut transaction = self.database.pool().begin().await.map_err(failed("find_snapshot"))?;
        let Some(row) = run_row(&mut transaction, organization_id, workspace_id, run_id).await? else {
            return Ok(None);
        };
        let loaded = load(&mut transaction, organization_id, workspace_id, run_id, row).await?;
        transaction.commit().await.map_err(failed("find_snapshot"))?;
        Ok(Some(loaded.snapshot))
    }

    async fn find_run(
        &self,
        organization_id: OrganizationId,
        workspace_id: WorkspaceId,
        run_id: RunId,
    ) -> Result<Option<Run>, StoreError> {
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
    ) -> Result<Page<Run>, StoreError> {
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
            total: u64::try_from(total).map_err(unreadable)?,
        })
    }

    async fn list_organization_runs(
        &self,
        organization_id: OrganizationId,
        filter: &OrganizationRunFilter,
        cursor: Option<&str>,
        limit: u32,
    ) -> Result<Page<OrganizationRun>, StoreError> {
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
            let environment: String = row.try_get("environment").map_err(unreadable)?;
            runs.push(OrganizationRun {
                run: run_from(row)?,
                workspace_name: row.try_get("workspace_name").map_err(unreadable)?,
                workspace_slug: row.try_get("workspace_slug").map_err(unreadable)?,
                environment: Environment::parse(&environment).ok_or_else(|| corrupt("an environment"))?,
            });
        }
        let (items, next_cursor) = paged(runs, limit, |item| Position {
            created_at: item.run.created_at.millis(),
            id: item.run.id.uuid(),
        });
        Ok(Page {
            items,
            next_cursor,
            total: u64::try_from(total).map_err(unreadable)?,
        })
    }

    async fn list_events(
        &self,
        organization_id: OrganizationId,
        workspace_id: WorkspaceId,
        run_id: RunId,
        after: u64,
    ) -> Result<Vec<RunEvent>, StoreError> {
        let mut transaction = self.database.pool().begin().await.map_err(failed("list_events"))?;
        let exists: Option<i64> =
            sqlx::query_scalar("SELECT 1 FROM runs WHERE organization_id = ? AND workspace_id = ? AND id = ?")
                .bind(organization_id.uuid())
                .bind(workspace_id.uuid())
                .bind(run_id.uuid())
                .fetch_optional(&mut *transaction)
                .await
                .map_err(failed("list_events"))?;
        if exists.is_none() {
            return Err(StoreError::NotFound);
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
    ) -> Result<Vec<AgentActivity>, StoreError> {
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
                    agent_id: AgentId::from_uuid(row.try_get("agent_id").map_err(unreadable)?),
                    runs_since: uint(row, "runs_since")?,
                    last_run_at: instant(row, "last_run_at")?,
                })
            })
            .collect()
    }

    async fn usage(
        &self,
        organization_id: OrganizationId,
        environment: Environment,
        since: Timestamp,
    ) -> Result<Vec<UsageDay>, StoreError> {
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
                    day: row.try_get("day").map_err(unreadable)?,
                    runs: uint(row, "runs")?,
                    failed_runs: uint(row, "failed_runs")?,
                    tool_calls: uint(row, "tool_calls")?,
                })
            })
            .collect()
    }

    async fn runs_waiting_on(
        &self,
        organization_id: OrganizationId,
        workspace_id: WorkspaceId,
        installation_id: InstallationId,
        after: Option<RunId>,
        limit: u32,
    ) -> Result<Vec<RunId>, StoreError> {
        let rows: Vec<Uuid> = sqlx::query_scalar(
            "SELECT r.id FROM runs r WHERE r.organization_id = ? AND r.workspace_id = ? \
             AND r.status IN ('running', 'waiting_for_approval') AND r.execution_version >= 1 \
             AND (? IS NULL OR r.id > ?) \
             AND EXISTS (SELECT 1 FROM run_tool_calls c \
               WHERE c.organization_id = r.organization_id AND c.workspace_id = r.workspace_id \
               AND c.run_id = r.id AND c.installation_id = ? AND c.state = 'awaiting_approval') \
             ORDER BY r.id LIMIT ?",
        )
        .bind(organization_id.uuid())
        .bind(workspace_id.uuid())
        .bind(after.map(|run| run.uuid()))
        .bind(after.map(|run| run.uuid()))
        .bind(installation_id.uuid())
        .bind(i64::from(limit.clamp(1, zupflow_core::runs::MAX_PAGE)))
        .fetch_all(self.database.pool())
        .await
        .map_err(failed("runs_waiting_on"))?;
        Ok(rows.into_iter().map(RunId::from_uuid).collect())
    }

    /// **Cross-tenant**, by design: the sweep serves every organization. It
    /// reads `runs` and `jobs` only, matching each run's job within the
    /// run's own organization and workspace, and answers each run's scope.
    async fn abandoned_runs(&self, limit: u32) -> Result<Vec<(OrganizationId, WorkspaceId, RunId)>, StoreError> {
        let rows = sqlx::query(
            "SELECT r.organization_id, r.workspace_id, r.id FROM runs r \
             WHERE r.status IN ('queued', 'running') AND r.execution_version >= 1 AND NOT EXISTS (SELECT 1 FROM jobs j \
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
                    OrganizationId::from_uuid(row.try_get("organization_id").map_err(unreadable)?),
                    WorkspaceId::from_uuid(row.try_get("workspace_id").map_err(unreadable)?),
                    RunId::from_uuid(row.try_get("id").map_err(unreadable)?),
                ))
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use zupflow_conformance::run_store::{Prepare, Stores, SuiteClock, Tenant};
    use zupflow_core::agents::{AgentStore, NewAgent};
    use zupflow_core::runs::NewRun;
    use zupflow_core::runs::reducer::{Facts, admit, advance};
    use zupflow_core::runs::state::ClaimantId;

    use super::*;
    use crate::schema::{Temporary, temporary};
    use crate::{SqliteAgentStore, SqliteAuditStore, SqliteJobQueue};

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
        let clock = Arc::new(SuiteClock::new());
        zupflow_conformance::run_store::run(
            Stores {
                runs: &SqliteRunStore::with_clock(database.clone(), clock.clone()),
                agents: &SqliteAgentStore::new(database.clone()),
                jobs: &SqliteJobQueue::with_clock(database.clone(), clock.clone()),
                audit: &SqliteAuditStore::new(database.clone()),
                clock: &clock,
            },
            prepare(database),
        )
        .await;
    }

    /// A tenant with an agent, and a run of it admitted.
    async fn admitted(database: &Database, store: &SqliteRunStore) -> (Tenant, RunId) {
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
                    model_id: None,
                    instructions: String::new(),
                },
                &|_| zupflow_conformance::audit_store::by_suite_in(tenant.workspace),
            )
            .await
            .unwrap();
        let run = RunId::generate();
        let request = admit(
            org,
            tenant.workspace,
            run,
            &NewRun {
                agent_id: agent.id,
                session_id: zupflow_core::ids::SessionId::generate(),
                trigger: RunTrigger::Console,
                input: "hi".to_owned(),
                model: zupflow_core::model_store::RunModel {
                    model_id: ModelId::generate(),
                    connection_id: ModelConnectionId::generate(),
                    provider_model_id: "m".to_owned(),
                },
                instructions: String::new(),
            },
        );
        assert!(matches!(
            store.apply(org, tenant.workspace, &request).await,
            Ok(Applied::Committed(_))
        ));
        (tenant, run)
    }

    async fn count(database: &Database, table: &str) -> i64 {
        sqlx::query_scalar(sqlx::AssertSqlSafe(format!("SELECT count(*) FROM {table}")))
            .fetch_one(database.pool())
            .await
            .unwrap()
    }

    /// A transition that fails at its very last write leaves nothing of what
    /// it wrote before: not the run, its calls, its events, their
    /// announcements, its audit row nor its job.
    #[tokio::test]
    async fn rolls_back_everything_a_failed_transition_wrote() {
        let Temporary {
            database,
            directory: _directory,
        } = temporary().await;
        let store = SqliteRunStore::new(database.clone());
        let (tenant, run) = admitted(&database, &store).await;
        let snapshot = store
            .find_snapshot(tenant.organization_id, tenant.workspace, run)
            .await
            .unwrap()
            .unwrap();
        let request = zupflow_core::runs::reducer::cancel(
            tenant.organization_id,
            tenant.workspace,
            &snapshot,
            zupflow_core::runs::state::RequestId::from_uuid(Uuid::now_v7()),
            RunTrigger::Api,
            AuditActor::System {
                name: "Test".to_owned(),
            },
        )
        .unwrap();
        let tables = ["runs", "run_events", "run_transitions", "jobs", "audit_events"];
        let mut before = Vec::new();
        for table in tables {
            before.push(count(&database, table).await);
        }
        sqlx::query(
            "CREATE TRIGGER fail_transitions BEFORE INSERT ON run_transitions \
             BEGIN SELECT RAISE(ABORT, 'injected'); END",
        )
        .execute(database.pool())
        .await
        .unwrap();
        assert_eq!(
            store.apply(tenant.organization_id, tenant.workspace, &request).await,
            Err(StoreError::Unavailable)
        );
        let mut after = Vec::new();
        for table in tables {
            after.push(count(&database, table).await);
        }
        assert_eq!(after, before, "no row more or less in {tables:?}");
        let still = store
            .find_snapshot(tenant.organization_id, tenant.workspace, run)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(still, snapshot, "the run as it was");
        sqlx::query("DROP TRIGGER fail_transitions")
            .execute(database.pool())
            .await
            .unwrap();
        assert!(matches!(
            store.apply(tenant.organization_id, tenant.workspace, &request).await,
            Ok(Applied::Committed(_))
        ));
    }

    /// The database itself keeps a run's transitions, turns and attempts in
    /// its workspace, and deleting the workspace takes them.
    #[tokio::test]
    async fn keeps_a_runs_rows_inside_its_workspace() {
        let Temporary {
            database,
            directory: _directory,
        } = temporary().await;
        let store = SqliteRunStore::new(database.clone());
        let (tenant, run) = admitted(&database, &store).await;
        let org = tenant.organization_id;
        let snapshot = store.find_snapshot(org, tenant.workspace, run).await.unwrap().unwrap();
        let mut facts = Facts::at(Timestamp::now());
        facts.claimant = Some(ClaimantId::from_uuid(Uuid::now_v7()));
        let claim = advance(&snapshot, &facts).unwrap();
        assert!(matches!(
            store.apply(org, tenant.workspace, claim.request().unwrap()).await,
            Ok(Applied::Committed(_))
        ));
        for statement in [
            "INSERT INTO run_transitions (id, organization_id, workspace_id, run_id, transition_id, from_revision, \
             to_revision, operation, request_digest, status, continuation, first_sequence, event_count, committed_at) \
             VALUES (?, ?, ?, ?, x'00000000000000000000000000000001', 7, 8, 'claim', \
             'sha256:0000000000000000000000000000000000000000000000000000000000000000', 'running', '{}', 1, 0, 0)",
            "INSERT INTO run_turns (id, organization_id, workspace_id, run_id, turn, model_attempt, input_tokens, \
             output_tokens, created_at) VALUES (?, ?, ?, ?, 5, 1, 0, 0, 0)",
            "INSERT INTO run_model_attempts (id, organization_id, workspace_id, run_id, turn, attempt, state, \
             request_digest, offer, created_at, updated_at) VALUES (?, ?, ?, ?, 5, 1, 'abandoned', \
             'sha256:0000000000000000000000000000000000000000000000000000000000000000', '[]', 0, 0)",
        ] {
            let refused = sqlx::query(statement)
                .bind(Uuid::now_v7())
                .bind(org.uuid())
                .bind(tenant.sibling.uuid())
                .bind(run.uuid())
                .execute(database.pool())
                .await;
            assert!(
                refused.is_err(),
                "a row in another workspace than its run's: {statement}"
            );
        }
        assert!(count(&database, "run_transitions").await >= 2);
        sqlx::query("DELETE FROM workspaces WHERE organization_id = ? AND id = ?")
            .bind(org.uuid())
            .bind(tenant.workspace.uuid())
            .execute(database.pool())
            .await
            .unwrap();
        for table in [
            "runs",
            "run_events",
            "run_transitions",
            "run_turns",
            "run_model_attempts",
            "run_call_attempts",
        ] {
            assert_eq!(count(&database, table).await, 0, "{table} goes with the workspace");
        }
    }

    /// What `durability` declares is what the database is opened with.
    #[tokio::test]
    async fn commits_with_wal_and_full_sync() {
        let Temporary {
            database,
            directory: _directory,
        } = temporary().await;
        let mut connection = database.pool().acquire().await.unwrap();
        let journal: String = sqlx::query_scalar("PRAGMA journal_mode")
            .fetch_one(&mut *connection)
            .await
            .unwrap();
        let synchronous: i64 = sqlx::query_scalar("PRAGMA synchronous")
            .fetch_one(&mut *connection)
            .await
            .unwrap();
        assert_eq!((journal.as_str(), synchronous), ("wal", 2));
        assert_eq!(
            SqliteRunStore::new(database.clone()).durability(),
            Durability::Synchronous
        );
    }

    /// Migrations 0028 and 0029 over a database holding the older engine's
    /// runs: a finished one keeps its rows and reads as before; one that
    /// hadn't finished fails as `run_unavailable`, since no worker takes it
    /// forward any more; the columns only that engine used are gone, with
    /// every reference intact; and the store never advances either run.
    #[tokio::test]
    async fn fails_older_runs_left_unfinished_through_migration_0029() {
        use std::str::FromStr;

        use sqlx::sqlite::SqliteConnectOptions;

        use crate::schema::TemporaryDirectory;

        let directory = TemporaryDirectory::new();
        let earlier = directory.0.join("migrations");
        std::fs::create_dir(&earlier).unwrap();
        let source = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("migrations");
        for entry in std::fs::read_dir(&source).unwrap() {
            let path = entry.unwrap().path();
            let name = path.file_name().unwrap().to_string_lossy().into_owned();
            if name.as_str() < "0028" {
                std::fs::copy(&path, earlier.join(&name)).unwrap();
            }
        }
        let options = SqliteConnectOptions::from_str("sqlite:")
            .unwrap()
            .filename(directory.file())
            .create_if_missing(true)
            .foreign_keys(true);
        let pool = sqlx::SqlitePool::connect_with(options).await.unwrap();
        sqlx::migrate::Migrator::new(earlier.as_path())
            .await
            .unwrap()
            .run(&pool)
            .await
            .unwrap();
        let (org, ws, running, finished, call) = (
            Uuid::now_v7(),
            Uuid::now_v7(),
            Uuid::now_v7(),
            Uuid::now_v7(),
            Uuid::now_v7(),
        );
        for statement in [
            "INSERT INTO organizations (id, name, slug, created_at) VALUES (?1, 'Org', 'org', 0)",
            "INSERT INTO workspaces (id, organization_id, environment, name, slug, created_at) \
             VALUES (?2, ?1, 'sandbox', 'W', 'w', 0)",
            "INSERT INTO runs (id, organization_id, workspace_id, agent_id, session_id, trigger, job_id, status, input, \
             model, instructions, turns, input_tokens, output_tokens, tool_calls, created_at, started_at) \
             VALUES (?3, ?1, ?2, ?6, ?6, 'api', ?6, 'running', 'hi', 'm', '', '[]', 1, 2, 1, 0, 0)",
            "INSERT INTO runs (id, organization_id, workspace_id, agent_id, session_id, trigger, job_id, status, input, \
             output, model, instructions, turns, input_tokens, output_tokens, tool_calls, created_at, started_at, \
             finished_at) VALUES (?4, ?1, ?2, ?6, ?6, 'api', ?6, 'completed', 'hi', 'Done.', 'm', '', '[]', 3, 4, 0, \
             0, 0, 5)",
            "INSERT INTO run_events (id, organization_id, workspace_id, run_id, sequence, type, data, created_at) \
             VALUES (?6, ?1, ?2, ?3, 1, 'run.queued', '{}', 0)",
            "INSERT INTO run_tool_calls (id, organization_id, workspace_id, run_id, turn, position, model_call_id, name, \
             installation_id, installation_label, app_name, tool, arguments, state, approval_requested) \
             VALUES (?5, ?1, ?2, ?3, 0, 0, 'm1', 'create_task', ?6, 'Main', 'Taskboard', 'create_task', '{}', \
             'started', 0)",
            "INSERT INTO run_tool_calls (id, organization_id, workspace_id, run_id, turn, position, model_call_id, name, \
             installation_id, installation_label, app_name, tool, arguments, state, approval_requested) \
             VALUES (?6, ?1, ?2, ?3, 0, 1, 'm2', 'delete_task', ?6, 'Main', 'Taskboard', 'delete_task', '{}', \
             'awaiting_approval', 1)",
        ] {
            sqlx::query(statement)
                .bind(org)
                .bind(ws)
                .bind(running)
                .bind(finished)
                .bind(call)
                .bind(Uuid::now_v7())
                .execute(&pool)
                .await
                .unwrap_or_else(|error| panic!("{statement}: {error}"));
        }
        pool.close().await;

        let database = Database::open(&directory.file()).await.unwrap();
        let (org, ws) = (OrganizationId::from_uuid(org), WorkspaceId::from_uuid(ws));
        let (running, finished) = (RunId::from_uuid(running), RunId::from_uuid(finished));
        let row = sqlx::query(
            "SELECT execution_version, revision, owner_epoch, continuation FROM runs \
             WHERE organization_id = ? AND workspace_id = ? AND id = ?",
        )
        .bind(org.uuid())
        .bind(ws.uuid())
        .bind(running.uuid())
        .fetch_one(database.pool())
        .await
        .unwrap();
        assert_eq!(
            (
                row.get::<i64, _>("execution_version"),
                row.get::<i64, _>("revision"),
                row.get::<i64, _>("owner_epoch"),
                row.get::<Option<String>, _>("continuation"),
            ),
            (0, 0, 0, None),
            "an older run keeps version 0 and no continuation"
        );
        let store = SqliteRunStore::new(database.clone());
        let failed = store.find_run(org, ws, running).await.unwrap().unwrap();
        assert_eq!(failed.status, RunStatus::Failed);
        assert_eq!(failed.error.unwrap().code, "run_unavailable");
        assert!(failed.finished_at.is_some());
        assert_eq!(failed.usage.tool_calls, 1, "its figures kept");
        let done = store.find_run(org, ws, finished).await.unwrap().unwrap();
        assert_eq!(
            (done.status, done.output.as_deref(), done.finished_at),
            (RunStatus::Completed, Some("Done."), Some(Timestamp::from_millis(5))),
            "a finished run is left as it was"
        );
        assert_eq!(store.list_events(org, ws, running, 0).await.unwrap().len(), 1);
        for run in [running, finished] {
            assert_eq!(store.find_snapshot(org, ws, run).await, Err(StoreError::Unsupported));
        }
        assert!(store.abandoned_runs(100).await.unwrap().is_empty());
        for (table, column) in [("runs", "turns"), ("run_tool_calls", "approval_requested")] {
            let left: i64 = sqlx::query_scalar(sqlx::AssertSqlSafe(format!(
                "SELECT count(*) FROM pragma_table_info('{table}') WHERE name = '{column}'"
            )))
            .fetch_one(database.pool())
            .await
            .unwrap();
            assert_eq!(left, 0, "{table}.{column} is gone");
        }
        let calls: Vec<(String, String, String)> = sqlx::query_as(
            "SELECT state, failure_code, outcome FROM run_tool_calls \
             WHERE organization_id = ? AND workspace_id = ? AND run_id = ? ORDER BY position",
        )
        .bind(org.uuid())
        .bind(ws.uuid())
        .bind(running.uuid())
        .fetch_all(database.pool())
        .await
        .unwrap();
        let closed: Vec<(&str, &str)> = calls
            .iter()
            .map(|(state, code, _)| (state.as_str(), code.as_str()))
            .collect();
        assert_eq!(
            closed,
            [("unknown", "outcome_unknown"), ("refused", "run_failed")],
            "its calls are kept and closed: the one sent is unknown, the one waiting refused"
        );
        let violations: i64 = sqlx::query_scalar("SELECT count(*) FROM pragma_foreign_key_check")
            .fetch_one(database.pool())
            .await
            .unwrap();
        assert_eq!(violations, 0);
    }
}
