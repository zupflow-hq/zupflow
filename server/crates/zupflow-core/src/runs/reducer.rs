//! The reducer: `advance(snapshot, facts) -> Plan` (decision 0046).
//!
//! A pure function. The [`Snapshot`] is the run as committed; the [`Facts`]
//! are what the driver brings: the time as it saw it, the claim it holds,
//! fresh IDs, the turn's offer, the model's answer, the policy's decisions
//! and the send's outcome. A [`Plan`] is one transition request and at most
//! one piece of I/O, to do only after that transition commits: call the
//! model, send one call, or nothing. The same snapshot and facts always give
//! the same plan. Nothing here reads a clock, makes an ID or touches I/O.
//!
//! The worker's steps come from [`advance`]. The others have their own pure
//! constructors: [`admit`] (the API), [`resolve_approval`] (an API key),
//! [`cancel`] (the API or the Console), [`fail_abandoned`] (the sweep) and
//! [`refuse_waiting`] (the job an installation's deletion or disconnection
//! queues).
//!
//! | Continuation | Plan |
//! | --- | --- |
//! | any, with the sender's outcome | `RecordOutcome`, fenced by the send claim, whatever the status |
//! | `Done` | idle; or `SettleUnknown` of a call still `started` whose send lease lapsed (its sender died), fenced by the send claim |
//! | `AwaitApproval` | idle: waiting for a person |
//! | any other, no claim held | `Claim` under a fresh claimant, or idle while another lease or a send claim is live |
//! | a call failed for a reconnect | `Finish` failed |
//! | `NeedModel` | `PrepareModel` + call the model; `Finish` past the turn limit; `Yield` past the step budget |
//! | `AwaitModel` with its answer | `CommitTurn`; `Finish` failed; `Yield` when the model is unavailable, `Finish` failed `model_unavailable` on the job's last attempt |
//! | `AwaitModel` without one | `PrepareModel` of a new attempt, the old one abandoned |
//! | `NeedCall` | `Park`, or `AuthorizeCall` refusing a call waiting for approval whose installation went away; `AuthorizeCall` (send or refuse) + send; `Yield` past the step budget |
//! | `AwaitCall`, send lease live | `Yield` until it lapses (the store reads when, from its own records) |
//! | `AwaitCall`, send lease lapsed | `SettleUnknown` |

use serde_json::{Map, Value, json};

use crate::audit::AuditActor;
use crate::ids::{ApiKeyId, InstallationId, OrganizationId, RunId, ToolCallId, WorkspaceId};
use crate::models::{Completion, Message, ModelToolCall, Reply, ToolSpec, Turn, validate_completion, validate_turn};
use crate::runs::digest::{Sha256Digest, call_digest, digest_json};
use crate::runs::names::{Offer, OfferedTool};
use crate::runs::state::{
    BoundTarget, CallAttempt, CallDecision, CallRecord, CallVerdict, ClaimantId, Continuation, EXECUTION_VERSION,
    InvocationId, ModelAttemptState, RequestId, Snapshot, Usage, supports,
};
use crate::runs::transition::{
    Admit, Authorization, AuthorizeCall, Cancel, Claim, Closing, CommitTurn, FailAbandoned, Finish, Guards,
    ModelAttemptRef, NewCall, Operation, OwnerGuard, Park, PrepareModel, RecordOutcome, RefuseWaiting, ResolveApproval,
    RunEnding, SendGuard, SendLease, SendOutcome, SettleUnknown, Settlement, TransitionRequest, Yield, YieldReason,
};
use crate::runs::{ApprovalDecision, CallOutcome, CallState, Failure, NewRun, RunStatus, RunTrigger};
use crate::time::Timestamp;

/// The domain of model request digests.
pub const MODEL_REQUEST_DOMAIN: &str = "zupflow.run.model_request.v1";

/// What the model reads back for a call a person denied without a reason.
pub const DENIED: &str = "A person denied this call, so it wasn't made. Continue without it.";

/// What the model reads back for a call whose outcome is unknown.
pub const UNKNOWN: &str =
    "The call was sent, but no answer came back, so whether it acted is unknown. It wasn't repeated.";

/// The failure code of a call that needs its installation reconnected; a
/// call failing with it fails the run (decision 0047).
pub const RECONNECT_REQUIRED: &str = "installation_reconnect_required";

/// The failure code of a run whose model gave no answer on its job's last
/// attempt.
pub const MODEL_UNAVAILABLE: &str = "model_unavailable";

/// The failure code of a call whose installation was deleted (decision
/// 0047): refused, and the run goes on.
pub const INSTALLATION_REMOVED: &str = "installation_removed";

/// The failure code of a call whose installation was disconnected (decision
/// 0047): refused, and the run goes on.
pub const INSTALLATION_DISCONNECTED: &str = "installation_disconnected";

/// How an installation went away (decision 0047).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Gone {
    /// Deleted: its row is gone.
    Removed,
    /// Its credential was deleted; the row is kept.
    Disconnected,
}

impl Gone {
    /// The code a call to the installation labelled `label` is refused with,
    /// and what the model and the timeline read.
    pub fn refusal(self, label: &str) -> (&'static str, String) {
        match self {
            Self::Removed => (
                INSTALLATION_REMOVED,
                format!("{label} was removed, so this call wasn't made."),
            ),
            Self::Disconnected => (
                INSTALLATION_DISCONNECTED,
                format!("{label} was disconnected, so this call wasn't made."),
            ),
        }
    }
}

impl InstallationCheck {
    /// How the installation went away, if it did.
    pub fn gone(self) -> Option<Gone> {
        match self {
            Self::Removed => Some(Gone::Removed),
            Self::Disconnected => Some(Gone::Disconnected),
            Self::Usable | Self::ReconnectRequired | Self::Unusable => None,
        }
    }
}

/// The limits a worker runs under. Facts, so tests choose them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Limits {
    /// The most turns a run takes before it fails.
    pub max_turns: u32,
    /// The most calls one turn may ask for; the rest are refused.
    pub max_calls_per_turn: u32,
    /// The most steps one claim takes before it yields the run.
    pub max_steps_per_claim: u32,
    /// How long a claim holds, renewed by every step.
    pub lease_ms: u64,
    /// How long a send claim holds: the outbound timeout for a call, a
    /// refresh and one replay, plus a margin.
    pub send_lease_ms: u64,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            max_turns: 10,
            max_calls_per_turn: 20,
            max_steps_per_claim: 200,
            lease_ms: 5 * 60 * 1000,
            send_lease_ms: 60 * 1000,
        }
    }
}

/// The model's answer to one prepared attempt.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModelAnswer {
    pub turn: u32,
    pub attempt: u32,
    pub result: ModelResult,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ModelResult {
    Answered(Completion),
    /// No answer, or not yet: nothing is committed, the job tries later.
    Unavailable,
    /// The provider can't answer this run: it fails with this.
    Failed(Failure),
}

/// An installation as a send finds it, read again however long the run was
/// parked (decision 0047).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InstallationCheck {
    Usable,
    /// Deleted: its row is gone.
    Removed,
    /// Its credential was deleted; the row is kept.
    Disconnected,
    /// Its credential stopped working: the run fails.
    ReconnectRequired,
    /// It can't be reached as it's set up (its app's configuration names no
    /// server for its environment, say): the call is refused, and the run
    /// goes on.
    Unusable,
}

/// A call checked again just before it's sent, or, for a call waiting for
/// approval, just before the run parks on it: then only its installation
/// counts, and `decision` is the one recorded with the call.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SendCheck {
    pub call_id: ToolCallId,
    pub installation: InstallationCheck,
    /// The tool's pinned definition now; `None` when the registry no longer
    /// has the tool.
    pub definition_sha256: Option<Sha256Digest>,
    /// The floor, then the policy, read now.
    pub decision: CallDecision,
}

/// What a send returned, for the attempt it was authorized as.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SendResult {
    pub call_id: ToolCallId,
    pub attempt: u32,
    pub invocation_id: InvocationId,
    pub outcome: SendOutcome,
}

/// What the driver knows besides the snapshot.
#[derive(Debug, Clone, PartialEq)]
pub struct Facts {
    /// The time as the driver read it. The store checks every lease again on
    /// its own clock.
    pub now: Timestamp,
    /// This worker's own identity for a claim, fresh per worker: needed to
    /// claim, and to act under the claim it made.
    pub claimant: Option<ClaimantId>,
    /// The owner epoch this worker holds, if it claimed the run.
    pub claim: Option<u64>,
    /// Steps this claim has taken.
    pub steps: u32,
    pub limits: Limits,
    /// The tools to offer the next model attempt, built by
    /// [`super::names::build_offer`].
    pub offer: Option<Offer>,
    pub model: Option<ModelAnswer>,
    /// The policy's decision on each call of the answer that needs one, by
    /// position ([`calls_to_decide`]).
    pub decisions: Vec<(u32, CallDecision)>,
    /// Fresh IDs for the answer's calls, one per position.
    pub call_ids: Vec<ToolCallId>,
    pub send_check: Option<SendCheck>,
    /// A fresh invocation ID for a send claim.
    pub invocation_id: Option<InvocationId>,
    /// The sender's outcome.
    pub sent: Option<SendResult>,
    /// Whether the job this worker holds is on its last attempt: a model
    /// that gives no answer then fails the run, as it always has, instead of
    /// leaving it for the sweep.
    pub last_attempt: bool,
}

impl Facts {
    /// Facts with nothing but the time and the default limits.
    pub fn at(now: Timestamp) -> Self {
        Self {
            now,
            claimant: None,
            claim: None,
            steps: 0,
            limits: Limits::default(),
            offer: None,
            model: None,
            decisions: Vec::new(),
            call_ids: Vec::new(),
            send_check: None,
            invocation_id: None,
            sent: None,
            last_attempt: false,
        }
    }
}

/// One request to the model.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModelRequest {
    pub turn: u32,
    pub attempt: u32,
    pub model: String,
    pub instructions: String,
    pub messages: Vec<Message>,
    pub tools: Vec<ToolSpec>,
}

impl ModelRequest {
    /// The request as a provider takes it.
    pub fn as_turn(&self) -> Turn<'_> {
        Turn {
            model: &self.model,
            instructions: &self.instructions,
            messages: &self.messages,
            tools: &self.tools,
        }
    }

    /// The SHA-256 of what the model is sent: model, instructions,
    /// conversation and tools.
    pub fn digest(&self) -> Sha256Digest {
        let messages: Vec<Value> = self
            .messages
            .iter()
            .map(|message| match message {
                Message::User { text } => json!({ "role": "user", "text": text }),
                Message::Assistant { text, tool_calls } => json!({
                    "role": "assistant",
                    "text": text,
                    "tool_calls": tool_calls
                        .iter()
                        .map(|call| json!({ "id": call.id, "name": call.name, "arguments": call.arguments }))
                        .collect::<Vec<_>>(),
                }),
                Message::ToolResult {
                    call_id,
                    content,
                    is_error,
                } => json!({ "role": "tool", "call_id": call_id, "content": content, "is_error": is_error }),
            })
            .collect();
        let tools: Vec<Value> = self
            .tools
            .iter()
            .map(
                |tool| json!({ "name": tool.name, "description": tool.description, "input_schema": tool.input_schema }),
            )
            .collect();
        digest_json(
            MODEL_REQUEST_DOMAIN,
            &json!({
                "model": self.model,
                "instructions": self.instructions,
                "messages": messages,
                "tools": tools,
            }),
        )
    }
}

/// One call to send, under its send claim.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SendRequest {
    /// The run's scope: the installation is found in it, and only in it.
    pub organization_id: OrganizationId,
    pub workspace_id: WorkspaceId,
    pub call_id: ToolCallId,
    pub attempt: u32,
    pub invocation_id: InvocationId,
    /// Only this installation, with its own credential: never another.
    pub installation_id: InstallationId,
    /// The original MCP tool name.
    pub tool: String,
    pub arguments: Map<String, Value>,
}

/// The I/O a plan allows once its transition commits.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Io {
    Nothing,
    CallModel(ModelRequest),
    SendCall(SendRequest),
}

/// Why there's nothing to apply now.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Idle {
    /// Completed, failed or cancelled.
    Finished,
    /// Waiting for a person's decision; the decision queues the run again.
    AwaitingApproval,
    /// Another worker's lease, or a call's send claim, holds the run until
    /// then.
    Busy { until: Timestamp },
    /// This worker's claim is gone: stop, write nothing.
    Lost,
    /// The sender's outcome is already recorded.
    Recorded,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Plan {
    /// Apply `request`; once it commits, do `then`.
    Apply {
        /// Boxed: a request is large beside an idle plan.
        request: Box<TransitionRequest>,
        then: Io,
    },
    Idle(Idle),
}

impl Plan {
    pub fn request(&self) -> Option<&TransitionRequest> {
        match self {
            Self::Apply { request, .. } => Some(request.as_ref()),
            Self::Idle(_) => None,
        }
    }

    pub fn io(&self) -> &Io {
        match self {
            Self::Apply { then, .. } => then,
            Self::Idle(_) => &Io::Nothing,
        }
    }
}

/// Why `advance` can't plan. Each is a bug or a missing input of the
/// driver's, or a run this build can't advance; none is a product outcome.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum AdvanceError {
    #[error("the run's execution version {0} isn't one this build advances")]
    UnsupportedVersion(u32),
    #[error("the step needs a fact the driver didn't give: {0}")]
    MissingFact(&'static str),
    #[error("a fact doesn't fit the run: {0}")]
    InvalidFact(&'static str),
    #[error("the run's committed state is inconsistent: {0}")]
    InvalidSnapshot(&'static str),
}

/// Why a control operation can't apply to the run as it is.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum ControlError {
    #[error("the run's execution version {0} isn't one this build advances")]
    UnsupportedVersion(u32),
    /// Completed, failed or cancelled.
    #[error("the run is finished")]
    Finished,
    /// Not waiting for a decision on that call.
    #[error("the run isn't waiting for that call")]
    NotWaiting,
    /// A worker's lease on the run is live.
    #[error("the run is claimed")]
    Claimed,
    /// Waiting for a person: no job is meant to take it forward.
    #[error("the run isn't one a job takes forward")]
    NotAbandoned,
    /// A call's send claim is live: its sender may still answer.
    #[error("a call of the run is being sent")]
    Sending,
    /// The snapshot isn't of the scope the caller acts in.
    #[error("run not found")]
    NotFound,
    #[error("the run's committed state is inconsistent: {0}")]
    InvalidSnapshot(&'static str),
}

/// One more than `n`, or an inconsistent snapshot: stored counters never
/// wrap.
fn next_u32(n: u32) -> Result<u32, AdvanceError> {
    n.checked_add(1)
        .ok_or(AdvanceError::InvalidSnapshot("a counter at its limit"))
}

impl From<AdvanceError> for ControlError {
    fn from(error: AdvanceError) -> Self {
        match error {
            AdvanceError::UnsupportedVersion(version) => Self::UnsupportedVersion(version),
            AdvanceError::MissingFact(why) | AdvanceError::InvalidFact(why) | AdvanceError::InvalidSnapshot(why) => {
                Self::InvalidSnapshot(why)
            }
        }
    }
}

/// The run's next step: one transition and at most one piece of I/O.
pub fn advance(snapshot: &Snapshot, facts: &Facts) -> Result<Plan, AdvanceError> {
    let control = &snapshot.control;
    if !supports(control.execution_version) {
        return Err(AdvanceError::UnsupportedVersion(control.execution_version));
    }
    check_shape(snapshot)?;
    // The sender's outcome is fenced by its send claim, not by the run's
    // owner or status: facts survive a cancel and a takeover.
    if let Some(sent) = &facts.sent {
        return record_outcome(snapshot, sent);
    }
    match control.continuation {
        Continuation::Done => return Ok(settle_lapsed(snapshot, facts.now)),
        Continuation::AwaitApproval { .. } => return Ok(Plan::Idle(Idle::AwaitingApproval)),
        _ => {}
    }
    let live = |until: Option<Timestamp>| until.filter(|until| *until > facts.now);
    let Some(epoch) = facts.claim else {
        if let Some(until) = live(control.lease_until) {
            return Ok(Plan::Idle(Idle::Busy { until }));
        }
        if let Continuation::AwaitCall { call_id, attempt } = control.continuation {
            let sending = snapshot.call(call_id).and_then(|call| call.attempt(attempt));
            if let Some(until) = live(sending.map(|sending| sending.send_lease_until)) {
                return Ok(Plan::Idle(Idle::Busy { until }));
            }
        }
        let claimant = facts.claimant.ok_or(AdvanceError::MissingFact("claimant"))?;
        let claim = Claim {
            epoch: control
                .owner_epoch
                .checked_add(1)
                .ok_or(AdvanceError::InvalidSnapshot("the owner epoch at its limit"))?,
            claimant,
            lease_ms: facts.limits.lease_ms,
            starts: control.status == RunStatus::Queued,
        };
        let guards = Guards {
            not_cancelled: true,
            no_live_claim: true,
            ..Guards::default()
        };
        return Ok(apply(snapshot, guards, Operation::Claim(claim), Io::Nothing));
    };
    let claimant = facts.claimant.ok_or(AdvanceError::MissingFact("claimant"))?;
    if epoch != control.owner_epoch || control.owner_claimant != Some(claimant) || live(control.lease_until).is_none() {
        return Ok(Plan::Idle(Idle::Lost));
    }
    let worker = Worker {
        snapshot,
        facts,
        epoch,
        claimant,
    };
    if let Some(failure) = reconnect_failure(snapshot) {
        return Ok(worker.finish(RunEnding::Failed { error: failure }));
    }
    match control.continuation {
        Continuation::NeedModel { turn } => worker.need_model(turn),
        Continuation::AwaitModel { turn, attempt } => worker.await_model(turn, attempt),
        Continuation::NeedCall { turn, position } => worker.need_call(turn, position),
        Continuation::AwaitCall { call_id, attempt } => worker.await_call(call_id, attempt),
        Continuation::AwaitApproval { .. } | Continuation::Done => Ok(Plan::Idle(Idle::Finished)),
    }
}

/// The status, continuation and turns must agree.
fn check_shape(snapshot: &Snapshot) -> Result<(), AdvanceError> {
    let control = &snapshot.control;
    if control.revision == 0 {
        return Err(AdvanceError::InvalidSnapshot("a run that was never admitted"));
    }
    if control.revision == u64::MAX {
        return Err(AdvanceError::InvalidSnapshot("the run's revisions are exhausted"));
    }
    let fits = match (control.status, control.continuation) {
        (RunStatus::Queued, Continuation::NeedModel { turn: 0 }) => snapshot.turns.is_empty(),
        (RunStatus::Queued, _) => false,
        (RunStatus::Running, continuation) => {
            continuation != Continuation::Done && !matches!(continuation, Continuation::AwaitApproval { .. })
        }
        (RunStatus::WaitingForApproval, continuation) => matches!(continuation, Continuation::AwaitApproval { .. }),
        (_, continuation) => continuation == Continuation::Done,
    };
    if !fits {
        return Err(AdvanceError::InvalidSnapshot(
            "the status and the continuation disagree",
        ));
    }
    if control.cancel_requested_at.is_some() && control.status != RunStatus::Cancelled {
        return Err(AdvanceError::InvalidSnapshot(
            "a cancel was asked for but the run isn't cancelled",
        ));
    }
    let in_order = snapshot
        .turns
        .iter()
        .enumerate()
        .all(|(index, turn)| u32::try_from(index).is_ok_and(|index| index == turn.turn));
    if !in_order {
        return Err(AdvanceError::InvalidSnapshot("turns out of order"));
    }
    Ok(())
}

fn apply(snapshot: &Snapshot, guards: Guards, operation: Operation, then: Io) -> Plan {
    Plan::Apply {
        request: Box::new(TransitionRequest::new(
            snapshot.organization_id,
            snapshot.workspace_id,
            snapshot.run_id,
            snapshot.control.revision,
            guards,
            operation,
        )),
        then,
    }
}

/// The first call, by turn and position, that failed for a reconnect.
fn reconnect_failure(snapshot: &Snapshot) -> Option<Failure> {
    let mut calls: Vec<&CallRecord> = snapshot.calls.iter().collect();
    calls.sort_by_key(|call| (call.turn, call.position));
    calls
        .into_iter()
        .filter_map(|call| call.failure.as_ref())
        .find(|failure| failure.code == RECONNECT_REQUIRED)
        .cloned()
}

/// The prepared model attempt the run is waiting on, if any: abandoned when
/// the run ends before its answer is committed.
fn prepared_attempt(snapshot: &Snapshot) -> Option<ModelAttemptRef> {
    match snapshot.control.continuation {
        Continuation::AwaitModel { turn, attempt } => Some(ModelAttemptRef { turn, attempt }),
        _ => None,
    }
}

/// Every call recorded as sent with its outcome not yet recorded, with its
/// latest send claim, by turn and position.
fn sends_in_flight(snapshot: &Snapshot) -> impl Iterator<Item = (&CallRecord, &CallAttempt)> {
    let mut calls: Vec<&CallRecord> = snapshot
        .calls
        .iter()
        .filter(|call| call.state == CallState::Started)
        .collect();
    calls.sort_by_key(|call| (call.turn, call.position));
    calls.into_iter().filter_map(|call| {
        call.attempts
            .iter()
            .max_by_key(|attempt| attempt.attempt)
            .filter(|attempt| !attempt.outcome_recorded)
            .map(|attempt| (call, attempt))
    })
}

/// A finished run whose sender died mid-send: once the send lease lapses,
/// the call is settled `unknown`, fenced by the send claim alone, and the
/// run stays as it is. Until then, wait.
fn settle_lapsed(snapshot: &Snapshot, now: Timestamp) -> Plan {
    let mut wait: Option<Timestamp> = None;
    for (call, attempt) in sends_in_flight(snapshot) {
        if attempt.send_lease_until > now {
            wait = Some(wait.map_or(attempt.send_lease_until, |until| until.min(attempt.send_lease_until)));
            continue;
        }
        let guards = Guards {
            send: Some(SendGuard {
                call_id: call.id,
                attempt: attempt.attempt,
                dispatch_epoch: attempt.dispatch_epoch,
                invocation_id: attempt.invocation_id,
                lease: SendLease::Lapsed,
            }),
            ..Guards::default()
        };
        let operation = Operation::SettleUnknown(SettleUnknown {
            call_id: call.id,
            attempt: attempt.attempt,
            outcome: refused(UNKNOWN.to_owned()),
            failure: unknown_failure(),
            next: None,
        });
        return apply(snapshot, guards, operation, Io::Nothing);
    }
    Plan::Idle(wait.map_or(Idle::Finished, |until| Idle::Busy { until }))
}

/// Where the run goes once `call` is settled: the next open call of its
/// turn, or the next turn.
fn next_after(snapshot: &Snapshot, call: &CallRecord) -> Result<Continuation, AdvanceError> {
    let next = snapshot
        .calls
        .iter()
        .filter(|other| other.turn == call.turn && other.id != call.id && other.is_open())
        .map(|other| other.position)
        .min();
    Ok(match next {
        Some(position) => Continuation::NeedCall {
            turn: call.turn,
            position,
        },
        None => Continuation::NeedModel {
            turn: next_u32(call.turn)?,
        },
    })
}

fn refused(content: String) -> CallOutcome {
    CallOutcome {
        content,
        is_error: true,
    }
}

/// Every open call, closed with `code` and `message`.
fn closing(snapshot: &Snapshot, code: &str, message: &str) -> Option<Closing> {
    let calls = snapshot.open_calls();
    (!calls.is_empty()).then(|| Closing {
        calls,
        outcome: refused(message.to_owned()),
        failure: Failure::new(code, message),
    })
}

/// The conversation so far, from what was committed. Every call of a
/// committed turn must have its outcome.
fn conversation(snapshot: &Snapshot) -> Result<Vec<Message>, AdvanceError> {
    let mut messages = vec![Message::User {
        text: snapshot.input.clone(),
    }];
    for turn in &snapshot.turns {
        let mut calls: Vec<&CallRecord> = snapshot.calls.iter().filter(|call| call.turn == turn.turn).collect();
        calls.sort_by_key(|call| call.position);
        messages.push(Message::Assistant {
            text: turn.text.clone(),
            tool_calls: calls
                .iter()
                .map(|call| ModelToolCall {
                    id: call.model_call_id.clone(),
                    name: call.model_tool_name.clone(),
                    arguments: call.arguments.clone(),
                })
                .collect(),
        });
        for call in calls {
            let Some(outcome) = &call.outcome else {
                return Err(AdvanceError::InvalidSnapshot(
                    "a call without an outcome before the next turn",
                ));
            };
            messages.push(Message::ToolResult {
                call_id: call.model_call_id.clone(),
                content: outcome.content.clone(),
                is_error: outcome.is_error,
            });
        }
    }
    Ok(messages)
}

fn model_request(snapshot: &Snapshot, turn: u32, attempt: u32, offer: &Offer) -> Result<ModelRequest, AdvanceError> {
    Ok(ModelRequest {
        turn,
        attempt,
        model: snapshot.model.clone(),
        instructions: snapshot.instructions.clone(),
        messages: conversation(snapshot)?,
        tools: offer.specs(),
    })
}

fn bound(tool: &OfferedTool) -> BoundTarget {
    BoundTarget {
        installation_id: tool.installation_id,
        installation_short_id: tool.installation_short_id.clone(),
        installation_label: tool.installation_label.clone(),
        app_key: tool.app_key.clone(),
        app_name: tool.app_name.clone(),
        tool: tool.tool.clone(),
        definition_sha256: tool.definition_sha256,
    }
}

fn position(index: usize) -> Result<u32, AdvanceError> {
    u32::try_from(index).map_err(|_| AdvanceError::InvalidFact("more calls than a turn can hold"))
}

/// The calls of `completion` that need a policy decision, by position, with
/// the offered tool each names: those whose name is in the prepared
/// attempt's frozen offer, within the turn's call limit. The driver decides
/// each (the floor, then the policy) and passes the decisions as
/// [`Facts::decisions`].
pub fn calls_to_decide<'a>(
    snapshot: &'a Snapshot,
    completion: &Completion,
    limits: &Limits,
) -> Result<Vec<(u32, &'a OfferedTool)>, AdvanceError> {
    let Continuation::AwaitModel { turn, attempt } = snapshot.control.continuation else {
        return Err(AdvanceError::InvalidFact("no model attempt is prepared"));
    };
    let prepared = snapshot
        .model_attempt(turn, attempt)
        .ok_or(AdvanceError::InvalidSnapshot(
            "the prepared model attempt isn't recorded",
        ))?;
    let Reply::ToolCalls { calls, .. } = &completion.reply else {
        return Ok(Vec::new());
    };
    let mut wanted = Vec::new();
    for (index, call) in calls.iter().enumerate() {
        let position = position(index)?;
        if position >= limits.max_calls_per_turn {
            break;
        }
        if let Some(tool) = prepared.offer.find(&call.name) {
            wanted.push((position, tool));
        }
    }
    Ok(wanted)
}

/// How a sender's outcome settles a call.
fn settlement(outcome: &SendOutcome, label: &str) -> Settlement {
    let failed = |code: &str, message: &str| Settlement {
        state: CallState::Failed,
        outcome: refused(message.to_owned()),
        failure: Some(Failure::new(code, message)),
    };
    match outcome {
        SendOutcome::Completed { result } => Settlement {
            state: CallState::Completed,
            outcome: CallOutcome {
                content: Value::Object(result.clone()).to_string(),
                is_error: false,
            },
            failure: None,
        },
        // The tool's own report is for the model; the timeline says what happened.
        SendOutcome::ToolError { result } => Settlement {
            state: CallState::Failed,
            outcome: refused(Value::Object(result.clone()).to_string()),
            failure: Some(Failure::new("tool_error", "The tool reported an error.")),
        },
        SendOutcome::NotSent => failed(
            "tool_unreachable",
            "The app's server couldn't be reached, so the call wasn't made.",
        ),
        SendOutcome::Rejected => failed("tool_rejected", "The app's server refused the call without acting."),
        SendOutcome::Withheld => failed(
            "result_withheld",
            "The app's answer repeated the installation's credential, so it was withheld.",
        ),
        SendOutcome::Unknown => unknown(),
        SendOutcome::Unauthorized => failed(
            RECONNECT_REQUIRED,
            &format!("{label} needs to be reconnected before its tools can run."),
        ),
    }
}

fn unknown_failure() -> Failure {
    Failure::new(
        "outcome_unknown",
        "The call was sent, but its outcome is unknown. It wasn't repeated.",
    )
}

fn unknown() -> Settlement {
    Settlement {
        state: CallState::Unknown,
        outcome: refused(UNKNOWN.to_owned()),
        failure: Some(unknown_failure()),
    }
}

/// The sender's outcome, fenced by its send claim.
fn record_outcome(snapshot: &Snapshot, sent: &SendResult) -> Result<Plan, AdvanceError> {
    let call = snapshot
        .call(sent.call_id)
        .ok_or(AdvanceError::InvalidFact("an outcome for a call this run doesn't have"))?;
    let attempt = call
        .attempt(sent.attempt)
        .filter(|attempt| attempt.invocation_id == sent.invocation_id)
        .ok_or(AdvanceError::InvalidFact(
            "an outcome for a send claim this call doesn't have",
        ))?;
    if attempt.outcome_recorded {
        return Ok(Plan::Idle(Idle::Recorded));
    }
    let latest = call.attempts.iter().map(|attempt| attempt.attempt).max() == Some(sent.attempt);
    let (settle, next) = match call.state {
        CallState::Started if latest => {
            let label = call
                .target
                .as_ref()
                .map_or("The installation", |target| target.installation_label.as_str());
            let awaited = snapshot.control.continuation
                == Continuation::AwaitCall {
                    call_id: call.id,
                    attempt: sent.attempt,
                };
            let next = if awaited && !snapshot.control.status.is_terminal() {
                Some(next_after(snapshot, call)?)
            } else {
                None
            };
            (Some(settlement(&sent.outcome, label)), next)
        }
        // Settled unknown after its send lease lapsed: the call stays
        // unknown, and this is kept as late evidence for operators.
        CallState::Unknown => (None, None),
        _ => {
            return Err(AdvanceError::InvalidSnapshot(
                "an unrecorded send claim on a call that isn't being sent",
            ));
        }
    };
    let guards = Guards {
        send: Some(SendGuard {
            call_id: call.id,
            attempt: attempt.attempt,
            dispatch_epoch: attempt.dispatch_epoch,
            invocation_id: attempt.invocation_id,
            lease: SendLease::Any,
        }),
        ..Guards::default()
    };
    let operation = Operation::RecordOutcome(RecordOutcome {
        call_id: call.id,
        attempt: sent.attempt,
        invocation_id: sent.invocation_id,
        outcome: sent.outcome.clone(),
        settle,
        next,
    });
    Ok(apply(snapshot, guards, operation, Io::Nothing))
}

/// A claimed worker's steps.
struct Worker<'a> {
    snapshot: &'a Snapshot,
    facts: &'a Facts,
    epoch: u64,
    claimant: ClaimantId,
}

impl Worker<'_> {
    /// Guards of a step that keeps the claim, renewing its lease.
    fn keeps(&self) -> Guards {
        Guards {
            owner: Some(OwnerGuard {
                epoch: self.epoch,
                claimant: self.claimant,
                renew_lease_ms: Some(self.facts.limits.lease_ms),
            }),
            not_cancelled: true,
            ..Guards::default()
        }
    }

    /// Guards of a step that releases the claim.
    fn releases(&self) -> Guards {
        Guards {
            owner: Some(OwnerGuard {
                epoch: self.epoch,
                claimant: self.claimant,
                renew_lease_ms: None,
            }),
            not_cancelled: true,
            ..Guards::default()
        }
    }

    fn apply(&self, guards: Guards, operation: Operation, then: Io) -> Plan {
        apply(self.snapshot, guards, operation, then)
    }

    fn finish(&self, ending: RunEnding) -> Plan {
        let closing = closing(self.snapshot, "run_failed", "The run ended before this call was made.");
        self.apply(
            self.releases(),
            Operation::Finish(Finish {
                ending,
                closing,
                abandons: prepared_attempt(self.snapshot),
            }),
            Io::Nothing,
        )
    }

    fn fail(&self, code: &str, message: impl Into<String>) -> Plan {
        self.finish(RunEnding::Failed {
            error: Failure::new(code, message),
        })
    }

    fn over_budget(&self) -> Option<Plan> {
        (self.facts.steps >= self.facts.limits.max_steps_per_claim).then(|| {
            self.apply(
                self.releases(),
                Operation::Yield(Yield {
                    why: YieldReason::StepBudget,
                }),
                Io::Nothing,
            )
        })
    }

    fn turns(&self) -> u32 {
        // Turns are bounded by the turn limit, far below u32::MAX.
        u32::try_from(self.snapshot.turns.len()).unwrap_or(u32::MAX)
    }

    fn need_model(&self, turn: u32) -> Result<Plan, AdvanceError> {
        if turn != self.turns() {
            return Err(AdvanceError::InvalidSnapshot(
                "the next turn isn't the one after the last",
            ));
        }
        if self.snapshot.calls.iter().any(|call| !call.state.is_settled()) {
            return Err(AdvanceError::InvalidSnapshot(
                "a call left unsettled before the next turn",
            ));
        }
        if self
            .snapshot
            .model_attempts
            .iter()
            .any(|attempt| attempt.turn == turn && attempt.state == ModelAttemptState::Prepared)
        {
            return Err(AdvanceError::InvalidSnapshot(
                "a prepared model attempt outside `AwaitModel`",
            ));
        }
        if let Some(plan) = self.over_budget() {
            return Ok(plan);
        }
        let limit = self.facts.limits.max_turns;
        if turn >= limit {
            return Ok(self.fail(
                "too_many_turns",
                format!("The agent took {limit} turns without finishing. Start a run with a narrower request."),
            ));
        }
        self.prepare(turn, None)
    }

    fn prepare(&self, turn: u32, abandons: Option<u32>) -> Result<Plan, AdvanceError> {
        let offer = self.facts.offer.as_ref().ok_or(AdvanceError::MissingFact("offer"))?;
        let attempt = next_u32(
            self.snapshot
                .model_attempts
                .iter()
                .filter(|attempt| attempt.turn == turn)
                .map(|attempt| attempt.attempt)
                .max()
                .unwrap_or(0),
        )?;
        let request = model_request(self.snapshot, turn, attempt, offer)?;
        validate_turn(&request.as_turn()).map_err(|_| AdvanceError::InvalidFact("a model request no model takes"))?;
        let operation = Operation::PrepareModel(PrepareModel {
            turn,
            attempt,
            abandons,
            request_digest: request.digest(),
            offer: offer.clone(),
        });
        Ok(self.apply(self.keeps(), operation, Io::CallModel(request)))
    }

    fn await_model(&self, turn: u32, attempt: u32) -> Result<Plan, AdvanceError> {
        if turn != self.turns() {
            return Err(AdvanceError::InvalidSnapshot(
                "the awaited turn isn't the one after the last",
            ));
        }
        let prepared = self
            .snapshot
            .model_attempt(turn, attempt)
            .filter(|prepared| prepared.state == ModelAttemptState::Prepared)
            .ok_or(AdvanceError::InvalidSnapshot(
                "the awaited model attempt isn't prepared",
            ))?;
        let Some(answer) = &self.facts.model else {
            // Its worker stopped before committing an answer: ask again,
            // under a new attempt, and mark this one abandoned.
            return self.prepare(turn, Some(attempt));
        };
        if (answer.turn, answer.attempt) != (turn, attempt) {
            return Err(AdvanceError::InvalidFact("an answer to another model attempt"));
        }
        let request = model_request(self.snapshot, turn, attempt, &prepared.offer)?;
        if request.digest() != prepared.request_digest {
            return Err(AdvanceError::InvalidSnapshot(
                "the conversation changed under a prepared model attempt",
            ));
        }
        let completion = match &answer.result {
            // On the job's last attempt the run fails now, as it always has.
            ModelResult::Unavailable if self.facts.last_attempt => {
                return Ok(self.fail(MODEL_UNAVAILABLE, "The model provider did not respond in time."));
            }
            // Release the claim at once, so the retry doesn't wait out the
            // lease, and record that nothing answered.
            ModelResult::Unavailable => {
                return Ok(self.apply(
                    self.releases(),
                    Operation::Yield(Yield {
                        why: YieldReason::ModelUnavailable { turn, attempt },
                    }),
                    Io::Nothing,
                ));
            }
            ModelResult::Failed(failure) => {
                return Ok(self.finish(RunEnding::Failed { error: failure.clone() }));
            }
            ModelResult::Answered(completion) => completion,
        };
        if validate_completion(&request.as_turn(), completion).is_err() {
            return Ok(self.fail(
                "model_failed",
                "The model provider couldn't answer this run. Start it again.",
            ));
        }
        let usage = Usage::from(completion.usage);
        let commit = match &completion.reply {
            Reply::Text(text) => CommitTurn {
                turn,
                attempt,
                text: Some(text.clone()),
                usage,
                calls: Vec::new(),
                next: Continuation::Done,
                ending: Some(RunEnding::Completed { output: text.clone() }),
            },
            Reply::ToolCalls { text, calls } => {
                let calls = self.new_calls(&prepared.offer, calls)?;
                let next = match calls.iter().find(|call| call.state != CallState::Refused) {
                    Some(call) => Continuation::NeedCall {
                        turn,
                        position: call.position,
                    },
                    None => Continuation::NeedModel { turn: next_u32(turn)? },
                };
                CommitTurn {
                    turn,
                    attempt,
                    text: text.clone(),
                    usage,
                    calls,
                    next,
                    ending: None,
                }
            }
        };
        let guards = if commit.ending.is_some() {
            self.releases()
        } else {
            self.keeps()
        };
        Ok(self.apply(guards, Operation::CommitTurn(commit), Io::Nothing))
    }

    /// Each call of the answer, looked up in the frozen offer and decided.
    fn new_calls(&self, offer: &Offer, asked: &[ModelToolCall]) -> Result<Vec<NewCall>, AdvanceError> {
        let limit = self.facts.limits.max_calls_per_turn;
        if self.facts.call_ids.len() < asked.len() {
            return Err(AdvanceError::MissingFact("call_ids"));
        }
        let ids = &self.facts.call_ids[..asked.len()];
        let fresh = ids
            .iter()
            .enumerate()
            .all(|(index, id)| !ids[..index].contains(id) && self.snapshot.call(*id).is_none());
        if !fresh {
            return Err(AdvanceError::InvalidFact("a call ID given twice or already used"));
        }
        let mut calls = Vec::with_capacity(asked.len());
        for (index, (call, id)) in asked.iter().zip(ids).enumerate() {
            let position = position(index)?;
            let mut new = NewCall {
                id: *id,
                position,
                model_call_id: call.id.clone(),
                model_tool_name: call.name.clone(),
                target: None,
                arguments: call.arguments.clone(),
                state: CallState::Refused,
                decision: None,
                call_digest: None,
                outcome: None,
                failure: None,
            };
            // Looked up, never parsed: a name not in the offer has no target.
            let Some(tool) = offer.find(&call.name) else {
                new.outcome = Some(refused(format!(
                    "There's no tool named {} available to you.",
                    call.name
                )));
                calls.push(new);
                continue;
            };
            new.call_digest = Some(call_digest(
                tool.installation_id,
                &tool.app_key,
                &tool.tool,
                &tool.definition_sha256,
                &call.arguments,
            ));
            new.target = Some(bound(tool));
            if position >= limit {
                let reason = format!("Only {limit} calls can be made in one turn.");
                new.failure = Some(Failure::new("tool_refused", reason.clone()));
                new.outcome = Some(refused(reason));
                calls.push(new);
                continue;
            }
            let decision = self
                .facts
                .decisions
                .iter()
                .find(|(decided, _)| *decided == position)
                .map(|(_, decision)| decision.clone())
                .ok_or(AdvanceError::MissingFact("decisions"))?;
            match &decision.verdict {
                CallVerdict::Allow => new.state = CallState::Allowed,
                CallVerdict::RequireApproval { .. } => new.state = CallState::AwaitingApproval,
                CallVerdict::Deny { reason } => {
                    new.failure = Some(Failure::new("tool_refused", reason.clone()));
                    new.outcome = Some(refused(reason.clone()));
                }
            }
            new.decision = Some(decision);
            calls.push(new);
        }
        Ok(calls)
    }

    fn need_call(&self, turn: u32, position: u32) -> Result<Plan, AdvanceError> {
        if next_u32(turn)? != self.turns() {
            return Err(AdvanceError::InvalidSnapshot("the next call isn't of the last turn"));
        }
        let call = self
            .snapshot
            .call_at(turn, position)
            .ok_or(AdvanceError::InvalidSnapshot("the next call isn't recorded"))?;
        if let Some(plan) = self.over_budget() {
            return Ok(plan);
        }
        match call.state {
            CallState::AwaitingApproval => self.park_waiting(call),
            CallState::Allowed | CallState::Approved => self.authorize(call),
            _ => Err(AdvanceError::InvalidSnapshot("the next call isn't open")),
        }
    }

    /// The send check for `call`, which the driver must give.
    fn send_check(&self, call: &CallRecord) -> Result<&SendCheck, AdvanceError> {
        let check = self
            .facts
            .send_check
            .as_ref()
            .ok_or(AdvanceError::MissingFact("send_check"))?;
        if check.call_id != call.id {
            return Err(AdvanceError::InvalidFact("a send check for another call"));
        }
        Ok(check)
    }

    /// Refuses `call` at this step, never sent, with `code` and `message`.
    fn refuse(
        &self,
        call: &CallRecord,
        decision: &CallDecision,
        code: &str,
        message: String,
    ) -> Result<Plan, AdvanceError> {
        let operation = Operation::AuthorizeCall(AuthorizeCall {
            call_id: call.id,
            decision: decision.clone(),
            authorization: Authorization::Refuse {
                outcome: refused(message.clone()),
                failure: Failure::new(code, message),
            },
            next: next_after(self.snapshot, call)?,
        });
        Ok(self.apply(self.keeps(), operation, Io::Nothing))
    }

    /// Parks the run on a call waiting for approval, unless its installation
    /// went away (decision 0047): then nobody is asked, and the call is
    /// refused with why. Checked here as well as by the job the deletion or
    /// disconnection queued, which only finds runs already parked.
    fn park_waiting(&self, call: &CallRecord) -> Result<Plan, AdvanceError> {
        let target = call
            .target
            .as_ref()
            .ok_or(AdvanceError::InvalidSnapshot("an open call without a target"))?;
        let check = self.send_check(call)?;
        match check.installation.gone() {
            Some(gone) => {
                let (code, message) = gone.refusal(&target.installation_label);
                self.refuse(call, &check.decision, code, message)
            }
            None => Ok(self.park(call, None)),
        }
    }

    fn park(&self, call: &CallRecord, decision: Option<CallDecision>) -> Plan {
        self.apply(
            self.releases(),
            Operation::Park(Park {
                call_id: call.id,
                decision,
            }),
            Io::Nothing,
        )
    }

    fn authorize(&self, call: &CallRecord) -> Result<Plan, AdvanceError> {
        let target = call
            .target
            .as_ref()
            .ok_or(AdvanceError::InvalidSnapshot("an open call without a target"))?;
        let check = self.send_check(call)?;
        let label = &target.installation_label;
        let refuse = |code: &str, message: String| self.refuse(call, &check.decision, code, message);
        if let Some(gone) = check.installation.gone() {
            let (code, message) = gone.refusal(label);
            return refuse(code, message);
        }
        match check.installation {
            InstallationCheck::Usable | InstallationCheck::Removed | InstallationCheck::Disconnected => {}
            InstallationCheck::Unusable => {
                return refuse(
                    "tool_refused",
                    format!("{label} can't be used as it's set up: check its app's configuration, or reconnect it."),
                );
            }
            // The call is refused; the next step fails the run.
            InstallationCheck::ReconnectRequired => {
                return refuse(
                    RECONNECT_REQUIRED,
                    format!("{label} needs to be reconnected before its tools can run."),
                );
            }
        }
        if let CallVerdict::Deny { reason } = &check.decision.verdict {
            return refuse("tool_refused", reason.clone());
        }
        // The digest binds the call, and any approval of it, to the pinned
        // definition it was decided against.
        let still = check.definition_sha256.map(|definition| {
            call_digest(
                target.installation_id,
                &target.app_key,
                &target.tool,
                &definition,
                &call.arguments,
            )
        });
        if still.is_none() || still != call.call_digest {
            return refuse(
                "tool_changed",
                format!(
                    "{}'s definition changed since this call was decided, so it wasn't made.",
                    target.tool
                ),
            );
        }
        // A call allowed then may need approval now. A person's approval
        // stands for what needed it.
        if matches!(check.decision.verdict, CallVerdict::RequireApproval { .. }) && call.state == CallState::Allowed {
            return Ok(self.park(call, Some(check.decision.clone())));
        }
        let invocation_id = self
            .facts
            .invocation_id
            .ok_or(AdvanceError::MissingFact("invocation_id"))?;
        let attempt = next_u32(call.attempts.iter().map(|attempt| attempt.attempt).max().unwrap_or(0))?;
        let operation = Operation::AuthorizeCall(AuthorizeCall {
            call_id: call.id,
            decision: check.decision.clone(),
            authorization: Authorization::Send {
                attempt,
                dispatch_epoch: self.epoch,
                invocation_id,
                send_lease_ms: self.facts.limits.send_lease_ms,
            },
            next: Continuation::AwaitCall {
                call_id: call.id,
                attempt,
            },
        });
        let send = SendRequest {
            organization_id: self.snapshot.organization_id,
            workspace_id: self.snapshot.workspace_id,
            call_id: call.id,
            attempt,
            invocation_id,
            installation_id: target.installation_id,
            tool: target.tool.clone(),
            arguments: call.arguments.clone(),
        };
        Ok(self.apply(self.keeps(), operation, Io::SendCall(send)))
    }

    fn await_call(&self, call_id: ToolCallId, attempt: u32) -> Result<Plan, AdvanceError> {
        let call = self
            .snapshot
            .call(call_id)
            .filter(|call| call.state == CallState::Started)
            .ok_or(AdvanceError::InvalidSnapshot("the awaited call isn't being sent"))?;
        let sending = call
            .attempt(attempt)
            .ok_or(AdvanceError::InvalidSnapshot("the awaited send claim isn't recorded"))?;
        if sending.send_lease_until > self.facts.now {
            // Never settled unknown while its sender may still answer.
            return Ok(self.apply(
                self.releases(),
                Operation::Yield(Yield {
                    why: YieldReason::SendInFlight { call_id, attempt },
                }),
                Io::Nothing,
            ));
        }
        let guards = Guards {
            send: Some(SendGuard {
                call_id,
                attempt,
                dispatch_epoch: sending.dispatch_epoch,
                invocation_id: sending.invocation_id,
                lease: SendLease::Lapsed,
            }),
            ..self.keeps()
        };
        let operation = Operation::SettleUnknown(SettleUnknown {
            call_id,
            attempt,
            outcome: refused(UNKNOWN.to_owned()),
            failure: unknown_failure(),
            next: Some(next_after(self.snapshot, call)?),
        });
        Ok(self.apply(guards, operation, Io::Nothing))
    }
}

/// The API's admission of a new run: queued, `run.queued`, its first job.
pub fn admit(
    organization_id: OrganizationId,
    workspace_id: WorkspaceId,
    run_id: RunId,
    run: &NewRun,
) -> TransitionRequest {
    TransitionRequest::new(
        organization_id,
        workspace_id,
        run_id,
        0,
        Guards::default(),
        Operation::Admit(Admit {
            agent_id: run.agent_id,
            session_id: run.session_id,
            trigger: run.trigger,
            input: run.input.clone(),
            model: run.model.clone(),
            instructions: run.instructions.clone(),
            execution_version: EXECUTION_VERSION,
        }),
    )
}

/// The snapshot, as long as it is of the scope the caller acts in.
fn in_scope(
    organization_id: OrganizationId,
    workspace_id: WorkspaceId,
    snapshot: &Snapshot,
) -> Result<&Snapshot, ControlError> {
    if snapshot.organization_id == organization_id && snapshot.workspace_id == workspace_id {
        if snapshot.control.revision == u64::MAX {
            return Err(ControlError::InvalidSnapshot("the run's revisions are exhausted"));
        }
        Ok(snapshot)
    } else {
        Err(ControlError::NotFound)
    }
}

/// A person's decision on a call, as an API key sends it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Approval<'a> {
    pub call_id: ToolCallId,
    pub decision: ApprovalDecision,
    pub api_key_id: ApiKeyId,
    /// The key's name, as the audit log records it.
    pub api_key_name: &'a str,
    /// For a denial, what the model reads back.
    pub reason: Option<&'a str>,
}

fn request(snapshot: &Snapshot, guards: Guards, operation: Operation) -> TransitionRequest {
    TransitionRequest::new(
        snapshot.organization_id,
        snapshot.workspace_id,
        snapshot.run_id,
        snapshot.control.revision,
        guards,
        operation,
    )
}

/// An API key's decision on the call the run waits for, bound to the call's
/// recorded digest. `request_id` is fresh for each caller's request.
pub fn resolve_approval(
    organization_id: OrganizationId,
    workspace_id: WorkspaceId,
    snapshot: &Snapshot,
    request_id: RequestId,
    approval: Approval<'_>,
) -> Result<TransitionRequest, ControlError> {
    let Approval {
        call_id,
        decision,
        api_key_id,
        api_key_name,
        reason,
    } = approval;
    let snapshot = in_scope(organization_id, workspace_id, snapshot)?;
    let control = &snapshot.control;
    if !supports(control.execution_version) {
        return Err(ControlError::UnsupportedVersion(control.execution_version));
    }
    let waiting = control.status == RunStatus::WaitingForApproval
        && control.continuation == Continuation::AwaitApproval { call_id };
    let call = snapshot
        .call(call_id)
        .filter(|call| waiting && call.state == CallState::AwaitingApproval)
        .ok_or(ControlError::NotWaiting)?;
    let (outcome, next) = match decision {
        ApprovalDecision::Approved => (
            None,
            Continuation::NeedCall {
                turn: call.turn,
                position: call.position,
            },
        ),
        ApprovalDecision::Denied => (
            Some(refused(match reason {
                Some(reason) => format!("A person denied this call, so it wasn't made. Their reason: {reason}"),
                None => DENIED.to_owned(),
            })),
            next_after(snapshot, call)?,
        ),
    };
    Ok(request(
        snapshot,
        Guards::default(),
        Operation::ResolveApproval(ResolveApproval {
            request_id,
            call_id,
            call_digest: call.call_digest,
            decision,
            api_key_id,
            api_key_name: api_key_name.to_owned(),
            reason: reason.map(str::to_owned),
            outcome,
            next,
        }),
    ))
}

/// Refuses the call the run waits on for approval because its installation,
/// `installation_id`, went away (decision 0047): the call is never sent, and
/// the run goes on without asking anyone. The job the installation's
/// deletion or disconnection queued builds it; `request_id` is fresh for
/// each of its requests. [`ControlError::NotWaiting`] unless the run waits
/// for a decision on a call to that installation.
pub fn refuse_waiting(
    organization_id: OrganizationId,
    workspace_id: WorkspaceId,
    snapshot: &Snapshot,
    request_id: RequestId,
    installation_id: InstallationId,
    gone: Gone,
) -> Result<TransitionRequest, ControlError> {
    let snapshot = in_scope(organization_id, workspace_id, snapshot)?;
    let control = &snapshot.control;
    if !supports(control.execution_version) {
        return Err(ControlError::UnsupportedVersion(control.execution_version));
    }
    let Continuation::AwaitApproval { call_id } = control.continuation else {
        return Err(ControlError::NotWaiting);
    };
    let call = snapshot
        .call(call_id)
        .filter(|call| control.status == RunStatus::WaitingForApproval && call.state == CallState::AwaitingApproval)
        .ok_or(ControlError::NotWaiting)?;
    let target = call
        .target
        .as_ref()
        .filter(|target| target.installation_id == installation_id)
        .ok_or(ControlError::NotWaiting)?;
    let (code, message) = gone.refusal(&target.installation_label);
    Ok(request(
        snapshot,
        Guards::default(),
        Operation::RefuseWaiting(RefuseWaiting {
            request_id,
            call_id,
            installation_id,
            outcome: refused(message.clone()),
            failure: Failure::new(code, message),
            next: next_after(snapshot, call)?,
        }),
    ))
}

/// Whether a worker may be about to park the run on a call waiting for
/// approval on `installation_id`: the run is at that call and hasn't parked.
/// The worker read the installation before parking, perhaps before it went,
/// so the job that refuses such calls waits for the park rather than miss it.
pub fn may_park_on(snapshot: &Snapshot, installation_id: InstallationId) -> bool {
    let control = &snapshot.control;
    let Continuation::NeedCall { turn, position } = control.continuation else {
        return false;
    };
    control.status == RunStatus::Running
        && snapshot.call_at(turn, position).is_some_and(|call| {
            call.state == CallState::AwaitingApproval
                && call
                    .target
                    .as_ref()
                    .is_some_and(|target| target.installation_id == installation_id)
        })
}

/// Cancels a run that isn't finished: every open call closed with
/// `run_cancelled`, no claim or send after it. A send already authorized
/// may still complete; its outcome is recorded. `request_id` is fresh for
/// each caller's request; `actor` is who asked, as the timeline and the
/// audit log name them.
pub fn cancel(
    organization_id: OrganizationId,
    workspace_id: WorkspaceId,
    snapshot: &Snapshot,
    request_id: RequestId,
    source: RunTrigger,
    actor: AuditActor,
) -> Result<TransitionRequest, ControlError> {
    let snapshot = in_scope(organization_id, workspace_id, snapshot)?;
    if snapshot.control.status.is_terminal() {
        return Err(ControlError::Finished);
    }
    let message = "The run was cancelled, so this call wasn't made.";
    let closing = Closing {
        calls: snapshot.open_calls(),
        outcome: refused(message.to_owned()),
        failure: Failure::new("run_cancelled", message),
    };
    Ok(request(
        snapshot,
        Guards::default(),
        Operation::Cancel(Cancel {
            request_id,
            source,
            actor,
            closing,
            abandons: prepared_attempt(snapshot),
        }),
    ))
}

/// The sweep's failure of a run no job takes forward: `run_unavailable`,
/// unless a worker's lease on it, or a call's send lease, is still live at
/// `now`. A call being sent stays `started`; once its send lease lapses,
/// [`advance`] settles it `unknown`. `request_id` is fresh for each sweep.
pub fn fail_abandoned(
    organization_id: OrganizationId,
    workspace_id: WorkspaceId,
    snapshot: &Snapshot,
    request_id: RequestId,
    now: Timestamp,
) -> Result<TransitionRequest, ControlError> {
    let snapshot = in_scope(organization_id, workspace_id, snapshot)?;
    let control = &snapshot.control;
    match control.status {
        RunStatus::Queued | RunStatus::Running => {}
        RunStatus::WaitingForApproval => return Err(ControlError::NotAbandoned),
        RunStatus::Completed | RunStatus::Failed | RunStatus::Cancelled => return Err(ControlError::Finished),
    }
    if control.lease_until.is_some_and(|until| until > now) {
        return Err(ControlError::Claimed);
    }
    if !sends_in_flight(snapshot).all(|(_, attempt)| attempt.send_lease_until <= now) {
        return Err(ControlError::Sending);
    }
    Ok(request(
        snapshot,
        Guards {
            no_live_claim: true,
            no_live_send: true,
            ..Guards::default()
        },
        Operation::FailAbandoned(FailAbandoned {
            request_id,
            error: Failure::new("run_unavailable", "The run couldn't continue. Start it again."),
            closing: closing(snapshot, "run_failed", "The run ended before this call was made."),
            abandons: prepared_attempt(snapshot),
        }),
    ))
}

#[cfg(test)]
mod tests;
