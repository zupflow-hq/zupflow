//! Forgetting what went with an agent, a workspace or a memory type. Each
//! deletion queues, in its own transaction, a job for each memory kind of the
//! organization's types at that moment (the type's own kind, for a type),
//! naming what went by its IDs ([`FORGET_JOB`]). This worker asks the kind to
//! forget it ([`zupflow_core::memory::MemoryProvider::forget_all`]) until it
//! has: a kind that fails is asked again, backing off up to sixty minutes
//! apart. The job of a kind this server doesn't serve is kept, without using
//! an attempt, so the kind forgets once a server serves it again: it's
//! looked at again after five minutes, then twice as long each time, up to
//! six hours, with a warning each time. A kind keeps its memories in the
//! database that holds the server's records, so what the database drops
//! with the deletion goes in the deletion's own transaction; the job has the
//! kind forget whatever else it keeps of what went.

use std::time::Duration;

use zupflow_core::jobs::{Job, Retry};
use zupflow_core::memory::{FORGET_JOB, Forget, MemoryError};
use zupflow_core::scope::{Name, Scope};
use zupflow_core::time::Timestamp;

use crate::auth::AppState;

/// How long a claim holds a forget job: longer than a kind takes to forget.
const LEASE: Duration = Duration::from_secs(5 * 60);

/// How often an idle worker looks for work.
const IDLE: Duration = Duration::from_secs(1);

/// How long a job of a kind this server doesn't serve first waits before
/// it's looked at again.
const UNSERVED_WAIT: Duration = Duration::from_secs(5 * 60);

/// The longest a job of a kind this server doesn't serve waits.
const LONGEST_UNSERVED_WAIT: Duration = Duration::from_secs(6 * 60 * 60);

/// The longest a kind that fails waits to be asked again.
const LONGEST_WAIT: Duration = Duration::from_secs(60 * 60);

fn kind() -> Option<Name> {
    Name::new(FORGET_JOB).ok()
}

/// How long the attempt after attempt `attempt` (from 1) waits: half a
/// minute, doubling, up to [`LONGEST_WAIT`].
fn backoff(attempt: u32) -> Duration {
    let doublings = attempt.saturating_sub(1).min(16);
    Duration::from_secs(30 << doublings).min(LONGEST_WAIT)
}

/// How long a job of a kind this server doesn't serve waits, `waited` since
/// it was queued: [`UNSERVED_WAIT`] more than that, so five minutes, then
/// twice as long each time, up to [`LONGEST_UNSERVED_WAIT`].
fn unserved_wait(waited: Duration) -> Duration {
    waited.saturating_add(UNSERVED_WAIT).min(LONGEST_UNSERVED_WAIT)
}

/// Claims and handles one forget job, if one is due. Whether one was.
pub async fn work_once(state: &AppState) -> bool {
    let Some(kind) = kind() else { return false };
    // Cross-tenant by design: a worker takes the next job of any scope.
    let job = match state.jobs.claim_next(std::slice::from_ref(&kind), LEASE).await {
        Ok(Some(job)) => job,
        Ok(None) => return false,
        Err(error) => {
            tracing::warn!(event = "memory_forgetting.claim_failed", %error);
            return false;
        }
    };
    handle(state, job).await;
    true
}

async fn handle(state: &AppState, job: Job) {
    let Job {
        id,
        scope,
        payload,
        attempt,
        lease,
        queued_at,
        ..
    } = job;
    // A job is its organization's: it names nothing of another one.
    let named = Forget::of_job(&payload).filter(|(what, _)| scope == Scope::Organization(what.organization_id()));
    let Some((what, kind)) = named else {
        tracing::error!(event = "memory_forgetting.malformed_job", kind = FORGET_JOB);
        if let Ok(reason) = Name::new("malformed") {
            let _ = state.jobs.bury(&scope, id, &lease, &reason).await;
        }
        return;
    };
    let organization_id = what.organization_id();
    let finished = if state.memory_providers.serves(&kind) {
        match state.memory_providers.provider(&kind).forget_all(&what).await {
            Ok(()) => state.jobs.complete(&scope, id, &lease).await,
            Err(error) => {
                let reason = match error {
                    MemoryError::Unavailable => "kind_unavailable",
                    _ => "kind_refused",
                };
                match Name::new(reason) {
                    Ok(reason) => match state.jobs.retry(&scope, id, &lease, backoff(attempt), &reason).await {
                        Ok(Retry::Scheduled) => {
                            tracing::warn!(
                                event = "memory_forgetting.kind_failed",
                                %organization_id,
                                kind = kind.as_str(),
                                attempt,
                                %error,
                            );
                            Ok(())
                        }
                        Ok(Retry::Exhausted) => {
                            tracing::error!(
                                event = "memory_forgetting.given_up",
                                %organization_id,
                                kind = kind.as_str(),
                                attempt,
                                %error,
                            );
                            Ok(())
                        }
                        Err(error) => Err(error),
                    },
                    // Never: each reason is a name. Were one not, the job
                    // would wait rather than go unforgotten.
                    Err(_) => state.jobs.postpone(&scope, id, &lease, backoff(attempt)).await,
                }
            }
        }
    } else {
        let waited = Timestamp::now().millis().saturating_sub(queued_at.millis());
        let wait = unserved_wait(Duration::from_millis(u64::try_from(waited).unwrap_or(0)));
        tracing::warn!(
            event = "memory_forgetting.kind_not_served",
            %organization_id,
            kind = kind.as_str(),
            wait_seconds = wait.as_secs(),
        );
        state.jobs.postpone(&scope, id, &lease, wait).await
    };
    if let Err(error) = finished {
        tracing::warn!(
            event = "memory_forgetting.finish_failed",
            %organization_id,
            kind = kind.as_str(),
            %error,
        );
    }
}

/// Works forget jobs until the process stops.
pub async fn run_worker(state: AppState) {
    loop {
        if !work_once(&state).await {
            tokio::time::sleep(IDLE).await;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn backs_off_up_to_an_hour() {
        let waits: Vec<u64> = (1..=9).map(|attempt| backoff(attempt).as_secs()).collect();
        assert_eq!(waits, [30, 60, 120, 240, 480, 960, 1920, 3600, 3600]);
        assert_eq!(backoff(0), Duration::from_secs(30));
        assert_eq!(backoff(u32::MAX), LONGEST_WAIT);
        let total: u64 = (1..zupflow_core::memory::FORGET_ATTEMPTS)
            .map(|attempt| backoff(attempt).as_secs())
            .sum();
        assert!((72 * 3600..96 * 3600).contains(&total), "more than three days: {total}");
    }

    #[test]
    fn waits_for_a_kind_it_does_not_serve_twice_as_long_each_time_up_to_six_hours() {
        let mut waited = Duration::ZERO;
        let mut waits = Vec::new();
        for _ in 0..9 {
            let wait = unserved_wait(waited);
            waits.push(wait.as_secs() / 60);
            waited += wait;
        }
        assert_eq!(waits, [5, 10, 20, 40, 80, 160, 320, 360, 360]);
        assert_eq!(unserved_wait(Duration::MAX), LONGEST_UNSERVED_WAIT);
    }
}
