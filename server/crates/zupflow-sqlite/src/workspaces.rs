//! [`SqliteWorkspaceStore`]: workspaces. Every statement repeats
//! `organization_id` beside the workspace's ID.
//!
//! Lists page by keyset: the cursor is the last row's position in the list's
//! order (lowercased name, environment, ID), so rows created or deleted while
//! someone pages never shift a page. It names a position only, never a row the
//! caller can't already see, and is bound to nothing else: used in another
//! organization it just starts there in that organization's own rows.

use async_trait::async_trait;
use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use sqlx::sqlite::SqliteRow;
use sqlx::{QueryBuilder, Row, Sqlite};
use uuid::Uuid;
use zupflow_core::audit::Audit;
use zupflow_core::environment::Environment;
use zupflow_core::ids::{OrganizationId, WorkspaceId};
use zupflow_core::time::Timestamp;
use zupflow_core::workspaces::{
    NewWorkspace, Page, Workspace, WorkspaceError, WorkspaceFilter, WorkspaceSort, WorkspaceStats, WorkspaceStore,
};

use crate::Database;

/// The window `runs_last_24h` counts.
const RUNS_WINDOW_MS: i64 = 24 * 60 * 60 * 1000;

pub struct SqliteWorkspaceStore {
    database: Database,
}

impl SqliteWorkspaceStore {
    pub fn new(database: Database) -> Self {
        Self { database }
    }

    async fn immediate(&self) -> Result<sqlx::Transaction<'static, Sqlite>, WorkspaceError> {
        self.database
            .pool()
            .begin_with("BEGIN IMMEDIATE")
            .await
            .map_err(failed("begin"))
    }
}

fn failed(operation: &'static str) -> impl FnOnce(sqlx::Error) -> WorkspaceError {
    move |error| {
        tracing::error!(event = "workspaces.database_error", operation, %error);
        WorkspaceError::Unavailable
    }
}

fn read(error: sqlx::Error) -> WorkspaceError {
    tracing::error!(event = "workspaces.unreadable_row", %error);
    WorkspaceError::Unavailable
}

fn workspace_from(row: &SqliteRow) -> Result<Workspace, WorkspaceError> {
    let environment: String = row.try_get("environment").map_err(read)?;
    Ok(Workspace {
        id: WorkspaceId::from_uuid(row.try_get("id").map_err(read)?),
        organization_id: OrganizationId::from_uuid(row.try_get("organization_id").map_err(read)?),
        name: row.try_get("name").map_err(read)?,
        slug: row.try_get("slug").map_err(read)?,
        environment: Environment::parse(&environment).ok_or(WorkspaceError::Unavailable)?,
        external_id: row.try_get("external_id").map_err(read)?,
        created_at: Timestamp::from_millis(row.try_get("created_at").map_err(read)?),
    })
}

/// The columns `workspace_from` reads. A macro, so queries stay static strings.
macro_rules! columns {
    () => {
        "id, organization_id, name, slug, environment, external_id, created_at"
    };
}

/// A position in the list's order.
struct Position {
    name: String,
    environment: Environment,
    id: Uuid,
}

impl Position {
    fn of(workspace: &Workspace) -> Self {
        Self {
            name: workspace.name.to_ascii_lowercase(),
            environment: workspace.environment,
            id: workspace.id.uuid(),
        }
    }

    /// The workspace's prefixed ID, never its bare UUID: a cursor leaves the
    /// server.
    fn encode(&self) -> String {
        let raw = format!(
            "{}\n{}\n{}",
            WorkspaceId::from_uuid(self.id),
            self.environment.as_str(),
            self.name
        );
        URL_SAFE_NO_PAD.encode(raw)
    }

    fn decode(cursor: &str) -> Option<Self> {
        let raw = String::from_utf8(URL_SAFE_NO_PAD.decode(cursor).ok()?).ok()?;
        let mut parts = raw.splitn(3, '\n');
        let id = WorkspaceId::parse(parts.next()?).ok()?.uuid();
        let environment = Environment::parse(parts.next()?)?;
        let name = parts.next()?.to_owned();
        (name.len() <= 240).then_some(Self { name, environment, id })
    }
}

/// Appends the filter's conditions, every one after the organization's.
fn push_filter(query: &mut QueryBuilder<Sqlite>, organization_id: OrganizationId, filter: &WorkspaceFilter) {
    query.push(" WHERE organization_id = ");
    query.push_bind(organization_id.uuid());
    if let Some(environment) = filter.environment {
        query.push(" AND environment = ");
        query.push_bind(environment.as_str());
    }
    if let Some(text) = filter.query.as_deref().map(str::trim).filter(|text| !text.is_empty()) {
        // SQLite's lower() folds ASCII only; fold the same way here.
        let text = text.to_ascii_lowercase();
        query.push(" AND (instr(lower(name), ");
        query.push_bind(text.clone());
        query.push(") > 0 OR instr(slug, ");
        query.push_bind(text.clone());
        query.push(") > 0 OR instr(lower(coalesce(external_id, '')), ");
        query.push_bind(text);
        query.push(") > 0)");
    }
}

#[async_trait]
impl WorkspaceStore for SqliteWorkspaceStore {
    async fn list_workspaces(
        &self,
        organization_id: OrganizationId,
        filter: &WorkspaceFilter,
        cursor: Option<&str>,
        limit: u32,
    ) -> Result<Page<Workspace>, WorkspaceError> {
        let after = cursor
            .map(|cursor| Position::decode(cursor).ok_or(WorkspaceError::InvalidCursor))
            .transpose()?;
        let limit = limit.clamp(1, 100);

        let mut count = QueryBuilder::<Sqlite>::new("SELECT count(*) FROM workspaces");
        push_filter(&mut count, organization_id, filter);
        let total: i64 = count
            .build_query_scalar()
            .fetch_one(self.database.pool())
            .await
            .map_err(failed("count_workspaces"))?;

        // Until runs exist, every workspace has none in the last day, so
        // `Activity` orders exactly like `Name` and shares its cursor.
        let (WorkspaceSort::Name | WorkspaceSort::Activity) = filter.sort;
        let mut query = QueryBuilder::<Sqlite>::new(concat!("SELECT ", columns!(), " FROM workspaces"));
        push_filter(&mut query, organization_id, filter);
        if let Some(after) = &after {
            query.push(" AND (lower(name), environment, id) > (");
            query.push_bind(after.name.clone());
            query.push(", ");
            query.push_bind(after.environment.as_str());
            query.push(", ");
            query.push_bind(after.id);
            query.push(")");
        }
        query.push(" ORDER BY lower(name), environment, id LIMIT ");
        query.push_bind(i64::from(limit) + 1);
        let rows = query
            .build()
            .fetch_all(self.database.pool())
            .await
            .map_err(failed("list_workspaces"))?;
        let mut items = rows.iter().map(workspace_from).collect::<Result<Vec<_>, _>>()?;
        let more = items.len() > limit as usize;
        items.truncate(limit as usize);
        let next_cursor = more
            .then(|| items.last().map(|last| Position::of(last).encode()))
            .flatten();
        Ok(Page {
            items,
            next_cursor,
            total: u64::try_from(total).unwrap_or(0),
        })
    }

    async fn create_workspace(
        &self,
        organization_id: OrganizationId,
        workspace_id: WorkspaceId,
        workspace: &NewWorkspace,
        audit: &Audit<'_, Workspace>,
    ) -> Result<Workspace, WorkspaceError> {
        let created_at = Timestamp::now();
        let mut transaction = self.immediate().await?;
        sqlx::query(
            "INSERT INTO workspaces (id, organization_id, environment, name, slug, external_id, created_at) \
             VALUES (?, ?, ?, ?, ?, ?, ?)",
        )
        .bind(workspace_id.uuid())
        .bind(organization_id.uuid())
        .bind(workspace.environment.as_str())
        .bind(&workspace.name)
        .bind(&workspace.slug)
        .bind(&workspace.external_id)
        .bind(created_at.millis())
        .execute(&mut *transaction)
        .await
        .map_err(|error| {
            if error
                .as_database_error()
                .is_some_and(|error| error.is_unique_violation())
            {
                WorkspaceError::SlugTaken
            } else {
                failed("insert_workspace")(error)
            }
        })?;
        let created = Workspace {
            id: workspace_id,
            organization_id,
            name: workspace.name.clone(),
            slug: workspace.slug.clone(),
            environment: workspace.environment,
            external_id: workspace.external_id.clone(),
            created_at,
        };
        crate::audit::record(&mut transaction, organization_id, &audit(&created))
            .await
            .map_err(failed("audit"))?;
        transaction.commit().await.map_err(failed("commit"))?;
        Ok(created)
    }

    async fn find_workspace(
        &self,
        organization_id: OrganizationId,
        workspace_id: WorkspaceId,
    ) -> Result<Option<Workspace>, WorkspaceError> {
        sqlx::query(concat!(
            "SELECT ",
            columns!(),
            " FROM workspaces WHERE organization_id = ? AND id = ?"
        ))
        .bind(organization_id.uuid())
        .bind(workspace_id.uuid())
        .fetch_optional(self.database.pool())
        .await
        .map_err(failed("find_workspace"))?
        .as_ref()
        .map(workspace_from)
        .transpose()
    }

    async fn update_workspace(
        &self,
        organization_id: OrganizationId,
        workspace_id: WorkspaceId,
        name: Option<&str>,
        external_id: Option<Option<&str>>,
        audit: &Audit<'_, Workspace>,
    ) -> Result<Workspace, WorkspaceError> {
        let mut transaction = self.immediate().await?;
        let row = sqlx::query(concat!(
            "UPDATE workspaces SET name = coalesce(?, name), \
             external_id = CASE WHEN ? THEN ? ELSE external_id END \
             WHERE organization_id = ? AND id = ? RETURNING ",
            columns!()
        ))
        .bind(name)
        .bind(external_id.is_some())
        .bind(external_id.flatten())
        .bind(organization_id.uuid())
        .bind(workspace_id.uuid())
        .fetch_optional(&mut *transaction)
        .await
        .map_err(failed("update_workspace"))?;
        let workspace = workspace_from(&row.ok_or(WorkspaceError::NotFound)?)?;
        crate::audit::record_in_workspace(&mut transaction, organization_id, workspace_id, &audit(&workspace))
            .await
            .map_err(failed("audit"))?;
        transaction.commit().await.map_err(failed("commit"))?;
        Ok(workspace)
    }

    async fn delete_workspace(
        &self,
        organization_id: OrganizationId,
        workspace_id: WorkspaceId,
        audit: &Audit<'_, Workspace>,
    ) -> Result<(), WorkspaceError> {
        // The event is recorded first, while the workspace is there to place
        // it: it keeps the workspace's ID, so the workspace's history finds
        // its deletion. Everything workspace-scoped references the workspace
        // with a cascading key, so its rows go with it in the one DELETE.
        // The audit log doesn't reference it: its history stays.
        let mut transaction = self.immediate().await?;
        let found = sqlx::query(concat!(
            "SELECT ",
            columns!(),
            " FROM workspaces WHERE organization_id = ? AND id = ?"
        ))
        .bind(organization_id.uuid())
        .bind(workspace_id.uuid())
        .fetch_optional(&mut *transaction)
        .await
        .map_err(failed("delete_workspace"))?;
        let workspace = workspace_from(&found.ok_or(WorkspaceError::NotFound)?)?;
        crate::audit::record_in_workspace(&mut transaction, organization_id, workspace_id, &audit(&workspace))
            .await
            .map_err(failed("audit"))?;
        let deleted = sqlx::query("DELETE FROM workspaces WHERE organization_id = ? AND id = ?")
            .bind(organization_id.uuid())
            .bind(workspace_id.uuid())
            .execute(&mut *transaction)
            .await
            .map_err(failed("delete_workspace"))?;
        if deleted.rows_affected() != 1 {
            return Err(WorkspaceError::NotFound);
        }
        transaction.commit().await.map_err(failed("commit"))?;
        Ok(())
    }

    async fn load_stats(
        &self,
        organization_id: OrganizationId,
        workspace_id: WorkspaceId,
    ) -> Result<WorkspaceStats, WorkspaceError> {
        let since = Timestamp::now().millis() - RUNS_WINDOW_MS;
        let (agents, installations, runs): (i64, i64, i64) = sqlx::query_as(
            "SELECT (SELECT count(*) FROM agents WHERE organization_id = ?1 AND workspace_id = ?2), \
             (SELECT count(*) FROM installations WHERE organization_id = ?1 AND workspace_id = ?2), \
             (SELECT count(*) FROM runs WHERE organization_id = ?1 AND workspace_id = ?2 AND created_at >= ?3)",
        )
        .bind(organization_id.uuid())
        .bind(workspace_id.uuid())
        .bind(since)
        .fetch_one(self.database.pool())
        .await
        .map_err(failed("load_stats"))?;
        let count = |n: i64| u64::try_from(n).map_err(|_| WorkspaceError::Unavailable);
        Ok(WorkspaceStats {
            agents: count(agents)?,
            installations: count(installations)?,
            runs_last_24h: count(runs)?,
        })
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use std::sync::Arc;

    use super::*;
    use crate::schema::{Temporary, temporary};

    #[tokio::test]
    async fn conforms() {
        let Temporary {
            database,
            directory: _directory,
        } = temporary().await;
        let organizations = database.clone();
        let prepare: zupflow_conformance::workspace_store::Prepare = Arc::new(move |organization_id| {
            let database = organizations.clone();
            Box::pin(async move {
                let id = organization_id.uuid();
                sqlx::query("INSERT INTO organizations (id, name, slug, created_at) VALUES (?, 'Test', ?, 0)")
                    .bind(id)
                    .bind(format!("org-{}", id.simple()))
                    .execute(database.pool())
                    .await
                    .unwrap();
            })
        });
        zupflow_conformance::workspace_store::run(&SqliteWorkspaceStore::new(database), prepare).await;
    }

    /// Deleting a workspace takes its jobs with it, and nothing of its siblings.
    #[tokio::test]
    async fn deletes_what_a_workspace_holds() {
        use zupflow_core::jobs::{JobOptions, JobQueue};
        use zupflow_core::scope::{Name, Scope};

        let Temporary {
            database,
            directory: _directory,
        } = temporary().await;
        let org = OrganizationId::generate();
        sqlx::query("INSERT INTO organizations (id, name, slug, created_at) VALUES (?, 'Test', 'test', 0)")
            .bind(org.uuid())
            .execute(database.pool())
            .await
            .unwrap();
        let store = SqliteWorkspaceStore::new(database.clone());
        let new = |slug: &str| NewWorkspace {
            name: slug.to_owned(),
            slug: slug.to_owned(),
            environment: Environment::Sandbox,
            external_id: None,
        };
        let doomed = store
            .create_workspace(
                org,
                WorkspaceId::generate(),
                &new("doomed"),
                &zupflow_conformance::audit_store::by_suite,
            )
            .await
            .unwrap();
        let sibling = store
            .create_workspace(
                org,
                WorkspaceId::generate(),
                &new("sibling"),
                &zupflow_conformance::audit_store::by_suite,
            )
            .await
            .unwrap();
        let queue = crate::SqliteJobQueue::new(database.clone());
        let kind = Name::new("test.kind").unwrap();
        for workspace in [&doomed, &sibling] {
            queue
                .enqueue(
                    &Scope::Workspace(org, workspace.id),
                    &kind,
                    vec![],
                    JobOptions::default(),
                )
                .await
                .unwrap();
        }
        store
            .delete_workspace(org, doomed.id, &|_| {
                zupflow_conformance::audit_store::by_suite_in(doomed.id)
            })
            .await
            .unwrap();
        let left: Vec<Uuid> = sqlx::query_scalar("SELECT workspace_id FROM jobs WHERE organization_id = ?")
            .bind(org.uuid())
            .fetch_all(database.pool())
            .await
            .unwrap();
        assert_eq!(left, vec![sibling.id.uuid()]);
    }
}
