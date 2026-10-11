//! Manual delivery creates its own bounded job and leaves the source intact.

use serde_json::json;
use zupflow_core::audit::{AuditAction, AuditEntry, AuditObject, AuditPlace, AuditTarget};
use zupflow_core::jobs::Retry;
use zupflow_core::webhooks::{DELIVERY_JOB_ATTEMPTS, DeliverySummary, ResendError, WebhookManager};

use super::*;

pub(super) fn audit(summary: &DeliverySummary) -> AuditEntry {
    AuditEntry {
        action: AuditAction::WebhookDeliveryResent,
        target: AuditTarget::new(AuditObject::WebhookDelivery(summary.id), &summary.event_type),
        place: summary
            .workspace
            .as_ref()
            .map_or(AuditPlace::Environment(summary.environment), |workspace| {
                AuditPlace::Workspace(workspace.id)
            }),
        ..by_suite(&())
    }
}

pub(super) async fn run(stores: &Stores<'_>, a: Tenant, b: Tenant) {
    let env = Environment::Sandbox;
    let now = Timestamp::now();
    let hook = endpoint(
        stores,
        a.organization_id,
        env,
        &[WebhookEventType::RunCompleted],
        31,
        now,
    )
    .await;
    let production = endpoint(
        stores,
        a.organization_id,
        Environment::Production,
        &[WebhookEventType::RunCompleted],
        32,
        now,
    )
    .await;
    let foreign = endpoint(
        stores,
        b.organization_id,
        env,
        &[WebhookEventType::RunCompleted],
        33,
        now,
    )
    .await;
    let other = endpoint(stores, a.organization_id, env, &[WebhookEventType::RunFailed], 34, now).await;
    let mut sources = Vec::new();
    for (tenant, workspace, environment, endpoint_id, terminal, manager) in [
        (
            a,
            a.sandbox,
            env,
            hook,
            DeliveryState::Failed,
            WebhookManager::Member(a.admin),
        ),
        (
            a,
            a.sibling,
            env,
            hook,
            DeliveryState::Succeeded,
            WebhookManager::ApiKey(a.sandbox_key),
        ),
        (
            a,
            a.production,
            Environment::Production,
            production,
            DeliveryState::Skipped,
            WebhookManager::ApiKey(a.production_key),
        ),
        (
            b,
            b.sandbox,
            env,
            foreign,
            DeliveryState::Succeeded,
            WebhookManager::Member(b.admin),
        ),
    ] {
        let org = tenant.organization_id;
        let id = WebhookEventId::generate();
        let body = event_body(
            id,
            "run.completed",
            now,
            environment,
            org,
            Some((workspace, Some("original"))),
            json!({ "object": "run", "id": RunId::generate(), "agent_id": AgentId::generate(), "status": "completed" }),
        );
        // Noncanonical JSON makes byte preservation observable even if a
        // future implementation accidentally reserializes a parsed event.
        let body = format!("\n {} \n", body.replace("original", "or\\u0069ginal"));
        let event = NewWebhookEvent {
            id,
            environment,
            workspace_id: Some(workspace),
            kind: "run.completed".to_owned(),
            body,
            created_at: now,
        };
        let (one, two) = tokio::join!(
            stores.webhooks.record_event(org, &event),
            stores.webhooks.record_event(org, &event)
        );
        let deliveries: Vec<_> = one.unwrap().into_iter().chain(two.unwrap()).collect();
        assert_eq!(
            deliveries.len(),
            1,
            "concurrent automatic announcement records one original"
        );
        let delivery = deliveries[0];
        assert_eq!(claim_deliveries(stores).await, [(Scope::Organization(org), delivery)]);
        match terminal {
            DeliveryState::Failed => {
                for number in 1..=8 {
                    stores
                        .webhooks
                        .record_attempt(org, delivery, &failure(number, now, number == 8))
                        .await
                        .unwrap();
                }
            }
            DeliveryState::Succeeded => {
                stores
                    .webhooks
                    .record_attempt(org, delivery, &failure(1, now, false))
                    .await
                    .unwrap();
                stores
                    .webhooks
                    .record_attempt(org, delivery, &success(2, now))
                    .await
                    .unwrap();
            }
            DeliveryState::Skipped => stores.webhooks.skip_delivery(org, delivery).await.unwrap(),
            DeliveryState::Pending => unreachable!(),
        }
        let source = stores
            .webhooks
            .find_delivery_detail(org, endpoint_id, environment, delivery)
            .await
            .unwrap()
            .unwrap();
        let attempts = stores
            .webhooks
            .list_delivery_attempts(org, endpoint_id, environment, delivery, None, 25)
            .await
            .unwrap();
        let resend = stores
            .webhooks
            .resend_delivery(org, endpoint_id, environment, delivery, manager, &audit)
            .await
            .unwrap();
        assert_ne!(resend.id, delivery);
        assert_eq!(resend.event_id, event.id);
        assert_eq!(resend.workspace.as_ref().unwrap().id, workspace);
        assert_eq!(
            resend.workspace.as_ref().unwrap().external_id.as_deref(),
            Some("original")
        );
        assert_eq!((resend.state, resend.attempts), (DeliveryState::Pending, 0));
        assert_eq!(
            (resend.last_status_code, resend.last_error, resend.last_attempt_at),
            (None, None, None)
        );
        assert_eq!(resend.next_attempt_at, Some(resend.created_at));
        let pending = stores.webhooks.find_delivery(org, resend.id).await.unwrap().unwrap();
        assert_eq!(pending.body, event.body, "exact signed bytes survive a resend");
        assert!(pending.subscribed && pending.enabled);
        assert_eq!(
            stores
                .webhooks
                .list_delivery_attempts(org, endpoint_id, environment, resend.id, None, 25)
                .await
                .unwrap()
                .total,
            0
        );
        assert_eq!(
            stores
                .webhooks
                .resend_delivery(org, endpoint_id, environment, resend.id, manager, &audit)
                .await
                .unwrap_err(),
            ResendError::Pending
        );
        assert!(
            stores.webhooks.record_event(org, &event).await.unwrap().is_empty(),
            "replay after a resend still creates no original or job"
        );
        exhaust_job(stores, org, resend.id).await;
        assert_eq!(
            stores
                .webhooks
                .find_delivery_detail(org, endpoint_id, environment, delivery)
                .await
                .unwrap()
                .unwrap(),
            source
        );
        assert_eq!(
            stores
                .webhooks
                .list_delivery_attempts(org, endpoint_id, environment, delivery, None, 25)
                .await
                .unwrap()
                .items,
            attempts.items
        );
        sources.push(delivery);
    }
    let source = sources[0];
    for (org, endpoint_id, environment, delivery, manager, expected) in [
        (
            a.organization_id,
            other,
            env,
            source,
            WebhookManager::Member(a.admin),
            ResendError::NotFound,
        ),
        (
            a.organization_id,
            hook,
            Environment::Production,
            source,
            WebhookManager::Member(a.admin),
            ResendError::NotFound,
        ),
        (
            b.organization_id,
            foreign,
            env,
            source,
            WebhookManager::Member(b.admin),
            ResendError::NotFound,
        ),
        (
            a.organization_id,
            hook,
            env,
            sources[1],
            WebhookManager::Member(b.admin),
            ResendError::NotAllowed,
        ),
        (
            a.organization_id,
            hook,
            env,
            source,
            WebhookManager::ApiKey(a.production_key),
            ResendError::NotAllowed,
        ),
        (
            a.organization_id,
            hook,
            env,
            source,
            WebhookManager::ApiKey(b.sandbox_key),
            ResendError::NotAllowed,
        ),
        (
            a.organization_id,
            hook,
            env,
            WebhookDeliveryId::generate(),
            WebhookManager::Member(a.admin),
            ResendError::NotFound,
        ),
    ] {
        assert_eq!(
            stores
                .webhooks
                .resend_delivery(org, endpoint_id, environment, delivery, manager, &audit)
                .await
                .unwrap_err(),
            expected
        );
    }
    let org = a.organization_id;
    let manager = WebhookManager::Member(a.admin);
    for wrong_place in [
        AuditPlace::Organization,
        AuditPlace::Environment(env),
        AuditPlace::Workspace(a.sibling),
        AuditPlace::Workspace(a.production),
        AuditPlace::Workspace(b.sandbox),
    ] {
        assert_eq!(
            stores
                .webhooks
                .resend_delivery(org, hook, env, source, manager, &|summary| AuditEntry {
                    place: wrong_place,
                    ..audit(summary)
                })
                .await
                .unwrap_err(),
            ResendError::Unavailable
        );
    }
    assert!(
        claim_deliveries(stores).await.is_empty(),
        "refused resends queue nothing"
    );
    eligibility(stores, a, hook, source).await;
}

async fn exhaust_job(stores: &Stores<'_>, org: OrganizationId, delivery: WebhookDeliveryId) {
    let kind = Name::new(DELIVER).unwrap();
    for number in 1..=DELIVERY_JOB_ATTEMPTS {
        let job = stores
            .jobs
            .claim_next(std::slice::from_ref(&kind), Duration::from_secs(60))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(job.scope, Scope::Organization(org));
        assert_eq!(delivery_of(&job.payload), Some(delivery));
        assert_eq!(job.attempt, number, "a resend has its own normal bounded job budget");
        assert_eq!(
            stores
                .jobs
                .retry(
                    &job.scope,
                    job.id,
                    &job.lease,
                    Duration::ZERO,
                    &Name::new("failed").unwrap()
                )
                .await
                .unwrap(),
            if number == DELIVERY_JOB_ATTEMPTS {
                Retry::Exhausted
            } else {
                Retry::Scheduled
            }
        );
    }
    assert!(
        stores
            .jobs
            .claim_next(&[kind], Duration::from_secs(60))
            .await
            .unwrap()
            .is_none()
    );
}

async fn eligibility(stores: &Stores<'_>, tenant: Tenant, hook: WebhookEndpointId, source: WebhookDeliveryId) {
    let org = tenant.organization_id;
    let env = Environment::Sandbox;
    let manager = WebhookManager::Member(tenant.admin);
    stores
        .webhooks
        .update_endpoint(
            org,
            hook,
            &EndpointChanges {
                enabled: Some(false),
                ..EndpointChanges::default()
            },
            &by_suite,
        )
        .await
        .unwrap();
    assert_eq!(
        stores
            .webhooks
            .resend_delivery(org, hook, env, source, manager, &audit)
            .await
            .unwrap_err(),
        ResendError::Disabled
    );
    stores
        .webhooks
        .update_endpoint(
            org,
            hook,
            &EndpointChanges {
                enabled: Some(true),
                events: Some(vec![WebhookEventType::RunFailed]),
                ..EndpointChanges::default()
            },
            &by_suite,
        )
        .await
        .unwrap();
    assert_eq!(
        stores
            .webhooks
            .resend_delivery(org, hook, env, source, manager, &audit)
            .await
            .unwrap_err(),
        ResendError::NotSubscribed
    );
    assert!(claim_deliveries(stores).await.is_empty());

    let now = Timestamp::now();
    let id = WebhookEventId::generate();
    let test = NewWebhookEvent {
        id,
        environment: env,
        workspace_id: None,
        kind: TEST_EVENT.to_owned(),
        created_at: now,
        body: event_body(
            id,
            TEST_EVENT,
            now,
            env,
            org,
            None,
            json!({ "object": "webhook_endpoint", "id": hook }),
        ),
    };
    let original = stores
        .webhooks
        .record_test(org, hook, &test, &failure(1, now, true))
        .await
        .unwrap();
    let resend = stores
        .webhooks
        .resend_delivery(org, hook, env, original, manager, &audit)
        .await
        .unwrap();
    assert!(resend.workspace.is_none());
    let pending = stores.webhooks.find_delivery(org, resend.id).await.unwrap().unwrap();
    assert!(
        pending.subscribed,
        "a queued test resend remains exempt from subscription checks"
    );
    assert_eq!(pending.body, test.body);
    assert_eq!(claim_deliveries(stores).await, [(Scope::Organization(org), resend.id)]);
    stores
        .webhooks
        .record_attempt(org, resend.id, &success(1, now))
        .await
        .unwrap();
    stores
        .webhooks
        .update_endpoint(
            org,
            hook,
            &EndpointChanges {
                events: Some(vec![WebhookEventType::RunCompleted]),
                ..EndpointChanges::default()
            },
            &by_suite,
        )
        .await
        .unwrap();
    let first = stores
        .webhooks
        .resend_delivery(org, hook, env, source, manager, &audit)
        .await
        .unwrap();
    let second = stores
        .webhooks
        .resend_delivery(org, hook, env, source, manager, &audit)
        .await
        .unwrap();
    assert_ne!(
        first.id, second.id,
        "unrelated pending work does not block an explicit new request"
    );
    let jobs = claim_deliveries(stores).await;
    assert_eq!(jobs.len(), 2);
    assert!(jobs.contains(&(Scope::Organization(org), first.id)));
    assert!(jobs.contains(&(Scope::Organization(org), second.id)));
}
