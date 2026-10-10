//! [`SqliteConnectStore`]: Connect settings and sessions. Every statement
//! repeats the organization, and the workspace for sessions, except the one
//! the trait declares cross-tenant: [`ConnectStore::attribute_session_by_reference`].

use async_trait::async_trait;
use sqlx::Row;
use sqlx::sqlite::SqliteRow;
use uuid::Uuid;
use zupflow_core::connect::{
    Completion, ConnectError, ConnectIntent, ConnectSession, ConnectSessionStatus, ConnectSettings, ConnectStore,
    NewConnectSession, ReferenceHash, SESSION_TTL_MS,
};
use zupflow_core::environment::Environment;
use zupflow_core::ids::{AppId, ConnectSessionId, InstallationId, OrganizationId, WorkspaceId};
use zupflow_core::installations::{Installation, InstallationError, InstalledApp, NewInstallation};
use zupflow_core::time::Timestamp;

use crate::Database;
use crate::installations::{Renewal, insert_connected, reconnect};

pub struct SqliteConnectStore {
    database: Database,
}

impl SqliteConnectStore {
    pub fn new(database: Database) -> Self {
        Self { database }
    }
}

fn failed(operation: &'static str) -> impl FnOnce(sqlx::Error) -> ConnectError {
    move |error| {
        tracing::error!(event = "connect.database_error", operation, %error);
        ConnectError::Unavailable
    }
}

fn read(error: impl std::fmt::Display) -> ConnectError {
    tracing::error!(event = "connect.unreadable_row", %error);
    ConnectError::Unavailable
}

/// The columns `session_from` reads. A macro, so queries stay static strings.
macro_rules! columns {
    () => {
        "id, organization_id, workspace_id, environment, catalog_app_id, workspace_app_id, configuration_version, \
         intent, installation_id, label, status, return_url, requested_scopes, created_at, expires_at, \
         completed_at, failure_code"
    };
}

fn session_from(row: &SqliteRow) -> Result<ConnectSession, ConnectError> {
    let catalog: Option<Uuid> = row.try_get("catalog_app_id").map_err(read)?;
    let own: Option<Uuid> = row.try_get("workspace_app_id").map_err(read)?;
    let app = match (catalog, own) {
        (Some(id), None) => InstalledApp::Catalog(AppId::from_uuid(id)),
        (None, Some(id)) => InstalledApp::Workspace(AppId::from_uuid(id)),
        _ => return Err(read("a session of no app, or of two")),
    };
    let text = |column: &str| -> Result<String, ConnectError> { row.try_get(column).map_err(read) };
    let version: i64 = row.try_get("configuration_version").map_err(read)?;
    let installation: Option<Uuid> = row.try_get("installation_id").map_err(read)?;
    let completed_at: Option<i64> = row.try_get("completed_at").map_err(read)?;
    Ok(ConnectSession {
        id: ConnectSessionId::from_uuid(row.try_get("id").map_err(read)?),
        organization_id: OrganizationId::from_uuid(row.try_get("organization_id").map_err(read)?),
        workspace_id: WorkspaceId::from_uuid(row.try_get("workspace_id").map_err(read)?),
        environment: Environment::parse(&text("environment")?).ok_or_else(|| read("an unknown environment"))?,
        app,
        configuration_version: u32::try_from(version).map_err(read)?,
        intent: ConnectIntent::parse(&text("intent")?).ok_or_else(|| read("an unknown intent"))?,
        installation_id: installation.map(InstallationId::from_uuid),
        label: text("label")?,
        status: ConnectSessionStatus::parse(&text("status")?).ok_or_else(|| read("an unknown status"))?,
        return_url: row.try_get("return_url").map_err(read)?,
        requested_scopes: serde_json::from_str(&text("requested_scopes")?).map_err(read)?,
        created_at: Timestamp::from_millis(row.try_get("created_at").map_err(read)?),
        expires_at: Timestamp::from_millis(row.try_get("expires_at").map_err(read)?),
        completed_at: completed_at.map(Timestamp::from_millis),
        failure_code: row.try_get("failure_code").map_err(read)?,
    })
}

/// An installation write that failed inside a completion: the store has logged it.
fn installation_failed(error: InstallationError) -> ConnectError {
    match error {
        InstallationError::NotFound => ConnectError::InstallationChanged,
        InstallationError::Unavailable | InstallationError::InvalidCursor => ConnectError::Unavailable,
    }
}

fn list(value: &str) -> Result<Vec<String>, ConnectError> {
    serde_json::from_str(value).map_err(read)
}

#[async_trait]
impl ConnectStore for SqliteConnectStore {
    async fn find_settings(
        &self,
        organization_id: OrganizationId,
        environment: Environment,
    ) -> Result<ConnectSettings, ConnectError> {
        let row: Option<(String, String, i64)> = sqlx::query_as(
            "SELECT return_urls, allowed_origins, updated_at FROM connect_settings \
             WHERE organization_id = ? AND environment = ?",
        )
        .bind(organization_id.uuid())
        .bind(environment.as_str())
        .fetch_optional(self.database.pool())
        .await
        .map_err(failed("find_settings"))?;
        match row {
            None => Ok(ConnectSettings::default()),
            Some((return_urls, allowed_origins, updated_at)) => Ok(ConnectSettings {
                return_urls: list(&return_urls)?,
                allowed_origins: list(&allowed_origins)?,
                updated_at: Some(Timestamp::from_millis(updated_at)),
            }),
        }
    }

    async fn save_settings(
        &self,
        organization_id: OrganizationId,
        environment: Environment,
        return_urls: &[String],
        allowed_origins: &[String],
    ) -> Result<ConnectSettings, ConnectError> {
        let now = Timestamp::now();
        let urls = serde_json::to_string(return_urls).map_err(read)?;
        let origins = serde_json::to_string(allowed_origins).map_err(read)?;
        sqlx::query(
            "INSERT INTO connect_settings (id, organization_id, environment, return_urls, allowed_origins, updated_at) \
             VALUES (?, ?, ?, ?, ?, ?) ON CONFLICT (organization_id, environment) DO UPDATE SET \
             return_urls = excluded.return_urls, allowed_origins = excluded.allowed_origins, \
             updated_at = excluded.updated_at",
        )
        .bind(Uuid::now_v7())
        .bind(organization_id.uuid())
        .bind(environment.as_str())
        .bind(urls)
        .bind(origins)
        .bind(now.millis())
        .execute(self.database.pool())
        .await
        .map_err(failed("save_settings"))?;
        Ok(ConnectSettings {
            return_urls: return_urls.to_vec(),
            allowed_origins: allowed_origins.to_vec(),
            updated_at: Some(now),
        })
    }

    async fn create_session(
        &self,
        organization_id: OrganizationId,
        workspace_id: WorkspaceId,
        session_id: ConnectSessionId,
        session: &NewConnectSession,
        now: Timestamp,
    ) -> Result<ConnectSession, ConnectError> {
        let (catalog_app_id, workspace_app_id) = match session.app {
            InstalledApp::Catalog(id) => (Some(id.uuid()), None),
            InstalledApp::Workspace(id) => (None, Some(id.uuid())),
        };
        let scopes = serde_json::to_string(&session.requested_scopes).map_err(read)?;
        let expires_at = Timestamp::from_millis(now.millis() + SESSION_TTL_MS);
        // The scope is bound from the trust context; the workspace row only has
        // to exist in it, and in the environment the session names.
        let inserted = sqlx::query(
            "INSERT INTO connect_sessions (id, organization_id, workspace_id, environment, catalog_app_id, \
             workspace_app_id, configuration_version, intent, installation_id, label, status, return_url, \
             requested_scopes, reference_hash, created_at, expires_at) \
             SELECT ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, 'pending', ?, ?, ?, ?, ? \
             FROM workspaces WHERE organization_id = ? AND id = ? AND environment = ?",
        )
        .bind(session_id.uuid())
        .bind(organization_id.uuid())
        .bind(workspace_id.uuid())
        .bind(session.environment.as_str())
        .bind(catalog_app_id)
        .bind(workspace_app_id)
        .bind(i64::from(session.configuration_version))
        .bind(session.intent.as_str())
        .bind(session.installation_id.map(|id| id.uuid()))
        .bind(&session.label)
        .bind(&session.return_url)
        .bind(scopes)
        .bind(session.reference_hash.0.as_slice())
        .bind(now.millis())
        .bind(expires_at.millis())
        .bind(organization_id.uuid())
        .bind(workspace_id.uuid())
        .bind(session.environment.as_str())
        .execute(self.database.pool())
        .await
        .map_err(failed("create_session"))?;
        if inserted.rows_affected() == 0 {
            return Err(ConnectError::NotFound);
        }
        Ok(ConnectSession {
            id: session_id,
            organization_id,
            workspace_id,
            environment: session.environment,
            app: session.app,
            configuration_version: session.configuration_version,
            intent: session.intent,
            installation_id: session.installation_id,
            label: session.label.clone(),
            status: ConnectSessionStatus::Pending,
            return_url: session.return_url.clone(),
            requested_scopes: session.requested_scopes.clone(),
            created_at: now,
            expires_at,
            completed_at: None,
            failure_code: None,
        })
    }

    async fn list_sessions(
        &self,
        organization_id: OrganizationId,
        workspace_id: WorkspaceId,
    ) -> Result<Vec<ConnectSession>, ConnectError> {
        let rows = sqlx::query(concat!(
            "SELECT ",
            columns!(),
            " FROM connect_sessions WHERE organization_id = ? AND workspace_id = ? ORDER BY created_at DESC, id DESC"
        ))
        .bind(organization_id.uuid())
        .bind(workspace_id.uuid())
        .fetch_all(self.database.pool())
        .await
        .map_err(failed("list_sessions"))?;
        rows.iter().map(session_from).collect()
    }

    async fn find_session(
        &self,
        organization_id: OrganizationId,
        workspace_id: WorkspaceId,
        session_id: ConnectSessionId,
    ) -> Result<Option<ConnectSession>, ConnectError> {
        let row = sqlx::query(concat!(
            "SELECT ",
            columns!(),
            " FROM connect_sessions WHERE organization_id = ? AND workspace_id = ? AND id = ?"
        ))
        .bind(organization_id.uuid())
        .bind(workspace_id.uuid())
        .bind(session_id.uuid())
        .fetch_optional(self.database.pool())
        .await
        .map_err(failed("find_session"))?;
        row.as_ref().map(session_from).transpose()
    }

    async fn attribute_session_by_reference(
        &self,
        reference_hash: &ReferenceHash,
    ) -> Result<Option<ConnectSession>, ConnectError> {
        // Cross-tenant by design: the hash is unique across the deployment.
        let row = sqlx::query(concat!(
            "SELECT ",
            columns!(),
            " FROM connect_sessions WHERE reference_hash = ?"
        ))
        .bind(reference_hash.0.as_slice())
        .fetch_optional(self.database.pool())
        .await
        .map_err(failed("attribute_session_by_reference"))?;
        row.as_ref().map(session_from).transpose()
    }

    async fn complete_session(
        &self,
        organization_id: OrganizationId,
        workspace_id: WorkspaceId,
        session_id: ConnectSessionId,
        completion: &Completion,
        now: Timestamp,
    ) -> Result<(ConnectSession, Installation), ConnectError> {
        // One writer at a time: the session is read pending and completed
        // with nothing in between, so it completes once.
        let mut transaction = self
            .database
            .pool()
            .begin_with("BEGIN IMMEDIATE")
            .await
            .map_err(failed("complete_session"))?;
        let row = sqlx::query(concat!(
            "SELECT ",
            columns!(),
            " FROM connect_sessions WHERE organization_id = ? AND workspace_id = ? AND id = ?"
        ))
        .bind(organization_id.uuid())
        .bind(workspace_id.uuid())
        .bind(session_id.uuid())
        .fetch_optional(&mut *transaction)
        .await
        .map_err(failed("complete_session"))?;
        let session = session_from(&row.ok_or(ConnectError::NotFound)?)?;
        if session.status_at(now) != ConnectSessionStatus::Pending {
            return Err(ConnectError::NotPending);
        }
        let installation = match session.intent {
            ConnectIntent::Create => {
                let installation = NewInstallation {
                    app: session.app,
                    label: session.label.clone(),
                    configuration_version: completion.configuration_version,
                    account: completion.account.clone(),
                    granted_scopes: completion.granted_scopes.clone(),
                    expires_at: completion.expires_at,
                    credential: completion.credential.clone(),
                };
                if completion.credential.version != 1 {
                    return Err(ConnectError::InstallationChanged);
                }
                insert_connected(
                    &mut transaction,
                    organization_id,
                    workspace_id,
                    completion.installation_id,
                    &installation,
                )
                .await
                .map_err(installation_failed)?
            }
            ConnectIntent::Reconnect => {
                if session.installation_id != Some(completion.installation_id) {
                    return Err(ConnectError::InstallationChanged);
                }
                let renewal = Renewal {
                    configuration_version: completion.configuration_version,
                    account: &completion.account,
                    granted_scopes: &completion.granted_scopes,
                    expires_at: completion.expires_at,
                    credential: &completion.credential,
                };
                reconnect(
                    &mut transaction,
                    organization_id,
                    workspace_id,
                    completion.installation_id,
                    &renewal,
                )
                .await
                .map_err(installation_failed)?
                .ok_or(ConnectError::InstallationChanged)?
            }
        };
        let row = sqlx::query(concat!(
            "UPDATE connect_sessions SET status = 'completed', completed_at = ?, installation_id = ? \
             WHERE organization_id = ? AND workspace_id = ? AND id = ? AND status = 'pending' RETURNING ",
            columns!()
        ))
        .bind(now.millis())
        .bind(installation.id.uuid())
        .bind(organization_id.uuid())
        .bind(workspace_id.uuid())
        .bind(session_id.uuid())
        .fetch_optional(&mut *transaction)
        .await
        .map_err(failed("complete_session"))?;
        let session = session_from(&row.ok_or(ConnectError::NotPending)?)?;
        transaction.commit().await.map_err(failed("complete_session"))?;
        Ok((session, installation))
    }

    async fn cancel_session(
        &self,
        organization_id: OrganizationId,
        workspace_id: WorkspaceId,
        session_id: ConnectSessionId,
        now: Timestamp,
    ) -> Result<ConnectSession, ConnectError> {
        let row = sqlx::query(concat!(
            "UPDATE connect_sessions SET status = 'cancelled' \
             WHERE organization_id = ? AND workspace_id = ? AND id = ? AND status = 'pending' AND expires_at > ? \
             RETURNING ",
            columns!()
        ))
        .bind(organization_id.uuid())
        .bind(workspace_id.uuid())
        .bind(session_id.uuid())
        .bind(now.millis())
        .fetch_optional(self.database.pool())
        .await
        .map_err(failed("cancel_session"))?;
        match row {
            Some(row) => session_from(&row),
            None => match self.find_session(organization_id, workspace_id, session_id).await? {
                Some(_) => Err(ConnectError::NotPending),
                None => Err(ConnectError::NotFound),
            },
        }
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use std::sync::Arc;

    use zupflow_conformance::connect_store::{Prepare, Tenant};

    use super::*;
    use crate::schema::{Temporary, temporary};

    /// Makes the tenant's rows straight in the database: the store under test
    /// only reads them.
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
                sqlx::query(
                    "INSERT INTO apps (id, organization_id, kind, name, slug, description, icon_glyph, icon_color, \
                     auth, tools, version, created_at, updated_at) \
                     VALUES (?, ?, 'mcp', 'Catalog', 'catalog', '', 'C', '#0E7490', '{\"type\":\"token\"}', '[]', 1, 0, 0)",
                )
                .bind(tenant.catalog_app.uuid())
                .bind(org)
                .execute(pool)
                .await
                .unwrap();
                sqlx::query(
                    "INSERT INTO workspace_apps (id, organization_id, workspace_id, kind, name, slug, description, \
                     icon_glyph, icon_color, auth, tools, version, created_at, updated_at) \
                     VALUES (?, ?, ?, 'mcp', 'Own', 'own', '', 'O', '#0E7490', '{\"type\":\"token\"}', '[]', 1, 0, 0)",
                )
                .bind(tenant.own_app.uuid())
                .bind(org)
                .bind(tenant.sandbox.uuid())
                .execute(pool)
                .await
                .unwrap();
                sqlx::query(
                    "INSERT INTO installations (id, organization_id, workspace_id, catalog_app_id, label, status, \
                     configuration_version, granted_scopes, created_at, updated_at) \
                     VALUES (?, ?, ?, ?, 'Main', 'disconnected', 1, '[]', 0, 0)",
                )
                .bind(tenant.installation.uuid())
                .bind(org)
                .bind(tenant.sandbox.uuid())
                .bind(tenant.catalog_app.uuid())
                .execute(pool)
                .await
                .unwrap();
            })
        })
    }

    #[tokio::test]
    async fn conforms() {
        let Temporary {
            database,
            directory: _directory,
        } = temporary().await;
        zupflow_conformance::connect_store::run(&SqliteConnectStore::new(database.clone()), prepare(database)).await;
    }
}
