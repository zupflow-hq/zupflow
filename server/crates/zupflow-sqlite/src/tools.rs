//! [`SqliteToolRegistry`]: the tool registry, each installation's tools in
//! `installation_tools`. Every statement repeats the full scope: the
//! organization, the workspace and the installation.

use async_trait::async_trait;
use sqlx::Row;
use sqlx::sqlite::{SqliteArguments, SqliteRow};
use sqlx::{Sqlite, Transaction};
use uuid::Uuid;
use zupflow_core::apps::ToolEffect;
use zupflow_core::ids::{InstallationId, OrganizationId, WorkspaceId};
use zupflow_core::time::Timestamp;
use zupflow_core::tools::{
    ListedTool, ListingChanges, MAX_GONE, MAX_PAGE, RegisteredTool, ToolError, ToolPage, ToolRegistry, ToolRevision,
    ToolStatus, validate_listing,
};

use crate::Database;

pub struct SqliteToolRegistry {
    database: Database,
}

impl SqliteToolRegistry {
    pub fn new(database: Database) -> Self {
        Self { database }
    }
}

fn failed(operation: &'static str) -> impl FnOnce(sqlx::Error) -> ToolError {
    move |error| {
        tracing::error!(event = "tools.database_error", operation, %error);
        ToolError::Unavailable
    }
}

fn read(error: impl std::fmt::Display) -> ToolError {
    tracing::error!(event = "tools.unreadable_row", %error);
    ToolError::Unavailable
}

macro_rules! columns {
    () => {
        "name, effect, pinned_definition, pinned_hash, latest_definition, latest_hash, first_seen_at, seen_at, \
         changed_at, gone_at"
    };
}

// Each statement starts its parameters with the scope, in [`bind_scope`]'s
// order: organization, workspace, installation.
const INSTALLATION_EXISTS: &str =
    "SELECT 1 FROM installations WHERE organization_id = ? AND workspace_id = ? AND id = ?";
const SELECT: &str = concat!(
    "SELECT ",
    columns!(),
    " FROM installation_tools WHERE organization_id = ? AND workspace_id = ? AND installation_id = ? ORDER BY name"
);
/// One page, by name: the scope, then the name to start after (or NULL for the first page), then the limit.
const PAGE: &str = concat!(
    "SELECT ",
    columns!(),
    " FROM installation_tools WHERE organization_id = ? AND workspace_id = ? AND installation_id = ? \
     AND (?4 IS NULL OR name > ?4) ORDER BY name LIMIT ?5"
);
const COUNT: &str =
    "SELECT count(*) FROM installation_tools WHERE organization_id = ? AND workspace_id = ? AND installation_id = ?";
/// Gone tools beyond the most kept, those gone longest first: the scope, then
/// the scope again for the inner query, then how many to keep.
const PRUNE_GONE: &str = "DELETE FROM installation_tools \
     WHERE organization_id = ? AND workspace_id = ? AND installation_id = ? AND gone_at IS NOT NULL AND name IN ( \
     SELECT name FROM installation_tools \
     WHERE organization_id = ? AND workspace_id = ? AND installation_id = ? AND gone_at IS NOT NULL \
     ORDER BY gone_at DESC, name ASC LIMIT -1 OFFSET ?)";
const INSERT: &str = "INSERT INTO installation_tools (organization_id, workspace_id, installation_id, id, name, effect, \
                      pinned_definition, pinned_hash, first_seen_at, seen_at) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?)";
/// Listed as pinned: no latest, not gone.
const CURRENT: &str = "UPDATE installation_tools SET latest_definition = NULL, latest_hash = NULL, changed_at = NULL, \
                       gone_at = NULL, seen_at = ? \
                       WHERE organization_id = ? AND workspace_id = ? AND installation_id = ? AND name = ?";
/// Listed as its latest already: seen, not gone.
const SEEN: &str = "UPDATE installation_tools SET gone_at = NULL, seen_at = ? \
                    WHERE organization_id = ? AND workspace_id = ? AND installation_id = ? AND name = ?";
/// Listed with a new definition.
const CHANGED: &str = "UPDATE installation_tools SET latest_definition = ?, latest_hash = ?, changed_at = ?, \
                       gone_at = NULL, seen_at = ? \
                       WHERE organization_id = ? AND workspace_id = ? AND installation_id = ? AND name = ?";
/// One tool, by name: the scope, then the name.
const FIND: &str = concat!(
    "SELECT ",
    columns!(),
    " FROM installation_tools WHERE organization_id = ? AND workspace_id = ? AND installation_id = ? AND name = ?"
);
/// The latest definition pinned: the scope, then the name.
const ACCEPT: &str = concat!(
    "UPDATE installation_tools SET pinned_definition = latest_definition, pinned_hash = latest_hash, \
     latest_definition = NULL, latest_hash = NULL, changed_at = NULL \
     WHERE organization_id = ? AND workspace_id = ? AND installation_id = ? AND name = ? \
     AND latest_definition IS NOT NULL AND latest_hash IS NOT NULL RETURNING ",
    columns!()
);
const GONE: &str = "UPDATE installation_tools SET gone_at = ? \
                    WHERE organization_id = ? AND workspace_id = ? AND installation_id = ? AND name = ?";

type Query<'q> = sqlx::query::Query<'q, Sqlite, SqliteArguments>;

#[derive(Clone, Copy)]
struct Owner {
    organization_id: OrganizationId,
    workspace_id: WorkspaceId,
    installation_id: InstallationId,
}

/// Binds the scope: organization, workspace, installation.
fn bind_scope(query: Query<'_>, owner: Owner) -> Query<'_> {
    query
        .bind(owner.organization_id.uuid())
        .bind(owner.workspace_id.uuid())
        .bind(owner.installation_id.uuid())
}

fn effect_str(effect: ToolEffect) -> &'static str {
    match effect {
        ToolEffect::Read => "read",
        ToolEffect::Write => "write",
        ToolEffect::Destructive => "destructive",
    }
}

fn effect_from(value: &str) -> Result<ToolEffect, ToolError> {
    match value {
        "read" => Ok(ToolEffect::Read),
        "write" => Ok(ToolEffect::Write),
        "destructive" => Ok(ToolEffect::Destructive),
        _ => Err(read("an unknown tool effect")),
    }
}

fn hash_from(bytes: Vec<u8>) -> Result<[u8; 32], ToolError> {
    bytes.try_into().map_err(|_| read("a tool hash of the wrong length"))
}

fn tool_from(row: &SqliteRow) -> Result<RegisteredTool, ToolError> {
    let latest_definition: Option<String> = row.try_get("latest_definition").map_err(read)?;
    let latest_hash: Option<Vec<u8>> = row.try_get("latest_hash").map_err(read)?;
    let latest = match (latest_definition, latest_hash) {
        (Some(definition), Some(hash)) => Some(ToolRevision {
            definition,
            hash: hash_from(hash)?,
        }),
        (None, None) => None,
        _ => return Err(read("half a latest tool revision")),
    };
    let instant = |column: &str| -> Result<Option<Timestamp>, ToolError> {
        let millis: Option<i64> = row.try_get(column).map_err(read)?;
        Ok(millis.map(Timestamp::from_millis))
    };
    Ok(RegisteredTool {
        name: row.try_get("name").map_err(read)?,
        effect: effect_from(row.try_get("effect").map_err(read)?)?,
        pinned: ToolRevision {
            definition: row.try_get("pinned_definition").map_err(read)?,
            hash: hash_from(row.try_get("pinned_hash").map_err(read)?)?,
        },
        latest,
        first_seen_at: Timestamp::from_millis(row.try_get("first_seen_at").map_err(read)?),
        seen_at: Timestamp::from_millis(row.try_get("seen_at").map_err(read)?),
        changed_at: instant("changed_at")?,
        gone_at: instant("gone_at")?,
    })
}

async fn installation_exists(transaction: &mut Transaction<'_, Sqlite>, owner: Owner) -> Result<bool, ToolError> {
    let found = bind_scope(sqlx::query(INSTALLATION_EXISTS), owner)
        .fetch_optional(&mut **transaction)
        .await
        .map_err(failed("installation_exists"))?;
    Ok(found.is_some())
}

#[async_trait]
impl ToolRegistry for SqliteToolRegistry {
    async fn record_listing(
        &self,
        organization_id: OrganizationId,
        workspace_id: WorkspaceId,
        installation_id: InstallationId,
        listed: &[ListedTool],
        now: Timestamp,
    ) -> Result<ListingChanges, ToolError> {
        validate_listing(listed)?;
        let owner = Owner {
            organization_id,
            workspace_id,
            installation_id,
        };
        let mut transaction = self
            .database
            .pool()
            .begin_with("BEGIN IMMEDIATE")
            .await
            .map_err(failed("record_listing"))?;
        if !installation_exists(&mut transaction, owner).await? {
            return Err(ToolError::NotFound);
        }
        let rows = bind_scope(sqlx::query(SELECT), owner)
            .fetch_all(&mut *transaction)
            .await
            .map_err(failed("record_listing"))?;
        let known: Vec<RegisteredTool> = rows.iter().map(tool_from).collect::<Result<_, _>>()?;

        let mut changes = ListingChanges::default();
        for tool in listed {
            let Some(existing) = known.iter().find(|known| known.name == tool.name) else {
                bind_scope(sqlx::query(INSERT), owner)
                    .bind(Uuid::now_v7())
                    .bind(&tool.name)
                    .bind(effect_str(tool.effect))
                    .bind(&tool.revision.definition)
                    .bind(tool.revision.hash.as_slice())
                    .bind(now.millis())
                    .bind(now.millis())
                    .execute(&mut *transaction)
                    .await
                    .map_err(failed("record_listing"))?;
                changes.added.push(tool.name.clone());
                continue;
            };
            if existing.gone_at.is_some() {
                changes.returned.push(tool.name.clone());
            }
            let query = if tool.revision.hash == existing.pinned.hash {
                sqlx::query(CURRENT).bind(now.millis())
            } else if existing.latest.as_ref().map(|latest| latest.hash) == Some(tool.revision.hash) {
                sqlx::query(SEEN).bind(now.millis())
            } else {
                changes.changed.push(tool.name.clone());
                sqlx::query(CHANGED)
                    .bind(&tool.revision.definition)
                    .bind(tool.revision.hash.as_slice())
                    .bind(now.millis())
                    .bind(now.millis())
            };
            bind_scope(query, owner)
                .bind(&tool.name)
                .execute(&mut *transaction)
                .await
                .map_err(failed("record_listing"))?;
        }
        for existing in &known {
            if existing.gone_at.is_none() && !listed.iter().any(|tool| tool.name == existing.name) {
                bind_scope(sqlx::query(GONE).bind(now.millis()), owner)
                    .bind(&existing.name)
                    .execute(&mut *transaction)
                    .await
                    .map_err(failed("record_listing"))?;
                changes.gone.push(existing.name.clone());
            }
        }
        let keep = i64::try_from(MAX_GONE).map_err(|_| ToolError::Unavailable)?;
        bind_scope(bind_scope(sqlx::query(PRUNE_GONE), owner), owner)
            .bind(keep)
            .execute(&mut *transaction)
            .await
            .map_err(failed("record_listing"))?;
        transaction.commit().await.map_err(failed("record_listing"))?;
        for names in [
            &mut changes.added,
            &mut changes.changed,
            &mut changes.gone,
            &mut changes.returned,
        ] {
            names.sort();
        }
        Ok(changes)
    }

    async fn list_tools(
        &self,
        organization_id: OrganizationId,
        workspace_id: WorkspaceId,
        installation_id: InstallationId,
        after: Option<&str>,
        limit: u32,
    ) -> Result<ToolPage, ToolError> {
        let owner = Owner {
            organization_id,
            workspace_id,
            installation_id,
        };
        let limit = limit.clamp(1, MAX_PAGE);
        let mut transaction = self.database.pool().begin().await.map_err(failed("list_tools"))?;
        if !installation_exists(&mut transaction, owner).await? {
            return Err(ToolError::NotFound);
        }
        let total: i64 = bind_scope(sqlx::query(COUNT), owner)
            .fetch_one(&mut *transaction)
            .await
            .map_err(failed("list_tools"))?
            .try_get(0)
            .map_err(read)?;
        let rows = bind_scope(sqlx::query(PAGE), owner)
            .bind(after)
            .bind(i64::from(limit) + 1)
            .fetch_all(&mut *transaction)
            .await
            .map_err(failed("list_tools"))?;
        let mut items = rows.iter().map(tool_from).collect::<Result<Vec<_>, _>>()?;
        let next_after = if items.len() > limit as usize {
            items.truncate(limit as usize);
            items.last().map(|last| last.name.clone())
        } else {
            None
        };
        Ok(ToolPage {
            items,
            next_after,
            total: u64::try_from(total).map_err(read)?,
        })
    }

    async fn accept_change(
        &self,
        organization_id: OrganizationId,
        workspace_id: WorkspaceId,
        installation_id: InstallationId,
        name: &str,
        expected: Option<[u8; 32]>,
    ) -> Result<RegisteredTool, ToolError> {
        let owner = Owner {
            organization_id,
            workspace_id,
            installation_id,
        };
        let mut transaction = self
            .database
            .pool()
            .begin_with("BEGIN IMMEDIATE")
            .await
            .map_err(failed("accept_change"))?;
        if !installation_exists(&mut transaction, owner).await? {
            return Err(ToolError::NotFound);
        }
        let row = bind_scope(sqlx::query(FIND), owner)
            .bind(name)
            .fetch_optional(&mut *transaction)
            .await
            .map_err(failed("accept_change"))?;
        let tool = tool_from(&row.ok_or(ToolError::ToolNotFound)?)?;
        if tool.status() != ToolStatus::Changed {
            return Err(ToolError::NotChanged);
        }
        if expected.is_none() || tool.latest.as_ref().map(|latest| latest.hash) != expected {
            return Err(ToolError::Stale);
        }
        let row = bind_scope(sqlx::query(ACCEPT), owner)
            .bind(name)
            .fetch_one(&mut *transaction)
            .await
            .map_err(failed("accept_change"))?;
        let accepted = tool_from(&row)?;
        transaction.commit().await.map_err(failed("accept_change"))?;
        Ok(accepted)
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use std::sync::Arc;

    use zupflow_conformance::tool_registry::{Prepare, Tenant};

    use super::*;
    use crate::schema::{Temporary, temporary};

    /// Makes the tenant's rows straight in the database: the registry under
    /// test only reads them.
    fn prepare(database: Database) -> Prepare {
        Arc::new(move |tenant: Tenant| {
            let database = database.clone();
            Box::pin(async move {
                let pool = database.pool();
                let org = tenant.organization_id.uuid();
                let app = Uuid::now_v7();
                sqlx::query("INSERT INTO organizations (id, name, slug, created_at) VALUES (?, 'Test', ?, 0)")
                    .bind(org)
                    .bind(format!("org-{}", org.simple()))
                    .execute(pool)
                    .await
                    .unwrap();
                sqlx::query(
                    "INSERT INTO apps (id, organization_id, kind, name, slug, description, icon_glyph, icon_color, \
                     auth, tools, version, created_at, updated_at) \
                     VALUES (?, ?, 'mcp', 'Catalog', 'catalog', '', 'C', '#0E7490', '{\"type\":\"token\"}', '[]', 1, 0, 0)",
                )
                .bind(app)
                .bind(org)
                .execute(pool)
                .await
                .unwrap();
                for (workspace, installation) in [
                    (tenant.workspace, tenant.installation),
                    (tenant.sibling, tenant.sibling_installation),
                ] {
                    sqlx::query(
                        "INSERT INTO workspaces (id, organization_id, environment, name, slug, created_at) \
                         VALUES (?, ?, 'sandbox', 'W', ?, 0)",
                    )
                    .bind(workspace.uuid())
                    .bind(org)
                    .bind(format!("w-{}", workspace.uuid().simple()))
                    .execute(pool)
                    .await
                    .unwrap();
                    sqlx::query(
                        "INSERT INTO installations (id, organization_id, workspace_id, catalog_app_id, label, status, \
                         configuration_version, granted_scopes, created_at, updated_at) \
                         VALUES (?, ?, ?, ?, 'Main', 'disconnected', 0, '[]', 0, 0)",
                    )
                    .bind(installation.uuid())
                    .bind(org)
                    .bind(workspace.uuid())
                    .bind(app)
                    .execute(pool)
                    .await
                    .unwrap();
                }
            })
        })
    }

    fn tenant() -> Tenant {
        Tenant {
            organization_id: OrganizationId::generate(),
            workspace: WorkspaceId::generate(),
            installation: InstallationId::generate(),
            sibling: WorkspaceId::generate(),
            sibling_installation: InstallationId::generate(),
        }
    }

    #[tokio::test]
    async fn conforms() {
        let Temporary {
            database,
            directory: _directory,
        } = temporary().await;
        zupflow_conformance::tool_registry::run(&SqliteToolRegistry::new(database.clone()), prepare(database)).await;
    }

    /// An installation's tools go with it, and so do a workspace's, while a
    /// sibling's stay.
    #[tokio::test]
    async fn deletes_tools_with_their_installation_or_workspace() {
        use zupflow_core::apps::ToolEffect;
        use zupflow_core::workspaces::WorkspaceStore;

        let Temporary {
            database,
            directory: _directory,
        } = temporary().await;
        let tenant = tenant();
        prepare(database.clone())(tenant).await;
        let registry = SqliteToolRegistry::new(database.clone());
        let tool = ListedTool {
            name: "search".to_owned(),
            revision: ToolRevision {
                definition: "{}".to_owned(),
                hash: [1; 32],
            },
            effect: ToolEffect::Read,
        };
        let org = tenant.organization_id;
        for (workspace, installation) in [
            (tenant.workspace, tenant.installation),
            (tenant.sibling, tenant.sibling_installation),
        ] {
            registry
                .record_listing(
                    org,
                    workspace,
                    installation,
                    std::slice::from_ref(&tool),
                    Timestamp::now(),
                )
                .await
                .unwrap();
        }
        let count = || async {
            sqlx::query_scalar::<_, i64>("SELECT count(*) FROM installation_tools")
                .fetch_one(database.pool())
                .await
                .unwrap()
        };
        sqlx::query("DELETE FROM installations WHERE organization_id = ? AND workspace_id = ? AND id = ?")
            .bind(org.uuid())
            .bind(tenant.workspace.uuid())
            .bind(tenant.installation.uuid())
            .execute(database.pool())
            .await
            .unwrap();
        assert_eq!(count().await, 1);
        crate::SqliteWorkspaceStore::new(database.clone())
            .delete_workspace(org, tenant.sibling)
            .await
            .unwrap();
        assert_eq!(count().await, 0);
    }
}
