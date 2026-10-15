//! A channel run's answer, posted to its thread.
//!
//! The transaction that ends a channel run with an answer queues the reply
//! job ([`REPLY_JOB`]), naming the run only. The job reads where the answer
//! goes ([`ChannelReply`]), opens the connection's token and posts it
//! through the provider, carrying the run's ID; then records the post as
//! the connection's outbound message, with the run and the hops that led
//! to it, so another agent of the workspace reading it knows how far the
//! exchange has gone. Delivery is at least once: a run whose answer is
//! recorded is done, and a post whose answer was lost is looked for before
//! it's sent again. Nothing here changes what a run was sent: the answer is
//! in its transcript already.

use std::time::Duration;

use serde_json::{Value, json};

use super::{ConnectionStatus, ConversationKind};
use crate::ids::{ChannelConnectionId, ChannelMessageId, RunId};
use crate::jobs::JobOptions;
use crate::runs::QueuedJob;
use crate::scope::Name;
use crate::time::Timestamp;

/// The kind of the job that posts a channel run's answer.
pub const REPLY_JOB: &str = "channel_reply";

/// How many times the reply job is tried; a rate limit uses none.
pub const REPLY_JOB_ATTEMPTS: u32 = 6;

/// The most people one message has read from the provider: its author and
/// the first people it mentions.
pub const MAX_PEOPLE_READ: usize = 10;

/// How long what the provider said about a person holds: a person it
/// didn't know is asked about again after it, and so is the email of an
/// author a sender list is checked against. A name, once known, is kept.
pub const PERSON_LOOKUP_TTL: Duration = Duration::from_secs(24 * 60 * 60);

/// How long after its run ended an answer still waits out the provider's
/// rate limits: past it, the reply is given up as `rate_limited`.
pub const REPLY_RATE_LIMIT_WINDOW: Duration = Duration::from_secs(24 * 60 * 60);

/// Whether what was looked up at `looked_up_at` is due to be looked up
/// again at `now`.
pub fn lookup_stale(looked_up_at: Option<Timestamp>, now: Timestamp) -> bool {
    looked_up_at.is_none_or(|at| now.millis().saturating_sub(at.millis()) >= ttl_millis(PERSON_LOOKUP_TTL))
}

/// Whether a run that ended at `finished_at` has waited out rate limits
/// for longer than [`REPLY_RATE_LIMIT_WINDOW`] at `now`.
pub fn reply_expired(finished_at: Timestamp, now: Timestamp) -> bool {
    now.millis().saturating_sub(finished_at.millis()) > ttl_millis(REPLY_RATE_LIMIT_WINDOW)
}

fn ttl_millis(ttl: Duration) -> i64 {
    i64::try_from(ttl.as_millis()).unwrap_or(i64::MAX)
}

/// The reply job for a run that ended with an answer.
pub fn reply_job(run_id: RunId) -> Option<QueuedJob> {
    Some(QueuedJob {
        kind: Name::new(REPLY_JOB).ok()?,
        payload: json!({ "run_id": run_id }).to_string().into_bytes(),
        options: JobOptions {
            delay: Duration::ZERO,
            max_attempts: REPLY_JOB_ATTEMPTS,
        },
    })
}

/// The run a reply job's payload names.
pub fn run_of_job(payload: &[u8]) -> Option<RunId> {
    let payload: Value = serde_json::from_slice(payload).ok()?;
    RunId::parse(payload.get("run_id")?.as_str()?).ok()
}

/// Where a channel run's answer goes, and what it says, read in the run's
/// scope.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChannelReply {
    pub connection_id: ChannelConnectionId,
    pub connection_status: ConnectionStatus,
    pub conversation_external_id: String,
    pub conversation_kind: ConversationKind,
    pub conversation_is_external: bool,
    /// The thread it answers in; `""` at a DM's top level.
    pub thread_key: String,
    /// The run's answer.
    pub text: String,
    /// One more than the most hops that led to a message the run took in.
    pub agent_hop: u32,
    /// When the run ended: a lost post is looked for from then.
    pub finished_at: Timestamp,
    /// The message the connection recorded posting it, once it has.
    pub sent: Option<ChannelMessageId>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn queues_a_job_naming_the_run_only() {
        let id = RunId::generate();
        let job = reply_job(id).expect("a job");
        assert_eq!(job.kind.as_str(), REPLY_JOB);
        assert_eq!(run_of_job(&job.payload), Some(id));
        assert_eq!(run_of_job(br#"{"run_id":"nope"}"#), None);
    }

    #[test]
    fn looks_people_up_again_after_a_day() {
        let at = Timestamp::from_millis(1_000_000);
        let day = 24 * 60 * 60 * 1000;
        assert!(lookup_stale(None, at));
        assert!(!lookup_stale(Some(at), Timestamp::from_millis(1_000_000 + day - 1)));
        assert!(lookup_stale(Some(at), Timestamp::from_millis(1_000_000 + day)));
        assert!(!reply_expired(at, Timestamp::from_millis(1_000_000 + day)));
        assert!(reply_expired(at, Timestamp::from_millis(1_000_000 + day + 1)));
    }
}
