#![allow(clippy::unwrap_used)]

use serde_json::json;
use sqlx::sqlite::{SqliteConnectOptions, SqlitePoolOptions};
use zupflow_core::audit::{AuditAction, AuditEntry, AuditObject, AuditPlace, AuditTarget};
use zupflow_core::ids::{ApiKeyId, MemberId, UserId};
use zupflow_core::webhooks::event_body;

use super::*;
use crate::schema::{Temporary, TemporaryDirectory, temporary};

pub(super) async fn manager(database: &Database, org: OrganizationId) -> (MemberId, ApiKeyId) {
    let user = UserId::generate();
    let member = MemberId::generate();
    let key = ApiKeyId::generate();
    sqlx::query("INSERT INTO users (id, name, email, created_at) VALUES (?, 'Jordan', ?, 0)")
        .bind(user.uuid())
        .bind(format!("{}@northwind.example", user.uuid().simple()))
        .execute(database.pool())
        .await
        .unwrap();
    sqlx::query("INSERT INTO members (id, organization_id, user_id, role, status, invited_at, joined_at) VALUES (?, ?, ?, 'admin', 'active', 0, 0)")
        .bind(member.uuid()).bind(org.uuid()).bind(user.uuid()).execute(database.pool()).await.unwrap();
    sqlx::query("INSERT INTO api_keys (id, organization_id, environment, name, prefix, last4, secret_hash, created_by_user_id, created_at) VALUES (?, ?, 'sandbox', 'Test', 'test', 'abcd', ?, ?, 0)")
        .bind(key.uuid()).bind(org.uuid()).bind(key.uuid().as_bytes().repeat(2)).bind(user.uuid()).execute(database.pool()).await.unwrap();
    (member, key)
}

fn audit(summary: &DeliverySummary) -> AuditEntry {
    AuditEntry {
        action: AuditAction::WebhookDeliveryResent,
        target: AuditTarget::new(AuditObject::WebhookDelivery(summary.id), &summary.event_type),
        place: summary
            .workspace
            .as_ref()
            .map_or(AuditPlace::Environment(summary.environment), |workspace| {
                AuditPlace::Workspace(workspace.id)
            }),
        ..zupflow_conformance::audit_store::by_suite(&())
    }
}

async fn counts(database: &Database, org: OrganizationId) -> (i64, i64, i64) {
    sqlx::query_as("SELECT (SELECT count(*) FROM webhook_deliveries WHERE organization_id = ?), (SELECT count(*) FROM jobs WHERE organization_id = ?), (SELECT count(*) FROM audit_events WHERE organization_id = ?)")
        .bind(org.uuid()).bind(org.uuid()).bind(org.uuid()).fetch_one(database.pool()).await.unwrap()
}

#[tokio::test]
async fn resend_rechecks_current_authority_and_rolls_back_its_job_and_audit() {
    let Temporary {
        database,
        directory: _directory,
    } = temporary().await;
    let (org, _, hook, source) = history_tests::recorded(&database).await;
    let (member, key) = manager(&database, org).await;
    let store = SqliteWebhookStore::new(database.clone());
    store
        .record_attempt(org, source, &history_tests::success(1))
        .await
        .unwrap();
    let before = counts(&database, org).await;
    let resend = |manager| store.resend_delivery(org, hook, Environment::Sandbox, source, manager, &audit);
    for role in ["developer", "viewer"] {
        sqlx::query("UPDATE members SET role = ? WHERE organization_id = ? AND id = ?")
            .bind(role)
            .bind(org.uuid())
            .bind(member.uuid())
            .execute(database.pool())
            .await
            .unwrap();
        assert_eq!(
            resend(WebhookManager::Member(member)).await.unwrap_err(),
            ResendError::NotAllowed
        );
    }
    sqlx::query(
        "UPDATE members SET role = 'admin', status = 'invited', joined_at = NULL WHERE organization_id = ? AND id = ?",
    )
    .bind(org.uuid())
    .bind(member.uuid())
    .execute(database.pool())
    .await
    .unwrap();
    assert_eq!(
        resend(WebhookManager::Member(member)).await.unwrap_err(),
        ResendError::NotAllowed
    );
    sqlx::query("UPDATE members SET status = 'active', joined_at = 0 WHERE organization_id = ? AND id = ?")
        .bind(org.uuid())
        .bind(member.uuid())
        .execute(database.pool())
        .await
        .unwrap();
    sqlx::query("UPDATE api_keys SET revoked_at = 1 WHERE organization_id = ? AND id = ?")
        .bind(org.uuid())
        .bind(key.uuid())
        .execute(database.pool())
        .await
        .unwrap();
    assert_eq!(
        resend(WebhookManager::ApiKey(key)).await.unwrap_err(),
        ResendError::NotAllowed
    );
    assert_eq!(counts(&database, org).await, before);
    let mut schema = database.pool().acquire().await.unwrap();
    for (table, trigger) in [
        (
            "jobs",
            "CREATE TRIGGER refuse_resend BEFORE INSERT ON jobs BEGIN SELECT RAISE(ABORT, 'fixture refusal'); END",
        ),
        (
            "audit_events",
            "CREATE TRIGGER refuse_resend BEFORE INSERT ON audit_events BEGIN SELECT RAISE(ABORT, 'fixture refusal'); END",
        ),
    ] {
        sqlx::query(trigger).execute(&mut *schema).await.unwrap();
        assert_eq!(
            resend(WebhookManager::Member(member)).await.unwrap_err(),
            ResendError::Unavailable
        );
        assert_eq!(
            counts(&database, org).await,
            before,
            "{table} failure rolls back delivery, job and audit together"
        );
        sqlx::query("DROP TRIGGER refuse_resend")
            .execute(&mut *schema)
            .await
            .unwrap();
    }
    let summary = resend(WebhookManager::Member(member)).await.unwrap();
    assert_eq!(counts(&database, org).await, (before.0 + 1, before.1 + 1, before.2 + 1));
    let target: (String, uuid::Uuid) = sqlx::query_as("SELECT target_type, target_id FROM audit_events WHERE organization_id = ? AND action = 'webhook.delivery_resent'")
        .bind(org.uuid()).fetch_one(database.pool()).await.unwrap();
    assert_eq!(target, ("webhook_delivery".to_owned(), summary.id.uuid()));
    sqlx::query("UPDATE webhook_events SET body = '{}' WHERE organization_id = ? AND id = ?")
        .bind(org.uuid())
        .bind(summary.event_id.uuid())
        .execute(database.pool())
        .await
        .unwrap();
    let committed = counts(&database, org).await;
    assert_eq!(
        resend(WebhookManager::Member(member)).await.unwrap_err(),
        ResendError::Unavailable
    );
    assert_eq!(
        counts(&database, org).await,
        committed,
        "corrupt metadata never creates a resend"
    );
}

const DELIVERY_SNAPSHOT: &str = "SELECT json_object('id', hex(id), 'org', hex(organization_id), 'endpoint', hex(endpoint_id), 'event', hex(event_id), 'state', state, 'attempts', attempts, 'status', last_status_code, 'error', last_error, 'last', last_attempt_at, 'next', next_attempt_at, 'created', created_at) FROM webhook_deliveries WHERE organization_id = ? ORDER BY id";
const ATTEMPT_SNAPSHOT: &str = "SELECT json_object('id', hex(id), 'org', hex(organization_id), 'delivery', hex(delivery_id), 'number', number, 'status', status_code, 'error', error_class, 'at', attempted_at, 'duration', duration_ms) FROM webhook_delivery_attempts WHERE organization_id = ? ORDER BY id";
const JOB_SNAPSHOT: &str = "SELECT json_object('id', hex(id), 'org', hex(organization_id), 'workspace', hex(workspace_id), 'environment', environment, 'kind', kind, 'payload', hex(payload), 'state', state, 'attempts', attempts, 'max_attempts', max_attempts, 'run_at', run_at, 'lease', hex(lease_token), 'expires', lease_expires_at, 'reason', reason, 'created', created_at) FROM jobs WHERE organization_id = ? ORDER BY id";

async fn snapshot(pool: &sqlx::SqlitePool, org: OrganizationId, query: &'static str) -> Vec<String> {
    sqlx::query_scalar(query)
        .bind(org.uuid())
        .fetch_all(pool)
        .await
        .unwrap()
}

#[tokio::test]
async fn upgrading_v23_preserves_deliveries_attempts_and_leased_jobs() {
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
    migrator.run_to(23, &pool).await.unwrap();
    let org = OrganizationId::generate();
    let hook = WebhookEndpointId::generate();
    sqlx::query("INSERT INTO organizations (id, name, slug, created_at) VALUES (?, 'Northwind', 'northwind', 0)")
        .bind(org.uuid())
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query("INSERT INTO webhook_endpoints (id, organization_id, environment, url, description, events, enabled, secret_version, secret_key_id, secret_wrapped_key, secret_nonce, secret_ciphertext, secret_last4, created_at) VALUES (?, ?, 'sandbox', 'https://northwind.example/webhooks', '', '[\"run.completed\"]', 1, 1, 'local', X'00', zeroblob(12), X'00', 'abcd', 0)")
        .bind(hook.uuid()).bind(org.uuid()).execute(&pool).await.unwrap();
    let mut deliveries = Vec::new();
    for (state, attempts, status, error, next) in [
        ("pending", 2, Some(503_i64), Some("status"), Some(200_i64)),
        ("failed", 1, Some(503), Some("status"), None),
        ("succeeded", 1, Some(204), None, None),
        ("skipped", 0, None, None, None),
    ] {
        let event = WebhookEventId::generate();
        let delivery = WebhookDeliveryId::generate();
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
        sqlx::query("INSERT INTO webhook_deliveries (id, organization_id, endpoint_id, event_id, state, attempts, last_status_code, last_error, last_attempt_at, next_attempt_at, created_at) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, 0)")
            .bind(delivery.uuid()).bind(org.uuid()).bind(hook.uuid()).bind(event.uuid()).bind(state).bind(attempts).bind(status).bind(error).bind(status.map(|_| 100_i64)).bind(next).execute(&pool).await.unwrap();
        for number in 1..=attempts {
            sqlx::query("INSERT INTO webhook_delivery_attempts (id, organization_id, delivery_id, number, status_code, error_class, attempted_at, duration_ms) VALUES (?, ?, ?, ?, ?, ?, 100, ?)")
                .bind(uuid::Uuid::now_v7()).bind(org.uuid()).bind(delivery.uuid()).bind(number).bind(status).bind(error).bind((number == 2).then_some(42_i64)).execute(&pool).await.unwrap();
        }
        deliveries.push(delivery);
    }
    let job = delivery_job(deliveries[0]).unwrap();
    sqlx::query("INSERT INTO jobs (id, organization_id, kind, payload, state, attempts, max_attempts, run_at, lease_token, lease_expires_at, created_at) VALUES (?, ?, ?, ?, 'running', 2, 20, 200, ?, 10000, 0)")
        .bind(uuid::Uuid::now_v7()).bind(org.uuid()).bind(job.kind.as_str()).bind(job.payload).bind(vec![7_u8; 16]).execute(&pool).await.unwrap();
    let before = [
        snapshot(&pool, org, DELIVERY_SNAPSHOT).await,
        snapshot(&pool, org, ATTEMPT_SNAPSHOT).await,
        snapshot(&pool, org, JOB_SNAPSHOT).await,
    ];
    pool.close().await;

    let database = Database::open(&directory.file()).await.unwrap();
    for (query, before) in [DELIVERY_SNAPSHOT, ATTEMPT_SNAPSHOT, JOB_SNAPSHOT]
        .into_iter()
        .zip(before)
    {
        assert_eq!(
            snapshot(database.pool(), org, query).await,
            before,
            "migration preserves every stored field"
        );
    }
    let violations = sqlx::query("PRAGMA foreign_key_check")
        .fetch_all(database.pool())
        .await
        .unwrap();
    assert!(
        violations.is_empty(),
        "rebuilt parents and children still reference each other"
    );
    let store = SqliteWebhookStore::new(database.clone());
    for (delivery, expected) in deliveries.iter().zip([2, 1, 1, 0]) {
        let detail = store
            .find_delivery_detail(org, hook, Environment::Sandbox, *delivery)
            .await
            .unwrap()
            .unwrap();
        assert!(detail.attempt_history_complete);
        assert_eq!(detail.summary.attempts, expected);
        assert_eq!(
            store
                .list_delivery_attempts(org, hook, Environment::Sandbox, *delivery, None, 25)
                .await
                .unwrap()
                .total,
            u64::from(expected)
        );
    }
    let (member, _) = manager(&database, org).await;
    let resent = store
        .resend_delivery(
            org,
            hook,
            Environment::Sandbox,
            deliveries[1],
            WebhookManager::Member(member),
            &audit,
        )
        .await
        .unwrap();
    let source: uuid::Uuid =
        sqlx::query_scalar("SELECT resend_of_delivery_id FROM webhook_deliveries WHERE organization_id = ? AND id = ?")
            .bind(org.uuid())
            .bind(resent.id.uuid())
            .fetch_one(database.pool())
            .await
            .unwrap();
    assert_eq!(source, deliveries[1].uuid());
    assert!(sqlx::query("INSERT INTO webhook_deliveries (id, organization_id, endpoint_id, event_id, state, attempts, created_at) VALUES (?, ?, ?, ?, 'skipped', 0, 0)")
        .bind(WebhookDeliveryId::generate().uuid()).bind(org.uuid()).bind(hook.uuid()).bind(resent.event_id.uuid()).execute(database.pool()).await.is_err(), "original delivery uniqueness still holds");
    let (other_org, _, _, other_delivery) = history_tests::recorded(&database).await;
    assert_ne!(other_org, org);
    assert!(
        sqlx::query("UPDATE webhook_deliveries SET resend_of_delivery_id = ? WHERE organization_id = ? AND id = ?")
            .bind(other_delivery.uuid())
            .bind(org.uuid())
            .bind(resent.id.uuid())
            .execute(database.pool())
            .await
            .is_err(),
        "a resend cannot refer to another organization's source"
    );
    store
        .delete_endpoint(org, hook, &zupflow_conformance::audit_store::by_suite)
        .await
        .unwrap();
    assert_eq!(
        snapshot(database.pool(), org, ATTEMPT_SNAPSHOT).await,
        Vec::<String>::new(),
        "deletion still cascades to old and new attempt children"
    );
    database.pool().close().await;
}
