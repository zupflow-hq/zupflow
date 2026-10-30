//! The job queue: background work that must happen even if the process
//! stops (delivering a webhook, say), done by workers inside the server.
//!
//! **Scope.** A job belongs to the scope it was enqueued in, and a worker gets
//! that scope back with the job. Claiming is the one operation that starts
//! without a scope: a worker takes the next job of any organization, which is
//! why [`JobQueue::claim_next`] is named and documented as cross-tenant.
//! Everything after it (completing, retrying, postponing, burying,
//! extending) names the job's scope again, and an adapter matches it exactly,
//! so a job ID and lease used from any other scope reach nothing.
//!
//! **Leases.** A claim holds the job for a lease. A worker that crashes or
//! stalls loses it when the lease runs out, and the job is claimed again as its
//! next attempt; the old holder's lease then reaches nothing. So delivery is at
//! least once: a handler is idempotent, or records what it did before acting.
//! A lease that ran out reaches nothing even before the job is claimed again,
//! so a stalled worker can't finish, retry or revive a job it no longer holds.
//! A job whose last attempt's lease runs out is dead, never claimed again.
//!
//! **Payloads** carry references (an event's ID, say), not secrets or bulk
//! data: at most [`MAX_JOB_BYTES`], and every adapter accepts up to that.

use std::fmt;
use std::time::Duration;

use async_trait::async_trait;
use uuid::Uuid;

use crate::scope::{Name, Scope};
use crate::time::Timestamp;

/// Every adapter accepts payloads up to this size and refuses larger ones.
pub const MAX_JOB_BYTES: usize = 64 << 10;

/// The most kinds one claim can ask for; any beyond are ignored.
pub const MAX_CLAIM_KINDS: usize = 100;

/// The most attempts a job can be given.
pub const MAX_ATTEMPTS: u32 = 100;

/// The shortest and longest lease a claim can take; anything outside is
/// clamped into this range.
pub const MIN_LEASE: Duration = Duration::from_millis(100);
pub const MAX_LEASE: Duration = Duration::from_secs(24 * 60 * 60);

/// A job's identifier. Internal: jobs never appear in the API, so it has no
/// prefix and isn't in the ID registry.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct JobId(Uuid);

impl JobId {
    pub fn generate() -> Self {
        Self(Uuid::now_v7())
    }

    /// For row mapping only.
    pub fn from_uuid(uuid: Uuid) -> Self {
        Self(uuid)
    }

    pub fn uuid(&self) -> Uuid {
        self.0
    }
}

/// Proof of holding a job: a random value given at each claim. Only the
/// current holder's token reaches the job.
#[derive(Clone, PartialEq, Eq)]
pub struct LeaseToken([u8; 16]);

impl LeaseToken {
    /// A fresh random token.
    pub fn generate() -> Result<Self, JobError> {
        let mut bytes = [0u8; 16];
        getrandom::fill(&mut bytes).map_err(|_| JobError::Unavailable)?;
        Ok(Self(bytes))
    }

    /// For row mapping only.
    pub fn from_bytes(bytes: [u8; 16]) -> Self {
        Self(bytes)
    }

    pub fn as_bytes(&self) -> &[u8; 16] {
        &self.0
    }
}

impl fmt::Debug for LeaseToken {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("LeaseToken(..)")
    }
}

/// How a job is enqueued.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct JobOptions {
    /// How long before it can first be claimed.
    pub delay: Duration,
    /// How many times it's tried, 1 to [`MAX_ATTEMPTS`]; clamped into that range.
    pub max_attempts: u32,
}

impl Default for JobOptions {
    fn default() -> Self {
        Self {
            delay: Duration::ZERO,
            max_attempts: 3,
        }
    }
}

/// A claimed job.
#[derive(Debug, Clone)]
pub struct Job {
    pub id: JobId,
    /// The scope it was enqueued in, exactly.
    pub scope: Scope,
    pub kind: Name,
    pub payload: Vec<u8>,
    /// Which attempt this claim is, from 1.
    pub attempt: u32,
    pub lease: LeaseToken,
    /// When it was queued: the same for every claim.
    pub queued_at: Timestamp,
}

/// What [`JobQueue::retry`] did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Retry {
    /// It will be claimed again after the delay.
    Scheduled,
    /// That was its last attempt; it's dead.
    Exhausted,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum JobError {
    #[error("the job queue is unavailable")]
    Unavailable,
    #[error("the job's payload is larger than a job carries")]
    TooLarge,
    /// The lease doesn't hold this job in this scope: it ran out (whether or
    /// not the job was claimed again), the job is finished, or the scope, ID or
    /// token is wrong. Deliberately one answer for all of them.
    #[error("the lease no longer holds this job")]
    LeaseLost,
}

#[async_trait]
pub trait JobQueue: Send + Sync {
    /// Adds a job of `kind` in `scope`. In the database-backed queue it's
    /// durable once this returns.
    async fn enqueue(
        &self,
        scope: &Scope,
        kind: &Name,
        payload: Vec<u8>,
        options: JobOptions,
    ) -> Result<JobId, JobError>;

    /// **Cross-tenant.** Claims the next due job of one of `kinds` (the first
    /// [`MAX_CLAIM_KINDS`]), from any scope, for `lease`: a queued job whose delay has passed, or a claimed
    /// one whose lease ran out with attempts left. `None` when there's none.
    /// Nothing else in the queue starts without a scope.
    async fn claim_next(&self, kinds: &[Name], lease: Duration) -> Result<Option<Job>, JobError>;

    /// Finishes a job; it's never claimed again.
    async fn complete(&self, scope: &Scope, id: JobId, lease: &LeaseToken) -> Result<(), JobError>;

    /// Releases a job to be claimed again after `after`, or, if this was its
    /// last attempt, buries it with `reason`.
    async fn retry(
        &self,
        scope: &Scope,
        id: JobId,
        lease: &LeaseToken,
        after: Duration,
        reason: &Name,
    ) -> Result<Retry, JobError>;

    /// Releases a job to be claimed again after `after` without using up an
    /// attempt: for a handler that didn't get to try, because something it
    /// waits its turn for was busy. A job only ever postponed is never
    /// finished, so postpone only for a wait that ends.
    async fn postpone(&self, scope: &Scope, id: JobId, lease: &LeaseToken, after: Duration) -> Result<(), JobError>;

    /// Gives up on a job now, keeping `reason` (a short code, never a message
    /// that could hold a secret).
    async fn bury(&self, scope: &Scope, id: JobId, lease: &LeaseToken, reason: &Name) -> Result<(), JobError>;

    /// Renews the lease to `lease` from now, for work that runs long.
    async fn extend(&self, scope: &Scope, id: JobId, lease_token: &LeaseToken, lease: Duration)
    -> Result<(), JobError>;
}
