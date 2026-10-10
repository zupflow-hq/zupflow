//! [`SqliteWebhookStore`]: endpoints in `webhook_endpoints`, the events
//! recorded for them in `webhook_events` and each delivery of an event to an
//! endpoint in `webhook_deliveries`. Every statement repeats
//! `organization_id`, joins included; a delivery's job is queued in the
//! transaction that records the delivery, through the queue's own table.

use async_trait::async_trait;
use sqlx::sqlite::SqliteRow;
use sqlx::{Row, Sqlite, Transaction};
use zupflow_core::audit::Audit;
use zupflow_core::environment::Environment;
use zupflow_core::ids::{OrganizationId, WebhookDeliveryId, WebhookEndpointId, WebhookEventId};
use zupflow_core::jobs::LeaseToken;
use zupflow_core::keys::{Sealed, WrappedKey};
use zupflow_core::scope::Scope;
use zupflow_core::time::Timestamp;
use zupflow_core::webhooks::{
    Attempt, DISABLE_AFTER, DeliveryState, EndpointChanges, LastDelivery, NewEndpoint, NewWebhookEvent,
    PendingDelivery, SECRET_VERSION, SigningSecret, WebhookEndpoint, WebhookError, WebhookEventType, WebhookStore,
    delivery_job,
};

use crate::Database;
use crate::database::duration_millis;
use crate::job_queue::insert_job;

pub struct SqliteWebhookStore {
    database: Database,
}

impl SqliteWebhookStore {
    pub fn new(database: Database) -> Self {
        Self { database }
    }

    async fn immediate(&self, operation: &'static str) -> Result<Transaction<'static, Sqlite>, WebhookError> {
        self.database
            .pool()
            .begin_with("BEGIN IMMEDIATE")
            .await
            .map_err(failed(operation))
    }
}

fn failed(operation: &'static str) -> impl FnOnce(sqlx::Error) -> WebhookError {
    move |error| {
        tracing::error!(event = "webhooks.database_error", operation, %error);
        WebhookError::Unavailable
    }
}

fn read(error: impl std::fmt::Display) -> WebhookError {
    tracing::error!(event = "webhooks.unreadable_row", %error);
    WebhookError::Unavailable
}

/// An endpoint's columns. A macro, so queries stay static strings.
macro_rules! select_endpoint {
    () => {
        "SELECT id, organization_id, environment, url, description, events, enabled, secret_last4, created_at, \
         last_status_code, last_delivered_at FROM webhook_endpoints"
    };
}

fn events_from(json: &str) -> Result<Vec<WebhookEventType>, WebhookError> {
    let names: Vec<String> = serde_json::from_str(json).map_err(read)?;
    names
        .iter()
        .map(|name| WebhookEventType::parse(name).ok_or_else(|| read("an unknown event type")))
        .collect()
}

fn events_json(events: &[WebhookEventType]) -> String {
    serde_json::Value::from(events.iter().map(|kind| kind.as_str()).collect::<Vec<_>>()).to_string()
}

fn endpoint_from(row: &SqliteRow) -> Result<WebhookEndpoint, WebhookError> {
    let environment: String = row.try_get("environment").map_err(read)?;
    let events: String = row.try_get("events").map_err(read)?;
    let status: Option<i64> = row.try_get("last_status_code").map_err(read)?;
    let delivered_at: Option<i64> = row.try_get("last_delivered_at").map_err(read)?;
    let last_delivery = match (status, delivered_at) {
        (Some(status), Some(at)) => Some(LastDelivery {
            status_code: u16::try_from(status).map_err(read)?,
            delivered_at: Timestamp::from_millis(at),
        }),
        _ => None,
    };
    Ok(WebhookEndpoint {
        id: WebhookEndpointId::from_uuid(row.try_get("id").map_err(read)?),
        organization_id: OrganizationId::from_uuid(row.try_get("organization_id").map_err(read)?),
        environment: Environment::parse(&environment).ok_or_else(|| read("an unknown environment"))?,
        url: row.try_get("url").map_err(read)?,
        description: row.try_get("description").map_err(read)?,
        events: events_from(&events)?,
        enabled: row.try_get::<i64, _>("enabled").map_err(read)? == 1,
        signing_secret_last4: row.try_get("secret_last4").map_err(read)?,
        created_at: Timestamp::from_millis(row.try_get("created_at").map_err(read)?),
        last_delivery,
    })
}

async fn insert_event(
    transaction: &mut Transaction<'static, Sqlite>,
    organization_id: OrganizationId,
    event: &NewWebhookEvent,
) -> Result<bool, WebhookError> {
    let inserted = sqlx::query(
        "INSERT INTO webhook_events (id, organization_id, workspace_id, environment, type, body, created_at) \
         VALUES (?, ?, ?, ?, ?, ?, ?) ON CONFLICT (id) DO NOTHING",
    )
    .bind(event.id.uuid())
    .bind(organization_id.uuid())
    .bind(event.workspace_id.map(|id| id.uuid()))
    .bind(event.environment.as_str())
    .bind(&event.kind)
    .bind(&event.body)
    .bind(event.created_at.millis())
    .execute(&mut **transaction)
    .await
    .map_err(failed("insert_event"))?;
    if inserted.rows_affected() == 1 {
        return Ok(true);
    }
    // Recorded before: in this organization, or the ID is someone else's.
    let here = sqlx::query("SELECT 1 FROM webhook_events WHERE organization_id = ? AND id = ?")
        .bind(organization_id.uuid())
        .bind(event.id.uuid())
        .fetch_optional(&mut **transaction)
        .await
        .map_err(failed("insert_event"))?;
    if here.is_none() {
        tracing::error!(event = "webhooks.event_id_taken", %organization_id);
        return Err(WebhookError::Unavailable);
    }
    Ok(false)
}

#[async_trait]
impl WebhookStore for SqliteWebhookStore {
    async fn list_endpoints(
        &self,
        organization_id: OrganizationId,
        environment: Option<Environment>,
    ) -> Result<Vec<WebhookEndpoint>, WebhookError> {
        let rows = sqlx::query(concat!(
            select_endpoint!(),
            " WHERE organization_id = ? AND (? IS NULL OR environment = ?) ORDER BY created_at, id"
        ))
        .bind(organization_id.uuid())
        .bind(environment.map(Environment::as_str))
        .bind(environment.map(Environment::as_str))
        .fetch_all(self.database.pool())
        .await
        .map_err(failed("list_endpoints"))?;
        rows.iter().map(endpoint_from).collect()
    }

    async fn find_endpoint(
        &self,
        organization_id: OrganizationId,
        endpoint_id: WebhookEndpointId,
    ) -> Result<Option<WebhookEndpoint>, WebhookError> {
        let row = sqlx::query(concat!(select_endpoint!(), " WHERE organization_id = ? AND id = ?"))
            .bind(organization_id.uuid())
            .bind(endpoint_id.uuid())
            .fetch_optional(self.database.pool())
            .await
            .map_err(failed("find_endpoint"))?;
        row.as_ref().map(endpoint_from).transpose()
    }

    async fn create_endpoint(
        &self,
        organization_id: OrganizationId,
        endpoint_id: WebhookEndpointId,
        endpoint: &NewEndpoint,
        now: Timestamp,
        audit: &Audit<'_, WebhookEndpoint>,
    ) -> Result<WebhookEndpoint, WebhookError> {
        let mut transaction = self.immediate("create_endpoint").await?;
        sqlx::query(
            "INSERT INTO webhook_endpoints (id, organization_id, environment, url, description, events, enabled, \
             secret_version, secret_key_id, secret_wrapped_key, secret_nonce, secret_ciphertext, secret_last4, \
             created_at) VALUES (?, ?, ?, ?, ?, ?, 1, ?, ?, ?, ?, ?, ?, ?)",
        )
        .bind(endpoint_id.uuid())
        .bind(organization_id.uuid())
        .bind(endpoint.environment.as_str())
        .bind(&endpoint.url)
        .bind(&endpoint.description)
        .bind(events_json(&endpoint.events))
        .bind(SECRET_VERSION)
        .bind(&endpoint.secret.wrapped_key.key_id)
        .bind(endpoint.secret.wrapped_key.bytes.as_slice())
        .bind(endpoint.secret.nonce.as_slice())
        .bind(endpoint.secret.ciphertext.as_slice())
        .bind(&endpoint.secret_last4)
        .bind(now.millis())
        .execute(&mut *transaction)
        .await
        .map_err(failed("create_endpoint"))?;
        let row = sqlx::query(concat!(select_endpoint!(), " WHERE organization_id = ? AND id = ?"))
            .bind(organization_id.uuid())
            .bind(endpoint_id.uuid())
            .fetch_one(&mut *transaction)
            .await
            .map_err(failed("create_endpoint"))?;
        let created = endpoint_from(&row)?;
        crate::audit::record(&mut transaction, organization_id, &audit(&created))
            .await
            .map_err(failed("audit"))?;
        transaction.commit().await.map_err(failed("create_endpoint"))?;
        Ok(created)
    }

    async fn update_endpoint(
        &self,
        organization_id: OrganizationId,
        endpoint_id: WebhookEndpointId,
        changes: &EndpointChanges,
        audit: &Audit<'_, WebhookEndpoint>,
    ) -> Result<WebhookEndpoint, WebhookError> {
        let mut transaction = self.immediate("update_endpoint").await?;
        let updated = sqlx::query(
            "UPDATE webhook_endpoints SET url = coalesce(?, url), description = coalesce(?, description), \
             events = coalesce(?, events), \
             failing_since = CASE WHEN ? = 1 AND enabled = 0 THEN NULL ELSE failing_since END, \
             enabled = coalesce(?, enabled) WHERE organization_id = ? AND id = ?",
        )
        .bind(changes.url.as_deref())
        .bind(changes.description.as_deref())
        .bind(changes.events.as_deref().map(events_json))
        .bind(changes.enabled.map(i64::from))
        .bind(changes.enabled.map(i64::from))
        .bind(organization_id.uuid())
        .bind(endpoint_id.uuid())
        .execute(&mut *transaction)
        .await
        .map_err(failed("update_endpoint"))?;
        if updated.rows_affected() == 0 {
            return Err(WebhookError::NotFound);
        }
        let row = sqlx::query(concat!(select_endpoint!(), " WHERE organization_id = ? AND id = ?"))
            .bind(organization_id.uuid())
            .bind(endpoint_id.uuid())
            .fetch_optional(&mut *transaction)
            .await
            .map_err(failed("update_endpoint"))?;
        let endpoint = endpoint_from(&row.ok_or(WebhookError::NotFound)?)?;
        crate::audit::record(&mut transaction, organization_id, &audit(&endpoint))
            .await
            .map_err(failed("audit"))?;
        transaction.commit().await.map_err(failed("update_endpoint"))?;
        Ok(endpoint)
    }

    async fn delete_endpoint(
        &self,
        organization_id: OrganizationId,
        endpoint_id: WebhookEndpointId,
        audit: &Audit<'_, WebhookEndpoint>,
    ) -> Result<(), WebhookError> {
        let mut transaction = self.immediate("delete_endpoint").await?;
        let row = sqlx::query(concat!(select_endpoint!(), " WHERE organization_id = ? AND id = ?"))
            .bind(organization_id.uuid())
            .bind(endpoint_id.uuid())
            .fetch_optional(&mut *transaction)
            .await
            .map_err(failed("delete_endpoint"))?;
        let endpoint = endpoint_from(&row.ok_or(WebhookError::NotFound)?)?;
        // Its deliveries go with it, by their cascading key.
        sqlx::query("DELETE FROM webhook_endpoints WHERE organization_id = ? AND id = ?")
            .bind(organization_id.uuid())
            .bind(endpoint_id.uuid())
            .execute(&mut *transaction)
            .await
            .map_err(failed("delete_endpoint"))?;
        crate::audit::record(&mut transaction, organization_id, &audit(&endpoint))
            .await
            .map_err(failed("audit"))?;
        transaction.commit().await.map_err(failed("delete_endpoint"))?;
        Ok(())
    }

    async fn signing_secret(
        &self,
        organization_id: OrganizationId,
        endpoint_id: WebhookEndpointId,
    ) -> Result<Option<SigningSecret>, WebhookError> {
        let row = sqlx::query(
            "SELECT environment, secret_version, secret_key_id, secret_wrapped_key, secret_nonce, secret_ciphertext \
             FROM webhook_endpoints WHERE organization_id = ? AND id = ?",
        )
        .bind(organization_id.uuid())
        .bind(endpoint_id.uuid())
        .fetch_optional(self.database.pool())
        .await
        .map_err(failed("signing_secret"))?;
        let Some(row) = row else { return Ok(None) };
        let environment: String = row.try_get("environment").map_err(read)?;
        let nonce: Vec<u8> = row.try_get("secret_nonce").map_err(read)?;
        Ok(Some(SigningSecret {
            environment: Environment::parse(&environment).ok_or_else(|| read("an unknown environment"))?,
            version: u32::try_from(row.try_get::<i64, _>("secret_version").map_err(read)?).map_err(read)?,
            sealed: Sealed {
                wrapped_key: WrappedKey {
                    key_id: row.try_get("secret_key_id").map_err(read)?,
                    bytes: row.try_get("secret_wrapped_key").map_err(read)?,
                },
                nonce: nonce.try_into().map_err(|_| read("a nonce of the wrong length"))?,
                ciphertext: row.try_get("secret_ciphertext").map_err(read)?,
            },
        }))
    }

    async fn record_event(
        &self,
        organization_id: OrganizationId,
        event: &NewWebhookEvent,
    ) -> Result<Vec<WebhookDeliveryId>, WebhookError> {
        let mut transaction = self.immediate("record_event").await?;
        if let Some(workspace_id) = event.workspace_id {
            // An event is its workspace's environment's, never the other one's.
            let here = sqlx::query("SELECT 1 FROM workspaces WHERE organization_id = ? AND id = ? AND environment = ?")
                .bind(organization_id.uuid())
                .bind(workspace_id.uuid())
                .bind(event.environment.as_str())
                .fetch_optional(&mut *transaction)
                .await
                .map_err(failed("record_event"))?;
            if here.is_none() {
                return Err(WebhookError::NotFound);
            }
        }
        if !insert_event(&mut transaction, organization_id, event).await? {
            // Recorded before, with its deliveries.
            transaction.commit().await.map_err(failed("record_event"))?;
            return Ok(Vec::new());
        }
        let endpoints = sqlx::query(
            "SELECT id, events FROM webhook_endpoints WHERE organization_id = ? AND environment = ? AND enabled = 1 \
             ORDER BY created_at, id",
        )
        .bind(organization_id.uuid())
        .bind(event.environment.as_str())
        .fetch_all(&mut *transaction)
        .await
        .map_err(failed("record_event"))?;
        let kind = WebhookEventType::parse(&event.kind);
        let mut deliveries = Vec::new();
        for row in &endpoints {
            let events: String = row.try_get("events").map_err(read)?;
            if kind.is_none_or(|kind| !events_from(&events).is_ok_and(|events| events.contains(&kind))) {
                continue;
            }
            let endpoint_id = WebhookEndpointId::from_uuid(row.try_get("id").map_err(read)?);
            let delivery_id = WebhookDeliveryId::generate();
            sqlx::query(
                "INSERT INTO webhook_deliveries (id, organization_id, endpoint_id, event_id, state, attempts, \
                 next_attempt_at, created_at) VALUES (?, ?, ?, ?, 'pending', 0, ?, ?)",
            )
            .bind(delivery_id.uuid())
            .bind(organization_id.uuid())
            .bind(endpoint_id.uuid())
            .bind(event.id.uuid())
            .bind(Timestamp::now().millis())
            .bind(Timestamp::now().millis())
            .execute(&mut *transaction)
            .await
            .map_err(failed("record_event"))?;
            let Some(job) = delivery_job(delivery_id) else {
                return Err(WebhookError::Unavailable);
            };
            insert_job(
                &mut *transaction,
                &Scope::Organization(organization_id),
                &job.kind,
                job.payload,
                job.options,
            )
            .await
            .map_err(|error| {
                tracing::error!(event = "webhooks.job_not_queued", %error);
                WebhookError::Unavailable
            })?;
            deliveries.push(delivery_id);
        }
        transaction.commit().await.map_err(failed("record_event"))?;
        Ok(deliveries)
    }

    async fn record_test(
        &self,
        organization_id: OrganizationId,
        endpoint_id: WebhookEndpointId,
        event: &NewWebhookEvent,
        attempt: &Attempt,
    ) -> Result<WebhookDeliveryId, WebhookError> {
        let mut transaction = self.immediate("record_test").await?;
        let here = sqlx::query("SELECT 1 FROM webhook_endpoints WHERE organization_id = ? AND id = ?")
            .bind(organization_id.uuid())
            .bind(endpoint_id.uuid())
            .fetch_optional(&mut *transaction)
            .await
            .map_err(failed("record_test"))?;
        if here.is_none() {
            return Err(WebhookError::NotFound);
        }
        insert_event(&mut transaction, organization_id, event).await?;
        let delivery_id = WebhookDeliveryId::generate();
        let state = if attempt.succeeded() {
            DeliveryState::Succeeded
        } else {
            DeliveryState::Failed
        };
        sqlx::query(
            "INSERT INTO webhook_deliveries (id, organization_id, endpoint_id, event_id, state, attempts, \
             last_status_code, last_error, last_attempt_at, created_at) VALUES (?, ?, ?, ?, ?, 1, ?, ?, ?, ?)",
        )
        .bind(delivery_id.uuid())
        .bind(organization_id.uuid())
        .bind(endpoint_id.uuid())
        .bind(event.id.uuid())
        .bind(state.as_str())
        .bind(attempt.status)
        .bind(attempt.error.as_deref())
        .bind(attempt.at.millis())
        .bind(attempt.at.millis())
        .execute(&mut *transaction)
        .await
        .map_err(failed("record_test"))?;
        // A success is a successful delivery, so the endpoint isn't failing
        // any more; a failure is only the test's, so it starts no count.
        sqlx::query(
            "UPDATE webhook_endpoints SET last_status_code = ?, last_delivered_at = ?, \
             failing_since = CASE WHEN ? THEN NULL ELSE failing_since END \
             WHERE organization_id = ? AND id = ?",
        )
        .bind(attempt.status.unwrap_or(0))
        .bind(attempt.at.millis())
        .bind(attempt.succeeded())
        .bind(organization_id.uuid())
        .bind(endpoint_id.uuid())
        .execute(&mut *transaction)
        .await
        .map_err(failed("record_test"))?;
        transaction.commit().await.map_err(failed("record_test"))?;
        Ok(delivery_id)
    }

    async fn find_delivery(
        &self,
        organization_id: OrganizationId,
        delivery_id: WebhookDeliveryId,
    ) -> Result<Option<PendingDelivery>, WebhookError> {
        let row = sqlx::query(
            "SELECT d.id, d.endpoint_id, d.event_id, d.state, d.attempts, e.type, e.body, p.url, p.enabled, \
             p.events FROM webhook_deliveries d \
             JOIN webhook_events e ON e.organization_id = d.organization_id AND e.id = d.event_id \
             JOIN webhook_endpoints p ON p.organization_id = d.organization_id AND p.id = d.endpoint_id \
             WHERE d.organization_id = ? AND e.organization_id = ? AND p.organization_id = ? AND d.id = ?",
        )
        .bind(organization_id.uuid())
        .bind(organization_id.uuid())
        .bind(organization_id.uuid())
        .bind(delivery_id.uuid())
        .fetch_optional(self.database.pool())
        .await
        .map_err(failed("find_delivery"))?;
        let Some(row) = row else { return Ok(None) };
        let kind: String = row.try_get("type").map_err(read)?;
        let state: String = row.try_get("state").map_err(read)?;
        let events: String = row.try_get("events").map_err(read)?;
        let subscribed = WebhookEventType::parse(&kind).is_some_and(|kind| {
            events_from(&events)
                .map(|events| events.contains(&kind))
                .unwrap_or(false)
        });
        Ok(Some(PendingDelivery {
            id: WebhookDeliveryId::from_uuid(row.try_get("id").map_err(read)?),
            organization_id,
            endpoint_id: WebhookEndpointId::from_uuid(row.try_get("endpoint_id").map_err(read)?),
            event_id: WebhookEventId::from_uuid(row.try_get("event_id").map_err(read)?),
            kind,
            body: row.try_get("body").map_err(read)?,
            state: DeliveryState::parse(&state).ok_or_else(|| read("an unknown delivery state"))?,
            attempts: u32::try_from(row.try_get::<i64, _>("attempts").map_err(read)?).map_err(read)?,
            url: row.try_get("url").map_err(read)?,
            enabled: row.try_get::<i64, _>("enabled").map_err(read)? == 1,
            subscribed,
        }))
    }

    async fn record_attempt(
        &self,
        organization_id: OrganizationId,
        delivery_id: WebhookDeliveryId,
        attempt: &Attempt,
    ) -> Result<(), WebhookError> {
        let mut transaction = self.immediate("record_attempt").await?;
        let endpoint: Option<uuid::Uuid> = sqlx::query_scalar(
            "UPDATE webhook_deliveries SET state = ?, attempts = ?, last_status_code = ?, last_error = ?, \
             last_attempt_at = ?, next_attempt_at = ? \
             WHERE organization_id = ? AND id = ? AND state = 'pending' RETURNING endpoint_id",
        )
        .bind(attempt.state.as_str())
        .bind(attempt.number)
        .bind(attempt.status)
        .bind(attempt.error.as_deref())
        .bind(attempt.at.millis())
        .bind(attempt.next_attempt_at.map(Timestamp::millis))
        .bind(organization_id.uuid())
        .bind(delivery_id.uuid())
        .fetch_optional(&mut *transaction)
        .await
        .map_err(failed("record_attempt"))?;
        let Some(endpoint) = endpoint else {
            return Err(WebhookError::NotFound);
        };
        let at = attempt.at.millis();
        let query = if attempt.succeeded() {
            sqlx::query(
                "UPDATE webhook_endpoints SET last_status_code = ?, last_delivered_at = ?, failing_since = NULL \
                 WHERE organization_id = ? AND id = ?",
            )
            .bind(attempt.status.unwrap_or(0))
            .bind(at)
        } else {
            // Failing since the first failure after the last success; failing
            // for too long, it's disabled until someone enables it again.
            sqlx::query(
                "UPDATE webhook_endpoints SET last_status_code = ?, last_delivered_at = ?, \
                 failing_since = coalesce(failing_since, ?), \
                 enabled = CASE WHEN ? - coalesce(failing_since, ?) >= ? THEN 0 ELSE enabled END \
                 WHERE organization_id = ? AND id = ?",
            )
            .bind(attempt.status.unwrap_or(0))
            .bind(at)
            .bind(at)
            .bind(at)
            .bind(at)
            .bind(duration_millis(DISABLE_AFTER))
        };
        query
            .bind(organization_id.uuid())
            .bind(endpoint)
            .execute(&mut *transaction)
            .await
            .map_err(failed("record_attempt"))?;
        transaction.commit().await.map_err(failed("record_attempt"))?;
        Ok(())
    }

    async fn skip_delivery(
        &self,
        organization_id: OrganizationId,
        delivery_id: WebhookDeliveryId,
    ) -> Result<(), WebhookError> {
        let skipped = sqlx::query(
            "UPDATE webhook_deliveries SET state = 'skipped', next_attempt_at = NULL \
             WHERE organization_id = ? AND id = ? AND state = 'pending'",
        )
        .bind(organization_id.uuid())
        .bind(delivery_id.uuid())
        .execute(self.database.pool())
        .await
        .map_err(failed("skip_delivery"))?;
        if skipped.rows_affected() == 0 {
            return Err(WebhookError::NotFound);
        }
        Ok(())
    }

    async fn abandon_delivery(
        &self,
        organization_id: OrganizationId,
        delivery_id: WebhookDeliveryId,
        reason: &str,
    ) -> Result<(), WebhookError> {
        // The endpoint's row is left alone: this isn't its failure.
        let abandoned = sqlx::query(
            "UPDATE webhook_deliveries SET state = 'failed', last_error = ?, next_attempt_at = NULL \
             WHERE organization_id = ? AND id = ? AND state = 'pending'",
        )
        .bind(reason)
        .bind(organization_id.uuid())
        .bind(delivery_id.uuid())
        .execute(self.database.pool())
        .await
        .map_err(failed("abandon_delivery"))?;
        if abandoned.rows_affected() == 0 {
            return Err(WebhookError::NotFound);
        }
        Ok(())
    }

    async fn hold_endpoint(
        &self,
        organization_id: OrganizationId,
        endpoint_id: WebhookEndpointId,
        holder: &LeaseToken,
        now: Timestamp,
        until: Timestamp,
    ) -> Result<bool, WebhookError> {
        let mut transaction = self.immediate("hold_endpoint").await?;
        let held = sqlx::query(
            "UPDATE webhook_endpoints SET delivery_holder = ?, delivery_held_until = ? \
             WHERE organization_id = ? AND id = ? \
               AND (delivery_holder IS NULL OR delivery_held_until <= ? OR delivery_holder = ?)",
        )
        .bind(holder.as_bytes().to_vec())
        .bind(until.millis())
        .bind(organization_id.uuid())
        .bind(endpoint_id.uuid())
        .bind(now.millis())
        .bind(holder.as_bytes().to_vec())
        .execute(&mut *transaction)
        .await
        .map_err(failed("hold_endpoint"))?;
        if held.rows_affected() == 1 {
            transaction.commit().await.map_err(failed("hold_endpoint"))?;
            return Ok(true);
        }
        let here = sqlx::query("SELECT 1 FROM webhook_endpoints WHERE organization_id = ? AND id = ?")
            .bind(organization_id.uuid())
            .bind(endpoint_id.uuid())
            .fetch_optional(&mut *transaction)
            .await
            .map_err(failed("hold_endpoint"))?;
        transaction.commit().await.map_err(failed("hold_endpoint"))?;
        match here {
            Some(_) => Ok(false),
            None => Err(WebhookError::NotFound),
        }
    }

    async fn release_endpoint(
        &self,
        organization_id: OrganizationId,
        endpoint_id: WebhookEndpointId,
        holder: &LeaseToken,
    ) -> Result<(), WebhookError> {
        sqlx::query(
            "UPDATE webhook_endpoints SET delivery_holder = NULL, delivery_held_until = NULL \
             WHERE organization_id = ? AND id = ? AND delivery_holder = ?",
        )
        .bind(organization_id.uuid())
        .bind(endpoint_id.uuid())
        .bind(holder.as_bytes().to_vec())
        .execute(self.database.pool())
        .await
        .map_err(failed("release_endpoint"))?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use std::sync::Arc;

    use zupflow_conformance::webhook_store::{Prepare, Stores, Tenant};

    use super::*;
    use crate::SqliteJobQueue;
    use crate::schema::{Temporary, temporary};

    /// Makes the tenant's rows straight in the database: an organization
    /// with a Sandbox and a Production workspace.
    fn prepare(database: Database) -> Prepare {
        Arc::new(move |tenant: Tenant| {
            let database = database.clone();
            Box::pin(async move {
                let pool = database.pool();
                let org = tenant.organization_id.uuid();
                sqlx::query("INSERT INTO organizations (id, name, slug, created_at) VALUES (?, 'Test', ?, 0)")
                    .bind(org)
                    .bind(format!("org-{}", org.simple()))
                    .execute(pool)
                    .await
                    .unwrap();
                for (workspace, environment) in [(tenant.sandbox, "sandbox"), (tenant.production, "production")] {
                    sqlx::query(
                        "INSERT INTO workspaces (id, organization_id, environment, name, slug, created_at) \
                         VALUES (?, ?, ?, 'W', ?, 0)",
                    )
                    .bind(workspace.uuid())
                    .bind(org)
                    .bind(environment)
                    .bind(format!("w-{}", workspace.uuid().simple()))
                    .execute(pool)
                    .await
                    .unwrap();
                }
            })
        })
    }

    #[tokio::test]
    async fn conforms() {
        let Temporary {
            database,
            directory: _directory,
        } = temporary().await;
        zupflow_conformance::webhook_store::run(
            Stores {
                webhooks: &SqliteWebhookStore::new(database.clone()),
                jobs: &SqliteJobQueue::new(database.clone()),
            },
            prepare(database),
        )
        .await;
    }

    /// The database itself keeps an event's workspace in its organization,
    /// and deleting the workspace takes its events and their deliveries.
    #[tokio::test]
    async fn deletes_a_workspaces_events_with_it() {
        use zupflow_core::ids::{OrganizationId, WorkspaceId};
        use zupflow_core::webhooks::WebhookEventType;
        use zupflow_core::workspaces::WorkspaceStore;

        let Temporary {
            database,
            directory: _directory,
        } = temporary().await;
        let prepare = prepare(database.clone());
        let a = Tenant {
            organization_id: OrganizationId::generate(),
            sandbox: WorkspaceId::generate(),
            production: WorkspaceId::generate(),
        };
        let b = Tenant {
            organization_id: OrganizationId::generate(),
            sandbox: WorkspaceId::generate(),
            production: WorkspaceId::generate(),
        };
        prepare(a).await;
        prepare(b).await;
        let store = SqliteWebhookStore::new(database.clone());
        let sealed = Sealed {
            wrapped_key: WrappedKey {
                key_id: "k".to_owned(),
                bytes: vec![1; 8],
            },
            nonce: [0; 12],
            ciphertext: vec![2; 8],
        };
        let hook = WebhookEndpointId::generate();
        store
            .create_endpoint(
                a.organization_id,
                hook,
                &NewEndpoint {
                    environment: Environment::Sandbox,
                    url: "https://hooks.example/a".to_owned(),
                    description: String::new(),
                    events: vec![WebhookEventType::RunCompleted],
                    secret: sealed,
                    secret_last4: "abcd".to_owned(),
                },
                Timestamp::now(),
                &zupflow_conformance::audit_store::by_suite,
            )
            .await
            .unwrap();
        let event = |workspace: WorkspaceId| NewWebhookEvent {
            id: WebhookEventId::generate(),
            environment: Environment::Sandbox,
            workspace_id: Some(workspace),
            kind: "run.completed".to_owned(),
            body: "{}".to_owned(),
            created_at: Timestamp::now(),
        };

        // Another organization's workspace can't be named, even with an existing ID.
        let foreign = sqlx::query(
            "INSERT INTO webhook_events (id, organization_id, workspace_id, environment, type, body, created_at) \
             VALUES (?, ?, ?, 'sandbox', 'run.completed', '{}', 0)",
        )
        .bind(WebhookEventId::generate().uuid())
        .bind(a.organization_id.uuid())
        .bind(b.sandbox.uuid())
        .execute(database.pool())
        .await;
        assert!(foreign.is_err(), "an event's workspace is in its organization");

        let delivery = store.record_event(a.organization_id, &event(a.sandbox)).await.unwrap()[0];
        crate::SqliteWorkspaceStore::new(database.clone())
            .delete_workspace(a.organization_id, a.sandbox, &|_| {
                zupflow_conformance::audit_store::by_suite_in(a.sandbox)
            })
            .await
            .unwrap();
        assert!(
            store
                .find_delivery(a.organization_id, delivery)
                .await
                .unwrap()
                .is_none()
        );
        assert!(
            store.find_endpoint(a.organization_id, hook).await.unwrap().is_some(),
            "the endpoint stays"
        );
    }
}
