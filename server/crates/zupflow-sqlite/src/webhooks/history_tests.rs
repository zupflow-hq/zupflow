#![allow(clippy::unwrap_used)]

use serde_json::json;
use sqlx::sqlite::{SqliteConnectOptions, SqlitePoolOptions};
use zupflow_conformance::audit_store::by_suite;
use zupflow_core::ids::{AgentId, RunId, WorkspaceId};
use zupflow_core::webhooks::event_body;

use super::*;
use crate::schema::{Temporary, TemporaryDirectory, temporary};

async fn organization(database: &Database, id: OrganizationId) {
    sqlx::query("INSERT INTO organizations (id, name, slug, created_at) VALUES (?, 'Northwind', ?, 0)")
        .bind(id.uuid())
        .bind(format!("org-{}", id.uuid().simple()))
        .execute(database.pool())
        .await
        .unwrap();
}

pub(super) async fn recorded(
    database: &Database,
) -> (OrganizationId, WorkspaceId, WebhookEndpointId, WebhookDeliveryId) {
    let org = OrganizationId::generate();
    organization(database, org).await;
    let ws = WorkspaceId::generate();
    sqlx::query("INSERT INTO workspaces (id, organization_id, environment, name, slug, external_id, created_at) VALUES (?, ?, 'sandbox', 'Acme', 'acme', 'original', 0)")
        .bind(ws.uuid()).bind(org.uuid()).execute(database.pool()).await.unwrap();
    let hook = WebhookEndpointId::generate();
    let store = SqliteWebhookStore::new(database.clone());
    store
        .create_endpoint(
            org,
            hook,
            &NewEndpoint {
                environment: Environment::Sandbox,
                url: "https://northwind.example/webhooks".to_owned(),
                description: String::new(),
                events: vec![WebhookEventType::RunCompleted],
                secret: Sealed {
                    wrapped_key: WrappedKey {
                        key_id: "local".to_owned(),
                        bytes: vec![1; 40],
                    },
                    nonce: [0; 12],
                    ciphertext: vec![2; 48],
                },
                secret_last4: "abcd".to_owned(),
            },
            Timestamp::from_millis(0),
            &by_suite,
        )
        .await
        .unwrap();
    let id = WebhookEventId::generate();
    let event = NewWebhookEvent {
        id,
        environment: Environment::Sandbox,
        workspace_id: Some(ws),
        kind: "run.completed".to_owned(),
        body: event_body(
            id,
            "run.completed",
            Timestamp::from_millis(0),
            Environment::Sandbox,
            org,
            Some((ws, Some("original"))),
            json!({ "object": "run", "id": RunId::generate(), "agent_id": AgentId::generate(), "status": "completed" }),
        ),
        created_at: Timestamp::from_millis(0),
    };
    let delivery = store.record_event(org, &event).await.unwrap()[0];
    (org, ws, hook, delivery)
}

pub(super) fn success(number: u32) -> Attempt {
    Attempt {
        number,
        status: Some(204),
        error: None,
        at: Timestamp::from_millis(200),
        duration_ms: 6,
        state: DeliveryState::Succeeded,
        next_attempt_at: None,
    }
}

#[tokio::test]
async fn attempt_failure_rolls_back_summary_and_endpoint_and_foreign_keys_carry_scope() {
    let Temporary {
        database,
        directory: _directory,
    } = temporary().await;
    let (org, _, hook, delivery) = recorded(&database).await;
    let store = SqliteWebhookStore::new(database.clone());
    sqlx::query("CREATE TRIGGER refuse_attempt BEFORE INSERT ON webhook_delivery_attempts BEGIN SELECT RAISE(ABORT, 'fixture refusal'); END")
        .execute(database.pool()).await.unwrap();
    assert_eq!(
        store.record_attempt(org, delivery, &success(1)).await.unwrap_err(),
        WebhookError::Unavailable
    );
    let unchanged = store
        .find_delivery_detail(org, hook, Environment::Sandbox, delivery)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(unchanged.summary.state, DeliveryState::Pending);
    assert_eq!(unchanged.summary.attempts, 0);
    assert!(unchanged.attempt_history_complete);
    assert_eq!(
        store.find_endpoint(org, hook).await.unwrap().unwrap().last_delivery,
        None
    );
    sqlx::query("DROP TRIGGER refuse_attempt")
        .execute(database.pool())
        .await
        .unwrap();
    let foreign = OrganizationId::generate();
    organization(&database, foreign).await;
    assert!(sqlx::query("INSERT INTO webhook_delivery_attempts (id, organization_id, delivery_id, number, attempted_at) VALUES (?, ?, ?, 1, 0)")
        .bind(uuid::Uuid::now_v7()).bind(foreign.uuid()).bind(delivery.uuid()).execute(database.pool()).await.is_err(), "a foreign delivery cannot be referenced even when both IDs exist");
    store.record_attempt(org, delivery, &success(1)).await.unwrap();
    let before = store
        .list_delivery_attempts(org, hook, Environment::Sandbox, delivery, None, 25)
        .await
        .unwrap();
    assert_eq!(before.total, 1);
    store.delete_endpoint(org, hook, &by_suite).await.unwrap();
    let count: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM webhook_delivery_attempts WHERE organization_id = ? AND delivery_id = ?",
    )
    .bind(org.uuid())
    .bind(delivery.uuid())
    .fetch_one(database.pool())
    .await
    .unwrap();
    assert_eq!(count, 0, "deletion cascades the attempt log too");
}

#[tokio::test]
async fn history_keeps_snapshot_attribution_and_refuses_corrupt_event_envelopes() {
    let Temporary {
        database,
        directory: _directory,
    } = temporary().await;
    let (org, ws, hook, delivery) = recorded(&database).await;
    let store = SqliteWebhookStore::new(database.clone());
    sqlx::query("UPDATE workspaces SET external_id = 'renamed' WHERE organization_id = ? AND id = ?")
        .bind(org.uuid())
        .bind(ws.uuid())
        .execute(database.pool())
        .await
        .unwrap();
    let detail = store
        .find_delivery_detail(org, hook, Environment::Sandbox, delivery)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        detail.summary.workspace.as_ref().unwrap().external_id.as_deref(),
        Some("original")
    );
    assert_eq!(detail.event["workspace"]["external_id"], "original");
    let event_id = detail.summary.event_id;
    for (key, wrong) in [
        ("id", json!(WebhookEventId::generate())),
        ("organization_id", json!(OrganizationId::generate())),
        ("environment", json!("production")),
        ("type", json!("run.failed")),
        ("created_at", json!(Timestamp::from_millis(1))),
        (
            "workspace",
            json!({ "id": WorkspaceId::generate(), "external_id": "foreign" }),
        ),
    ] {
        let mut bad = detail.event.clone();
        bad[key] = wrong;
        sqlx::query("UPDATE webhook_events SET body = ? WHERE organization_id = ? AND id = ?")
            .bind(bad.to_string())
            .bind(org.uuid())
            .bind(event_id.uuid())
            .execute(database.pool())
            .await
            .unwrap();
        assert_eq!(
            store
                .find_delivery_detail(org, hook, Environment::Sandbox, delivery)
                .await
                .unwrap_err(),
            WebhookError::Unavailable,
            "contradictory {key}"
        );
        assert_eq!(
            store.find_delivery(org, delivery).await.unwrap_err(),
            WebhookError::Unavailable,
            "a worker cannot sign contradictory {key}"
        );
        assert_eq!(
            store
                .list_deliveries(org, hook, Environment::Sandbox, &DeliveryFilter::default(), None, 25)
                .await
                .unwrap_err(),
            WebhookError::Unavailable
        );
        assert_eq!(
            store
                .list_delivery_attempts(org, hook, Environment::Sandbox, delivery, None, 25)
                .await
                .unwrap_err(),
            WebhookError::Unavailable
        );
    }
    sqlx::query("UPDATE webhook_events SET body = ?, environment = 'production' WHERE organization_id = ? AND id = ?")
        .bind(detail.event.to_string())
        .bind(org.uuid())
        .bind(event_id.uuid())
        .execute(database.pool())
        .await
        .unwrap();
    assert!(
        store
            .find_delivery_detail(org, hook, Environment::Sandbox, delivery)
            .await
            .unwrap()
            .is_none(),
        "a row outside the environment is hidden before reading JSON"
    );
    let hidden = store
        .list_deliveries(org, hook, Environment::Sandbox, &DeliveryFilter::default(), None, 25)
        .await
        .unwrap();
    assert_eq!(hidden.total, 0);
    assert!(hidden.items.is_empty());
    assert!(store.find_delivery(org, delivery).await.unwrap().is_none());
    sqlx::query("UPDATE webhook_endpoints SET environment = 'production' WHERE organization_id = ? AND id = ?")
        .bind(org.uuid())
        .bind(hook.uuid())
        .execute(database.pool())
        .await
        .unwrap();
    assert!(
        store.find_delivery(org, delivery).await.unwrap().is_none(),
        "matching endpoint and event still cannot reach a workspace from another environment"
    );
}

#[tokio::test]
async fn upgrading_a_summary_only_delivery_does_not_invent_its_history() {
    let directory = TemporaryDirectory::new();
    let pool = SqlitePoolOptions::new()
        .max_connections(1)
        .connect_with(
            SqliteConnectOptions::new()
                .filename(directory.file())
                .create_if_missing(true)
                .foreign_keys(true),
        )
        .await
        .unwrap();
    let migrator = sqlx::migrate!("./migrations");
    migrator.run_to(22, &pool).await.unwrap();
    let org = OrganizationId::generate();
    let hook = WebhookEndpointId::generate();
    let event = WebhookEventId::generate();
    let delivery = WebhookDeliveryId::generate();
    sqlx::query("INSERT INTO organizations (id, name, slug, created_at) VALUES (?, 'Northwind', 'northwind', 0)")
        .bind(org.uuid())
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query("INSERT INTO webhook_endpoints (id, organization_id, environment, url, description, events, enabled, secret_version, secret_key_id, secret_wrapped_key, secret_nonce, secret_ciphertext, secret_last4, created_at) VALUES (?, ?, 'sandbox', 'https://northwind.example/webhooks', '', '[\"run.completed\"]', 1, 1, 'local', X'00', zeroblob(12), X'00', 'abcd', 0)")
        .bind(hook.uuid()).bind(org.uuid()).execute(&pool).await.unwrap();
    let body = event_body(
        event,
        TEST_EVENT,
        Timestamp::from_millis(0),
        Environment::Sandbox,
        org,
        None,
        json!({ "object": "webhook_endpoint", "id": hook }),
    );
    sqlx::query("INSERT INTO webhook_events (id, organization_id, environment, type, body, created_at) VALUES (?, ?, 'sandbox', 'webhook.test', ?, 0)")
        .bind(event.uuid()).bind(org.uuid()).bind(&body).execute(&pool).await.unwrap();
    sqlx::query("INSERT INTO webhook_deliveries (id, organization_id, endpoint_id, event_id, state, attempts, last_status_code, last_error, last_attempt_at, next_attempt_at, created_at) VALUES (?, ?, ?, ?, 'pending', 2, 503, 'status', 100, 200, 0)")
        .bind(delivery.uuid()).bind(org.uuid()).bind(hook.uuid()).bind(event.uuid()).execute(&pool).await.unwrap();
    pool.close().await;

    let database = Database::open(&directory.file()).await.unwrap();
    let store = SqliteWebhookStore::new(database.clone());
    let old = store
        .find_delivery_detail(org, hook, Environment::Sandbox, delivery)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(old.summary.attempts, 2);
    assert!(!old.attempt_history_complete);
    let empty = store
        .list_delivery_attempts(org, hook, Environment::Sandbox, delivery, None, 25)
        .await
        .unwrap();
    assert_eq!(empty.total, 0);
    assert!(empty.items.is_empty());
    store.record_attempt(org, delivery, &success(3)).await.unwrap();
    let changed = store
        .find_delivery_detail(org, hook, Environment::Sandbox, delivery)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(changed.summary.attempts, 3);
    assert!(!changed.attempt_history_complete, "new attempts don't fill older gaps");
    let log = store
        .list_delivery_attempts(org, hook, Environment::Sandbox, delivery, None, 25)
        .await
        .unwrap();
    assert_eq!(log.total, 1);
    assert_eq!(log.items[0].number, 3);
    assert_eq!(log.items[0].duration_ms, Some(6));
    sqlx::query(
        "UPDATE webhook_delivery_attempts SET duration_ms = NULL WHERE organization_id = ? AND delivery_id = ?",
    )
    .bind(org.uuid())
    .bind(delivery.uuid())
    .execute(database.pool())
    .await
    .unwrap();
    assert_eq!(
        store
            .list_delivery_attempts(org, hook, Environment::Sandbox, delivery, None, 25)
            .await
            .unwrap()
            .items[0]
            .duration_ms,
        None
    );
    database.pool().close().await;
}
