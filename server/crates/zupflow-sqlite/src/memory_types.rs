//! [`SqliteMemoryTypeStore`]: memory types in `memory_types`, the
//! organization's default in `default_memory_types`. Every statement repeats
//! the organization, joins and subqueries included, beside a type's or an
//! agent's ID.
//!
//! Each change is an immediate transaction that first checks, in it, that the
//! acting member is still an active owner or admin, then the rules that must
//! hold across concurrent changes (a name is the organization's once, a type
//! the default or an agent names isn't deleted, a default is one of the
//! organization's types), then writes the change and its audit event. A
//! type's notes reference it with a cascading key: they go with it, in the
//! statement that deletes it, and the transaction queues the job that has
//! its kind forget them wherever it keeps them ([`queue_forgetting`], which
//! deleting an agent or a workspace queues too).

use async_trait::async_trait;
use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use serde_json::{Map, Value};
use sqlx::sqlite::SqliteRow;
use sqlx::{Row, Sqlite, SqliteConnection, Transaction};
use uuid::Uuid;
use zupflow_core::audit::Audit;
use zupflow_core::ids::{AgentId, MemberId, MemoryTypeId, OrganizationId, UserId, WorkspaceId};
use zupflow_core::memory::type_store::{
    DefaultMemoryType, MemoryType, MemoryTypeChanges, MemoryTypeError, MemoryTypeHolders, MemoryTypeStore,
    NAMED_HOLDERS, NewMemoryType, RunMemory,
};
use zupflow_core::memory::{Forget, MemoryKind, MemoryWrites};
use zupflow_core::model_store::HoldingAgent;
use zupflow_core::organizations::User;
use zupflow_core::scope::Scope;
use zupflow_core::time::Timestamp;
use zupflow_core::workspaces::Page;

use crate::Database;
use crate::job_queue::insert_job;

/// The forget jobs of a deletion couldn't be queued; the cause is logged,
/// and the deletion fails with it.
#[derive(Debug)]
pub(crate) struct ForgettingNotQueued;

/// Queues, in a deletion's own transaction, the job that has a memory kind
/// forget what went ([`Forget::job`]): one for each kind of the
/// organization's types as they are now, or for `kind` alone, a deleted
/// type's own. In the organization's scope, so a workspace deleted later
/// doesn't take it along.
pub(crate) async fn queue_forgetting(
    connection: &mut SqliteConnection,
    what: &Forget,
    kind: Option<&MemoryKind>,
) -> Result<(), ForgettingNotQueued> {
    let organization_id = what.organization_id();
    let kinds = match kind {
        Some(kind) => vec![kind.clone()],
        None => {
            let stored: Vec<String> =
                sqlx::query_scalar("SELECT DISTINCT kind FROM memory_types WHERE organization_id = ? ORDER BY kind")
                    .bind(organization_id.uuid())
                    .fetch_all(&mut *connection)
                    .await
                    .map_err(|error| {
                        tracing::error!(event = "memory.forgetting_not_queued", %error);
                        ForgettingNotQueued
                    })?;
            stored
                .iter()
                .map(|kind| MemoryKind::parse(kind))
                .collect::<Option<Vec<_>>>()
                .ok_or_else(|| {
                    tracing::error!(event = "memory.forgetting_not_queued", error = "a malformed kind");
                    ForgettingNotQueued
                })?
        }
    };
    for kind in &kinds {
        let job = what.job(kind).ok_or(ForgettingNotQueued)?;
        insert_job(
            &mut *connection,
            &Scope::Organization(organization_id),
            &job.kind,
            job.payload,
            job.options,
        )
        .await
        .map_err(|error| {
            tracing::error!(event = "memory.forgetting_not_queued", %error);
            ForgettingNotQueued
        })?;
    }
    Ok(())
}

pub struct SqliteMemoryTypeStore {
    database: Database,
}

impl SqliteMemoryTypeStore {
    pub fn new(database: Database) -> Self {
        Self { database }
    }

    async fn immediate(&self, operation: &'static str) -> Result<Transaction<'static, Sqlite>, MemoryTypeError> {
        self.database
            .pool()
            .begin_with("BEGIN IMMEDIATE")
            .await
            .map_err(failed(operation))
    }

    async fn reader(&self, operation: &'static str) -> Result<Transaction<'static, Sqlite>, MemoryTypeError> {
        self.database.pool().begin().await.map_err(failed(operation))
    }
}

fn failed(operation: &'static str) -> impl FnOnce(sqlx::Error) -> MemoryTypeError {
    move |error| {
        tracing::error!(event = "memory_types.database_error", operation, %error);
        MemoryTypeError::Unavailable
    }
}

fn read(error: impl std::fmt::Display) -> MemoryTypeError {
    tracing::error!(event = "memory_types.unreadable_row", %error);
    MemoryTypeError::Unavailable
}

/// A type's columns, whether it's the default and how many agents name it,
/// from `memory_types t`, with the organization bound as `?1`. A macro, so
/// queries stay static strings.
macro_rules! type_select {
    () => {
        "SELECT t.id, t.organization_id, t.name, t.kind, t.settings, t.writes, t.created_at, t.updated_at, \
         EXISTS (SELECT 1 FROM default_memory_types d WHERE d.organization_id = ?1 AND d.memory_type_id = t.id) \
         AS is_default, \
         (SELECT count(*) FROM agents a WHERE a.organization_id = ?1 AND a.memory_type_id = t.id) AS agent_count \
         FROM memory_types t WHERE t.organization_id = ?1"
    };
}

const FIND_TYPE: &str = concat!(type_select!(), " AND t.id = ?2");
/// A page by name (any case), then ID, after the position `?2` (name),
/// `?3` (ID) when given; `?4` is the limit.
const LIST_TYPES: &str = concat!(
    type_select!(),
    " AND (?2 IS NULL OR (lower(t.name), t.id) > (?2, ?3)) ORDER BY lower(t.name), t.id LIMIT ?4"
);
const COUNT_TYPES: &str = "SELECT count(*) FROM memory_types WHERE organization_id = ?";
/// The acting member's user, while they're an active owner or admin.
const MANAGER: &str = "SELECT user_id FROM members WHERE organization_id = ? AND id = ? \
                       AND status = 'active' AND role IN ('owner', 'admin')";
/// Another of the organization's types by that name: the organization, the
/// name, then the type it may be.
const NAME_TAKEN: &str = "SELECT 1 FROM memory_types WHERE organization_id = ? AND name = ? AND id <> ?";
const IS_DEFAULT: &str = "SELECT 1 FROM default_memory_types WHERE organization_id = ? AND memory_type_id = ?";
/// The first agents naming the type, with how many there are: the
/// organization, then the type.
const HOLDING_AGENTS: &str = "SELECT a.name AS agent_name, w.name AS workspace_name, count(*) OVER () AS total \
                              FROM agents a \
                              JOIN workspaces w ON w.organization_id = a.organization_id AND w.id = a.workspace_id \
                              WHERE a.organization_id = ?1 AND w.organization_id = ?1 AND a.memory_type_id = ?2 \
                              ORDER BY a.created_at, a.id LIMIT ?3";
/// The type a run of the agent uses: its own, or the organization's
/// default; no row when its memory is off or there's no default.
const RUN_MEMORY: &str = "SELECT t.id, t.kind, t.settings, t.writes FROM agents a \
                          JOIN memory_types t ON t.organization_id = a.organization_id AND t.id = CASE a.memory \
                          WHEN 'type' THEN a.memory_type_id \
                          WHEN 'default' THEN (SELECT d.memory_type_id FROM default_memory_types d \
                                               WHERE d.organization_id = ?1) \
                          END \
                          WHERE a.organization_id = ?1 AND a.workspace_id = ?2 AND a.id = ?3 \
                          AND t.organization_id = ?1";

fn kind_of(row: &SqliteRow) -> Result<MemoryKind, MemoryTypeError> {
    let kind: String = row.try_get("kind").map_err(read)?;
    MemoryKind::parse(&kind).ok_or_else(|| read("a memory kind that isn't a kind's form"))
}

fn writes_of(row: &SqliteRow) -> Result<MemoryWrites, MemoryTypeError> {
    let writes: String = row.try_get("writes").map_err(read)?;
    MemoryWrites::parse(&writes).ok_or_else(|| read("an unknown writes level"))
}

fn settings_of(row: &SqliteRow) -> Result<Map<String, Value>, MemoryTypeError> {
    let settings: String = row.try_get("settings").map_err(read)?;
    serde_json::from_str(&settings).map_err(read)
}

fn type_from(row: &SqliteRow) -> Result<MemoryType, MemoryTypeError> {
    let agent_count: i64 = row.try_get("agent_count").map_err(read)?;
    Ok(MemoryType {
        id: MemoryTypeId::from_uuid(row.try_get("id").map_err(read)?),
        organization_id: OrganizationId::from_uuid(row.try_get("organization_id").map_err(read)?),
        name: row.try_get("name").map_err(read)?,
        kind: kind_of(row)?,
        settings: settings_of(row)?,
        writes: writes_of(row)?,
        agent_count: u64::try_from(agent_count).map_err(read)?,
        is_default: row.try_get("is_default").map_err(read)?,
        created_at: Timestamp::from_millis(row.try_get("created_at").map_err(read)?),
        updated_at: Timestamp::from_millis(row.try_get("updated_at").map_err(read)?),
    })
}

fn settings_json(settings: &Map<String, Value>) -> Result<String, MemoryTypeError> {
    serde_json::to_string(settings).map_err(read)
}

/// The acting member's user, while they may still change memory types: an
/// active owner or admin of the organization.
async fn manager_user(
    connection: &mut SqliteConnection,
    organization_id: OrganizationId,
    manager: MemberId,
) -> Result<UserId, MemoryTypeError> {
    let user: Option<Uuid> = sqlx::query_scalar(MANAGER)
        .bind(organization_id.uuid())
        .bind(manager.uuid())
        .fetch_optional(connection)
        .await
        .map_err(failed("manager_user"))?;
    user.map(UserId::from_uuid).ok_or(MemoryTypeError::NotAllowed)
}

async fn find_type_in(
    connection: &mut SqliteConnection,
    organization_id: OrganizationId,
    memory_type_id: MemoryTypeId,
) -> Result<Option<MemoryType>, MemoryTypeError> {
    let row = sqlx::query(FIND_TYPE)
        .bind(organization_id.uuid())
        .bind(memory_type_id.uuid())
        .fetch_optional(connection)
        .await
        .map_err(failed("find_type"))?;
    row.as_ref().map(type_from).transpose()
}

/// Refuses a name another of the organization's types has.
async fn check_name(
    connection: &mut SqliteConnection,
    organization_id: OrganizationId,
    memory_type_id: MemoryTypeId,
    name: &str,
) -> Result<(), MemoryTypeError> {
    let taken: Option<i64> = sqlx::query_scalar(NAME_TAKEN)
        .bind(organization_id.uuid())
        .bind(name)
        .bind(memory_type_id.uuid())
        .fetch_optional(connection)
        .await
        .map_err(failed("check_name"))?;
    match taken {
        Some(_) => Err(MemoryTypeError::NameTaken),
        None => Ok(()),
    }
}

/// What keeps the type from being deleted, in this organization only.
async fn holders(
    connection: &mut SqliteConnection,
    organization_id: OrganizationId,
    memory_type_id: MemoryTypeId,
) -> Result<MemoryTypeHolders, MemoryTypeError> {
    let is_default: Option<i64> = sqlx::query_scalar(IS_DEFAULT)
        .bind(organization_id.uuid())
        .bind(memory_type_id.uuid())
        .fetch_optional(&mut *connection)
        .await
        .map_err(failed("holders"))?;
    let rows = sqlx::query(HOLDING_AGENTS)
        .bind(organization_id.uuid())
        .bind(memory_type_id.uuid())
        .bind(i64::try_from(NAMED_HOLDERS).unwrap_or(5))
        .fetch_all(&mut *connection)
        .await
        .map_err(failed("holders"))?;
    let mut holders = MemoryTypeHolders {
        is_default: is_default.is_some(),
        agents: Vec::with_capacity(rows.len()),
        agent_total: 0,
    };
    for row in &rows {
        let total: i64 = row.try_get("total").map_err(read)?;
        holders.agent_total = u64::try_from(total).map_err(read)?;
        holders.agents.push(HoldingAgent {
            agent: row.try_get("agent_name").map_err(read)?,
            workspace: row.try_get("workspace_name").map_err(read)?,
        });
    }
    Ok(holders)
}

async fn read_default(
    connection: &mut SqliteConnection,
    organization_id: OrganizationId,
) -> Result<DefaultMemoryType, MemoryTypeError> {
    let row = sqlx::query(
        "SELECT d.memory_type_id, d.updated_at, u.id AS user_id, u.name AS user_name, u.email AS user_email, \
         u.avatar_url AS user_avatar_url \
         FROM default_memory_types d LEFT JOIN users u ON u.id = d.updated_by_user_id \
         WHERE d.organization_id = ?",
    )
    .bind(organization_id.uuid())
    .fetch_optional(&mut *connection)
    .await
    .map_err(failed("default_type"))?;
    let Some(row) = row else {
        return Ok(DefaultMemoryType {
            memory_type: None,
            updated_at: None,
            updated_by: None,
        });
    };
    let memory_type_id: Option<Uuid> = row.try_get("memory_type_id").map_err(read)?;
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
    let memory_type = match memory_type_id {
        None => None,
        Some(id) => find_type_in(connection, organization_id, MemoryTypeId::from_uuid(id)).await?,
    };
    Ok(DefaultMemoryType {
        memory_type,
        updated_at: Some(Timestamp::from_millis(row.try_get("updated_at").map_err(read)?)),
        updated_by,
    })
}

/// A position in the list of types: its name as sorted, then its ID.
struct Position {
    name: String,
    id: Uuid,
}

impl Position {
    fn of(memory_type: &MemoryType) -> Self {
        Self {
            // SQLite's lower() folds ASCII only; fold the same way here.
            name: memory_type.name.to_ascii_lowercase(),
            id: memory_type.id.uuid(),
        }
    }

    /// The type's prefixed ID, never its bare UUID: a cursor leaves the server.
    fn encode(&self) -> String {
        URL_SAFE_NO_PAD.encode(format!("{}\n{}", MemoryTypeId::from_uuid(self.id), self.name))
    }

    fn decode(cursor: &str) -> Option<Self> {
        let raw = String::from_utf8(URL_SAFE_NO_PAD.decode(cursor).ok()?).ok()?;
        let (id, name) = raw.split_once('\n')?;
        let id = MemoryTypeId::parse(id).ok()?.uuid();
        (name.len() <= 240).then(|| Self {
            name: name.to_owned(),
            id,
        })
    }
}

#[async_trait]
impl MemoryTypeStore for SqliteMemoryTypeStore {
    async fn list_types(
        &self,
        organization_id: OrganizationId,
        cursor: Option<&str>,
        limit: u32,
    ) -> Result<Page<MemoryType>, MemoryTypeError> {
        let after = cursor
            .map(|cursor| Position::decode(cursor).ok_or(MemoryTypeError::InvalidCursor))
            .transpose()?;
        let limit = limit.clamp(1, zupflow_core::memory::type_store::MAX_PAGE);
        // One read transaction, so the count and the page are of one moment.
        let mut transaction = self.reader("list_types").await?;
        let total: i64 = sqlx::query_scalar(COUNT_TYPES)
            .bind(organization_id.uuid())
            .fetch_one(&mut *transaction)
            .await
            .map_err(failed("list_types"))?;
        let rows = sqlx::query(LIST_TYPES)
            .bind(organization_id.uuid())
            .bind(after.as_ref().map(|after| after.name.clone()))
            .bind(after.as_ref().map(|after| after.id))
            .bind(i64::from(limit) + 1)
            .fetch_all(&mut *transaction)
            .await
            .map_err(failed("list_types"))?;
        transaction.commit().await.map_err(failed("list_types"))?;
        let mut items = rows.iter().map(type_from).collect::<Result<Vec<_>, _>>()?;
        let more = items.len() > limit as usize;
        items.truncate(limit as usize);
        let next_cursor = more
            .then(|| items.last().map(|last| Position::of(last).encode()))
            .flatten();
        Ok(Page {
            items,
            next_cursor,
            total: u64::try_from(total).map_err(read)?,
        })
    }

    async fn find_type(
        &self,
        organization_id: OrganizationId,
        memory_type_id: MemoryTypeId,
    ) -> Result<Option<MemoryType>, MemoryTypeError> {
        let mut connection = self.database.pool().acquire().await.map_err(failed("find_type"))?;
        find_type_in(&mut connection, organization_id, memory_type_id).await
    }

    async fn create_type(
        &self,
        organization_id: OrganizationId,
        memory_type_id: MemoryTypeId,
        manager: MemberId,
        memory_type: &NewMemoryType,
        now: Timestamp,
        audit: &Audit<'_, MemoryType>,
    ) -> Result<MemoryType, MemoryTypeError> {
        let mut transaction = self.immediate("create_type").await?;
        manager_user(&mut transaction, organization_id, manager).await?;
        check_name(&mut transaction, organization_id, memory_type_id, &memory_type.name).await?;
        sqlx::query(
            "INSERT INTO memory_types (id, organization_id, name, kind, settings, writes, created_at, updated_at) \
             VALUES (?, ?, ?, ?, ?, ?, ?, ?)",
        )
        .bind(memory_type_id.uuid())
        .bind(organization_id.uuid())
        .bind(&memory_type.name)
        .bind(memory_type.kind.as_str())
        .bind(settings_json(memory_type.settings.values())?)
        .bind(memory_type.writes.as_str())
        .bind(now.millis())
        .bind(now.millis())
        .execute(&mut *transaction)
        .await
        .map_err(failed("create_type"))?;
        let created = find_type_in(&mut transaction, organization_id, memory_type_id)
            .await?
            .ok_or_else(|| read("a memory type gone inside its own transaction"))?;
        crate::audit::record_at(&mut transaction, organization_id, &audit(&created), now)
            .await
            .map_err(failed("audit"))?;
        transaction.commit().await.map_err(failed("create_type"))?;
        Ok(created)
    }

    async fn update_type(
        &self,
        organization_id: OrganizationId,
        memory_type_id: MemoryTypeId,
        manager: MemberId,
        changes: &MemoryTypeChanges,
        now: Timestamp,
        audit: &Audit<'_, MemoryType>,
    ) -> Result<MemoryType, MemoryTypeError> {
        let mut transaction = self.immediate("update_type").await?;
        manager_user(&mut transaction, organization_id, manager).await?;
        if find_type_in(&mut transaction, organization_id, memory_type_id)
            .await?
            .is_none()
        {
            return Err(MemoryTypeError::NotFound);
        }
        if let Some(name) = &changes.name {
            check_name(&mut transaction, organization_id, memory_type_id, name).await?;
        }
        let settings = changes
            .settings
            .as_ref()
            .map(|settings| settings_json(settings.values()))
            .transpose()?;
        sqlx::query(
            "UPDATE memory_types SET name = coalesce(?, name), settings = coalesce(?, settings), \
             writes = coalesce(?, writes), updated_at = ? WHERE organization_id = ? AND id = ?",
        )
        .bind(changes.name.as_deref())
        .bind(settings)
        .bind(changes.writes.map(MemoryWrites::as_str))
        .bind(now.millis())
        .bind(organization_id.uuid())
        .bind(memory_type_id.uuid())
        .execute(&mut *transaction)
        .await
        .map_err(failed("update_type"))?;
        let updated = find_type_in(&mut transaction, organization_id, memory_type_id)
            .await?
            .ok_or(MemoryTypeError::NotFound)?;
        crate::audit::record_at(&mut transaction, organization_id, &audit(&updated), now)
            .await
            .map_err(failed("audit"))?;
        transaction.commit().await.map_err(failed("update_type"))?;
        Ok(updated)
    }

    async fn delete_type(
        &self,
        organization_id: OrganizationId,
        memory_type_id: MemoryTypeId,
        manager: MemberId,
        audit: &Audit<'_, MemoryType>,
    ) -> Result<(), MemoryTypeError> {
        let mut transaction = self.immediate("delete_type").await?;
        manager_user(&mut transaction, organization_id, manager).await?;
        let memory_type = find_type_in(&mut transaction, organization_id, memory_type_id)
            .await?
            .ok_or(MemoryTypeError::NotFound)?;
        let held = holders(&mut transaction, organization_id, memory_type_id).await?;
        if !held.is_empty() {
            return Err(MemoryTypeError::InUse(held));
        }
        // Its notes reference it with a cascading key: they go in this one
        // statement, and their search entries with them. Its kind is told
        // to forget them too, wherever it keeps them.
        sqlx::query("DELETE FROM memory_types WHERE organization_id = ? AND id = ?")
            .bind(organization_id.uuid())
            .bind(memory_type_id.uuid())
            .execute(&mut *transaction)
            .await
            .map_err(failed("delete_type"))?;
        let forget = Forget::Type {
            organization_id,
            memory_type_id,
        };
        queue_forgetting(&mut transaction, &forget, Some(&memory_type.kind))
            .await
            .map_err(|_| MemoryTypeError::Unavailable)?;
        crate::audit::record(&mut transaction, organization_id, &audit(&memory_type))
            .await
            .map_err(failed("audit"))?;
        transaction.commit().await.map_err(failed("delete_type"))?;
        Ok(())
    }

    async fn default_type(&self, organization_id: OrganizationId) -> Result<DefaultMemoryType, MemoryTypeError> {
        let mut transaction = self.reader("default_type").await?;
        let default = read_default(&mut transaction, organization_id).await?;
        transaction.commit().await.map_err(failed("default_type"))?;
        Ok(default)
    }

    async fn set_default_type(
        &self,
        organization_id: OrganizationId,
        manager: MemberId,
        memory_type_id: Option<MemoryTypeId>,
        now: Timestamp,
        audit: &Audit<'_, DefaultMemoryType>,
    ) -> Result<DefaultMemoryType, MemoryTypeError> {
        let mut transaction = self.immediate("set_default_type").await?;
        let user = manager_user(&mut transaction, organization_id, manager).await?;
        if let Some(memory_type_id) = memory_type_id
            && find_type_in(&mut transaction, organization_id, memory_type_id)
                .await?
                .is_none()
        {
            return Err(MemoryTypeError::UnknownType);
        }
        sqlx::query(
            "INSERT INTO default_memory_types (id, organization_id, memory_type_id, updated_at, updated_by_user_id) \
             VALUES (?, ?, ?, ?, ?) ON CONFLICT (organization_id) DO UPDATE SET \
             memory_type_id = excluded.memory_type_id, updated_at = excluded.updated_at, \
             updated_by_user_id = excluded.updated_by_user_id",
        )
        .bind(Uuid::now_v7())
        .bind(organization_id.uuid())
        .bind(memory_type_id.map(|id| id.uuid()))
        .bind(now.millis())
        .bind(user.uuid())
        .execute(&mut *transaction)
        .await
        .map_err(failed("set_default_type"))?;
        let default = read_default(&mut transaction, organization_id).await?;
        crate::audit::record_at(&mut transaction, organization_id, &audit(&default), now)
            .await
            .map_err(failed("audit"))?;
        transaction.commit().await.map_err(failed("set_default_type"))?;
        Ok(default)
    }

    async fn run_memory(
        &self,
        organization_id: OrganizationId,
        workspace_id: WorkspaceId,
        agent_id: AgentId,
    ) -> Result<Option<RunMemory>, MemoryTypeError> {
        let row = sqlx::query(RUN_MEMORY)
            .bind(organization_id.uuid())
            .bind(workspace_id.uuid())
            .bind(agent_id.uuid())
            .fetch_optional(self.database.pool())
            .await
            .map_err(failed("run_memory"))?;
        row.map(|row| {
            Ok(RunMemory {
                memory_type_id: MemoryTypeId::from_uuid(row.try_get("id").map_err(read)?),
                kind: kind_of(&row)?,
                settings: settings_of(&row)?,
                writes: writes_of(&row)?,
            })
        })
        .transpose()
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use std::sync::Arc;

    use zupflow_conformance::memory_type_store::{Prepare, Stores, Tenant};

    use super::*;
    use crate::schema::{Temporary, temporary};

    /// Makes the tenant's rows straight in the database: an organization, its
    /// two workspaces, and its members with their users.
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
                for (member, user, role, status) in [
                    (tenant.owner, tenant.owner_user, "owner", "active"),
                    (tenant.admin, tenant.admin_user, "admin", "active"),
                    (tenant.developer, tenant.developer_user, "developer", "active"),
                    (tenant.invited_admin, tenant.invited_admin_user, "admin", "invited"),
                ] {
                    sqlx::query("INSERT INTO users (id, name, email, created_at) VALUES (?, ?, ?, 0)")
                        .bind(user.uuid())
                        .bind(format!("The {role}"))
                        .bind(format!("{}@test.example", user.uuid().simple()))
                        .execute(pool)
                        .await
                        .unwrap();
                    sqlx::query(
                        "INSERT INTO members (id, organization_id, user_id, role, status, invited_at, joined_at) \
                         VALUES (?, ?, ?, ?, ?, 0, CASE WHEN ? = 'active' THEN 0 END)",
                    )
                    .bind(member.uuid())
                    .bind(org)
                    .bind(user.uuid())
                    .bind(role)
                    .bind(status)
                    .bind(status)
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
        zupflow_conformance::memory_type_store::run(
            Stores {
                types: &SqliteMemoryTypeStore::new(database.clone()),
                agents: &crate::SqliteAgentStore::new(database.clone()),
                notes: &crate::SqliteNoteStore::new(database.clone()),
                audit: &crate::SqliteAuditStore::new(database.clone()),
                workspaces: &crate::SqliteWorkspaceStore::new(database.clone()),
                jobs: &crate::SqliteJobQueue::new(database.clone()),
            },
            prepare(database),
        )
        .await;
    }

    /// The database itself refuses deleting a type the default or an agent
    /// names, a default or an agent naming another organization's type, and
    /// an agent's memory and type that don't go together.
    #[tokio::test]
    async fn refuses_dangling_references_itself() {
        let Temporary {
            database,
            directory: _directory,
        } = temporary().await;
        let pool = database.pool();
        let (org, other) = (Uuid::now_v7(), Uuid::now_v7());
        for (organization, slug) in [(org, "test"), (other, "other")] {
            sqlx::query("INSERT INTO organizations (id, name, slug, created_at) VALUES (?, 'Test', ?, 0)")
                .bind(organization)
                .bind(slug)
                .execute(pool)
                .await
                .unwrap();
        }
        let memory_type = |organization: Uuid, name: &'static str| async move {
            let id = Uuid::now_v7();
            sqlx::query(
                "INSERT INTO memory_types (id, organization_id, name, kind, settings, writes, created_at, \
                 updated_at) VALUES (?, ?, ?, 'notes', '{}', 'allow', 0, 0)",
            )
            .bind(id)
            .bind(organization)
            .bind(name)
            .execute(pool)
            .await
            .unwrap();
            id
        };
        let ours = memory_type(org, "Ours").await;
        let theirs = memory_type(other, "Theirs").await;
        for (kind, settings, writes) in [
            ("Notes", "{}", "allow"),
            ("notes", "[]", "allow"),
            ("notes", "{}", "always"),
        ] {
            let refused = sqlx::query(
                "INSERT INTO memory_types (id, organization_id, name, kind, settings, writes, created_at, \
                 updated_at) VALUES (?, ?, 'Refused', ?, ?, ?, 0, 0)",
            )
            .bind(Uuid::now_v7())
            .bind(org)
            .bind(kind)
            .bind(settings)
            .bind(writes)
            .execute(pool)
            .await;
            assert!(refused.is_err(), "{kind} {settings} {writes}");
        }
        let default = |memory_type: Uuid| {
            sqlx::query(
                "INSERT INTO default_memory_types (id, organization_id, memory_type_id, updated_at) \
                 VALUES (?, ?, ?, 0) ON CONFLICT (organization_id) DO UPDATE SET \
                 memory_type_id = excluded.memory_type_id",
            )
            .bind(Uuid::now_v7())
            .bind(org)
            .bind(memory_type)
            .execute(pool)
        };
        assert!(
            default(theirs).await.is_err(),
            "a default of another organization's type"
        );
        default(ours).await.unwrap();
        let removed = sqlx::query("DELETE FROM memory_types WHERE organization_id = ? AND id = ?")
            .bind(org)
            .bind(ours)
            .execute(pool)
            .await;
        assert!(removed.is_err(), "the default names it");
        sqlx::query("DELETE FROM default_memory_types WHERE organization_id = ?")
            .bind(org)
            .execute(pool)
            .await
            .unwrap();

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
        let agent = |memory: &'static str, memory_type: Option<Uuid>| {
            sqlx::query(
                "INSERT INTO agents (id, organization_id, workspace_id, name, description, memory, memory_type_id, \
                 instructions, status, created_at, updated_at) VALUES (?, ?, ?, 'A', '', ?, ?, '', 'active', 0, 0)",
            )
            .bind(Uuid::now_v7())
            .bind(org)
            .bind(workspace)
            .bind(memory)
            .bind(memory_type)
            .execute(pool)
        };
        for (memory, memory_type) in [
            ("type", None),
            ("default", Some(ours)),
            ("off", Some(ours)),
            ("type", Some(theirs)),
            ("type", Some(Uuid::now_v7())),
            ("none", None),
        ] {
            assert!(agent(memory, memory_type).await.is_err(), "{memory} {memory_type:?}");
        }
        agent("type", Some(ours)).await.unwrap();
        agent("off", None).await.unwrap();
        let removed = sqlx::query("DELETE FROM memory_types WHERE organization_id = ? AND id = ?")
            .bind(org)
            .bind(ours)
            .execute(pool)
            .await;
        assert!(removed.is_err(), "an agent names it");
    }
}
