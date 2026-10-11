//! The `WebhookStore` suite: endpoints reached only through their own
//! organization, listed oldest first and by environment; their sealed secret
//! read back exactly; an event recorded once, with a delivery and its job
//! only for the enabled endpoints of its organization and environment that
//! subscribe to its type; attempts recorded only on a pending delivery of the
//! organization, each answer becoming the endpoint's last delivery, and an
//! endpoint failing every attempt for three days disabled; the test button's
//! event recorded with its one attempt, its success clearing that count;
//! deliveries going with their endpoint; a delivery abandoned for Zupflow's
//! own failure without charging the endpoint; and an endpoint held for one
//! delivery at a time.
//!
//! `prepare` makes a [`Tenant`] exist: its organization, a Sandbox workspace
//! and a Production workspace. The suite claims delivery jobs through `jobs`.

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;

use zupflow_core::environment::Environment;
use zupflow_core::ids::{
    AgentId, OrganizationId, RunId, WebhookDeliveryId, WebhookEndpointId, WebhookEventId, WorkspaceId,
};
use zupflow_core::jobs::{JobQueue, LeaseToken};
use zupflow_core::keys::{Sealed, WrappedKey};
use zupflow_core::scope::{Name, Scope};
use zupflow_core::time::Timestamp;
use zupflow_core::webhooks::{
    Attempt, DELIVER, DISABLE_AFTER, DeliveryFilter, DeliveryState, EndpointChanges, NewEndpoint, NewWebhookEvent,
    TEST_EVENT, WebhookError, WebhookEventType, WebhookStore, delivery_of, event_body,
};

use crate::audit_store::by_suite;

#[derive(Debug, Clone, Copy)]
pub struct Tenant {
    pub organization_id: OrganizationId,
    pub sandbox: WorkspaceId,
    pub sibling: WorkspaceId,
    pub production: WorkspaceId,
}

pub type Prepare = Arc<dyn Fn(Tenant) -> Pin<Box<dyn Future<Output = ()> + Send>> + Send + Sync>;

pub struct Stores<'a> {
    pub webhooks: &'a dyn WebhookStore,
    pub jobs: &'a dyn JobQueue,
}

async fn tenant(prepare: &Prepare) -> Tenant {
    let tenant = Tenant {
        organization_id: OrganizationId::generate(),
        sandbox: WorkspaceId::generate(),
        sibling: WorkspaceId::generate(),
        production: WorkspaceId::generate(),
    };
    prepare(tenant).await;
    tenant
}

fn sealed(marker: u8) -> Sealed {
    Sealed {
        wrapped_key: WrappedKey {
            key_id: format!("key-{marker}"),
            bytes: vec![marker; 40],
        },
        nonce: [marker; 12],
        ciphertext: vec![marker; 48],
    }
}

async fn endpoint(
    stores: &Stores<'_>,
    organization_id: OrganizationId,
    environment: Environment,
    events: &[WebhookEventType],
    marker: u8,
    at: Timestamp,
) -> WebhookEndpointId {
    let id = WebhookEndpointId::generate();
    let created = stores
        .webhooks
        .create_endpoint(
            organization_id,
            id,
            &NewEndpoint {
                environment,
                url: format!("https://hooks.example/{marker}"),
                description: format!("Endpoint {marker}"),
                events: events.to_vec(),
                secret: sealed(marker),
                secret_last4: format!("{marker:04}"),
            },
            at,
            &by_suite,
        )
        .await
        .unwrap();
    assert_eq!(
        (
            created.id,
            created.organization_id,
            created.environment,
            created.enabled
        ),
        (id, organization_id, environment, true),
        "created as asked, enabled"
    );
    assert_eq!(created.events, events, "with its events in order");
    assert_eq!(created.signing_secret_last4, format!("{marker:04}"));
    assert_eq!(created.last_delivery, None, "never delivered to");
    id
}

fn event(environment: Environment, workspace: Option<WorkspaceId>, kind: &str) -> NewWebhookEvent {
    let id = WebhookEventId::generate();
    NewWebhookEvent {
        id,
        environment,
        workspace_id: workspace,
        kind: kind.to_owned(),
        body: format!("{{\"id\":\"{id}\",\"type\":\"{kind}\"}}"),
        created_at: Timestamp::now(),
    }
}

fn failure(number: u32, at: Timestamp, last: bool) -> Attempt {
    Attempt {
        number,
        status: Some(503),
        error: Some("status".to_owned()),
        at,
        duration_ms: 12,
        state: if last {
            DeliveryState::Failed
        } else {
            DeliveryState::Pending
        },
        next_attempt_at: (!last).then(|| Timestamp::from_millis(at.millis() + 60_000)),
    }
}

fn success(number: u32, at: Timestamp) -> Attempt {
    Attempt {
        number,
        status: Some(204),
        error: None,
        at,
        duration_ms: 7,
        state: DeliveryState::Succeeded,
        next_attempt_at: None,
    }
}

/// Claims every queued delivery job, answering each delivery with its job's scope.
async fn claim_deliveries(stores: &Stores<'_>) -> Vec<(Scope, WebhookDeliveryId)> {
    let kind = Name::new(DELIVER).unwrap();
    let mut claimed = Vec::new();
    while let Some(job) = stores
        .jobs
        .claim_next(std::slice::from_ref(&kind), Duration::from_secs(60))
        .await
        .unwrap()
    {
        stores.jobs.complete(&job.scope, job.id, &job.lease).await.unwrap();
        claimed.push((
            job.scope,
            delivery_of(&job.payload).expect("a delivery job names its delivery"),
        ));
    }
    claimed
}

pub async fn run(stores: Stores<'_>, prepare: Prepare) {
    let a = tenant(&prepare).await;
    let b = tenant(&prepare).await;
    keeps_endpoints_in_their_organization(&stores, a, b).await;
    records_each_event_once_for_its_subscribers(&stores, a, b).await;
    records_attempts_on_pending_deliveries(&stores, a, b).await;
    disables_an_endpoint_failing_for_days(&stores, a).await;
    records_the_test_buttons_event(&stores, a, b).await;
    deletes_deliveries_with_their_endpoint(&stores, a).await;
    abandons_a_delivery_without_charging_its_endpoint(&stores, a, b).await;
    holds_an_endpoint_for_one_delivery_at_a_time(&stores, a, b).await;
    reads_history_in_its_endpoint_environment_and_workspace(&stores, a, b).await;
}

async fn keeps_endpoints_in_their_organization(stores: &Stores<'_>, a: Tenant, b: Tenant) {
    let org = a.organization_id;
    let t = Timestamp::now().millis();
    let first = endpoint(
        stores,
        org,
        Environment::Sandbox,
        &[WebhookEventType::RunCompleted],
        1,
        Timestamp::from_millis(t),
    )
    .await;
    let second = endpoint(
        stores,
        org,
        Environment::Production,
        &[WebhookEventType::RunFailed, WebhookEventType::RunCompleted],
        2,
        Timestamp::from_millis(t + 1),
    )
    .await;
    let theirs = endpoint(
        stores,
        b.organization_id,
        Environment::Sandbox,
        &[WebhookEventType::RunCompleted],
        3,
        Timestamp::from_millis(t),
    )
    .await;

    let ids = |list: Vec<zupflow_core::webhooks::WebhookEndpoint>| list.into_iter().map(|e| e.id).collect::<Vec<_>>();
    let all = ids(stores.webhooks.list_endpoints(org, None).await.unwrap());
    assert_eq!(all, vec![first, second], "an organization lists its own, oldest first");
    assert_eq!(
        ids(stores
            .webhooks
            .list_endpoints(org, Some(Environment::Sandbox))
            .await
            .unwrap()),
        vec![first],
        "and only one environment's when asked"
    );
    assert_eq!(
        ids(stores.webhooks.list_endpoints(b.organization_id, None).await.unwrap()),
        vec![theirs]
    );

    assert!(
        stores.webhooks.find_endpoint(org, theirs).await.unwrap().is_none(),
        "another organization's isn't found"
    );
    assert!(
        stores.webhooks.signing_secret(org, theirs).await.unwrap().is_none(),
        "nor is its secret"
    );
    let secret = stores.webhooks.signing_secret(org, first).await.unwrap().unwrap();
    assert_eq!(secret.sealed, sealed(1), "the sealed secret reads back exactly");
    assert_eq!((secret.environment, secret.version), (Environment::Sandbox, 1));

    let changes = EndpointChanges {
        enabled: Some(false),
        ..EndpointChanges::default()
    };
    assert_eq!(
        stores
            .webhooks
            .update_endpoint(org, theirs, &changes, &by_suite)
            .await
            .unwrap_err(),
        WebhookError::NotFound,
        "another organization's can't be changed"
    );
    assert_eq!(
        stores
            .webhooks
            .delete_endpoint(org, theirs, &by_suite)
            .await
            .unwrap_err(),
        WebhookError::NotFound,
        "nor deleted"
    );
    let still = stores
        .webhooks
        .find_endpoint(b.organization_id, theirs)
        .await
        .unwrap()
        .unwrap();
    assert!(still.enabled, "and stays as it was");

    let updated = stores
        .webhooks
        .update_endpoint(
            org,
            first,
            &EndpointChanges {
                url: Some("https://hooks.example/changed".to_owned()),
                events: Some(vec![WebhookEventType::RunCancelled, WebhookEventType::AppToolsChanged]),
                ..EndpointChanges::default()
            },
            &by_suite,
        )
        .await
        .unwrap();
    assert_eq!(updated.url, "https://hooks.example/changed");
    assert_eq!(
        updated.events,
        vec![WebhookEventType::RunCancelled, WebhookEventType::AppToolsChanged]
    );
    assert_eq!(
        (updated.description.as_str(), updated.enabled),
        ("Endpoint 1", true),
        "what an update doesn't name stays"
    );
    let disabled = stores
        .webhooks
        .update_endpoint(org, first, &changes, &by_suite)
        .await
        .unwrap();
    assert!(!disabled.enabled);

    stores.webhooks.delete_endpoint(org, first, &by_suite).await.unwrap();
    stores.webhooks.delete_endpoint(org, second, &by_suite).await.unwrap();
    assert!(stores.webhooks.list_endpoints(org, None).await.unwrap().is_empty());
    assert_eq!(
        stores
            .webhooks
            .delete_endpoint(org, first, &by_suite)
            .await
            .unwrap_err(),
        WebhookError::NotFound
    );
    stores
        .webhooks
        .delete_endpoint(b.organization_id, theirs, &by_suite)
        .await
        .unwrap();
}

async fn records_each_event_once_for_its_subscribers(stores: &Stores<'_>, a: Tenant, b: Tenant) {
    let org = a.organization_id;
    let now = Timestamp::now();
    claim_deliveries(stores).await;
    let completed = [WebhookEventType::RunCompleted];
    let subscriber = endpoint(stores, org, Environment::Sandbox, &completed, 10, now).await;
    let other_type = endpoint(
        stores,
        org,
        Environment::Sandbox,
        &[WebhookEventType::RunFailed],
        11,
        now,
    )
    .await;
    let disabled = endpoint(stores, org, Environment::Sandbox, &completed, 12, now).await;
    stores
        .webhooks
        .update_endpoint(
            org,
            disabled,
            &EndpointChanges {
                enabled: Some(false),
                ..EndpointChanges::default()
            },
            &by_suite,
        )
        .await
        .unwrap();
    let production = endpoint(stores, org, Environment::Production, &completed, 13, now).await;
    let theirs = endpoint(stores, b.organization_id, Environment::Sandbox, &completed, 14, now).await;

    let recorded = event(Environment::Sandbox, Some(a.sandbox), "run.completed");
    let deliveries = stores.webhooks.record_event(org, &recorded).await.unwrap();
    assert_eq!(
        deliveries.len(),
        1,
        "one delivery: the enabled endpoint of the organization and environment subscribed to the type"
    );
    let delivery = stores
        .webhooks
        .find_delivery(org, deliveries[0])
        .await
        .unwrap()
        .unwrap();
    assert_eq!(delivery.endpoint_id, subscriber);
    assert!(![other_type, disabled, production, theirs].contains(&delivery.endpoint_id));
    assert_eq!(
        (
            delivery.event_id,
            delivery.kind.as_str(),
            delivery.body.as_str(),
            delivery.state
        ),
        (
            recorded.id,
            "run.completed",
            recorded.body.as_str(),
            DeliveryState::Pending
        ),
        "the delivery sends the event exactly as recorded"
    );
    assert_eq!(delivery.attempts, 0);
    assert!(delivery.enabled && delivery.subscribed);
    assert_eq!(delivery.url, "https://hooks.example/10");
    assert!(
        stores
            .webhooks
            .find_delivery(b.organization_id, deliveries[0])
            .await
            .unwrap()
            .is_none(),
        "another organization can't read it"
    );
    assert_eq!(
        claim_deliveries(stores).await,
        vec![(Scope::Organization(org), deliveries[0])],
        "its job is queued with it, in the organization's scope"
    );

    assert!(
        stores.webhooks.record_event(org, &recorded).await.unwrap().is_empty(),
        "an event recorded again delivers nothing more"
    );
    assert!(claim_deliveries(stores).await.is_empty(), "and queues nothing");

    // An event is its workspace's environment's, in its own organization.
    for (wrong, why) in [
        (
            event(Environment::Production, Some(a.sandbox), "run.completed"),
            "a Sandbox workspace's event can't be recorded as Production's",
        ),
        (
            event(Environment::Sandbox, Some(b.sandbox), "run.completed"),
            "another organization's workspace can't be named",
        ),
    ] {
        assert_eq!(
            stores.webhooks.record_event(org, &wrong).await.unwrap_err(),
            WebhookError::NotFound,
            "{why}"
        );
    }
    assert!(
        claim_deliveries(stores).await.is_empty(),
        "and nothing is queued for it"
    );

    let production_event = event(Environment::Production, Some(a.production), "run.completed");
    let delivered = stores.webhooks.record_event(org, &production_event).await.unwrap();
    assert_eq!(delivered.len(), 1);
    let found = stores.webhooks.find_delivery(org, delivered[0]).await.unwrap().unwrap();
    assert_eq!(
        found.endpoint_id, production,
        "a Production event reaches Production endpoints only"
    );

    // Unsubscribing after the event was recorded shows on its delivery.
    stores
        .webhooks
        .update_endpoint(
            org,
            subscriber,
            &EndpointChanges {
                events: Some(vec![WebhookEventType::RunFailed]),
                ..EndpointChanges::default()
            },
            &by_suite,
        )
        .await
        .unwrap();
    let unsubscribed = stores
        .webhooks
        .find_delivery(org, deliveries[0])
        .await
        .unwrap()
        .unwrap();
    assert!(
        !unsubscribed.subscribed,
        "a delivery shows whether its endpoint still subscribes"
    );
    assert_eq!(
        stores
            .webhooks
            .skip_delivery(b.organization_id, deliveries[0])
            .await
            .unwrap_err(),
        WebhookError::NotFound,
        "another organization can't skip it"
    );
    stores.webhooks.skip_delivery(org, deliveries[0]).await.unwrap();
    let skipped = stores
        .webhooks
        .find_delivery(org, deliveries[0])
        .await
        .unwrap()
        .unwrap();
    assert_eq!(skipped.state, DeliveryState::Skipped);
    assert_eq!(
        stores.webhooks.skip_delivery(org, deliveries[0]).await.unwrap_err(),
        WebhookError::NotFound,
        "only a pending delivery is skipped"
    );
    claim_deliveries(stores).await;
    for id in [subscriber, other_type, disabled, production] {
        stores.webhooks.delete_endpoint(org, id, &by_suite).await.unwrap();
    }
    stores
        .webhooks
        .delete_endpoint(b.organization_id, theirs, &by_suite)
        .await
        .unwrap();
}

async fn records_attempts_on_pending_deliveries(stores: &Stores<'_>, a: Tenant, b: Tenant) {
    let org = a.organization_id;
    claim_deliveries(stores).await;
    let hook = endpoint(
        stores,
        org,
        Environment::Sandbox,
        &[WebhookEventType::RunCancelled],
        20,
        Timestamp::now(),
    )
    .await;
    let recorded = event(Environment::Sandbox, Some(a.sandbox), "run.cancelled");
    let delivery = stores.webhooks.record_event(org, &recorded).await.unwrap()[0];
    let at = Timestamp::now();

    assert_eq!(
        stores
            .webhooks
            .record_attempt(b.organization_id, delivery, &failure(1, at, false))
            .await
            .unwrap_err(),
        WebhookError::NotFound,
        "another organization can't record an attempt"
    );
    stores
        .webhooks
        .record_attempt(org, delivery, &failure(1, at, false))
        .await
        .unwrap();
    let pending = stores.webhooks.find_delivery(org, delivery).await.unwrap().unwrap();
    assert_eq!((pending.state, pending.attempts), (DeliveryState::Pending, 1));
    let last = stores
        .webhooks
        .find_endpoint(org, hook)
        .await
        .unwrap()
        .unwrap()
        .last_delivery
        .unwrap();
    assert_eq!(
        (last.status_code, last.delivered_at),
        (503, at),
        "each answer is the endpoint's last delivery"
    );

    let later = Timestamp::from_millis(at.millis() + 1_000);
    stores
        .webhooks
        .record_attempt(org, delivery, &success(2, later))
        .await
        .unwrap();
    let done = stores.webhooks.find_delivery(org, delivery).await.unwrap().unwrap();
    assert_eq!((done.state, done.attempts), (DeliveryState::Succeeded, 2));
    let endpoint_now = stores.webhooks.find_endpoint(org, hook).await.unwrap().unwrap();
    assert_eq!(endpoint_now.last_delivery.map(|last| last.status_code), Some(204));
    assert_eq!(
        stores
            .webhooks
            .record_attempt(org, delivery, &success(3, later))
            .await
            .unwrap_err(),
        WebhookError::NotFound,
        "a delivery that succeeded takes no more attempts"
    );

    // No answer at all is recorded as `0`.
    let again = event(Environment::Sandbox, Some(a.sandbox), "run.cancelled");
    let delivery = stores.webhooks.record_event(org, &again).await.unwrap()[0];
    let unanswered = Attempt {
        number: 1,
        status: None,
        error: Some("timeout".to_owned()),
        at: later,
        duration_ms: 10_000,
        state: DeliveryState::Failed,
        next_attempt_at: None,
    };
    stores
        .webhooks
        .record_attempt(org, delivery, &unanswered)
        .await
        .unwrap();
    let failed = stores.webhooks.find_delivery(org, delivery).await.unwrap().unwrap();
    assert_eq!(failed.state, DeliveryState::Failed);
    assert_eq!(
        stores
            .webhooks
            .find_endpoint(org, hook)
            .await
            .unwrap()
            .unwrap()
            .last_delivery
            .map(|last| last.status_code),
        Some(0)
    );
    claim_deliveries(stores).await;
    stores.webhooks.delete_endpoint(org, hook, &by_suite).await.unwrap();
}

async fn disables_an_endpoint_failing_for_days(stores: &Stores<'_>, a: Tenant) {
    let org = a.organization_id;
    let hook = endpoint(
        stores,
        org,
        Environment::Sandbox,
        &[WebhookEventType::RunFailed],
        30,
        Timestamp::now(),
    )
    .await;
    let start = Timestamp::now().millis();
    let window = i64::try_from(DISABLE_AFTER.as_millis()).unwrap();
    let fail_at = |offset: i64| async move {
        let recorded = event(Environment::Sandbox, Some(a.sandbox), "run.failed");
        let delivery = stores.webhooks.record_event(org, &recorded).await.unwrap()[0];
        stores
            .webhooks
            .record_attempt(org, delivery, &failure(1, Timestamp::from_millis(start + offset), true))
            .await
            .unwrap();
    };
    let enabled = || async { stores.webhooks.find_endpoint(org, hook).await.unwrap().unwrap().enabled };

    fail_at(0).await;
    fail_at(window - 1).await;
    assert!(enabled().await, "failing for less than the window, it stays enabled");
    fail_at(window).await;
    assert!(!enabled().await, "failing for the whole window, it's disabled");

    // Enabled again, it starts counting afresh.
    stores
        .webhooks
        .update_endpoint(
            org,
            hook,
            &EndpointChanges {
                enabled: Some(true),
                ..EndpointChanges::default()
            },
            &by_suite,
        )
        .await
        .unwrap();
    fail_at(window + 1).await;
    assert!(enabled().await, "enabling it again forgets how long it was failing");

    // A success clears it too.
    let recorded = event(Environment::Sandbox, Some(a.sandbox), "run.failed");
    let delivery = stores.webhooks.record_event(org, &recorded).await.unwrap()[0];
    stores
        .webhooks
        .record_attempt(org, delivery, &success(1, Timestamp::from_millis(start + window + 2)))
        .await
        .unwrap();
    fail_at(2 * window + 3).await;
    assert!(enabled().await, "a success restarts the count");

    // So does the test button's success; its failure starts no count.
    fail_at(3 * window).await;
    let test = |offset: i64, attempt: fn(u32, Timestamp) -> Attempt| async move {
        let test = event(Environment::Sandbox, None, TEST_EVENT);
        stores
            .webhooks
            .record_test(org, hook, &test, &attempt(1, Timestamp::from_millis(start + offset)))
            .await
            .unwrap();
    };
    test(4 * window - 1, success).await;
    fail_at(4 * window).await;
    assert!(enabled().await, "a successful test restarts the count");
    test(5 * window - 1, success).await;
    test(5 * window, |number, at| failure(number, at, true)).await;
    fail_at(6 * window).await;
    assert!(enabled().await, "a failed test isn't when it started failing");
    claim_deliveries(stores).await;
    stores.webhooks.delete_endpoint(org, hook, &by_suite).await.unwrap();
}

async fn records_the_test_buttons_event(stores: &Stores<'_>, a: Tenant, b: Tenant) {
    let org = a.organization_id;
    claim_deliveries(stores).await;
    let hook = endpoint(
        stores,
        org,
        Environment::Production,
        &[WebhookEventType::RunCompleted],
        40,
        Timestamp::now(),
    )
    .await;
    let at = Timestamp::now();
    let test = event(Environment::Production, None, TEST_EVENT);
    let attempt = success(1, at);
    assert_eq!(
        stores
            .webhooks
            .record_test(b.organization_id, hook, &test, &attempt)
            .await
            .unwrap_err(),
        WebhookError::NotFound,
        "another organization's endpoint can't be tested"
    );
    let delivery = stores.webhooks.record_test(org, hook, &test, &attempt).await.unwrap();
    let recorded = stores.webhooks.find_delivery(org, delivery).await.unwrap().unwrap();
    assert_eq!(
        (recorded.kind.as_str(), recorded.state, recorded.attempts),
        (TEST_EVENT, DeliveryState::Succeeded, 1)
    );
    assert!(!recorded.subscribed, "nobody subscribes to the test button's event");
    let last = stores
        .webhooks
        .find_endpoint(org, hook)
        .await
        .unwrap()
        .unwrap()
        .last_delivery
        .unwrap();
    assert_eq!((last.status_code, last.delivered_at), (204, at));
    assert!(
        claim_deliveries(stores).await.is_empty(),
        "a test is sent outside the queue"
    );
    stores.webhooks.delete_endpoint(org, hook, &by_suite).await.unwrap();
}

async fn deletes_deliveries_with_their_endpoint(stores: &Stores<'_>, a: Tenant) {
    let org = a.organization_id;
    let hook = endpoint(
        stores,
        org,
        Environment::Sandbox,
        &[WebhookEventType::RunCompleted],
        50,
        Timestamp::now(),
    )
    .await;
    let recorded = event(Environment::Sandbox, Some(a.sandbox), "run.completed");
    let delivery = stores.webhooks.record_event(org, &recorded).await.unwrap()[0];
    stores.webhooks.delete_endpoint(org, hook, &by_suite).await.unwrap();
    assert!(
        stores.webhooks.find_delivery(org, delivery).await.unwrap().is_none(),
        "a deleted endpoint's deliveries go with it"
    );
    claim_deliveries(stores).await;
}

async fn abandons_a_delivery_without_charging_its_endpoint(stores: &Stores<'_>, a: Tenant, b: Tenant) {
    let org = a.organization_id;
    let hook = endpoint(
        stores,
        org,
        Environment::Sandbox,
        &[WebhookEventType::RunCompleted],
        60,
        Timestamp::now(),
    )
    .await;
    let recorded = event(Environment::Sandbox, Some(a.sandbox), "run.completed");
    let delivery = stores.webhooks.record_event(org, &recorded).await.unwrap()[0];
    assert_eq!(
        stores
            .webhooks
            .abandon_delivery(b.organization_id, delivery, "store_unavailable")
            .await
            .unwrap_err(),
        WebhookError::NotFound,
        "another organization can't abandon it"
    );
    stores
        .webhooks
        .abandon_delivery(org, delivery, "store_unavailable")
        .await
        .unwrap();
    let abandoned = stores.webhooks.find_delivery(org, delivery).await.unwrap().unwrap();
    assert_eq!(
        (abandoned.state, abandoned.attempts),
        (DeliveryState::Failed, 0),
        "failed, with no attempt counted"
    );
    let endpoint_now = stores.webhooks.find_endpoint(org, hook).await.unwrap().unwrap();
    assert_eq!(
        (endpoint_now.last_delivery, endpoint_now.enabled),
        (None, true),
        "the endpoint didn't answer, so it has no last delivery"
    );
    assert_eq!(
        stores
            .webhooks
            .abandon_delivery(org, delivery, "store_unavailable")
            .await
            .unwrap_err(),
        WebhookError::NotFound,
        "only a pending delivery is abandoned"
    );
    claim_deliveries(stores).await;
    stores.webhooks.delete_endpoint(org, hook, &by_suite).await.unwrap();
}

async fn holds_an_endpoint_for_one_delivery_at_a_time(stores: &Stores<'_>, a: Tenant, b: Tenant) {
    let org = a.organization_id;
    let hook = endpoint(
        stores,
        org,
        Environment::Sandbox,
        &[WebhookEventType::RunCompleted],
        70,
        Timestamp::now(),
    )
    .await;
    let other = endpoint(
        stores,
        org,
        Environment::Sandbox,
        &[WebhookEventType::RunCompleted],
        71,
        Timestamp::now(),
    )
    .await;
    let first = LeaseToken::generate().unwrap();
    let second = LeaseToken::generate().unwrap();
    let now = Timestamp::now();
    let until = Timestamp::from_millis(now.millis() + 30_000);
    let hold = |endpoint, holder, at: Timestamp| async move {
        stores
            .webhooks
            .hold_endpoint(org, endpoint, holder, at, Timestamp::from_millis(at.millis() + 30_000))
            .await
    };
    assert_eq!(hold(hook, &first, now).await, Ok(true));
    assert_eq!(
        hold(hook, &first, now).await,
        Ok(true),
        "a holder holding it already still does"
    );
    assert_eq!(hold(hook, &second, now).await, Ok(false), "held by one, not two");
    assert_eq!(
        hold(other, &second, now).await,
        Ok(true),
        "each endpoint has its own hold"
    );
    assert_eq!(
        stores
            .webhooks
            .hold_endpoint(b.organization_id, hook, &second, now, until)
            .await,
        Err(WebhookError::NotFound),
        "another organization can't hold it"
    );
    stores.webhooks.release_endpoint(org, hook, &second).await.unwrap();
    stores
        .webhooks
        .release_endpoint(b.organization_id, hook, &first)
        .await
        .unwrap();
    assert_eq!(
        hold(hook, &second, now).await,
        Ok(false),
        "only its holder, in its organization, lets go"
    );
    assert_eq!(
        hold(hook, &second, until).await,
        Ok(true),
        "a hold that ran out is anyone's"
    );
    stores.webhooks.release_endpoint(org, hook, &second).await.unwrap();
    assert_eq!(hold(hook, &first, now).await, Ok(true), "let go, it's free");
    stores.webhooks.release_endpoint(org, hook, &first).await.unwrap();
    stores.webhooks.release_endpoint(org, other, &second).await.unwrap();
    for id in [hook, other] {
        stores.webhooks.delete_endpoint(org, id, &by_suite).await.unwrap();
    }
}

async fn reads_history_in_its_endpoint_environment_and_workspace(stores: &Stores<'_>, a: Tenant, b: Tenant) {
    use serde_json::json;

    let org = a.organization_id;
    let env = Environment::Sandbox;
    let now = Timestamp::now();
    let hook = endpoint(stores, org, env, &[WebhookEventType::RunCompleted], 80, now).await;
    let other = endpoint(stores, org, env, &[WebhookEventType::RunFailed], 81, now).await;
    let production = endpoint(
        stores,
        org,
        Environment::Production,
        &[WebhookEventType::RunCompleted],
        82,
        now,
    )
    .await;
    let foreign = endpoint(
        stores,
        b.organization_id,
        env,
        &[WebhookEventType::RunCompleted],
        83,
        now,
    )
    .await;
    let mut deliveries = Vec::new();
    for (organization, workspace, environment, external_id) in [
        (org, a.sandbox, env, "tenant-a"),
        (org, a.sibling, env, "tenant-b"),
        (org, a.production, Environment::Production, "tenant-prod"),
        (b.organization_id, b.sandbox, env, "tenant-other"),
    ] {
        let id = WebhookEventId::generate();
        let event = NewWebhookEvent {
            id,
            environment,
            workspace_id: Some(workspace),
            kind: "run.completed".to_owned(),
            body: event_body(
                id,
                "run.completed",
                now,
                environment,
                organization,
                Some((workspace, Some(external_id))),
                json!({ "object": "run", "id": RunId::generate(), "agent_id": AgentId::generate(), "status": "completed" }),
            ),
            created_at: now,
        };
        deliveries.push(stores.webhooks.record_event(organization, &event).await.unwrap()[0]);
    }
    let first = deliveries[0];
    let sibling = deliveries[1];
    let page = stores
        .webhooks
        .list_deliveries(org, hook, env, &DeliveryFilter::default(), None, 1)
        .await
        .unwrap();
    assert_eq!(page.total, 2, "only the endpoint's environment and organization");
    assert_eq!(page.items.len(), 1);
    let cursor = page.next_cursor.as_deref().unwrap();
    let rest = stores
        .webhooks
        .list_deliveries(org, hook, env, &DeliveryFilter::default(), Some(cursor), 1)
        .await
        .unwrap();
    assert_eq!(rest.total, 2);
    assert_eq!(rest.items.len(), 1);
    assert_ne!(rest.items[0].id, page.items[0].id);
    assert!(rest.next_cursor.is_none());
    for (organization, endpoint_id, environment) in [
        (org, other, env),
        (org, production, Environment::Production),
        (b.organization_id, foreign, env),
    ] {
        assert_eq!(
            stores
                .webhooks
                .list_deliveries(
                    organization,
                    endpoint_id,
                    environment,
                    &DeliveryFilter::default(),
                    Some(cursor),
                    25
                )
                .await
                .unwrap_err(),
            WebhookError::InvalidCursor,
            "a cursor is bound to all three scopes"
        );
    }
    for bad in ["", "not-a-cursor"] {
        assert_eq!(
            stores
                .webhooks
                .list_deliveries(org, hook, env, &DeliveryFilter::default(), Some(bad), 25)
                .await
                .unwrap_err(),
            WebhookError::InvalidCursor
        );
    }
    for (workspace, expected) in [
        (a.sandbox, Some(first)),
        (a.sibling, Some(sibling)),
        (a.production, None),
        (b.sandbox, None),
        (WorkspaceId::generate(), None),
    ] {
        let filter = DeliveryFilter {
            workspace_id: Some(workspace),
            ..DeliveryFilter::default()
        };
        let page = stores
            .webhooks
            .list_deliveries(org, hook, env, &filter, None, 25)
            .await
            .unwrap();
        assert_eq!(
            page.items.iter().map(|item| item.id).collect::<Vec<_>>(),
            expected.into_iter().collect::<Vec<_>>()
        );
        assert_eq!(page.total, u64::from(expected.is_some()));
        assert_eq!(
            stores
                .webhooks
                .list_deliveries(org, hook, env, &filter, Some(cursor), 25)
                .await
                .unwrap_err(),
            WebhookError::InvalidCursor
        );
    }
    assert_eq!(
        stores
            .webhooks
            .list_deliveries(org, foreign, env, &DeliveryFilter::default(), None, 25)
            .await
            .unwrap_err(),
        WebhookError::NotFound
    );
    assert_eq!(
        stores
            .webhooks
            .list_deliveries(org, hook, Environment::Production, &DeliveryFilter::default(), None, 25)
            .await
            .unwrap_err(),
        WebhookError::NotFound
    );
    for (organization, endpoint_id, environment, delivery) in [
        (b.organization_id, foreign, env, first),
        (org, other, env, first),
        (org, hook, Environment::Production, first),
        (org, hook, env, deliveries[2]),
        (org, hook, env, deliveries[3]),
    ] {
        assert!(
            stores
                .webhooks
                .find_delivery_detail(organization, endpoint_id, environment, delivery)
                .await
                .unwrap()
                .is_none()
        );
        assert_eq!(
            stores
                .webhooks
                .list_delivery_attempts(organization, endpoint_id, environment, delivery, None, 25)
                .await
                .unwrap_err(),
            WebhookError::NotFound
        );
    }
    let pending = stores
        .webhooks
        .find_delivery_detail(org, hook, env, first)
        .await
        .unwrap()
        .unwrap();
    assert!(pending.attempt_history_complete);
    assert_eq!(
        pending.summary.workspace.as_ref().unwrap().external_id.as_deref(),
        Some("tenant-a")
    );
    assert_eq!(pending.event["workspace"]["id"], json!(a.sandbox));
    assert_eq!(pending.event["id"], json!(pending.summary.event_id));
    let previous_next = pending.summary.next_attempt_at;
    let deferred_until = Timestamp::from_millis(now.millis() + 5_000);
    for (organization, attempts, previous) in [
        (b.organization_id, 0, previous_next),
        (org, 1, previous_next),
        (org, 0, Some(deferred_until)),
    ] {
        assert_eq!(
            stores
                .webhooks
                .defer_delivery(organization, first, attempts, previous, deferred_until)
                .await
                .unwrap_err(),
            WebhookError::NotFound,
            "deferral belongs to the organization and the exact progress snapshot"
        );
    }
    stores
        .webhooks
        .defer_delivery(org, first, 0, previous_next, deferred_until)
        .await
        .unwrap();
    assert_eq!(
        stores
            .webhooks
            .defer_delivery(org, first, 0, previous_next, now)
            .await
            .unwrap_err(),
        WebhookError::NotFound,
        "a stale worker cannot overwrite the winner's retry time"
    );
    let deferred = stores
        .webhooks
        .find_delivery_detail(org, hook, env, first)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(deferred.summary.next_attempt_at, Some(deferred_until));
    assert_eq!(deferred.summary.attempts, 0);
    assert!(deferred.attempt_history_complete, "no fabricated attempt");
    assert_eq!(
        stores
            .webhooks
            .list_delivery_attempts(org, hook, env, first, None, 25)
            .await
            .unwrap()
            .total,
        0
    );
    stores
        .webhooks
        .record_attempt(org, first, &failure(1, now, false))
        .await
        .unwrap();
    for stale in [failure(1, now, false), success(3, now)] {
        assert_eq!(
            stores.webhooks.record_attempt(org, first, &stale).await.unwrap_err(),
            WebhookError::NotFound,
            "only the next number is recorded"
        );
    }
    stores
        .webhooks
        .record_attempt(org, first, &success(2, Timestamp::from_millis(now.millis() + 100)))
        .await
        .unwrap();
    let first_attempt = stores
        .webhooks
        .list_delivery_attempts(org, hook, env, first, None, 1)
        .await
        .unwrap();
    assert_eq!(first_attempt.total, 2);
    assert_eq!(
        (
            first_attempt.items[0].number,
            first_attempt.items[0].status_code,
            first_attempt.items[0].duration_ms
        ),
        (1, Some(503), Some(12))
    );
    assert_eq!(first_attempt.items[0].error_class.as_deref(), Some("status"));
    let attempt_cursor = first_attempt.next_cursor.as_deref().unwrap();
    let second_attempt = stores
        .webhooks
        .list_delivery_attempts(org, hook, env, first, Some(attempt_cursor), 1)
        .await
        .unwrap();
    assert_eq!(second_attempt.total, 2);
    assert_eq!(
        (
            second_attempt.items[0].number,
            second_attempt.items[0].status_code,
            second_attempt.items[0].duration_ms
        ),
        (2, Some(204), Some(7))
    );
    assert!(second_attempt.next_cursor.is_none());
    assert_eq!(
        stores
            .webhooks
            .list_delivery_attempts(org, hook, env, sibling, Some(attempt_cursor), 25)
            .await
            .unwrap_err(),
        WebhookError::InvalidCursor
    );
    assert_eq!(
        stores
            .webhooks
            .list_delivery_attempts(org, hook, env, first, Some(cursor), 25)
            .await
            .unwrap_err(),
        WebhookError::InvalidCursor
    );
    let detail = stores
        .webhooks
        .find_delivery_detail(org, hook, env, first)
        .await
        .unwrap()
        .unwrap();
    assert!(detail.attempt_history_complete);
    assert_eq!(detail.summary.attempts, 2);
    assert_eq!(detail.summary.state, DeliveryState::Succeeded);
    assert_eq!(
        stores
            .webhooks
            .defer_delivery(org, first, 2, None, deferred_until)
            .await
            .unwrap_err(),
        WebhookError::NotFound,
        "a terminal delivery cannot be deferred"
    );
    let filter = DeliveryFilter {
        state: Some(DeliveryState::Succeeded),
        event_type: Some("run.completed".to_owned()),
        ..DeliveryFilter::default()
    };
    let page = stores
        .webhooks
        .list_deliveries(org, hook, env, &filter, None, 25)
        .await
        .unwrap();
    assert_eq!(page.items.iter().map(|item| item.id).collect::<Vec<_>>(), vec![first]);
    assert_eq!(page.total, 1);
    assert!(
        stores
            .webhooks
            .list_deliveries(
                org,
                hook,
                env,
                &DeliveryFilter {
                    matches_nothing: true,
                    ..DeliveryFilter::default()
                },
                None,
                25
            )
            .await
            .unwrap()
            .items
            .is_empty()
    );
    let mut test = event(Environment::Production, None, TEST_EVENT);
    assert_eq!(
        stores
            .webhooks
            .record_test(org, hook, &test, &success(1, now))
            .await
            .unwrap_err(),
        WebhookError::NotFound,
        "the endpoint never records another environment's test"
    );
    test.environment = env;
    test.body = event_body(
        test.id,
        TEST_EVENT,
        test.created_at,
        env,
        org,
        None,
        json!({ "object": "webhook_endpoint", "id": hook }),
    );
    let test_delivery = stores
        .webhooks
        .record_test(org, hook, &test, &success(1, now))
        .await
        .unwrap();
    let test_detail = stores
        .webhooks
        .find_delivery_detail(org, hook, env, test_delivery)
        .await
        .unwrap()
        .unwrap();
    assert!(test_detail.attempt_history_complete);
    assert!(test_detail.summary.workspace.is_none());
    assert_eq!(
        stores
            .webhooks
            .list_delivery_attempts(org, hook, env, test_delivery, None, 25)
            .await
            .unwrap()
            .total,
        1
    );
    claim_deliveries(stores).await;
    for endpoint_id in [hook, other, production] {
        stores
            .webhooks
            .delete_endpoint(org, endpoint_id, &by_suite)
            .await
            .unwrap();
    }
    assert!(
        stores
            .webhooks
            .find_delivery_detail(org, hook, env, first)
            .await
            .unwrap()
            .is_none()
    );
    stores
        .webhooks
        .delete_endpoint(b.organization_id, foreign, &by_suite)
        .await
        .unwrap();
}
