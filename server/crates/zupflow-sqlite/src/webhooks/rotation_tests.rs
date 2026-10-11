#![allow(clippy::unwrap_used)]

use sqlx::sqlite::{SqliteConnectOptions, SqlitePoolOptions};
use zupflow_core::audit::{AuditAction, AuditEntry, AuditObject, AuditPlace, AuditTarget};

use super::*;
use crate::schema::{Temporary, TemporaryDirectory, temporary};

fn audit(endpoint: &WebhookEndpoint) -> AuditEntry {
    AuditEntry {
        action: AuditAction::WebhookSecretRolled,
        target: AuditTarget::new(AuditObject::Webhook(endpoint.id), &endpoint.url),
        place: AuditPlace::Environment(endpoint.environment),
        ..zupflow_conformance::audit_store::by_suite(&())
    }
}

#[tokio::test]
async fn rotation_rechecks_authority_and_commits_exactly_one_audit_with_both_versions() {
    let Temporary {
        database,
        directory: _directory,
    } = temporary().await;
    let (org, _, hook, _) = history_tests::recorded(&database).await;
    let (member, key) = resend_tests::manager(&database, org).await;
    let store = SqliteWebhookStore::new(database.clone());
    let now = Timestamp::now();
    let before = store
        .signing_secrets(org, hook, Environment::Sandbox, now)
        .await
        .unwrap()
        .unwrap();
    let next = SecretRotation {
        expected_version: 1,
        sealed: before.current.sealed.clone(),
        last4: "next".to_owned(),
        at: now,
    };
    let roll = |manager| store.roll_secret(org, hook, Environment::Sandbox, manager, &next, &audit);
    for role in ["developer", "viewer"] {
        sqlx::query("UPDATE members SET role = ? WHERE organization_id = ? AND id = ?")
            .bind(role)
            .bind(org.uuid())
            .bind(member.uuid())
            .execute(database.pool())
            .await
            .unwrap();
        assert_eq!(
            roll(WebhookManager::Member(member)).await.unwrap_err(),
            RotationError::NotAllowed
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
        roll(WebhookManager::Member(member)).await.unwrap_err(),
        RotationError::NotAllowed
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
        roll(WebhookManager::ApiKey(key)).await.unwrap_err(),
        RotationError::NotAllowed
    );
    let mut schema = database.pool().acquire().await.unwrap();
    sqlx::query("CREATE TRIGGER refuse_rotation BEFORE INSERT ON audit_events BEGIN SELECT RAISE(ABORT, 'fixture refusal'); END")
        .execute(&mut *schema).await.unwrap();
    assert_eq!(
        roll(WebhookManager::Member(member)).await.unwrap_err(),
        RotationError::Unavailable
    );
    assert_eq!(
        store
            .signing_secrets(org, hook, Environment::Sandbox, now)
            .await
            .unwrap()
            .unwrap(),
        before
    );
    sqlx::query("DROP TRIGGER refuse_rotation")
        .execute(&mut *schema)
        .await
        .unwrap();
    let (one, two) = tokio::join!(
        roll(WebhookManager::Member(member)),
        roll(WebhookManager::Member(member))
    );
    assert!(matches!(
        (&one, &two),
        (Ok(_), Err(RotationError::InProgress)) | (Err(RotationError::InProgress), Ok(_))
    ));
    let records: Vec<(String, uuid::Uuid, Option<uuid::Uuid>, Option<String>)> = sqlx::query_as(
        "SELECT target_type, target_id, workspace_id, environment FROM audit_events WHERE organization_id = ? AND action = 'webhook.secret_rolled'",
    ).bind(org.uuid()).fetch_all(database.pool()).await.unwrap();
    assert_eq!(
        records,
        vec![("webhook".to_owned(), hook.uuid(), None, Some("sandbox".to_owned()))]
    );
    let latest = store
        .signing_secrets(org, hook, Environment::Sandbox, now)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(latest.current.version, 2);
    assert_eq!(latest.previous.unwrap().secret, before.current);
    // Projection never advertises an expired overlap, even before another roll.
    sqlx::query("UPDATE webhook_endpoints SET previous_secret_expires_at = 0 WHERE organization_id = ? AND id = ? AND environment = 'sandbox'")
        .bind(org.uuid()).bind(hook.uuid()).execute(database.pool()).await.unwrap();
    assert_eq!(
        store
            .find_endpoint(org, hook)
            .await
            .unwrap()
            .unwrap()
            .previous_signing_secret_expires_at,
        None
    );
    assert!(
        store
            .list_endpoints(org, Some(Environment::Sandbox))
            .await
            .unwrap()
            .iter()
            .all(|endpoint| endpoint.previous_signing_secret_expires_at.is_none())
    );
}

#[tokio::test]
async fn upgrading_v24_preserves_current_secret_and_endpoint_state() {
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
    migrator.run_to(24, &pool).await.unwrap();
    let org = OrganizationId::generate();
    let hook = WebhookEndpointId::generate();
    sqlx::query("INSERT INTO organizations (id, name, slug, created_at) VALUES (?, 'Northwind', 'northwind', 0)")
        .bind(org.uuid())
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query("INSERT INTO webhook_endpoints (id, organization_id, environment, url, description, events, enabled, secret_version, secret_key_id, secret_wrapped_key, secret_nonce, secret_ciphertext, secret_last4, created_at, last_status_code, last_delivered_at, failing_since, delivery_holder, delivery_held_until) VALUES (?, ?, 'production', 'https://hooks.example/in', 'Legacy endpoint', '[\"run.completed\"]', 0, 1, 'local', X'123456', zeroblob(12), X'789abc', 'abcd', 10, 503, 20, 30, zeroblob(16), 40)")
        .bind(hook.uuid()).bind(org.uuid()).execute(&pool).await.unwrap();
    let snapshot = || {
        sqlx::query_scalar::<_, String>("SELECT json_object('id', hex(id), 'org', hex(organization_id), 'env', environment, 'url', url, 'description', description, 'events', events, 'enabled', enabled, 'version', secret_version, 'key', secret_key_id, 'wrapped', hex(secret_wrapped_key), 'nonce', hex(secret_nonce), 'ciphertext', hex(secret_ciphertext), 'last4', secret_last4, 'created', created_at, 'status', last_status_code, 'last', last_delivered_at, 'failing', failing_since, 'holder', hex(delivery_holder), 'until', delivery_held_until) FROM webhook_endpoints WHERE organization_id = ? AND id = ?")
        .bind(org.uuid()).bind(hook.uuid()).fetch_one(&pool)
    };
    let before = snapshot().await.unwrap();
    migrator.run(&pool).await.unwrap();
    assert_eq!(snapshot().await.unwrap(), before);
    let no_overlap: bool = sqlx::query_scalar(
        "SELECT previous_secret_version IS NULL AND previous_secret_key_id IS NULL \
         AND previous_secret_wrapped_key IS NULL AND previous_secret_nonce IS NULL \
         AND previous_secret_ciphertext IS NULL AND previous_secret_expires_at IS NULL \
         FROM webhook_endpoints WHERE organization_id = ? AND id = ?",
    )
    .bind(org.uuid())
    .bind(hook.uuid())
    .fetch_one(&pool)
    .await
    .unwrap();
    assert!(no_overlap);
    for update in [
        "UPDATE webhook_endpoints SET previous_secret_expires_at = 99 WHERE organization_id = ? AND id = ?",
        "UPDATE webhook_endpoints SET previous_secret_version = secret_version, previous_secret_key_id = secret_key_id, previous_secret_wrapped_key = secret_wrapped_key, previous_secret_nonce = secret_nonce, previous_secret_ciphertext = secret_ciphertext, previous_secret_expires_at = 99 WHERE organization_id = ? AND id = ?",
        "UPDATE webhook_endpoints SET secret_version = 2, previous_secret_version = 1, previous_secret_key_id = secret_key_id, previous_secret_wrapped_key = secret_wrapped_key, previous_secret_nonce = X'00', previous_secret_ciphertext = secret_ciphertext, previous_secret_expires_at = 99 WHERE organization_id = ? AND id = ?",
    ] {
        assert!(
            sqlx::query(update)
                .bind(org.uuid())
                .bind(hook.uuid())
                .execute(&pool)
                .await
                .is_err(),
            "partial or contradictory pairs are refused"
        );
    }
    assert!(
        sqlx::query("PRAGMA foreign_key_check")
            .fetch_all(&pool)
            .await
            .unwrap()
            .is_empty()
    );
    pool.close().await;
}
