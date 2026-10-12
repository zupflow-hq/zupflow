//! Runs: an agent working on one input, step by step, and the timeline of
//! what it did (decisions 0040 and 0046; the backend standard's "Runs").
//!
//! A run is an explicit state machine (decision 0046). Its public status is
//! [`RunStatus`]; its exact position is a continuation, and every change to
//! it is one transition its store commits atomically, with the events,
//! webhook announcements, audit rows and jobs it implies:
//!
//! - [`state`]: a run's continuation and control fields, and the snapshot
//!   the reducer reads.
//! - [`transition`]: transition requests, their guards, IDs and digests.
//! - [`reducer`]: `advance(snapshot, facts) -> Plan`, and the API's and the
//!   sweep's own operations.
//! - [`names`]: the names tools reach the model by (decision 0047).
//! - [`digest`]: canonical JSON digests, the call digest among them.
//! - [`store`]: [`RunStore`], whose `apply` commits each transition with
//!   everything it implies.
//!
//! Every method takes the organization, then the workspace, and reaches
//! nothing outside them. A run outlives its agent: deleting an agent keeps
//! its runs, named "Deleted agent".

pub mod digest;
pub mod names;
pub mod reducer;
pub mod state;
pub mod store;
pub mod transition;

pub use store::RunStore;

use serde_json::Value;

use crate::environment::Environment;
use crate::ids::{AgentId, OrganizationId, RunEventId, RunId, SessionId, WorkspaceId};
use crate::jobs::JobOptions;
use crate::scope::Name;
use crate::time::Timestamp;

/// The longest input a run is started with.
pub const MAX_INPUT: usize = 8000;

/// The largest page of runs a list answers.
pub const MAX_PAGE: u32 = 100;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum RunStatus {
    Queued,
    Running,
    WaitingForApproval,
    Completed,
    Failed,
    Cancelled,
}

impl RunStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Queued => "queued",
            Self::Running => "running",
            Self::WaitingForApproval => "waiting_for_approval",
            Self::Completed => "completed",
            Self::Failed => "failed",
            Self::Cancelled => "cancelled",
        }
    }

    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "queued" => Some(Self::Queued),
            "running" => Some(Self::Running),
            "waiting_for_approval" => Some(Self::WaitingForApproval),
            "completed" => Some(Self::Completed),
            "failed" => Some(Self::Failed),
            "cancelled" => Some(Self::Cancelled),
            _ => None,
        }
    }

    /// Completed, failed or cancelled: nothing changes it again.
    pub fn is_terminal(self) -> bool {
        matches!(self, Self::Completed | Self::Failed | Self::Cancelled)
    }
}

/// Who started a run.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum RunTrigger {
    /// An API key: the customer's product.
    Api,
    /// A Console member, in Sandbox.
    Console,
}

impl RunTrigger {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Api => "api",
            Self::Console => "console",
        }
    }

    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "api" => Some(Self::Api),
            "console" => Some(Self::Console),
            _ => None,
        }
    }
}

/// The contract's `RunUsage`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, serde::Serialize)]
pub struct RunUsage {
    pub input_tokens: u64,
    pub output_tokens: u64,
    /// Calls sent to an app's server.
    pub tool_calls: u64,
}

/// Why a run or a call failed: a stable code and a message written for a
/// developer. Never a provider's own words.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct Failure {
    pub code: String,
    pub message: String,
}

impl Failure {
    pub fn new(code: &str, message: impl Into<String>) -> Self {
        Self {
            code: code.to_owned(),
            message: message.into(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Run {
    pub id: RunId,
    pub organization_id: OrganizationId,
    pub workspace_id: WorkspaceId,
    pub agent_id: AgentId,
    /// The agent's name now; `None` once it's deleted.
    pub agent_name: Option<String>,
    pub session_id: SessionId,
    pub trigger: RunTrigger,
    pub status: RunStatus,
    pub input: String,
    pub output: Option<String>,
    pub usage: RunUsage,
    pub error: Option<Failure>,
    pub created_at: Timestamp,
    pub started_at: Option<Timestamp>,
    pub finished_at: Option<Timestamp>,
}

/// A run listed across an organization's workspaces, with its workspace.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OrganizationRun {
    pub run: Run,
    pub workspace_name: String,
    pub workspace_slug: String,
    pub environment: Environment,
}

/// What a run is started with.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NewRun {
    pub agent_id: AgentId,
    pub session_id: SessionId,
    pub trigger: RunTrigger,
    pub input: String,
    /// The agent's model and instructions as the run starts: a later change
    /// to the agent doesn't change a run under way.
    pub model: String,
    pub instructions: String,
}

/// A job the store queues in the transaction of the change that needs it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct QueuedJob {
    pub kind: Name,
    /// References only: at most `jobs::MAX_JOB_BYTES`.
    pub payload: Vec<u8>,
    pub options: JobOptions,
}

/// The event types of a run's timeline, as the contract names them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RunEventType {
    RunQueued,
    RunStarted,
    MessageAssistant,
    ToolCallStarted,
    ToolCallCompleted,
    ToolCallFailed,
    ApprovalRequested,
    ApprovalResolved,
    RunCompleted,
    RunFailed,
    RunCancelled,
}

impl RunEventType {
    pub const ALL: [Self; 11] = [
        Self::RunQueued,
        Self::RunStarted,
        Self::MessageAssistant,
        Self::ToolCallStarted,
        Self::ToolCallCompleted,
        Self::ToolCallFailed,
        Self::ApprovalRequested,
        Self::ApprovalResolved,
        Self::RunCompleted,
        Self::RunFailed,
        Self::RunCancelled,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            Self::RunQueued => "run.queued",
            Self::RunStarted => "run.started",
            Self::MessageAssistant => "message.assistant",
            Self::ToolCallStarted => "tool_call.started",
            Self::ToolCallCompleted => "tool_call.completed",
            Self::ToolCallFailed => "tool_call.failed",
            Self::ApprovalRequested => "approval.requested",
            Self::ApprovalResolved => "approval.resolved",
            Self::RunCompleted => "run.completed",
            Self::RunFailed => "run.failed",
            Self::RunCancelled => "run.cancelled",
        }
    }

    pub fn parse(value: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|kind| kind.as_str() == value)
    }
}

/// One event of a run's timeline. `sequence` counts from 1 within the run.
#[derive(Debug, Clone, PartialEq)]
pub struct RunEvent {
    pub id: RunEventId,
    pub organization_id: OrganizationId,
    pub workspace_id: WorkspaceId,
    pub run_id: RunId,
    pub sequence: u64,
    pub kind: RunEventType,
    pub data: Value,
    pub created_at: Timestamp,
}

/// Where a call stands.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CallState {
    /// The policy allows it; not sent yet.
    Allowed,
    /// Waiting for a person's decision.
    AwaitingApproval,
    /// A person approved it; not sent yet.
    Approved,
    /// A person denied it: never sent.
    Denied,
    /// The floor or the policy refused it, or it couldn't be sent: never sent.
    Refused,
    /// Recorded as sent; no outcome yet.
    Started,
    /// Sent, and it succeeded.
    Completed,
    /// Sent, and it failed.
    Failed,
    /// Sent, and its outcome is unknown: settled as such, never sent again.
    Unknown,
}

impl CallState {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Allowed => "allowed",
            Self::AwaitingApproval => "awaiting_approval",
            Self::Approved => "approved",
            Self::Denied => "denied",
            Self::Refused => "refused",
            Self::Started => "started",
            Self::Completed => "completed",
            Self::Failed => "failed",
            Self::Unknown => "unknown",
        }
    }

    pub fn parse(value: &str) -> Option<Self> {
        [
            Self::Allowed,
            Self::AwaitingApproval,
            Self::Approved,
            Self::Denied,
            Self::Refused,
            Self::Started,
            Self::Completed,
            Self::Failed,
            Self::Unknown,
        ]
        .into_iter()
        .find(|state| state.as_str() == value)
    }

    /// Settled: what the model reads back is known.
    pub fn is_settled(self) -> bool {
        matches!(
            self,
            Self::Denied | Self::Refused | Self::Completed | Self::Failed | Self::Unknown
        )
    }
}

/// A person's decision on a call, as an API key recorded it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ApprovalDecision {
    Approved,
    Denied,
}

impl ApprovalDecision {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Approved => "approved",
            Self::Denied => "denied",
        }
    }

    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "approved" => Some(Self::Approved),
            "denied" => Some(Self::Denied),
            _ => None,
        }
    }
}

/// What the model reads back for a call.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CallOutcome {
    pub content: String,
    pub is_error: bool,
}

/// A workspace's runs, filtered.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RunFilter {
    pub agent_id: Option<AgentId>,
    pub status: Option<RunStatus>,
}

/// An organization's runs, filtered.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct OrganizationRunFilter {
    pub environment: Option<Environment>,
    pub workspace_id: Option<WorkspaceId>,
    pub status: Option<RunStatus>,
}

/// One page, newest first.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Page<T> {
    pub items: Vec<T>,
    /// Opaque; the next page's `cursor`.
    pub next_cursor: Option<String>,
    pub total: u64,
}

/// An agent's runs, as its view counts them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AgentActivity {
    pub agent_id: AgentId,
    pub runs_since: u64,
    pub last_run_at: Option<Timestamp>,
}

/// Runs started on one UTC day, as usage counts them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct UsageDay {
    /// Days since 1970-01-01.
    pub day: i64,
    pub runs: u64,
    pub failed_runs: u64,
    /// Calls sent to an app's server, as each run counted them.
    pub tool_calls: u64,
}

/// The day an instant falls on, in days since 1970-01-01, UTC.
pub fn day_of(at: Timestamp) -> i64 {
    at.millis().div_euclid(86_400_000)
}
