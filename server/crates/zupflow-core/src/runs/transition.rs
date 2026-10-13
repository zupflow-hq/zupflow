//! Transition requests: the only way a run changes (decision 0046).
//!
//! A [`TransitionRequest`] carries one [`Operation`], the [`Guards`] the
//! store checks before applying it, a [`TransitionId`] derived from the run
//! and the revision it starts from, and a canonical digest of all of it. The
//! store returns the first receipt for a request whose ID and digest already
//! committed, and a conflict for the same ID with another digest. A step
//! retried after an unclear commit is therefore recognisably the same step.
//!
//! Requests carry no time: the store reads its own clock after it takes the
//! write lock. Lease lengths are durations, set from that clock.

use std::fmt;

use serde::{Serialize, Serializer};
use serde_json::{Map, Value};

use crate::audit::AuditActor;
use crate::ids::{
    AgentId, ApiKeyId, InstallationId, ModelConnectionId, ModelId, OrganizationId, RunId, SessionId, ToolCallId,
    WorkspaceId,
};
use crate::runs::digest::{Sha256Digest, digest_json};
use crate::runs::names::Offer;
use crate::runs::state::{BoundTarget, CallDecision, ClaimantId, Continuation, InvocationId, RequestId, Usage, ser};
use crate::runs::{ApprovalDecision, CallOutcome, CallState, Failure, RunStatus, RunTrigger};

/// The domain of transition IDs.
pub const TRANSITION_DOMAIN: &str = "zupflow.run.transition.v1";

/// The domain of request digests.
pub const REQUEST_DOMAIN: &str = "zupflow.run.request.v1";

/// A transition's identity: the first 16 bytes of a SHA-256 over the run's
/// scope, its ID and the revision the transition starts from. Two requests
/// starting from the same revision of the same run share it, so at most one
/// of them commits.
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct TransitionId([u8; 16]);

impl TransitionId {
    pub fn derive(
        organization_id: OrganizationId,
        workspace_id: WorkspaceId,
        run_id: RunId,
        from_revision: u64,
    ) -> Self {
        let digest = digest_json(
            TRANSITION_DOMAIN,
            &serde_json::json!({
                "organization_id": organization_id,
                "workspace_id": workspace_id,
                "run_id": run_id,
                "from_revision": from_revision,
            }),
        );
        let mut bytes = [0u8; 16];
        bytes.copy_from_slice(&digest.as_bytes()[..16]);
        Self(bytes)
    }

    pub fn as_bytes(&self) -> &[u8; 16] {
        &self.0
    }
}

impl fmt::Display for TransitionId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&crate::runs::digest::hex(&self.0))
    }
}

impl fmt::Debug for TransitionId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "TransitionId({self})")
    }
}

impl Serialize for TransitionId {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.collect_str(self)
    }
}

/// The worker that must hold the run.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct OwnerGuard {
    /// The run's owner epoch must equal it, and its lease must be live.
    pub epoch: u64,
    /// The run's owner claimant must equal it: a worker's request is its own,
    /// never another claimant's of the same epoch.
    pub claimant: ClaimantId,
    /// On commit, the lease is renewed for this long from the store's clock;
    /// `None` releases the claim (park, yield, finish).
    pub renew_lease_ms: Option<u64>,
}

/// What a send claim's lease must be, on the store's clock.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum SendLease {
    /// Lapsed: nobody may still be sending (settling `unknown`).
    Lapsed,
    /// Either: the sender's outcome is recorded whenever it returns.
    Any,
}

/// The send claim that must hold: the call's attempt, with the epoch that
/// authorized it and its invocation ID.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct SendGuard {
    pub call_id: ToolCallId,
    pub attempt: u32,
    pub dispatch_epoch: u64,
    pub invocation_id: InvocationId,
    pub lease: SendLease,
}

/// What the store checks, under its write lock and on its own clock, before
/// it applies a request. The expected revision is always checked: it is the
/// request's `from_revision`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize)]
pub struct Guards {
    /// Worker steps: the owner epoch and a live lease.
    pub owner: Option<OwnerGuard>,
    /// Sends: the call's attempt, its dispatch epoch and its send lease.
    pub send: Option<SendGuard>,
    /// Refused once a cancel was asked for.
    pub not_cancelled: bool,
    /// Refused while any worker's lease on the run is live (a claim, the
    /// sweep).
    pub no_live_claim: bool,
    /// Refused while any call of the run has a live send lease (the sweep).
    pub no_live_send: bool,
}

/// How a run ends.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum RunEnding {
    Completed { output: String },
    Failed { error: Failure },
}

/// Calls closed, never sent, because the run ended: each settled refused
/// with the same outcome and failure.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Closing {
    /// By turn and position.
    pub calls: Vec<ToolCallId>,
    #[serde(serialize_with = "ser::call_outcome")]
    pub outcome: CallOutcome,
    pub failure: Failure,
}

/// The run, its first job and `run.queued`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Admit {
    pub agent_id: AgentId,
    pub session_id: SessionId,
    pub trigger: RunTrigger,
    pub input: String,
    /// The provider's model ID the run calls.
    pub model: String,
    /// The organization's model and the connection it's reached through.
    pub model_id: ModelId,
    pub connection_id: ModelConnectionId,
    pub instructions: String,
    pub execution_version: u32,
}

/// A worker takes the run: owner epoch raised to `epoch`, owner claimant set
/// to `claimant`, leased. Two workers claiming from the same revision carry
/// different claimants, so their requests share an ID but never a digest:
/// one commits, the other conflicts.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct Claim {
    pub epoch: u64,
    pub claimant: ClaimantId,
    pub lease_ms: u64,
    /// The first claim: queued to running, `run.started`.
    pub starts: bool,
}

/// A model attempt, committed before its request goes out, with the offer
/// frozen.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct PrepareModel {
    pub turn: u32,
    pub attempt: u32,
    /// A prepared attempt of this turn whose answer was never committed:
    /// marked abandoned.
    pub abandons: Option<u32>,
    pub request_digest: Sha256Digest,
    pub offer: Offer,
    /// The version of the connection's key the attempt uses; `None` for a
    /// run admitted before organizations had models.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub key_version: Option<u32>,
}

/// A call as a turn records it.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct NewCall {
    pub id: ToolCallId,
    pub position: u32,
    pub model_call_id: String,
    pub model_tool_name: String,
    pub target: Option<BoundTarget>,
    pub arguments: Map<String, Value>,
    /// `allowed`, `awaiting_approval` or `refused`.
    #[serde(serialize_with = "ser::call_state")]
    pub state: CallState,
    pub decision: Option<CallDecision>,
    pub call_digest: Option<Sha256Digest>,
    /// For a refused call, what the model reads back.
    #[serde(serialize_with = "ser::call_outcome_option")]
    pub outcome: Option<CallOutcome>,
    pub failure: Option<Failure>,
}

/// The model's whole answer for a turn: its text, usage and calls, each with
/// its decision and digest; the attempt answered.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct CommitTurn {
    pub turn: u32,
    pub attempt: u32,
    pub text: Option<String>,
    pub usage: Usage,
    pub calls: Vec<NewCall>,
    pub next: Continuation,
    /// A final answer ends the run with the turn.
    pub ending: Option<RunEnding>,
}

/// Waiting for a person: `waiting_for_approval`, `approval.requested`,
/// claim released.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Park {
    pub call_id: ToolCallId,
    /// A new decision, when an allowed call needs approval now.
    pub decision: Option<CallDecision>,
}

/// A person's decision, from an API key, bound to the call's digest.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ResolveApproval {
    /// The caller's request: two callers never build the same request.
    pub request_id: RequestId,
    pub call_id: ToolCallId,
    /// The call's digest as recorded when the model asked for it. The store
    /// refuses the decision unless the call's row still holds it, and keeps
    /// it with the approval. It doesn't say whether the tool's pinned
    /// definition changed since: that is checked when the call is sent
    /// (`AuthorizeCall`), which refuses it with `tool_changed`, so the
    /// approval never authorizes a call other than the one reviewed.
    pub call_digest: Option<Sha256Digest>,
    pub decision: ApprovalDecision,
    pub api_key_id: ApiKeyId,
    /// The key's name as the audit log records it.
    pub api_key_name: String,
    pub reason: Option<String>,
    /// For a denial, what the model reads back.
    #[serde(serialize_with = "ser::call_outcome_option")]
    pub outcome: Option<CallOutcome>,
    pub next: Continuation,
}

/// A call the run waits on for approval, refused because its installation
/// was deleted or disconnected (decision 0047): never sent, and the run
/// runnable again. Nobody decided it, so no approval is recorded. The job
/// the installation's deletion or disconnection queued asks for it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct RefuseWaiting {
    /// The job's request: two jobs never build the same request.
    pub request_id: RequestId,
    pub call_id: ToolCallId,
    /// The installation that went away. The store refuses the request
    /// unless the call is bound to it.
    pub installation_id: InstallationId,
    /// What the model reads back.
    #[serde(serialize_with = "ser::call_outcome")]
    pub outcome: CallOutcome,
    /// `installation_removed` or `installation_disconnected`.
    pub failure: Failure,
    pub next: Continuation,
}

/// What a call's check at send allows.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "authorization", rename_all = "snake_case")]
pub enum Authorization {
    /// A send claim: attempt `attempt`, under the owner's epoch, with its own
    /// invocation ID and lease. `tool_call.started`.
    Send {
        attempt: u32,
        dispatch_epoch: u64,
        invocation_id: InvocationId,
        send_lease_ms: u64,
    },
    /// Refused, never sent.
    Refuse {
        #[serde(serialize_with = "ser::call_outcome")]
        outcome: CallOutcome,
        failure: Failure,
    },
}

/// A call checked again at send: the floor and the policy, the grant, the
/// installation, the pinned definition (by the call's digest) and the
/// cancel flag.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct AuthorizeCall {
    pub call_id: ToolCallId,
    /// The decision as read at send.
    pub decision: CallDecision,
    pub authorization: Authorization,
    pub next: Continuation,
}

/// What came of sending a call, as the sender saw it: sanitized, never the
/// server's own words beyond the tool's result.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum SendOutcome {
    /// The tool answered.
    Completed { result: Map<String, Value> },
    /// The tool answered with an error of its own.
    ToolError { result: Map<String, Value> },
    /// It couldn't be sent.
    NotSent,
    /// The server refused it without acting.
    Rejected,
    /// The answer repeated the installation's credential.
    Withheld,
    /// Sent, and whether it acted is unknown.
    Unknown,
    /// Refused for the credential even after one refresh: the installation
    /// needs reconnecting.
    Unauthorized,
}

/// How a call is settled.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Settlement {
    /// `completed`, `failed`, `refused` or `unknown`.
    #[serde(serialize_with = "ser::call_state")]
    pub state: CallState,
    #[serde(serialize_with = "ser::call_outcome")]
    pub outcome: CallOutcome,
    pub failure: Option<Failure>,
}

/// The sender's outcome, fenced by its send claim, whatever the run's status.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct RecordOutcome {
    pub call_id: ToolCallId,
    pub attempt: u32,
    pub invocation_id: InvocationId,
    pub outcome: SendOutcome,
    /// `None`: the call was already settled `unknown`, so the outcome is
    /// kept on the attempt as late evidence only.
    pub settle: Option<Settlement>,
    /// `None` leaves the continuation as it is (the run finished, or the
    /// call was already settled).
    pub next: Option<Continuation>,
}

/// A call whose send lease ran out with no outcome: `unknown`, never sent
/// again.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct SettleUnknown {
    pub call_id: ToolCallId,
    pub attempt: u32,
    #[serde(serialize_with = "ser::call_outcome")]
    pub outcome: CallOutcome,
    pub failure: Failure,
    /// `None` on a finished run: the call is settled and the run stays as it
    /// is.
    pub next: Option<Continuation>,
}

/// A prepared model attempt whose answer will never be committed: the store
/// marks it abandoned.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct ModelAttemptRef {
    pub turn: u32,
    pub attempt: u32,
}

/// `cancelled`: open calls closed, claim and future sends barred.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Cancel {
    /// The caller's request: two callers never build the same request.
    pub request_id: RequestId,
    /// Who asked.
    pub source: RunTrigger,
    /// Who asked, as the timeline (`cancelled_by`) and the audit log name
    /// them.
    pub actor: AuditActor,
    pub closing: Closing,
    pub abandons: Option<ModelAttemptRef>,
}

/// Why a worker gives the run back.
/// No time is carried: the store works out when to resume from its own
/// records and clock.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(tag = "why", rename_all = "snake_case")]
pub enum YieldReason {
    /// The claim took its share of steps; the run is healthy. Resume now.
    StepBudget,
    /// A call's send claim is still live: resume when that attempt's send
    /// lease lapses, as the store recorded it.
    SendInFlight { call_id: ToolCallId, attempt: u32 },
    /// The model gave no answer to this prepared attempt. The store marks it
    /// `unanswered` and the run goes back to `NeedModel` for its turn. Unlike
    /// the others, the job is retried, using one of its attempts, so a model
    /// that stays unavailable still fails the run when they run out, as
    /// today.
    ///
    /// `timed_out` marks the attempt as one sent and not answered in time
    /// (a second fails the turn); `retry_after_secs` is the provider's own
    /// wait, which the job keeps instead of its usual step.
    ModelUnavailable {
        turn: u32,
        attempt: u32,
        #[serde(skip_serializing_if = "is_false")]
        timed_out: bool,
        #[serde(skip_serializing_if = "Option::is_none")]
        retry_after_secs: Option<u32>,
    },
}

fn is_false(value: &bool) -> bool {
    !*value
}

/// The claim released at a safe boundary; the job postponed, not retried.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct Yield {
    #[serde(flatten)]
    pub why: YieldReason,
}

/// `completed` or `failed`, open calls closed.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Finish {
    pub ending: RunEnding,
    pub closing: Option<Closing>,
    pub abandons: Option<ModelAttemptRef>,
}

/// The sweep fails a run whose job died: `run_unavailable`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct FailAbandoned {
    /// The sweep's request: two sweeps never build the same request.
    pub request_id: RequestId,
    pub error: Failure,
    pub closing: Option<Closing>,
    pub abandons: Option<ModelAttemptRef>,
}

/// One change to a run.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum Operation {
    /// The API.
    Admit(Admit),
    /// A worker.
    Claim(Claim),
    PrepareModel(PrepareModel),
    CommitTurn(CommitTurn),
    Park(Park),
    /// An API key.
    ResolveApproval(ResolveApproval),
    /// The job an installation's deletion or disconnection queued.
    RefuseWaiting(RefuseWaiting),
    /// A worker.
    AuthorizeCall(AuthorizeCall),
    /// The sender, under its send claim.
    RecordOutcome(RecordOutcome),
    /// A worker.
    SettleUnknown(SettleUnknown),
    /// The API or the Console.
    Cancel(Cancel),
    /// A worker.
    Yield(Yield),
    Finish(Finish),
    /// The sweep.
    FailAbandoned(FailAbandoned),
}

impl Operation {
    /// The operation's stable name, as stored with the transition.
    pub fn name(&self) -> &'static str {
        match self {
            Self::Admit(_) => "admit",
            Self::Claim(_) => "claim",
            Self::PrepareModel(_) => "prepare_model",
            Self::CommitTurn(_) => "commit_turn",
            Self::Park(_) => "park",
            Self::ResolveApproval(_) => "resolve_approval",
            Self::RefuseWaiting(_) => "refuse_waiting",
            Self::AuthorizeCall(_) => "authorize_call",
            Self::RecordOutcome(_) => "record_outcome",
            Self::SettleUnknown(_) => "settle_unknown",
            Self::Cancel(_) => "cancel",
            Self::Yield(_) => "yield",
            Self::Finish(_) => "finish",
            Self::FailAbandoned(_) => "fail_abandoned",
        }
    }

    /// The public status after the operation, from `before`.
    pub fn status_after(&self, before: RunStatus) -> RunStatus {
        let ended = |ending: &RunEnding| match ending {
            RunEnding::Completed { .. } => RunStatus::Completed,
            RunEnding::Failed { .. } => RunStatus::Failed,
        };
        match self {
            Self::Admit(_) => RunStatus::Queued,
            Self::Claim(claim) if claim.starts => RunStatus::Running,
            Self::CommitTurn(commit) => commit.ending.as_ref().map_or(RunStatus::Running, ended),
            Self::Park(_) => RunStatus::WaitingForApproval,
            Self::ResolveApproval(_) | Self::RefuseWaiting(_) => RunStatus::Running,
            Self::Cancel(_) => RunStatus::Cancelled,
            Self::Finish(finish) => ended(&finish.ending),
            Self::FailAbandoned(_) => RunStatus::Failed,
            Self::Claim(_)
            | Self::PrepareModel(_)
            | Self::AuthorizeCall(_)
            | Self::RecordOutcome(_)
            | Self::SettleUnknown(_)
            | Self::Yield(_) => before,
        }
    }

    /// The continuation after the operation, from `before`.
    pub fn continuation_after(&self, before: Continuation) -> Continuation {
        match self {
            Self::Admit(_) => Continuation::NeedModel { turn: 0 },
            Self::PrepareModel(prepare) => Continuation::AwaitModel {
                turn: prepare.turn,
                attempt: prepare.attempt,
            },
            Self::CommitTurn(commit) => commit.next,
            Self::Park(park) => Continuation::AwaitApproval { call_id: park.call_id },
            Self::ResolveApproval(resolve) => resolve.next,
            Self::RefuseWaiting(refuse) => refuse.next,
            Self::AuthorizeCall(authorize) => authorize.next,
            Self::RecordOutcome(record) => record.next.unwrap_or(before),
            Self::SettleUnknown(settle) => settle.next.unwrap_or(before),
            Self::Cancel(_) | Self::Finish(_) | Self::FailAbandoned(_) => Continuation::Done,
            Self::Yield(Yield {
                why: YieldReason::ModelUnavailable { turn, .. },
            }) => Continuation::NeedModel { turn: *turn },
            Self::Claim(_) | Self::Yield(_) => before,
        }
    }
}

/// One transition, ready for `RunStore::apply`. Built only through
/// [`TransitionRequest::new`], so its ID and digest always match its content.
///
/// A request names its own scope, and `apply` is given one too. The store
/// refuses a request (as not found, changing nothing) unless its
/// organization, workspace and run are exactly `apply`'s, and its ID is
/// [`TransitionId::derive`] of that scope and its `from_revision`; and it
/// checks the digest it stores against [`TransitionRequest::digest`].
#[derive(Debug, Clone, PartialEq)]
pub struct TransitionRequest {
    organization_id: OrganizationId,
    workspace_id: WorkspaceId,
    run_id: RunId,
    id: TransitionId,
    from_revision: u64,
    guards: Guards,
    operation: Operation,
    digest: Sha256Digest,
}

#[derive(Serialize)]
struct Canonical<'a> {
    transition_id: TransitionId,
    organization_id: OrganizationId,
    workspace_id: WorkspaceId,
    run_id: RunId,
    from_revision: u64,
    guards: &'a Guards,
    operation: &'a Operation,
}

impl TransitionRequest {
    /// A request to move the run from `from_revision` (0 for a run that
    /// doesn't exist yet), with its ID and digest.
    pub fn new(
        organization_id: OrganizationId,
        workspace_id: WorkspaceId,
        run_id: RunId,
        from_revision: u64,
        guards: Guards,
        operation: Operation,
    ) -> Self {
        let id = TransitionId::derive(organization_id, workspace_id, run_id, from_revision);
        let digest = digest_of(&Canonical {
            transition_id: id,
            organization_id,
            workspace_id,
            run_id,
            from_revision,
            guards: &guards,
            operation: &operation,
        });
        Self {
            organization_id,
            workspace_id,
            run_id,
            id,
            from_revision,
            guards,
            operation,
            digest,
        }
    }

    pub fn organization_id(&self) -> OrganizationId {
        self.organization_id
    }

    pub fn workspace_id(&self) -> WorkspaceId {
        self.workspace_id
    }

    pub fn run_id(&self) -> RunId {
        self.run_id
    }

    pub fn id(&self) -> TransitionId {
        self.id
    }

    /// The revision the run must be at: the expected revision.
    pub fn from_revision(&self) -> u64 {
        self.from_revision
    }

    /// The revision the run is at once it commits; `None` when the run's
    /// revisions are exhausted (the store refuses it).
    pub fn to_revision(&self) -> Option<u64> {
        self.from_revision.checked_add(1)
    }

    pub fn guards(&self) -> &Guards {
        &self.guards
    }

    pub fn operation(&self) -> &Operation {
        &self.operation
    }

    /// The SHA-256 of the canonical request: its ID, scope, revision, guards
    /// and operation.
    pub fn digest(&self) -> Sha256Digest {
        self.digest
    }
}

fn digest_of(canonical: &Canonical<'_>) -> Sha256Digest {
    // Every field serializes: string keys only, no maps keyed otherwise.
    let value = serde_json::to_value(canonical).expect("a transition request serializes to JSON");
    digest_json(REQUEST_DOMAIN, &value)
}

/// Why the store refused a request.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum Conflict {
    /// The run is at another revision, or a request with the same ID and
    /// another digest committed: reload, recompute, no I/O.
    #[error("the run moved on")]
    Revision,
    /// Another worker holds the run, or this one's lease ran out: stop.
    #[error("the run's claim is not this worker's")]
    Owner,
    /// The send claim isn't this attempt's, or its lease isn't as required.
    #[error("the call's send claim doesn't hold")]
    SendClaim,
    /// A cancel was asked for: no claim, step or send.
    #[error("the run was cancelled")]
    Cancelled,
    /// A worker's lease on the run is live.
    #[error("the run is claimed")]
    Claimed,
}

impl Conflict {
    /// Whether to reload the run and compute the step again. Anything else
    /// means stop.
    pub fn reload(self) -> bool {
        self == Self::Revision
    }
}

#[cfg(test)]
mod tests {
    use uuid::Uuid;

    use super::*;

    fn scope() -> (OrganizationId, WorkspaceId, RunId) {
        (
            OrganizationId::from_uuid(Uuid::from_u128(1)),
            WorkspaceId::from_uuid(Uuid::from_u128(2)),
            RunId::from_uuid(Uuid::from_u128(3)),
        )
    }

    /// A yield, waiting on attempt `attempt` of a call when it's above 0.
    fn yielding(attempt: u32) -> Operation {
        let why = if attempt == 0 {
            YieldReason::StepBudget
        } else {
            YieldReason::SendInFlight {
                call_id: ToolCallId::from_uuid(Uuid::from_u128(4)),
                attempt,
            }
        };
        Operation::Yield(Yield { why })
    }

    fn owned(claimant: u128) -> Guards {
        Guards {
            owner: Some(OwnerGuard {
                epoch: 2,
                claimant: ClaimantId::from_uuid(Uuid::from_u128(claimant)),
                renew_lease_ms: None,
            }),
            not_cancelled: true,
            ..Guards::default()
        }
    }

    #[test]
    fn transition_ids_depend_on_the_run_and_its_revision_only() {
        let (organization, workspace, run) = scope();
        let id = TransitionId::derive(organization, workspace, run, 4);
        assert_eq!(id, TransitionId::derive(organization, workspace, run, 4));
        assert_eq!(id.to_string().len(), 32);
        for other in [
            TransitionId::derive(organization, workspace, run, 5),
            TransitionId::derive(organization, workspace, RunId::from_uuid(Uuid::from_u128(9)), 4),
            TransitionId::derive(organization, WorkspaceId::from_uuid(Uuid::from_u128(9)), run, 4),
            TransitionId::derive(OrganizationId::from_uuid(Uuid::from_u128(9)), workspace, run, 4),
        ] {
            assert_ne!(other, id);
        }
        // Two different requests from the same revision share it; their
        // digests differ.
        let a = TransitionRequest::new(organization, workspace, run, 4, Guards::default(), yielding(1));
        let b = TransitionRequest::new(organization, workspace, run, 4, Guards::default(), yielding(2));
        assert_eq!(a.id(), b.id());
        assert_ne!(a.digest(), b.digest());
        assert_eq!(a.to_revision(), Some(5));
        let last = TransitionRequest::new(organization, workspace, run, u64::MAX, Guards::default(), yielding(0));
        assert_eq!(last.to_revision(), None);
    }

    #[test]
    fn the_same_request_has_the_same_digest() {
        let (organization, workspace, run) = scope();
        let guards = owned(1);
        let a = TransitionRequest::new(organization, workspace, run, 7, guards, yielding(5));
        let b = TransitionRequest::new(organization, workspace, run, 7, guards, yielding(5));
        assert_eq!(a, b);
        assert_eq!(a.digest(), b.digest());
        // Guards are part of what is committed.
        let unguarded = TransitionRequest::new(organization, workspace, run, 7, Guards::default(), yielding(5));
        assert_ne!(unguarded.digest(), a.digest());
    }

    #[test]
    fn two_claimants_never_build_the_same_request() {
        let (organization, workspace, run) = scope();
        let claim = |claimant: u128| {
            Operation::Claim(Claim {
                epoch: 3,
                claimant: ClaimantId::from_uuid(Uuid::from_u128(claimant)),
                lease_ms: 1000,
                starts: false,
            })
        };
        let guards = Guards {
            not_cancelled: true,
            no_live_claim: true,
            ..Guards::default()
        };
        let a = TransitionRequest::new(organization, workspace, run, 7, guards, claim(1));
        let b = TransitionRequest::new(organization, workspace, run, 7, guards, claim(2));
        // One ID, so at most one commits; two digests, so the loser
        // conflicts instead of replaying the winner's receipt.
        assert_eq!(a.id(), b.id());
        assert_ne!(a.digest(), b.digest());
        // The same worker's retry is the same request.
        assert_eq!(
            a,
            TransitionRequest::new(organization, workspace, run, 7, guards, claim(1))
        );
        // A worker step is its claimant's own.
        let mine = TransitionRequest::new(organization, workspace, run, 8, owned(1), yielding(0));
        let theirs = TransitionRequest::new(organization, workspace, run, 8, owned(2), yielding(0));
        assert_ne!(mine.digest(), theirs.digest());
    }

    #[test]
    fn only_a_revision_conflict_reloads() {
        assert!(Conflict::Revision.reload());
        for conflict in [
            Conflict::Owner,
            Conflict::SendClaim,
            Conflict::Cancelled,
            Conflict::Claimed,
        ] {
            assert!(!conflict.reload(), "{conflict:?}");
        }
    }
}
