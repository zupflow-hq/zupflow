//! A resend is a new delivery of the existing immutable event. Eligibility,
//! the delivery, its job and its audit event belong to one transaction.

use sqlx::SqliteConnection;
use zupflow_core::audit::AuditPlace;

use super::*;

fn store_error(error: WebhookError) -> ResendError {
    match error {
        WebhookError::NotFound => ResendError::NotFound,
        WebhookError::Unavailable | WebhookError::InvalidCursor => ResendError::Unavailable,
    }
}

async fn require_manager(
    connection: &mut SqliteConnection,
    organization_id: OrganizationId,
    environment: Environment,
    manager: WebhookManager,
) -> Result<(), ResendError> {
    let query = match manager {
        WebhookManager::Member(member) => sqlx::query(
            "SELECT 1 FROM members WHERE organization_id = ? AND id = ? \
             AND status = 'active' AND role IN ('owner', 'admin')",
        )
        .bind(organization_id.uuid())
        .bind(member.uuid()),
        WebhookManager::ApiKey(key) => sqlx::query(
            "SELECT 1 FROM api_keys WHERE organization_id = ? AND id = ? \
             AND environment = ? AND revoked_at IS NULL",
        )
        .bind(organization_id.uuid())
        .bind(key.uuid())
        .bind(environment.as_str()),
    };
    let allowed = query
        .fetch_optional(connection)
        .await
        .map_err(failed("resend_authorization"))
        .map_err(store_error)?;
    allowed.map(|_| ()).ok_or(ResendError::NotAllowed)
}

pub(super) async fn resend_delivery(
    store: &SqliteWebhookStore,
    organization_id: OrganizationId,
    endpoint_id: WebhookEndpointId,
    environment: Environment,
    delivery_id: WebhookDeliveryId,
    manager: WebhookManager,
    audit: &Audit<'_, DeliverySummary>,
) -> Result<DeliverySummary, ResendError> {
    let mut transaction = store.immediate("resend_delivery").await.map_err(store_error)?;
    require_manager(&mut transaction, organization_id, environment, manager).await?;
    let source = history::detail(&mut transaction, organization_id, endpoint_id, environment, delivery_id)
        .await
        .map_err(store_error)?
        .ok_or(ResendError::NotFound)?;
    if source.summary.state == DeliveryState::Pending {
        return Err(ResendError::Pending);
    }
    let row = sqlx::query(
        "SELECT enabled, events FROM webhook_endpoints WHERE organization_id = ? AND id = ? AND environment = ?",
    )
    .bind(organization_id.uuid())
    .bind(endpoint_id.uuid())
    .bind(environment.as_str())
    .fetch_optional(&mut *transaction)
    .await
    .map_err(failed("resend_endpoint"))
    .map_err(store_error)?
    .ok_or(ResendError::NotFound)?;
    if row.try_get::<i64, _>("enabled").map_err(read).map_err(store_error)? != 1 {
        return Err(ResendError::Disabled);
    }
    let events: String = row.try_get("events").map_err(read).map_err(store_error)?;
    let subscribed = events_from(&events).map_err(store_error)?;
    if source.summary.event_type != TEST_EVENT
        && !WebhookEventType::parse(&source.summary.event_type).is_some_and(|kind| subscribed.contains(&kind))
    {
        return Err(ResendError::NotSubscribed);
    }
    let id = WebhookDeliveryId::generate();
    let now = Timestamp::now();
    // Refer to the existing event instead of parsing and reserializing it:
    // the signed bytes and its event ID stay identical across resends.
    sqlx::query(
        "INSERT INTO webhook_deliveries (id, organization_id, endpoint_id, event_id, state, attempts, \
         next_attempt_at, created_at, resend_of_delivery_id) VALUES (?, ?, ?, ?, 'pending', 0, ?, ?, ?)",
    )
    .bind(id.uuid())
    .bind(organization_id.uuid())
    .bind(endpoint_id.uuid())
    .bind(source.summary.event_id.uuid())
    .bind(now.millis())
    .bind(now.millis())
    .bind(delivery_id.uuid())
    .execute(&mut *transaction)
    .await
    .map_err(failed("resend_delivery"))
    .map_err(store_error)?;
    let job = delivery_job(id).ok_or(ResendError::Unavailable)?;
    insert_job(
        &mut *transaction,
        &Scope::Organization(organization_id),
        &job.kind,
        job.payload,
        job.options,
    )
    .await
    .map_err(|_| ResendError::Unavailable)?;
    let summary = DeliverySummary {
        id,
        state: DeliveryState::Pending,
        attempts: 0,
        last_status_code: None,
        last_error: None,
        last_attempt_at: None,
        next_attempt_at: Some(now),
        created_at: now,
        ..source.summary
    };
    let entry = audit(&summary);
    // A callback cannot place a resend's audit event in a sibling workspace
    // or the other environment, even within this organization.
    let place = summary
        .workspace
        .as_ref()
        .map_or(AuditPlace::Environment(environment), |workspace| {
            AuditPlace::Workspace(workspace.id)
        });
    if entry.place != place {
        return Err(ResendError::Unavailable);
    }
    crate::audit::record(&mut transaction, organization_id, &entry)
        .await
        .map_err(failed("audit_resend_delivery"))
        .map_err(store_error)?;
    transaction
        .commit()
        .await
        .map_err(failed("resend_delivery"))
        .map_err(store_error)?;
    Ok(summary)
}
