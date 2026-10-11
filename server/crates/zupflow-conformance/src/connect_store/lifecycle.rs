//! Atomic Connect outcomes and immutable lifecycle delivery snapshots.
use std::collections::HashSet;
use std::time::Duration;

use serde_json::{Value, json};
use zupflow_core::audit::{AuditFilter, AuditStore};
use zupflow_core::connect::{ConnectError, ConnectIntent, ConnectSessionStatus, ConnectStore, Reference};
use zupflow_core::environment::Environment;
use zupflow_core::ids::{ConnectSessionId, InstallationId, OrganizationId, WebhookEndpointId};
use zupflow_core::installations::{InstallationError, InstallationStore, InstalledApp};
use zupflow_core::jobs::JobQueue;
use zupflow_core::scope::{Name, Scope};
use zupflow_core::time::Timestamp;
use zupflow_core::webhooks::{DELIVER, DeliveryFilter, NewEndpoint, WebhookEventType, WebhookStore, delivery_of};

use super::{Prepare, completion, new, start, tenant};
use crate::audit_store::{by_suite, by_suite_in};

async fn endpoint(store: &dyn WebhookStore, org: OrganizationId, env: Environment) -> WebhookEndpointId {
    let id = WebhookEndpointId::generate();
    store
        .create_endpoint(
            org,
            id,
            &NewEndpoint {
                environment: env,
                url: "https://hooks.example/lifecycle".to_owned(),
                description: "Lifecycle".to_owned(),
                events: vec![
                    WebhookEventType::InstallationConnected,
                    WebhookEventType::InstallationDisconnected,
                    WebhookEventType::ConnectSessionCompleted,
                    WebhookEventType::ConnectSessionFailed,
                ],
                secret: completion(InstallationId::generate(), 1).credential.sealed,
                secret_last4: "last".to_owned(),
            },
            Timestamp::now(),
            &by_suite,
        )
        .await
        .unwrap();
    id
}

async fn events(
    store: &dyn WebhookStore,
    org: OrganizationId,
    hook: WebhookEndpointId,
    env: Environment,
) -> Vec<Value> {
    let rows = store
        .list_deliveries(org, hook, env, &DeliveryFilter::default(), None, 100)
        .await
        .unwrap();
    let mut events = Vec::new();
    for row in rows.items {
        let detail = store
            .find_delivery_detail(org, hook, env, row.id)
            .await
            .unwrap()
            .unwrap();
        events.push(detail.event);
    }
    events
}

pub async fn run(
    connect: &dyn ConnectStore,
    installations: &dyn InstallationStore,
    webhooks: &dyn WebhookStore,
    audit: &dyn AuditStore,
    jobs: &dyn JobQueue,
    prepare: Prepare,
) {
    let a = tenant(&prepare).await;
    let b = tenant(&prepare).await;
    let hook = endpoint(webhooks, a.organization_id, Environment::Sandbox).await;
    let production = endpoint(webhooks, a.organization_id, Environment::Production).await;
    let foreign = endpoint(webhooks, b.organization_id, Environment::Sandbox).await;
    let now = Timestamp::now();
    let (session, _) = start(connect, &a, now).await;
    let created = completion(InstallationId::generate(), 1);
    let record = |_: &(
        zupflow_core::connect::ConnectSession,
        zupflow_core::installations::Installation,
    )| by_suite_in(a.sandbox);
    for (org, ws) in [
        (b.organization_id, a.sandbox),
        (a.organization_id, a.sibling),
        (a.organization_id, a.production),
    ] {
        assert_eq!(
            connect
                .complete_session(org, ws, session.id, &created, now, &record)
                .await,
            Err(ConnectError::NotFound)
        );
        assert_eq!(
            connect.cancel_session(org, ws, session.id, now).await,
            Err(ConnectError::NotFound)
        );
        assert_eq!(
            connect.fail_session(org, ws, session.id, "consent_denied", now).await,
            Err(ConnectError::NotFound)
        );
    }
    assert!(
        events(webhooks, a.organization_id, hook, Environment::Sandbox)
            .await
            .is_empty()
    );
    let (one, two) = tokio::join!(
        connect.complete_session(a.organization_id, a.sandbox, session.id, &created, now, &record),
        connect.complete_session(a.organization_id, a.sandbox, session.id, &created, now, &record),
    );
    assert_eq!(usize::from(one.is_ok()) + usize::from(two.is_ok()), 1);
    assert!(matches!(
        (one, two),
        (Ok(_), Err(ConnectError::NotPending)) | (Err(ConnectError::NotPending), Ok(_))
    ));
    let snapshots = events(webhooks, a.organization_id, hook, Environment::Sandbox).await;
    assert_eq!(snapshots.len(), 2);
    for event in &snapshots {
        assert_eq!(event["organization_id"], json!(a.organization_id));
        assert_eq!(event["workspace"], json!({ "id": a.sandbox, "external_id": null }));
        assert_eq!(event["environment"], "sandbox");
        assert_eq!(event["created_at"], json!(now));
    }
    let connected = snapshots
        .iter()
        .find(|event| event["type"] == "installation.connected")
        .unwrap();
    assert_eq!(
        connected["data"],
        json!({ "object": "installation", "id": created.installation_id,
        "app_id": a.catalog_app, "status": "connected" })
    );
    let completed = snapshots
        .iter()
        .find(|event| event["type"] == "connect_session.completed")
        .unwrap();
    assert_eq!(
        completed["data"],
        json!({ "object": "connect_session", "id": session.id,
        "status": "completed", "installation_id": created.installation_id, "failure_code": null })
    );
    for (org, ws) in [
        (b.organization_id, a.sandbox),
        (a.organization_id, a.sibling),
        (a.organization_id, a.production),
    ] {
        assert_eq!(
            installations
                .disconnect_installation(org, ws, created.installation_id, &|_| by_suite_in(ws))
                .await,
            Err(InstallationError::NotFound)
        );
        assert_eq!(
            installations
                .delete_installation(org, ws, created.installation_id, &|_| by_suite_in(ws))
                .await,
            Err(InstallationError::NotFound)
        );
    }
    let installation_audit = |_: &zupflow_core::installations::Installation| by_suite_in(a.sandbox);
    let (one, two) = tokio::join!(
        installations.disconnect_installation(
            a.organization_id,
            a.sandbox,
            created.installation_id,
            &installation_audit
        ),
        installations.disconnect_installation(
            a.organization_id,
            a.sandbox,
            created.installation_id,
            &installation_audit
        ),
    );
    assert_eq!(one.unwrap(), two.unwrap());
    assert_eq!(
        events(webhooks, a.organization_id, hook, Environment::Sandbox)
            .await
            .len(),
        3
    );
    let audit_before = audit
        .list_audit_events(a.organization_id, &AuditFilter::default(), None, 100)
        .await
        .unwrap();
    installations
        .disconnect_installation(
            a.organization_id,
            a.sandbox,
            created.installation_id,
            &installation_audit,
        )
        .await
        .unwrap();
    assert_eq!(
        audit
            .list_audit_events(a.organization_id, &AuditFilter::default(), None, 100)
            .await
            .unwrap(),
        audit_before
    );

    // Reconnect commits another pair, preserving the installation's identity.
    let reference = Reference::generate().unwrap();
    let mut reconnect = new(InstalledApp::Catalog(a.catalog_app), &reference);
    reconnect.intent = ConnectIntent::Reconnect;
    reconnect.installation_id = Some(created.installation_id);
    let renewed = connect
        .create_session(
            a.organization_id,
            a.sandbox,
            ConnectSessionId::generate(),
            &reconnect,
            now,
        )
        .await
        .unwrap();
    connect
        .complete_session(
            a.organization_id,
            a.sandbox,
            renewed.id,
            &completion(created.installation_id, 2),
            now,
            &record,
        )
        .await
        .unwrap();
    let cancelled = connect
        .create_session(
            a.organization_id,
            a.sandbox,
            ConnectSessionId::generate(),
            &{
                let mut session = reconnect.clone();
                session.reference_hash = Reference::generate().unwrap().hash();
                session
            },
            now,
        )
        .await
        .unwrap();
    let (one, two) = tokio::join!(
        connect.cancel_session(a.organization_id, a.sandbox, cancelled.id, now),
        connect.fail_session(a.organization_id, a.sandbox, cancelled.id, "consent_denied", now),
    );
    assert_eq!(usize::from(one.is_ok()) + usize::from(two.is_ok()), 1);
    let outcomes = events(webhooks, a.organization_id, hook, Environment::Sandbox).await;
    assert_eq!(outcomes.len(), 6);
    let failed = outcomes
        .iter()
        .find(|event| event["data"]["id"] == json!(cancelled.id))
        .unwrap();
    assert_eq!(failed["type"], "connect_session.failed");
    assert_eq!(
        failed["data"]["installation_id"],
        Value::Null,
        "a failed reconnect created no installation result"
    );
    installations
        .delete_installation(
            a.organization_id,
            a.sandbox,
            created.installation_id,
            &installation_audit,
        )
        .await
        .unwrap();
    assert!(
        installations
            .find_credential(a.organization_id, a.sandbox, created.installation_id)
            .await
            .unwrap()
            .is_none()
    );
    let after = events(webhooks, a.organization_id, hook, Environment::Sandbox).await;
    assert_eq!(after.len(), 7);
    for event in snapshots {
        assert!(
            after.contains(&event),
            "deleting the installation cannot change an event"
        );
    }
    assert!(
        events(webhooks, a.organization_id, production, Environment::Production)
            .await
            .is_empty()
    );
    assert!(
        events(webhooks, b.organization_id, foreign, Environment::Sandbox)
            .await
            .is_empty()
    );

    // Expiry is inclusive, bounded, and once-only across both isolation layers.
    let mut due = Vec::new();
    for (org, ws, app, env) in [
        (a.organization_id, a.sandbox, a.catalog_app, Environment::Sandbox),
        (a.organization_id, a.sibling, a.catalog_app, Environment::Sandbox),
        (a.organization_id, a.production, a.catalog_app, Environment::Production),
        (b.organization_id, b.sandbox, b.catalog_app, Environment::Sandbox),
    ] {
        let mut input = new(InstalledApp::Catalog(app), &Reference::generate().unwrap());
        input.environment = env;
        due.push(
            connect
                .create_session(org, ws, ConnectSessionId::generate(), &input, now)
                .await
                .unwrap(),
        );
    }
    let expiry = due[0].expires_at;
    assert_eq!(
        connect
            .expire_sessions_cross_tenant(Timestamp::from_millis(expiry.millis() - 1), 100)
            .await
            .unwrap(),
        0
    );
    let (one, two) = tokio::join!(
        connect.expire_sessions_cross_tenant(expiry, 1),
        connect.expire_sessions_cross_tenant(expiry, 1)
    );
    assert_eq!((one.unwrap(), two.unwrap()), (1, 1));
    assert_eq!(connect.expire_sessions_cross_tenant(expiry, 100).await.unwrap(), 2);
    assert_eq!(connect.expire_sessions_cross_tenant(expiry, 100).await.unwrap(), 0);
    for session in due {
        assert_eq!(
            connect
                .find_session(session.organization_id, session.workspace_id, session.id)
                .await
                .unwrap()
                .unwrap()
                .status,
            ConnectSessionStatus::Expired
        );
        assert_eq!(
            connect
                .complete_session(
                    session.organization_id,
                    session.workspace_id,
                    session.id,
                    &completion(InstallationId::generate(), 1),
                    expiry,
                    &record
                )
                .await,
            Err(ConnectError::NotPending)
        );
        let endpoint = if session.organization_id == b.organization_id {
            foreign
        } else if session.environment == Environment::Production {
            production
        } else {
            hook
        };
        let events = events(webhooks, session.organization_id, endpoint, session.environment).await;
        let found: Vec<_> = events
            .iter()
            .filter(|event| event["data"]["id"] == json!(session.id))
            .collect();
        assert_eq!(found.len(), 1);
        assert_eq!(found[0]["created_at"], json!(expiry));
        assert_eq!(found[0]["workspace"]["id"], json!(session.workspace_id));
        assert_eq!(
            found[0]["data"],
            json!({ "object": "connect_session", "id": session.id,
            "status": "expired", "installation_id": null, "failure_code": null })
        );
    }
    let (racing, _) = start(connect, &a, expiry).await;
    let racing_completion = completion(InstallationId::generate(), 1);
    let (complete, cancel) = tokio::join!(
        connect.complete_session(
            a.organization_id,
            a.sandbox,
            racing.id,
            &racing_completion,
            expiry,
            &record
        ),
        connect.cancel_session(a.organization_id, a.sandbox, racing.id, expiry),
    );
    assert_eq!(usize::from(complete.is_ok()) + usize::from(cancel.is_ok()), 1);
    let race_deliveries = if complete.is_ok() { 2 } else { 1 };
    assert_eq!(
        installations
            .find_installation(a.organization_id, a.sandbox, racing_completion.installation_id)
            .await
            .unwrap()
            .is_some(),
        complete.is_ok(),
        "a cancellation winner creates no installation"
    );
    let outcomes = events(webhooks, a.organization_id, hook, Environment::Sandbox).await;
    assert_eq!(
        outcomes
            .iter()
            .filter(|event| event["data"]["id"] == json!(racing.id))
            .count(),
        1
    );
    // Each persisted delivery has exactly one scoped job; no credential or reference is queued.
    let kind = Name::new(DELIVER).unwrap();
    let mut delivered = HashSet::new();
    while let Some(job) = jobs
        .claim_next(std::slice::from_ref(&kind), Duration::from_secs(60))
        .await
        .unwrap()
    {
        let Scope::Organization(org) = job.scope else {
            panic!("a delivery belongs to its organization")
        };
        let id = delivery_of(&job.payload).unwrap();
        assert!(delivered.insert(id));
        assert!(webhooks.find_delivery(org, id).await.unwrap().is_some());
        assert!(
            webhooks
                .find_delivery(OrganizationId::generate(), id)
                .await
                .unwrap()
                .is_none()
        );
        jobs.complete(&job.scope, job.id, &job.lease).await.unwrap();
    }
    assert_eq!(delivered.len(), 11 + race_deliveries);
}
