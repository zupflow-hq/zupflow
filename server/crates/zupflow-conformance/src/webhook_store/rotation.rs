//! Rotation is atomic, scoped, versioned, and keeps one full overlap.

use zupflow_core::audit::{AuditAction, AuditEntry, AuditObject, AuditPlace, AuditTarget};
use zupflow_core::webhooks::{RotationError, SECRET_OVERLAP, SecretRotation, WebhookEndpoint, WebhookManager};

use super::*;

fn audit(endpoint: &WebhookEndpoint) -> AuditEntry {
    AuditEntry {
        action: AuditAction::WebhookSecretRolled,
        target: AuditTarget::new(AuditObject::Webhook(endpoint.id), &endpoint.url),
        place: AuditPlace::Environment(endpoint.environment),
        ..by_suite(&())
    }
}

fn rotation(version: u32, marker: u8, at: Timestamp) -> SecretRotation {
    SecretRotation {
        expected_version: version,
        sealed: sealed(marker),
        last4: format!("{marker:04}"),
        at,
    }
}

pub(super) async fn run(stores: &Stores<'_>, a: Tenant, b: Tenant) {
    let now = Timestamp::now();
    let org = a.organization_id;
    let env = Environment::Sandbox;
    let hook = endpoint(stores, org, env, &[WebhookEventType::RunCompleted], 61, now).await;
    let other = endpoint(stores, org, env, &[WebhookEventType::RunCompleted], 62, now).await;
    let production = endpoint(
        stores,
        org,
        Environment::Production,
        &[WebhookEventType::RunCompleted],
        63,
        now,
    )
    .await;
    let foreign = endpoint(
        stores,
        b.organization_id,
        env,
        &[WebhookEventType::RunCompleted],
        64,
        now,
    )
    .await;
    let before = stores.webhooks.find_endpoint(org, hook).await.unwrap().unwrap();
    assert_eq!(before.previous_signing_secret_expires_at, None);
    let next = rotation(1, 65, now);
    let manager = WebhookManager::Member(a.admin);
    for (organization, endpoint_id, environment, caller, error) in [
        (org, foreign, env, manager, RotationError::NotFound),
        (org, hook, Environment::Production, manager, RotationError::NotFound),
        (
            org,
            WebhookEndpointId::generate(),
            env,
            manager,
            RotationError::NotFound,
        ),
        (
            org,
            hook,
            env,
            WebhookManager::Member(b.admin),
            RotationError::NotAllowed,
        ),
        (
            org,
            hook,
            env,
            WebhookManager::ApiKey(a.production_key),
            RotationError::NotAllowed,
        ),
        (
            org,
            hook,
            env,
            WebhookManager::ApiKey(b.sandbox_key),
            RotationError::NotAllowed,
        ),
        (
            b.organization_id,
            hook,
            env,
            WebhookManager::Member(b.admin),
            RotationError::NotFound,
        ),
    ] {
        assert_eq!(
            stores
                .webhooks
                .roll_secret(organization, endpoint_id, environment, caller, &next, &audit)
                .await
                .unwrap_err(),
            error
        );
    }
    assert!(
        stores
            .webhooks
            .signing_secrets(org, hook, Environment::Production, now)
            .await
            .unwrap()
            .is_none()
    );
    assert!(
        stores
            .webhooks
            .signing_secrets(b.organization_id, hook, env, now)
            .await
            .unwrap()
            .is_none()
    );
    let bad_audit = |endpoint: &WebhookEndpoint| AuditEntry {
        place: AuditPlace::Workspace(a.sandbox),
        ..audit(endpoint)
    };
    assert_eq!(
        stores
            .webhooks
            .roll_secret(org, hook, env, manager, &next, &bad_audit)
            .await
            .unwrap_err(),
        RotationError::Unavailable
    );
    assert_eq!(stores.webhooks.find_endpoint(org, hook).await.unwrap().unwrap(), before);

    let competitor = rotation(1, 66, now);
    let (one, two) = tokio::join!(
        stores.webhooks.roll_secret(org, hook, env, manager, &next, &audit),
        stores
            .webhooks
            .roll_secret(org, hook, env, manager, &competitor, &audit),
    );
    let (winner, loser) = match one {
        Ok(winner) => (winner, two),
        Err(error) => (two.unwrap(), Err(error)),
    };
    assert_eq!(loser.unwrap_err(), RotationError::InProgress);
    let expiry = Timestamp::from_millis(now.millis() + i64::try_from(SECRET_OVERLAP.as_millis()).unwrap());
    assert_eq!(winner.previous_signing_secret_expires_at, Some(expiry));
    let kept = WebhookEndpoint {
        signing_secret_last4: before.signing_secret_last4.clone(),
        previous_signing_secret_expires_at: None,
        ..winner.clone()
    };
    assert_eq!(kept, before, "rotation preserves settings and delivery state");
    let snapshot = stores
        .webhooks
        .signing_secrets(org, hook, env, now)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(snapshot.current.version, 2);
    assert_eq!(
        snapshot.current.sealed,
        if winner.signing_secret_last4 == "0065" {
            sealed(65)
        } else {
            sealed(66)
        }
    );
    let previous = snapshot.previous.as_ref().unwrap();
    assert_eq!(
        (previous.secret.version, &previous.secret.sealed, previous.expires_at),
        (1, &sealed(61), expiry)
    );
    let before_expiry = Timestamp::from_millis(expiry.millis() - 1);
    assert!(
        stores
            .webhooks
            .signing_secrets(org, hook, env, before_expiry)
            .await
            .unwrap()
            .unwrap()
            .previous
            .is_some()
    );
    let third = rotation(2, 67, before_expiry);
    assert_eq!(
        stores
            .webhooks
            .roll_secret(org, hook, env, manager, &third, &audit)
            .await
            .unwrap_err(),
        RotationError::InProgress
    );
    for at in [expiry, Timestamp::from_millis(expiry.millis() + 1)] {
        assert!(
            stores
                .webhooks
                .signing_secrets(org, hook, env, at)
                .await
                .unwrap()
                .unwrap()
                .previous
                .is_none()
        );
    }
    let third = SecretRotation { at: expiry, ..third };
    stores
        .webhooks
        .roll_secret(org, hook, env, manager, &third, &audit)
        .await
        .unwrap();
    let latest = stores
        .webhooks
        .signing_secrets(org, hook, env, expiry)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(latest.current.version, 3);
    assert_eq!(latest.current.sealed, sealed(67));
    assert_eq!(latest.previous.unwrap().secret, snapshot.current);
    assert_eq!(
        stores
            .webhooks
            .roll_secret(org, hook, env, manager, &next, &audit)
            .await
            .unwrap_err(),
        RotationError::InProgress
    );

    // Neighboring endpoint, environment, and organization remain unchanged.
    for (organization, endpoint_id, environment, marker) in [
        (org, other, env, 62),
        (org, production, Environment::Production, 63),
        (b.organization_id, foreign, env, 64),
    ] {
        let still = stores
            .webhooks
            .signing_secrets(organization, endpoint_id, environment, now)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(still.current.sealed, sealed(marker));
        assert_eq!(still.current.version, 1);
        assert!(still.previous.is_none());
    }
    stores
        .webhooks
        .update_endpoint(
            org,
            production,
            &EndpointChanges {
                enabled: Some(false),
                ..EndpointChanges::default()
            },
            &by_suite,
        )
        .await
        .unwrap();
    let disabled = stores
        .webhooks
        .roll_secret(
            org,
            production,
            Environment::Production,
            WebhookManager::ApiKey(a.production_key),
            &next,
            &audit,
        )
        .await
        .unwrap();
    assert!(!disabled.enabled, "credential maintenance also works while disabled");
    stores
        .webhooks
        .roll_secret(
            b.organization_id,
            foreign,
            env,
            WebhookManager::ApiKey(b.sandbox_key),
            &next,
            &audit,
        )
        .await
        .unwrap();
}
