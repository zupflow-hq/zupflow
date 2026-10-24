//! [`SqliteConnectStore`]: Connect settings and sessions. Every statement
//! repeats the organization, and the workspace for sessions, except the two
//! the trait declares cross-tenant: [`ConnectStore::attribute_session_by_reference`]
//! and the spending of a `state` in [`ConnectStore::claim_authorization`].

use async_trait::async_trait;
use sqlx::Row;
use sqlx::sqlite::SqliteRow;
use uuid::Uuid;
use zupflow_core::audit::Audit;
use zupflow_core::channels::{ChannelProviderKind, ConnectTarget};
use zupflow_core::connect::{
    Authorization, Completion, ConnectError, ConnectIntent, ConnectSession, ConnectSessionStatus, ConnectSettings,
    ConnectStore, NewConnectSession, ReferenceHash, SESSION_TTL_MS,
};
use zupflow_core::environment::Environment;
use zupflow_core::ids::{
    AppId, ChannelAppId, ChannelAppManagerId, ChannelConnectionId, ConnectSessionId, InstallationId, OrganizationId,
    WorkspaceId,
};
use zupflow_core::installations::{Installation, InstallationError, InstalledApp, NewInstallation};
use zupflow_core::keys::{Sealed, WrappedKey};
use zupflow_core::time::Timestamp;
use zupflow_core::webhooks::WebhookEventType;

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
         completed_at, failure_code, authorization_attempt, target_kind, target_provider, target_app_id, \
         target_manager_id, target_connection_id"
    };
}

/// The columns, for the channel store's statements on sessions.
pub(crate) use columns as session_columns;

/// A channel session's target, as its columns hold it.
fn target_from(row: &SqliteRow) -> Result<Option<ConnectTarget>, ConnectError> {
    let kind: Option<String> = row.try_get("target_kind").map_err(read)?;
    let Some(kind) = kind else {
        return Ok(None);
    };
    let provider: Option<String> = row.try_get("target_provider").map_err(read)?;
    let provider = provider
        .as_deref()
        .and_then(ChannelProviderKind::parse)
        .ok_or_else(|| read("a target of no known provider"))?;
    let app: Option<Uuid> = row.try_get("target_app_id").map_err(read)?;
    let manager: Option<Uuid> = row.try_get("target_manager_id").map_err(read)?;
    let connection: Option<Uuid> = row.try_get("target_connection_id").map_err(read)?;
    match kind.as_str() {
        "channel_app_manager" => Ok(Some(ConnectTarget::ChannelAppManager {
            provider,
            manager: manager.map(ChannelAppManagerId::from_uuid),
        })),
        "channel_app" => Ok(Some(ConnectTarget::ChannelApp {
            provider,
            app: ChannelAppId::from_uuid(app.ok_or_else(|| read("an app's target with no app"))?),
            connection: connection.map(ChannelConnectionId::from_uuid),
        })),
        _ => Err(read("an unknown target")),
    }
}

/// A target's columns: kind, provider, app, manager, connection.
type TargetColumns = (
    Option<&'static str>,
    Option<&'static str>,
    Option<Uuid>,
    Option<Uuid>,
    Option<Uuid>,
);

/// A target as its columns store it.
pub(crate) fn target_columns(target: Option<&ConnectTarget>) -> TargetColumns {
    match target {
        None => (None, None, None, None, None),
        Some(ConnectTarget::ChannelAppManager { provider, manager }) => (
            Some("channel_app_manager"),
            Some(provider.as_str()),
            None,
            manager.map(|id| id.uuid()),
            None,
        ),
        Some(ConnectTarget::ChannelApp {
            provider,
            app,
            connection,
        }) => (
            Some("channel_app"),
            Some(provider.as_str()),
            Some(app.uuid()),
            None,
            connection.map(|id| id.uuid()),
        ),
    }
}

pub(crate) fn session_from(row: &SqliteRow) -> Result<ConnectSession, ConnectError> {
    let catalog: Option<Uuid> = row.try_get("catalog_app_id").map_err(read)?;
    let own: Option<Uuid> = row.try_get("workspace_app_id").map_err(read)?;
    let target = target_from(row)?;
    let app = match (catalog, own, &target) {
        (Some(id), None, None) => Some(InstalledApp::Catalog(AppId::from_uuid(id))),
        (None, Some(id), None) => Some(InstalledApp::Workspace(AppId::from_uuid(id))),
        (None, None, Some(_)) => None,
        _ => return Err(read("a session of no app or target, or of two")),
    };
    let text = |column: &str| -> Result<String, ConnectError> { row.try_get(column).map_err(read) };
    let version: i64 = row.try_get("configuration_version").map_err(read)?;
    let installation: Option<Uuid> = row.try_get("installation_id").map_err(read)?;
    let completed_at: Option<i64> = row.try_get("completed_at").map_err(read)?;
    let attempt: i64 = row.try_get("authorization_attempt").map_err(read)?;
    Ok(ConnectSession {
        id: ConnectSessionId::from_uuid(row.try_get("id").map_err(read)?),
        organization_id: OrganizationId::from_uuid(row.try_get("organization_id").map_err(read)?),
        workspace_id: WorkspaceId::from_uuid(row.try_get("workspace_id").map_err(read)?),
        environment: Environment::parse(&text("environment")?).ok_or_else(|| read("an unknown environment"))?,
        app,
        target,
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
        authorization_attempt: u32::try_from(attempt).map_err(read)?,
    })
}

/// Deletes a session's authorization in progress, if any, inside the
/// caller's transaction: once a session leaves `pending`, nothing it sealed
/// for the provider's consent is kept.
async fn forget_authorization(
    connection: &mut sqlx::SqliteConnection,
    organization_id: OrganizationId,
    workspace_id: WorkspaceId,
    session_id: ConnectSessionId,
) -> Result<(), ConnectError> {
    sqlx::query("DELETE FROM connect_authorizations WHERE organization_id = ? AND workspace_id = ? AND session_id = ?")
        .bind(organization_id.uuid())
        .bind(workspace_id.uuid())
        .bind(session_id.uuid())
        .execute(connection)
        .await
        .map_err(failed("forget_authorization"))?;
    Ok(())
}

/// A terminal session exposes a result installation only after completion.
pub(crate) async fn record_outcome(
    transaction: &mut sqlx::Transaction<'static, sqlx::Sqlite>,
    organization_id: OrganizationId,
    workspace_id: WorkspaceId,
    session: &ConnectSession,
    at: Timestamp,
) -> Result<(), ConnectError> {
    let completed = session.status == ConnectSessionStatus::Completed;
    crate::webhooks::record_lifecycle(
        transaction,
        organization_id,
        workspace_id,
        if completed {
            WebhookEventType::ConnectSessionCompleted
        } else {
            WebhookEventType::ConnectSessionFailed
        },
        serde_json::json!({
            "object": "connect_session", "id": session.id, "status": session.status,
            "installation_id": if completed { session.installation_id } else { None },
            "target": session.target,
            "failure_code": session.failure_code,
        }),
        at,
    )
    .await
    .map_err(|_| ConnectError::Unavailable)
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
        audit: &Audit<'_, ConnectSettings>,
    ) -> Result<ConnectSettings, ConnectError> {
        let now = Timestamp::now();
        let urls = serde_json::to_string(return_urls).map_err(read)?;
        let origins = serde_json::to_string(allowed_origins).map_err(read)?;
        let mut transaction = self
            .database
            .pool()
            .begin_with("BEGIN IMMEDIATE")
            .await
            .map_err(failed("save_settings"))?;
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
        .execute(&mut *transaction)
        .await
        .map_err(failed("save_settings"))?;
        let settings = ConnectSettings {
            return_urls: return_urls.to_vec(),
            allowed_origins: allowed_origins.to_vec(),
            updated_at: Some(now),
        };
        crate::audit::record(&mut transaction, organization_id, &audit(&settings))
            .await
            .map_err(failed("audit"))?;
        transaction.commit().await.map_err(failed("save_settings"))?;
        Ok(settings)
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
            Some(InstalledApp::Catalog(id)) => (Some(id.uuid()), None),
            Some(InstalledApp::Workspace(id)) => (None, Some(id.uuid())),
            None => (None, None),
        };
        let (target_kind, target_provider, target_app_id, target_manager_id, target_connection_id) =
            target_columns(session.target.as_ref());
        // A new channel session names nothing it made yet.
        if session.app.is_some() == session.target.is_some()
            || target_manager_id.is_some()
            || target_connection_id.is_some()
        {
            return Err(read("a new session names one app or one fresh target"));
        }
        let scopes = serde_json::to_string(&session.requested_scopes).map_err(read)?;
        let expires_at = Timestamp::from_millis(now.millis() + SESSION_TTL_MS);
        // The scope is bound from the trust context; the workspace row only has
        // to exist in it, and in the environment the session names.
        let inserted = sqlx::query(
            "INSERT INTO connect_sessions (id, organization_id, workspace_id, environment, catalog_app_id, \
             workspace_app_id, configuration_version, intent, installation_id, label, status, return_url, \
             requested_scopes, reference_hash, created_at, expires_at, target_kind, target_provider, target_app_id) \
             SELECT ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, 'pending', ?, ?, ?, ?, ?, ?, ?, ? \
             FROM workspaces WHERE organization_id = ? AND id = ? AND environment = ? \
             AND (? IS NULL OR EXISTS (SELECT 1 FROM channel_apps a WHERE a.organization_id = workspaces.organization_id \
             AND a.workspace_id = workspaces.id AND a.id = ? AND a.status <> 'deleted'))",
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
        .bind(target_kind)
        .bind(target_provider)
        .bind(target_app_id)
        .bind(organization_id.uuid())
        .bind(workspace_id.uuid())
        .bind(session.environment.as_str())
        .bind(target_app_id)
        .bind(target_app_id)
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
            target: session.target,
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
            authorization_attempt: 0,
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
        audit: &Audit<'_, (ConnectSession, Installation)>,
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
        // Only an app's session completes with an installation; a channel's completes through the channel store.
        let Some(app) = session.app else {
            return Err(ConnectError::NotFound);
        };
        let installation = match session.intent {
            ConnectIntent::Create => {
                let installation = NewInstallation {
                    app,
                    label: session.label.clone(),
                    configuration_version: completion.configuration_version,
                    account: completion.account.clone(),
                    granted_scopes: completion.granted_scopes.clone(),
                    expires_at: completion.expires_at,
                    credential: completion.credential.clone(),
                    snapshot: completion.snapshot.clone(),
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
                    snapshot: &completion.snapshot,
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
        forget_authorization(&mut transaction, organization_id, workspace_id, session_id).await?;
        crate::webhooks::record_lifecycle(
            &mut transaction,
            organization_id,
            workspace_id,
            WebhookEventType::InstallationConnected,
            serde_json::json!({ "object": "installation", "id": installation.id,
                "app_id": installation.app.id(), "status": "connected" }),
            now,
        )
        .await
        .map_err(|_| ConnectError::Unavailable)?;
        record_outcome(&mut transaction, organization_id, workspace_id, &session, now).await?;
        let completed = (session, installation);
        crate::audit::record_in_workspace(&mut transaction, organization_id, workspace_id, &audit(&completed))
            .await
            .map_err(failed("audit"))?;
        transaction.commit().await.map_err(failed("complete_session"))?;
        Ok(completed)
    }

    async fn begin_authorization(
        &self,
        organization_id: OrganizationId,
        workspace_id: WorkspaceId,
        session_id: ConnectSessionId,
        authorization: &Authorization,
        now: Timestamp,
    ) -> Result<(), ConnectError> {
        let mut transaction = self
            .database
            .pool()
            .begin_with("BEGIN IMMEDIATE")
            .await
            .map_err(failed("begin_authorization"))?;
        let advanced = sqlx::query(
            "UPDATE connect_sessions SET authorization_attempt = ? \
             WHERE organization_id = ? AND workspace_id = ? AND id = ? AND status = 'pending' AND expires_at > ? \
             AND authorization_attempt = ?",
        )
        .bind(i64::from(authorization.attempt))
        .bind(organization_id.uuid())
        .bind(workspace_id.uuid())
        .bind(session_id.uuid())
        .bind(now.millis())
        .bind(i64::from(authorization.attempt) - 1)
        .execute(&mut *transaction)
        .await
        .map_err(failed("begin_authorization"))?;
        if advanced.rows_affected() == 0 {
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
            .map_err(failed("begin_authorization"))?;
            let session = session_from(&row.ok_or(ConnectError::NotFound)?)?;
            return Err(if session.status_at(now) == ConnectSessionStatus::Pending {
                ConnectError::AuthorizationChanged
            } else {
                ConnectError::NotPending
            });
        }
        forget_authorization(&mut transaction, organization_id, workspace_id, session_id).await?;
        sqlx::query(
            "INSERT INTO connect_authorizations (id, organization_id, workspace_id, session_id, attempt, state_hash, \
             key_id, wrapped_key, nonce, ciphertext, created_at) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
        )
        .bind(Uuid::now_v7())
        .bind(organization_id.uuid())
        .bind(workspace_id.uuid())
        .bind(session_id.uuid())
        .bind(i64::from(authorization.attempt))
        .bind(authorization.state_hash.0.as_slice())
        .bind(&authorization.sealed.wrapped_key.key_id)
        .bind(&authorization.sealed.wrapped_key.bytes)
        .bind(authorization.sealed.nonce.as_slice())
        .bind(&authorization.sealed.ciphertext)
        .bind(now.millis())
        .execute(&mut *transaction)
        .await
        .map_err(failed("begin_authorization"))?;
        transaction.commit().await.map_err(failed("begin_authorization"))?;
        Ok(())
    }

    async fn claim_authorization(
        &self,
        state_hash: &ReferenceHash,
    ) -> Result<Option<(ConnectSession, Authorization)>, ConnectError> {
        let mut transaction = self
            .database
            .pool()
            .begin_with("BEGIN IMMEDIATE")
            .await
            .map_err(failed("claim_authorization"))?;
        // Cross-tenant by design: the hash is unique across the deployment,
        // and deleting the row spends the `state`.
        let row = sqlx::query(
            "DELETE FROM connect_authorizations WHERE state_hash = ? \
             RETURNING organization_id, workspace_id, session_id, attempt, key_id, wrapped_key, nonce, ciphertext",
        )
        .bind(state_hash.0.as_slice())
        .fetch_optional(&mut *transaction)
        .await
        .map_err(failed("claim_authorization"))?;
        let Some(row) = row else {
            return Ok(None);
        };
        let organization_id = OrganizationId::from_uuid(row.try_get("organization_id").map_err(read)?);
        let workspace_id = WorkspaceId::from_uuid(row.try_get("workspace_id").map_err(read)?);
        let session_id = ConnectSessionId::from_uuid(row.try_get("session_id").map_err(read)?);
        let attempt: i64 = row.try_get("attempt").map_err(read)?;
        let nonce: Vec<u8> = row.try_get("nonce").map_err(read)?;
        let authorization = Authorization {
            state_hash: *state_hash,
            attempt: u32::try_from(attempt).map_err(read)?,
            sealed: Sealed {
                wrapped_key: WrappedKey {
                    key_id: row.try_get("key_id").map_err(read)?,
                    bytes: row.try_get("wrapped_key").map_err(read)?,
                },
                nonce: nonce.try_into().map_err(|_| read("a nonce that isn't 12 bytes"))?,
                ciphertext: row.try_get("ciphertext").map_err(read)?,
            },
        };
        // Everything from here carries the scope the row named.
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
        .map_err(failed("claim_authorization"))?;
        let session = session_from(&row.ok_or_else(|| read("an authorization without its session"))?)?;
        transaction.commit().await.map_err(failed("claim_authorization"))?;
        Ok(Some((session, authorization)))
    }

    async fn fail_session(
        &self,
        organization_id: OrganizationId,
        workspace_id: WorkspaceId,
        session_id: ConnectSessionId,
        failure_code: &str,
        now: Timestamp,
    ) -> Result<ConnectSession, ConnectError> {
        let mut transaction = self
            .database
            .pool()
            .begin_with("BEGIN IMMEDIATE")
            .await
            .map_err(failed("fail_session"))?;
        let row = sqlx::query(concat!(
            "UPDATE connect_sessions SET status = 'failed', failure_code = ? \
             WHERE organization_id = ? AND workspace_id = ? AND id = ? AND status = 'pending' AND expires_at > ? \
             RETURNING ",
            columns!()
        ))
        .bind(failure_code)
        .bind(organization_id.uuid())
        .bind(workspace_id.uuid())
        .bind(session_id.uuid())
        .bind(now.millis())
        .fetch_optional(&mut *transaction)
        .await
        .map_err(failed("fail_session"))?;
        let Some(row) = row else {
            drop(transaction);
            return match self.find_session(organization_id, workspace_id, session_id).await? {
                Some(_) => Err(ConnectError::NotPending),
                None => Err(ConnectError::NotFound),
            };
        };
        let session = session_from(&row)?;
        forget_authorization(&mut transaction, organization_id, workspace_id, session_id).await?;
        record_outcome(&mut transaction, organization_id, workspace_id, &session, now).await?;
        transaction.commit().await.map_err(failed("fail_session"))?;
        Ok(session)
    }

    async fn expire_sessions_cross_tenant(&self, now: Timestamp, limit: u32) -> Result<usize, ConnectError> {
        let mut transaction = self
            .database
            .pool()
            .begin_with("BEGIN IMMEDIATE")
            .await
            .map_err(failed("expire_sessions"))?;
        // The only cross-tenant scan: bounded, due pending rows only. Every
        // write below repeats the scope this scan attributes to the session.
        let rows = sqlx::query(concat!(
            "SELECT ",
            columns!(),
            " FROM connect_sessions WHERE status = 'pending' AND expires_at <= ? ORDER BY expires_at, id LIMIT ?"
        ))
        .bind(now.millis())
        .bind(limit.clamp(1, 100))
        .fetch_all(&mut *transaction)
        .await
        .map_err(failed("expire_sessions"))?;
        let mut expired = 0;
        for row in rows {
            let session = session_from(&row)?;
            let organization_id = session.organization_id;
            let workspace_id = session.workspace_id;
            let row = sqlx::query(concat!(
                "UPDATE connect_sessions SET status = 'expired' WHERE organization_id = ? AND workspace_id = ? \
                 AND id = ? AND status = 'pending' AND expires_at <= ? RETURNING ",
                columns!()
            ))
            .bind(organization_id.uuid())
            .bind(workspace_id.uuid())
            .bind(session.id.uuid())
            .bind(now.millis())
            .fetch_optional(&mut *transaction)
            .await
            .map_err(failed("expire_sessions"))?;
            if let Some(row) = row {
                let session = session_from(&row)?;
                // Expiry preserves the sealed authorization until session retention is settled.
                record_outcome(
                    &mut transaction,
                    organization_id,
                    workspace_id,
                    &session,
                    session.expires_at,
                )
                .await?;
                expired += 1;
            }
        }
        transaction.commit().await.map_err(failed("expire_sessions"))?;
        Ok(expired)
    }

    async fn cancel_session(
        &self,
        organization_id: OrganizationId,
        workspace_id: WorkspaceId,
        session_id: ConnectSessionId,
        now: Timestamp,
    ) -> Result<ConnectSession, ConnectError> {
        let mut transaction = self
            .database
            .pool()
            .begin_with("BEGIN IMMEDIATE")
            .await
            .map_err(failed("cancel_session"))?;
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
        .fetch_optional(&mut *transaction)
        .await
        .map_err(failed("cancel_session"))?;
        let Some(row) = row else {
            drop(transaction);
            return match self.find_session(organization_id, workspace_id, session_id).await? {
                Some(_) => Err(ConnectError::NotPending),
                None => Err(ConnectError::NotFound),
            };
        };
        let session = session_from(&row)?;
        forget_authorization(&mut transaction, organization_id, workspace_id, session_id).await?;
        record_outcome(&mut transaction, organization_id, workspace_id, &session, now).await?;
        transaction.commit().await.map_err(failed("cancel_session"))?;
        Ok(session)
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
                for (workspace, environment) in [
                    (tenant.sandbox, "sandbox"),
                    (tenant.sibling, "sandbox"),
                    (tenant.production, "production"),
                ] {
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
                    "INSERT INTO apps (id, organization_id, kind, name, key, description, icon_glyph, icon_color, \
                     auth, tools, version, created_at, updated_at) \
                     VALUES (?, ?, 'mcp', 'Catalog', 'catalog', '', 'C', '#0E7490', '{\"type\":\"token\"}', '[]', 1, 0, 0)",
                )
                .bind(tenant.catalog_app.uuid())
                .bind(org)
                .execute(pool)
                .await
                .unwrap();
                sqlx::query(
                    "INSERT INTO workspace_apps (id, organization_id, workspace_id, kind, name, key, description, \
                     icon_glyph, icon_color, auth, tools, version, created_at, updated_at) \
                     VALUES (?, ?, ?, 'mcp', 'Own', 'own', '', 'O', '#0E7490', '{\"type\":\"token\"}', '[]', 1, 0, 0)",
                )
                .bind(tenant.own_app.uuid())
                .bind(org)
                .bind(tenant.sandbox.uuid())
                .execute(pool)
                .await
                .unwrap();
                let short_id = crate::installations::record_test_short_id(pool, org, tenant.sandbox.uuid()).await;
                sqlx::query(
                    "INSERT INTO installations (id, organization_id, workspace_id, catalog_app_id, short_id, label, \
                     status, configuration_version, granted_scopes, created_at, updated_at) \
                     VALUES (?, ?, ?, ?, ?, 'Main', 'disconnected', 1, '[]', 0, 0)",
                )
                .bind(tenant.installation.uuid())
                .bind(org)
                .bind(tenant.sandbox.uuid())
                .bind(tenant.catalog_app.uuid())
                .bind(short_id)
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
        zupflow_conformance::connect_store::run(&SqliteConnectStore::new(database.clone()), prepare(database.clone()))
            .await;
    }

    #[tokio::test]
    async fn lifecycle_conforms() {
        let Temporary {
            database,
            directory: _directory,
        } = temporary().await;
        zupflow_conformance::connect_store::lifecycle::run(
            &SqliteConnectStore::new(database.clone()),
            &crate::SqliteInstallationStore::new(database.clone()),
            &crate::SqliteWebhookStore::new(database.clone()),
            &crate::SqliteAuditStore::new(database.clone()),
            &crate::SqliteJobQueue::new(database.clone()),
            prepare(database),
        )
        .await;
    }
    #[tokio::test]
    async fn lifecycle_rolls_back_if_event_or_delivery_job_cannot_be_written() {
        use zupflow_conformance::audit_store::{by_suite, by_suite_in};
        use zupflow_core::installations::InstallationStore;
        use zupflow_core::webhooks::{NewEndpoint, WebhookEventType, WebhookStore};
        let Temporary {
            database,
            directory: _directory,
        } = temporary().await;
        let t = Tenant {
            organization_id: OrganizationId::generate(),
            sandbox: WorkspaceId::generate(),
            sibling: WorkspaceId::generate(),
            production: WorkspaceId::generate(),
            catalog_app: AppId::generate(),
            own_app: AppId::generate(),
            installation: InstallationId::generate(),
        };
        prepare(database.clone())(t).await;
        let store = SqliteConnectStore::new(database.clone());
        let installations = crate::SqliteInstallationStore::new(database.clone());
        let credential = zupflow_core::installations::NewCredential {
            version: 1,
            sealed: Sealed {
                wrapped_key: WrappedKey {
                    key_id: "test".to_owned(),
                    bytes: vec![1; 40],
                },
                nonce: [1; 12],
                ciphertext: vec![1; 16],
            },
        };
        crate::SqliteWebhookStore::new(database.clone())
            .create_endpoint(
                t.organization_id,
                zupflow_core::ids::WebhookEndpointId::generate(),
                &NewEndpoint {
                    environment: Environment::Sandbox,
                    url: "https://hooks.example/atomic".to_owned(),
                    description: String::new(),
                    events: vec![
                        WebhookEventType::InstallationConnected,
                        WebhookEventType::InstallationDisconnected,
                        WebhookEventType::InstallationReconnectRequired,
                        WebhookEventType::ConnectSessionCompleted,
                        WebhookEventType::ConnectSessionFailed,
                    ],
                    secret: credential.sealed.clone(),
                    secret_last4: "last".to_owned(),
                },
                Timestamp::now(),
                &by_suite,
            )
            .await
            .unwrap();
        let now = Timestamp::now();
        let input = NewConnectSession {
            environment: Environment::Sandbox,
            app: Some(InstalledApp::Catalog(t.catalog_app)),
            target: None,
            configuration_version: 1,
            intent: ConnectIntent::Create,
            installation_id: None,
            label: "Private label".to_owned(),
            return_url: None,
            requested_scopes: vec![],
            reference_hash: zupflow_core::connect::Reference::generate().unwrap().hash(),
        };
        let session = store
            .create_session(t.organization_id, t.sandbox, ConnectSessionId::generate(), &input, now)
            .await
            .unwrap();
        let completion = Completion {
            installation_id: InstallationId::generate(),
            configuration_version: 1,
            account: zupflow_core::installations::ConnectedAccount {
                external_id: "private-account".to_owned(),
                display_name: "Private name".to_owned(),
                email: None,
            },
            granted_scopes: vec![],
            expires_at: None,
            credential,
            snapshot: zupflow_conformance::installation_store::snapshot(1),
        };
        let completion_audit = |_: &(ConnectSession, Installation)| by_suite_in(t.sandbox);
        let installation_audit = |_: &Installation| by_suite_in(t.sandbox);
        for trigger in [
            "CREATE TRIGGER IF NOT EXISTS refuse_lifecycle BEFORE INSERT ON webhook_events BEGIN SELECT RAISE(ABORT, 'unavailable'); END",
            "CREATE TRIGGER IF NOT EXISTS refuse_lifecycle BEFORE INSERT ON jobs BEGIN SELECT RAISE(ABORT, 'unavailable'); END",
        ] {
            sqlx::query(trigger)
                .persistent(false)
                .execute(database.pool())
                .await
                .unwrap();
            let definition: String = sqlx::query_scalar(
                "SELECT sql FROM sqlite_schema WHERE type = 'trigger' AND name = 'refuse_lifecycle'",
            )
            .fetch_one(database.pool())
            .await
            .unwrap();
            assert_eq!(definition, trigger.replace("IF NOT EXISTS ", ""));
            assert_eq!(
                store
                    .complete_session(
                        t.organization_id,
                        t.sandbox,
                        session.id,
                        &completion,
                        now,
                        &completion_audit
                    )
                    .await,
                Err(ConnectError::Unavailable)
            );
            assert_eq!(
                store
                    .cancel_session(t.organization_id, t.sandbox, session.id, now)
                    .await,
                Err(ConnectError::Unavailable)
            );
            assert_eq!(
                store
                    .fail_session(t.organization_id, t.sandbox, session.id, "consent_denied", now)
                    .await,
                Err(ConnectError::Unavailable)
            );
            assert_eq!(
                store.expire_sessions_cross_tenant(session.expires_at, 100).await,
                Err(ConnectError::Unavailable)
            );
            assert_eq!(
                store
                    .find_session(t.organization_id, t.sandbox, session.id)
                    .await
                    .unwrap()
                    .unwrap()
                    .status,
                ConnectSessionStatus::Pending
            );
            assert!(
                installations
                    .find_installation(t.organization_id, t.sandbox, completion.installation_id)
                    .await
                    .unwrap()
                    .is_none()
            );
            let count: i64 = sqlx::query_scalar(
                "SELECT count(*) FROM webhook_events WHERE organization_id = ? AND workspace_id = ?",
            )
            .bind(t.organization_id.uuid())
            .bind(t.sandbox.uuid())
            .fetch_one(database.pool())
            .await
            .unwrap();
            assert_eq!(count, 0);
            let audits: i64 =
                sqlx::query_scalar("SELECT count(*) FROM audit_events WHERE organization_id = ? AND workspace_id = ?")
                    .bind(t.organization_id.uuid())
                    .bind(t.sandbox.uuid())
                    .fetch_one(database.pool())
                    .await
                    .unwrap();
            assert_eq!(audits, 0);
            sqlx::query("DROP TRIGGER IF EXISTS refuse_lifecycle")
                .persistent(false)
                .execute(database.pool())
                .await
                .unwrap();
        }
        store
            .complete_session(
                t.organization_id,
                t.sandbox,
                session.id,
                &completion,
                now,
                &completion_audit,
            )
            .await
            .unwrap();
        sqlx::query(
            "CREATE TRIGGER IF NOT EXISTS refuse_lifecycle BEFORE INSERT ON jobs BEGIN SELECT RAISE(ABORT, 'unavailable'); END",
        )
        .execute(database.pool())
        .await
        .unwrap();
        assert_eq!(
            installations
                .disconnect_installation(
                    t.organization_id,
                    t.sandbox,
                    completion.installation_id,
                    &installation_audit
                )
                .await,
            Err(InstallationError::Unavailable)
        );
        assert_eq!(
            installations
                .delete_installation(
                    t.organization_id,
                    t.sandbox,
                    completion.installation_id,
                    &installation_audit
                )
                .await,
            Err(InstallationError::Unavailable)
        );
        assert_eq!(
            installations
                .require_reconnect(
                    t.organization_id,
                    t.sandbox,
                    completion.installation_id,
                    1,
                    now,
                    &installation_audit
                )
                .await,
            Err(InstallationError::Unavailable)
        );
        assert_eq!(
            installations
                .find_installation(t.organization_id, t.sandbox, completion.installation_id)
                .await
                .unwrap()
                .unwrap()
                .status,
            zupflow_core::installations::InstallationStatus::Connected
        );
        assert!(
            installations
                .find_credential(t.organization_id, t.sandbox, completion.installation_id)
                .await
                .unwrap()
                .is_some()
        );
        sqlx::query("DROP TRIGGER IF EXISTS refuse_lifecycle")
            .execute(database.pool())
            .await
            .unwrap();
        installations
            .require_reconnect(
                t.organization_id,
                t.sandbox,
                completion.installation_id,
                1,
                now,
                &installation_audit,
            )
            .await
            .unwrap();
        let snapshots: Vec<(Vec<u8>, String)> = sqlx::query_as(
            "SELECT id, body FROM webhook_events WHERE organization_id = ? AND workspace_id = ? ORDER BY id",
        )
        .bind(t.organization_id.uuid())
        .bind(t.sandbox.uuid())
        .fetch_all(database.pool())
        .await
        .unwrap();
        assert_eq!(snapshots.len(), 3);
        sqlx::query("UPDATE workspaces SET external_id = 'changed' WHERE organization_id = ? AND id = ?")
            .bind(t.organization_id.uuid())
            .bind(t.sandbox.uuid())
            .execute(database.pool())
            .await
            .unwrap();
        let reconnect = store
            .create_session(
                t.organization_id,
                t.sandbox,
                ConnectSessionId::generate(),
                &NewConnectSession {
                    intent: ConnectIntent::Reconnect,
                    installation_id: Some(completion.installation_id),
                    reference_hash: zupflow_core::connect::Reference::generate().unwrap().hash(),
                    ..input
                },
                now,
            )
            .await
            .unwrap();
        let mut next = completion.clone();
        next.credential.version = 2;
        store
            .complete_session(
                t.organization_id,
                t.sandbox,
                reconnect.id,
                &next,
                now,
                &completion_audit,
            )
            .await
            .unwrap();
        installations
            .delete_installation(
                t.organization_id,
                t.sandbox,
                completion.installation_id,
                &installation_audit,
            )
            .await
            .unwrap();
        for (id, body) in snapshots {
            let saved: String = sqlx::query_scalar(
                "SELECT body FROM webhook_events WHERE organization_id = ? AND workspace_id = ? AND id = ?",
            )
            .bind(t.organization_id.uuid())
            .bind(t.sandbox.uuid())
            .bind(id)
            .fetch_one(database.pool())
            .await
            .unwrap();
            assert_eq!(
                saved, body,
                "reconnect, workspace edits and deletion cannot rewrite queued events"
            );
            let body: serde_json::Value = serde_json::from_str(&saved).unwrap();
            assert_eq!(body["workspace"]["external_id"], serde_json::Value::Null);
            if body["type"] == "installation.reconnect_required" {
                assert_eq!(
                    body["data"],
                    serde_json::json!({ "object": "installation", "id": completion.installation_id,
                    "app_id": t.catalog_app, "status": "reconnect_required" })
                );
            }
        }
    }
}
