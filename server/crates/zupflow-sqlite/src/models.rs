//! [`SqliteModelStore`]: connections in `model_connections`, their sealed
//! keys in `model_connection_credentials`, models in `models` and each
//! environment's default in `default_models`. Every statement repeats the
//! organization and, below it, the environment, joins and subqueries
//! included, beside a connection's or model's ID.
//!
//! Each change is an immediate transaction that first checks, in it, that
//! the acting manager may still manage models, then the rules that must hold
//! across concurrent changes (a model in use isn't removed; a default is a
//! model of its environment; a key is replaced from the version read), then
//! writes the change and its audit event.

use async_trait::async_trait;
use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use sqlx::sqlite::SqliteRow;
use sqlx::{QueryBuilder, Row, Sqlite, SqliteConnection, Transaction};
use uuid::Uuid;
use zupflow_core::audit::Audit;
use zupflow_core::environment::Environment;
use zupflow_core::ids::{ModelConnectionId, ModelId, OrganizationId, UserId, WorkspaceId};
use zupflow_core::keys::{Sealed, WrappedKey};
use zupflow_core::model_providers::ProviderKind;
use zupflow_core::model_store::{
    ConnectionKey, ConnectionStatus, DefaultModel, FIRST_KEY_VERSION, HoldingAgent, KeyReplacement, KeyState, Model,
    ModelConnection, ModelFilter, ModelHolders, ModelManager, ModelRoute, ModelStatus, ModelStore, ModelStoreError,
    NAMED_HOLDERS, NewConnection, NewModel, RouteState, RunModel, SealedKey,
};
use zupflow_core::organizations::User;
use zupflow_core::time::Timestamp;
use zupflow_core::workspaces::Page;

use crate::Database;

pub struct SqliteModelStore {
    database: Database,
}

impl SqliteModelStore {
    pub fn new(database: Database) -> Self {
        Self { database }
    }

    async fn immediate(&self, operation: &'static str) -> Result<Transaction<'static, Sqlite>, ModelStoreError> {
        self.database
            .pool()
            .begin_with("BEGIN IMMEDIATE")
            .await
            .map_err(failed(operation))
    }

    async fn reader(&self, operation: &'static str) -> Result<Transaction<'static, Sqlite>, ModelStoreError> {
        self.database.pool().begin().await.map_err(failed(operation))
    }
}

fn failed(operation: &'static str) -> impl FnOnce(sqlx::Error) -> ModelStoreError {
    move |error| {
        tracing::error!(event = "models.database_error", operation, %error);
        ModelStoreError::Unavailable
    }
}

fn read(error: impl std::fmt::Display) -> ModelStoreError {
    tracing::error!(event = "models.unreadable_row", %error);
    ModelStoreError::Unavailable
}

/// A connection's columns and its model count, from `model_connections c`,
/// with the organization bound as `?1`. A macro, so queries stay static strings.
macro_rules! connection_select {
    () => {
        "SELECT c.id, c.organization_id, c.environment, c.kind, c.label, c.key_version, c.key_last4, \
         c.key_updated_at, c.status, c.created_at, c.updated_at, \
         (SELECT count(*) FROM models m \
          WHERE m.organization_id = ?1 AND m.environment = c.environment AND m.connection_id = c.id) AS model_count \
         FROM model_connections c WHERE c.organization_id = ?1"
    };
}

const LIST_CONNECTIONS: &str = concat!(
    connection_select!(),
    " AND (?2 IS NULL OR c.environment = ?2) ORDER BY c.created_at, c.id"
);
const FIND_CONNECTION: &str = concat!(
    connection_select!(),
    " AND (?2 IS NULL OR c.environment = ?2) AND c.id = ?3"
);
const FIND_CONNECTION_IN: &str = concat!(connection_select!(), " AND c.environment = ?2 AND c.id = ?3");

/// A model's columns with its route, whether it's the default and how many
/// agents override the default with it, from `models m` joined to its
/// connection `c`, before any condition. The subqueries hold to the model's
/// own organization and environment, which the conditions after bind.
macro_rules! model_columns {
    () => {
        "SELECT m.id, m.organization_id, m.environment, m.connection_id, c.kind, c.label, m.provider_model_id, \
         m.name, m.author, m.context_window, m.supports_tools, m.status, m.created_at, m.updated_at, \
         EXISTS (SELECT 1 FROM default_models d WHERE d.organization_id = m.organization_id \
                 AND d.environment = m.environment AND d.model_id = m.id) AS is_default, \
         (SELECT count(*) FROM agents a JOIN workspaces w \
          ON w.organization_id = a.organization_id AND w.id = a.workspace_id \
          WHERE a.organization_id = m.organization_id AND w.organization_id = m.organization_id \
          AND w.environment = m.environment AND a.model_id = m.id) AS agent_count \
         FROM models m JOIN model_connections c \
         ON c.organization_id = m.organization_id AND c.environment = m.environment AND c.id = m.connection_id"
    };
}

/// [`model_columns`] in one organization, bound as `?1`.
macro_rules! model_select {
    () => {
        concat!(
            model_columns!(),
            " WHERE m.organization_id = ?1 AND c.organization_id = ?1"
        )
    };
}

const FIND_MODEL: &str = concat!(model_select!(), " AND (?2 IS NULL OR m.environment = ?2) AND m.id = ?3");
const FIND_MODEL_IN: &str = concat!(model_select!(), " AND m.environment = ?2 AND m.id = ?3");

const MANAGER_MEMBER: &str = "SELECT 1 FROM members WHERE organization_id = ? AND id = ? \
                              AND status = 'active' AND role IN ('owner', 'admin')";
const MANAGER_KEY: &str = "SELECT 1 FROM api_keys WHERE organization_id = ? AND id = ? \
                           AND environment = ? AND revoked_at IS NULL";

fn environment_of(row: &SqliteRow) -> Result<Environment, ModelStoreError> {
    let environment: String = row.try_get("environment").map_err(read)?;
    Environment::parse(&environment).ok_or_else(|| read("an unknown environment"))
}

fn kind_of(row: &SqliteRow) -> Result<ProviderKind, ModelStoreError> {
    let kind: String = row.try_get("kind").map_err(read)?;
    ProviderKind::parse(&kind).ok_or_else(|| read("an unknown provider kind"))
}

fn count(row: &SqliteRow, column: &str) -> Result<u64, ModelStoreError> {
    let value: i64 = row.try_get(column).map_err(read)?;
    u64::try_from(value).map_err(read)
}

fn version(value: i64) -> Result<u32, ModelStoreError> {
    u32::try_from(value).map_err(read)
}

fn connection_from(row: &SqliteRow) -> Result<ModelConnection, ModelStoreError> {
    let status: String = row.try_get("status").map_err(read)?;
    Ok(ModelConnection {
        id: ModelConnectionId::from_uuid(row.try_get("id").map_err(read)?),
        organization_id: OrganizationId::from_uuid(row.try_get("organization_id").map_err(read)?),
        environment: environment_of(row)?,
        kind: kind_of(row)?,
        label: row.try_get("label").map_err(read)?,
        api_key: KeyState {
            set: true,
            last4: Some(row.try_get("key_last4").map_err(read)?),
            updated_at: Some(Timestamp::from_millis(row.try_get("key_updated_at").map_err(read)?)),
        },
        key_version: version(row.try_get("key_version").map_err(read)?)?,
        status: ConnectionStatus::parse(&status).ok_or_else(|| read("an unknown connection status"))?,
        model_count: count(row, "model_count")?,
        created_at: Timestamp::from_millis(row.try_get("created_at").map_err(read)?),
        updated_at: Timestamp::from_millis(row.try_get("updated_at").map_err(read)?),
    })
}

fn model_from(row: &SqliteRow) -> Result<Model, ModelStoreError> {
    let status: String = row.try_get("status").map_err(read)?;
    let context_window: Option<i64> = row.try_get("context_window").map_err(read)?;
    Ok(Model {
        id: ModelId::from_uuid(row.try_get("id").map_err(read)?),
        organization_id: OrganizationId::from_uuid(row.try_get("organization_id").map_err(read)?),
        environment: environment_of(row)?,
        route: ModelRoute {
            connection_id: ModelConnectionId::from_uuid(row.try_get("connection_id").map_err(read)?),
            kind: kind_of(row)?,
            label: row.try_get("label").map_err(read)?,
        },
        provider_model_id: row.try_get("provider_model_id").map_err(read)?,
        name: row.try_get("name").map_err(read)?,
        author: row.try_get("author").map_err(read)?,
        context_window: context_window.map(u64::try_from).transpose().map_err(read)?,
        supports_tools: row.try_get("supports_tools").map_err(read)?,
        status: ModelStatus::parse(&status).ok_or_else(|| read("an unknown model status"))?,
        is_default: row.try_get("is_default").map_err(read)?,
        agent_count: count(row, "agent_count")?,
        created_at: Timestamp::from_millis(row.try_get("created_at").map_err(read)?),
        updated_at: Timestamp::from_millis(row.try_get("updated_at").map_err(read)?),
    })
}

/// Whether `manager` may still manage models of this organization and
/// environment: an active owner or admin, or an unrevoked key of the
/// environment.
async fn manager_allowed(
    connection: &mut SqliteConnection,
    organization_id: OrganizationId,
    environment: Environment,
    manager: ModelManager,
) -> Result<(), ModelStoreError> {
    let query = match manager {
        ModelManager::Member(member) => sqlx::query(MANAGER_MEMBER)
            .bind(organization_id.uuid())
            .bind(member.uuid()),
        ModelManager::ApiKey(key) => sqlx::query(MANAGER_KEY)
            .bind(organization_id.uuid())
            .bind(key.uuid())
            .bind(environment.as_str()),
    };
    let found = query
        .fetch_optional(connection)
        .await
        .map_err(failed("manager_allowed"))?;
    if found.is_some() {
        Ok(())
    } else {
        Err(ModelStoreError::NotAllowed)
    }
}

async fn find_connection_in(
    connection: &mut SqliteConnection,
    organization_id: OrganizationId,
    environment: Environment,
    connection_id: ModelConnectionId,
) -> Result<Option<ModelConnection>, ModelStoreError> {
    let row = sqlx::query(FIND_CONNECTION_IN)
        .bind(organization_id.uuid())
        .bind(environment.as_str())
        .bind(connection_id.uuid())
        .fetch_optional(connection)
        .await
        .map_err(failed("find_connection"))?;
    row.as_ref().map(connection_from).transpose()
}

async fn find_model_in(
    connection: &mut SqliteConnection,
    organization_id: OrganizationId,
    environment: Environment,
    model_id: ModelId,
) -> Result<Option<Model>, ModelStoreError> {
    let row = sqlx::query(FIND_MODEL_IN)
        .bind(organization_id.uuid())
        .bind(environment.as_str())
        .bind(model_id.uuid())
        .fetch_optional(connection)
        .await
        .map_err(failed("find_model"))?;
    row.as_ref().map(model_from).transpose()
}

/// The models whose holders a removal checks.
#[derive(Clone, Copy)]
enum Removing {
    Model(ModelId),
    /// Every model of the connection.
    Connection(ModelConnectionId),
}

fn push_removing(query: &mut QueryBuilder<Sqlite>, removing: Removing) {
    match removing {
        Removing::Model(model_id) => {
            query.push(" AND m.id = ");
            query.push_bind(model_id.uuid());
        }
        Removing::Connection(connection_id) => {
            query.push(" AND m.connection_id = ");
            query.push_bind(connection_id.uuid());
        }
    }
}

/// What holds the models being removed, in this organization and
/// environment only: the default, and agents overriding it with one.
async fn holders(
    connection: &mut SqliteConnection,
    organization_id: OrganizationId,
    environment: Environment,
    removing: Removing,
) -> Result<ModelHolders, ModelStoreError> {
    let mut default = QueryBuilder::<Sqlite>::new(
        "SELECT m.name FROM default_models d JOIN models m \
         ON m.organization_id = d.organization_id AND m.environment = d.environment AND m.id = d.model_id \
         WHERE d.organization_id = ",
    );
    default.push_bind(organization_id.uuid());
    default.push(" AND m.organization_id = ");
    default.push_bind(organization_id.uuid());
    default.push(" AND d.environment = ");
    default.push_bind(environment.as_str());
    default.push(" AND m.environment = ");
    default.push_bind(environment.as_str());
    push_removing(&mut default, removing);
    let default_model: Option<String> = default
        .build_query_scalar()
        .fetch_optional(&mut *connection)
        .await
        .map_err(failed("holders"))?;

    let mut agents = QueryBuilder::<Sqlite>::new(
        "SELECT a.name AS agent_name, w.name AS workspace_name, count(*) OVER () AS total \
         FROM agents a \
         JOIN workspaces w ON w.organization_id = a.organization_id AND w.id = a.workspace_id \
         JOIN models m ON m.organization_id = a.organization_id AND m.id = a.model_id \
         WHERE a.organization_id = ",
    );
    agents.push_bind(organization_id.uuid());
    agents.push(" AND w.organization_id = ");
    agents.push_bind(organization_id.uuid());
    agents.push(" AND m.organization_id = ");
    agents.push_bind(organization_id.uuid());
    agents.push(" AND w.environment = ");
    agents.push_bind(environment.as_str());
    agents.push(" AND m.environment = ");
    agents.push_bind(environment.as_str());
    push_removing(&mut agents, removing);
    agents.push(" ORDER BY a.created_at, a.id LIMIT ");
    agents.push_bind(i64::try_from(NAMED_HOLDERS).unwrap_or(5));
    let rows = agents
        .build()
        .fetch_all(&mut *connection)
        .await
        .map_err(failed("holders"))?;
    let mut holders = ModelHolders {
        default_model,
        agents: Vec::with_capacity(rows.len()),
        agent_total: 0,
    };
    for row in &rows {
        holders.agent_total = count(row, "total")?;
        holders.agents.push(HoldingAgent {
            agent: row.try_get("agent_name").map_err(read)?,
            workspace: row.try_get("workspace_name").map_err(read)?,
        });
    }
    Ok(holders)
}

async fn insert_key(
    connection: &mut SqliteConnection,
    organization_id: OrganizationId,
    environment: Environment,
    connection_id: ModelConnectionId,
    key_version: u32,
    sealed: &Sealed,
    now: Timestamp,
) -> Result<(), ModelStoreError> {
    sqlx::query(
        "INSERT INTO model_connection_credentials (id, organization_id, environment, connection_id, version, key_id, \
         wrapped_key, nonce, ciphertext, created_at) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
    )
    .bind(Uuid::now_v7())
    .bind(organization_id.uuid())
    .bind(environment.as_str())
    .bind(connection_id.uuid())
    .bind(i64::from(key_version))
    .bind(&sealed.wrapped_key.key_id)
    .bind(sealed.wrapped_key.bytes.as_slice())
    .bind(sealed.nonce.as_slice())
    .bind(sealed.ciphertext.as_slice())
    .bind(now.millis())
    .execute(connection)
    .await
    .map_err(failed("insert_key"))?;
    Ok(())
}

fn last4(key: &SealedKey) -> Result<&str, ModelStoreError> {
    let count = key.last4.chars().count();
    if (1..=4).contains(&count) {
        Ok(&key.last4)
    } else {
        Err(read("a key's last four that isn't one to four characters"))
    }
}

async fn read_default(
    connection: &mut SqliteConnection,
    organization_id: OrganizationId,
    environment: Environment,
) -> Result<DefaultModel, ModelStoreError> {
    let row = sqlx::query(
        "SELECT d.model_id, d.updated_at, u.id AS user_id, u.name AS user_name, u.email AS user_email, \
         u.avatar_url AS user_avatar_url \
         FROM default_models d LEFT JOIN users u ON u.id = d.updated_by_user_id \
         WHERE d.organization_id = ? AND d.environment = ?",
    )
    .bind(organization_id.uuid())
    .bind(environment.as_str())
    .fetch_optional(&mut *connection)
    .await
    .map_err(failed("default_model"))?;
    let Some(row) = row else {
        return Ok(DefaultModel {
            environment,
            model: None,
            updated_at: None,
            updated_by: None,
        });
    };
    let model_id: Option<Uuid> = row.try_get("model_id").map_err(read)?;
    let user_id: Option<Uuid> = row.try_get("user_id").map_err(read)?;
    let updated_by = match user_id {
        None => None,
        Some(id) => Some(User {
            id: UserId::from_uuid(id),
            name: row.try_get("user_name").map_err(read)?,
            email: row.try_get("user_email").map_err(read)?,
            avatar_url: row.try_get("user_avatar_url").map_err(read)?,
        }),
    };
    let model = match model_id {
        None => None,
        Some(id) => find_model_in(connection, organization_id, environment, ModelId::from_uuid(id)).await?,
    };
    Ok(DefaultModel {
        environment,
        model,
        updated_at: Some(Timestamp::from_millis(row.try_get("updated_at").map_err(read)?)),
        updated_by,
    })
}

async fn workspace_environment(
    connection: &mut SqliteConnection,
    organization_id: OrganizationId,
    workspace_id: WorkspaceId,
) -> Result<Option<Environment>, ModelStoreError> {
    let environment: Option<String> =
        sqlx::query_scalar("SELECT environment FROM workspaces WHERE organization_id = ? AND id = ?")
            .bind(organization_id.uuid())
            .bind(workspace_id.uuid())
            .fetch_optional(connection)
            .await
            .map_err(failed("workspace_environment"))?;
    environment
        .map(|environment| Environment::parse(&environment).ok_or_else(|| read("an unknown environment")))
        .transpose()
}

/// A position in the list of models: its name as sorted, then its ID.
struct Position {
    name: String,
    id: Uuid,
}

impl Position {
    fn of(model: &Model) -> Self {
        Self {
            name: model.name.to_ascii_lowercase(),
            id: model.id.uuid(),
        }
    }

    /// The model's prefixed ID, never its bare UUID: a cursor leaves the server.
    fn encode(&self) -> String {
        URL_SAFE_NO_PAD.encode(format!("{}\n{}", ModelId::from_uuid(self.id), self.name))
    }

    fn decode(cursor: &str) -> Option<Self> {
        let raw = String::from_utf8(URL_SAFE_NO_PAD.decode(cursor).ok()?).ok()?;
        let (id, name) = raw.split_once('\n')?;
        let id = ModelId::parse(id).ok()?.uuid();
        (name.len() <= 240).then(|| Self {
            name: name.to_owned(),
            id,
        })
    }
}

/// Appends the filter's conditions, every one after the organization's.
fn push_model_filter(query: &mut QueryBuilder<Sqlite>, filter: &ModelFilter) {
    if let Some(environment) = filter.environment {
        query.push(" AND m.environment = ");
        query.push_bind(environment.as_str());
    }
    if let Some(connection_id) = filter.connection_id {
        query.push(" AND m.connection_id = ");
        query.push_bind(connection_id.uuid());
    }
    if let Some(text) = filter.q.as_deref().map(str::trim).filter(|text| !text.is_empty()) {
        // SQLite's lower() folds ASCII only; fold the same way here.
        let text = text.to_ascii_lowercase();
        query.push(" AND (instr(lower(m.name), ");
        query.push_bind(text.clone());
        query.push(") > 0 OR instr(lower(m.provider_model_id), ");
        query.push_bind(text.clone());
        query.push(") > 0 OR instr(lower(m.author), ");
        query.push_bind(text);
        query.push(") > 0)");
    }
}

#[async_trait]
impl ModelStore for SqliteModelStore {
    async fn list_connections(
        &self,
        organization_id: OrganizationId,
        environment: Option<Environment>,
    ) -> Result<Vec<ModelConnection>, ModelStoreError> {
        let rows = sqlx::query(LIST_CONNECTIONS)
            .bind(organization_id.uuid())
            .bind(environment.map(Environment::as_str))
            .fetch_all(self.database.pool())
            .await
            .map_err(failed("list_connections"))?;
        rows.iter().map(connection_from).collect()
    }

    async fn find_connection(
        &self,
        organization_id: OrganizationId,
        environment: Option<Environment>,
        connection_id: ModelConnectionId,
    ) -> Result<Option<ModelConnection>, ModelStoreError> {
        let row = sqlx::query(FIND_CONNECTION)
            .bind(organization_id.uuid())
            .bind(environment.map(Environment::as_str))
            .bind(connection_id.uuid())
            .fetch_optional(self.database.pool())
            .await
            .map_err(failed("find_connection"))?;
        row.as_ref().map(connection_from).transpose()
    }

    async fn connection_key(
        &self,
        organization_id: OrganizationId,
        environment: Environment,
        connection_id: ModelConnectionId,
    ) -> Result<Option<ConnectionKey>, ModelStoreError> {
        let row = sqlx::query(
            "SELECT k.version, k.key_id, k.wrapped_key, k.nonce, k.ciphertext \
             FROM model_connection_credentials k JOIN model_connections c \
             ON c.organization_id = k.organization_id AND c.environment = k.environment \
             AND c.id = k.connection_id AND c.key_version = k.version \
             WHERE k.organization_id = ? AND c.organization_id = ? AND k.environment = ? AND c.environment = ? \
             AND k.connection_id = ?",
        )
        .bind(organization_id.uuid())
        .bind(organization_id.uuid())
        .bind(environment.as_str())
        .bind(environment.as_str())
        .bind(connection_id.uuid())
        .fetch_optional(self.database.pool())
        .await
        .map_err(failed("connection_key"))?;
        let Some(row) = row else { return Ok(None) };
        let nonce: Vec<u8> = row.try_get("nonce").map_err(read)?;
        Ok(Some(ConnectionKey {
            version: version(row.try_get("version").map_err(read)?)?,
            sealed: Sealed {
                wrapped_key: WrappedKey {
                    key_id: row.try_get("key_id").map_err(read)?,
                    bytes: row.try_get("wrapped_key").map_err(read)?,
                },
                nonce: nonce.try_into().map_err(|_| read("a nonce that isn't 12 bytes"))?,
                ciphertext: row.try_get("ciphertext").map_err(read)?,
            },
        }))
    }

    async fn create_connection(
        &self,
        organization_id: OrganizationId,
        connection_id: ModelConnectionId,
        manager: ModelManager,
        connection: &NewConnection,
        now: Timestamp,
        audit: &Audit<'_, ModelConnection>,
    ) -> Result<ModelConnection, ModelStoreError> {
        let environment = connection.environment;
        let mut transaction = self.immediate("create_connection").await?;
        manager_allowed(&mut transaction, organization_id, environment, manager).await?;
        sqlx::query(
            "INSERT INTO model_connections (id, organization_id, environment, kind, label, key_version, key_last4, \
             key_updated_at, status, created_at, updated_at) VALUES (?, ?, ?, ?, ?, ?, ?, ?, 'active', ?, ?)",
        )
        .bind(connection_id.uuid())
        .bind(organization_id.uuid())
        .bind(environment.as_str())
        .bind(connection.kind.as_str())
        .bind(&connection.label)
        .bind(i64::from(FIRST_KEY_VERSION))
        .bind(last4(&connection.key)?)
        .bind(now.millis())
        .bind(now.millis())
        .bind(now.millis())
        .execute(&mut *transaction)
        .await
        .map_err(failed("create_connection"))?;
        insert_key(
            &mut transaction,
            organization_id,
            environment,
            connection_id,
            FIRST_KEY_VERSION,
            &connection.key.sealed,
            now,
        )
        .await?;
        let created = find_connection_in(&mut transaction, organization_id, environment, connection_id)
            .await?
            .ok_or_else(|| read("a connection gone inside its own transaction"))?;
        crate::audit::record(&mut transaction, organization_id, &audit(&created))
            .await
            .map_err(failed("audit"))?;
        transaction.commit().await.map_err(failed("create_connection"))?;
        Ok(created)
    }

    async fn rename_connection(
        &self,
        organization_id: OrganizationId,
        environment: Environment,
        connection_id: ModelConnectionId,
        manager: ModelManager,
        label: &str,
        now: Timestamp,
        audit: &Audit<'_, ModelConnection>,
    ) -> Result<ModelConnection, ModelStoreError> {
        let mut transaction = self.immediate("rename_connection").await?;
        manager_allowed(&mut transaction, organization_id, environment, manager).await?;
        let renamed = sqlx::query(
            "UPDATE model_connections SET label = ?, updated_at = ? \
             WHERE organization_id = ? AND environment = ? AND id = ?",
        )
        .bind(label)
        .bind(now.millis())
        .bind(organization_id.uuid())
        .bind(environment.as_str())
        .bind(connection_id.uuid())
        .execute(&mut *transaction)
        .await
        .map_err(failed("rename_connection"))?;
        if renamed.rows_affected() == 0 {
            return Err(ModelStoreError::ConnectionNotFound);
        }
        let connection = find_connection_in(&mut transaction, organization_id, environment, connection_id)
            .await?
            .ok_or(ModelStoreError::ConnectionNotFound)?;
        crate::audit::record(&mut transaction, organization_id, &audit(&connection))
            .await
            .map_err(failed("audit"))?;
        transaction.commit().await.map_err(failed("rename_connection"))?;
        Ok(connection)
    }

    async fn replace_key(
        &self,
        organization_id: OrganizationId,
        environment: Environment,
        connection_id: ModelConnectionId,
        manager: ModelManager,
        replacement: &KeyReplacement,
        now: Timestamp,
        audit: &Audit<'_, ModelConnection>,
    ) -> Result<ModelConnection, ModelStoreError> {
        let next = replacement
            .expected_version
            .checked_add(1)
            .ok_or(ModelStoreError::KeyChanged)?;
        let mut transaction = self.immediate("replace_key").await?;
        manager_allowed(&mut transaction, organization_id, environment, manager).await?;
        let current: Option<i64> = sqlx::query_scalar(
            "SELECT key_version FROM model_connections WHERE organization_id = ? AND environment = ? AND id = ?",
        )
        .bind(organization_id.uuid())
        .bind(environment.as_str())
        .bind(connection_id.uuid())
        .fetch_optional(&mut *transaction)
        .await
        .map_err(failed("replace_key"))?;
        let current = version(current.ok_or(ModelStoreError::ConnectionNotFound)?)?;
        if current != replacement.expected_version {
            return Err(ModelStoreError::KeyChanged);
        }
        insert_key(
            &mut transaction,
            organization_id,
            environment,
            connection_id,
            next,
            &replacement.key.sealed,
            now,
        )
        .await?;
        // Only the current key is kept: an earlier version is never opened again.
        sqlx::query(
            "DELETE FROM model_connection_credentials \
             WHERE organization_id = ? AND environment = ? AND connection_id = ? AND version < ?",
        )
        .bind(organization_id.uuid())
        .bind(environment.as_str())
        .bind(connection_id.uuid())
        .bind(i64::from(next))
        .execute(&mut *transaction)
        .await
        .map_err(failed("replace_key"))?;
        sqlx::query(
            "UPDATE model_connections SET key_version = ?, key_last4 = ?, key_updated_at = ?, status = 'active', \
             updated_at = ? WHERE organization_id = ? AND environment = ? AND id = ?",
        )
        .bind(i64::from(next))
        .bind(last4(&replacement.key)?)
        .bind(now.millis())
        .bind(now.millis())
        .bind(organization_id.uuid())
        .bind(environment.as_str())
        .bind(connection_id.uuid())
        .execute(&mut *transaction)
        .await
        .map_err(failed("replace_key"))?;
        let connection = find_connection_in(&mut transaction, organization_id, environment, connection_id)
            .await?
            .ok_or(ModelStoreError::ConnectionNotFound)?;
        crate::audit::record(&mut transaction, organization_id, &audit(&connection))
            .await
            .map_err(failed("audit"))?;
        transaction.commit().await.map_err(failed("replace_key"))?;
        Ok(connection)
    }

    async fn mark_key_refused(
        &self,
        organization_id: OrganizationId,
        environment: Environment,
        connection_id: ModelConnectionId,
        key_version: u32,
    ) -> Result<(), ModelStoreError> {
        // `updated_at` stays: it records people's changes, which alone are
        // audited, and the provider refusing a key is not one.
        sqlx::query(
            "UPDATE model_connections SET status = 'invalid_key' \
             WHERE organization_id = ? AND environment = ? AND id = ? AND key_version = ?",
        )
        .bind(organization_id.uuid())
        .bind(environment.as_str())
        .bind(connection_id.uuid())
        .bind(i64::from(key_version))
        .execute(self.database.pool())
        .await
        .map_err(failed("mark_key_refused"))?;
        Ok(())
    }

    async fn record_model_status(
        &self,
        organization_id: OrganizationId,
        environment: Environment,
        model_id: ModelId,
        status: ModelStatus,
    ) -> Result<(), ModelStoreError> {
        // Only a change writes: a model answering as before costs a read.
        sqlx::query(
            "UPDATE models SET status = ? \
             WHERE organization_id = ? AND environment = ? AND id = ? AND status <> ?",
        )
        .bind(status.as_str())
        .bind(organization_id.uuid())
        .bind(environment.as_str())
        .bind(model_id.uuid())
        .bind(status.as_str())
        .execute(self.database.pool())
        .await
        .map_err(failed("record_model_status"))?;
        Ok(())
    }

    async fn delete_connection(
        &self,
        organization_id: OrganizationId,
        environment: Environment,
        connection_id: ModelConnectionId,
        manager: ModelManager,
        audit: &Audit<'_, ModelConnection>,
    ) -> Result<(), ModelStoreError> {
        let mut transaction = self.immediate("delete_connection").await?;
        manager_allowed(&mut transaction, organization_id, environment, manager).await?;
        let connection = find_connection_in(&mut transaction, organization_id, environment, connection_id)
            .await?
            .ok_or(ModelStoreError::ConnectionNotFound)?;
        let held = holders(
            &mut transaction,
            organization_id,
            environment,
            Removing::Connection(connection_id),
        )
        .await?;
        if !held.is_empty() {
            return Err(ModelStoreError::InUse(held));
        }
        // Its keys and models reference it with cascading keys: they go in this one statement.
        sqlx::query("DELETE FROM model_connections WHERE organization_id = ? AND environment = ? AND id = ?")
            .bind(organization_id.uuid())
            .bind(environment.as_str())
            .bind(connection_id.uuid())
            .execute(&mut *transaction)
            .await
            .map_err(failed("delete_connection"))?;
        crate::audit::record(&mut transaction, organization_id, &audit(&connection))
            .await
            .map_err(failed("audit"))?;
        transaction.commit().await.map_err(failed("delete_connection"))?;
        Ok(())
    }

    async fn list_models(
        &self,
        organization_id: OrganizationId,
        filter: &ModelFilter,
        cursor: Option<&str>,
        limit: u32,
    ) -> Result<Page<Model>, ModelStoreError> {
        let after = cursor
            .map(|cursor| Position::decode(cursor).ok_or(ModelStoreError::InvalidCursor))
            .transpose()?;
        if filter.matches_nothing {
            return Ok(Page {
                items: Vec::new(),
                next_cursor: None,
                total: 0,
            });
        }
        let limit = limit.clamp(1, zupflow_core::model_store::MAX_PAGE);
        // One read transaction, so the count and the page are of one moment.
        let mut transaction = self.reader("list_models").await?;
        let mut total = QueryBuilder::<Sqlite>::new("SELECT count(*) FROM models m WHERE m.organization_id = ");
        total.push_bind(organization_id.uuid());
        push_model_filter(&mut total, filter);
        let total: i64 = total
            .build_query_scalar()
            .fetch_one(&mut *transaction)
            .await
            .map_err(failed("list_models"))?;

        let mut query = QueryBuilder::<Sqlite>::new(concat!(model_columns!(), " WHERE m.organization_id = "));
        query.push_bind(organization_id.uuid());
        query.push(" AND c.organization_id = ");
        query.push_bind(organization_id.uuid());
        push_model_filter(&mut query, filter);
        if let Some(after) = &after {
            query.push(" AND (lower(m.name), m.id) > (");
            query.push_bind(after.name.clone());
            query.push(", ");
            query.push_bind(after.id);
            query.push(")");
        }
        query.push(" ORDER BY lower(m.name), m.id LIMIT ");
        query.push_bind(i64::from(limit) + 1);
        let rows = query
            .build()
            .fetch_all(&mut *transaction)
            .await
            .map_err(failed("list_models"))?;
        transaction.commit().await.map_err(failed("list_models"))?;
        let mut items = rows.iter().map(model_from).collect::<Result<Vec<_>, _>>()?;
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

    async fn find_model(
        &self,
        organization_id: OrganizationId,
        environment: Option<Environment>,
        model_id: ModelId,
    ) -> Result<Option<Model>, ModelStoreError> {
        let row = sqlx::query(FIND_MODEL)
            .bind(organization_id.uuid())
            .bind(environment.map(Environment::as_str))
            .bind(model_id.uuid())
            .fetch_optional(self.database.pool())
            .await
            .map_err(failed("find_model"))?;
        row.as_ref().map(model_from).transpose()
    }

    async fn added_models(
        &self,
        organization_id: OrganizationId,
        environment: Environment,
        connection_id: ModelConnectionId,
    ) -> Result<Vec<(String, ModelId)>, ModelStoreError> {
        let rows = sqlx::query(
            "SELECT provider_model_id, id FROM models \
             WHERE organization_id = ? AND environment = ? AND connection_id = ?",
        )
        .bind(organization_id.uuid())
        .bind(environment.as_str())
        .bind(connection_id.uuid())
        .fetch_all(self.database.pool())
        .await
        .map_err(failed("added_models"))?;
        rows.iter()
            .map(|row| {
                Ok((
                    row.try_get("provider_model_id").map_err(read)?,
                    ModelId::from_uuid(row.try_get("id").map_err(read)?),
                ))
            })
            .collect()
    }

    async fn create_model(
        &self,
        organization_id: OrganizationId,
        environment: Environment,
        model_id: ModelId,
        manager: ModelManager,
        model: &NewModel,
        now: Timestamp,
        audit: &Audit<'_, Model>,
    ) -> Result<Model, ModelStoreError> {
        let mut transaction = self.immediate("create_model").await?;
        manager_allowed(&mut transaction, organization_id, environment, manager).await?;
        if find_connection_in(&mut transaction, organization_id, environment, model.connection_id)
            .await?
            .is_none()
        {
            return Err(ModelStoreError::ConnectionNotFound);
        }
        let exists: Option<i64> = sqlx::query_scalar(
            "SELECT 1 FROM models WHERE organization_id = ? AND environment = ? AND connection_id = ? \
             AND provider_model_id = ?",
        )
        .bind(organization_id.uuid())
        .bind(environment.as_str())
        .bind(model.connection_id.uuid())
        .bind(&model.provider_model_id)
        .fetch_optional(&mut *transaction)
        .await
        .map_err(failed("create_model"))?;
        if exists.is_some() {
            return Err(ModelStoreError::ModelExists);
        }
        let context_window = model
            .context_window
            .map(i64::try_from)
            .transpose()
            .map_err(|_| read("a context window past what's stored"))?;
        sqlx::query(
            "INSERT INTO models (id, organization_id, environment, connection_id, provider_model_id, name, author, \
             context_window, supports_tools, status, created_at, updated_at) \
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, 'available', ?, ?)",
        )
        .bind(model_id.uuid())
        .bind(organization_id.uuid())
        .bind(environment.as_str())
        .bind(model.connection_id.uuid())
        .bind(&model.provider_model_id)
        .bind(&model.name)
        .bind(&model.author)
        .bind(context_window)
        .bind(model.supports_tools)
        .bind(now.millis())
        .bind(now.millis())
        .execute(&mut *transaction)
        .await
        .map_err(failed("create_model"))?;
        let created = find_model_in(&mut transaction, organization_id, environment, model_id)
            .await?
            .ok_or_else(|| read("a model gone inside its own transaction"))?;
        crate::audit::record(&mut transaction, organization_id, &audit(&created))
            .await
            .map_err(failed("audit"))?;
        transaction.commit().await.map_err(failed("create_model"))?;
        Ok(created)
    }

    async fn rename_model(
        &self,
        organization_id: OrganizationId,
        environment: Environment,
        model_id: ModelId,
        manager: ModelManager,
        name: &str,
        now: Timestamp,
        audit: &Audit<'_, Model>,
    ) -> Result<Model, ModelStoreError> {
        let mut transaction = self.immediate("rename_model").await?;
        manager_allowed(&mut transaction, organization_id, environment, manager).await?;
        let renamed = sqlx::query(
            "UPDATE models SET name = ?, updated_at = ? WHERE organization_id = ? AND environment = ? AND id = ?",
        )
        .bind(name)
        .bind(now.millis())
        .bind(organization_id.uuid())
        .bind(environment.as_str())
        .bind(model_id.uuid())
        .execute(&mut *transaction)
        .await
        .map_err(failed("rename_model"))?;
        if renamed.rows_affected() == 0 {
            return Err(ModelStoreError::ModelNotFound);
        }
        let model = find_model_in(&mut transaction, organization_id, environment, model_id)
            .await?
            .ok_or(ModelStoreError::ModelNotFound)?;
        crate::audit::record(&mut transaction, organization_id, &audit(&model))
            .await
            .map_err(failed("audit"))?;
        transaction.commit().await.map_err(failed("rename_model"))?;
        Ok(model)
    }

    async fn delete_model(
        &self,
        organization_id: OrganizationId,
        environment: Environment,
        model_id: ModelId,
        manager: ModelManager,
        audit: &Audit<'_, Model>,
    ) -> Result<(), ModelStoreError> {
        let mut transaction = self.immediate("delete_model").await?;
        manager_allowed(&mut transaction, organization_id, environment, manager).await?;
        let model = find_model_in(&mut transaction, organization_id, environment, model_id)
            .await?
            .ok_or(ModelStoreError::ModelNotFound)?;
        let held = holders(
            &mut transaction,
            organization_id,
            environment,
            Removing::Model(model_id),
        )
        .await?;
        if !held.is_empty() {
            return Err(ModelStoreError::InUse(held));
        }
        sqlx::query("DELETE FROM models WHERE organization_id = ? AND environment = ? AND id = ?")
            .bind(organization_id.uuid())
            .bind(environment.as_str())
            .bind(model_id.uuid())
            .execute(&mut *transaction)
            .await
            .map_err(failed("delete_model"))?;
        crate::audit::record(&mut transaction, organization_id, &audit(&model))
            .await
            .map_err(failed("audit"))?;
        transaction.commit().await.map_err(failed("delete_model"))?;
        Ok(())
    }

    async fn default_model(
        &self,
        organization_id: OrganizationId,
        environment: Environment,
    ) -> Result<DefaultModel, ModelStoreError> {
        let mut transaction = self.reader("default_model").await?;
        let default = read_default(&mut transaction, organization_id, environment).await?;
        transaction.commit().await.map_err(failed("default_model"))?;
        Ok(default)
    }

    async fn set_default_model(
        &self,
        organization_id: OrganizationId,
        environment: Environment,
        manager: ModelManager,
        model_id: Option<ModelId>,
        updated_by: Option<UserId>,
        now: Timestamp,
        audit: &Audit<'_, DefaultModel>,
    ) -> Result<DefaultModel, ModelStoreError> {
        let mut transaction = self.immediate("set_default_model").await?;
        manager_allowed(&mut transaction, organization_id, environment, manager).await?;
        if let Some(model_id) = model_id
            && find_model_in(&mut transaction, organization_id, environment, model_id)
                .await?
                .is_none()
        {
            return Err(ModelStoreError::UnknownModel);
        }
        sqlx::query(
            "INSERT INTO default_models (id, organization_id, environment, model_id, updated_at, updated_by_user_id) \
             VALUES (?, ?, ?, ?, ?, ?) ON CONFLICT (organization_id, environment) DO UPDATE SET \
             model_id = excluded.model_id, updated_at = excluded.updated_at, \
             updated_by_user_id = excluded.updated_by_user_id",
        )
        .bind(Uuid::now_v7())
        .bind(organization_id.uuid())
        .bind(environment.as_str())
        .bind(model_id.map(|id| id.uuid()))
        .bind(now.millis())
        .bind(updated_by.map(|id| id.uuid()))
        .execute(&mut *transaction)
        .await
        .map_err(failed("set_default_model"))?;
        let default = read_default(&mut transaction, organization_id, environment).await?;
        crate::audit::record(&mut transaction, organization_id, &audit(&default))
            .await
            .map_err(failed("audit"))?;
        transaction.commit().await.map_err(failed("set_default_model"))?;
        Ok(default)
    }

    async fn run_model(
        &self,
        organization_id: OrganizationId,
        workspace_id: WorkspaceId,
        model_id: Option<ModelId>,
    ) -> Result<Option<RunModel>, ModelStoreError> {
        let mut transaction = self.reader("run_model").await?;
        let Some(environment) = workspace_environment(&mut transaction, organization_id, workspace_id).await? else {
            return Ok(None);
        };
        let row = match model_id {
            Some(model_id) => sqlx::query(
                "SELECT id, connection_id, provider_model_id FROM models \
                 WHERE organization_id = ? AND environment = ? AND id = ?",
            )
            .bind(organization_id.uuid())
            .bind(environment.as_str())
            .bind(model_id.uuid()),
            None => sqlx::query(
                "SELECT m.id, m.connection_id, m.provider_model_id FROM default_models d JOIN models m \
                 ON m.organization_id = d.organization_id AND m.environment = d.environment AND m.id = d.model_id \
                 WHERE d.organization_id = ? AND m.organization_id = ? AND d.environment = ? AND m.environment = ?",
            )
            .bind(organization_id.uuid())
            .bind(organization_id.uuid())
            .bind(environment.as_str())
            .bind(environment.as_str()),
        }
        .fetch_optional(&mut *transaction)
        .await
        .map_err(failed("run_model"))?;
        transaction.commit().await.map_err(failed("run_model"))?;
        row.map(|row| {
            Ok(RunModel {
                model_id: ModelId::from_uuid(row.try_get("id").map_err(read)?),
                connection_id: ModelConnectionId::from_uuid(row.try_get("connection_id").map_err(read)?),
                provider_model_id: row.try_get("provider_model_id").map_err(read)?,
            })
        })
        .transpose()
    }

    async fn route_state(
        &self,
        organization_id: OrganizationId,
        workspace_id: WorkspaceId,
        model_id: ModelId,
        connection_id: ModelConnectionId,
    ) -> Result<RouteState, ModelStoreError> {
        let mut transaction = self.reader("route_state").await?;
        let Some(environment) = workspace_environment(&mut transaction, organization_id, workspace_id).await? else {
            return Ok(RouteState::Removed);
        };
        let row = sqlx::query(
            "SELECT c.status, c.label, c.key_version FROM models m JOIN model_connections c \
             ON c.organization_id = m.organization_id AND c.environment = m.environment AND c.id = m.connection_id \
             WHERE m.organization_id = ? AND c.organization_id = ? AND m.environment = ? AND c.environment = ? \
             AND m.id = ? AND m.connection_id = ?",
        )
        .bind(organization_id.uuid())
        .bind(organization_id.uuid())
        .bind(environment.as_str())
        .bind(environment.as_str())
        .bind(model_id.uuid())
        .bind(connection_id.uuid())
        .fetch_optional(&mut *transaction)
        .await
        .map_err(failed("route_state"))?;
        transaction.commit().await.map_err(failed("route_state"))?;
        let Some(row) = row else {
            return Ok(RouteState::Removed);
        };
        let status: String = row.try_get("status").map_err(read)?;
        Ok(
            match ConnectionStatus::parse(&status).ok_or_else(|| read("an unknown connection status"))? {
                ConnectionStatus::InvalidKey => RouteState::KeyInvalid {
                    connection_label: row.try_get("label").map_err(read)?,
                },
                ConnectionStatus::Active => RouteState::Usable {
                    key_version: version(row.try_get("key_version").map_err(read)?)?,
                },
            },
        )
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use std::sync::Arc;

    use zupflow_conformance::model_store::{Prepare, Stores, Tenant};

    use super::*;
    use crate::schema::{Temporary, temporary};

    /// Makes the tenant's rows straight in the database: an organization, its
    /// workspaces, an admin, a viewer and a key per environment.
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
                    .bind(format!("W {}", &workspace.uuid().simple().to_string()[24..]))
                    .bind(format!("w-{}", workspace.uuid().simple()))
                    .execute(pool)
                    .await
                    .unwrap();
                }
                for (member, user, role) in [
                    (tenant.admin, tenant.admin_user, "admin"),
                    (tenant.viewer, tenant.viewer_user, "viewer"),
                ] {
                    sqlx::query("INSERT INTO users (id, name, email, created_at) VALUES (?, 'Jordan', ?, 0)")
                        .bind(user.uuid())
                        .bind(format!("{}@northwind.example", user.uuid().simple()))
                        .execute(pool)
                        .await
                        .unwrap();
                    sqlx::query(
                        "INSERT INTO members (id, organization_id, user_id, role, status, invited_at, joined_at) \
                         VALUES (?, ?, ?, ?, 'active', 0, 0)",
                    )
                    .bind(member.uuid())
                    .bind(org)
                    .bind(user.uuid())
                    .bind(role)
                    .execute(pool)
                    .await
                    .unwrap();
                }
                for (key, environment) in [(tenant.sandbox_key, "sandbox"), (tenant.production_key, "production")] {
                    let hash = key.uuid().as_bytes().repeat(2);
                    sqlx::query(
                        "INSERT INTO api_keys (id, organization_id, environment, name, prefix, last4, secret_hash, \
                         created_by_user_id, created_at) VALUES (?, ?, ?, 'Test', 'test', 'abcd', ?, ?, 0)",
                    )
                    .bind(key.uuid())
                    .bind(org)
                    .bind(environment)
                    .bind(hash)
                    .bind(tenant.admin_user.uuid())
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
        zupflow_conformance::model_store::run(
            Stores {
                models: &SqliteModelStore::new(database.clone()),
                agents: &crate::SqliteAgentStore::new(database.clone()),
                audit: &crate::SqliteAuditStore::new(database.clone()),
            },
            prepare(database),
        )
        .await;
    }

    /// The database itself refuses removing a model the default or an agent
    /// names, and a default naming another environment's model.
    #[tokio::test]
    async fn refuses_dangling_references_itself() {
        let Temporary {
            database,
            directory: _directory,
        } = temporary().await;
        let pool = database.pool();
        let org = Uuid::now_v7();
        sqlx::query("INSERT INTO organizations (id, name, slug, created_at) VALUES (?, 'Test', 'test', 0)")
            .bind(org)
            .execute(pool)
            .await
            .unwrap();
        let connection = Uuid::now_v7();
        sqlx::query(
            "INSERT INTO model_connections (id, organization_id, environment, kind, label, key_version, key_last4, \
             key_updated_at, status, created_at, updated_at) VALUES (?, ?, 'sandbox', 'openai', 'O', 1, 'abcd', 0, \
             'active', 0, 0)",
        )
        .bind(connection)
        .bind(org)
        .execute(pool)
        .await
        .unwrap();
        let model = Uuid::now_v7();
        sqlx::query(
            "INSERT INTO models (id, organization_id, environment, connection_id, provider_model_id, name, author, \
             context_window, supports_tools, status, created_at, updated_at) \
             VALUES (?, ?, 'sandbox', ?, 'gpt-5', 'GPT-5', 'openai', NULL, 1, 'available', 0, 0)",
        )
        .bind(model)
        .bind(org)
        .bind(connection)
        .execute(pool)
        .await
        .unwrap();
        let other_environment = sqlx::query(
            "INSERT INTO default_models (id, organization_id, environment, model_id, updated_at) \
             VALUES (?, ?, 'production', ?, 0)",
        )
        .bind(Uuid::now_v7())
        .bind(org)
        .bind(model)
        .execute(pool)
        .await;
        assert!(other_environment.is_err(), "a default of another environment's model");
        sqlx::query(
            "INSERT INTO default_models (id, organization_id, environment, model_id, updated_at) \
             VALUES (?, ?, 'sandbox', ?, 0)",
        )
        .bind(Uuid::now_v7())
        .bind(org)
        .bind(model)
        .execute(pool)
        .await
        .unwrap();
        let removed = sqlx::query("DELETE FROM model_connections WHERE organization_id = ? AND id = ?")
            .bind(org)
            .bind(connection)
            .execute(pool)
            .await;
        assert!(removed.is_err(), "the default still names a model of the connection");

        // An agent names a model of its own organization, or none: never one
        // that isn't there, and the model stays while the agent names it.
        let workspace = Uuid::now_v7();
        sqlx::query(
            "INSERT INTO workspaces (id, organization_id, environment, name, slug, created_at) \
             VALUES (?, ?, 'sandbox', 'W', 'w', 0)",
        )
        .bind(workspace)
        .bind(org)
        .execute(pool)
        .await
        .unwrap();
        let agent = |model_id: Uuid| {
            sqlx::query(
                "INSERT INTO agents (id, organization_id, workspace_id, name, description, model_id, instructions, \
                 status, created_at, updated_at) VALUES (?, ?, ?, 'A', '', ?, '', 'active', 0, 0)",
            )
            .bind(Uuid::now_v7())
            .bind(org)
            .bind(workspace)
            .bind(model_id)
            .execute(pool)
        };
        assert!(agent(Uuid::now_v7()).await.is_err(), "a model that isn't there");
        agent(model).await.unwrap();
        sqlx::query("DELETE FROM default_models WHERE organization_id = ?")
            .bind(org)
            .execute(pool)
            .await
            .unwrap();
        let removed = sqlx::query("DELETE FROM models WHERE organization_id = ? AND id = ?")
            .bind(org)
            .bind(model)
            .execute(pool)
            .await;
        assert!(removed.is_err(), "an agent still names the model");
    }
}
