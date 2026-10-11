use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};

use async_trait::async_trait;
use zupflow_core::environment::Environment;
use zupflow_core::ids::{ApiKeyId, MemberId, OrganizationId, WebhookEndpointId};
use zupflow_core::keys::{DataKey, EncryptionContext, KeyError, KeyService, WrappedKey};
use zupflow_server::RequestGuard;

use super::*;

#[tokio::test]
async fn rotation_key_failures_send_no_http_and_charge_no_attempt() {
    let server = Server::new().await;
    let org = server.organization("alice", "northwind").await;
    let ws = server.workspace("alice", &org, "sandbox", "sandbox").await;
    let ws = id(&ws);
    let helper = agent(&server, "alice", &org, ws).await;
    let (endpoint, old_secret) = hook(&server, "alice", &org, "sandbox", HOOK, json!(["run.completed"])).await;
    let endpoint_id = WebhookEndpointId::parse(id(&endpoint)).unwrap();
    let org_id = OrganizationId::parse(&org).unwrap();
    let (status, rolled) = server
        .send(call(
            "alice",
            Method::POST,
            format!("/v1/organizations/{org}/webhooks/{endpoint_id}/roll-secret"),
        ))
        .await;
    assert_eq!(status, StatusCode::OK, "{rolled}");
    let new_secret = rolled["data"]["signing_secret"].as_str().unwrap();
    let pool = pool(&server).await;
    let (current, previous): (Vec<u8>, Vec<u8>) = sqlx::query_as("SELECT secret_ciphertext, previous_secret_ciphertext FROM webhook_endpoints WHERE organization_id = ? AND id = ? AND environment = 'sandbox'")
        .bind(org_id.uuid()).bind(endpoint_id.uuid()).fetch_one(&pool).await.unwrap();
    for corrupt in [
        "UPDATE webhook_endpoints SET secret_ciphertext = X'00' WHERE organization_id = ? AND id = ? AND environment = 'sandbox'",
        "UPDATE webhook_endpoints SET previous_secret_ciphertext = X'00' WHERE organization_id = ? AND id = ? AND environment = 'sandbox'",
    ] {
        sqlx::query(corrupt)
            .bind(org_id.uuid())
            .bind(endpoint_id.uuid())
            .execute(&pool)
            .await
            .unwrap();
        let (status, body) = server
            .send(call(
                "alice",
                Method::POST,
                format!("/v1/organizations/{org}/webhooks/{endpoint_id}/test"),
            ))
            .await;
        assert_eq!((status, code(&body)), (StatusCode::SERVICE_UNAVAILABLE, "unavailable"));
        assert!(!body.to_string().contains(&old_secret));
        assert!(!body.to_string().contains(new_secret));
        assert!(server.providers.requests_to(HOOK).is_empty());
        let count: i64 =
            sqlx::query_scalar("SELECT count(*) FROM webhook_deliveries WHERE organization_id = ? AND endpoint_id = ?")
                .bind(org_id.uuid())
                .bind(endpoint_id.uuid())
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(count, 0, "an unsent test records no delivery");
        sqlx::query("UPDATE webhook_endpoints SET secret_ciphertext = ?, previous_secret_ciphertext = ? WHERE organization_id = ? AND id = ? AND environment = 'sandbox'")
            .bind(&current).bind(&previous).bind(org_id.uuid()).bind(endpoint_id.uuid()).execute(&pool).await.unwrap();
    }
    sqlx::query("UPDATE webhook_endpoints SET previous_secret_ciphertext = X'00' WHERE organization_id = ? AND id = ? AND environment = 'sandbox'")
        .bind(org_id.uuid()).bind(endpoint_id.uuid()).execute(&pool).await.unwrap();
    start(&server, "alice", &org, ws, &helper, "Hello").await;
    work(&server).await;
    let attempts: Vec<i64> =
        sqlx::query_scalar("SELECT attempts FROM webhook_deliveries WHERE organization_id = ? AND endpoint_id = ?")
            .bind(org_id.uuid())
            .bind(endpoint_id.uuid())
            .fetch_all(&pool)
            .await
            .unwrap();
    assert_eq!(attempts, vec![0]);
    assert!(server.providers.requests_to(HOOK).is_empty());
    sqlx::query("UPDATE webhook_endpoints SET previous_secret_ciphertext = ? WHERE organization_id = ? AND id = ? AND environment = 'sandbox'")
        .bind(&previous).bind(org_id.uuid()).bind(endpoint_id.uuid()).execute(&pool).await.unwrap();
    run_now(&pool, "deliver_webhook").await;
    assert!(webhook_delivery::deliver_once(&server.state).await);
    let sent = server.providers.requests_to(HOOK);
    assert_eq!(sent.len(), 1);
    assert_eq!(sent[0].header("Zupflow-Delivery-Attempt"), Some("1"));
    let header = sent[0].header("Zupflow-Signature").unwrap();
    let timestamp = header
        .split(',')
        .next()
        .unwrap()
        .strip_prefix("t=")
        .unwrap()
        .parse()
        .unwrap();
    assert_eq!(
        header,
        zupflow_core::webhooks::signatures(
            new_secret.as_bytes(),
            Some(old_secret.as_bytes()),
            timestamp,
            &sent[0].body
        )
    );
    pool.close().await;
}

enum DuringSeal {
    Fail,
    Demote(MemberId),
    Revoke(ApiKeyId),
}

struct SealBoundary {
    inner: Arc<dyn KeyService>,
    pool: sqlx::SqlitePool,
    org: OrganizationId,
    action: DuringSeal,
    armed: AtomicBool,
}

#[async_trait]
impl KeyService for SealBoundary {
    async fn wrap(&self, context: &EncryptionContext, key: &DataKey) -> Result<WrappedKey, KeyError> {
        if self.armed.swap(false, Ordering::SeqCst) {
            match self.action {
                DuringSeal::Fail => return Err(KeyError::Unavailable),
                DuringSeal::Demote(member) => {
                    sqlx::query("UPDATE members SET role = 'viewer' WHERE organization_id = ? AND id = ?")
                        .bind(self.org.uuid())
                        .bind(member.uuid())
                        .execute(&self.pool)
                        .await
                        .unwrap();
                }
                DuringSeal::Revoke(key) => {
                    sqlx::query("UPDATE api_keys SET revoked_at = 1 WHERE organization_id = ? AND id = ? AND environment = 'sandbox'")
                        .bind(self.org.uuid()).bind(key.uuid()).execute(&self.pool).await.unwrap();
                }
            }
        }
        self.inner.wrap(context, key).await
    }
    async fn unwrap(&self, context: &EncryptionContext, wrapped: &WrappedKey) -> Result<DataKey, KeyError> {
        self.inner.unwrap(context, wrapped).await
    }
}

#[tokio::test]
async fn rotation_rechecks_authority_after_sealing_and_a_failed_seal_can_retry() {
    for kind in ["failure", "member", "key"] {
        let mut server = Server::new().await;
        let org = server.organization("alice", "northwind").await;
        let org_id = OrganizationId::parse(&org).unwrap();
        let (endpoint, _) = hook(&server, "alice", &org, "sandbox", HOOK, json!(["run.completed"])).await;
        let hook_id = WebhookEndpointId::parse(id(&endpoint)).unwrap();
        let pool = pool(&server).await;
        let (caller, action) = match kind {
            "member" => {
                let member: uuid::Uuid = sqlx::query_scalar(
                    "SELECT id FROM members WHERE organization_id = ? AND role = 'owner' AND status = 'active'",
                )
                .bind(org_id.uuid())
                .fetch_one(&pool)
                .await
                .unwrap();
                ("alice".to_owned(), DuringSeal::Demote(MemberId::from_uuid(member)))
            }
            "key" => {
                let secret = api_key(&server, "alice", &org, "sandbox").await;
                let key: uuid::Uuid = sqlx::query_scalar("SELECT id FROM api_keys WHERE organization_id = ? AND environment = 'sandbox' AND revoked_at IS NULL")
                    .bind(org_id.uuid()).fetch_one(&pool).await.unwrap();
                (secret, DuringSeal::Revoke(ApiKeyId::from_uuid(key)))
            }
            _ => ("alice".to_owned(), DuringSeal::Fail),
        };
        let before = server
            .state
            .webhooks
            .signing_secrets(
                org_id,
                hook_id,
                Environment::Sandbox,
                zupflow_core::time::Timestamp::now(),
            )
            .await
            .unwrap();
        server.state.keys = Arc::new(SealBoundary {
            inner: server.state.keys.clone(),
            pool: pool.clone(),
            org: org_id,
            action,
            armed: AtomicBool::new(true),
        });
        server.app = zupflow_server::app(server.state.clone(), RequestGuard::default());
        let path = format!("/v1/organizations/{org}/webhooks/{hook_id}/roll-secret");
        let (status, body) = server
            .send(call(&caller, Method::POST, &path).idempotent("roll-once"))
            .await;
        let expected = if kind == "failure" {
            (StatusCode::SERVICE_UNAVAILABLE, "unavailable")
        } else {
            (StatusCode::FORBIDDEN, "forbidden")
        };
        assert_eq!((status, code(&body)), expected, "{body}");
        assert_eq!(
            server
                .state
                .webhooks
                .signing_secrets(
                    org_id,
                    hook_id,
                    Environment::Sandbox,
                    zupflow_core::time::Timestamp::now()
                )
                .await
                .unwrap(),
            before
        );
        let audits: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM audit_events WHERE organization_id = ? AND action = 'webhook.secret_rolled'",
        )
        .bind(org_id.uuid())
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(audits, 0);
        if kind == "failure" {
            let (status, response) = server
                .send(call(&caller, Method::POST, &path).idempotent("roll-once"))
                .await;
            assert_eq!(status, StatusCode::OK, "{response}");
            let (status, replay) = server
                .send(call(&caller, Method::POST, &path).idempotent("roll-once"))
                .await;
            assert_eq!(
                (status, code(&replay)),
                (StatusCode::CONFLICT, "idempotency_key_completed")
            );
            assert!(
                !replay
                    .to_string()
                    .contains(response["data"]["signing_secret"].as_str().unwrap())
            );
        }
        pool.close().await;
    }
}
