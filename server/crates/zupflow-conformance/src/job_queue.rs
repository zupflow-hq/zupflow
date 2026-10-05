//! The `JobQueue` suite: a job is claimed by one worker at a time, in its own
//! scope, and finished, retried or buried only by the lease that holds it,
//! named with that scope. Nothing reaches a job from another scope, with
//! another lease, or after its lease was lost.
//!
//! Every test uses kinds of its own, so the cross-tenant claim of one test
//! never takes another's jobs. Lease expiry and delays use real time with
//! one-second margins, so a queue whose clock is a server's still passes.
//!
//! `prepare` is called with each workspace scope a test enqueues in (its
//! organization's scopes included), for an adapter whose storage needs the
//! organization and workspace to exist, like the database-backed queue's
//! foreign keys. Others pass a no-op.

use std::collections::HashSet;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;

use zupflow_core::environment::Environment;
use zupflow_core::ids::{OrganizationId, WorkspaceId};
use zupflow_core::jobs::{Job, JobError, JobId, JobOptions, JobQueue, LeaseToken, MAX_JOB_BYTES, Retry};
use zupflow_core::scope::{Name, Scope};

/// Makes a scope's organization and workspace exist, where the queue needs them.
pub type Prepare = Arc<dyn Fn(Scope) -> Pin<Box<dyn Future<Output = ()> + Send>> + Send + Sync>;

const LEASE: Duration = Duration::from_secs(1);
const PAST_LEASE: Duration = Duration::from_millis(2000);
const LONG: Duration = Duration::from_secs(60);

fn name(value: &str) -> Name {
    Name::new(value).unwrap()
}

/// A kind no other test, or earlier run, uses.
fn fresh_kind(label: &str) -> Name {
    name(&format!("test.{label}.{}", WorkspaceId::generate().uuid().simple()))
}

struct Fixture {
    org: OrganizationId,
    other_org: OrganizationId,
    ws: WorkspaceId,
    sibling: WorkspaceId,
}

impl Fixture {
    async fn new(prepare: &Prepare) -> Self {
        let fixture = Self {
            org: OrganizationId::generate(),
            other_org: OrganizationId::generate(),
            ws: WorkspaceId::generate(),
            sibling: WorkspaceId::generate(),
        };
        for scope in [
            Scope::Workspace(fixture.org, fixture.ws),
            Scope::Workspace(fixture.org, fixture.sibling),
        ] {
            prepare(scope).await;
        }
        fixture
    }

    fn workspace(&self) -> Scope {
        Scope::Workspace(self.org, self.ws)
    }
}

async fn claim(queue: &dyn JobQueue, kind: &Name) -> Option<Job> {
    queue.claim_next(std::slice::from_ref(kind), LEASE).await.unwrap()
}

pub async fn run(queue: &dyn JobQueue, prepare: Prepare) {
    runs_a_job_once(queue, &prepare).await;
    returns_each_scope_exactly(queue, &prepare).await;
    claims_only_the_kinds_asked_for(queue, &prepare).await;
    waits_out_a_delay(queue, &prepare).await;
    carries_payloads_up_to_the_limit(queue, &prepare).await;
    never_gives_one_job_to_two_workers(queue, &prepare).await;
    reclaims_after_a_lost_lease_and_refuses_the_old_holder(queue, &prepare).await;
    refuses_a_lease_that_ran_out_before_anyone_reclaimed(queue, &prepare).await;
    keeps_a_lease_that_was_extended(queue, &prepare).await;
    retries_until_the_attempts_run_out(queue, &prepare).await;
    buries_on_request(queue, &prepare).await;
    lets_a_job_die_when_its_last_lease_runs_out(queue, &prepare).await;
    refuses_a_lease_named_with_another_scope(queue, &prepare).await;
}

async fn runs_a_job_once(queue: &dyn JobQueue, prepare: &Prepare) {
    let fixture = Fixture::new(prepare).await;
    let kind = fresh_kind("once");
    let id = queue
        .enqueue(&fixture.workspace(), &kind, b"payload".to_vec(), JobOptions::default())
        .await
        .unwrap();
    let job = claim(queue, &kind).await.expect("the job is due");
    assert_eq!(job.id, id);
    assert_eq!(job.scope, fixture.workspace());
    assert_eq!(job.kind, kind);
    assert_eq!(job.payload, b"payload");
    assert_eq!(job.attempt, 1);
    assert!(claim(queue, &kind).await.is_none(), "a held job isn't claimed twice");
    queue.complete(&job.scope, job.id, &job.lease).await.unwrap();
    assert!(claim(queue, &kind).await.is_none(), "a completed job is gone");
    assert_eq!(
        queue.complete(&job.scope, job.id, &job.lease).await,
        Err(JobError::LeaseLost),
        "and can't be completed again"
    );
}

async fn returns_each_scope_exactly(queue: &dyn JobQueue, prepare: &Prepare) {
    let fixture = Fixture::new(prepare).await;
    let kind = fresh_kind("scopes");
    let scopes = [
        Scope::Organization(fixture.org),
        Scope::Environment(fixture.org, Environment::Sandbox),
        Scope::Environment(fixture.org, Environment::Production),
        Scope::Workspace(fixture.org, fixture.ws),
        Scope::Workspace(fixture.org, fixture.sibling),
    ];
    for (index, scope) in scopes.iter().enumerate() {
        queue
            .enqueue(scope, &kind, vec![u8::try_from(index).unwrap()], JobOptions::default())
            .await
            .unwrap();
    }
    for _ in &scopes {
        let job = claim(queue, &kind).await.expect("every job is due");
        assert_eq!(
            job.scope,
            scopes[usize::from(job.payload[0])],
            "the job keeps its scope"
        );
        queue.complete(&job.scope, job.id, &job.lease).await.unwrap();
    }
}

async fn claims_only_the_kinds_asked_for(queue: &dyn JobQueue, prepare: &Prepare) {
    let fixture = Fixture::new(prepare).await;
    let (wanted, other) = (fresh_kind("wanted"), fresh_kind("other"));
    queue
        .enqueue(&fixture.workspace(), &other, vec![], JobOptions::default())
        .await
        .unwrap();
    assert!(claim(queue, &wanted).await.is_none());
    assert!(
        queue.claim_next(&[], LEASE).await.unwrap().is_none(),
        "no kinds, no job"
    );
    let job = queue
        .claim_next(&[wanted.clone(), other.clone()], LEASE)
        .await
        .unwrap()
        .expect("one of the kinds asked for");
    assert_eq!(job.kind, other);
    queue.complete(&job.scope, job.id, &job.lease).await.unwrap();
}

async fn waits_out_a_delay(queue: &dyn JobQueue, prepare: &Prepare) {
    let fixture = Fixture::new(prepare).await;
    let kind = fresh_kind("delay");
    let options = JobOptions {
        delay: Duration::from_secs(1),
        ..JobOptions::default()
    };
    queue
        .enqueue(&fixture.workspace(), &kind, vec![], options)
        .await
        .unwrap();
    assert!(claim(queue, &kind).await.is_none(), "not due yet");
    tokio::time::sleep(PAST_LEASE).await;
    let job = claim(queue, &kind).await.expect("due after its delay");
    queue.complete(&job.scope, job.id, &job.lease).await.unwrap();
}

async fn carries_payloads_up_to_the_limit(queue: &dyn JobQueue, prepare: &Prepare) {
    let fixture = Fixture::new(prepare).await;
    let kind = fresh_kind("size");
    let payload: Vec<u8> = (0..MAX_JOB_BYTES).map(|n| (n % 251) as u8).collect();
    queue
        .enqueue(&fixture.workspace(), &kind, payload.clone(), JobOptions::default())
        .await
        .unwrap();
    assert_eq!(
        queue
            .enqueue(
                &fixture.workspace(),
                &kind,
                vec![0; MAX_JOB_BYTES + 1],
                JobOptions::default()
            )
            .await,
        Err(JobError::TooLarge)
    );
    let job = claim(queue, &kind).await.unwrap();
    assert_eq!(job.payload, payload, "byte for byte");
    queue.complete(&job.scope, job.id, &job.lease).await.unwrap();
    assert!(claim(queue, &kind).await.is_none(), "the refused job wasn't enqueued");
}

async fn never_gives_one_job_to_two_workers(queue: &dyn JobQueue, prepare: &Prepare) {
    let fixture = Fixture::new(prepare).await;
    let kind = fresh_kind("race");
    let mut enqueued = HashSet::new();
    for n in 0..40u8 {
        enqueued.insert(
            queue
                .enqueue(&fixture.workspace(), &kind, vec![n], JobOptions::default())
                .await
                .unwrap(),
        );
    }
    // Eight workers claim at once until nothing is left.
    let claimed: Vec<JobId> = {
        let workers = (0..8).map(|_| async {
            let mut mine = Vec::new();
            while let Some(job) = queue.claim_next(std::slice::from_ref(&kind), LONG).await.unwrap() {
                mine.push(job.id);
            }
            mine
        });
        futures_join(workers).await.into_iter().flatten().collect()
    };
    let unique: HashSet<JobId> = claimed.iter().copied().collect();
    assert_eq!(unique.len(), claimed.len(), "no job claimed twice");
    assert_eq!(unique, enqueued, "every job claimed");
}

/// Runs futures concurrently on the current task and collects their outputs.
async fn futures_join<F: Future>(futures: impl Iterator<Item = F>) -> Vec<F::Output> {
    let mut pending: Vec<Pin<Box<F>>> = futures.map(Box::pin).collect();
    let mut outputs: Vec<Option<F::Output>> = pending.iter().map(|_| None).collect();
    std::future::poll_fn(|cx| {
        let mut done = true;
        for (future, output) in pending.iter_mut().zip(outputs.iter_mut()) {
            if output.is_none() {
                match future.as_mut().poll(cx) {
                    std::task::Poll::Ready(value) => *output = Some(value),
                    std::task::Poll::Pending => done = false,
                }
            }
        }
        if done {
            std::task::Poll::Ready(())
        } else {
            std::task::Poll::Pending
        }
    })
    .await;
    outputs.into_iter().map(Option::unwrap).collect()
}

async fn reclaims_after_a_lost_lease_and_refuses_the_old_holder(queue: &dyn JobQueue, prepare: &Prepare) {
    let fixture = Fixture::new(prepare).await;
    let kind = fresh_kind("reclaim");
    queue
        .enqueue(&fixture.workspace(), &kind, vec![], JobOptions::default())
        .await
        .unwrap();
    let first = claim(queue, &kind).await.unwrap();
    tokio::time::sleep(PAST_LEASE).await;
    let second = claim(queue, &kind).await.expect("claimed again once the lease ran out");
    assert_eq!(second.id, first.id);
    assert_eq!(second.attempt, 2, "as its next attempt");
    assert_ne!(second.lease, first.lease, "with a new lease");
    let reason = name("stale");
    assert_eq!(
        queue.complete(&first.scope, first.id, &first.lease).await,
        Err(JobError::LeaseLost)
    );
    assert_eq!(
        queue
            .retry(&first.scope, first.id, &first.lease, Duration::ZERO, &reason)
            .await,
        Err(JobError::LeaseLost)
    );
    assert_eq!(
        queue.bury(&first.scope, first.id, &first.lease, &reason).await,
        Err(JobError::LeaseLost)
    );
    assert_eq!(
        queue.extend(&first.scope, first.id, &first.lease, LONG).await,
        Err(JobError::LeaseLost)
    );
    queue
        .complete(&second.scope, second.id, &second.lease)
        .await
        .expect("the current holder still finishes it");
}

/// A lease that ran out reaches nothing, even with no other worker about:
/// the stalled holder can't finish, retry, bury or revive the job.
async fn refuses_a_lease_that_ran_out_before_anyone_reclaimed(queue: &dyn JobQueue, prepare: &Prepare) {
    let fixture = Fixture::new(prepare).await;
    let kind = fresh_kind("expired");
    let reason = name("late");
    queue
        .enqueue(&fixture.workspace(), &kind, vec![], JobOptions::default())
        .await
        .unwrap();
    let job = claim(queue, &kind).await.unwrap();
    tokio::time::sleep(PAST_LEASE).await;
    assert_eq!(
        queue.extend(&job.scope, job.id, &job.lease, LONG).await,
        Err(JobError::LeaseLost),
        "extend"
    );
    assert_eq!(
        queue.complete(&job.scope, job.id, &job.lease).await,
        Err(JobError::LeaseLost),
        "complete"
    );
    assert_eq!(
        queue
            .retry(&job.scope, job.id, &job.lease, Duration::ZERO, &reason)
            .await,
        Err(JobError::LeaseLost),
        "retry"
    );
    assert_eq!(
        queue.bury(&job.scope, job.id, &job.lease, &reason).await,
        Err(JobError::LeaseLost),
        "bury"
    );
    let again = claim(queue, &kind).await.expect("claimable as its next attempt");
    assert_eq!(again.attempt, 2);
    queue.complete(&again.scope, again.id, &again.lease).await.unwrap();
}

async fn keeps_a_lease_that_was_extended(queue: &dyn JobQueue, prepare: &Prepare) {
    let fixture = Fixture::new(prepare).await;
    let kind = fresh_kind("extend");
    queue
        .enqueue(&fixture.workspace(), &kind, vec![], JobOptions::default())
        .await
        .unwrap();
    let job = claim(queue, &kind).await.unwrap();
    queue.extend(&job.scope, job.id, &job.lease, LONG).await.unwrap();
    tokio::time::sleep(PAST_LEASE).await;
    assert!(claim(queue, &kind).await.is_none(), "the extended lease still holds it");
    queue.complete(&job.scope, job.id, &job.lease).await.unwrap();
}

async fn retries_until_the_attempts_run_out(queue: &dyn JobQueue, prepare: &Prepare) {
    let fixture = Fixture::new(prepare).await;
    let kind = fresh_kind("retry");
    let reason = name("provider_unavailable");
    let options = JobOptions {
        max_attempts: 2,
        ..JobOptions::default()
    };
    queue
        .enqueue(&fixture.workspace(), &kind, vec![], options)
        .await
        .unwrap();
    let first = claim(queue, &kind).await.unwrap();
    assert_eq!(
        queue
            .retry(&first.scope, first.id, &first.lease, Duration::from_secs(1), &reason)
            .await,
        Ok(Retry::Scheduled)
    );
    assert!(claim(queue, &kind).await.is_none(), "not before the retry's delay");
    assert_eq!(
        queue.complete(&first.scope, first.id, &first.lease).await,
        Err(JobError::LeaseLost),
        "a released lease reaches nothing"
    );
    tokio::time::sleep(PAST_LEASE).await;
    let second = claim(queue, &kind).await.expect("retried");
    assert_eq!(second.attempt, 2);
    assert_eq!(
        queue
            .retry(&second.scope, second.id, &second.lease, Duration::ZERO, &reason)
            .await,
        Ok(Retry::Exhausted)
    );
    assert!(claim(queue, &kind).await.is_none(), "a dead job isn't claimed");
}

async fn buries_on_request(queue: &dyn JobQueue, prepare: &Prepare) {
    let fixture = Fixture::new(prepare).await;
    let kind = fresh_kind("bury");
    queue
        .enqueue(&fixture.workspace(), &kind, vec![], JobOptions::default())
        .await
        .unwrap();
    let job = claim(queue, &kind).await.unwrap();
    queue
        .bury(&job.scope, job.id, &job.lease, &name("endpoint_gone"))
        .await
        .unwrap();
    assert!(claim(queue, &kind).await.is_none(), "a buried job isn't claimed");
    assert_eq!(
        queue.complete(&job.scope, job.id, &job.lease).await,
        Err(JobError::LeaseLost)
    );
}

async fn lets_a_job_die_when_its_last_lease_runs_out(queue: &dyn JobQueue, prepare: &Prepare) {
    let fixture = Fixture::new(prepare).await;
    let kind = fresh_kind("last");
    let options = JobOptions {
        max_attempts: 1,
        ..JobOptions::default()
    };
    queue
        .enqueue(&fixture.workspace(), &kind, vec![], options)
        .await
        .unwrap();
    claim(queue, &kind).await.expect("its only attempt");
    tokio::time::sleep(PAST_LEASE).await;
    assert!(
        claim(queue, &kind).await.is_none(),
        "no attempts left: never claimed again"
    );
}

async fn refuses_a_lease_named_with_another_scope(queue: &dyn JobQueue, prepare: &Prepare) {
    let fixture = Fixture::new(prepare).await;
    let scopes = [
        Scope::Organization(fixture.org),
        Scope::Environment(fixture.org, Environment::Sandbox),
        Scope::Environment(fixture.org, Environment::Production),
        Scope::Workspace(fixture.org, fixture.ws),
        Scope::Workspace(fixture.org, fixture.sibling),
        // The same IDs under another organization.
        Scope::Organization(fixture.other_org),
        Scope::Environment(fixture.other_org, Environment::Production),
        Scope::Workspace(fixture.other_org, fixture.ws),
    ];
    // A job in each kind of scope, reached from every other scope.
    for own in [
        Scope::Organization(fixture.org),
        Scope::Environment(fixture.org, Environment::Production),
        Scope::Workspace(fixture.org, fixture.ws),
    ] {
        refuses_every_other_scope(queue, own, &scopes).await;
    }
}

async fn refuses_every_other_scope(queue: &dyn JobQueue, own: Scope, scopes: &[Scope]) {
    let kind = fresh_kind("scope");
    let reason = name("stolen");
    queue.enqueue(&own, &kind, vec![], JobOptions::default()).await.unwrap();
    // A long lease, so a slow adapter's many calls below can't outlast it.
    let job = queue
        .claim_next(std::slice::from_ref(&kind), LONG)
        .await
        .unwrap()
        .unwrap();
    for scope in scopes.iter().filter(|scope| **scope != own) {
        assert_eq!(
            queue.extend(scope, job.id, &job.lease, LONG).await,
            Err(JobError::LeaseLost),
            "extend {own:?} from {scope:?}"
        );
        assert_eq!(
            queue.retry(scope, job.id, &job.lease, Duration::ZERO, &reason).await,
            Err(JobError::LeaseLost),
            "retry {own:?} from {scope:?}"
        );
        assert_eq!(
            queue.bury(scope, job.id, &job.lease, &reason).await,
            Err(JobError::LeaseLost),
            "bury {own:?} from {scope:?}"
        );
        assert_eq!(
            queue.complete(scope, job.id, &job.lease).await,
            Err(JobError::LeaseLost),
            "complete {own:?} from {scope:?}"
        );
    }
    let forged = LeaseToken::from_bytes([0; 16]);
    assert_eq!(
        queue.complete(&own, job.id, &forged).await,
        Err(JobError::LeaseLost),
        "another token"
    );
    let other_id = JobId::generate();
    assert_eq!(
        queue.complete(&own, other_id, &job.lease).await,
        Err(JobError::LeaseLost),
        "complete another job's ID"
    );
    assert_eq!(
        queue.retry(&own, other_id, &job.lease, Duration::ZERO, &reason).await,
        Err(JobError::LeaseLost),
        "retry another job's ID"
    );
    assert_eq!(
        queue.bury(&own, other_id, &job.lease, &reason).await,
        Err(JobError::LeaseLost),
        "bury another job's ID"
    );
    assert_eq!(
        queue.extend(&own, other_id, &job.lease, LONG).await,
        Err(JobError::LeaseLost),
        "extend another job's ID"
    );
    // Nothing above touched it: still held, and its own scope finishes it.
    assert!(claim(queue, &kind).await.is_none(), "still held");
    queue
        .complete(&own, job.id, &job.lease)
        .await
        .expect("the right scope and lease finish it");
}
