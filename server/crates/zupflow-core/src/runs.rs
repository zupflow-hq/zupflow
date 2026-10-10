//! Runs: an agent working on one input, step by step, and the timeline of
//! what it did (decision 0040; the backend standard's "Runs").
//!
//! A run advances as recorded steps, each one transaction of the
//! [`RunStore`] that checks the run's state and appends its events in order:
//!
//! 1. **Queued**, with its job, in the transaction that creates it.
//! 2. **Started** by the worker that claims the job.
//! 3. A **turn**: what the model answered, recorded with every tool call it
//!    asked for and the decision on each (allowed, waiting for approval, or
//!    refused with its reason).
//! 4. Each allowed or approved call is **begun** (recorded as sent) before
//!    it's sent and **settled** with its outcome after. A crash between the
//!    two leaves a call begun and never settled: its outcome is unknown, and
//!    it's settled as such, never sent again.
//! 5. A call that needs approval **pauses** the run until an API key
//!    **resolves** it; resolving queues the run's job again, in the same
//!    transaction.
//! 6. **Finished**: completed with the model's last text, failed, or cancelled.
//!
//! Every step that appends an event a webhook names (`run.completed`,
//! `run.failed`, `run.cancelled`, and `approval.requested` as
//! `run.waiting_for_approval`) queues its announcement
//! ([`crate::webhooks::Announcement`]) in the same transaction: the outbox.
//!
//! Every method takes the organization, then the workspace, and reaches
//! nothing outside them. A run outlives its agent: deleting an agent keeps
//! its runs, named "Deleted agent".

use async_trait::async_trait;
use serde_json::{Map, Value};

use crate::audit::Audit;
use crate::environment::Environment;
use crate::ids::{
    AgentId, ApiKeyId, InstallationId, OrganizationId, RunEventId, RunId, SessionId, ToolCallId, WorkspaceId,
};
use crate::jobs::JobOptions;
use crate::models::TokenUsage;
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

/// An event to append: its type and its data, as the contract's `RunEvent`
/// shapes it for that type.
#[derive(Debug, Clone, PartialEq)]
pub struct NewRunEvent {
    pub kind: RunEventType,
    pub data: Value,
}

impl NewRunEvent {
    pub fn new(kind: RunEventType, data: Value) -> Self {
        Self { kind, data }
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

/// The installation and tool a call reached, as it was named when the call
/// was decided.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CallTarget {
    pub installation_id: InstallationId,
    pub installation_label: String,
    pub app_name: String,
    pub tool: String,
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

/// The decision a policy made on a call, recorded with it (decision 0040).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RecordedDecision {
    /// `allow`, `require_approval` or `deny`.
    pub verdict: String,
    pub policy: String,
    /// `None` when the floor decided, or the mode couldn't be read.
    pub mode: Option<String>,
    pub rule: String,
    pub reason: Option<String>,
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

/// A tool call the model asked for, and where it stands.
#[derive(Debug, Clone, PartialEq)]
pub struct RunToolCall {
    pub id: ToolCallId,
    /// The turn that asked for it, from 0, and its place in that turn.
    pub turn: u32,
    pub position: u32,
    /// The model's own ID for it, which its result answers.
    pub model_call_id: String,
    /// The name the model called it by.
    pub name: String,
    /// `None` when the model named a tool it wasn't given.
    pub target: Option<CallTarget>,
    pub arguments: Map<String, Value>,
    pub state: CallState,
    pub decision: Option<RecordedDecision>,
    /// Whether the run paused for it: only then can it be resolved.
    pub approval_requested: bool,
    pub approval: Option<(ApprovalDecision, ApiKeyId)>,
    pub outcome: Option<CallOutcome>,
    pub started_at: Option<Timestamp>,
    pub finished_at: Option<Timestamp>,
}

/// A call as a turn records it.
#[derive(Debug, Clone, PartialEq)]
pub struct NewToolCall {
    pub id: ToolCallId,
    pub model_call_id: String,
    pub name: String,
    pub target: Option<CallTarget>,
    pub arguments: Map<String, Value>,
    /// `Allowed`, `AwaitingApproval` or `Refused`.
    pub state: CallState,
    pub decision: Option<RecordedDecision>,
    /// For a refused call, what the model reads back.
    pub outcome: Option<CallOutcome>,
}

/// One answer of the model: its text and the calls it asked for, in order.
#[derive(Debug, Clone, PartialEq)]
pub struct NewTurn {
    pub text: Option<String>,
    pub calls: Vec<NewToolCall>,
    pub usage: TokenUsage,
    /// Ends the run with the turn, in the same transaction: a final answer.
    pub finish: Option<Ending>,
}

/// A recorded turn: its text and its calls' IDs, in order.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct RunTurn {
    pub text: Option<String>,
    pub calls: Vec<ToolCallId>,
}

/// Everything the worker needs to take a run's next step.
#[derive(Debug, Clone, PartialEq)]
pub struct RunProgress {
    pub run: Run,
    pub model: String,
    pub instructions: String,
    pub turns: Vec<RunTurn>,
    /// Every call, by turn and position.
    pub calls: Vec<RunToolCall>,
}

/// How a run ends.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Ending {
    Completed { output: String },
    Failed(Failure),
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

#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum RunError {
    #[error("the store is unavailable")]
    Unavailable,
    /// The run isn't in this scope.
    #[error("run not found")]
    NotFound,
    /// The agent isn't in this scope.
    #[error("agent not found")]
    AgentNotFound,
    #[error("the cursor isn't one this list gave")]
    InvalidCursor,
    /// The run is completed, failed or cancelled.
    #[error("the run is finished")]
    Finished,
    /// The run isn't waiting for a decision on that call.
    #[error("the run isn't waiting for that call")]
    NotWaiting,
    /// The run or the call isn't where the step expects: another worker, a
    /// cancel or a decision moved it. The worker reads it again.
    #[error("the run moved on")]
    Moved,
}

#[async_trait]
pub trait RunStore: Send + Sync {
    /// Creates a queued run of an agent of the workspace (`AgentNotFound`
    /// otherwise), appends `events` and queues `job`, in one transaction.
    #[allow(clippy::too_many_arguments)]
    async fn create_run(
        &self,
        organization_id: OrganizationId,
        workspace_id: WorkspaceId,
        run_id: RunId,
        run: &NewRun,
        events: &[NewRunEvent],
        job: &QueuedJob,
        now: Timestamp,
    ) -> Result<(Run, Vec<RunEvent>), RunError>;

    async fn find_run(
        &self,
        organization_id: OrganizationId,
        workspace_id: WorkspaceId,
        run_id: RunId,
    ) -> Result<Option<Run>, RunError>;

    /// The workspace's runs, newest first; `limit` clamped to 1 to [`MAX_PAGE`].
    async fn list_runs(
        &self,
        organization_id: OrganizationId,
        workspace_id: WorkspaceId,
        filter: &RunFilter,
        cursor: Option<&str>,
        limit: u32,
    ) -> Result<Page<Run>, RunError>;

    /// The organization's runs across its workspaces, newest first.
    async fn list_organization_runs(
        &self,
        organization_id: OrganizationId,
        filter: &OrganizationRunFilter,
        cursor: Option<&str>,
        limit: u32,
    ) -> Result<Page<OrganizationRun>, RunError>;

    /// The run's events whose sequence is greater than `after`, in order.
    /// `NotFound` when the run isn't here.
    async fn list_events(
        &self,
        organization_id: OrganizationId,
        workspace_id: WorkspaceId,
        run_id: RunId,
        after: u64,
    ) -> Result<Vec<RunEvent>, RunError>;

    /// How many runs each of the workspace's agents started since `since`,
    /// and when its last one started; agents with none are left out.
    async fn agent_activity(
        &self,
        organization_id: OrganizationId,
        workspace_id: WorkspaceId,
        since: Timestamp,
    ) -> Result<Vec<AgentActivity>, RunError>;

    /// The organization's runs in workspaces of `environment` created at or
    /// after `since`, counted per UTC day, in order; a day without runs is
    /// left out.
    async fn usage(
        &self,
        organization_id: OrganizationId,
        environment: Environment,
        since: Timestamp,
    ) -> Result<Vec<UsageDay>, RunError>;

    /// The run with its turns and calls. `None` when it isn't here.
    async fn find_progress(
        &self,
        organization_id: OrganizationId,
        workspace_id: WorkspaceId,
        run_id: RunId,
    ) -> Result<Option<RunProgress>, RunError>;

    /// Queued to running. `Moved` unless it's queued.
    async fn start_run(
        &self,
        organization_id: OrganizationId,
        workspace_id: WorkspaceId,
        run_id: RunId,
        events: &[NewRunEvent],
        now: Timestamp,
    ) -> Result<Vec<RunEvent>, RunError>;

    /// Records turn `index` of a running run: its text, its calls with
    /// their decisions, and the tokens it took; and, with
    /// [`NewTurn::finish`], ends the run. `Moved` unless the run is running
    /// and has exactly `index` turns.
    #[allow(clippy::too_many_arguments)]
    async fn record_turn(
        &self,
        organization_id: OrganizationId,
        workspace_id: WorkspaceId,
        run_id: RunId,
        index: u32,
        turn: &NewTurn,
        events: &[NewRunEvent],
        now: Timestamp,
    ) -> Result<Vec<RunEvent>, RunError>;

    /// Pauses a running run on a call awaiting approval. `Moved` unless the
    /// run is running and the call awaits approval.
    async fn request_approval(
        &self,
        organization_id: OrganizationId,
        workspace_id: WorkspaceId,
        run_id: RunId,
        call_id: ToolCallId,
        events: &[NewRunEvent],
        now: Timestamp,
    ) -> Result<Vec<RunEvent>, RunError>;

    /// Records a person's decision on the call the run waits for, sets the
    /// run running and queues `job`, in one transaction. A denial settles
    /// the call with `denial` as what the model reads back. `NotFound`
    /// unless the run is here; `NotWaiting` unless it's waiting for
    /// approval of exactly this call. Records `audit` of the run, resolved.
    #[allow(clippy::too_many_arguments)]
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
        audit: &Audit<'_, Run>,
    ) -> Result<(Run, Vec<RunEvent>), RunError>;

    /// Records an allowed or approved call as sent, before it's sent, and
    /// counts it. `Moved` unless the run is running and the call is allowed
    /// or approved: a call is begun at most once.
    async fn begin_call(
        &self,
        organization_id: OrganizationId,
        workspace_id: WorkspaceId,
        run_id: RunId,
        call_id: ToolCallId,
        events: &[NewRunEvent],
        now: Timestamp,
    ) -> Result<Vec<RunEvent>, RunError>;

    /// Settles a call that is in one of `from` as `to` (refused, completed,
    /// failed or unknown) with what the model reads back. A sent call's
    /// outcome is recorded whatever the run's state. `Moved` unless the call
    /// is in one of `from`.
    #[allow(clippy::too_many_arguments)]
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
    ) -> Result<Vec<RunEvent>, RunError>;

    /// Records a new decision on an allowed or approved call before it's
    /// sent: `to` is `AwaitingApproval` (the call now needs a person's
    /// decision) or `Refused` (with what the model reads back), and the
    /// decision replaces the one recorded. `Moved` unless the run is
    /// running and the call is in one of `from`.
    #[allow(clippy::too_many_arguments)]
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
    ) -> Result<Vec<RunEvent>, RunError>;

    /// Ends a run that isn't finished (`Finished` otherwise) and has exactly
    /// `turns` turns (`Moved` otherwise: another step got there first).
    #[allow(clippy::too_many_arguments)]
    async fn finish_run(
        &self,
        organization_id: OrganizationId,
        workspace_id: WorkspaceId,
        run_id: RunId,
        turns: u32,
        ending: &Ending,
        events: &[NewRunEvent],
        now: Timestamp,
    ) -> Result<Vec<RunEvent>, RunError>;

    /// **Cross-tenant**, for the run worker's sweep: up to `limit` runs that
    /// are queued or running with no job left to take them forward (their
    /// last job died), each with its scope.
    async fn abandoned_runs(&self, limit: u32) -> Result<Vec<(OrganizationId, WorkspaceId, RunId)>, RunError>;

    /// Cancels a run that isn't finished (`Finished` otherwise). A call
    /// already sent isn't stopped; its outcome is still recorded. Records
    /// `audit` of the run, cancelled.
    #[allow(clippy::too_many_arguments)]
    async fn cancel_run(
        &self,
        organization_id: OrganizationId,
        workspace_id: WorkspaceId,
        run_id: RunId,
        events: &[NewRunEvent],
        now: Timestamp,
        audit: &Audit<'_, Run>,
    ) -> Result<(Run, Vec<RunEvent>), RunError>;
}
