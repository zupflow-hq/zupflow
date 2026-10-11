//! Frozen delivery scenarios for the shared API suite and local Console.
//! They have no delivery jobs: starting workers must not change the fixture.

use serde_json::json;
use sqlx::SqlitePool;
use sqlx::types::Uuid;
use zupflow_core::environment::Environment;
use zupflow_core::ids::{
    AgentId, OrganizationId, RunId, WebhookDeliveryId, WebhookEndpointId, WebhookEventId, WorkspaceId,
};
use zupflow_core::time::Timestamp;
use zupflow_core::webhooks::{DeliveryState, TEST_EVENT, event_body};

pub async fn seed(
    pool: &SqlitePool,
    organization_id: OrganizationId,
    environment: Environment,
    endpoint_id: WebhookEndpointId,
    workspaces: &[WorkspaceId],
    now: Timestamp,
) -> Result<(), String> {
    let scenarios = [
        (DeliveryState::Pending, 2, false),
        (DeliveryState::Succeeded, 2, false),
        (DeliveryState::Failed, 8, false),
        (DeliveryState::Skipped, 0, false),
        (DeliveryState::Succeeded, 1, true),
    ];
    if workspaces.is_empty() {
        return Err("webhook history needs a workspace".to_owned());
    }
    let mut transaction = pool.begin().await.map_err(failed)?;
    for (index, (state, count, test)) in scenarios.into_iter().enumerate() {
        let workspace_id = workspaces[index % workspaces.len()];
        let (workspace, subject) = if test {
            (None, json!({"object": "webhook_endpoint", "id": endpoint_id}))
        } else {
            let (run, agent, external_id): (Uuid, Uuid, Option<String>) = sqlx::query_as(
                "SELECT r.id, r.agent_id, w.external_id FROM runs r \
                 JOIN workspaces w ON w.organization_id = r.organization_id AND w.id = r.workspace_id \
                 WHERE r.organization_id = ? AND r.workspace_id = ? \
                 AND w.organization_id = ? AND w.id = ? AND w.environment = ? AND r.status = 'completed' \
                 ORDER BY r.created_at DESC, r.id DESC LIMIT 1",
            )
            .bind(organization_id.uuid())
            .bind(workspace_id.uuid())
            .bind(organization_id.uuid())
            .bind(workspace_id.uuid())
            .bind(environment.as_str())
            .fetch_one(&mut *transaction)
            .await
            .map_err(failed)?;
            (
                Some((workspace_id, external_id)),
                json!({"object": "run", "id": RunId::from_uuid(run), "agent_id": AgentId::from_uuid(agent), "status": "completed"}),
            )
        };
        let event_id = WebhookEventId::generate();
        let delivery_id = WebhookDeliveryId::generate();
        let at = now.millis() - (index as i64 + 1) * 3_600_000;
        let kind = if test { TEST_EVENT } else { "run.completed" };
        let body = event_body(
            event_id,
            kind,
            Timestamp::from_millis(at),
            environment,
            organization_id,
            workspace
                .as_ref()
                .map(|(id, external_id)| (*id, external_id.as_deref())),
            subject,
        );
        sqlx::query(
            "INSERT INTO webhook_events (id, organization_id, workspace_id, environment, type, body, created_at) \
             VALUES (?, ?, ?, ?, ?, ?, ?)",
        )
        .bind(event_id.uuid())
        .bind(organization_id.uuid())
        .bind(workspace.as_ref().map(|(id, _)| id.uuid()))
        .bind(environment.as_str())
        .bind(kind)
        .bind(body)
        .bind(at)
        .execute(&mut *transaction)
        .await
        .map_err(failed)?;
        let succeeded = state == DeliveryState::Succeeded;
        let status = (count > 0).then_some(if succeeded { 200 } else { 503 });
        let error = (count > 0 && !succeeded).then_some("status");
        sqlx::query(
            "INSERT INTO webhook_deliveries (id, organization_id, endpoint_id, event_id, state, attempts, \
             last_status_code, last_error, last_attempt_at, next_attempt_at, created_at) \
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
        )
        .bind(delivery_id.uuid())
        .bind(organization_id.uuid())
        .bind(endpoint_id.uuid())
        .bind(event_id.uuid())
        .bind(state.as_str())
        .bind(count)
        .bind(status)
        .bind(error)
        .bind((count > 0).then_some(at + i64::from(count) * 1000))
        .bind((state == DeliveryState::Pending).then_some(now.millis() + 60_000))
        .bind(at)
        .execute(&mut *transaction)
        .await
        .map_err(failed)?;
        for number in 1..=count {
            let succeeded = succeeded && number == count;
            sqlx::query(
                "INSERT INTO webhook_delivery_attempts \
                 (id, organization_id, delivery_id, number, status_code, error_class, attempted_at, duration_ms) \
                 VALUES (?, ?, ?, ?, ?, ?, ?, ?)",
            )
            .bind(Uuid::now_v7())
            .bind(organization_id.uuid())
            .bind(delivery_id.uuid())
            .bind(number)
            .bind(if succeeded { 200 } else { 503 })
            .bind((!succeeded).then_some("status"))
            .bind(at + i64::from(number) * 1000)
            .bind(12 + number)
            .execute(&mut *transaction)
            .await
            .map_err(failed)?;
        }
    }
    transaction.commit().await.map_err(failed)
}

fn failed(error: sqlx::Error) -> String {
    format!("webhook history fixture: {error}")
}
