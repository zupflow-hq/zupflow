//! A roll keeps the current and previous sealed versions together. Key
//! service calls finish before this transaction begins.

use zupflow_core::audit::AuditPlace;

use super::*;

fn store_error(error: WebhookError) -> RotationError {
    match error {
        WebhookError::NotFound => RotationError::NotFound,
        WebhookError::Unavailable | WebhookError::InvalidCursor => RotationError::Unavailable,
    }
}

fn secret(row: &SqliteRow, fields: [&str; 5]) -> Result<SigningSecret, WebhookError> {
    let [version, key_id, wrapped_key, nonce, ciphertext] = fields;
    let version = u32::try_from(row.try_get::<i64, _>(version).map_err(read)?).map_err(read)?;
    if version == 0 {
        return Err(read("an invalid signing secret version"));
    }
    let nonce: Vec<u8> = row.try_get(nonce).map_err(read)?;
    Ok(SigningSecret {
        version,
        sealed: Sealed {
            wrapped_key: WrappedKey {
                key_id: row.try_get(key_id).map_err(read)?,
                bytes: row.try_get(wrapped_key).map_err(read)?,
            },
            nonce: nonce.try_into().map_err(|_| read("a nonce of the wrong length"))?,
            ciphertext: row.try_get(ciphertext).map_err(read)?,
        },
    })
}

pub(super) async fn signing_secrets(
    database: &Database,
    organization_id: OrganizationId,
    endpoint_id: WebhookEndpointId,
    environment: Environment,
    now: Timestamp,
) -> Result<Option<SigningSecrets>, WebhookError> {
    let row = sqlx::query(
        "SELECT secret_version, secret_key_id, secret_wrapped_key, secret_nonce, secret_ciphertext, \
         previous_secret_version, previous_secret_key_id, previous_secret_wrapped_key, previous_secret_nonce, \
         previous_secret_ciphertext, previous_secret_expires_at FROM webhook_endpoints \
         WHERE organization_id = ? AND id = ? AND environment = ?",
    )
    .bind(organization_id.uuid())
    .bind(endpoint_id.uuid())
    .bind(environment.as_str())
    .fetch_optional(database.pool())
    .await
    .map_err(failed("signing_secrets"))?;
    let Some(row) = row else { return Ok(None) };
    let current = secret(
        &row,
        [
            "secret_version",
            "secret_key_id",
            "secret_wrapped_key",
            "secret_nonce",
            "secret_ciphertext",
        ],
    )?;
    let expiry = row
        .try_get::<Option<i64>, _>("previous_secret_expires_at")
        .map_err(read)?
        .map(Timestamp::from_millis);
    let previous = active_secret_expiry(expiry, now)
        .map(|expires_at| {
            let previous = secret(
                &row,
                [
                    "previous_secret_version",
                    "previous_secret_key_id",
                    "previous_secret_wrapped_key",
                    "previous_secret_nonce",
                    "previous_secret_ciphertext",
                ],
            )?;
            if previous.version >= current.version {
                return Err(read("an invalid previous signing secret version"));
            }
            Ok(PreviousSigningSecret {
                secret: previous,
                expires_at,
            })
        })
        .transpose()?;
    Ok(Some(SigningSecrets { current, previous }))
}

#[allow(clippy::too_many_arguments)]
pub(super) async fn roll_secret(
    store: &SqliteWebhookStore,
    organization_id: OrganizationId,
    endpoint_id: WebhookEndpointId,
    environment: Environment,
    manager: WebhookManager,
    rotation: &SecretRotation,
    audit: &Audit<'_, WebhookEndpoint>,
) -> Result<WebhookEndpoint, RotationError> {
    let mut transaction = store.immediate("roll_secret").await.map_err(store_error)?;
    if !manager_allowed(&mut transaction, organization_id, environment, manager)
        .await
        .map_err(store_error)?
    {
        return Err(RotationError::NotAllowed);
    }
    let row = sqlx::query(
        "SELECT secret_version, previous_secret_expires_at FROM webhook_endpoints \
         WHERE organization_id = ? AND id = ? AND environment = ?",
    )
    .bind(organization_id.uuid())
    .bind(endpoint_id.uuid())
    .bind(environment.as_str())
    .fetch_optional(&mut *transaction)
    .await
    .map_err(failed("roll_secret"))
    .map_err(store_error)?
    .ok_or(RotationError::NotFound)?;
    let current: i64 = row.try_get("secret_version").map_err(read).map_err(store_error)?;
    let previous_expiry: Option<i64> = row
        .try_get("previous_secret_expires_at")
        .map_err(read)
        .map_err(store_error)?;
    if current != i64::from(rotation.expected_version)
        || previous_expiry.is_some_and(|expiry| rotation.at.millis() < expiry)
    {
        return Err(RotationError::InProgress);
    }
    let version = rotation
        .expected_version
        .checked_add(1)
        .ok_or(RotationError::Unavailable)?;
    let expiry = rotation
        .at
        .millis()
        .checked_add(duration_millis(SECRET_OVERLAP))
        .ok_or(RotationError::Unavailable)?;
    let updated = sqlx::query(
        "UPDATE webhook_endpoints SET previous_secret_version = secret_version, \
         previous_secret_key_id = secret_key_id, previous_secret_wrapped_key = secret_wrapped_key, \
         previous_secret_nonce = secret_nonce, previous_secret_ciphertext = secret_ciphertext, \
         previous_secret_expires_at = ?, secret_version = ?, secret_key_id = ?, secret_wrapped_key = ?, \
         secret_nonce = ?, secret_ciphertext = ?, secret_last4 = ? \
         WHERE organization_id = ? AND id = ? AND environment = ? AND secret_version = ? \
         AND (previous_secret_expires_at IS NULL OR previous_secret_expires_at <= ?)",
    )
    .bind(expiry)
    .bind(version)
    .bind(&rotation.sealed.wrapped_key.key_id)
    .bind(&rotation.sealed.wrapped_key.bytes)
    .bind(rotation.sealed.nonce.as_slice())
    .bind(&rotation.sealed.ciphertext)
    .bind(&rotation.last4)
    .bind(organization_id.uuid())
    .bind(endpoint_id.uuid())
    .bind(environment.as_str())
    .bind(rotation.expected_version)
    .bind(rotation.at.millis())
    .execute(&mut *transaction)
    .await
    .map_err(failed("roll_secret"))
    .map_err(store_error)?;
    if updated.rows_affected() != 1 {
        return Err(RotationError::InProgress);
    }
    let endpoint = endpoint_in_environment(&mut transaction, organization_id, endpoint_id, environment)
        .await
        .map_err(store_error)?
        .ok_or(RotationError::NotFound)?;
    let entry = audit(&endpoint);
    if entry.place != AuditPlace::Environment(environment) {
        return Err(RotationError::Unavailable);
    }
    crate::audit::record(&mut transaction, organization_id, &entry)
        .await
        .map_err(failed("audit_roll_secret"))
        .map_err(store_error)?;
    transaction
        .commit()
        .await
        .map_err(failed("roll_secret"))
        .map_err(store_error)?;
    Ok(endpoint)
}
