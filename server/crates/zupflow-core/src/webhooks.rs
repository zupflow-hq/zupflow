//! Webhooks (decision 0017; the webhooks standard): endpoints an organization
//! registers, one environment each, and the lean, signed events delivered to
//! them at least once.
//!
//! **The outbox.** A change that causes an event queues an *announcement*
//! job in its own transaction: the run store when it appends a run event a
//! webhook names, the tool registry when a listing changes a tool. The
//! announcement carries references only (the event's ID, type and instant,
//! and the run or installation), so a rolled-back change announces nothing
//! and a committed one is never lost. A worker turns each announcement into
//! one stored event and one delivery, with its own job, per enabled endpoint
//! of the event's organization and environment that subscribes to its type
//! ([`WebhookStore::record_event`], idempotent by the event's ID).
//!
//! **Delivery** is at least once, unordered, through `OutboundHttp`, signed
//! with the endpoint's secret: [`signature`] over `"<t>.<raw body>"`.
//! [`DELIVERY_ATTEMPTS`] attempts, [`RETRY_AFTER`] apart.
//!
//! Every method takes the organization first and reaches nothing outside it.

use std::time::Duration;

use async_trait::async_trait;
use hmac::{Hmac, KeyInit, Mac};
use serde::Serialize;
use serde_json::{Value, json};
use sha2::Sha256;

use crate::audit::Audit;
use crate::environment::Environment;
use crate::ids::{
    InstallationId, OrganizationId, RunId, WebhookDeliveryId, WebhookEndpointId, WebhookEventId, WorkspaceId,
};
use crate::jobs::{JobOptions, LeaseToken};
use crate::keys::Sealed;
use crate::runs::{QueuedJob, RunEventType};
use crate::scope::Name;
use crate::time::Timestamp;

/// The event types an endpoint can subscribe to, as the contract's
/// `WEBHOOK_EVENT_TYPES` lists them.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum WebhookEventType {
    InstallationConnected,
    InstallationReconnectRequired,
    InstallationDisconnected,
    ConnectSessionCompleted,
    ConnectSessionFailed,
    RunCompleted,
    RunFailed,
    RunWaitingForApproval,
    RunCancelled,
    AppToolsChanged,
}

impl WebhookEventType {
    pub const ALL: [Self; 10] = [
        Self::InstallationConnected,
        Self::InstallationReconnectRequired,
        Self::InstallationDisconnected,
        Self::ConnectSessionCompleted,
        Self::ConnectSessionFailed,
        Self::RunCompleted,
        Self::RunFailed,
        Self::RunWaitingForApproval,
        Self::RunCancelled,
        Self::AppToolsChanged,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            Self::InstallationConnected => "installation.connected",
            Self::InstallationReconnectRequired => "installation.reconnect_required",
            Self::InstallationDisconnected => "installation.disconnected",
            Self::ConnectSessionCompleted => "connect_session.completed",
            Self::ConnectSessionFailed => "connect_session.failed",
            Self::RunCompleted => "run.completed",
            Self::RunFailed => "run.failed",
            Self::RunWaitingForApproval => "run.waiting_for_approval",
            Self::RunCancelled => "run.cancelled",
            Self::AppToolsChanged => "app.tools_changed",
        }
    }

    pub fn parse(value: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|kind| kind.as_str() == value)
    }

    /// The webhook a run's timeline event is announced as, if any.
    pub fn of_run_event(kind: RunEventType) -> Option<Self> {
        match kind {
            RunEventType::RunCompleted => Some(Self::RunCompleted),
            RunEventType::RunFailed => Some(Self::RunFailed),
            RunEventType::RunCancelled => Some(Self::RunCancelled),
            RunEventType::ApprovalRequested => Some(Self::RunWaitingForApproval),
            _ => None,
        }
    }

    /// The run status a run event of this type always carries; `None` for
    /// the others.
    pub fn run_status(self) -> Option<&'static str> {
        match self {
            Self::RunCompleted => Some("completed"),
            Self::RunFailed => Some("failed"),
            Self::RunWaitingForApproval => Some("waiting_for_approval"),
            Self::RunCancelled => Some("cancelled"),
            _ => None,
        }
    }
}

impl Serialize for WebhookEventType {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(self.as_str())
    }
}

/// The test button's event type: never subscribed to, sent only by it.
pub const TEST_EVENT: &str = "webhook.test";

/// The contract's `WebhookDelivery`: an endpoint's last answer. `0` when no
/// answer came back (a timeout, a network error, a refused destination).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct LastDelivery {
    pub status_code: u16,
    pub delivered_at: Timestamp,
}

/// The contract's `WebhookEndpoint`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct WebhookEndpoint {
    pub id: WebhookEndpointId,
    pub organization_id: OrganizationId,
    pub environment: Environment,
    pub url: String,
    pub description: String,
    pub events: Vec<WebhookEventType>,
    pub enabled: bool,
    pub signing_secret_last4: String,
    pub created_at: Timestamp,
    pub last_delivery: Option<LastDelivery>,
}

/// What an endpoint is created with. The secret arrives sealed under the
/// endpoint's `webhook_signing_secret` context at [`SECRET_VERSION`].
#[derive(Debug, Clone)]
pub struct NewEndpoint {
    pub environment: Environment,
    pub url: String,
    pub description: String,
    pub events: Vec<WebhookEventType>,
    pub secret: Sealed,
    pub secret_last4: String,
}

/// The version every endpoint's secret is sealed at: there's no rotation yet.
pub const SECRET_VERSION: u32 = 1;

/// What an update changes; `None` leaves it.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct EndpointChanges {
    pub url: Option<String>,
    pub description: Option<String>,
    pub events: Option<Vec<WebhookEventType>>,
    pub enabled: Option<bool>,
}

/// An endpoint's signing secret, as stored: opened only by the code that signs.
#[derive(Debug, Clone)]
pub struct SigningSecret {
    pub environment: Environment,
    pub version: u32,
    pub sealed: Sealed,
}

/// An event as it's stored and sent. `body` is the exact JSON every attempt
/// sends: lean, never content or secrets.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NewWebhookEvent {
    pub id: WebhookEventId,
    pub environment: Environment,
    /// `None` for the test button's event, which belongs to no workspace.
    pub workspace_id: Option<WorkspaceId>,
    /// A subscribable type, or [`TEST_EVENT`].
    pub kind: String,
    pub body: String,
    pub created_at: Timestamp,
}

/// Where a delivery stands.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeliveryState {
    /// Not answered with a `2xx` yet; attempts are left.
    Pending,
    Succeeded,
    /// Every attempt failed.
    Failed,
    /// Not sent: its endpoint was disabled or stopped subscribing to the type.
    Skipped,
}

impl DeliveryState {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::Succeeded => "succeeded",
            Self::Failed => "failed",
            Self::Skipped => "skipped",
        }
    }

    pub fn parse(value: &str) -> Option<Self> {
        [Self::Pending, Self::Succeeded, Self::Failed, Self::Skipped]
            .into_iter()
            .find(|state| state.as_str() == value)
    }
}

/// A delivery with what sending it needs: its event, and its endpoint as it
/// is now.
#[derive(Debug, Clone)]
pub struct PendingDelivery {
    pub id: WebhookDeliveryId,
    pub organization_id: OrganizationId,
    pub endpoint_id: WebhookEndpointId,
    pub event_id: WebhookEventId,
    pub kind: String,
    pub body: String,
    pub state: DeliveryState,
    pub attempts: u32,
    pub url: String,
    pub enabled: bool,
    pub subscribed: bool,
}

/// What one attempt came to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Attempt {
    /// Which attempt, from 1.
    pub number: u32,
    /// The status answered; `None` when no answer came back.
    pub status: Option<u16>,
    /// Why it failed, as a short class (`timeout`, `unreachable`,
    /// `refused`, `redirected`, `status`…), never the other side's words.
    pub error: Option<String>,
    pub at: Timestamp,
    /// `Pending` with another attempt to come at `next_attempt_at`,
    /// `Succeeded`, or `Failed` when it was the last.
    pub state: DeliveryState,
    pub next_attempt_at: Option<Timestamp>,
}

impl Attempt {
    /// A `2xx`.
    pub fn succeeded(&self) -> bool {
        self.status.is_some_and(|status| (200..300).contains(&status))
    }
}

/// An endpoint with no successful delivery for this long, failing every
/// attempt since, is disabled.
pub const DISABLE_AFTER: Duration = Duration::from_secs(3 * 24 * 60 * 60);

#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum WebhookError {
    #[error("the store is unavailable")]
    Unavailable,
    /// The endpoint (or delivery) isn't in this organization.
    #[error("not found")]
    NotFound,
}

#[async_trait]
pub trait WebhookStore: Send + Sync {
    /// The organization's endpoints, oldest first; only `environment`'s when given.
    async fn list_endpoints(
        &self,
        organization_id: OrganizationId,
        environment: Option<Environment>,
    ) -> Result<Vec<WebhookEndpoint>, WebhookError>;

    async fn find_endpoint(
        &self,
        organization_id: OrganizationId,
        endpoint_id: WebhookEndpointId,
    ) -> Result<Option<WebhookEndpoint>, WebhookError>;

    async fn create_endpoint(
        &self,
        organization_id: OrganizationId,
        endpoint_id: WebhookEndpointId,
        endpoint: &NewEndpoint,
        now: Timestamp,
        audit: &Audit<'_, WebhookEndpoint>,
    ) -> Result<WebhookEndpoint, WebhookError>;

    /// Changes what `changes` names. Enabling an endpoint forgets how long
    /// it was failing. Records `audit` of it, changed. `NotFound` unless
    /// it's here.
    async fn update_endpoint(
        &self,
        organization_id: OrganizationId,
        endpoint_id: WebhookEndpointId,
        changes: &EndpointChanges,
        audit: &Audit<'_, WebhookEndpoint>,
    ) -> Result<WebhookEndpoint, WebhookError>;

    /// Deletes the endpoint with its deliveries, recording `audit` of it as
    /// it was. `NotFound` unless it's here.
    async fn delete_endpoint(
        &self,
        organization_id: OrganizationId,
        endpoint_id: WebhookEndpointId,
        audit: &Audit<'_, WebhookEndpoint>,
    ) -> Result<(), WebhookError>;

    /// The endpoint's sealed signing secret, for the code that signs.
    async fn signing_secret(
        &self,
        organization_id: OrganizationId,
        endpoint_id: WebhookEndpointId,
    ) -> Result<Option<SigningSecret>, WebhookError>;

    /// Records the event, once: in one transaction, the event and, for each
    /// enabled endpoint of its organization and environment subscribed to
    /// its type, a delivery and its [`delivery_job`]. Recording an event
    /// already recorded changes nothing. The deliveries made now.
    /// `NotFound` when the event names a workspace that isn't in this
    /// organization with the event's environment.
    async fn record_event(
        &self,
        organization_id: OrganizationId,
        event: &NewWebhookEvent,
    ) -> Result<Vec<WebhookDeliveryId>, WebhookError>;

    /// Records the test button's event, sent to one endpoint of the
    /// organization outside the queue, with its one attempt; the endpoint's
    /// last delivery becomes that answer, and a success clears how long it
    /// was failing. `NotFound` unless it's here.
    async fn record_test(
        &self,
        organization_id: OrganizationId,
        endpoint_id: WebhookEndpointId,
        event: &NewWebhookEvent,
        attempt: &Attempt,
    ) -> Result<WebhookDeliveryId, WebhookError>;

    /// A delivery with its event and endpoint. `None` when it isn't here
    /// (its endpoint was deleted, say).
    async fn find_delivery(
        &self,
        organization_id: OrganizationId,
        delivery_id: WebhookDeliveryId,
    ) -> Result<Option<PendingDelivery>, WebhookError>;

    /// Records an attempt on a pending delivery, and the answer as its
    /// endpoint's last delivery. A failure disables an endpoint that has
    /// failed every attempt for [`DISABLE_AFTER`]; a success clears that.
    /// `NotFound` unless the delivery is here and pending.
    async fn record_attempt(
        &self,
        organization_id: OrganizationId,
        delivery_id: WebhookDeliveryId,
        attempt: &Attempt,
    ) -> Result<(), WebhookError>;

    /// Settles a pending delivery as skipped: its endpoint is disabled or no
    /// longer subscribes to its type.
    async fn skip_delivery(
        &self,
        organization_id: OrganizationId,
        delivery_id: WebhookDeliveryId,
    ) -> Result<(), WebhookError>;

    /// Settles a pending delivery as failed for a reason of Zupflow's own
    /// (`reason`, a short code: its job ran out of attempts, say), leaving the
    /// endpoint's last delivery and how long it has been failing as they
    /// are: the endpoint didn't fail. `NotFound` unless it's here and pending.
    async fn abandon_delivery(
        &self,
        organization_id: OrganizationId,
        delivery_id: WebhookDeliveryId,
        reason: &str,
    ) -> Result<(), WebhookError>;

    /// Holds the endpoint for `holder`'s delivery until `until`, unless
    /// another holder's hold hasn't run out by `now`: whether `holder` holds
    /// it now. So an endpoint has one delivery in flight at a time, from every
    /// worker of every process. `NotFound` unless it's here.
    async fn hold_endpoint(
        &self,
        organization_id: OrganizationId,
        endpoint_id: WebhookEndpointId,
        holder: &LeaseToken,
        now: Timestamp,
        until: Timestamp,
    ) -> Result<bool, WebhookError>;

    /// Lets go of `holder`'s hold on the endpoint; anyone else's stays.
    async fn release_endpoint(
        &self,
        organization_id: OrganizationId,
        endpoint_id: WebhookEndpointId,
        holder: &LeaseToken,
    ) -> Result<(), WebhookError>;
}

/// The job kind of an announcement: a change that causes an event.
pub const ANNOUNCE: &str = "announce_webhook_event";

/// The job kind of one delivery to one endpoint.
pub const DELIVER: &str = "deliver_webhook";

/// How many times a delivery is tried: eight attempts over about two days.
pub const DELIVERY_ATTEMPTS: u32 = 8;

/// How many times a delivery's job can be claimed: its delivery's attempts,
/// and room for claims that aren't attempts (a store that was unavailable, a
/// worker that stopped mid-attempt). A wait for a busy endpoint is
/// postponed, not counted. A delivery whose job comes to its last claim is
/// settled there, so none is left pending.
pub const DELIVERY_JOB_ATTEMPTS: u32 = DELIVERY_ATTEMPTS + 12;

/// How many times an announcement's job can be claimed, backing off up to
/// an hour apart: a committed event survives most of a day of a store being
/// unavailable before it's given up on, loudly.
pub const ANNOUNCE_ATTEMPTS: u32 = 30;

/// How long after each failed attempt the next one comes, before jitter:
/// 1 minute, 5 minutes, 30 minutes, 2 hours, 6 hours, 12 hours, 24 hours.
pub const RETRY_AFTER: [Duration; 7] = [
    Duration::from_secs(60),
    Duration::from_secs(5 * 60),
    Duration::from_secs(30 * 60),
    Duration::from_secs(2 * 60 * 60),
    Duration::from_secs(6 * 60 * 60),
    Duration::from_secs(12 * 60 * 60),
    Duration::from_secs(24 * 60 * 60),
];

/// What a change announced.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Subject {
    Run(RunId),
    Installation(InstallationId),
}

/// An announcement, as its job carries it: references only.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Announcement {
    /// The event's ID, fixed when the change is made, so announcing twice
    /// records one event.
    pub event_id: WebhookEventId,
    pub kind: WebhookEventType,
    pub subject: Subject,
    pub created_at: Timestamp,
}

impl Announcement {
    /// The job that announces it, queued in the workspace's scope in the
    /// change's own transaction.
    pub fn job(&self) -> Option<QueuedJob> {
        let subject = match self.subject {
            Subject::Run(run_id) => json!({ "run_id": run_id }),
            Subject::Installation(installation_id) => json!({ "installation_id": installation_id }),
        };
        Some(QueuedJob {
            kind: Name::new(ANNOUNCE).ok()?,
            payload: json!({
                "event_id": self.event_id,
                "type": self.kind.as_str(),
                "created_at": self.created_at.millis(),
                "subject": subject,
            })
            .to_string()
            .into_bytes(),
            options: JobOptions {
                delay: Duration::ZERO,
                max_attempts: ANNOUNCE_ATTEMPTS,
            },
        })
    }

    /// An announcement job's payload, read back.
    pub fn parse(payload: &[u8]) -> Option<Self> {
        let payload: Value = serde_json::from_slice(payload).ok()?;
        let subject = &payload["subject"];
        let subject = if let Some(run) = subject["run_id"].as_str() {
            Subject::Run(RunId::parse(run).ok()?)
        } else {
            Subject::Installation(InstallationId::parse(subject["installation_id"].as_str()?).ok()?)
        };
        Some(Self {
            event_id: WebhookEventId::parse(payload["event_id"].as_str()?).ok()?,
            kind: WebhookEventType::parse(payload["type"].as_str()?)?,
            subject,
            created_at: Timestamp::from_millis(payload["created_at"].as_i64()?),
        })
    }

    /// The announcement of a run's timeline event, when a webhook names its type.
    pub fn of_run_event(run_id: RunId, kind: RunEventType, at: Timestamp) -> Option<Self> {
        Some(Self {
            event_id: WebhookEventId::generate(),
            kind: WebhookEventType::of_run_event(kind)?,
            subject: Subject::Run(run_id),
            created_at: at,
        })
    }

    pub fn reconnect_required(installation_id: InstallationId, at: Timestamp) -> Self {
        Self {
            event_id: WebhookEventId::generate(),
            kind: WebhookEventType::InstallationReconnectRequired,
            subject: Subject::Installation(installation_id),
            created_at: at,
        }
    }

    /// The announcement that a listing changed one or more of an installation's tools.
    pub fn tools_changed(installation_id: InstallationId, at: Timestamp) -> Self {
        Self {
            event_id: WebhookEventId::generate(),
            kind: WebhookEventType::AppToolsChanged,
            subject: Subject::Installation(installation_id),
            created_at: at,
        }
    }
}

/// The job that sends one delivery, queued in its organization's scope.
pub fn delivery_job(delivery_id: WebhookDeliveryId) -> Option<QueuedJob> {
    Some(QueuedJob {
        kind: Name::new(DELIVER).ok()?,
        payload: json!({ "delivery_id": delivery_id }).to_string().into_bytes(),
        options: JobOptions {
            delay: Duration::ZERO,
            max_attempts: DELIVERY_JOB_ATTEMPTS,
        },
    })
}

/// A delivery job's delivery, read back.
pub fn delivery_of(payload: &[u8]) -> Option<WebhookDeliveryId> {
    let payload: Value = serde_json::from_slice(payload).ok()?;
    WebhookDeliveryId::parse(payload["delivery_id"].as_str()?).ok()
}

/// The `Zupflow-Signature` header's value: `t=<unix seconds>,v1=<lowercase
/// hex HMAC-SHA256 of "<t>.<body>" under the secret>`.
pub fn signature(secret: &[u8], unix_seconds: i64, body: &[u8]) -> String {
    let mut mac = Hmac::<Sha256>::new_from_slice(secret).expect("HMAC takes a key of any length");
    mac.update(unix_seconds.to_string().as_bytes());
    mac.update(b".");
    mac.update(body);
    let digest = mac.finalize().into_bytes();
    let hex: String = digest.iter().map(|byte| format!("{byte:02x}")).collect();
    format!("t={unix_seconds},v1={hex}")
}

/// A new signing secret: `whsec_{live|test}_` and 32 random base62
/// characters, shown once.
pub fn new_signing_secret(environment: Environment) -> Option<zeroize::Zeroizing<String>> {
    let label = match environment {
        Environment::Production => "live",
        Environment::Sandbox => "test",
    };
    let random = crate::api_keys::random_base62(32).ok()?;
    Some(zeroize::Zeroizing::new(format!("whsec_{label}_{random}")))
}

/// The delivery body every attempt sends: the contract's `WebhookEvent`.
pub fn event_body(
    event_id: WebhookEventId,
    kind: &str,
    created_at: Timestamp,
    environment: Environment,
    organization_id: OrganizationId,
    workspace: Option<(WorkspaceId, Option<&str>)>,
    data: Value,
) -> String {
    json!({
        "id": event_id,
        "type": kind,
        "created_at": created_at,
        "environment": environment,
        "organization_id": organization_id,
        "workspace": workspace.map(|(id, external_id)| json!({ "id": id, "external_id": external_id })),
        "data": data,
    })
    .to_string()
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;

    #[test]
    fn names_every_type_as_the_contract_does() {
        for kind in WebhookEventType::ALL {
            assert_eq!(WebhookEventType::parse(kind.as_str()), Some(kind));
        }
        assert_eq!(WebhookEventType::parse(TEST_EVENT), None);
    }

    #[test]
    fn signs_as_the_sdk_verifies() {
        // HMAC-SHA256 under the whole secret string of "1700000000.{}", as `signWebhook` computes it.
        let header = signature(b"whsec_test_abc", 1_700_000_000, b"{}");
        assert_eq!(
            header,
            "t=1700000000,v1=609b4d347b5443560436f9004f8a47f19599bcad1ad08b23204bb26973d9fb42"
        );
        assert_ne!(header, signature(b"whsec_test_abd", 1_700_000_000, b"{}"));
        assert_ne!(header, signature(b"whsec_test_abc", 1_700_000_001, b"{}"));
    }

    #[test]
    fn reads_an_announcement_back() {
        let announcement = Announcement::of_run_event(
            RunId::generate(),
            RunEventType::ApprovalRequested,
            Timestamp::from_millis(5),
        )
        .unwrap();
        assert_eq!(announcement.kind, WebhookEventType::RunWaitingForApproval);
        let job = announcement.job().unwrap();
        assert_eq!(Announcement::parse(&job.payload), Some(announcement));
        assert!(Announcement::of_run_event(RunId::generate(), RunEventType::RunStarted, Timestamp::now()).is_none());
        let tools = Announcement::tools_changed(InstallationId::generate(), Timestamp::from_millis(9));
        assert_eq!(Announcement::parse(&tools.job().unwrap().payload), Some(tools));
    }

    #[test]
    fn makes_secrets_in_the_documented_shape() {
        let secret = new_signing_secret(Environment::Sandbox).unwrap();
        assert!(secret.starts_with("whsec_test_"));
        assert_eq!(secret.len(), "whsec_test_".len() + 32);
        assert!(
            new_signing_secret(Environment::Production)
                .unwrap()
                .starts_with("whsec_live_")
        );
    }
}
