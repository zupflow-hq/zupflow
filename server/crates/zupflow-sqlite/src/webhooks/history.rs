//! Read-only delivery history. Every query repeats its organization,
//! endpoint and environment, including the joins and attempt counts.

use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use serde_json::{Value, json};
use sqlx::sqlite::SqliteRow;
use sqlx::{QueryBuilder, Row, Sqlite, Transaction};
use uuid::Uuid;
use zupflow_core::environment::Environment;
use zupflow_core::ids::{OrganizationId, WebhookDeliveryId, WebhookEndpointId, WebhookEventId, WorkspaceId};
use zupflow_core::time::Timestamp;
use zupflow_core::webhooks::{
    DeliveryDetail, DeliveryFilter, DeliveryState, DeliverySummary, EventWorkspace, MAX_DELIVERY_PAGE, RecordedAttempt,
    TEST_EVENT, WebhookError, WebhookEventType, safe_delivery_error,
};
use zupflow_core::workspaces::Page;

use super::{failed, read};
use crate::Database;

const JOINS: &str = " FROM webhook_deliveries d \
    JOIN webhook_endpoints p ON p.organization_id = d.organization_id AND p.id = d.endpoint_id \
    JOIN webhook_events e ON e.organization_id = d.organization_id AND e.id = d.event_id \
    LEFT JOIN workspaces w ON w.organization_id = e.organization_id AND w.id = e.workspace_id \
        AND w.environment = e.environment";

const COLUMNS: &str = "SELECT d.id, d.organization_id, d.endpoint_id, d.event_id, d.state, d.attempts, \
    d.last_status_code, d.last_error, d.last_attempt_at, d.next_attempt_at, d.created_at, \
    e.environment, e.workspace_id, e.type, e.created_at AS event_created_at, e.body";

fn scope(
    query: &mut QueryBuilder<Sqlite>,
    organization_id: OrganizationId,
    endpoint_id: WebhookEndpointId,
    environment: Environment,
) {
    query
        .push(" WHERE d.organization_id = ")
        .push_bind(organization_id.uuid());
    query
        .push(" AND p.organization_id = ")
        .push_bind(organization_id.uuid());
    query
        .push(" AND e.organization_id = ")
        .push_bind(organization_id.uuid());
    query.push(" AND d.endpoint_id = ").push_bind(endpoint_id.uuid());
    query.push(" AND p.id = ").push_bind(endpoint_id.uuid());
    query.push(" AND p.environment = ").push_bind(environment.as_str());
    query.push(" AND e.environment = ").push_bind(environment.as_str());
    query.push(" AND (e.workspace_id IS NULL OR (w.organization_id = ");
    query.push_bind(organization_id.uuid());
    query.push(" AND w.id = e.workspace_id AND w.environment = ");
    query.push_bind(environment.as_str()).push("))");
}

fn filter(query: &mut QueryBuilder<Sqlite>, filter: &DeliveryFilter) {
    if let Some(workspace) = filter.workspace_id {
        query.push(" AND e.workspace_id = ").push_bind(workspace.uuid());
    }
    if let Some(state) = filter.state {
        query.push(" AND d.state = ").push_bind(state.as_str());
    }
    if let Some(kind) = &filter.event_type {
        query.push(" AND e.type = ").push_bind(kind.clone());
    }
    if filter.matches_nothing {
        query.push(" AND 0 = 1");
    }
}

async fn require_endpoint(
    transaction: &mut Transaction<'_, Sqlite>,
    organization_id: OrganizationId,
    endpoint_id: WebhookEndpointId,
    environment: Environment,
) -> Result<(), WebhookError> {
    let found = sqlx::query("SELECT 1 FROM webhook_endpoints WHERE organization_id = ? AND id = ? AND environment = ?")
        .bind(organization_id.uuid())
        .bind(endpoint_id.uuid())
        .bind(environment.as_str())
        .fetch_optional(&mut **transaction)
        .await
        .map_err(failed("history_endpoint"))?;
    found.map(|_| ()).ok_or(WebhookError::NotFound)
}

fn cursor_scope(
    organization_id: OrganizationId,
    endpoint_id: WebhookEndpointId,
    environment: Environment,
    filter: &DeliveryFilter,
) -> Value {
    json!([
        organization_id,
        endpoint_id,
        environment,
        filter.workspace_id,
        filter.state,
        filter.event_type,
        filter.matches_nothing
    ])
}

fn encode_cursor(scope: &Value, position: Value) -> String {
    URL_SAFE_NO_PAD.encode(json!({ "scope": scope, "position": position }).to_string())
}

fn decode_cursor(cursor: &str, scope: &Value) -> Result<Value, WebhookError> {
    if cursor.len() > 8192 {
        return Err(WebhookError::InvalidCursor);
    }
    let raw = URL_SAFE_NO_PAD
        .decode(cursor)
        .map_err(|_| WebhookError::InvalidCursor)?;
    let cursor: Value = serde_json::from_slice(&raw).map_err(|_| WebhookError::InvalidCursor)?;
    if cursor.as_object().is_none_or(|object| object.len() != 2)
        || cursor.get("scope") != Some(scope)
        || cursor.get("position").is_none()
    {
        return Err(WebhookError::InvalidCursor);
    }
    Ok(cursor["position"].clone())
}

fn delivery_position(position: Value) -> Result<(i64, WebhookDeliveryId), WebhookError> {
    let items = position
        .as_array()
        .filter(|items| items.len() == 2)
        .ok_or(WebhookError::InvalidCursor)?;
    let at = items[0].as_i64().ok_or(WebhookError::InvalidCursor)?;
    let id = items[1].as_str().ok_or(WebhookError::InvalidCursor)?;
    Ok((
        at,
        WebhookDeliveryId::parse(id).map_err(|_| WebhookError::InvalidCursor)?,
    ))
}

fn summary(row: &SqliteRow) -> Result<(DeliverySummary, Value), WebhookError> {
    let event_id = WebhookEventId::from_uuid(row.try_get("event_id").map_err(read)?);
    let organization_id = OrganizationId::from_uuid(row.try_get("organization_id").map_err(read)?);
    let environment: String = row.try_get("environment").map_err(read)?;
    let environment = Environment::parse(&environment).ok_or_else(|| read("an unknown environment"))?;
    let kind: String = row.try_get("type").map_err(read)?;
    if kind != TEST_EVENT && WebhookEventType::parse(&kind).is_none() {
        return Err(read("an unknown webhook event type"));
    }
    let event_created_at = Timestamp::from_millis(row.try_get("event_created_at").map_err(read)?);
    let workspace_id = row
        .try_get::<Option<Uuid>, _>("workspace_id")
        .map_err(read)?
        .map(WorkspaceId::from_uuid);
    let raw: String = row.try_get("body").map_err(read)?;
    let event: Value = serde_json::from_str(&raw).map_err(|_| read("invalid stored webhook JSON"))?;
    // A scoped row with contradictory JSON is corrupt, not an opportunity
    // to expose whichever tenant the JSON happens to name.
    if event["id"] != json!(event_id)
        || event["organization_id"] != json!(organization_id)
        || event["environment"] != json!(environment)
        || event["type"] != json!(kind)
        || event["created_at"] != json!(event_created_at)
    {
        return Err(read("a stored webhook envelope contradicts its row"));
    }
    let workspace = match workspace_id {
        Some(id) => {
            if event["workspace"]["id"] != json!(id) {
                return Err(read("a stored webhook workspace contradicts its row"));
            }
            let external_id = match event["workspace"].get("external_id") {
                Some(Value::Null) => None,
                Some(Value::String(value)) => Some(value.clone()),
                _ => return Err(read("invalid stored webhook workspace")),
            };
            Some(EventWorkspace { id, external_id })
        }
        None if event.get("workspace") == Some(&Value::Null) => None,
        None => return Err(read("a stored webhook workspace contradicts its row")),
    };
    let state: String = row.try_get("state").map_err(read)?;
    let last_error: Option<String> = row.try_get("last_error").map_err(read)?;
    Ok((
        DeliverySummary {
            id: WebhookDeliveryId::from_uuid(row.try_get("id").map_err(read)?),
            organization_id,
            endpoint_id: WebhookEndpointId::from_uuid(row.try_get("endpoint_id").map_err(read)?),
            event_id,
            environment,
            event_type: kind,
            event_created_at,
            workspace,
            state: DeliveryState::parse(&state).ok_or_else(|| read("an unknown delivery state"))?,
            attempts: u32::try_from(row.try_get::<i64, _>("attempts").map_err(read)?).map_err(read)?,
            last_status_code: row
                .try_get::<Option<i64>, _>("last_status_code")
                .map_err(read)?
                .map(|status| u16::try_from(status).map_err(read))
                .transpose()?,
            last_error: last_error.map(|error| safe_delivery_error(&error).to_owned()),
            last_attempt_at: row
                .try_get::<Option<i64>, _>("last_attempt_at")
                .map_err(read)?
                .map(Timestamp::from_millis),
            next_attempt_at: row
                .try_get::<Option<i64>, _>("next_attempt_at")
                .map_err(read)?
                .map(Timestamp::from_millis),
            created_at: Timestamp::from_millis(row.try_get("created_at").map_err(read)?),
        },
        event,
    ))
}

#[allow(clippy::too_many_arguments)]
pub(super) async fn list_deliveries(
    database: &Database,
    organization_id: OrganizationId,
    endpoint_id: WebhookEndpointId,
    environment: Environment,
    filters: &DeliveryFilter,
    cursor: Option<&str>,
    limit: u32,
) -> Result<Page<DeliverySummary>, WebhookError> {
    let mut transaction = database.pool().begin().await.map_err(failed("list_deliveries"))?;
    require_endpoint(&mut transaction, organization_id, endpoint_id, environment).await?;
    let cursor_scope = cursor_scope(organization_id, endpoint_id, environment, filters);
    let position = cursor
        .map(|cursor| decode_cursor(cursor, &cursor_scope).and_then(delivery_position))
        .transpose()?;
    let limit = limit.clamp(1, MAX_DELIVERY_PAGE);
    let mut count = QueryBuilder::<Sqlite>::new("SELECT count(*)");
    count.push(JOINS);
    scope(&mut count, organization_id, endpoint_id, environment);
    filter(&mut count, filters);
    let total: i64 = count
        .build_query_scalar()
        .fetch_one(&mut *transaction)
        .await
        .map_err(failed("count_deliveries"))?;
    let mut query = QueryBuilder::<Sqlite>::new(COLUMNS);
    query.push(JOINS);
    scope(&mut query, organization_id, endpoint_id, environment);
    filter(&mut query, filters);
    if let Some((at, id)) = position {
        query.push(" AND (d.created_at < ").push_bind(at);
        query.push(" OR (d.created_at = ").push_bind(at);
        query.push(" AND d.id < ").push_bind(id.uuid()).push("))");
    }
    query
        .push(" ORDER BY d.created_at DESC, d.id DESC LIMIT ")
        .push_bind(i64::from(limit) + 1);
    let rows = query
        .build()
        .fetch_all(&mut *transaction)
        .await
        .map_err(failed("list_deliveries"))?;
    let mut items = rows
        .iter()
        .map(|row| summary(row).map(|(summary, _)| summary))
        .collect::<Result<Vec<_>, _>>()?;
    let more = items.len() > limit as usize;
    items.truncate(limit as usize);
    let next_cursor = more
        .then(|| {
            items
                .last()
                .map(|last| encode_cursor(&cursor_scope, json!([last.created_at.millis(), last.id])))
        })
        .flatten();
    transaction.commit().await.map_err(failed("list_deliveries"))?;
    Ok(Page {
        items,
        next_cursor,
        total: u64::try_from(total).map_err(read)?,
    })
}

async fn detail(
    transaction: &mut Transaction<'_, Sqlite>,
    organization_id: OrganizationId,
    endpoint_id: WebhookEndpointId,
    environment: Environment,
    delivery_id: WebhookDeliveryId,
) -> Result<Option<DeliveryDetail>, WebhookError> {
    let mut query = QueryBuilder::<Sqlite>::new(COLUMNS);
    query.push(JOINS);
    scope(&mut query, organization_id, endpoint_id, environment);
    query.push(" AND d.id = ").push_bind(delivery_id.uuid());
    let row = query
        .build()
        .fetch_optional(&mut **transaction)
        .await
        .map_err(failed("delivery_detail"))?;
    let Some(row) = row else { return Ok(None) };
    let (summary, event) = summary(&row)?;
    let mut count = QueryBuilder::<Sqlite>::new("SELECT count(a.id), max(a.number)");
    count.push(JOINS);
    count.push(" JOIN webhook_delivery_attempts a ON a.organization_id = d.organization_id AND a.delivery_id = d.id");
    scope(&mut count, organization_id, endpoint_id, environment);
    count.push(" AND d.id = ").push_bind(delivery_id.uuid());
    count
        .push(" AND a.organization_id = ")
        .push_bind(organization_id.uuid());
    count.push(" AND a.delivery_id = ").push_bind(delivery_id.uuid());
    let (total, last): (i64, Option<i64>) = count
        .build_query_as()
        .fetch_one(&mut **transaction)
        .await
        .map_err(failed("count_delivery_attempts"))?;
    let complete = total == i64::from(summary.attempts) && last.unwrap_or(0) == i64::from(summary.attempts);
    Ok(Some(DeliveryDetail {
        summary,
        event,
        attempt_history_complete: complete,
    }))
}

pub(super) async fn find_delivery_detail(
    database: &Database,
    organization_id: OrganizationId,
    endpoint_id: WebhookEndpointId,
    environment: Environment,
    delivery_id: WebhookDeliveryId,
) -> Result<Option<DeliveryDetail>, WebhookError> {
    let mut transaction = database.pool().begin().await.map_err(failed("delivery_detail"))?;
    let result = detail(&mut transaction, organization_id, endpoint_id, environment, delivery_id).await?;
    transaction.commit().await.map_err(failed("delivery_detail"))?;
    Ok(result)
}

#[allow(clippy::too_many_arguments)]
pub(super) async fn list_delivery_attempts(
    database: &Database,
    organization_id: OrganizationId,
    endpoint_id: WebhookEndpointId,
    environment: Environment,
    delivery_id: WebhookDeliveryId,
    cursor: Option<&str>,
    limit: u32,
) -> Result<Page<RecordedAttempt>, WebhookError> {
    let mut transaction = database
        .pool()
        .begin()
        .await
        .map_err(failed("list_delivery_attempts"))?;
    detail(&mut transaction, organization_id, endpoint_id, environment, delivery_id)
        .await?
        .ok_or(WebhookError::NotFound)?;
    let cursor_scope = json!([organization_id, endpoint_id, environment, delivery_id]);
    let position = cursor
        .map(|cursor| {
            let position = decode_cursor(cursor, &cursor_scope)?;
            position
                .as_u64()
                .filter(|number| *number > 0 && *number <= u64::from(u32::MAX))
                .ok_or(WebhookError::InvalidCursor)
        })
        .transpose()?;
    let limit = limit.clamp(1, MAX_DELIVERY_PAGE);
    let scoped = |query: &mut QueryBuilder<Sqlite>| {
        query.push(JOINS);
        query.push(
            " JOIN webhook_delivery_attempts a ON a.organization_id = d.organization_id AND a.delivery_id = d.id",
        );
        scope(query, organization_id, endpoint_id, environment);
        query.push(" AND d.id = ").push_bind(delivery_id.uuid());
        query
            .push(" AND a.organization_id = ")
            .push_bind(organization_id.uuid());
        query.push(" AND a.delivery_id = ").push_bind(delivery_id.uuid());
    };
    let mut count = QueryBuilder::<Sqlite>::new("SELECT count(a.id)");
    scoped(&mut count);
    let total: i64 = count
        .build_query_scalar()
        .fetch_one(&mut *transaction)
        .await
        .map_err(failed("count_delivery_attempts"))?;
    let mut query =
        QueryBuilder::<Sqlite>::new("SELECT a.number, a.status_code, a.error_class, a.attempted_at, a.duration_ms");
    scoped(&mut query);
    if let Some(after) = position {
        query
            .push(" AND a.number > ")
            .push_bind(i64::try_from(after).map_err(read)?);
    }
    query.push(" ORDER BY a.number LIMIT ").push_bind(i64::from(limit) + 1);
    let rows = query
        .build()
        .fetch_all(&mut *transaction)
        .await
        .map_err(failed("list_delivery_attempts"))?;
    let mut items = rows
        .iter()
        .map(|row| {
            let error: Option<String> = row.try_get("error_class").map_err(read)?;
            Ok(RecordedAttempt {
                number: u32::try_from(row.try_get::<i64, _>("number").map_err(read)?).map_err(read)?,
                status_code: row
                    .try_get::<Option<i64>, _>("status_code")
                    .map_err(read)?
                    .map(|status| u16::try_from(status).map_err(read))
                    .transpose()?,
                error_class: error.map(|error| safe_delivery_error(&error).to_owned()),
                attempted_at: Timestamp::from_millis(row.try_get("attempted_at").map_err(read)?),
                duration_ms: row
                    .try_get::<Option<i64>, _>("duration_ms")
                    .map_err(read)?
                    .map(|duration| u64::try_from(duration).map_err(read))
                    .transpose()?,
            })
        })
        .collect::<Result<Vec<RecordedAttempt>, WebhookError>>()?;
    let more = items.len() > limit as usize;
    items.truncate(limit as usize);
    let next_cursor = more
        .then(|| {
            items
                .last()
                .map(|last| encode_cursor(&cursor_scope, json!(last.number)))
        })
        .flatten();
    transaction.commit().await.map_err(failed("list_delivery_attempts"))?;
    Ok(Page {
        items,
        next_cursor,
        total: u64::try_from(total).map_err(read)?,
    })
}
