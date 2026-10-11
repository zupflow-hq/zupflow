//! Webhook delivery (decision 0017): workers in the same process take two
//! kinds of job from the queue, each on loops of its own, so a slow endpoint
//! never holds up an announcement.
//!
//! - **Announcements**, queued by the change that causes an event in its own
//!   transaction (the outbox), on one loop: the worker reads what the event
//!   names in the job's own scope, builds the lean event body once and
//!   records it with a delivery for each subscribed endpoint of the event's
//!   organization and environment ([`WebhookStore::record_event`],
//!   idempotent by the event's ID). A store that's unavailable is waited out,
//!   backing off up to an hour, for [`ANNOUNCE_ATTEMPTS`] claims; an event
//!   given up on after that is logged as lost.
//! - **Deliveries**, one per endpoint, on [`DELIVERY_WORKERS`] loops: the
//!   event's body, signed with the endpoint's secret (opened here, at the
//!   moment of sending, and nowhere else), sent through `OutboundHttp`. Any
//!   `2xx` within 10 seconds is success; anything else is retried
//!   [`RETRY_AFTER`] later, with jitter, up to [`DELIVERY_ATTEMPTS`]
//!   attempts. The answer's body is never read or stored. An endpoint has
//!   one delivery in flight at a time, across every worker: a delivery that
//!   finds its endpoint busy waits its turn without using an attempt.
//!
//! Only the endpoint's own failures are attempts. When Zupflow can't send (a
//! store or the key service is unavailable), nothing is recorded against the
//! endpoint and the job tries again, backing off; a delivery whose job comes
//! to its last claim that way ([`DELIVERY_JOB_ATTEMPTS`]) is settled as
//! failed, without touching the endpoint. Only a worker stopping in the
//! middle of that last claim leaves a delivery pending.
//!
//! Delivery is at least once and unordered: a delivery cut off by a restart
//! is sent again, with the same event ID.
//!
//! [`WebhookStore::record_event`]: zupflow_core::webhooks::WebhookStore::record_event

use std::time::Duration;

use serde_json::json;
use sha2::{Digest, Sha256};
use zupflow_core::ids::{OrganizationId, WebhookDeliveryId, WebhookEndpointId, WebhookEventId, WorkspaceId};
use zupflow_core::installations::InstallationStatus;
use zupflow_core::jobs::{Job, JobError, Retry};
use zupflow_core::keys::{EncryptionContext, open};
use zupflow_core::outbound::{Limits, Method, OutboundError, OutboundRequest};
use zupflow_core::scope::{Name, Scope};
use zupflow_core::time::Timestamp;
use zupflow_core::webhooks::{
    ANNOUNCE, ANNOUNCE_ATTEMPTS, Announcement, Attempt, DELIVER, DELIVERY_ATTEMPTS, DELIVERY_JOB_ATTEMPTS,
    DeliveryState, NewWebhookEvent, RETRY_AFTER, Subject, WebhookError, WebhookEventType, delivery_of, event_body,
    signature,
};

use crate::auth::AppState;

/// How long a claim holds a job: longer than one delivery's timeout.
const LEASE: Duration = Duration::from_secs(60);

const IDLE: Duration = Duration::from_secs(1);

/// How long a delivery may take: any `2xx` within it is success.
const TIMEOUT: Duration = Duration::from_secs(10);

/// How long a delivery holds its endpoint: its timeout, with room. A worker
/// that stops mid-delivery lets go when this runs out.
const HOLD: Duration = Duration::from_secs(30);

/// How many deliveries are in flight at once, to different endpoints.
pub const DELIVERY_WORKERS: usize = 8;

/// About how long a delivery waits for a busy endpoint before trying again.
const BUSY_WAIT: Duration = Duration::from_secs(5);

/// How long a job waits after Zupflow's own failure: doubling from the
/// first, up to the most.
const BACKOFF_FIRST: Duration = Duration::from_secs(5);
const BACKOFF_MOST: Duration = Duration::from_secs(60 * 60);

pub const USER_AGENT: &str = "Zupflow-Webhooks/1";

/// What one send came to, as the endpoint answered it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Sent {
    /// The status answered; `None` when nothing came back.
    pub status: Option<u16>,
    /// Why it failed, as a short class; `None` on a `2xx`.
    pub error: Option<&'static str>,
}

impl Sent {
    pub fn succeeded(&self) -> bool {
        self.error.is_none()
    }
}

fn failed(error: &'static str) -> Sent {
    Sent {
        status: None,
        error: Some(error),
    }
}

/// Why nothing was sent: Zupflow's own failure, never the endpoint's, so
/// never recorded as an attempt.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Unsent {
    /// The endpoint was deleted.
    EndpointGone,
    StoreUnavailable,
    /// The key service couldn't open the signing secret.
    SecretUnavailable,
}

impl Unsent {
    fn code(self) -> &'static str {
        match self {
            Self::EndpointGone => "endpoint_gone",
            Self::StoreUnavailable => "store_unavailable",
            Self::SecretUnavailable => "secret_unavailable",
        }
    }
}

/// Signs and sends one event to one endpoint, as attempt `attempt`. The
/// signing secret is opened here, under the endpoint's context, and dropped
/// (wiped) when the request is built.
#[allow(clippy::too_many_arguments)]
pub async fn send(
    state: &AppState,
    organization_id: OrganizationId,
    endpoint_id: WebhookEndpointId,
    url: &str,
    event_id: WebhookEventId,
    kind: &str,
    body: &str,
    attempt: u32,
) -> Result<Sent, Unsent> {
    let secret = match state.webhooks.signing_secret(organization_id, endpoint_id).await {
        Ok(Some(secret)) => secret,
        Ok(None) => return Err(Unsent::EndpointGone),
        Err(_) => return Err(Unsent::StoreUnavailable),
    };
    let context =
        EncryptionContext::webhook_signing_secret(organization_id, secret.environment, endpoint_id, secret.version);
    let header = match open(state.keys.as_ref(), &context, &secret.sealed).await {
        Ok(opened) => signature(
            opened.expose(),
            Timestamp::now().millis().div_euclid(1000),
            body.as_bytes(),
        ),
        Err(error) => {
            tracing::error!(event = "webhooks.secret_unavailable", %organization_id, %endpoint_id, %error);
            return Err(Unsent::SecretUnavailable);
        }
    };
    let request = OutboundRequest {
        method: Method::Post,
        url: url.to_owned(),
        headers: vec![
            ("Content-Type".to_owned(), "application/json".to_owned()),
            ("User-Agent".to_owned(), USER_AGENT.to_owned()),
            ("Zupflow-Signature".to_owned(), header),
            ("Zupflow-Event-Id".to_owned(), event_id.to_string()),
            ("Zupflow-Event-Type".to_owned(), kind.to_owned()),
            ("Zupflow-Delivery-Attempt".to_owned(), attempt.to_string()),
        ],
        body: Some(body.as_bytes().to_vec()),
        credential: None,
    };
    let limits = Limits {
        timeout: TIMEOUT,
        ..Limits::default()
    };
    // Classified by status and error kind only: the endpoint's own words are never read.
    Ok(match state.outbound.send(request, limits).await {
        Ok(response) if (200..300).contains(&response.status) => Sent {
            status: Some(response.status),
            error: None,
        },
        Ok(response) => Sent {
            status: Some(response.status),
            error: Some("status"),
        },
        Err(OutboundError::Refused(_)) => failed("refused"),
        Err(OutboundError::Redirected) => failed("redirected"),
        Err(OutboundError::TimedOut) => failed("timeout"),
        Err(OutboundError::TooLarge) => failed("too_large"),
        Err(OutboundError::Unreachable) => failed("unreachable"),
        Err(OutboundError::Failed) => failed("failed"),
    })
}

/// How long after failed attempt `attempt` (from 1) the next one comes:
/// [`RETRY_AFTER`], give or take a tenth, spread by the delivery so a burst of
/// failures doesn't come back all at once.
pub fn retry_after(delivery: &str, attempt: u32) -> Duration {
    let index = usize::try_from(attempt.saturating_sub(1)).unwrap_or(usize::MAX);
    spread(
        RETRY_AFTER[index.min(RETRY_AFTER.len() - 1)],
        &format!("{delivery}:{attempt}"),
    )
}

/// How long a job waits after Zupflow's own failure number `failure` (from
/// 1): [`BACKOFF_FIRST`], doubling each time, up to [`BACKOFF_MOST`], give
/// or take a tenth.
pub fn backoff(seed: &str, failure: u32) -> Duration {
    let doublings = failure.saturating_sub(1).min(16);
    let base = BACKOFF_FIRST.saturating_mul(1 << doublings).min(BACKOFF_MOST);
    spread(base, &format!("{seed}:backoff:{failure}"))
}

/// `base`, give or take a tenth, by `seed`.
fn spread(base: Duration, seed: &str) -> Duration {
    let digest = Sha256::digest(seed.as_bytes());
    // 0..=2000 per mille of a tenth: -10% to +10%.
    let spread = u64::from(u16::from_be_bytes([digest[0], digest[1]]) % 2001);
    let tenth = base.as_millis() / 10;
    let millis = base.as_millis() - tenth + tenth * u128::from(spread) / 1000;
    Duration::from_millis(u64::try_from(millis).unwrap_or(u64::MAX))
}

/// What a delivery waits for a busy endpoint: about [`BUSY_WAIT`], half to
/// one and a half of it, so the deliveries waiting don't all come back at once.
fn busy_wait(delivery: &str, now: Timestamp) -> Duration {
    let digest = Sha256::digest(format!("{delivery}:busy:{}", now.millis()).as_bytes());
    let per_mille = u64::from(u16::from_be_bytes([digest[0], digest[1]]) % 1001);
    let base = u64::try_from(BUSY_WAIT.as_millis()).unwrap_or(u64::MAX);
    Duration::from_millis(base / 2 + base * per_mille / 1000)
}

/// What to do with a job once handled.
enum Finish {
    Complete,
    Retry(Duration, &'static str),
    Bury(&'static str),
    /// Back in the queue without using an attempt: its endpoint was busy.
    Postpone(Duration),
}

/// Claims and handles one announcement, if one is due. Whether one was.
pub async fn announce_once(state: &AppState) -> bool {
    work_one(state, ANNOUNCE).await
}

/// Claims and sends one delivery, if one is due. Whether one was.
pub async fn deliver_once(state: &AppState) -> bool {
    work_one(state, DELIVER).await
}

/// One announcement and one delivery, if due. Whether either was.
pub async fn work_once(state: &AppState) -> bool {
    let announced = announce_once(state).await;
    let delivered = deliver_once(state).await;
    announced || delivered
}

async fn work_one(state: &AppState, kind: &'static str) -> bool {
    let Ok(kind_name) = Name::new(kind) else { return false };
    // Cross-tenant by design: the worker takes the next job of any
    // organization, and everything after is scoped to the job's own.
    let job = match state.jobs.claim_next(std::slice::from_ref(&kind_name), LEASE).await {
        Ok(Some(job)) => job,
        Ok(None) => return false,
        Err(error) => {
            tracing::warn!(event = "webhooks.claim_failed", kind, %error);
            return false;
        }
    };
    let finish = if kind == ANNOUNCE {
        announce(state, &job).await
    } else {
        deliver(state, &job).await
    };
    let Job { id, scope, lease, .. } = &job;
    let finished: Result<(), JobError> = match finish {
        Finish::Complete => state.jobs.complete(scope, *id, lease).await,
        Finish::Postpone(after) => state.jobs.postpone(scope, *id, lease, after).await,
        Finish::Retry(after, reason) => match Name::new(reason) {
            Ok(name) => match state.jobs.retry(scope, *id, lease, after, &name).await {
                Ok(Retry::Scheduled) => Ok(()),
                Ok(Retry::Exhausted) => {
                    // The handlers settle a job's last claim themselves; this is a backstop.
                    tracing::error!(
                        event = "webhooks.job_given_up",
                        kind,
                        ?scope,
                        reason,
                        attempt = job.attempt
                    );
                    Ok(())
                }
                Err(error) => Err(error),
            },
            Err(_) => state.jobs.complete(scope, *id, lease).await,
        },
        Finish::Bury(reason) => match Name::new(reason) {
            Ok(name) => {
                tracing::error!(
                    event = "webhooks.job_given_up",
                    kind,
                    ?scope,
                    reason,
                    attempt = job.attempt
                );
                state.jobs.bury(scope, *id, lease, &name).await
            }
            Err(_) => state.jobs.complete(scope, *id, lease).await,
        },
    };
    if let Err(error) = finished {
        tracing::warn!(event = "webhooks.finish_failed", kind, %error);
    }
    true
}

/// Why an announcement couldn't be read.
enum Unread {
    Unavailable,
    Malformed,
}

/// Records an announced event and its deliveries, in the job's workspace.
async fn announce(state: &AppState, job: &Job) -> Finish {
    let (Scope::Workspace(organization_id, workspace_id), Some(announcement)) =
        (job.scope, Announcement::parse(&job.payload))
    else {
        tracing::error!(event = "webhooks.malformed_job", kind = ANNOUNCE);
        return Finish::Bury("malformed");
    };
    let unavailable = || {
        if job.attempt >= ANNOUNCE_ATTEMPTS {
            // Every claim spent: the event is lost, and says so.
            tracing::error!(
                event = "webhooks.event_lost",
                %organization_id,
                %workspace_id,
                event_id = %announcement.event_id,
                kind = announcement.kind.as_str(),
            );
            Finish::Bury("store_unavailable")
        } else {
            Finish::Retry(
                backoff(&announcement.event_id.to_string(), job.attempt),
                "store_unavailable",
            )
        }
    };
    match event_of(state, organization_id, workspace_id, &announcement).await {
        Ok(Some(event)) => match state.webhooks.record_event(organization_id, &event).await {
            Ok(deliveries) => {
                tracing::info!(
                    event = "webhooks.recorded",
                    %organization_id,
                    %workspace_id,
                    event_id = %event.id,
                    kind = event.kind.as_str(),
                    deliveries = deliveries.len(),
                );
                Finish::Complete
            }
            // Its workspace went between reading it and recording.
            Err(WebhookError::NotFound) => Finish::Complete,
            Err(WebhookError::Unavailable) => unavailable(),
        },
        // What it names is gone (its workspace, with everything in it), or
        // no longer what the event says.
        Ok(None) => Finish::Complete,
        Err(Unread::Malformed) => {
            tracing::error!(event = "webhooks.malformed_job", kind = ANNOUNCE);
            Finish::Bury("malformed")
        }
        Err(Unread::Unavailable) => unavailable(),
    }
}

/// The event an announcement names, read in its own scope: the workspace
/// (its environment and external ID) and the object, as they are now, with
/// the status the event's type means. `None` when what it names is gone.
async fn event_of(
    state: &AppState,
    organization_id: OrganizationId,
    workspace_id: WorkspaceId,
    announcement: &Announcement,
) -> Result<Option<NewWebhookEvent>, Unread> {
    let workspace = match state.workspaces.find_workspace(organization_id, workspace_id).await {
        Ok(Some(workspace)) => workspace,
        Ok(None) => return Ok(None),
        Err(_) => return Err(Unread::Unavailable),
    };
    let data = match announcement.subject {
        Subject::Run(run_id) => {
            let Some(status) = announcement.kind.run_status() else {
                return Err(Unread::Malformed);
            };
            match state.runs.find_run(organization_id, workspace_id, run_id).await {
                Ok(Some(run)) => json!({ "object": "run", "id": run.id, "agent_id": run.agent_id, "status": status }),
                Ok(None) => return Ok(None),
                Err(_) => return Err(Unread::Unavailable),
            }
        }
        Subject::Installation(installation_id) => {
            let status = match announcement.kind {
                WebhookEventType::AppToolsChanged => "connected",
                WebhookEventType::InstallationReconnectRequired => "reconnect_required",
                _ => return Err(Unread::Malformed),
            };
            match state
                .installations
                .find_installation(organization_id, workspace_id, installation_id)
                .await
            {
                // Discovery lists connected installations only, but it may
                // have been disconnected since: then there's nothing to say.
                Ok(Some(installation))
                    if announcement.kind == WebhookEventType::AppToolsChanged
                        && installation.status != InstallationStatus::Connected =>
                {
                    return Ok(None);
                }
                Ok(Some(installation)) => json!({
                    "object": "installation",
                    "id": installation.id,
                    "app_id": installation.app.id(),
                    "status": status,
                }),
                Ok(None) => return Ok(None),
                Err(_) => return Err(Unread::Unavailable),
            }
        }
    };
    let kind = announcement.kind.as_str();
    Ok(Some(NewWebhookEvent {
        id: announcement.event_id,
        environment: workspace.environment,
        workspace_id: Some(workspace_id),
        kind: kind.to_owned(),
        body: event_body(
            announcement.event_id,
            kind,
            announcement.created_at,
            workspace.environment,
            organization_id,
            Some((workspace_id, workspace.external_id.as_deref())),
            data,
        ),
        created_at: announcement.created_at,
    }))
}

/// Sends one delivery, in the job's organization, and records the attempt.
async fn deliver(state: &AppState, job: &Job) -> Finish {
    let (Scope::Organization(organization_id), Some(delivery_id)) = (job.scope, delivery_of(&job.payload)) else {
        tracing::error!(event = "webhooks.malformed_job", kind = DELIVER);
        return Finish::Bury("malformed");
    };
    let delivery = match state.webhooks.find_delivery(organization_id, delivery_id).await {
        Ok(Some(delivery)) => delivery,
        // Its endpoint was deleted, with its deliveries.
        Ok(None) => return Finish::Complete,
        Err(_) => return ours(state, job, organization_id, delivery_id, 0, Unsent::StoreUnavailable).await,
    };
    if delivery.state != DeliveryState::Pending {
        return Finish::Complete;
    }
    let ours = |unsent| ours(state, job, organization_id, delivery_id, delivery.attempts, unsent);
    if !delivery.enabled || !delivery.subscribed {
        return match state.webhooks.skip_delivery(organization_id, delivery_id).await {
            Ok(()) | Err(WebhookError::NotFound) => Finish::Complete,
            Err(WebhookError::Unavailable) => ours(Unsent::StoreUnavailable).await,
        };
    }
    // One delivery in flight per endpoint: the claim's own lease token is the holder.
    let endpoint_id = delivery.endpoint_id;
    let holder = &job.lease;
    let now = Timestamp::now();
    let until = Timestamp::from_millis(
        now.millis()
            .saturating_add(i64::try_from(HOLD.as_millis()).unwrap_or(i64::MAX)),
    );
    match state
        .webhooks
        .hold_endpoint(organization_id, endpoint_id, holder, now, until)
        .await
    {
        Ok(true) => {}
        Ok(false) => return Finish::Postpone(busy_wait(&delivery_id.to_string(), now)),
        Err(WebhookError::NotFound) => return Finish::Complete,
        Err(WebhookError::Unavailable) => return ours(Unsent::StoreUnavailable).await,
    }
    let finish = attempt(state, job, organization_id, &delivery).await;
    if let Err(error) = state
        .webhooks
        .release_endpoint(organization_id, endpoint_id, holder)
        .await
    {
        // The hold runs out on its own.
        tracing::warn!(event = "webhooks.release_failed", %organization_id, %endpoint_id, %error);
    }
    match finish {
        Ok(finish) => finish,
        Err(unsent) => ours(unsent).await,
    }
}

/// Sends a pending delivery, holding its endpoint, as its next attempt, and
/// records the answer; what Zupflow itself couldn't do is `Err`, with
/// nothing recorded.
async fn attempt(
    state: &AppState,
    job: &Job,
    organization_id: OrganizationId,
    delivery: &zupflow_core::webhooks::PendingDelivery,
) -> Result<Finish, Unsent> {
    let delivery_id = delivery.id;
    // The delivery counts its attempts; the job's claims include ones that weren't.
    let number = delivery.attempts.saturating_add(1);
    let sent = match send(
        state,
        organization_id,
        delivery.endpoint_id,
        &delivery.url,
        delivery.event_id,
        &delivery.kind,
        &delivery.body,
        number,
    )
    .await
    {
        Ok(sent) => sent,
        // Deleted since it was read, with its deliveries.
        Err(Unsent::EndpointGone) => return Ok(Finish::Complete),
        Err(unsent) => return Err(unsent),
    };
    let at = Timestamp::now();
    // The last attempt, or the job's last claim: either way, settled now.
    let last = number >= DELIVERY_ATTEMPTS || job.attempt >= DELIVERY_JOB_ATTEMPTS;
    let after = retry_after(&delivery_id.to_string(), number);
    let state_after = if sent.succeeded() {
        DeliveryState::Succeeded
    } else if last {
        DeliveryState::Failed
    } else {
        DeliveryState::Pending
    };
    let next_attempt_at = (state_after == DeliveryState::Pending).then(|| {
        Timestamp::from_millis(
            at.millis()
                .saturating_add(i64::try_from(after.as_millis()).unwrap_or(0)),
        )
    });
    let attempt = Attempt {
        number,
        status: sent.status,
        error: sent.error.map(str::to_owned),
        at,
        state: state_after,
        next_attempt_at,
    };
    tracing::info!(
        event = "webhooks.attempted",
        %organization_id,
        %delivery_id,
        endpoint_id = %delivery.endpoint_id,
        attempt = number,
        status = sent.status,
        error = sent.error,
    );
    match state
        .webhooks
        .record_attempt(organization_id, delivery_id, &attempt)
        .await
    {
        Ok(()) | Err(WebhookError::NotFound) => {}
        // Sent, but not recorded: tried again, so it may arrive twice, never not at all.
        Err(WebhookError::Unavailable) => return Err(Unsent::StoreUnavailable),
    }
    Ok(match state_after {
        DeliveryState::Pending => Finish::Retry(after, "delivery_failed"),
        _ => Finish::Complete,
    })
}

/// Zupflow's own failure on a delivery, never the endpoint's: tried again,
/// backing off, without an attempt recorded. On the job's last claim the
/// delivery is settled as failed instead, leaving the endpoint as it is.
async fn ours(
    state: &AppState,
    job: &Job,
    organization_id: OrganizationId,
    delivery_id: WebhookDeliveryId,
    attempts: u32,
    unsent: Unsent,
) -> Finish {
    let reason = unsent.code();
    if job.attempt < DELIVERY_JOB_ATTEMPTS {
        // The claims so far that weren't attempts, about.
        let failures = job.attempt.saturating_sub(attempts).max(1);
        return Finish::Retry(backoff(&delivery_id.to_string(), failures), reason);
    }
    let settled = state
        .webhooks
        .abandon_delivery(organization_id, delivery_id, reason)
        .await;
    tracing::error!(
        event = "webhooks.delivery_abandoned",
        %organization_id,
        %delivery_id,
        reason,
        settled = !matches!(settled, Err(WebhookError::Unavailable)),
    );
    match settled {
        Ok(()) | Err(WebhookError::NotFound) => Finish::Complete,
        Err(WebhookError::Unavailable) => Finish::Bury(reason),
    }
}

/// Works announcements on one loop and deliveries on [`DELIVERY_WORKERS`]
/// more, until the process stops; aborting the handle stops them all.
pub async fn run_workers(state: AppState) {
    let mut workers = tokio::task::JoinSet::new();
    let announcer = state.clone();
    workers.spawn(async move {
        loop {
            if !announce_once(&announcer).await {
                tokio::time::sleep(IDLE).await;
            }
        }
    });
    for _ in 0..DELIVERY_WORKERS {
        let deliverer = state.clone();
        workers.spawn(async move {
            loop {
                if !deliver_once(&deliverer).await {
                    tokio::time::sleep(IDLE).await;
                }
            }
        });
    }
    // The loops never end; a panic in one is logged and the rest go on.
    while let Some(ended) = workers.join_next().await {
        let Err(error) = ended;
        tracing::error!(event = "webhooks.worker_stopped", %error);
    }
}
