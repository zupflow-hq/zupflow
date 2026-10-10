//! [`SqliteToolPolicyStore`]: each organization's tool policy in
//! `tool_policies`, its levels per catalog app and tool name in
//! `app_tool_levels`. Every statement repeats the organization, and a
//! level's the app beside it; a level references its app with the
//! organization, so only the organization's own catalog apps have levels.

use async_trait::async_trait;
use sqlx::sqlite::SqliteRow;
use sqlx::{Row, Sqlite, Transaction};
use uuid::Uuid;
use zupflow_core::apps::ToolEffect;
use zupflow_core::ids::{AppId, OrganizationId, UserId};
use zupflow_core::organizations::User;
use zupflow_core::policy::{
    AppToolLevel, ApprovalMode, LevelPage, MAX_PAGE, NewToolLevel, PolicyError, PolicySettings, ToolLevel, ToolPolicy,
    ToolPolicyStore, validate_levels,
};
use zupflow_core::time::Timestamp;

use crate::Database;

pub struct SqliteToolPolicyStore {
    database: Database,
}

impl SqliteToolPolicyStore {
    pub fn new(database: Database) -> Self {
        Self { database }
    }
}

fn failed(operation: &'static str) -> impl FnOnce(sqlx::Error) -> PolicyError {
    move |error| {
        tracing::error!(event = "tool_policy.database_error", operation, %error);
        PolicyError::Unavailable
    }
}

fn read(error: impl std::fmt::Display) -> PolicyError {
    tracing::error!(event = "tool_policy.unreadable_row", %error);
    PolicyError::Unavailable
}

/// The writer's columns, from `users u`.
macro_rules! user_columns {
    () => {
        "u.id AS user_id, u.name AS user_name, u.email AS user_email, u.avatar_url AS user_avatar_url"
    };
}

/// A level's columns, from `app_tool_levels l` joined to its writer, with
/// the scope bound as `?1` (organization) and `?2` (app).
macro_rules! level_select {
    () => {
        concat!(
            "SELECT l.app_id, l.name, l.level, l.confirmed, l.destructive, l.updated_at, ",
            user_columns!(),
            " FROM app_tool_levels l LEFT JOIN users u ON u.id = l.updated_by_user_id \
             WHERE l.organization_id = ?1 AND l.app_id = ?2"
        )
    };
}

const LOAD_POLICY: &str = concat!(
    "SELECT p.mode, p.new_tool_level, p.updated_at, ",
    user_columns!(),
    " FROM tool_policies p LEFT JOIN users u ON u.id = p.updated_by_user_id WHERE p.organization_id = ?"
);
const SAVE_POLICY: &str = "INSERT INTO tool_policies (id, organization_id, mode, new_tool_level, updated_at, \
                           updated_by_user_id) VALUES (?, ?, ?, ?, ?, ?) \
                           ON CONFLICT (organization_id) DO UPDATE SET mode = excluded.mode, \
                           new_tool_level = excluded.new_tool_level, updated_at = excluded.updated_at, \
                           updated_by_user_id = excluded.updated_by_user_id";
/// A catalog app of the organization: the organization, then the app.
const APP_EXISTS: &str = "SELECT 1 FROM apps WHERE organization_id = ? AND id = ?";
/// One page: the scope, the name to start after (or NULL), the limit.
const PAGE: &str = concat!(
    level_select!(),
    " AND (?3 IS NULL OR l.name > ?3) ORDER BY l.name LIMIT ?4"
);
const COUNT: &str = "SELECT count(*) FROM app_tool_levels WHERE organization_id = ? AND app_id = ?";
/// The levels named: the scope, then the names as a JSON array.
const FIND: &str = concat!(
    level_select!(),
    " AND l.name IN (SELECT value FROM json_each(?3)) ORDER BY l.name"
);
/// Sets a level confirmed; a suggested level's hint stays.
const SET: &str = "INSERT INTO app_tool_levels (id, organization_id, app_id, name, level, confirmed, destructive, \
                   updated_at, updated_by_user_id) VALUES (?, ?, ?, ?, ?, 1, 0, ?, ?) \
                   ON CONFLICT (organization_id, app_id, name) DO UPDATE SET level = excluded.level, confirmed = 1, \
                   updated_at = excluded.updated_at, updated_by_user_id = excluded.updated_by_user_id";
/// Suggests a level where there's none.
const SUGGEST: &str = "INSERT INTO app_tool_levels (id, organization_id, app_id, name, level, confirmed, destructive, \
                       updated_at, updated_by_user_id) VALUES (?, ?, ?, ?, ?, 0, ?, ?, NULL) \
                       ON CONFLICT (organization_id, app_id, name) DO NOTHING";

fn user_from(row: &SqliteRow) -> Result<Option<User>, PolicyError> {
    let id: Option<Uuid> = row.try_get("user_id").map_err(read)?;
    let Some(id) = id else {
        return Ok(None);
    };
    Ok(Some(User {
        id: UserId::from_uuid(id),
        name: row.try_get("user_name").map_err(read)?,
        email: row.try_get("user_email").map_err(read)?,
        avatar_url: row.try_get("user_avatar_url").map_err(read)?,
    }))
}

fn level_from(row: &SqliteRow) -> Result<AppToolLevel, PolicyError> {
    let level: String = row.try_get("level").map_err(read)?;
    Ok(AppToolLevel {
        app_id: AppId::from_uuid(row.try_get("app_id").map_err(read)?),
        tool: row.try_get("name").map_err(read)?,
        level: ToolLevel::parse(&level).ok_or_else(|| read("an unknown tool level"))?,
        confirmed: row.try_get("confirmed").map_err(read)?,
        destructive: row.try_get("destructive").map_err(read)?,
        updated_at: Timestamp::from_millis(row.try_get("updated_at").map_err(read)?),
        updated_by: user_from(row)?,
    })
}

async fn app_exists(
    transaction: &mut Transaction<'_, Sqlite>,
    organization_id: OrganizationId,
    app_id: AppId,
) -> Result<(), PolicyError> {
    let found = sqlx::query(APP_EXISTS)
        .bind(organization_id.uuid())
        .bind(app_id.uuid())
        .fetch_optional(&mut **transaction)
        .await
        .map_err(failed("app_exists"))?;
    found.map(|_| ()).ok_or(PolicyError::AppNotFound)
}

async fn find(
    transaction: &mut Transaction<'_, Sqlite>,
    organization_id: OrganizationId,
    app_id: AppId,
    names: &[String],
) -> Result<Vec<AppToolLevel>, PolicyError> {
    let names = serde_json::to_string(names).map_err(read)?;
    let rows = sqlx::query(FIND)
        .bind(organization_id.uuid())
        .bind(app_id.uuid())
        .bind(names)
        .fetch_all(&mut **transaction)
        .await
        .map_err(failed("find_levels"))?;
    rows.iter().map(level_from).collect()
}

async fn load(
    transaction: &mut Transaction<'_, Sqlite>,
    organization_id: OrganizationId,
) -> Result<ToolPolicy, PolicyError> {
    let row = sqlx::query(LOAD_POLICY)
        .bind(organization_id.uuid())
        .fetch_optional(&mut **transaction)
        .await
        .map_err(failed("load_policy"))?;
    let Some(row) = row else {
        return Ok(ToolPolicy {
            organization_id,
            settings: PolicySettings::default(),
            updated_at: None,
            updated_by: None,
        });
    };
    let mode: String = row.try_get("mode").map_err(read)?;
    let new_tool_level: String = row.try_get("new_tool_level").map_err(read)?;
    Ok(ToolPolicy {
        organization_id,
        settings: PolicySettings {
            mode: ApprovalMode::parse(&mode).ok_or_else(|| read("an unknown approval mode"))?,
            new_tool_level: NewToolLevel::parse(&new_tool_level)
                .ok_or_else(|| read("an unknown level for new tools"))?,
        },
        updated_at: Some(Timestamp::from_millis(row.try_get("updated_at").map_err(read)?)),
        updated_by: user_from(&row)?,
    })
}

#[async_trait]
impl ToolPolicyStore for SqliteToolPolicyStore {
    async fn load_policy(&self, organization_id: OrganizationId) -> Result<ToolPolicy, PolicyError> {
        let mut transaction = self.database.pool().begin().await.map_err(failed("load_policy"))?;
        let policy = load(&mut transaction, organization_id).await?;
        transaction.commit().await.map_err(failed("load_policy"))?;
        Ok(policy)
    }

    async fn save_policy(
        &self,
        organization_id: OrganizationId,
        settings: PolicySettings,
        updated_by: Option<UserId>,
        now: Timestamp,
    ) -> Result<ToolPolicy, PolicyError> {
        let mut transaction = self
            .database
            .pool()
            .begin_with("BEGIN IMMEDIATE")
            .await
            .map_err(failed("save_policy"))?;
        sqlx::query(SAVE_POLICY)
            .bind(Uuid::now_v7())
            .bind(organization_id.uuid())
            .bind(settings.mode.as_str())
            .bind(settings.new_tool_level.as_str())
            .bind(now.millis())
            .bind(updated_by.map(|user| user.uuid()))
            .execute(&mut *transaction)
            .await
            .map_err(failed("save_policy"))?;
        let policy = load(&mut transaction, organization_id).await?;
        transaction.commit().await.map_err(failed("save_policy"))?;
        Ok(policy)
    }

    async fn list_levels(
        &self,
        organization_id: OrganizationId,
        app_id: AppId,
        after: Option<&str>,
        limit: u32,
    ) -> Result<LevelPage, PolicyError> {
        let limit = limit.clamp(1, MAX_PAGE);
        let mut transaction = self.database.pool().begin().await.map_err(failed("list_levels"))?;
        app_exists(&mut transaction, organization_id, app_id).await?;
        let total: i64 = sqlx::query(COUNT)
            .bind(organization_id.uuid())
            .bind(app_id.uuid())
            .fetch_one(&mut *transaction)
            .await
            .map_err(failed("list_levels"))?
            .try_get(0)
            .map_err(read)?;
        let rows = sqlx::query(PAGE)
            .bind(organization_id.uuid())
            .bind(app_id.uuid())
            .bind(after)
            .bind(i64::from(limit) + 1)
            .fetch_all(&mut *transaction)
            .await
            .map_err(failed("list_levels"))?;
        transaction.commit().await.map_err(failed("list_levels"))?;
        let mut items = rows.iter().map(level_from).collect::<Result<Vec<_>, _>>()?;
        let next_after = if items.len() > limit as usize {
            items.truncate(limit as usize);
            items.last().map(|last| last.tool.clone())
        } else {
            None
        };
        Ok(LevelPage {
            items,
            next_after,
            total: u64::try_from(total).map_err(read)?,
        })
    }

    async fn find_levels(
        &self,
        organization_id: OrganizationId,
        app_id: AppId,
        tools: &[String],
    ) -> Result<Vec<AppToolLevel>, PolicyError> {
        let mut transaction = self.database.pool().begin().await.map_err(failed("find_levels"))?;
        app_exists(&mut transaction, organization_id, app_id).await?;
        let levels = find(&mut transaction, organization_id, app_id, tools).await?;
        transaction.commit().await.map_err(failed("find_levels"))?;
        Ok(levels)
    }

    async fn set_levels(
        &self,
        organization_id: OrganizationId,
        app_id: AppId,
        levels: &[(String, ToolLevel)],
        updated_by: Option<UserId>,
        now: Timestamp,
    ) -> Result<Vec<AppToolLevel>, PolicyError> {
        validate_levels(levels)?;
        let mut transaction = self
            .database
            .pool()
            .begin_with("BEGIN IMMEDIATE")
            .await
            .map_err(failed("set_levels"))?;
        app_exists(&mut transaction, organization_id, app_id).await?;
        for (tool, level) in levels {
            sqlx::query(SET)
                .bind(Uuid::now_v7())
                .bind(organization_id.uuid())
                .bind(app_id.uuid())
                .bind(tool)
                .bind(level.as_str())
                .bind(now.millis())
                .bind(updated_by.map(|user| user.uuid()))
                .execute(&mut *transaction)
                .await
                .map_err(failed("set_levels"))?;
        }
        let names: Vec<String> = levels.iter().map(|(tool, _)| tool.clone()).collect();
        let written = find(&mut transaction, organization_id, app_id, &names).await?;
        transaction.commit().await.map_err(failed("set_levels"))?;
        Ok(written)
    }

    async fn suggest_levels(
        &self,
        organization_id: OrganizationId,
        app_id: AppId,
        listed: &[(String, ToolEffect)],
        now: Timestamp,
    ) -> Result<u64, PolicyError> {
        let mut transaction = self
            .database
            .pool()
            .begin_with("BEGIN IMMEDIATE")
            .await
            .map_err(failed("suggest_levels"))?;
        app_exists(&mut transaction, organization_id, app_id).await?;
        let mut suggested = 0;
        for (tool, effect) in listed {
            suggested += sqlx::query(SUGGEST)
                .bind(Uuid::now_v7())
                .bind(organization_id.uuid())
                .bind(app_id.uuid())
                .bind(tool)
                .bind(ToolLevel::suggested(*effect).as_str())
                .bind(*effect == ToolEffect::Destructive)
                .bind(now.millis())
                .execute(&mut *transaction)
                .await
                .map_err(failed("suggest_levels"))?
                .rows_affected();
        }
        transaction.commit().await.map_err(failed("suggest_levels"))?;
        Ok(suggested)
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use std::sync::Arc;

    use zupflow_conformance::tool_policy_store::{Prepare, Tenant};

    use super::*;
    use crate::schema::{Temporary, temporary};

    /// Makes the tenant's rows straight in the database: the store under
    /// test only reads them.
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
                sqlx::query("INSERT INTO users (id, name, email, created_at) VALUES (?, 'Ada', ?, 0)")
                    .bind(tenant.user.uuid())
                    .bind(format!("{}@acme.example", tenant.user.uuid().simple()))
                    .execute(pool)
                    .await
                    .unwrap();
                for (app, slug) in [(tenant.catalog_app, "catalog"), (tenant.second_catalog_app, "second")] {
                    sqlx::query(
                        "INSERT INTO apps (id, organization_id, kind, name, slug, description, icon_glyph, \
                         icon_color, auth, tools, version, created_at, updated_at) \
                         VALUES (?, ?, 'mcp', 'Catalog', ?, '', 'C', '#0E7490', '{\"type\":\"token\"}', '[]', 1, 0, 0)",
                    )
                    .bind(app.uuid())
                    .bind(org)
                    .bind(slug)
                    .execute(pool)
                    .await
                    .unwrap();
                }
                sqlx::query(
                    "INSERT INTO workspaces (id, organization_id, environment, name, slug, created_at) \
                     VALUES (?, ?, 'sandbox', 'W', ?, 0)",
                )
                .bind(tenant.workspace.uuid())
                .bind(org)
                .bind(format!("w-{}", tenant.workspace.uuid().simple()))
                .execute(pool)
                .await
                .unwrap();
                sqlx::query(
                    "INSERT INTO workspace_apps (id, organization_id, workspace_id, kind, name, slug, description, \
                     icon_glyph, icon_color, auth, tools, version, created_at, updated_at) \
                     VALUES (?, ?, ?, 'mcp', 'Own', 'own', '', 'O', '#0E7490', '{\"type\":\"token\"}', '[]', 1, 0, 0)",
                )
                .bind(tenant.workspace_app.uuid())
                .bind(org)
                .bind(tenant.workspace.uuid())
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
        zupflow_conformance::tool_policy_store::run(&SqliteToolPolicyStore::new(database.clone()), prepare(database))
            .await;
    }

    /// The policy and levels go with their organization, and levels with their app.
    #[tokio::test]
    async fn deletes_with_the_organization_or_app() {
        let Temporary {
            database,
            directory: _directory,
        } = temporary().await;
        let tenant = Tenant {
            organization_id: OrganizationId::generate(),
            user: UserId::generate(),
            catalog_app: AppId::generate(),
            second_catalog_app: AppId::generate(),
            workspace: zupflow_core::ids::WorkspaceId::generate(),
            workspace_app: AppId::generate(),
        };
        prepare(database.clone())(tenant).await;
        let store = SqliteToolPolicyStore::new(database.clone());
        let org = tenant.organization_id;
        let now = Timestamp::now();
        store
            .save_policy(org, PolicySettings::default(), Some(tenant.user), now)
            .await
            .unwrap();
        for app in [tenant.catalog_app, tenant.second_catalog_app] {
            store
                .set_levels(
                    org,
                    app,
                    &[("search".to_owned(), ToolLevel::Allow)],
                    Some(tenant.user),
                    now,
                )
                .await
                .unwrap();
        }
        let count = |query: &'static str| {
            let database = database.clone();
            async move {
                sqlx::query_scalar::<_, i64>(query)
                    .fetch_one(database.pool())
                    .await
                    .unwrap()
            }
        };
        const LEVELS: &str = "SELECT count(*) FROM app_tool_levels";
        const POLICIES: &str = "SELECT count(*) FROM tool_policies";
        // A writer who's gone leaves the level standing, unattributed.
        sqlx::query("DELETE FROM users WHERE id = ?")
            .bind(tenant.user.uuid())
            .execute(database.pool())
            .await
            .unwrap();
        let level = store
            .find_levels(org, tenant.catalog_app, &["search".to_owned()])
            .await
            .unwrap();
        assert_eq!((level[0].confirmed, level[0].updated_by.clone()), (true, None));
        assert_eq!(store.load_policy(org).await.unwrap().updated_by, None);

        sqlx::query("DELETE FROM apps WHERE organization_id = ? AND id = ?")
            .bind(org.uuid())
            .bind(tenant.catalog_app.uuid())
            .execute(database.pool())
            .await
            .unwrap();
        assert_eq!(count(LEVELS).await, 1);
        sqlx::query("DELETE FROM organizations WHERE id = ?")
            .bind(org.uuid())
            .execute(database.pool())
            .await
            .unwrap();
        assert_eq!((count(LEVELS).await, count(POLICIES).await), (0, 0));
    }
}
