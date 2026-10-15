//! A run's exact position and who may move it (decision 0046).
//!
//! The public [`RunStatus`] stays as the contract has it. Beside it, a run
//! keeps internal fields ([`RunControl`]): its [`Continuation`], the exact
//! next step; a `revision` every transition raises; the owner epoch and
//! lease of the worker that holds it; when a cancel was asked for; and the
//! execution version it was created under.
//!
//! A [`Snapshot`] is a run as committed: its control fields, what it was
//! started with, its turns, its calls with their attempts to send, and its
//! model attempts. The reducer reads nothing else.

use std::fmt;

use serde::{Deserialize, Serialize, Serializer};
use serde_json::{Map, Value};
use uuid::Uuid;

use crate::ids::{
    ApiKeyId, InstallationId, ModelConnectionId, ModelId, OrganizationId, RunId, ToolCallId, WorkspaceId,
};
use crate::models::TokenUsage;
use crate::policy::{Decision, Verdict};
use crate::runs::digest::Sha256Digest;
use crate::runs::names::Offer;
use crate::runs::{ApprovalDecision, CallOutcome, CallState, Failure, RunStatus};
use crate::time::Timestamp;

/// The execution version new runs are created under. A worker advances only
/// runs of a version it supports ([`supports`]).
pub const EXECUTION_VERSION: u32 = 1;

/// Whether this build can advance runs created under `version`.
pub fn supports(version: u32) -> bool {
    version == EXECUTION_VERSION
}

/// The version of the continuation's stored JSON.
pub const CONTINUATION_VERSION: u32 = 1;

/// The exact next step of a run.
///
/// A worker holds a claim from `NeedModel` until it parks or finishes.
/// `AwaitApproval` holds none; `AwaitCall` holds only that call's send
/// claim.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "step", rename_all = "snake_case")]
pub enum Continuation {
    /// Ask the model for turn `turn` (from 0).
    NeedModel { turn: u32 },
    /// Model attempt `attempt` (from 1) for `turn` is prepared; its answer is
    /// not committed yet.
    AwaitModel { turn: u32, attempt: u32 },
    /// Send, park or refuse the call at `position` of `turn`.
    NeedCall { turn: u32, position: u32 },
    /// Waiting for a person's decision on `call_id`.
    AwaitApproval { call_id: ToolCallId },
    /// Attempt `attempt` (from 1) of `call_id` is authorized to send; its
    /// outcome is not recorded yet.
    AwaitCall { call_id: ToolCallId, attempt: u32 },
    /// Finished: completed, failed or cancelled.
    Done,
}

#[derive(Serialize, Deserialize)]
struct Versioned {
    v: u32,
    #[serde(flatten)]
    step: Continuation,
}

/// Stored continuation JSON this build can't read.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum ContinuationError {
    #[error("the continuation isn't valid JSON of a known step")]
    Malformed,
    #[error("the continuation's version {0} isn't one this build reads")]
    UnknownVersion(u32),
}

impl Continuation {
    /// Whether a worker must hold the run's claim to take this step.
    pub fn needs_claim(&self) -> bool {
        matches!(
            self,
            Self::NeedModel { .. } | Self::AwaitModel { .. } | Self::NeedCall { .. }
        )
    }

    /// The stored form: versioned JSON, `{"v":1,"step":"need_model","turn":0}`.
    pub fn to_json(&self) -> String {
        let versioned = Versioned {
            v: CONTINUATION_VERSION,
            step: *self,
        };
        // A struct of integers, strings and IDs always serializes.
        serde_json::to_string(&versioned).unwrap_or_default()
    }

    /// Reads the stored form; any other version is refused, never guessed.
    pub fn from_json(json: &str) -> Result<Self, ContinuationError> {
        let value: Value = serde_json::from_str(json).map_err(|_| ContinuationError::Malformed)?;
        let version = value
            .get("v")
            .and_then(Value::as_u64)
            .ok_or(ContinuationError::Malformed)?;
        if version != u64::from(CONTINUATION_VERSION) {
            return Err(ContinuationError::UnknownVersion(
                u32::try_from(version).unwrap_or(u32::MAX),
            ));
        }
        serde_json::from_value::<Versioned>(value)
            .map(|versioned| versioned.step)
            .map_err(|_| ContinuationError::Malformed)
    }
}

/// A run's internal control fields, as committed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RunControl {
    /// Public.
    pub status: RunStatus,
    pub continuation: Continuation,
    /// Raised by every transition. A run that doesn't exist yet is at 0.
    pub revision: u64,
    /// Raised by each claim; 0 before the first.
    pub owner_epoch: u64,
    /// The worker that made the current claim; `None` before the first.
    pub owner_claimant: Option<ClaimantId>,
    /// Until when the owner's claim holds, on the store's clock; `None` when
    /// no worker holds the run.
    pub lease_until: Option<Timestamp>,
    pub cancel_requested_at: Option<Timestamp>,
    pub execution_version: u32,
}

/// Internal identities: plain UUIDs that never leave the server. They have
/// no `Display`, and `Debug` never prints the value, so none reaches a log;
/// they serialize (as 32 hex characters) only into a request's digest and
/// the store's rows.
macro_rules! internal_id {
    ($(#[$doc:meta])* $name:ident) => {
        $(#[$doc])*
        #[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
        pub struct $name(Uuid);

        impl $name {
            pub fn from_uuid(uuid: Uuid) -> Self {
                Self(uuid)
            }

            pub fn uuid(&self) -> Uuid {
                self.0
            }
        }

        impl fmt::Debug for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str(concat!(stringify!($name), "(…)"))
            }
        }

        impl Serialize for $name {
            fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
                serializer.collect_str(&self.0.simple())
            }
        }
    };
}

internal_id! {
    /// An authorization to send's own identity: the send claim's invocation
    /// ID.
    InvocationId
}

internal_id! {
    /// A control request's own identity (an approval, a cancel, the sweep's
    /// failure), fresh for every caller's request, so two callers acting on
    /// one revision conflict: only the same caller's retry replays.
    RequestId
}

internal_id! {
    /// One claim's own identity, fresh for every worker that claims a run,
    /// so two workers claiming from the same revision never build the same
    /// request: only the same worker's retry replays its receipt.
    ClaimantId
}

/// Tokens one model answer took ([`TokenUsage`] says what each counts).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Usage {
    pub input_tokens: u64,
    pub output_tokens: u64,
    #[serde(default)]
    pub cache_read_input_tokens: u64,
    #[serde(default)]
    pub cache_write_input_tokens: u64,
}

impl From<TokenUsage> for Usage {
    fn from(usage: TokenUsage) -> Self {
        // Kept within the input, as `TokenUsage::new` builds it.
        let usage = TokenUsage::new(
            usage.input_tokens,
            usage.output_tokens,
            usage.cache_read_input_tokens,
            usage.cache_write_input_tokens,
        );
        Self {
            input_tokens: usage.input_tokens,
            output_tokens: usage.output_tokens,
            cache_read_input_tokens: usage.cache_read_input_tokens,
            cache_write_input_tokens: usage.cache_write_input_tokens,
        }
    }
}

/// A policy's verdict on a call, as recorded with it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "verdict", rename_all = "snake_case")]
pub enum CallVerdict {
    Allow,
    RequireApproval { reason: String },
    Deny { reason: String },
}

/// The decision the floor or the policy made on a call (decision 0040), as
/// recorded with it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CallDecision {
    #[serde(flatten)]
    pub verdict: CallVerdict,
    pub policy: String,
    pub mode: Option<String>,
    pub rule: String,
}

impl From<&Decision> for CallDecision {
    fn from(decision: &Decision) -> Self {
        Self {
            verdict: match &decision.verdict {
                Verdict::Allow => CallVerdict::Allow,
                Verdict::RequireApproval { reason } => CallVerdict::RequireApproval { reason: reason.clone() },
                Verdict::Deny { reason } => CallVerdict::Deny { reason: reason.clone() },
            },
            policy: decision.policy.to_owned(),
            mode: decision.mode.map(|mode| mode.as_str().to_owned()),
            rule: decision.rule.as_str().to_owned(),
        }
    }
}

/// The installation and tool a call goes to, recorded with it when the
/// model's name was looked up in the turn's frozen offer (decision 0047).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BoundTarget {
    pub installation_id: InstallationId,
    pub installation_short_id: String,
    pub installation_label: String,
    pub app_key: String,
    pub app_name: String,
    /// The original MCP tool name the call is sent under.
    pub tool: String,
    /// The pinned definition the call was decided against.
    pub definition_sha256: Sha256Digest,
}

/// One authorization to send a call: its send claim.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CallAttempt {
    /// From 1.
    pub attempt: u32,
    /// The owner epoch that authorized it.
    pub dispatch_epoch: u64,
    pub invocation_id: InvocationId,
    /// Until when the send claim holds, on the store's clock.
    pub send_lease_until: Timestamp,
    /// Whether the sender's outcome was recorded for this attempt (as the
    /// call's outcome, or as late evidence).
    pub outcome_recorded: bool,
}

/// A call the model asked for, as committed.
#[derive(Debug, Clone, PartialEq)]
pub struct CallRecord {
    pub id: ToolCallId,
    pub turn: u32,
    pub position: u32,
    /// The model's own ID for the call.
    pub model_call_id: String,
    /// The name the model called it by.
    pub model_tool_name: String,
    /// `None` when the name wasn't in the turn's offer.
    pub target: Option<BoundTarget>,
    pub arguments: Map<String, Value>,
    pub state: CallState,
    pub decision: Option<CallDecision>,
    /// Over the target and the arguments; `None` without a target.
    pub call_digest: Option<Sha256Digest>,
    pub approval: Option<(ApprovalDecision, ApiKeyId)>,
    /// What the model reads back, once settled.
    pub outcome: Option<CallOutcome>,
    /// The code and message the timeline shows when the call failed or was
    /// refused.
    pub failure: Option<Failure>,
    pub attempts: Vec<CallAttempt>,
}

impl CallRecord {
    /// Waiting to be sent, parked or refused: allowed, approved or awaiting
    /// approval.
    pub fn is_open(&self) -> bool {
        matches!(
            self.state,
            CallState::Allowed | CallState::Approved | CallState::AwaitingApproval
        )
    }

    pub fn attempt(&self, attempt: u32) -> Option<&CallAttempt> {
        self.attempts.iter().find(|recorded| recorded.attempt == attempt)
    }
}

/// Where a model attempt stands.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ModelAttemptState {
    /// Committed before the request went out; no answer committed.
    Prepared,
    /// Its answer is committed: never asked for again.
    Answered,
    /// Its worker stopped before the answer was committed: tokens may have
    /// been spent and not counted.
    Abandoned,
    /// The provider gave no answer (unavailable): nothing committed, tokens
    /// unknown. The turn is asked again under a new attempt.
    Unanswered,
}

/// One request to the model, as committed by `PrepareModel`.
#[derive(Debug, Clone, PartialEq)]
pub struct ModelAttempt {
    pub turn: u32,
    /// From 1.
    pub attempt: u32,
    pub state: ModelAttemptState,
    pub request_digest: Sha256Digest,
    /// The tools offered, frozen.
    pub offer: Offer,
    /// Unanswered because it was sent and no answer came in time.
    pub timed_out: bool,
}

/// One committed model answer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommittedTurn {
    pub turn: u32,
    pub text: Option<String>,
}

/// A run as committed: everything [`super::reducer::advance`] reads.
#[derive(Debug, Clone, PartialEq)]
pub struct Snapshot {
    pub organization_id: OrganizationId,
    pub workspace_id: WorkspaceId,
    pub run_id: RunId,
    pub control: RunControl,
    /// The provider's model ID, and the agent's instructions, as the run
    /// was admitted.
    pub model: String,
    /// The organization's model and its connection, as the run was
    /// admitted; `None` for a run admitted before organizations had models.
    pub model_id: Option<ModelId>,
    pub connection_id: Option<ModelConnectionId>,
    pub instructions: String,
    pub input: String,
    /// In order: `turns[n].turn == n`.
    pub turns: Vec<CommittedTurn>,
    /// Every call, by turn and position.
    pub calls: Vec<CallRecord>,
    /// Every model attempt, by turn and attempt.
    pub model_attempts: Vec<ModelAttempt>,
}

impl Snapshot {
    pub fn call(&self, call_id: ToolCallId) -> Option<&CallRecord> {
        self.calls.iter().find(|call| call.id == call_id)
    }

    pub fn call_at(&self, turn: u32, position: u32) -> Option<&CallRecord> {
        self.calls
            .iter()
            .find(|call| call.turn == turn && call.position == position)
    }

    pub fn model_attempt(&self, turn: u32, attempt: u32) -> Option<&ModelAttempt> {
        self.model_attempts
            .iter()
            .find(|recorded| recorded.turn == turn && recorded.attempt == attempt)
    }

    /// Calls not yet sent, parked or refused, by turn and position.
    pub fn open_calls(&self) -> Vec<ToolCallId> {
        let mut open: Vec<&CallRecord> = self.calls.iter().filter(|call| call.is_open()).collect();
        open.sort_by_key(|call| (call.turn, call.position));
        open.into_iter().map(|call| call.id).collect()
    }
}

/// Serializers for the run types the transitions carry.
pub(crate) mod ser {
    use serde::Serializer;
    use serde::ser::SerializeStruct;

    use crate::runs::{CallOutcome, CallState};

    pub fn call_state<S: Serializer>(state: &CallState, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(state.as_str())
    }

    pub fn call_outcome<S: Serializer>(outcome: &CallOutcome, serializer: S) -> Result<S::Ok, S::Error> {
        let mut out = serializer.serialize_struct("CallOutcome", 2)?;
        out.serialize_field("content", &outcome.content)?;
        out.serialize_field("is_error", &outcome.is_error)?;
        out.end()
    }

    pub fn call_outcome_option<S: Serializer>(outcome: &Option<CallOutcome>, serializer: S) -> Result<S::Ok, S::Error> {
        match outcome {
            Some(outcome) => call_outcome(outcome, serializer),
            None => serializer.serialize_none(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::policy::{ApprovalMode, Rule};

    #[test]
    fn every_continuation_round_trips_its_stored_form() {
        let call_id = ToolCallId::from_uuid(Uuid::from_u128(7));
        let all = [
            Continuation::NeedModel { turn: 0 },
            Continuation::AwaitModel { turn: 3, attempt: 2 },
            Continuation::NeedCall { turn: 1, position: 4 },
            Continuation::AwaitApproval { call_id },
            Continuation::AwaitCall { call_id, attempt: 1 },
            Continuation::Done,
        ];
        for continuation in all {
            let json = continuation.to_json();
            assert_eq!(Continuation::from_json(&json), Ok(continuation), "{json}");
        }
        assert_eq!(
            Continuation::NeedModel { turn: 0 }.to_json(),
            r#"{"v":1,"step":"need_model","turn":0}"#
        );
        assert_eq!(Continuation::Done.to_json(), r#"{"v":1,"step":"done"}"#);
    }

    #[test]
    fn refuses_stored_forms_it_cannot_read() {
        for (json, error) in [
            (r#"{"v":2,"step":"done"}"#, ContinuationError::UnknownVersion(2)),
            (r#"{"step":"done"}"#, ContinuationError::Malformed),
            (r#"{"v":1,"step":"sleep"}"#, ContinuationError::Malformed),
            (r#"{"v":1,"step":"need_model"}"#, ContinuationError::Malformed),
            (r#"{"v":1,"step":"need_model","turn":-1}"#, ContinuationError::Malformed),
            (
                r#"{"v":1,"step":"await_approval","call_id":"run_031DCp7XMZ9AHIB2uF6QgS"}"#,
                ContinuationError::Malformed,
            ),
            ("not json", ContinuationError::Malformed),
        ] {
            assert_eq!(Continuation::from_json(json), Err(error), "{json}");
        }
    }

    #[test]
    fn only_working_steps_need_a_claim() {
        let call_id = ToolCallId::from_uuid(Uuid::from_u128(7));
        assert!(Continuation::NeedModel { turn: 0 }.needs_claim());
        assert!(Continuation::AwaitModel { turn: 0, attempt: 1 }.needs_claim());
        assert!(Continuation::NeedCall { turn: 0, position: 0 }.needs_claim());
        assert!(!Continuation::AwaitApproval { call_id }.needs_claim());
        assert!(!Continuation::AwaitCall { call_id, attempt: 1 }.needs_claim());
        assert!(!Continuation::Done.needs_claim());
    }

    #[test]
    fn internal_ids_never_print_their_uuid() {
        let uuid = Uuid::from_u128(0x0199_a7c4_5e2b_7c3d_9f1a_2b3c_4d5e_6f70);
        for printed in [
            format!("{:?}", InvocationId::from_uuid(uuid)),
            format!("{:?}", ClaimantId::from_uuid(uuid)),
        ] {
            assert!(!printed.contains(&uuid.simple().to_string()), "{printed}");
            assert!(!printed.contains(&uuid.hyphenated().to_string()), "{printed}");
        }
        assert_eq!(format!("{:?}", InvocationId::from_uuid(uuid)), "InvocationId(…)");
    }

    #[test]
    fn only_this_execution_version_is_supported() {
        assert!(supports(EXECUTION_VERSION));
        assert!(!supports(0));
        assert!(!supports(EXECUTION_VERSION + 1));
    }

    #[test]
    fn records_a_policy_decision() {
        let decision = Decision {
            verdict: Verdict::RequireApproval {
                reason: "Needs a person.".to_owned(),
            },
            policy: "configured",
            mode: Some(ApprovalMode::Manual),
            rule: Rule::GrantRequiresApproval,
        };
        let recorded = CallDecision::from(&decision);
        assert_eq!(
            serde_json::to_value(&recorded).expect("serializes"),
            serde_json::json!({
                "verdict": "require_approval",
                "reason": "Needs a person.",
                "policy": "configured",
                "mode": "manual",
                "rule": "grant_requires_approval",
            })
        );
    }
}
