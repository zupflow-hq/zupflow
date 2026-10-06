//! [`SqliteAppStore`]: catalog apps in `apps`, workspace apps in
//! `workspace_apps` (decision 0031). Every statement repeats
//! `organization_id`, and `workspace_id` on workspace apps, beside the app's ID.

use async_trait::async_trait;
use sqlx::sqlite::SqliteRow;
use sqlx::{Row, Sqlite, Transaction};
use zupflow_core::apps::{
    App, AppChanges, AppError, AppKind, AppStore, Configuration, ConfigurationChanges, NewApp, NewClientSecret,
    SecretState, StoredClientSecret,
};
use zupflow_core::environment::Environment;
use zupflow_core::ids::{AppId, OrganizationId, UserId, WorkspaceId};
use zupflow_core::keys::{Sealed, WrappedKey};
use zupflow_core::organizations::User;
use zupflow_core::time::Timestamp;

use crate::Database;

pub struct SqliteAppStore {
    database: Database,
}

impl SqliteAppStore {
    pub fn new(database: Database) -> Self {
        Self { database }
    }

    /// Reads, checks and writes a configuration as one: two writers can't
    /// both take the same secret version.
    async fn immediate(&self) -> Result<Transaction<'static, Sqlite>, AppError> {
        self.database
            .pool()
            .begin_with("BEGIN IMMEDIATE")
            .await
            .map_err(failed("begin"))
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

/// The configuration columns both tables share, with the updater as
/// `user_*`. `c` is the configuration, `u` the user (a `LEFT JOIN`).
macro_rules! configuration_columns {
    () => {
        "c.app_id, c.version, c.mcp_server_url, c.oauth_client_id, c.oauth_scopes, c.secret_last4, \
         c.secret_updated_at, c.secret_version, c.updated_at, u.id AS user_id, u.name AS user_name, \
         u.email AS user_email, u.avatar_url AS user_avatar_url"
    };
}

fn configuration_from(row: &SqliteRow, environment: Environment) -> Result<Configuration, AppError> {
    let version: i64 = row.try_get("version").map_err(read)?;
    let scopes: String = row.try_get("oauth_scopes").map_err(read)?;
    let last4: Option<String> = row.try_get("secret_last4").map_err(read)?;
    let secret_updated_at: Option<i64> = row.try_get("secret_updated_at").map_err(read)?;
    let secret_version: i64 = row.try_get("secret_version").map_err(read)?;
    let client_secret = match (last4, secret_updated_at) {
        (Some(last4), Some(updated_at)) => Some(SecretState {
            last4,
            updated_at: Timestamp::from_millis(updated_at),
            version: u32::try_from(secret_version).map_err(read)?,
        }),
        _ => None,
    };
    let user_id: Option<uuid::Uuid> = row.try_get("user_id").map_err(read)?;
    let updated_by = match user_id {
        Some(id) => Some(User {
            id: UserId::from_uuid(id),
            name: row.try_get("user_name").map_err(read)?,
            email: row.try_get("user_email").map_err(read)?,
            avatar_url: row.try_get("user_avatar_url").map_err(read)?,
        }),
        None => None,
    };
    Ok(Configuration {
        app_id: AppId::from_uuid(row.try_get("app_id").map_err(read)?),
        environment,
        version: u32::try_from(version).map_err(read)?,
        mcp_server_url: row.try_get("mcp_server_url").map_err(read)?,
        oauth_client_id: row.try_get("oauth_client_id").map_err(read)?,
        client_secret,
        secret_version: u32::try_from(secret_version).map_err(read)?,
        oauth_scopes: serde_json::from_str(&scopes).map_err(read)?,
        updated_at: Some(Timestamp::from_millis(row.try_get("updated_at").map_err(read)?)),
        updated_by,
    })
}

/// The row's environment column.
fn environment_of(row: &SqliteRow) -> Result<Environment, AppError> {
    let environment: String = row.try_get("environment").map_err(read)?;
    Environment::parse(&environment).ok_or(AppError::Unavailable)
}

fn stored_secret_from(row: &SqliteRow) -> Result<Option<StoredClientSecret>, AppError> {
    let ciphertext: Option<Vec<u8>> = row.try_get("secret_ciphertext").map_err(read)?;
    let Some(ciphertext) = ciphertext else {
        return Ok(None);
    };
    let nonce: Vec<u8> = row.try_get("secret_nonce").map_err(read)?;
    let version: i64 = row.try_get("secret_version").map_err(read)?;
    Ok(Some(StoredClientSecret {
        sealed: Sealed {
            wrapped_key: WrappedKey {
                key_id: row.try_get("secret_key_id").map_err(read)?,
                bytes: row.try_get("secret_wrapped_key").map_err(read)?,
            },
            nonce: nonce.try_into().map_err(|_| read("a nonce of the wrong length"))?,
            ciphertext,
        },
        version: u32::try_from(version).map_err(read)?,
    }))
}

/// The values a write binds, in the order both upserts take them.
struct Write {
    set_url: bool,
    url: Option<String>,
    set_client_id: bool,
    client_id: Option<String>,
    set_scopes: bool,
    scopes: String,
    secret: Option<NewClientSecret>,
    now: i64,
    updated_by: Option<uuid::Uuid>,
}

impl Write {
    fn of(changes: &ConfigurationChanges) -> Result<Self, AppError> {
        Ok(Self {
            set_url: changes.mcp_server_url.is_some(),
            url: changes.mcp_server_url.clone().flatten(),
            set_client_id: changes.oauth_client_id.is_some(),
            client_id: changes.oauth_client_id.clone().flatten(),
            set_scopes: changes.oauth_scopes.is_some(),
            scopes: serde_json::to_string(&changes.oauth_scopes.clone().unwrap_or_default()).map_err(read)?,
            secret: changes.client_secret.clone(),
            now: Timestamp::now().millis(),
            updated_by: changes.updated_by.map(|id| id.uuid()),
        })
    }

    /// Binds, after the scope: the values, then the flags the update reads.
    fn bind<'q>(
        &'q self,
        query: sqlx::query::Query<'q, Sqlite, sqlx::sqlite::SqliteArguments>,
    ) -> sqlx::query::Query<'q, Sqlite, sqlx::sqlite::SqliteArguments> {
        let secret = self.secret.as_ref();
        query
            .bind(&self.url)
            .bind(&self.client_id)
            .bind(&self.scopes)
            .bind(secret.map(|secret| secret.sealed.wrapped_key.key_id.as_str()))
            .bind(secret.map(|secret| secret.sealed.wrapped_key.bytes.as_slice()))
            .bind(secret.map(|secret| secret.sealed.nonce.as_slice()))
            .bind(secret.map(|secret| secret.sealed.ciphertext.as_slice()))
            .bind(secret.map(|secret| secret.last4.as_str()))
            .bind(secret.map(|_| self.now))
            .bind(secret.map_or(0, |secret| i64::from(secret.version)))
            .bind(self.now)
            .bind(self.updated_by)
            .bind(self.set_url)
            .bind(self.set_client_id)
            .bind(self.set_scopes)
            .bind(secret.is_some())
    }
}

/// The upsert's update half, the same for both tables: each field changes
/// only when its flag is set, the version always rises.
macro_rules! upsert_update {
    () => {
        " DO UPDATE SET \
         mcp_server_url = CASE WHEN ?17 THEN excluded.mcp_server_url ELSE mcp_server_url END, \
         oauth_client_id = CASE WHEN ?18 THEN excluded.oauth_client_id ELSE oauth_client_id END, \
         oauth_scopes = CASE WHEN ?19 THEN excluded.oauth_scopes ELSE oauth_scopes END, \
         secret_key_id = CASE WHEN ?20 THEN excluded.secret_key_id ELSE secret_key_id END, \
         secret_wrapped_key = CASE WHEN ?20 THEN excluded.secret_wrapped_key ELSE secret_wrapped_key END, \
         secret_nonce = CASE WHEN ?20 THEN excluded.secret_nonce ELSE secret_nonce END, \
         secret_ciphertext = CASE WHEN ?20 THEN excluded.secret_ciphertext ELSE secret_ciphertext END, \
         secret_last4 = CASE WHEN ?20 THEN excluded.secret_last4 ELSE secret_last4 END, \
         secret_updated_at = CASE WHEN ?20 THEN excluded.secret_updated_at ELSE secret_updated_at END, \
         secret_version = CASE WHEN ?20 THEN excluded.secret_version ELSE secret_version END, \
         version = version + 1, \
         updated_at = excluded.updated_at, \
         updated_by_user_id = excluded.updated_by_user_id"
    };
}

/// `SecretVersionTaken` unless a new secret's version follows the stored one.
fn check_secret_version(stored: i64, secret: Option<&NewClientSecret>) -> Result<(), AppError> {
    match secret {
        Some(secret) if i64::from(secret.version) != stored + 1 => Err(AppError::SecretVersionTaken),
        _ => Ok(()),
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

    async fn list_organization_configurations(
        &self,
        organization_id: OrganizationId,
    ) -> Result<Vec<Configuration>, AppError> {
        let rows = sqlx::query(concat!(
            "SELECT c.environment, ",
            configuration_columns!(),
            " FROM app_configurations c LEFT JOIN users u ON u.id = c.updated_by_user_id              WHERE c.organization_id = ?"
        ))
        .bind(organization_id.uuid())
        .fetch_all(self.database.pool())
        .await
        .map_err(failed("list_organization_configurations"))?;
        rows.iter()
            .map(|row| configuration_from(row, environment_of(row)?))
            .collect()
    }

    async fn find_organization_configuration(
        &self,
        organization_id: OrganizationId,
        app_id: AppId,
        environment: Environment,
    ) -> Result<Option<Configuration>, AppError> {
        let row = sqlx::query(concat!(
            "SELECT ",
            configuration_columns!(),
            " FROM app_configurations c LEFT JOIN users u ON u.id = c.updated_by_user_id              WHERE c.organization_id = ? AND c.app_id = ? AND c.environment = ?"
        ))
        .bind(organization_id.uuid())
        .bind(app_id.uuid())
        .bind(environment.as_str())
        .fetch_optional(self.database.pool())
        .await
        .map_err(failed("find_organization_configuration"))?;
        row.map(|row| configuration_from(&row, environment)).transpose()
    }

    async fn save_organization_configuration(
        &self,
        organization_id: OrganizationId,
        app_id: AppId,
        environment: Environment,
        changes: &ConfigurationChanges,
    ) -> Result<Configuration, AppError> {
        let write = Write::of(changes)?;
        let mut transaction = self.immediate().await?;
        let app = sqlx::query("SELECT 1 FROM apps WHERE organization_id = ? AND id = ?")
            .bind(organization_id.uuid())
            .bind(app_id.uuid())
            .fetch_optional(&mut *transaction)
            .await
            .map_err(failed("save_organization_configuration"))?;
        if app.is_none() {
            return Err(AppError::NotFound);
        }
        let stored: Option<i64> = sqlx::query_scalar(
            "SELECT secret_version FROM app_configurations              WHERE organization_id = ? AND app_id = ? AND environment = ?",
        )
        .bind(organization_id.uuid())
        .bind(app_id.uuid())
        .bind(environment.as_str())
        .fetch_optional(&mut *transaction)
        .await
        .map_err(failed("save_organization_configuration"))?;
        check_secret_version(stored.unwrap_or(0), write.secret.as_ref())?;
        let query = sqlx::query(concat!(
            "INSERT INTO app_configurations (id, organization_id, app_id, environment, version, mcp_server_url,              oauth_client_id, oauth_scopes, secret_key_id, secret_wrapped_key, secret_nonce, secret_ciphertext,              secret_last4, secret_updated_at, secret_version, updated_at, updated_by_user_id)              VALUES (?1, ?2, ?3, ?4, 1, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16)              ON CONFLICT (organization_id, app_id, environment)",
            upsert_update!()
        ))
        .bind(uuid::Uuid::now_v7())
        .bind(organization_id.uuid())
        .bind(app_id.uuid())
        .bind(environment.as_str());
        write
            .bind(query)
            .execute(&mut *transaction)
            .await
            .map_err(failed("save_organization_configuration"))?;
        // A configuration change is a change to its app, as the mock shows it.
        sqlx::query("UPDATE apps SET updated_at = ? WHERE organization_id = ? AND id = ?")
            .bind(write.now)
            .bind(organization_id.uuid())
            .bind(app_id.uuid())
            .execute(&mut *transaction)
            .await
            .map_err(failed("save_organization_configuration"))?;
        let row = sqlx::query(concat!(
            "SELECT ",
            configuration_columns!(),
            " FROM app_configurations c LEFT JOIN users u ON u.id = c.updated_by_user_id              WHERE c.organization_id = ? AND c.app_id = ? AND c.environment = ?"
        ))
        .bind(organization_id.uuid())
        .bind(app_id.uuid())
        .bind(environment.as_str())
        .fetch_one(&mut *transaction)
        .await
        .map_err(failed("save_organization_configuration"))?;
        let configuration = configuration_from(&row, environment)?;
        transaction
            .commit()
            .await
            .map_err(failed("save_organization_configuration"))?;
        Ok(configuration)
    }

    async fn find_organization_client_secret(
        &self,
        organization_id: OrganizationId,
        app_id: AppId,
        environment: Environment,
    ) -> Result<Option<StoredClientSecret>, AppError> {
        let row = sqlx::query(
            "SELECT secret_key_id, secret_wrapped_key, secret_nonce, secret_ciphertext, secret_version              FROM app_configurations WHERE organization_id = ? AND app_id = ? AND environment = ?",
        )
        .bind(organization_id.uuid())
        .bind(app_id.uuid())
        .bind(environment.as_str())
        .fetch_optional(self.database.pool())
        .await
        .map_err(failed("find_organization_client_secret"))?;
        Ok(row.as_ref().map(stored_secret_from).transpose()?.flatten())
    }

    async fn list_workspace_configurations(
        &self,
        organization_id: OrganizationId,
        workspace_id: WorkspaceId,
    ) -> Result<Vec<Configuration>, AppError> {
        let rows = sqlx::query(concat!(
            "SELECT w.environment, ",
            configuration_columns!(),
            " FROM workspace_app_configurations c              JOIN workspaces w ON w.organization_id = c.organization_id AND w.id = c.workspace_id              LEFT JOIN users u ON u.id = c.updated_by_user_id              WHERE c.organization_id = ? AND c.workspace_id = ?"
        ))
        .bind(organization_id.uuid())
        .bind(workspace_id.uuid())
        .fetch_all(self.database.pool())
        .await
        .map_err(failed("list_workspace_configurations"))?;
        rows.iter()
            .map(|row| configuration_from(row, environment_of(row)?))
            .collect()
    }

    async fn find_workspace_configuration(
        &self,
        organization_id: OrganizationId,
        workspace_id: WorkspaceId,
        app_id: AppId,
    ) -> Result<Option<Configuration>, AppError> {
        let row = sqlx::query(concat!(
            "SELECT w.environment, ",
            configuration_columns!(),
            " FROM workspace_app_configurations c              JOIN workspaces w ON w.organization_id = c.organization_id AND w.id = c.workspace_id              LEFT JOIN users u ON u.id = c.updated_by_user_id              WHERE c.organization_id = ? AND c.workspace_id = ? AND c.app_id = ?"
        ))
        .bind(organization_id.uuid())
        .bind(workspace_id.uuid())
        .bind(app_id.uuid())
        .fetch_optional(self.database.pool())
        .await
        .map_err(failed("find_workspace_configuration"))?;
        row.map(|row| configuration_from(&row, environment_of(&row)?))
            .transpose()
    }

    async fn save_workspace_configuration(
        &self,
        organization_id: OrganizationId,
        workspace_id: WorkspaceId,
        app_id: AppId,
        changes: &ConfigurationChanges,
    ) -> Result<Configuration, AppError> {
        let write = Write::of(changes)?;
        let mut transaction = self.immediate().await?;
        let app = sqlx::query("SELECT 1 FROM workspace_apps WHERE organization_id = ? AND workspace_id = ? AND id = ?")
            .bind(organization_id.uuid())
            .bind(workspace_id.uuid())
            .bind(app_id.uuid())
            .fetch_optional(&mut *transaction)
            .await
            .map_err(failed("save_workspace_configuration"))?;
        if app.is_none() {
            return Err(AppError::NotFound);
        }
        let stored: Option<i64> = sqlx::query_scalar(
            "SELECT secret_version FROM workspace_app_configurations              WHERE organization_id = ? AND workspace_id = ? AND app_id = ?",
        )
        .bind(organization_id.uuid())
        .bind(workspace_id.uuid())
        .bind(app_id.uuid())
        .fetch_optional(&mut *transaction)
        .await
        .map_err(failed("save_workspace_configuration"))?;
        check_secret_version(stored.unwrap_or(0), write.secret.as_ref())?;
        let query = sqlx::query(concat!(
            "INSERT INTO workspace_app_configurations (id, organization_id, workspace_id, app_id, version,              mcp_server_url, oauth_client_id, oauth_scopes, secret_key_id, secret_wrapped_key, secret_nonce,              secret_ciphertext, secret_last4, secret_updated_at, secret_version, updated_at, updated_by_user_id)              VALUES (?1, ?2, ?3, ?4, 1, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16)              ON CONFLICT (organization_id, workspace_id, app_id)",
            upsert_update!()
        ))
        .bind(uuid::Uuid::now_v7())
        .bind(organization_id.uuid())
        .bind(workspace_id.uuid())
        .bind(app_id.uuid());
        write
            .bind(query)
            .execute(&mut *transaction)
            .await
            .map_err(failed("save_workspace_configuration"))?;
        sqlx::query(
            "UPDATE workspace_apps SET updated_at = ? WHERE organization_id = ? AND workspace_id = ? AND id = ?",
        )
        .bind(write.now)
        .bind(organization_id.uuid())
        .bind(workspace_id.uuid())
        .bind(app_id.uuid())
        .execute(&mut *transaction)
        .await
        .map_err(failed("save_workspace_configuration"))?;
        let row = sqlx::query(concat!(
            "SELECT w.environment, ",
            configuration_columns!(),
            " FROM workspace_app_configurations c              JOIN workspaces w ON w.organization_id = c.organization_id AND w.id = c.workspace_id              LEFT JOIN users u ON u.id = c.updated_by_user_id              WHERE c.organization_id = ? AND c.workspace_id = ? AND c.app_id = ?"
        ))
        .bind(organization_id.uuid())
        .bind(workspace_id.uuid())
        .bind(app_id.uuid())
        .fetch_one(&mut *transaction)
        .await
        .map_err(failed("save_workspace_configuration"))?;
        let configuration = configuration_from(&row, environment_of(&row)?)?;
        transaction
            .commit()
            .await
            .map_err(failed("save_workspace_configuration"))?;
        Ok(configuration)
    }

    async fn find_workspace_client_secret(
        &self,
        organization_id: OrganizationId,
        workspace_id: WorkspaceId,
        app_id: AppId,
    ) -> Result<Option<StoredClientSecret>, AppError> {
        let row = sqlx::query(
            "SELECT secret_key_id, secret_wrapped_key, secret_nonce, secret_ciphertext, secret_version              FROM workspace_app_configurations WHERE organization_id = ? AND workspace_id = ? AND app_id = ?",
        )
        .bind(organization_id.uuid())
        .bind(workspace_id.uuid())
        .bind(app_id.uuid())
        .fetch_optional(self.database.pool())
        .await
        .map_err(failed("find_workspace_client_secret"))?;
        Ok(row.as_ref().map(stored_secret_from).transpose()?.flatten())
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
