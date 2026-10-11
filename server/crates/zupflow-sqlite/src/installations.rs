//! [`SqliteInstallationStore`]: installations and their sealed credentials.
//! Every statement repeats `organization_id` and `workspace_id`, beside the
//! installation's ID; the catalog-wide list repeats the organization and the
//! app, and joins workspaces on both columns.

use async_trait::async_trait;
use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use sqlx::sqlite::SqliteRow;
use sqlx::{QueryBuilder, Row, Sqlite, Transaction};
use uuid::Uuid;
use zupflow_core::audit::Audit;
use zupflow_core::environment::Environment;
use zupflow_core::ids::{AppId, InstallationId, OrganizationId, WorkspaceId};
use zupflow_core::installations::{
    AccountConnection, AppInstallation, ConnectedAccount, Installation, InstallationError, InstallationStatus,
    InstallationStore, InstalledApp, NewCredential, NewInstallation, RefreshClaim, TemplateSnapshot,
};
use zupflow_core::keys::{Sealed, WrappedKey};
use zupflow_core::time::Timestamp;
use zupflow_core::workspaces::Page;

use crate::Database;

pub struct SqliteInstallationStore {
    database: Database,
}

impl SqliteInstallationStore {
    pub fn new(database: Database) -> Self {
        Self { database }
    }

    async fn immediate(&self) -> Result<Transaction<'static, Sqlite>, InstallationError> {
        self.database
            .pool()
            .begin_with("BEGIN IMMEDIATE")
            .await
            .map_err(failed("begin"))
    }
}

fn failed(operation: &'static str) -> impl FnOnce(sqlx::Error) -> InstallationError {
    move |error| {
        tracing::error!(event = "installations.database_error", operation, %error);
        InstallationError::Unavailable
    }
}

fn read(error: impl std::fmt::Display) -> InstallationError {
    tracing::error!(event = "installations.unreadable_row", %error);
    InstallationError::Unavailable
}

/// The columns `installation_from` reads, from `installations i`. A macro,
/// so queries stay static strings.
macro_rules! columns {
    () => {
        "i.id, i.organization_id, i.workspace_id, i.catalog_app_id, i.workspace_app_id, i.label, i.status, \
         i.configuration_version, i.account_external_id, i.account_display_name, i.account_email, \
         i.granted_scopes, i.credential_version, i.connected_at, i.expires_at, i.created_at, i.updated_at, \
         i.auth_template, i.app_version, i.oauth_client"
    };
}

/// The same columns, unqualified, for `RETURNING`.
macro_rules! returning {
    () => {
        "id, organization_id, workspace_id, catalog_app_id, workspace_app_id, label, status, \
         configuration_version, account_external_id, account_display_name, account_email, \
         granted_scopes, credential_version, connected_at, expires_at, created_at, updated_at, auth_template, \
         app_version, oauth_client"
    };
}

fn installation_from(row: &SqliteRow) -> Result<Installation, InstallationError> {
    let catalog: Option<Uuid> = row.try_get("catalog_app_id").map_err(read)?;
    let own: Option<Uuid> = row.try_get("workspace_app_id").map_err(read)?;
    let app = match (catalog, own) {
        (Some(id), None) => InstalledApp::Catalog(AppId::from_uuid(id)),
        (None, Some(id)) => InstalledApp::Workspace(AppId::from_uuid(id)),
        _ => return Err(read("an installation of no app, or of two")),
    };
    let status: String = row.try_get("status").map_err(read)?;
    let configuration_version: i64 = row.try_get("configuration_version").map_err(read)?;
    let external_id: Option<String> = row.try_get("account_external_id").map_err(read)?;
    let connection = match external_id {
        None => None,
        Some(external_id) => {
            let scopes: String = row.try_get("granted_scopes").map_err(read)?;
            let credential_version: i64 = row.try_get("credential_version").map_err(read)?;
            let expires_at: Option<i64> = row.try_get("expires_at").map_err(read)?;
            Some(AccountConnection {
                account: ConnectedAccount {
                    external_id,
                    display_name: row.try_get("account_display_name").map_err(read)?,
                    email: row.try_get("account_email").map_err(read)?,
                },
                granted_scopes: serde_json::from_str(&scopes).map_err(read)?,
                credential_version: u32::try_from(credential_version).map_err(read)?,
                connected_at: Timestamp::from_millis(row.try_get("connected_at").map_err(read)?),
                expires_at: expires_at.map(Timestamp::from_millis),
            })
        }
    };
    let auth_template: Option<String> = row.try_get("auth_template").map_err(read)?;
    let app_version: Option<i64> = row.try_get("app_version").map_err(read)?;
    let oauth_client: Option<String> = row.try_get("oauth_client").map_err(read)?;
    let snapshot = match (auth_template, app_version) {
        (Some(auth), Some(version)) => Some(TemplateSnapshot {
            auth: serde_json::from_str(&auth).map_err(read)?,
            app_version: u32::try_from(version).map_err(read)?,
            client: oauth_client
                .map(|client| serde_json::from_str(&client))
                .transpose()
                .map_err(read)?,
        }),
        (None, None) => None,
        _ => return Err(read("half a template snapshot")),
    };
    Ok(Installation {
        id: InstallationId::from_uuid(row.try_get("id").map_err(read)?),
        organization_id: OrganizationId::from_uuid(row.try_get("organization_id").map_err(read)?),
        workspace_id: WorkspaceId::from_uuid(row.try_get("workspace_id").map_err(read)?),
        app,
        label: row.try_get("label").map_err(read)?,
        status: InstallationStatus::parse(&status).ok_or_else(|| read("an unknown status"))?,
        configuration_version: u32::try_from(configuration_version).map_err(read)?,
        connection,
        snapshot,
        created_at: Timestamp::from_millis(row.try_get("created_at").map_err(read)?),
        updated_at: Timestamp::from_millis(row.try_get("updated_at").map_err(read)?),
    })
}

/// Inserts a connected installation and its credential, inside the caller's
/// transaction. The caller has checked the app belongs to the scope.
pub(crate) async fn insert_connected(
    transaction: &mut Transaction<'static, Sqlite>,
    organization_id: OrganizationId,
    workspace_id: WorkspaceId,
    installation_id: InstallationId,
    installation: &NewInstallation,
) -> Result<Installation, InstallationError> {
    let (catalog_app_id, workspace_app_id) = match installation.app {
        InstalledApp::Catalog(id) => (Some(id.uuid()), None),
        InstalledApp::Workspace(id) => (None, Some(id.uuid())),
    };
    let scopes = serde_json::to_string(&installation.granted_scopes).map_err(read)?;
    let auth_template = serde_json::to_string(&installation.snapshot.auth).map_err(read)?;
    let oauth_client = installation
        .snapshot
        .client
        .as_ref()
        .map(serde_json::to_string)
        .transpose()
        .map_err(read)?;
    let now = Timestamp::now().millis();
    let row = sqlx::query(concat!(
        "INSERT INTO installations (id, organization_id, workspace_id, catalog_app_id, workspace_app_id, label, \
         status, configuration_version, account_external_id, account_display_name, account_email, \
         granted_scopes, credential_version, connected_at, expires_at, created_at, updated_at, auth_template, \
         app_version, oauth_client) \
         VALUES (?, ?, ?, ?, ?, ?, 'connected', ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?) RETURNING ",
        returning!()
    ))
    .bind(installation_id.uuid())
    .bind(organization_id.uuid())
    .bind(workspace_id.uuid())
    .bind(catalog_app_id)
    .bind(workspace_app_id)
    .bind(&installation.label)
    .bind(i64::from(installation.configuration_version))
    .bind(&installation.account.external_id)
    .bind(&installation.account.display_name)
    .bind(&installation.account.email)
    .bind(scopes)
    .bind(i64::from(installation.credential.version))
    .bind(now)
    .bind(installation.expires_at.map(Timestamp::millis))
    .bind(now)
    .bind(now)
    .bind(auth_template)
    .bind(i64::from(installation.snapshot.app_version))
    .bind(oauth_client)
    .fetch_one(&mut **transaction)
    .await
    .map_err(failed("insert_installation"))?;
    insert_credential(
        transaction,
        organization_id,
        workspace_id,
        installation_id,
        &installation.credential,
        now,
    )
    .await?;
    installation_from(&row)
}

async fn insert_credential(
    transaction: &mut Transaction<'static, Sqlite>,
    organization_id: OrganizationId,
    workspace_id: WorkspaceId,
    installation_id: InstallationId,
    credential: &NewCredential,
    now: i64,
) -> Result<(), InstallationError> {
    sqlx::query(
        "INSERT INTO installation_credentials (id, organization_id, workspace_id, installation_id, version, \
         key_id, wrapped_key, nonce, ciphertext, created_at) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
    )
    .bind(Uuid::now_v7())
    .bind(organization_id.uuid())
    .bind(workspace_id.uuid())
    .bind(installation_id.uuid())
    .bind(i64::from(credential.version))
    .bind(&credential.sealed.wrapped_key.key_id)
    .bind(&credential.sealed.wrapped_key.bytes)
    .bind(credential.sealed.nonce.as_slice())
    .bind(&credential.sealed.ciphertext)
    .bind(now)
    .execute(&mut **transaction)
    .await
    .map_err(failed("insert_credential"))?;
    Ok(())
}

/// Connects the installation again, inside the caller's transaction: its
/// account, scopes and expiry, and `credential`, which must be the stored
/// version plus one (1 if it never held one); its old credential goes.
/// `None` when it isn't in this scope or another version was stored since.
pub(crate) async fn reconnect(
    transaction: &mut Transaction<'static, Sqlite>,
    organization_id: OrganizationId,
    workspace_id: WorkspaceId,
    installation_id: InstallationId,
    renewal: &Renewal<'_>,
) -> Result<Option<Installation>, InstallationError> {
    let scopes = serde_json::to_string(renewal.granted_scopes).map_err(read)?;
    let auth_template = serde_json::to_string(&renewal.snapshot.auth).map_err(read)?;
    let oauth_client = renewal
        .snapshot
        .client
        .as_ref()
        .map(serde_json::to_string)
        .transpose()
        .map_err(read)?;
    let now = Timestamp::now().millis();
    let credential = renewal.credential;
    let row = sqlx::query(concat!(
        "UPDATE installations SET status = 'connected', refresh_started_at = NULL, configuration_version = ?, account_external_id = ?, \
         account_display_name = ?, account_email = ?, granted_scopes = ?, credential_version = ?, \
         connected_at = ?, expires_at = ?, updated_at = ?, auth_template = ?, app_version = ?, oauth_client = ? \
         WHERE organization_id = ? AND workspace_id = ? AND id = ? AND coalesce(credential_version, 0) = ? RETURNING ",
        returning!()
    ))
    .bind(i64::from(renewal.configuration_version))
    .bind(&renewal.account.external_id)
    .bind(&renewal.account.display_name)
    .bind(&renewal.account.email)
    .bind(scopes)
    .bind(i64::from(credential.version))
    .bind(now)
    .bind(renewal.expires_at.map(Timestamp::millis))
    .bind(now)
    .bind(auth_template)
    .bind(i64::from(renewal.snapshot.app_version))
    .bind(oauth_client)
    .bind(organization_id.uuid())
    .bind(workspace_id.uuid())
    .bind(installation_id.uuid())
    .bind(i64::from(credential.version) - 1)
    .fetch_optional(&mut **transaction)
    .await
    .map_err(failed("reconnect_installation"))?;
    let Some(row) = row else {
        return Ok(None);
    };
    sqlx::query(
        "DELETE FROM installation_credentials WHERE organization_id = ? AND workspace_id = ? AND installation_id = ?",
    )
    .bind(organization_id.uuid())
    .bind(workspace_id.uuid())
    .bind(installation_id.uuid())
    .execute(&mut **transaction)
    .await
    .map_err(failed("reconnect_installation"))?;
    insert_credential(
        transaction,
        organization_id,
        workspace_id,
        installation_id,
        credential,
        now,
    )
    .await?;
    installation_from(&row).map(Some)
}

/// What [`reconnect`] writes.
pub(crate) struct Renewal<'a> {
    pub configuration_version: u32,
    pub account: &'a ConnectedAccount,
    pub granted_scopes: &'a [String],
    pub expires_at: Option<Timestamp>,
    pub credential: &'a NewCredential,
    pub snapshot: &'a TemplateSnapshot,
}

/// A position in the catalog-wide list: newest first, then by ID.
struct Position {
    created_at: i64,
    id: Uuid,
}

impl Position {
    /// The installation's prefixed ID, never its bare UUID: a cursor leaves the server.
    fn encode(&self) -> String {
        URL_SAFE_NO_PAD.encode(format!("{}\n{}", self.created_at, InstallationId::from_uuid(self.id)))
    }

    fn decode(cursor: &str) -> Option<Self> {
        let raw = String::from_utf8(URL_SAFE_NO_PAD.decode(cursor).ok()?).ok()?;
        let (created_at, id) = raw.split_once('\n')?;
        Some(Self {
            created_at: created_at.parse().ok()?,
            id: InstallationId::parse(id).ok()?.uuid(),
        })
    }
}

/// The catalog-wide list's conditions: the organization and the app first.
fn push_app_filter(
    query: &mut QueryBuilder<Sqlite>,
    organization_id: OrganizationId,
    app_id: AppId,
    environment: Option<Environment>,
) {
    query.push(
        " FROM installations i JOIN workspaces w ON w.organization_id = i.organization_id AND w.id = i.workspace_id \
         WHERE i.organization_id = ",
    );
    query.push_bind(organization_id.uuid());
    query.push(" AND i.catalog_app_id = ");
    query.push_bind(app_id.uuid());
    if let Some(environment) = environment {
        query.push(" AND w.environment = ");
        query.push_bind(environment.as_str());
    }
}

#[async_trait]
impl InstallationStore for SqliteInstallationStore {
    async fn list_installations(
        &self,
        organization_id: OrganizationId,
        workspace_id: WorkspaceId,
    ) -> Result<Vec<Installation>, InstallationError> {
        let rows = sqlx::query(concat!(
            "SELECT ",
            columns!(),
            " FROM installations i WHERE i.organization_id = ? AND i.workspace_id = ? \
             ORDER BY i.created_at DESC, i.id DESC"
        ))
        .bind(organization_id.uuid())
        .bind(workspace_id.uuid())
        .fetch_all(self.database.pool())
        .await
        .map_err(failed("list_installations"))?;
        rows.iter().map(installation_from).collect()
    }

    async fn find_installation(
        &self,
        organization_id: OrganizationId,
        workspace_id: WorkspaceId,
        installation_id: InstallationId,
    ) -> Result<Option<Installation>, InstallationError> {
        let row = sqlx::query(concat!(
            "SELECT ",
            columns!(),
            " FROM installations i WHERE i.organization_id = ? AND i.workspace_id = ? AND i.id = ?"
        ))
        .bind(organization_id.uuid())
        .bind(workspace_id.uuid())
        .bind(installation_id.uuid())
        .fetch_optional(self.database.pool())
        .await
        .map_err(failed("find_installation"))?;
        row.as_ref().map(installation_from).transpose()
    }

    async fn create_installation(
        &self,
        organization_id: OrganizationId,
        workspace_id: WorkspaceId,
        installation_id: InstallationId,
        installation: &NewInstallation,
    ) -> Result<Installation, InstallationError> {
        let mut transaction = self.immediate().await?;
        let app = match installation.app {
            InstalledApp::Catalog(app_id) => {
                sqlx::query(
                    "SELECT 1 FROM workspaces w JOIN apps a ON a.organization_id = w.organization_id \
                     WHERE w.organization_id = ? AND w.id = ? AND a.id = ?",
                )
                .bind(organization_id.uuid())
                .bind(workspace_id.uuid())
                .bind(app_id.uuid())
                .fetch_optional(&mut *transaction)
                .await
            }
            InstalledApp::Workspace(app_id) => {
                sqlx::query("SELECT 1 FROM workspace_apps WHERE organization_id = ? AND workspace_id = ? AND id = ?")
                    .bind(organization_id.uuid())
                    .bind(workspace_id.uuid())
                    .bind(app_id.uuid())
                    .fetch_optional(&mut *transaction)
                    .await
            }
        }
        .map_err(failed("create_installation"))?;
        if app.is_none() {
            return Err(InstallationError::NotFound);
        }
        let created = insert_connected(
            &mut transaction,
            organization_id,
            workspace_id,
            installation_id,
            installation,
        )
        .await?;
        transaction.commit().await.map_err(failed("create_installation"))?;
        Ok(created)
    }

    async fn rename_installation(
        &self,
        organization_id: OrganizationId,
        workspace_id: WorkspaceId,
        installation_id: InstallationId,
        label: &str,
        audit: &Audit<'_, Installation>,
    ) -> Result<Installation, InstallationError> {
        let mut transaction = self.immediate().await?;
        let row = sqlx::query(concat!(
            "UPDATE installations SET label = ?, updated_at = ? \
             WHERE organization_id = ? AND workspace_id = ? AND id = ? RETURNING ",
            returning!()
        ))
        .bind(label)
        .bind(Timestamp::now().millis())
        .bind(organization_id.uuid())
        .bind(workspace_id.uuid())
        .bind(installation_id.uuid())
        .fetch_optional(&mut *transaction)
        .await
        .map_err(failed("rename_installation"))?;
        let installation = installation_from(&row.ok_or(InstallationError::NotFound)?)?;
        crate::audit::record_in_workspace(&mut transaction, organization_id, workspace_id, &audit(&installation))
            .await
            .map_err(failed("audit"))?;
        transaction.commit().await.map_err(failed("rename_installation"))?;
        Ok(installation)
    }

    async fn disconnect_installation(
        &self,
        organization_id: OrganizationId,
        workspace_id: WorkspaceId,
        installation_id: InstallationId,
        audit: &Audit<'_, Installation>,
    ) -> Result<Installation, InstallationError> {
        let mut transaction = self.immediate().await?;
        let row = sqlx::query(concat!(
            "UPDATE installations SET status = 'disconnected', refresh_started_at = NULL, updated_at = ? \
             WHERE organization_id = ? AND workspace_id = ? AND id = ? RETURNING ",
            returning!()
        ))
        .bind(Timestamp::now().millis())
        .bind(organization_id.uuid())
        .bind(workspace_id.uuid())
        .bind(installation_id.uuid())
        .fetch_optional(&mut *transaction)
        .await
        .map_err(failed("disconnect_installation"))?;
        let installation = installation_from(&row.ok_or(InstallationError::NotFound)?)?;
        sqlx::query(
            "DELETE FROM installation_credentials \
             WHERE organization_id = ? AND workspace_id = ? AND installation_id = ?",
        )
        .bind(organization_id.uuid())
        .bind(workspace_id.uuid())
        .bind(installation_id.uuid())
        .execute(&mut *transaction)
        .await
        .map_err(failed("disconnect_installation"))?;
        crate::audit::record_in_workspace(&mut transaction, organization_id, workspace_id, &audit(&installation))
            .await
            .map_err(failed("audit"))?;
        transaction.commit().await.map_err(failed("disconnect_installation"))?;
        Ok(installation)
    }

    async fn delete_installation(
        &self,
        organization_id: OrganizationId,
        workspace_id: WorkspaceId,
        installation_id: InstallationId,
        audit: &Audit<'_, Installation>,
    ) -> Result<(), InstallationError> {
        // The credential references the installation with a cascading key.
        let mut transaction = self.immediate().await?;
        let deleted = sqlx::query(concat!(
            "DELETE FROM installations WHERE organization_id = ? AND workspace_id = ? AND id = ? RETURNING ",
            returning!()
        ))
        .bind(organization_id.uuid())
        .bind(workspace_id.uuid())
        .bind(installation_id.uuid())
        .fetch_optional(&mut *transaction)
        .await
        .map_err(failed("delete_installation"))?;
        let installation = installation_from(&deleted.ok_or(InstallationError::NotFound)?)?;
        crate::audit::record_in_workspace(&mut transaction, organization_id, workspace_id, &audit(&installation))
            .await
            .map_err(failed("audit"))?;
        transaction.commit().await.map_err(failed("delete_installation"))?;
        Ok(())
    }

    async fn find_credential(
        &self,
        organization_id: OrganizationId,
        workspace_id: WorkspaceId,
        installation_id: InstallationId,
    ) -> Result<Option<NewCredential>, InstallationError> {
        let row = sqlx::query(
            "SELECT version, key_id, wrapped_key, nonce, ciphertext FROM installation_credentials \
             WHERE organization_id = ? AND workspace_id = ? AND installation_id = ?",
        )
        .bind(organization_id.uuid())
        .bind(workspace_id.uuid())
        .bind(installation_id.uuid())
        .fetch_optional(self.database.pool())
        .await
        .map_err(failed("find_credential"))?;
        let Some(row) = row else {
            return Ok(None);
        };
        let version: i64 = row.try_get("version").map_err(read)?;
        let nonce: Vec<u8> = row.try_get("nonce").map_err(read)?;
        Ok(Some(NewCredential {
            sealed: Sealed {
                wrapped_key: WrappedKey {
                    key_id: row.try_get("key_id").map_err(read)?,
                    bytes: row.try_get("wrapped_key").map_err(read)?,
                },
                nonce: nonce.try_into().map_err(|_| read("a nonce of the wrong length"))?,
                ciphertext: row.try_get("ciphertext").map_err(read)?,
            },
            version: u32::try_from(version).map_err(read)?,
        }))
    }

    async fn claim_refresh(
        &self,
        organization_id: OrganizationId,
        workspace_id: WorkspaceId,
        installation_id: InstallationId,
        version: u32,
        now: Timestamp,
    ) -> Result<RefreshClaim, InstallationError> {
        let mut transaction = self.immediate().await?;
        let row = sqlx::query("SELECT refresh_started_at FROM installations WHERE organization_id = ? AND workspace_id = ? AND id = ? AND status = 'connected' AND credential_version = ?")
            .bind(organization_id.uuid()).bind(workspace_id.uuid()).bind(installation_id.uuid()).bind(i64::from(version))
            .fetch_optional(&mut *transaction).await.map_err(failed("claim_refresh"))?;
        let Some(row) = row else {
            return Ok(RefreshClaim::Changed);
        };
        let started: Option<i64> = row.try_get("refresh_started_at").map_err(read)?;
        if let Some(started_at) = started {
            return Ok(RefreshClaim::Pending {
                started_at: Timestamp::from_millis(started_at),
            });
        }
        sqlx::query("UPDATE installations SET refresh_started_at = ? WHERE organization_id = ? AND workspace_id = ? AND id = ? AND status = 'connected' AND credential_version = ? AND refresh_started_at IS NULL")
            .bind(now.millis()).bind(organization_id.uuid()).bind(workspace_id.uuid()).bind(installation_id.uuid()).bind(i64::from(version))
            .execute(&mut *transaction).await.map_err(failed("claim_refresh"))?;
        transaction.commit().await.map_err(failed("claim_refresh"))?;
        Ok(RefreshClaim::Claimed)
    }

    async fn rotate_credential(
        &self,
        organization_id: OrganizationId,
        workspace_id: WorkspaceId,
        installation_id: InstallationId,
        credential: &NewCredential,
        expires_at: Option<Timestamp>,
        now: Timestamp,
    ) -> Result<bool, InstallationError> {
        let Some(previous) = credential.version.checked_sub(1).filter(|v| *v > 0) else {
            return Ok(false);
        };
        let mut transaction = self.immediate().await?;
        let changed = sqlx::query("UPDATE installations SET credential_version = ?, expires_at = ?, refresh_started_at = NULL, updated_at = ? WHERE organization_id = ? AND workspace_id = ? AND id = ? AND status = 'connected' AND credential_version = ? AND refresh_started_at IS NOT NULL")
            .bind(i64::from(credential.version)).bind(expires_at.map(Timestamp::millis)).bind(now.millis())
            .bind(organization_id.uuid()).bind(workspace_id.uuid()).bind(installation_id.uuid()).bind(i64::from(previous))
            .execute(&mut *transaction).await.map_err(failed("rotate_credential"))?.rows_affected();
        if changed == 0 {
            return Ok(false);
        }
        let replaced = sqlx::query("UPDATE installation_credentials SET version = ?, key_id = ?, wrapped_key = ?, nonce = ?, ciphertext = ?, created_at = ? WHERE organization_id = ? AND workspace_id = ? AND installation_id = ? AND version = ?")
            .bind(i64::from(credential.version)).bind(&credential.sealed.wrapped_key.key_id)
            .bind(&credential.sealed.wrapped_key.bytes).bind(credential.sealed.nonce.as_slice()).bind(&credential.sealed.ciphertext).bind(now.millis())
            .bind(organization_id.uuid()).bind(workspace_id.uuid()).bind(installation_id.uuid()).bind(i64::from(previous))
            .execute(&mut *transaction).await.map_err(failed("rotate_credential"))?.rows_affected();
        if replaced != 1 {
            return Err(InstallationError::Unavailable);
        }
        transaction.commit().await.map_err(failed("rotate_credential"))?;
        Ok(true)
    }

    async fn require_reconnect(
        &self,
        organization_id: OrganizationId,
        workspace_id: WorkspaceId,
        installation_id: InstallationId,
        version: u32,
        now: Timestamp,
        audit: &Audit<'_, Installation>,
    ) -> Result<bool, InstallationError> {
        let mut transaction = self.immediate().await?;
        let row = sqlx::query(concat!("UPDATE installations SET status = 'reconnect_required', refresh_started_at = NULL, updated_at = ? WHERE organization_id = ? AND workspace_id = ? AND id = ? AND status = 'connected' AND credential_version = ? RETURNING ", returning!()))
            .bind(now.millis()).bind(organization_id.uuid()).bind(workspace_id.uuid()).bind(installation_id.uuid()).bind(i64::from(version))
            .fetch_optional(&mut *transaction).await.map_err(failed("require_reconnect"))?;
        let Some(row) = row else {
            return Ok(false);
        };
        let installation = installation_from(&row)?;
        crate::audit::record_in_workspace(&mut transaction, organization_id, workspace_id, &audit(&installation))
            .await
            .map_err(failed("audit"))?;
        let job = zupflow_core::webhooks::Announcement::reconnect_required(installation_id, now)
            .job()
            .ok_or(InstallationError::Unavailable)?;
        crate::job_queue::insert_job(
            &mut *transaction,
            &zupflow_core::scope::Scope::Workspace(organization_id, workspace_id),
            &job.kind,
            job.payload,
            job.options,
        )
        .await
        .map_err(|_| InstallationError::Unavailable)?;
        transaction.commit().await.map_err(failed("require_reconnect"))?;
        Ok(true)
    }

    async fn list_app_installations(
        &self,
        organization_id: OrganizationId,
        app_id: AppId,
        environment: Option<Environment>,
        cursor: Option<&str>,
        limit: u32,
    ) -> Result<Page<AppInstallation>, InstallationError> {
        let after = cursor
            .map(|cursor| Position::decode(cursor).ok_or(InstallationError::InvalidCursor))
            .transpose()?;
        let limit = limit.clamp(1, 100);

        let mut count = QueryBuilder::<Sqlite>::new("SELECT count(*)");
        push_app_filter(&mut count, organization_id, app_id, environment);
        let total: i64 = count
            .build_query_scalar()
            .fetch_one(self.database.pool())
            .await
            .map_err(failed("list_app_installations"))?;

        let mut query = QueryBuilder::<Sqlite>::new(concat!(
            "SELECT ",
            columns!(),
            ", w.name AS workspace_name, w.slug AS workspace_slug, w.environment"
        ));
        push_app_filter(&mut query, organization_id, app_id, environment);
        if let Some(after) = &after {
            query.push(" AND (i.created_at < ");
            query.push_bind(after.created_at);
            query.push(" OR (i.created_at = ");
            query.push_bind(after.created_at);
            query.push(" AND i.id < ");
            query.push_bind(after.id);
            query.push("))");
        }
        query.push(" ORDER BY i.created_at DESC, i.id DESC LIMIT ");
        query.push_bind(i64::from(limit) + 1);
        let rows = query
            .build()
            .fetch_all(self.database.pool())
            .await
            .map_err(failed("list_app_installations"))?;

        let mut items = Vec::with_capacity(rows.len());
        for row in &rows {
            let environment: String = row.try_get("environment").map_err(read)?;
            items.push(AppInstallation {
                installation: installation_from(row)?,
                workspace_name: row.try_get("workspace_name").map_err(read)?,
                workspace_slug: row.try_get("workspace_slug").map_err(read)?,
                environment: Environment::parse(&environment).ok_or_else(|| read("an unknown environment"))?,
            });
        }
        let next_cursor = if items.len() > limit as usize {
            items.truncate(limit as usize);
            items.last().map(|last| {
                Position {
                    created_at: last.installation.created_at.millis(),
                    id: last.installation.id.uuid(),
                }
                .encode()
            })
        } else {
            None
        };
        Ok(Page {
            items,
            next_cursor,
            total: u64::try_from(total).map_err(read)?,
        })
    }

    async fn count_by_app(
        &self,
        organization_id: OrganizationId,
        environment: Option<Environment>,
    ) -> Result<Vec<(AppId, u64)>, InstallationError> {
        let rows = sqlx::query(
            "SELECT coalesce(i.catalog_app_id, i.workspace_app_id) AS app_id, count(*) AS installations \
             FROM installations i JOIN workspaces w ON w.organization_id = i.organization_id AND w.id = i.workspace_id \
             WHERE i.organization_id = ?1 AND (?2 IS NULL OR w.environment = ?2) GROUP BY 1",
        )
        .bind(organization_id.uuid())
        .bind(environment.map(Environment::as_str))
        .fetch_all(self.database.pool())
        .await
        .map_err(failed("count_by_app"))?;
        rows.iter()
            .map(|row| {
                let count: i64 = row.try_get("installations").map_err(read)?;
                Ok((
                    AppId::from_uuid(row.try_get("app_id").map_err(read)?),
                    u64::try_from(count).map_err(read)?,
                ))
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use std::sync::Arc;

    use zupflow_conformance::installation_store::{Prepare, Tenant};

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
                         VALUES (?, ?, ?, ?, ?, 0)",
                    )
                    .bind(workspace.uuid())
                    .bind(org)
                    .bind(environment)
                    .bind(format!("W {environment}"))
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
            })
        })
    }

    #[tokio::test]
    async fn conforms() {
        let Temporary {
            database,
            directory: _directory,
        } = temporary().await;
        zupflow_conformance::installation_store::run(
            &SqliteInstallationStore::new(database.clone()),
            &crate::SqliteAuditStore::new(database.clone()),
            &crate::SqliteJobQueue::new(database.clone()),
            prepare(database),
        )
        .await;
    }

    #[tokio::test]
    async fn reconnect_status_audit_and_outbox_commit_together_once() {
        use zupflow_core::audit::{AuditAction, AuditActor, AuditEntry, AuditObject, AuditPlace, AuditTarget};
        let Temporary {
            database,
            directory: _directory,
        } = temporary().await;
        let tenant = Tenant {
            organization_id: OrganizationId::generate(),
            sandbox: WorkspaceId::generate(),
            sibling: WorkspaceId::generate(),
            production: WorkspaceId::generate(),
            catalog_app: AppId::generate(),
            own_app: AppId::generate(),
        };
        prepare(database.clone())(tenant).await;
        let store = SqliteInstallationStore::new(database.clone());
        let installation = store
            .create_installation(
                tenant.organization_id,
                tenant.sandbox,
                InstallationId::generate(),
                &NewInstallation {
                    app: InstalledApp::Catalog(tenant.catalog_app),
                    label: "Board".to_owned(),
                    configuration_version: 1,
                    account: ConnectedAccount {
                        external_id: "one".to_owned(),
                        display_name: "Robin".to_owned(),
                        email: None,
                    },
                    granted_scopes: vec![],
                    expires_at: None,
                    credential: NewCredential {
                        sealed: Sealed {
                            wrapped_key: WrappedKey {
                                key_id: "local".to_owned(),
                                bytes: vec![1; 40],
                            },
                            nonce: [1; 12],
                            ciphertext: vec![1; 16],
                        },
                        version: 1,
                    },
                    snapshot: zupflow_conformance::installation_store::snapshot(1),
                },
            )
            .await
            .unwrap();
        let audit = |i: &Installation| {
            AuditEntry::new(
                AuditActor::System {
                    name: "Credentials".to_owned(),
                },
                AuditAction::InstallationReconnectRequired,
                AuditTarget::new(AuditObject::Installation(i.id), &i.label),
                AuditPlace::Workspace(tenant.sandbox),
            )
        };
        sqlx::query("CREATE TRIGGER refuse_outbox BEFORE INSERT ON jobs BEGIN SELECT RAISE(ABORT, 'unavailable'); END")
            .execute(database.pool())
            .await
            .unwrap();
        assert_eq!(
            store
                .require_reconnect(
                    tenant.organization_id,
                    tenant.sandbox,
                    installation.id,
                    1,
                    Timestamp::now(),
                    &audit
                )
                .await,
            Err(InstallationError::Unavailable)
        );
        assert_eq!(
            store
                .find_installation(tenant.organization_id, tenant.sandbox, installation.id)
                .await
                .unwrap()
                .unwrap()
                .status,
            InstallationStatus::Connected
        );
        let count: i64 =
            sqlx::query_scalar("SELECT count(*) FROM audit_events WHERE organization_id = ? AND workspace_id = ?")
                .bind(tenant.organization_id.uuid())
                .bind(tenant.sandbox.uuid())
                .fetch_one(database.pool())
                .await
                .unwrap();
        assert_eq!(count, 0);
        sqlx::query("DROP TRIGGER refuse_outbox")
            .execute(database.pool())
            .await
            .unwrap();
        assert!(
            store
                .require_reconnect(
                    tenant.organization_id,
                    tenant.sandbox,
                    installation.id,
                    1,
                    Timestamp::now(),
                    &audit
                )
                .await
                .unwrap()
        );
        assert!(
            !store
                .require_reconnect(
                    tenant.organization_id,
                    tenant.sandbox,
                    installation.id,
                    1,
                    Timestamp::now(),
                    &audit
                )
                .await
                .unwrap()
        );
        let audit_count: i64 =
            sqlx::query_scalar("SELECT count(*) FROM audit_events WHERE organization_id = ? AND workspace_id = ?")
                .bind(tenant.organization_id.uuid())
                .bind(tenant.sandbox.uuid())
                .fetch_one(database.pool())
                .await
                .unwrap();
        assert_eq!(audit_count, 1);
        let jobs: i64 = sqlx::query_scalar("SELECT count(*) FROM jobs")
            .fetch_one(database.pool())
            .await
            .unwrap();
        assert_eq!(jobs, 1);
    }

    /// Deleting a workspace takes its installations and their credentials,
    /// and nothing of a sibling's.
    #[tokio::test]
    async fn deletes_a_workspaces_installations_with_it() {
        use zupflow_core::workspaces::WorkspaceStore;

        let Temporary {
            database,
            directory: _directory,
        } = temporary().await;
        let tenant = Tenant {
            organization_id: OrganizationId::generate(),
            sandbox: WorkspaceId::generate(),
            sibling: WorkspaceId::generate(),
            production: WorkspaceId::generate(),
            catalog_app: AppId::generate(),
            own_app: AppId::generate(),
        };
        prepare(database.clone())(tenant).await;
        let store = SqliteInstallationStore::new(database.clone());
        let credential = NewCredential {
            sealed: Sealed {
                wrapped_key: WrappedKey {
                    key_id: "k".to_owned(),
                    bytes: vec![1; 40],
                },
                nonce: [1; 12],
                ciphertext: vec![1; 16],
            },
            version: 1,
        };
        let new = |app| NewInstallation {
            app,
            label: "Main".to_owned(),
            configuration_version: 1,
            account: ConnectedAccount {
                external_id: "a".to_owned(),
                display_name: "A".to_owned(),
                email: None,
            },
            granted_scopes: Vec::new(),
            expires_at: None,
            credential: credential.clone(),
            snapshot: zupflow_conformance::installation_store::snapshot(1),
        };
        let org = tenant.organization_id;
        let gone = store
            .create_installation(
                org,
                tenant.sandbox,
                InstallationId::generate(),
                &new(InstalledApp::Workspace(tenant.own_app)),
            )
            .await
            .unwrap();
        let kept = store
            .create_installation(
                org,
                tenant.production,
                InstallationId::generate(),
                &new(InstalledApp::Catalog(tenant.catalog_app)),
            )
            .await
            .unwrap();
        crate::SqliteWorkspaceStore::new(database.clone())
            .delete_workspace(org, tenant.sandbox, &|_| {
                zupflow_conformance::audit_store::by_suite_in(tenant.sandbox)
            })
            .await
            .unwrap();
        assert_eq!(
            store.find_installation(org, tenant.sandbox, gone.id).await.unwrap(),
            None
        );
        assert_eq!(store.find_credential(org, tenant.sandbox, gone.id).await.unwrap(), None);
        let left: i64 = sqlx::query_scalar("SELECT count(*) FROM installation_credentials")
            .fetch_one(database.pool())
            .await
            .unwrap();
        assert_eq!(left, 1, "only the sibling's credential is left");
        assert_eq!(
            store.find_installation(org, tenant.production, kept.id).await.unwrap(),
            Some(kept.clone())
        );

        // A snapshot is whole or absent: the database refuses half of one.
        for half in [
            "UPDATE installations SET app_version = NULL WHERE organization_id = ? AND workspace_id = ? AND id = ?",
            "UPDATE installations SET auth_template = NULL WHERE organization_id = ? AND workspace_id = ? AND id = ?",
        ] {
            let refused = sqlx::query(half)
                .bind(org.uuid())
                .bind(tenant.production.uuid())
                .bind(kept.id.uuid())
                .execute(database.pool())
                .await;
            assert!(refused.is_err(), "{half}");
        }
    }
}
