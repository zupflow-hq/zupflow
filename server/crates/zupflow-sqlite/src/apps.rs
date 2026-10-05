//! [`SqliteAppStore`]: catalog apps in `apps`, workspace apps in
//! `workspace_apps` (decision 0031). Every statement repeats
//! `organization_id`, and `workspace_id` on workspace apps, beside the app's ID.

use async_trait::async_trait;
use sqlx::Row;
use sqlx::sqlite::SqliteRow;
use zupflow_core::apps::{App, AppChanges, AppError, AppKind, AppStore, NewApp};
use zupflow_core::ids::{AppId, OrganizationId, WorkspaceId};
use zupflow_core::time::Timestamp;

use crate::Database;

pub struct SqliteAppStore {
    database: Database,
}

impl SqliteAppStore {
    pub fn new(database: Database) -> Self {
        Self { database }
    }
}

fn failed(operation: &'static str) -> impl FnOnce(sqlx::Error) -> AppError {
    move |error| {
        tracing::error!(event = "apps.database_error", operation, %error);
        AppError::Unavailable
    }
}

/// A unique violation is the slug: the ID is new, so only the slug can collide.
fn inserted(operation: &'static str) -> impl FnOnce(sqlx::Error) -> AppError {
    move |error| {
        if error
            .as_database_error()
            .is_some_and(|error| error.is_unique_violation())
        {
            AppError::SlugTaken
        } else {
            failed(operation)(error)
        }
    }
}

fn read(error: impl std::fmt::Display) -> AppError {
    tracing::error!(event = "apps.unreadable_row", %error);
    AppError::Unavailable
}

/// The columns both tables share, as `app_from` reads them. Macros, so
/// queries stay static strings.
macro_rules! columns {
    () => {
        "id, organization_id, kind, name, slug, description, icon_glyph, icon_color, auth, tools, \
         version, created_at, updated_at"
    };
}

macro_rules! workspace_columns {
    () => {
        concat!(columns!(), ", workspace_id")
    };
}

fn app_from(row: &SqliteRow, owner_workspace_id: Option<WorkspaceId>) -> Result<App, AppError> {
    let kind: String = row.try_get("kind").map_err(read)?;
    let auth: String = row.try_get("auth").map_err(read)?;
    let tools: String = row.try_get("tools").map_err(read)?;
    let version: i64 = row.try_get("version").map_err(read)?;
    Ok(App {
        id: AppId::from_uuid(row.try_get("id").map_err(read)?),
        organization_id: OrganizationId::from_uuid(row.try_get("organization_id").map_err(read)?),
        owner_workspace_id,
        kind: AppKind::parse(&kind).ok_or(AppError::Unavailable)?,
        name: row.try_get("name").map_err(read)?,
        slug: row.try_get("slug").map_err(read)?,
        description: row.try_get("description").map_err(read)?,
        icon: zupflow_core::apps::AppIcon {
            glyph: row.try_get("icon_glyph").map_err(read)?,
            color: row.try_get("icon_color").map_err(read)?,
        },
        auth: serde_json::from_str(&auth).map_err(read)?,
        tools: serde_json::from_str(&tools).map_err(read)?,
        version: u32::try_from(version).map_err(read)?,
        created_at: Timestamp::from_millis(row.try_get("created_at").map_err(read)?),
        updated_at: Timestamp::from_millis(row.try_get("updated_at").map_err(read)?),
    })
}

fn workspace_app_from(row: &SqliteRow) -> Result<App, AppError> {
    let workspace_id = WorkspaceId::from_uuid(row.try_get("workspace_id").map_err(read)?);
    app_from(row, Some(workspace_id))
}

/// The JSON columns.
fn encoded(app: &NewApp) -> Result<(String, String), AppError> {
    let auth = serde_json::to_string(&app.auth).map_err(read)?;
    let tools = serde_json::to_string(&app.tools).map_err(read)?;
    Ok((auth, tools))
}

fn created(
    organization_id: OrganizationId,
    owner: Option<WorkspaceId>,
    id: AppId,
    app: &NewApp,
    now: Timestamp,
) -> App {
    App {
        id,
        organization_id,
        owner_workspace_id: owner,
        kind: app.kind,
        name: app.name.clone(),
        slug: app.slug.clone(),
        description: app.description.clone(),
        icon: app.icon.clone(),
        auth: app.auth.clone(),
        tools: app.tools.clone(),
        version: 1,
        created_at: now,
        updated_at: now,
    }
}

#[async_trait]
impl AppStore for SqliteAppStore {
    async fn list_organization_apps(&self, organization_id: OrganizationId) -> Result<Vec<App>, AppError> {
        let rows = sqlx::query(concat!(
            "SELECT ",
            columns!(),
            " FROM apps WHERE organization_id = ? ORDER BY lower(name), id"
        ))
        .bind(organization_id.uuid())
        .fetch_all(self.database.pool())
        .await
        .map_err(failed("list_organization_apps"))?;
        rows.iter().map(|row| app_from(row, None)).collect()
    }

    async fn create_organization_app(
        &self,
        organization_id: OrganizationId,
        app_id: AppId,
        app: &NewApp,
    ) -> Result<App, AppError> {
        let (auth, tools) = encoded(app)?;
        let now = Timestamp::now();
        sqlx::query(
            "INSERT INTO apps (id, organization_id, kind, name, slug, description, icon_glyph, icon_color, \
             auth, tools, version, created_at, updated_at) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, 1, ?, ?)",
        )
        .bind(app_id.uuid())
        .bind(organization_id.uuid())
        .bind(app.kind.as_str())
        .bind(&app.name)
        .bind(&app.slug)
        .bind(&app.description)
        .bind(&app.icon.glyph)
        .bind(&app.icon.color)
        .bind(auth)
        .bind(tools)
        .bind(now.millis())
        .bind(now.millis())
        .execute(self.database.pool())
        .await
        .map_err(inserted("insert_organization_app"))?;
        Ok(created(organization_id, None, app_id, app, now))
    }

    async fn find_organization_app(
        &self,
        organization_id: OrganizationId,
        app_id: AppId,
    ) -> Result<Option<App>, AppError> {
        let row = sqlx::query(concat!(
            "SELECT ",
            columns!(),
            " FROM apps WHERE organization_id = ? AND id = ?"
        ))
        .bind(organization_id.uuid())
        .bind(app_id.uuid())
        .fetch_optional(self.database.pool())
        .await
        .map_err(failed("find_organization_app"))?;
        row.map(|row| app_from(&row, None)).transpose()
    }

    async fn update_organization_app(
        &self,
        organization_id: OrganizationId,
        app_id: AppId,
        changes: AppChanges<'_>,
    ) -> Result<App, AppError> {
        let row = sqlx::query(concat!(
            "UPDATE apps SET name = coalesce(?, name), description = coalesce(?, description), updated_at = ? \
             WHERE organization_id = ? AND id = ? RETURNING ",
            columns!()
        ))
        .bind(changes.name)
        .bind(changes.description)
        .bind(Timestamp::now().millis())
        .bind(organization_id.uuid())
        .bind(app_id.uuid())
        .fetch_optional(self.database.pool())
        .await
        .map_err(failed("update_organization_app"))?;
        app_from(&row.ok_or(AppError::NotFound)?, None)
    }

    async fn list_workspace_apps(
        &self,
        organization_id: OrganizationId,
        workspace_id: WorkspaceId,
    ) -> Result<Vec<App>, AppError> {
        let rows = sqlx::query(concat!(
            "SELECT ",
            workspace_columns!(),
            " FROM workspace_apps WHERE organization_id = ? AND workspace_id = ? ORDER BY lower(name), id"
        ))
        .bind(organization_id.uuid())
        .bind(workspace_id.uuid())
        .fetch_all(self.database.pool())
        .await
        .map_err(failed("list_workspace_apps"))?;
        rows.iter().map(workspace_app_from).collect()
    }

    async fn create_workspace_app(
        &self,
        organization_id: OrganizationId,
        workspace_id: WorkspaceId,
        app_id: AppId,
        app: &NewApp,
    ) -> Result<App, AppError> {
        let (auth, tools) = encoded(app)?;
        let now = Timestamp::now();
        let inserted_rows = sqlx::query(
            "INSERT INTO workspace_apps (id, organization_id, workspace_id, kind, name, slug, description, \
             icon_glyph, icon_color, auth, tools, version, created_at, updated_at) \
             SELECT ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, 1, ?, ? \
             FROM workspaces WHERE organization_id = ? AND id = ?",
        )
        .bind(app_id.uuid())
        // The scope from the trust context, never from the parent row; the
        // row only has to exist in it.
        .bind(organization_id.uuid())
        .bind(workspace_id.uuid())
        .bind(app.kind.as_str())
        .bind(&app.name)
        .bind(&app.slug)
        .bind(&app.description)
        .bind(&app.icon.glyph)
        .bind(&app.icon.color)
        .bind(auth)
        .bind(tools)
        .bind(now.millis())
        .bind(now.millis())
        .bind(organization_id.uuid())
        .bind(workspace_id.uuid())
        .execute(self.database.pool())
        .await
        .map_err(inserted("insert_workspace_app"))?;
        if inserted_rows.rows_affected() == 0 {
            return Err(AppError::NotFound);
        }
        Ok(created(organization_id, Some(workspace_id), app_id, app, now))
    }

    async fn find_workspace_app(
        &self,
        organization_id: OrganizationId,
        workspace_id: WorkspaceId,
        app_id: AppId,
    ) -> Result<Option<App>, AppError> {
        let row = sqlx::query(concat!(
            "SELECT ",
            workspace_columns!(),
            " FROM workspace_apps WHERE organization_id = ? AND workspace_id = ? AND id = ?"
        ))
        .bind(organization_id.uuid())
        .bind(workspace_id.uuid())
        .bind(app_id.uuid())
        .fetch_optional(self.database.pool())
        .await
        .map_err(failed("find_workspace_app"))?;
        row.as_ref().map(workspace_app_from).transpose()
    }

    async fn update_workspace_app(
        &self,
        organization_id: OrganizationId,
        workspace_id: WorkspaceId,
        app_id: AppId,
        changes: AppChanges<'_>,
    ) -> Result<App, AppError> {
        let row = sqlx::query(concat!(
            "UPDATE workspace_apps SET name = coalesce(?, name), description = coalesce(?, description), \
             updated_at = ? WHERE organization_id = ? AND workspace_id = ? AND id = ? RETURNING ",
            workspace_columns!()
        ))
        .bind(changes.name)
        .bind(changes.description)
        .bind(Timestamp::now().millis())
        .bind(organization_id.uuid())
        .bind(workspace_id.uuid())
        .bind(app_id.uuid())
        .fetch_optional(self.database.pool())
        .await
        .map_err(failed("update_workspace_app"))?;
        workspace_app_from(&row.ok_or(AppError::NotFound)?)
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
        let setup = database.clone();
        let prepare: zupflow_conformance::app_store::Prepare = Arc::new(move |organization_id, workspaces| {
            let database = setup.clone();
            Box::pin(async move {
                let id = organization_id.uuid();
                sqlx::query("INSERT INTO organizations (id, name, slug, created_at) VALUES (?, 'Test', ?, 0)")
                    .bind(id)
                    .bind(format!("org-{}", id.simple()))
                    .execute(database.pool())
                    .await
                    .unwrap();
                for workspace_id in workspaces {
                    let workspace = workspace_id.uuid();
                    sqlx::query(
                        "INSERT INTO workspaces (id, organization_id, environment, name, slug, created_at) \
                         VALUES (?, ?, 'sandbox', 'W', ?, 0)",
                    )
                    .bind(workspace)
                    .bind(id)
                    .bind(format!("w-{}", workspace.simple()))
                    .execute(database.pool())
                    .await
                    .unwrap();
                }
            })
        });
        zupflow_conformance::app_store::run(&SqliteAppStore::new(database), prepare).await;
    }

    /// Deleting a workspace takes its own apps with it, and nothing of its
    /// siblings' or the catalog's.
    #[tokio::test]
    async fn deletes_a_workspaces_apps_with_it() {
        use zupflow_core::apps::AuthType;
        use zupflow_core::workspaces::{NewWorkspace, WorkspaceStore};

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
        let workspaces = crate::SqliteWorkspaceStore::new(database.clone());
        let mut ids = Vec::new();
        for slug in ["gone", "kept"] {
            let workspace = NewWorkspace {
                name: slug.to_owned(),
                slug: slug.to_owned(),
                environment: zupflow_core::environment::Environment::Sandbox,
                external_id: None,
            };
            ids.push(
                workspaces
                    .create_workspace(org, WorkspaceId::generate(), &workspace)
                    .await
                    .unwrap()
                    .id,
            );
        }
        let apps = SqliteAppStore::new(database);
        let app = || NewApp::blank("Tools".into(), "tools".into(), String::new(), AuthType::Token);
        let gone = apps
            .create_workspace_app(org, ids[0], AppId::generate(), &app())
            .await
            .unwrap();
        let kept = apps
            .create_workspace_app(org, ids[1], AppId::generate(), &app())
            .await
            .unwrap();
        let catalog = apps
            .create_organization_app(org, AppId::generate(), &app())
            .await
            .unwrap();
        workspaces.delete_workspace(org, ids[0]).await.unwrap();
        assert_eq!(apps.find_workspace_app(org, ids[0], gone.id).await.unwrap(), None);
        assert_eq!(apps.find_workspace_app(org, ids[1], kept.id).await.unwrap(), Some(kept));
        assert_eq!(
            apps.find_organization_app(org, catalog.id).await.unwrap(),
            Some(catalog)
        );
    }
}
