//! [`SqliteAuditStore`]: the audit log, and [`record`], which every store
//! calls inside the transaction of the change it records. Every statement
//! names the organization; an event placed in a workspace is written only
//! from that workspace's row in the organization.
//!
//! The log keeps history: `audit_events.workspace_id` doesn't reference
//! `workspaces`, so a deleted workspace's events stay, and a list joins the
//! workspace only while it exists. Lists page by keyset, newest first: the
//! cursor is the last event's position (instant, ID), and names nothing a
//! caller can't already see.

use async_trait::async_trait;
use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use sqlx::sqlite::SqliteRow;
use sqlx::{QueryBuilder, Row, Sqlite, SqliteConnection};
use uuid::Uuid;
use zupflow_core::audit::{
    AuditAction, AuditActor, AuditEntry, AuditError, AuditEvent, AuditFilter, AuditObject, AuditPlace, AuditStore,
    AuditTarget, AuditWorkspace, MAX_PAGE,
};
use zupflow_core::environment::Environment;
use zupflow_core::ids::{AuditEventId, OrganizationId, WorkspaceId};
use zupflow_core::time::Timestamp;
use zupflow_core::workspaces::Page;

use crate::Database;

/// Records `entry` in the organization, on the connection of the change it
/// records. A workspace's event takes the workspace's environment, and fails
/// with [`sqlx::Error::RowNotFound`] when the organization holds no such
/// workspace, so the change fails with it.
pub(crate) async fn record(
    connection: &mut SqliteConnection,
    organization_id: OrganizationId,
    entry: &AuditEntry,
) -> Result<AuditEventId, sqlx::Error> {
    record_at(connection, organization_id, entry, Timestamp::now()).await
}

/// [`record`], at `at`: for a store that keeps its own clock.
pub(crate) async fn record_at(
    connection: &mut SqliteConnection,
    organization_id: OrganizationId,
    entry: &AuditEntry,
    at: Timestamp,
) -> Result<AuditEventId, sqlx::Error> {
    let id = AuditEventId::generate();
    let created_at = at.millis();
    let written = match entry.place {
        AuditPlace::Workspace(workspace_id) => sqlx::query(
            "INSERT INTO audit_events (id, organization_id, workspace_id, environment, actor_type, actor_id, \
             actor_name, action, target_type, target_id, target_name, created_at) \
             SELECT ?, w.organization_id, w.id, w.environment, ?, ?, ?, ?, ?, ?, ?, ? \
             FROM workspaces w WHERE w.organization_id = ? AND w.id = ?",
        )
        .bind(id.uuid())
        .bind(entry.actor.kind())
        .bind(entry.actor.uuid())
        .bind(entry.actor.name())
        .bind(entry.action.as_str())
        .bind(entry.target.object.kind())
        .bind(entry.target.object.uuid())
        .bind(&entry.target.name)
        .bind(created_at)
        .bind(organization_id.uuid())
        .bind(workspace_id.uuid()),
        AuditPlace::Organization | AuditPlace::Environment(_) => {
            let environment = match entry.place {
                AuditPlace::Environment(environment) => Some(environment.as_str()),
                _ => None,
            };
            sqlx::query(
                "INSERT INTO audit_events (id, organization_id, workspace_id, environment, actor_type, actor_id, \
                 actor_name, action, target_type, target_id, target_name, created_at) \
                 VALUES (?, ?, NULL, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
            )
            .bind(id.uuid())
            .bind(organization_id.uuid())
            .bind(environment)
            .bind(entry.actor.kind())
            .bind(entry.actor.uuid())
            .bind(entry.actor.name())
            .bind(entry.action.as_str())
            .bind(entry.target.object.kind())
            .bind(entry.target.object.uuid())
            .bind(&entry.target.name)
            .bind(created_at)
        }
    }
    .execute(&mut *connection)
    .await?;
    if written.rows_affected() != 1 {
        return Err(sqlx::Error::RowNotFound);
    }
    Ok(id)
}

/// Records `entry` for a change made in one workspace: refused (with
/// [`sqlx::Error::Protocol`], failing the change) unless the entry is placed
/// in that same workspace. A caller's mistake is never quietly moved.
pub(crate) async fn record_in_workspace(
    connection: &mut SqliteConnection,
    organization_id: OrganizationId,
    workspace_id: WorkspaceId,
    entry: &AuditEntry,
) -> Result<AuditEventId, sqlx::Error> {
    if entry.place != AuditPlace::Workspace(workspace_id) {
        return Err(sqlx::Error::Protocol(
            "an audit event of a workspace's change placed outside that workspace".to_owned(),
        ));
    }
    record(connection, organization_id, entry).await
}

pub struct SqliteAuditStore {
    database: Database,
}

impl SqliteAuditStore {
    pub fn new(database: Database) -> Self {
        Self { database }
    }
}

fn failed(operation: &'static str) -> impl FnOnce(sqlx::Error) -> AuditError {
    move |error| {
        tracing::error!(event = "audit.database_error", operation, %error);
        AuditError::Unavailable
    }
}

fn read(error: impl std::fmt::Display) -> AuditError {
    tracing::error!(event = "audit.unreadable_row", %error);
    AuditError::Unavailable
}

/// The columns `event_from` reads. A macro, so queries stay static strings.
macro_rules! select_events {
    () => {
        "SELECT a.id, a.organization_id, a.workspace_id, a.environment, a.actor_type, a.actor_id, a.actor_name, \
         a.action, a.target_type, a.target_id, a.target_name, a.created_at, w.name AS workspace_name, \
         w.slug AS workspace_slug, w.environment AS workspace_environment FROM audit_events a \
         LEFT JOIN workspaces w ON w.organization_id = a.organization_id AND w.id = a.workspace_id"
    };
}

fn environment(value: Option<String>) -> Result<Option<Environment>, AuditError> {
    value
        .map(|value| Environment::parse(&value).ok_or_else(|| read("an unknown environment")))
        .transpose()
}

fn event_from(row: &SqliteRow) -> Result<AuditEvent, AuditError> {
    let workspace_id = row
        .try_get::<Option<Uuid>, _>("workspace_id")
        .map_err(read)?
        .map(WorkspaceId::from_uuid);
    let workspace_name: Option<String> = row.try_get("workspace_name").map_err(read)?;
    let workspace = match (workspace_id, workspace_name) {
        (Some(id), Some(name)) => Some(AuditWorkspace {
            id,
            name,
            slug: row.try_get("workspace_slug").map_err(read)?,
            environment: environment(row.try_get("workspace_environment").map_err(read)?)?
                .ok_or_else(|| read("a workspace without an environment"))?,
        }),
        _ => None,
    };
    let actor_type: String = row.try_get("actor_type").map_err(read)?;
    let actor = AuditActor::from_stored(
        &actor_type,
        row.try_get("actor_id").map_err(read)?,
        row.try_get("actor_name").map_err(read)?,
    )
    .ok_or_else(|| read("an unknown actor"))?;
    let action: String = row.try_get("action").map_err(read)?;
    let target_type: String = row.try_get("target_type").map_err(read)?;
    let object = AuditObject::from_stored(&target_type, row.try_get("target_id").map_err(read)?)
        .ok_or_else(|| read("an unknown target"))?;
    Ok(AuditEvent {
        id: AuditEventId::from_uuid(row.try_get("id").map_err(read)?),
        organization_id: OrganizationId::from_uuid(row.try_get("organization_id").map_err(read)?),
        workspace_id,
        workspace,
        environment: environment(row.try_get("environment").map_err(read)?)?,
        actor,
        action: AuditAction::parse(&action).ok_or_else(|| read("an unknown action"))?,
        target: AuditTarget {
            object,
            name: row.try_get("target_name").map_err(read)?,
        },
        created_at: Timestamp::from_millis(row.try_get("created_at").map_err(read)?),
    })
}

/// A position in the list: newest first, then by ID.
struct Position {
    created_at: i64,
    id: Uuid,
}

impl Position {
    /// The event's prefixed ID, never its bare UUID: a cursor leaves the server.
    fn encode(&self) -> String {
        URL_SAFE_NO_PAD.encode(format!("{}\n{}", self.created_at, AuditEventId::from_uuid(self.id)))
    }

    fn decode(cursor: &str) -> Option<Self> {
        let raw = String::from_utf8(URL_SAFE_NO_PAD.decode(cursor).ok()?).ok()?;
        let (created_at, id) = raw.split_once('\n')?;
        Some(Self {
            created_at: created_at.parse().ok()?,
            id: AuditEventId::parse(id).ok()?.uuid(),
        })
    }
}

/// Appends the filter's conditions, every one after the organization's.
fn push_filter(query: &mut QueryBuilder<Sqlite>, organization_id: OrganizationId, filter: &AuditFilter) {
    query.push(" WHERE a.organization_id = ");
    query.push_bind(organization_id.uuid());
    if let Some(workspace_id) = filter.workspace_id {
        query.push(" AND a.workspace_id = ");
        query.push_bind(workspace_id.uuid());
    }
}

#[async_trait]
impl AuditStore for SqliteAuditStore {
    async fn list_audit_events(
        &self,
        organization_id: OrganizationId,
        filter: &AuditFilter,
        cursor: Option<&str>,
        limit: u32,
    ) -> Result<Page<AuditEvent>, AuditError> {
        let after = cursor
            .map(|cursor| Position::decode(cursor).ok_or(AuditError::InvalidCursor))
            .transpose()?;
        let limit = limit.clamp(1, MAX_PAGE);
        let mut transaction = self
            .database
            .pool()
            .begin()
            .await
            .map_err(failed("list_audit_events"))?;
        let mut count = QueryBuilder::<Sqlite>::new("SELECT count(*) FROM audit_events a");
        push_filter(&mut count, organization_id, filter);
        let total: i64 = count
            .build_query_scalar()
            .fetch_one(&mut *transaction)
            .await
            .map_err(failed("count_audit_events"))?;
        let mut query = QueryBuilder::<Sqlite>::new(select_events!());
        push_filter(&mut query, organization_id, filter);
        if let Some(after) = &after {
            query.push(" AND (a.created_at < ");
            query.push_bind(after.created_at);
            query.push(" OR (a.created_at = ");
            query.push_bind(after.created_at);
            query.push(" AND a.id < ");
            query.push_bind(after.id);
            query.push("))");
        }
        query.push(" ORDER BY a.created_at DESC, a.id DESC LIMIT ");
        query.push_bind(i64::from(limit) + 1);
        let rows = query
            .build()
            .fetch_all(&mut *transaction)
            .await
            .map_err(failed("list_audit_events"))?;
        transaction.commit().await.map_err(failed("list_audit_events"))?;
        let mut items = rows.iter().map(event_from).collect::<Result<Vec<_>, _>>()?;
        let more = items.len() > limit as usize;
        items.truncate(limit as usize);
        let next_cursor = more
            .then(|| {
                items.last().map(|last| {
                    Position {
                        created_at: last.created_at.millis(),
                        id: last.id.uuid(),
                    }
                    .encode()
                })
            })
            .flatten();
        Ok(Page {
            items,
            next_cursor,
            total: u64::try_from(total).map_err(read)?,
        })
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use std::sync::Arc;

    use super::*;
    use crate::SqliteWorkspaceStore;
    use crate::schema::{Temporary, temporary};

    #[tokio::test]
    async fn conforms() {
        let Temporary {
            database,
            directory: _directory,
        } = temporary().await;
        let seeding = database.clone();
        let prepare: zupflow_conformance::audit_store::Prepare = Arc::new(move |organization_id| {
            let database = seeding.clone();
            Box::pin(async move {
                sqlx::query("INSERT INTO organizations (id, name, slug, created_at) VALUES (?, 'Test', ?, 0)")
                    .bind(organization_id.uuid())
                    .bind(format!("org-{}", organization_id.uuid().simple()))
                    .execute(database.pool())
                    .await
                    .unwrap();
            })
        });
        zupflow_conformance::audit_store::run(
            zupflow_conformance::audit_store::Stores {
                audit: &SqliteAuditStore::new(database.clone()),
                workspaces: &SqliteWorkspaceStore::new(database),
            },
            prepare,
        )
        .await;
    }
}
