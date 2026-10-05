//! The local key service passes the `KeyService` conformance suite, rotates
//! without losing what it wrapped, and refuses bad key configuration.

use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use zupflow_basic::local_keys::{LocalKeyError, LocalKeyService};
use zupflow_core::environment::Environment;
use zupflow_core::ids::{OrganizationId, WebhookEndpointId};
use zupflow_core::keys::{self, DataKey, EncryptionContext, KeyError};

/// A fresh 256-bit key, base64-encoded as configuration carries it.
fn random_key() -> String {
    let key = DataKey::generate().expect("the OS has randomness");
    STANDARD.encode(key.expose())
}

#[tokio::test]
async fn conforms() {
    let keys = LocalKeyService::new("local:1", &random_key()).expect("a valid key");
    let other = LocalKeyService::new("local:1", &random_key()).expect("a valid key");
    zupflow_conformance::key_service::run(&keys, &other).await;
}

#[tokio::test]
async fn rotates_without_losing_what_it_wrapped() {
    let (old, new) = (random_key(), random_key());
    let context = EncryptionContext::webhook_signing_secret(
        OrganizationId::generate(),
        Environment::Production,
        WebhookEndpointId::generate(),
        1,
    );
    let before = LocalKeyService::new("local:1", &old).expect("a valid key");
    let sealed = keys::seal(&before, &context, b"signing secret").await.expect("seals");

    let rotated = LocalKeyService::new("local:2", &new)
        .and_then(|keys| keys.with_previous("local:1", &old))
        .expect("valid keys");
    let opened = keys::open(&rotated, &context, &sealed)
        .await
        .expect("the old key still opens it");
    assert_eq!(opened.expose(), b"signing secret");

    // New secrets are wrapped with the current key's material, not only labelled with its ID.
    let resealed = keys::seal(&rotated, &context, b"signing secret").await.expect("seals");
    assert_eq!(resealed.wrapped_key.key_id, "local:2");
    let only_new = LocalKeyService::new("local:2", &new).expect("a valid key");
    let opened = keys::open(&only_new, &context, &resealed)
        .await
        .expect("the current key alone opens it");
    assert_eq!(opened.expose(), b"signing secret");
    let only_old_under_new_id = LocalKeyService::new("local:2", &old).expect("a valid key");
    assert_eq!(
        keys::open(&only_old_under_new_id, &context, &resealed)
            .await
            .map(|_| ()),
        Err(KeyError::Refused)
    );

    let forgot = LocalKeyService::new("local:2", &new).expect("a valid key");
    assert_eq!(
        keys::open(&forgot, &context, &sealed).await.map(|_| ()),
        Err(KeyError::Refused)
    );
}

#[test]
fn refuses_unusable_configuration() {
    assert!(matches!(
        LocalKeyService::new("local:1", "not base64!"),
        Err(LocalKeyError::NotBase64)
    ));
    assert!(matches!(
        LocalKeyService::new("local:1", &STANDARD.encode([0u8; 16])),
        Err(LocalKeyError::WrongLength)
    ));
    assert!(matches!(
        LocalKeyService::new("", &random_key()),
        Err(LocalKeyError::InvalidKeyId)
    ));
    assert!(matches!(
        LocalKeyService::new("local;1", &random_key()),
        Err(LocalKeyError::InvalidKeyId)
    ));
    let duplicate =
        LocalKeyService::new("local:1", &random_key()).and_then(|keys| keys.with_previous("local:1", &random_key()));
    assert!(matches!(duplicate, Err(LocalKeyError::DuplicateKeyId)));
}
