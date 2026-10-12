//! The durable engine's store (decision 0046): [`DurableRunStore`].
//!
//! [`DurableRunStore::apply`] is the only way the durable engine changes a
//! run. It takes the organization and the workspace the caller acts in and
//! one [`TransitionRequest`], and answers a [`Receipt`] or a [`Conflict`].
//! Every adapter, in one transaction under its write lock:
//!
//! 1. Refuses, as [`StoreError::NotFound`] and changing nothing, a request
//!    whose organization or workspace isn't `apply`'s, or whose ID isn't
//!    derived from that scope, its run and its starting revision.
//! 2. Answers the first receipt again when a transition with the request's
//!    ID and digest already committed, and [`Conflict::Revision`] when one
//!    with its ID and another digest did.
//! 3. Reads the time from its own clock, after taking the lock, and checks
//!    the request's guards on it: the expected revision; the owner's epoch,
//!    claimant and live lease; the send claim and its lease; the cancel flag;
//!    no live claim; no live send. A guard that fails answers its conflict.
//! 4. Refuses ([`StoreError::Refused`]) a request that doesn't fit the run
//!    as committed, or that leaves out a guard its operation needs.
//! 5. Writes everything the operation implies: the run and its
//!    continuation, turns, calls, attempts, the run events it derives (with
//!    their gap-free sequence), the webhook announcements those events name
//!    (the outbox), the audit row of a control request, and the run's job
//!    (queued, postponed, retried or released), with the transition itself.
//!    All of it commits, or none of it.
//!
//! The reads are the ones the API and the driver need: a run's snapshot
//! (what [`super::reducer::advance`] reads), the run as the API shows it,
//! lists, the timeline, activity and usage. The one statement without a
//! scope is [`DurableRunStore::abandoned_runs`], for the sweep.
//!
//! The suite every adapter passes is `zupflow-conformance::durable_run_store`.

use std::time::Duration;

use async_trait::async_trait;
use serde_json::{Value, json};

use crate::environment::Environment;
use crate::ids::{OrganizationId, RunId, WorkspaceId};
use crate::jobs::JobOptions;
use crate::runs::state::{Continuation, Snapshot};
use crate::runs::transition::{Conflict, TransitionId, TransitionRequest};
use crate::runs::{
    AgentActivity, OrganizationRun, OrganizationRunFilter, Page, QueuedJob, Run, RunEvent, RunFilter, RunStatus,
    UsageDay,
};
use crate::scope::Name;
use crate::time::Timestamp;

/// The kind of the job that takes a run forward.
pub const RUN_JOB: &str = "run_agent";

/// How many times a run's job is tried. A yield gives its attempt back; a
/// model that gives no answer uses one.
pub const RUN_JOB_ATTEMPTS: u32 = 5;

/// How long a run's job waits, per attempt it used, before asking a model
/// that gave no answer again.
pub const MODEL_RETRY_STEP: Duration = Duration::from_secs(5);

/// The job that takes `run_id` forward: its payload names the run only.
pub fn run_job(run_id: RunId) -> Option<QueuedJob> {
    Some(QueuedJob {
        kind: Name::new(RUN_JOB).ok()?,
        payload: json!({ "run_id": run_id }).to_string().into_bytes(),
        options: JobOptions {
            delay: Duration::ZERO,
            max_attempts: RUN_JOB_ATTEMPTS,
        },
    })
}

/// The run a run job's payload names.
pub fn run_of_job(payload: &[u8]) -> Option<RunId> {
    let payload: Value = serde_json::from_slice(payload).ok()?;
    RunId::parse(payload.get("run_id")?.as_str()?).ok()
}

/// What a committed transition did. Answered again, the same, to a retry of
/// the same request.
#[derive(Debug, Clone, PartialEq)]
pub struct Receipt {
    pub transition_id: TransitionId,
    /// The operation's stable name ([`crate::runs::transition::Operation::name`]).
    pub operation: String,
    pub from_revision: u64,
    pub to_revision: u64,
    /// The run's public status after it.
    pub status: RunStatus,
    pub continuation: Continuation,
    /// Until when the run's owner holds it after it, on the store's clock;
    /// `None` when no worker does.
    pub lease_until: Option<Timestamp>,
    /// The store's time when it committed.
    pub committed_at: Timestamp,
    /// The events it appended, in order: what to publish once it commits.
    pub events: Vec<RunEvent>,
}

/// What `apply` answers.
#[derive(Debug, Clone, PartialEq)]
pub enum Applied {
    /// Committed now, or earlier by the same request.
    Committed(Receipt),
    /// Refused by a guard, or by another request with the same ID: nothing
    /// changed. [`Conflict::reload`] says whether to reload and compute again.
    Conflict(Conflict),
}

impl Applied {
    pub fn receipt(&self) -> Option<&Receipt> {
        match self {
            Self::Committed(receipt) => Some(receipt),
            Self::Conflict(_) => None,
        }
    }

    pub fn conflict(&self) -> Option<Conflict> {
        match self {
            Self::Committed(_) => None,
            Self::Conflict(conflict) => Some(*conflict),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum StoreError {
    #[error("the store is unavailable")]
    Unavailable,
    /// The run isn't in this scope, or the request's scope or ID isn't
    /// `apply`'s. Nothing changed.
    #[error("run not found")]
    NotFound,
    /// An admission names an agent that isn't in the workspace.
    #[error("agent not found")]
    AgentNotFound,
    #[error("the cursor isn't one this list gave")]
    InvalidCursor,
    /// The request doesn't fit the run as committed, leaves out a guard its
    /// operation needs, or breaks a rule the store keeps. Nothing changed. A
    /// caller's bug, never a product outcome.
    #[error("the store refused the request: {0}")]
    Refused(&'static str),
    /// A run this build's engine doesn't advance: another engine or another
    /// execution version created it.
    #[error("the run isn't one this engine advances")]
    Unsupported,
}

/// How safely an adapter keeps what it commits.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Durability {
    /// A transition is on stable storage before `apply` answers, and
    /// survives a power cut (SQLite: WAL with `synchronous = FULL`).
    Synchronous,
    /// A transition `apply` answered can be lost in a crash. The server
    /// refuses such a store.
    Relaxed,
}

#[async_trait]
pub trait DurableRunStore: Send + Sync {
    /// How safely it keeps what it commits.
    fn durability(&self) -> Durability;

    /// Applies one transition to a run of the workspace, as described above.
    async fn apply(
        &self,
        organization_id: OrganizationId,
        workspace_id: WorkspaceId,
        request: &TransitionRequest,
    ) -> Result<Applied, StoreError>;

    /// The run as committed, with its turns, calls, attempts and model
    /// attempts: what the reducer reads. `None` when it isn't here;
    /// [`StoreError::Unsupported`] for a run this engine doesn't advance.
    async fn find_snapshot(
        &self,
        organization_id: OrganizationId,
        workspace_id: WorkspaceId,
        run_id: RunId,
    ) -> Result<Option<Snapshot>, StoreError>;

    async fn find_run(
        &self,
        organization_id: OrganizationId,
        workspace_id: WorkspaceId,
        run_id: RunId,
    ) -> Result<Option<Run>, StoreError>;

    /// The workspace's runs, newest first; `limit` clamped to 1 to
    /// [`crate::runs::MAX_PAGE`].
    async fn list_runs(
        &self,
        organization_id: OrganizationId,
        workspace_id: WorkspaceId,
        filter: &RunFilter,
        cursor: Option<&str>,
        limit: u32,
    ) -> Result<Page<Run>, StoreError>;

    /// The organization's runs across its workspaces, newest first.
    async fn list_organization_runs(
        &self,
        organization_id: OrganizationId,
        filter: &OrganizationRunFilter,
        cursor: Option<&str>,
        limit: u32,
    ) -> Result<Page<OrganizationRun>, StoreError>;

    /// The run's events whose sequence is greater than `after`, in order.
    /// `NotFound` when the run isn't here.
    async fn list_events(
        &self,
        organization_id: OrganizationId,
        workspace_id: WorkspaceId,
        run_id: RunId,
        after: u64,
    ) -> Result<Vec<RunEvent>, StoreError>;

    /// How many runs each of the workspace's agents started since `since`,
    /// and when its last one started; agents with none are left out.
    async fn agent_activity(
        &self,
        organization_id: OrganizationId,
        workspace_id: WorkspaceId,
        since: Timestamp,
    ) -> Result<Vec<AgentActivity>, StoreError>;

    /// The organization's runs in workspaces of `environment` created at or
    /// after `since`, counted per UTC day, in order.
    async fn usage(
        &self,
        organization_id: OrganizationId,
        environment: Environment,
        since: Timestamp,
    ) -> Result<Vec<UsageDay>, StoreError>;

    /// **Cross-tenant**, for the sweep: up to `limit` runs this engine
    /// advances that are queued or running with no job left to take them
    /// forward (their last job died), each with its scope.
    async fn abandoned_runs(&self, limit: u32) -> Result<Vec<(OrganizationId, WorkspaceId, RunId)>, StoreError>;
}

#[cfg(test)]
mod tests {
    use uuid::Uuid;

    use super::*;

    #[test]
    fn a_run_job_names_its_run_and_nothing_else() {
        let run_id = RunId::from_uuid(Uuid::from_u128(3));
        let job = run_job(run_id).expect("a job");
        assert_eq!(job.kind.as_str(), RUN_JOB);
        assert_eq!(job.options.max_attempts, RUN_JOB_ATTEMPTS);
        assert_eq!(run_of_job(&job.payload), Some(run_id));
        assert_eq!(
            serde_json::from_slice::<Value>(&job.payload).expect("JSON"),
            json!({ "run_id": run_id.to_string() })
        );
        assert_eq!(run_of_job(b"{}"), None);
        assert_eq!(run_of_job(br#"{"run_id":"agt_031DCp7XMZ9AHIB2uF6QgS"}"#), None);
    }
}
