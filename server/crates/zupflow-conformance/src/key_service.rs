//! The `KeyService` suite: wrapped keys open only under the exact context
//! they were wrapped for, by the service that wrapped them, and tampering is
//! refused.
//!
//! ```ignore
//! #[tokio::test]
//! async fn conforms() {
//!     zupflow_conformance::key_service::run(&my_service(), &another_instance_with_another_key()).await;
//! }
//! ```

use zupflow_core::environment::Environment;
use zupflow_core::ids::{AppId, InstallationId, OrganizationId, WebhookEndpointId, WorkspaceId};
use zupflow_core::keys::{self, DataKey, EncryptionContext, KeyError, KeyService};

/// Runs every check. `other` must be an independent instance with different
/// key material: nothing it wraps may open in `keys`.
pub async fn run(keys: &dyn KeyService, other: &dyn KeyService) {
    round_trips_every_kind_of_secret(keys).await;
    refuses_every_other_context(keys).await;
    refuses_tampering(keys).await;
    refuses_another_services_keys(keys, other).await;
    wraps_freshly_each_time(keys).await;
    seals_each_secret_with_a_fresh_key(keys).await;
    seals_and_opens_secrets(keys).await;
}

fn credential() -> EncryptionContext {
    EncryptionContext::installation_credential(
        OrganizationId::generate(),
        WorkspaceId::generate(),
        Environment::Sandbox,
        AppId::generate(),
        InstallationId::generate(),
        1,
    )
}

async fn round_trips_every_kind_of_secret(keys: &dyn KeyService) {
    let org = OrganizationId::generate();
    let contexts = [
        credential(),
        EncryptionContext::oauth_client_secret(org, None, Environment::Production, AppId::generate(), 1),
        EncryptionContext::oauth_client_secret(
            org,
            Some(WorkspaceId::generate()),
            Environment::Sandbox,
            AppId::generate(),
            1,
        ),
        EncryptionContext::webhook_signing_secret(org, Environment::Production, WebhookEndpointId::generate(), 1),
    ];
    for context in contexts {
        let key = DataKey::generate().unwrap();
        let wrapped = keys.wrap(&context, &key).await.unwrap();
        let opened = keys
            .unwrap(&context, &wrapped)
            .await
            .expect("a key opens under its own context");
        assert_eq!(opened.expose(), key.expose(), "unwrapping gives back the same key");
        assert!(
            !wrapped.bytes.windows(32).any(|window| window == key.expose()),
            "the wrapped form doesn't contain the key"
        );
    }
}

/// Each variation changes one part of the context, or for purpose and kind the
/// least the constructors allow (a purpose implies its object's kind); every
/// one must be refused.
async fn refuses_every_other_context(keys: &dyn KeyService) {
    let org = OrganizationId::generate();
    let ws = WorkspaceId::generate();
    let app = AppId::generate();
    let inst = InstallationId::generate();
    let env = Environment::Sandbox;
    let base = EncryptionContext::installation_credential(org, ws, env, app, inst, 2);
    let key = DataKey::generate().unwrap();
    let wrapped = keys.wrap(&base, &key).await.unwrap();

    let others = [
        (
            "another organization",
            EncryptionContext::installation_credential(OrganizationId::generate(), ws, env, app, inst, 2),
        ),
        (
            "another workspace of the same organization",
            EncryptionContext::installation_credential(org, WorkspaceId::generate(), env, app, inst, 2),
        ),
        (
            "the other environment",
            EncryptionContext::installation_credential(org, ws, Environment::Production, app, inst, 2),
        ),
        (
            "another app",
            EncryptionContext::installation_credential(org, ws, env, AppId::generate(), inst, 2),
        ),
        (
            "another installation",
            EncryptionContext::installation_credential(org, ws, env, app, InstallationId::generate(), 2),
        ),
        (
            "another version",
            EncryptionContext::installation_credential(org, ws, env, app, inst, 3),
        ),
        // Same organization and same UUIDs, another kind of secret.
        (
            "another purpose",
            EncryptionContext::oauth_client_secret(org, Some(ws), env, app, 2),
        ),
        (
            "another kind of object",
            EncryptionContext::webhook_signing_secret(org, env, WebhookEndpointId::from_uuid(inst.uuid()), 2),
        ),
    ];
    for (what, context) in others {
        assert_eq!(
            keys.unwrap(&context, &wrapped).await.map(|_| ()),
            Err(KeyError::Refused),
            "{what} must be refused"
        );
    }

    // A catalog app's secret and a workspace app's secret never stand in for each other.
    let catalog = EncryptionContext::oauth_client_secret(org, None, env, app, 1);
    let wrapped = keys.wrap(&catalog, &key).await.unwrap();
    let workspace_app = EncryptionContext::oauth_client_secret(org, Some(ws), env, app, 1);
    assert_eq!(
        keys.unwrap(&workspace_app, &wrapped).await.map(|_| ()),
        Err(KeyError::Refused),
        "a workspace app's context"
    );
}

async fn refuses_tampering(keys: &dyn KeyService) {
    let context = credential();
    let key = DataKey::generate().unwrap();
    let wrapped = keys.wrap(&context, &key).await.unwrap();
    for index in [0, wrapped.bytes.len() / 2, wrapped.bytes.len() - 1] {
        let mut tampered = wrapped.clone();
        tampered.bytes[index] ^= 0x01;
        assert_eq!(
            keys.unwrap(&context, &tampered).await.map(|_| ()),
            Err(KeyError::Refused),
            "a flipped bit at byte {index} must be refused"
        );
    }
    let mut truncated = wrapped.clone();
    truncated.bytes.truncate(4);
    assert_eq!(
        keys.unwrap(&context, &truncated).await.map(|_| ()),
        Err(KeyError::Refused),
        "a truncated key"
    );
    let mut unknown = wrapped.clone();
    unknown.key_id = format!("{}-unknown", wrapped.key_id);
    assert_eq!(
        keys.unwrap(&context, &unknown).await.map(|_| ()),
        Err(KeyError::Refused),
        "an unknown key ID"
    );
}

async fn refuses_another_services_keys(keys: &dyn KeyService, other: &dyn KeyService) {
    let context = credential();
    let key = DataKey::generate().unwrap();
    let wrapped = other.wrap(&context, &key).await.unwrap();
    assert_eq!(
        keys.unwrap(&context, &wrapped).await.map(|_| ()),
        Err(KeyError::Refused),
        "a key wrapped by another service's key must not open"
    );
}

async fn wraps_freshly_each_time(keys: &dyn KeyService) {
    let context = credential();
    let key = DataKey::generate().unwrap();
    let first = keys.wrap(&context, &key).await.unwrap();
    let second = keys.wrap(&context, &key).await.unwrap();
    assert_ne!(
        first.bytes, second.bytes,
        "wrapping is randomized: equal keys don't look equal"
    );
}

async fn seals_each_secret_with_a_fresh_key(keys: &dyn KeyService) {
    let context = credential();
    let first = keys::seal(keys, &context, b"same secret").await.unwrap();
    let second = keys::seal(keys, &context, b"same secret").await.unwrap();
    let first_key = keys.unwrap(&context, &first.wrapped_key).await.unwrap();
    let second_key = keys.unwrap(&context, &second.wrapped_key).await.unwrap();
    assert_ne!(first_key.expose(), second_key.expose(), "a new data key per secret");
    assert_ne!(first.nonce, second.nonce, "a new nonce per secret");
    assert_ne!(first.ciphertext, second.ciphertext, "equal secrets don't look equal");
}

async fn seals_and_opens_secrets(keys: &dyn KeyService) {
    let org = OrganizationId::generate();
    let ws = WorkspaceId::generate();
    let app = AppId::generate();
    let inst = InstallationId::generate();
    let context = EncryptionContext::installation_credential(org, ws, Environment::Sandbox, app, inst, 1);
    let secret = b"provider-token-1234";
    let sealed = keys::seal(keys, &context, secret).await.unwrap();
    assert!(
        !sealed.ciphertext.windows(secret.len()).any(|window| window == secret),
        "the ciphertext doesn't contain the secret"
    );
    let opened = keys::open(keys, &context, &sealed)
        .await
        .expect("a secret opens under its own context");
    assert_eq!(opened.expose(), secret);

    let elsewhere = EncryptionContext::installation_credential(org, ws, Environment::Sandbox, app, inst, 9);
    assert_eq!(
        keys::open(keys, &elsewhere, &sealed).await.map(|_| ()),
        Err(KeyError::Refused)
    );

    // The ciphertext is bound to the context too, not only the wrapped key: a
    // secret's data key rewrapped for another context still can't open it.
    let other_context = EncryptionContext::installation_credential(org, ws, Environment::Sandbox, app, inst, 2);
    let data_key = keys.unwrap(&context, &sealed.wrapped_key).await.unwrap();
    let rewrapped = keys.wrap(&other_context, &data_key).await.unwrap();
    let moved = keys::Sealed {
        wrapped_key: rewrapped,
        ..sealed.clone()
    };
    assert_eq!(
        keys::open(keys, &other_context, &moved).await.map(|_| ()),
        Err(KeyError::Refused)
    );

    let mut tampered = sealed.clone();
    tampered.ciphertext[0] ^= 0x01;
    assert_eq!(
        keys::open(keys, &context, &tampered).await.map(|_| ()),
        Err(KeyError::Refused)
    );
}
