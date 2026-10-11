#![allow(clippy::unwrap_used)]

use std::collections::VecDeque;
use std::path::PathBuf;
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicI64, AtomicUsize, Ordering},
};

use async_trait::async_trait;
use zupflow_basic::local_keys::LocalKeyService;
use zupflow_core::audit::{AuditAction, AuditActor, AuditEntry, AuditObject, AuditPlace, AuditTarget};
use zupflow_core::ids::{MemberId, UserId};
use zupflow_core::keys::{DataKey, KeyError, WrappedKey, seal};
use zupflow_core::webhooks::{NewEndpoint, SECRET_OVERLAP, SecretRotation, WebhookEndpoint, WebhookManager, signature};
use zupflow_sqlite::{Database, SqliteWebhookStore};

use super::*;

const OLD: &[u8] = b"whsec_test_original";
const NEW: &[u8] = b"whsec_test_replacement";
const BODY: &str = " {\n\"data\":\"\\u0061\"} ";
const ENV: Environment = Environment::Sandbox;

fn audit(endpoint: &WebhookEndpoint) -> AuditEntry {
    AuditEntry::new(
        AuditActor::System {
            name: "Fixture".to_owned(),
        },
        AuditAction::WebhookSecretRolled,
        AuditTarget::new(AuditObject::Webhook(endpoint.id), &endpoint.url),
        AuditPlace::Environment(endpoint.environment),
    )
}

struct Fixture {
    directory: PathBuf,
    store: Arc<SqliteWebhookStore>,
    keys: Arc<LocalKeyService>,
    org: OrganizationId,
    endpoint: WebhookEndpointId,
    manager: WebhookManager,
    clock: Arc<AtomicI64>,
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.directory);
    }
}

impl Fixture {
    async fn new() -> Self {
        let directory = std::env::temp_dir().join(format!("zupflow-signing-{}", uuid::Uuid::now_v7().simple()));
        std::fs::create_dir(&directory).unwrap();
        let path = directory.join("fixture.sqlite");
        let database = Database::open(&path).await.unwrap();
        let pool = sqlx::SqlitePool::connect(&format!("sqlite:{}", path.display()))
            .await
            .unwrap();
        let org = OrganizationId::generate();
        let user = UserId::generate();
        let member = MemberId::generate();
        let endpoint = WebhookEndpointId::generate();
        sqlx::query("INSERT INTO organizations (id, name, slug, created_at) VALUES (?, 'Northwind', 'northwind', 0)")
            .bind(org.uuid())
            .execute(&pool)
            .await
            .unwrap();
        sqlx::query(
            "INSERT INTO users (id, name, email, created_at) VALUES (?, 'Jordan', 'jordan@northwind.example', 0)",
        )
        .bind(user.uuid())
        .execute(&pool)
        .await
        .unwrap();
        sqlx::query("INSERT INTO members (id, organization_id, user_id, role, status, invited_at, joined_at) VALUES (?, ?, ?, 'admin', 'active', 0, 0)")
            .bind(member.uuid()).bind(org.uuid()).bind(user.uuid()).execute(&pool).await.unwrap();
        pool.close().await;
        let keys = Arc::new(LocalKeyService::from_server_secret(&[7; 32]));
        let context = EncryptionContext::webhook_signing_secret(org, ENV, endpoint, 1);
        let sealed = seal(keys.as_ref(), &context, OLD).await.unwrap();
        let store = Arc::new(SqliteWebhookStore::new(database));
        let at = Timestamp::now().millis().div_euclid(1000) * 1000 + 123;
        store
            .create_endpoint(
                org,
                endpoint,
                &NewEndpoint {
                    environment: ENV,
                    url: "https://hooks.example/in".to_owned(),
                    description: String::new(),
                    events: vec![WebhookEventType::RunCompleted],
                    secret: sealed,
                    secret_last4: "inal".to_owned(),
                },
                Timestamp::from_millis(at),
                &audit,
            )
            .await
            .unwrap();
        Self {
            directory,
            store,
            keys,
            org,
            endpoint,
            manager: WebhookManager::Member(member),
            clock: Arc::new(AtomicI64::new(at)),
        }
    }

    fn now(&self) -> Timestamp {
        Timestamp::from_millis(self.clock.load(Ordering::SeqCst))
    }

    async fn replacement(&self, expected_version: u32, secret: &[u8], at: Timestamp) -> SecretRotation {
        let context = EncryptionContext::webhook_signing_secret(self.org, ENV, self.endpoint, expected_version + 1);
        SecretRotation {
            expected_version,
            sealed: seal(self.keys.as_ref(), &context, secret).await.unwrap(),
            last4: "last".to_owned(),
            at,
        }
    }

    async fn rotate(&self) -> Timestamp {
        let rotation = self.replacement(1, NEW, self.now()).await;
        self.store
            .roll_secret(self.org, self.endpoint, ENV, self.manager, &rotation, &audit)
            .await
            .unwrap();
        Timestamp::from_millis(rotation.at.millis() + i64::try_from(SECRET_OVERLAP.as_millis()).unwrap())
    }

    async fn header(&self, keys: &dyn KeyService) -> Result<String, Unsent> {
        signature_header(self.store.as_ref(), keys, self.org, self.endpoint, ENV, BODY, || {
            self.now()
        })
        .await
    }
}

struct FailingKeys {
    inner: Arc<LocalKeyService>,
    refused: EncryptionContext,
    advance: Option<(Arc<AtomicI64>, i64)>,
    calls: AtomicUsize,
}

#[async_trait]
impl KeyService for FailingKeys {
    async fn wrap(&self, context: &EncryptionContext, key: &DataKey) -> Result<WrappedKey, KeyError> {
        self.inner.wrap(context, key).await
    }
    async fn unwrap(&self, context: &EncryptionContext, wrapped: &WrappedKey) -> Result<DataKey, KeyError> {
        if context == &self.refused {
            self.calls.fetch_add(1, Ordering::SeqCst);
            if let Some((clock, at)) = &self.advance {
                clock.store(*at, Ordering::SeqCst);
            }
            return Err(KeyError::Unavailable);
        }
        self.inner.unwrap(context, wrapped).await
    }
}

#[tokio::test]
async fn signer_uses_full_precision_expiry_and_each_versions_context() {
    let fixture = Fixture::new().await;
    let expiry = fixture.rotate().await;
    for (millis, previous) in [
        (expiry.millis() - 1, Some(OLD)),
        (expiry.millis(), None),
        (expiry.millis() + 1, None),
    ] {
        fixture.clock.store(millis, Ordering::SeqCst);
        assert_eq!(
            fixture.header(fixture.keys.as_ref()).await.unwrap(),
            signatures(NEW, previous, millis.div_euclid(1000), BODY.as_bytes())
        );
    }
    // The last millisecond of overlap and its deadline share one Unix second.
    assert_eq!((expiry.millis() - 1).div_euclid(1000), expiry.millis().div_euclid(1000));
    let snapshot = fixture
        .store
        .signing_secrets(
            fixture.org,
            fixture.endpoint,
            ENV,
            Timestamp::from_millis(expiry.millis() - 1),
        )
        .await
        .unwrap()
        .unwrap();
    let old = snapshot.previous.unwrap().secret;
    for context in [
        EncryptionContext::webhook_signing_secret(fixture.org, ENV, fixture.endpoint, 2),
        EncryptionContext::webhook_signing_secret(OrganizationId::generate(), ENV, fixture.endpoint, 1),
        EncryptionContext::webhook_signing_secret(fixture.org, Environment::Production, fixture.endpoint, 1),
        EncryptionContext::webhook_signing_secret(fixture.org, ENV, WebhookEndpointId::generate(), 1),
    ] {
        assert_eq!(
            open(fixture.keys.as_ref(), &context, &old.sealed).await.unwrap_err(),
            KeyError::Refused
        );
    }
}

#[tokio::test]
async fn signer_requires_both_live_keys_but_ignores_one_that_expires_during_open() {
    let fixture = Fixture::new().await;
    let expiry = fixture.rotate().await;
    for version in [1, 2] {
        let failing = FailingKeys {
            inner: fixture.keys.clone(),
            refused: EncryptionContext::webhook_signing_secret(fixture.org, ENV, fixture.endpoint, version),
            advance: None,
            calls: AtomicUsize::new(0),
        };
        assert_eq!(fixture.header(&failing).await.unwrap_err(), Unsent::SecretUnavailable);
        assert_eq!(failing.calls.load(Ordering::SeqCst), 1);
    }
    fixture.clock.store(expiry.millis() - 1, Ordering::SeqCst);
    let failing = FailingKeys {
        inner: fixture.keys.clone(),
        refused: EncryptionContext::webhook_signing_secret(fixture.org, ENV, fixture.endpoint, 1),
        advance: Some((fixture.clock.clone(), expiry.millis())),
        calls: AtomicUsize::new(0),
    };
    assert_eq!(
        fixture.header(&failing).await.unwrap(),
        signature(NEW, expiry.millis().div_euclid(1000), BODY.as_bytes())
    );
    assert_eq!(
        failing.calls.load(Ordering::SeqCst),
        1,
        "the failed old key expired while opening"
    );
    fixture.header(&failing).await.unwrap();
    assert_eq!(
        failing.calls.load(Ordering::SeqCst),
        1,
        "an expired key isn't opened again"
    );
}

struct RollingKeys {
    inner: Arc<LocalKeyService>,
    store: Arc<SqliteWebhookStore>,
    org: OrganizationId,
    endpoint: WebhookEndpointId,
    manager: WebhookManager,
    clock: Arc<AtomicI64>,
    replacements: Mutex<VecDeque<SecretRotation>>,
}

#[async_trait]
impl KeyService for RollingKeys {
    async fn wrap(&self, context: &EncryptionContext, key: &DataKey) -> Result<WrappedKey, KeyError> {
        self.inner.wrap(context, key).await
    }
    async fn unwrap(&self, context: &EncryptionContext, wrapped: &WrappedKey) -> Result<DataKey, KeyError> {
        let replacement = {
            let mut replacements = self.replacements.lock().unwrap();
            if replacements.front().is_some_and(|rotation| {
                context
                    == &EncryptionContext::webhook_signing_secret(
                        self.org,
                        ENV,
                        self.endpoint,
                        rotation.expected_version,
                    )
            }) {
                replacements.pop_front()
            } else {
                None
            }
        };
        if let Some(replacement) = replacement {
            self.clock.store(replacement.at.millis(), Ordering::SeqCst);
            self.store
                .roll_secret(self.org, self.endpoint, ENV, self.manager, &replacement, &audit)
                .await
                .unwrap();
        }
        self.inner.unwrap(context, wrapped).await
    }
}

#[tokio::test]
async fn signer_reloads_a_roll_during_open_and_bounds_an_unstable_snapshot() {
    for unstable in [false, true] {
        let fixture = Fixture::new().await;
        let now = fixture.now();
        let mut replacements = VecDeque::from([fixture.replacement(1, NEW, now).await]);
        if unstable {
            let later = Timestamp::from_millis(now.millis() + i64::try_from(SECRET_OVERLAP.as_millis()).unwrap());
            replacements.push_back(fixture.replacement(2, b"whsec_test_third", later).await);
        }
        let rolling = RollingKeys {
            inner: fixture.keys.clone(),
            store: fixture.store.clone(),
            org: fixture.org,
            endpoint: fixture.endpoint,
            manager: fixture.manager,
            clock: fixture.clock.clone(),
            replacements: Mutex::new(replacements),
        };
        let result = fixture.header(&rolling).await;
        if unstable {
            assert_eq!(result.unwrap_err(), Unsent::StoreUnavailable);
        } else {
            assert_eq!(
                result.unwrap(),
                signatures(NEW, Some(OLD), now.millis().div_euclid(1000), BODY.as_bytes())
            );
        }
        assert!(rolling.replacements.lock().unwrap().is_empty());
    }
}
