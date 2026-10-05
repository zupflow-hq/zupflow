//! The `ApiKeyStore` suite: keys reached only through their own organization,
//! found by hash only while unrevoked, revoked once, and listed newest first.
//!
//! `prepare` makes an organization and a user exist, for a store whose keys
//! reference them.

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use zupflow_core::api_keys::{ApiKey, ApiKeyError, ApiKeyStore, NewApiKey};
use zupflow_core::environment::Environment;
use zupflow_core::ids::{ApiKeyId, OrganizationId, UserId};

pub type Prepare = Arc<dyn Fn(OrganizationId, UserId) -> Pin<Box<dyn Future<Output = ()> + Send>> + Send + Sync>;

struct Tenant {
    org: OrganizationId,
    user: UserId,
}

async fn tenant(prepare: &Prepare) -> Tenant {
    let tenant = Tenant {
        org: OrganizationId::generate(),
        user: UserId::generate(),
    };
    prepare(tenant.org, tenant.user).await;
    tenant
}

/// A distinct hash for each key, as distinct secrets give.
fn hash() -> [u8; 32] {
    let mut hash = [0u8; 32];
    hash[..16].copy_from_slice(ApiKeyId::generate().uuid().as_bytes());
    hash[16..].copy_from_slice(ApiKeyId::generate().uuid().as_bytes());
    hash
}

async fn create(
    store: &dyn ApiKeyStore,
    tenant: &Tenant,
    name: &str,
    environment: Environment,
    secret_hash: [u8; 32],
) -> ApiKey {
    store
        .create_api_key(
            tenant.org,
            ApiKeyId::generate(),
            &NewApiKey {
                environment,
                name: name.to_owned(),
                prefix: "zf_test_abcd".to_owned(),
                last4: "wxyz".to_owned(),
                secret_hash,
                created_by: tenant.user,
            },
        )
        .await
        .unwrap()
}

pub async fn run(store: &dyn ApiKeyStore, prepare: Prepare) {
    keeps_keys_inside_their_organization(store, &prepare).await;
    finds_an_unrevoked_key_by_its_hash(store, &prepare).await;
    revokes_once_and_for_good(store, &prepare).await;
}

async fn keeps_keys_inside_their_organization(store: &dyn ApiKeyStore, prepare: &Prepare) {
    let (a, b) = (tenant(prepare).await, tenant(prepare).await);
    let first = create(store, &a, "First", Environment::Sandbox, hash()).await;
    let second = create(store, &a, "Second", Environment::Production, hash()).await;
    let theirs = create(store, &b, "Theirs", Environment::Sandbox, hash()).await;
    assert_eq!(first.organization_id, a.org);
    assert_eq!(first.created_by.id, a.user);
    assert_eq!((first.revoked_at, first.last_used_at), (None, None));
    assert_eq!(
        store.list_api_keys(a.org).await.unwrap(),
        vec![second.clone(), first.clone()],
        "newest first"
    );
    assert_eq!(store.find_api_key(a.org, first.id).await.unwrap(), Some(first));
    assert_eq!(
        store.find_api_key(a.org, theirs.id).await.unwrap(),
        None,
        "another organization's key"
    );
    assert_eq!(store.revoke_api_key(a.org, theirs.id).await, Err(ApiKeyError::NotFound));
    assert_eq!(
        store.find_api_key(b.org, theirs.id).await.unwrap().unwrap().revoked_at,
        None,
        "untouched"
    );
}

async fn finds_an_unrevoked_key_by_its_hash(store: &dyn ApiKeyStore, prepare: &Prepare) {
    let a = tenant(prepare).await;
    let secret_hash = hash();
    let key = create(store, &a, "Server", Environment::Production, secret_hash).await;
    let found = store.find_active_api_key_by_hash(&secret_hash).await.unwrap().unwrap();
    assert_eq!(
        (found.id, found.organization_id, found.environment),
        (key.id, a.org, Environment::Production)
    );
    assert!(found.last_used_at.is_some(), "use is recorded");
    assert_eq!(
        store.find_api_key(a.org, key.id).await.unwrap().unwrap().last_used_at,
        found.last_used_at
    );
    assert_eq!(store.find_active_api_key_by_hash(&hash()).await.unwrap(), None);
}

async fn revokes_once_and_for_good(store: &dyn ApiKeyStore, prepare: &Prepare) {
    let a = tenant(prepare).await;
    let secret_hash = hash();
    let key = create(store, &a, "Doomed", Environment::Sandbox, secret_hash).await;
    let revoked = store.revoke_api_key(a.org, key.id).await.unwrap();
    assert!(revoked.revoked_at.is_some());
    assert_eq!(
        store.find_active_api_key_by_hash(&secret_hash).await.unwrap(),
        None,
        "no longer works"
    );
    assert_eq!(
        store.revoke_api_key(a.org, key.id).await,
        Err(ApiKeyError::AlreadyRevoked)
    );
    assert_eq!(
        store.revoke_api_key(a.org, ApiKeyId::generate()).await,
        Err(ApiKeyError::NotFound)
    );
    assert_eq!(
        store.list_api_keys(a.org).await.unwrap().len(),
        1,
        "a revoked key is still listed"
    );
}
