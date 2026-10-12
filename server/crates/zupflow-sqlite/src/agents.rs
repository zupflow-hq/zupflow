//! [`SqliteAgentStore`]: agents in `agents`, their grants in `agent_grants`.
//! Every statement repeats `organization_id` and `workspace_id` beside the
//! agent's, grant's or installation's ID.
//!
//! A grant is written in an immediate transaction that first reads, in the
//! same scope, the agent, the installation and the installation's registry
//! (`installation_tools`) and the organization's levels (`tool_policies`,
//! `app_tool_levels`), so neither a listing recorded meanwhile nor a level
//! tightened meanwhile can slip into a grant. Deleting an agent or an
//! installation deletes its grants through the cascading keys, in that one
//! statement.

use std::collections::HashMap;

use async_trait::async_trait;
use sqlx::sqlite::SqliteRow;
use sqlx::{Row, Sqlite, Transaction};
use zupflow_core::agents::{
    Agent, AgentChanges, AgentError, AgentStatus, AgentStore, Grant, GrantTools, NewAgent, approval_outside,
    levels_refusal,
};
use zupflow_core::audit::Audit;
use zupflow_core::ids::{AgentId, AppId, GrantId, InstallationId, OrganizationId, WorkspaceId};
use zupflow_core::policy::{AppToolLevel, GrantLevels, NewToolLevel, PolicySettings, ToolLevel};
use zupflow_core::time::Timestamp;

use crate::Database;

pub struct SqliteAgentStore {
    database: Database,
}

impl SqliteAgentStore {
    pub fn new(database: Database) -> Self {
        Self { database }
    }

    async fn immediate(&self, operation: &'static str) -> Result<Transaction<'static, Sqlite>, AgentError> {
        self.database
            .pool()
            .begin_with("BEGIN IMMEDIATE")
            .await
            .map_err(failed(operation))
    }
}

fn failed(operation: &'static str) -> impl FnOnce(sqlx::Error) -> AgentError {
    move |error| {
        tracing::error!(event = "agents.database_error", operation, %error);
        AgentError::Unavailable
    }
}

fn read(error: impl std::fmt::Display) -> AgentError {
    tracing::error!(event = "agents.unreadable_row", %error);
    AgentError::Unavailable
}

/// An agent's columns and its grant count, from `agents a`, with the scope
/// bound as `?1` (organization) and `?2` (workspace). A macro, so queries
/// stay static strings.
macro_rules! agent_select {
    () => {
        "SELECT a.id, a.organization_id, a.workspace_id, a.name, a.description, a.model, a.instructions, \
         a.status, a.created_at, a.updated_at, \
         (SELECT count(*) FROM agent_grants g \
          WHERE g.organization_id = ?1 AND g.workspace_id = ?2 AND g.agent_id = a.id) AS grant_count \
         FROM agents a WHERE a.organization_id = ?1 AND a.workspace_id = ?2"
    };
}

const LIST_AGENTS: &str = concat!(agent_select!(), " ORDER BY lower(a.name), a.name, a.id");
const FIND_AGENT: &str = concat!(agent_select!(), " AND a.id = ?3");
const AGENT_EXISTS: &str = "SELECT 1 FROM agents WHERE organization_id = ? AND workspace_id = ? AND id = ?";
const INSTALLATION_EXISTS: &str =
    "SELECT 1 FROM installations WHERE organization_id = ? AND workspace_id = ? AND id = ?";
/// The installation's registry: each tool's name and whether it's gone.
const REGISTRY: &str = "SELECT name, gone_at IS NOT NULL AS gone FROM installation_tools \
                        WHERE organization_id = ? AND workspace_id = ? AND installation_id = ?";

/// The installation's catalog app; NULL for a workspace app's.
const CATALOG_APP: &str =
    "SELECT catalog_app_id FROM installations WHERE organization_id = ? AND workspace_id = ? AND id = ?";
/// The organization's level for new tools, when it saved a policy.
const NEW_TOOL_LEVEL: &str = "SELECT new_tool_level FROM tool_policies WHERE organization_id = ?";
/// The app's levels for the tools named: the organization, the app, then the names as a JSON array.
const LEVELS: &str = "SELECT name, level, confirmed FROM app_tool_levels \
                      WHERE organization_id = ? AND app_id = ? AND name IN (SELECT value FROM json_each(?))";

macro_rules! grant_columns {
    () => {
        "id, organization_id, workspace_id, agent_id, installation_id, tools, approval_required, created_at, \
         updated_at"
    };
}

const LIST_GRANTS: &str = concat!(
    "SELECT ",
    grant_columns!(),
    " FROM agent_grants WHERE organization_id = ? AND workspace_id = ? AND agent_id = ? ORDER BY created_at, id"
);
const FIND_GRANT: &str = concat!(
    "SELECT ",
    grant_columns!(),
    " FROM agent_grants WHERE organization_id = ? AND workspace_id = ? AND agent_id = ? AND id = ?"
);
const GRANT_EXISTS: &str = "SELECT 1 FROM agent_grants \
                            WHERE organization_id = ? AND workspace_id = ? AND agent_id = ? AND installation_id = ?";
const INSERT_GRANT: &str = concat!(
    "INSERT INTO agent_grants (id, organization_id, workspace_id, agent_id, installation_id, tools, \
     approval_required, created_at, updated_at) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?) RETURNING ",
    grant_columns!()
);
const UPDATE_GRANT: &str = concat!(
    "UPDATE agent_grants SET tools = ?, approval_required = ?, updated_at = ? \
     WHERE organization_id = ? AND workspace_id = ? AND agent_id = ? AND id = ? RETURNING ",
    grant_columns!()
);

fn agent_from(row: &SqliteRow) -> Result<Agent, AgentError> {
    let status: String = row.try_get("status").map_err(read)?;
    let grant_count: i64 = row.try_get("grant_count").map_err(read)?;
    Ok(Agent {
        id: AgentId::from_uuid(row.try_get("id").map_err(read)?),
        organization_id: OrganizationId::from_uuid(row.try_get("organization_id").map_err(read)?),
        workspace_id: WorkspaceId::from_uuid(row.try_get("workspace_id").map_err(read)?),
        name: row.try_get("name").map_err(read)?,
        description: row.try_get("description").map_err(read)?,
        model: row.try_get("model").map_err(read)?,
        instructions: row.try_get("instructions").map_err(read)?,
        status: AgentStatus::parse(&status).ok_or_else(|| read("an unknown agent status"))?,
        grant_count: u64::try_from(grant_count).map_err(read)?,
        created_at: Timestamp::from_millis(row.try_get("created_at").map_err(read)?),
        updated_at: Timestamp::from_millis(row.try_get("updated_at").map_err(read)?),
    })
}

fn grant_from(row: &SqliteRow) -> Result<Grant, AgentError> {
    let names = |column: &str| -> Result<Vec<String>, AgentError> {
        let json: String = row.try_get(column).map_err(read)?;
        serde_json::from_str(&json).map_err(read)
    };
    Ok(Grant {
        id: GrantId::from_uuid(row.try_get("id").map_err(read)?),
        organization_id: OrganizationId::from_uuid(row.try_get("organization_id").map_err(read)?),
        workspace_id: WorkspaceId::from_uuid(row.try_get("workspace_id").map_err(read)?),
        agent_id: AgentId::from_uuid(row.try_get("agent_id").map_err(read)?),
        installation_id: InstallationId::from_uuid(row.try_get("installation_id").map_err(read)?),
        tools: names("tools")?,
        approval_required: names("approval_required")?,
        created_at: Timestamp::from_millis(row.try_get("created_at").map_err(read)?),
        updated_at: Timestamp::from_millis(row.try_get("updated_at").map_err(read)?),
    })
}

async fn agent_exists(
    transaction: &mut Transaction<'static, Sqlite>,
    organization_id: OrganizationId,
    workspace_id: WorkspaceId,
    agent_id: AgentId,
) -> Result<bool, AgentError> {
    let found = sqlx::query(AGENT_EXISTS)
        .bind(organization_id.uuid())
        .bind(workspace_id.uuid())
        .bind(agent_id.uuid())
        .fetch_optional(&mut **transaction)
        .await
        .map_err(failed("agent_exists"))?;
    Ok(found.is_some())
}

/// Holds `tools` to the installation's registry, inside the caller's
/// transaction: each tool must be there and not gone, checked in the order
/// given; then every approval must be granted; then the grant must hold
/// inside the organization's levels in force, read here too.
async fn check_tools(
    transaction: &mut Transaction<'static, Sqlite>,
    organization_id: OrganizationId,
    workspace_id: WorkspaceId,
    installation_id: InstallationId,
    tools: &GrantTools,
) -> Result<(), AgentError> {
    let rows = sqlx::query(REGISTRY)
        .bind(organization_id.uuid())
        .bind(workspace_id.uuid())
        .bind(installation_id.uuid())
        .fetch_all(&mut **transaction)
        .await
        .map_err(failed("check_tools"))?;
    let mut registry: HashMap<String, bool> = HashMap::with_capacity(rows.len());
    for row in &rows {
        registry.insert(row.try_get("name").map_err(read)?, row.try_get("gone").map_err(read)?);
    }
    for name in &tools.tools {
        match registry.get(name) {
            None => return Err(AgentError::UnknownTool(name.clone())),
            Some(true) => return Err(AgentError::ToolGone(name.clone())),
            Some(false) => {}
        }
    }
    if let Some(name) = approval_outside(tools) {
        return Err(AgentError::ApprovalOutsideGrant(name.to_owned()));
    }
    let levels = levels_in_force(transaction, organization_id, workspace_id, installation_id, tools).await?;
    match levels_refusal(tools, &levels) {
        Some(refusal) => Err(refusal),
        None => Ok(()),
    }
}

/// The organization's levels in force for the grant's tools, inside the
/// caller's transaction: those of the installation's catalog app, under the
/// policy's level for new tools; none for a workspace app's installation.
async fn levels_in_force(
    transaction: &mut Transaction<'static, Sqlite>,
    organization_id: OrganizationId,
    workspace_id: WorkspaceId,
    installation_id: InstallationId,
    tools: &GrantTools,
) -> Result<GrantLevels, AgentError> {
    let app: Option<uuid::Uuid> = sqlx::query_scalar(CATALOG_APP)
        .bind(organization_id.uuid())
        .bind(workspace_id.uuid())
        .bind(installation_id.uuid())
        .fetch_one(&mut **transaction)
        .await
        .map_err(failed("levels_in_force"))?;
    let Some(app) = app else {
        return Ok(GrantLevels::none());
    };
    let new_tool_level: Option<String> = sqlx::query_scalar(NEW_TOOL_LEVEL)
        .bind(organization_id.uuid())
        .fetch_optional(&mut **transaction)
        .await
        .map_err(failed("levels_in_force"))?;
    let new_tool_level = match new_tool_level {
        None => PolicySettings::default().new_tool_level,
        Some(level) => NewToolLevel::parse(&level).ok_or_else(|| read("an unknown level for new tools"))?,
    };
    let rows = sqlx::query(LEVELS)
        .bind(organization_id.uuid())
        .bind(app)
        .bind(names_json(&tools.tools)?)
        .fetch_all(&mut **transaction)
        .await
        .map_err(failed("levels_in_force"))?;
    let mut levels = Vec::with_capacity(rows.len());
    for row in &rows {
        let level: String = row.try_get("level").map_err(read)?;
        levels.push(AppToolLevel {
            app_id: AppId::from_uuid(app),
            tool: row.try_get("name").map_err(read)?,
            level: ToolLevel::parse(&level).ok_or_else(|| read("an unknown tool level"))?,
            confirmed: row.try_get("confirmed").map_err(read)?,
            destructive: false,
            updated_at: Timestamp::from_millis(0),
            updated_by: None,
        });
    }
    Ok(GrantLevels::in_force(&tools.tools, &levels, new_tool_level))
}

fn names_json(names: &[String]) -> Result<String, AgentError> {
    serde_json::to_string(names).map_err(read)
}

#[async_trait]
impl AgentStore for SqliteAgentStore {
    async fn list_agents(
        &self,
        organization_id: OrganizationId,
        workspace_id: WorkspaceId,
    ) -> Result<Vec<Agent>, AgentError> {
        let rows = sqlx::query(LIST_AGENTS)
            .bind(organization_id.uuid())
            .bind(workspace_id.uuid())
            .fetch_all(self.database.pool())
            .await
            .map_err(failed("list_agents"))?;
        rows.iter().map(agent_from).collect()
    }

    async fn find_agent(
        &self,
        organization_id: OrganizationId,
        workspace_id: WorkspaceId,
        agent_id: AgentId,
    ) -> Result<Option<Agent>, AgentError> {
        let row = sqlx::query(FIND_AGENT)
            .bind(organization_id.uuid())
            .bind(workspace_id.uuid())
            .bind(agent_id.uuid())
            .fetch_optional(self.database.pool())
            .await
            .map_err(failed("find_agent"))?;
        row.as_ref().map(agent_from).transpose()
    }

    async fn create_agent(
        &self,
        organization_id: OrganizationId,
        workspace_id: WorkspaceId,
        agent_id: AgentId,
        agent: &NewAgent,
        audit: &Audit<'_, Agent>,
    ) -> Result<Agent, AgentError> {
        let now = Timestamp::now().millis();
        let mut transaction = self.immediate("create_agent").await?;
        sqlx::query(
            "INSERT INTO agents (id, organization_id, workspace_id, name, description, model, instructions, status, \
             created_at, updated_at) VALUES (?, ?, ?, ?, ?, ?, ?, 'active', ?, ?)",
        )
        .bind(agent_id.uuid())
        .bind(organization_id.uuid())
        .bind(workspace_id.uuid())
        .bind(&agent.name)
        .bind(&agent.description)
        .bind(&agent.model)
        .bind(&agent.instructions)
        .bind(now)
        .bind(now)
        .execute(&mut *transaction)
        .await
        .map_err(failed("create_agent"))?;
        let row = sqlx::query(FIND_AGENT)
            .bind(organization_id.uuid())
            .bind(workspace_id.uuid())
            .bind(agent_id.uuid())
            .fetch_one(&mut *transaction)
            .await
            .map_err(failed("create_agent"))?;
        let created = agent_from(&row)?;
        crate::audit::record_in_workspace(&mut transaction, organization_id, workspace_id, &audit(&created))
            .await
            .map_err(failed("audit"))?;
        transaction.commit().await.map_err(failed("create_agent"))?;
        Ok(created)
    }

    async fn update_agent(
        &self,
        organization_id: OrganizationId,
        workspace_id: WorkspaceId,
        agent_id: AgentId,
        changes: &AgentChanges,
        audit: &Audit<'_, Agent>,
    ) -> Result<Agent, AgentError> {
        let mut transaction = self.immediate("update_agent").await?;
        let updated = sqlx::query(
            "UPDATE agents SET name = coalesce(?, name), description = coalesce(?, description), \
             model = coalesce(?, model), instructions = coalesce(?, instructions), status = coalesce(?, status), \
             updated_at = ? WHERE organization_id = ? AND workspace_id = ? AND id = ?",
        )
        .bind(changes.name.as_deref())
        .bind(changes.description.as_deref())
        .bind(changes.model.as_deref())
        .bind(changes.instructions.as_deref())
        .bind(changes.status.map(AgentStatus::as_str))
        .bind(Timestamp::now().millis())
        .bind(organization_id.uuid())
        .bind(workspace_id.uuid())
        .bind(agent_id.uuid())
        .execute(&mut *transaction)
        .await
        .map_err(failed("update_agent"))?;
        if updated.rows_affected() == 0 {
            return Err(AgentError::AgentNotFound);
        }
        let row = sqlx::query(FIND_AGENT)
            .bind(organization_id.uuid())
            .bind(workspace_id.uuid())
            .bind(agent_id.uuid())
            .fetch_one(&mut *transaction)
            .await
            .map_err(failed("update_agent"))?;
        let agent = agent_from(&row)?;
        crate::audit::record_in_workspace(&mut transaction, organization_id, workspace_id, &audit(&agent))
            .await
            .map_err(failed("audit"))?;
        transaction.commit().await.map_err(failed("update_agent"))?;
        Ok(agent)
    }

    async fn delete_agent(
        &self,
        organization_id: OrganizationId,
        workspace_id: WorkspaceId,
        agent_id: AgentId,
        audit: &Audit<'_, Agent>,
    ) -> Result<(), AgentError> {
        let mut transaction = self.immediate("delete_agent").await?;
        let row = sqlx::query(FIND_AGENT)
            .bind(organization_id.uuid())
            .bind(workspace_id.uuid())
            .bind(agent_id.uuid())
            .fetch_optional(&mut *transaction)
            .await
            .map_err(failed("delete_agent"))?;
        let agent = agent_from(&row.ok_or(AgentError::AgentNotFound)?)?;
        // Its grants reference it with a cascading key: they go in this one statement.
        sqlx::query("DELETE FROM agents WHERE organization_id = ? AND workspace_id = ? AND id = ?")
            .bind(organization_id.uuid())
            .bind(workspace_id.uuid())
            .bind(agent_id.uuid())
            .execute(&mut *transaction)
            .await
            .map_err(failed("delete_agent"))?;
        crate::audit::record_in_workspace(&mut transaction, organization_id, workspace_id, &audit(&agent))
            .await
            .map_err(failed("audit"))?;
        transaction.commit().await.map_err(failed("delete_agent"))?;
        Ok(())
    }

    async fn list_grants(
        &self,
        organization_id: OrganizationId,
        workspace_id: WorkspaceId,
        agent_id: AgentId,
    ) -> Result<Vec<Grant>, AgentError> {
        // One read transaction, so the agent and its grants are of one moment.
        let mut transaction = self.database.pool().begin().await.map_err(failed("list_grants"))?;
        if !agent_exists(&mut transaction, organization_id, workspace_id, agent_id).await? {
            return Err(AgentError::AgentNotFound);
        }
        let rows = sqlx::query(LIST_GRANTS)
            .bind(organization_id.uuid())
            .bind(workspace_id.uuid())
            .bind(agent_id.uuid())
            .fetch_all(&mut *transaction)
            .await
            .map_err(failed("list_grants"))?;
        transaction.commit().await.map_err(failed("list_grants"))?;
        rows.iter().map(grant_from).collect()
    }

    async fn create_grant(
        &self,
        organization_id: OrganizationId,
        workspace_id: WorkspaceId,
        agent_id: AgentId,
        grant_id: GrantId,
        installation_id: InstallationId,
        tools: &GrantTools,
        audit: &Audit<'_, Grant>,
    ) -> Result<Grant, AgentError> {
        let mut transaction = self.immediate("create_grant").await?;
        if !agent_exists(&mut transaction, organization_id, workspace_id, agent_id).await? {
            return Err(AgentError::AgentNotFound);
        }
        let installation = sqlx::query(INSTALLATION_EXISTS)
            .bind(organization_id.uuid())
            .bind(workspace_id.uuid())
            .bind(installation_id.uuid())
            .fetch_optional(&mut *transaction)
            .await
            .map_err(failed("create_grant"))?;
        if installation.is_none() {
            return Err(AgentError::InstallationNotFound);
        }
        check_tools(&mut transaction, organization_id, workspace_id, installation_id, tools).await?;
        let existing = sqlx::query(GRANT_EXISTS)
            .bind(organization_id.uuid())
            .bind(workspace_id.uuid())
            .bind(agent_id.uuid())
            .bind(installation_id.uuid())
            .fetch_optional(&mut *transaction)
            .await
            .map_err(failed("create_grant"))?;
        if existing.is_some() {
            return Err(AgentError::GrantExists);
        }
        let now = Timestamp::now().millis();
        let row = sqlx::query(INSERT_GRANT)
            .bind(grant_id.uuid())
            .bind(organization_id.uuid())
            .bind(workspace_id.uuid())
            .bind(agent_id.uuid())
            .bind(installation_id.uuid())
            .bind(names_json(&tools.tools)?)
            .bind(names_json(&tools.approval_required)?)
            .bind(now)
            .bind(now)
            .fetch_one(&mut *transaction)
            .await
            .map_err(failed("create_grant"))?;
        let grant = grant_from(&row)?;
        crate::audit::record_in_workspace(&mut transaction, organization_id, workspace_id, &audit(&grant))
            .await
            .map_err(failed("audit"))?;
        transaction.commit().await.map_err(failed("create_grant"))?;
        Ok(grant)
    }

    async fn update_grant(
        &self,
        organization_id: OrganizationId,
        workspace_id: WorkspaceId,
        agent_id: AgentId,
        grant_id: GrantId,
        tools: &GrantTools,
        audit: &Audit<'_, Grant>,
    ) -> Result<Grant, AgentError> {
        let mut transaction = self.immediate("update_grant").await?;
        if !agent_exists(&mut transaction, organization_id, workspace_id, agent_id).await? {
            return Err(AgentError::AgentNotFound);
        }
        let row = sqlx::query(FIND_GRANT)
            .bind(organization_id.uuid())
            .bind(workspace_id.uuid())
            .bind(agent_id.uuid())
            .bind(grant_id.uuid())
            .fetch_optional(&mut *transaction)
            .await
            .map_err(failed("update_grant"))?;
        let Some(row) = row else {
            return Err(AgentError::GrantNotFound);
        };
        let installation_id = grant_from(&row)?.installation_id;
        check_tools(&mut transaction, organization_id, workspace_id, installation_id, tools).await?;
        let row = sqlx::query(UPDATE_GRANT)
            .bind(names_json(&tools.tools)?)
            .bind(names_json(&tools.approval_required)?)
            .bind(Timestamp::now().millis())
            .bind(organization_id.uuid())
            .bind(workspace_id.uuid())
            .bind(agent_id.uuid())
            .bind(grant_id.uuid())
            .fetch_one(&mut *transaction)
            .await
            .map_err(failed("update_grant"))?;
        let grant = grant_from(&row)?;
        crate::audit::record_in_workspace(&mut transaction, organization_id, workspace_id, &audit(&grant))
            .await
            .map_err(failed("audit"))?;
        transaction.commit().await.map_err(failed("update_grant"))?;
        Ok(grant)
    }

    async fn delete_grant(
        &self,
        organization_id: OrganizationId,
        workspace_id: WorkspaceId,
        agent_id: AgentId,
        grant_id: GrantId,
        audit: &Audit<'_, ()>,
    ) -> Result<(), AgentError> {
        let mut transaction = self.immediate("delete_grant").await?;
        if !agent_exists(&mut transaction, organization_id, workspace_id, agent_id).await? {
            return Err(AgentError::AgentNotFound);
        }
        let deleted = sqlx::query(
            "DELETE FROM agent_grants WHERE organization_id = ? AND workspace_id = ? AND agent_id = ? AND id = ?",
        )
        .bind(organization_id.uuid())
        .bind(workspace_id.uuid())
        .bind(agent_id.uuid())
        .bind(grant_id.uuid())
        .execute(&mut *transaction)
        .await
        .map_err(failed("delete_grant"))?;
        if deleted.rows_affected() == 0 {
            return Err(AgentError::GrantNotFound);
        }
        crate::audit::record_in_workspace(&mut transaction, organization_id, workspace_id, &audit(&()))
            .await
            .map_err(failed("audit"))?;
        transaction.commit().await.map_err(failed("delete_grant"))?;
        Ok(())
    }

    async fn count_grants_by_installation(
        &self,
        organization_id: OrganizationId,
        workspace_id: WorkspaceId,
    ) -> Result<Vec<(InstallationId, u64)>, AgentError> {
        let rows = sqlx::query(
            "SELECT installation_id, count(*) AS grants FROM agent_grants \
             WHERE organization_id = ? AND workspace_id = ? GROUP BY installation_id",
        )
        .bind(organization_id.uuid())
        .bind(workspace_id.uuid())
        .fetch_all(self.database.pool())
        .await
        .map_err(failed("count_grants_by_installation"))?;
        rows.iter()
            .map(|row| {
                let grants: i64 = row.try_get("grants").map_err(read)?;
                Ok((
                    InstallationId::from_uuid(row.try_get("installation_id").map_err(read)?),
                    u64::try_from(grants).map_err(read)?,
                ))
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use std::sync::Arc;

    use zupflow_conformance::agent_store::{Prepare, Stores, Tenant};

    use super::*;
    use crate::schema::{Temporary, temporary};

    /// Makes the tenant's rows straight in the database: an organization, two
    /// workspaces, and installations of one catalog app.
    fn prepare(database: Database) -> Prepare {
        Arc::new(move |tenant: Tenant| {
            let database = database.clone();
            Box::pin(async move {
                let pool = database.pool();
                let org = tenant.organization_id.uuid();
                let app = tenant.app.uuid();
                sqlx::query("INSERT INTO organizations (id, name, slug, created_at) VALUES (?, 'Test', ?, 0)")
                    .bind(org)
                    .bind(format!("org-{}", org.simple()))
                    .execute(pool)
                    .await
                    .unwrap();
                sqlx::query(
                    "INSERT INTO apps (id, organization_id, kind, name, key, description, icon_glyph, icon_color, \
                     auth, tools, version, created_at, updated_at) \
                     VALUES (?, ?, 'mcp', 'Catalog', 'catalog', '', 'C', '#0E7490', '{\"type\":\"token\"}', '[]', 1, 0, 0)",
                )
                .bind(app)
                .bind(org)
                .execute(pool)
                .await
                .unwrap();
                for (workspace, installations) in [
                    (tenant.workspace, vec![tenant.installation, tenant.second_installation]),
                    (tenant.sibling, vec![tenant.sibling_installation]),
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
                    for installation in installations {
                        let short_id = crate::installations::record_test_short_id(pool, org, workspace.uuid()).await;
                        sqlx::query(
                            "INSERT INTO installations (id, organization_id, workspace_id, catalog_app_id, short_id, \
                             label, status, configuration_version, granted_scopes, created_at, updated_at) \
                             VALUES (?, ?, ?, ?, ?, 'Main', 'disconnected', 0, '[]', 0, 0)",
                        )
                        .bind(installation.uuid())
                        .bind(org)
                        .bind(workspace.uuid())
                        .bind(app)
                        .bind(short_id)
                        .execute(pool)
                        .await
                        .unwrap();
                    }
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
        zupflow_conformance::agent_store::run(
            Stores {
                agents: &SqliteAgentStore::new(database.clone()),
                policies: &crate::SqliteToolPolicyStore::new(database.clone()),
                registry: &crate::SqliteToolRegistry::new(database.clone()),
                installations: &crate::SqliteInstallationStore::new(database.clone()),
            },
            prepare(database),
        )
        .await;
    }

    /// Deleting a workspace takes its agents and grants, and nothing of a
    /// sibling's; the database refuses a grant naming another workspace's
    /// agent or installation even when the store is bypassed.
    #[tokio::test]
    async fn keeps_grants_inside_their_workspace_in_the_database_itself() {
        use zupflow_core::workspaces::WorkspaceStore;

        let Temporary {
            database,
            directory: _directory,
        } = temporary().await;
        let tenant = Tenant {
            organization_id: OrganizationId::generate(),
            app: AppId::generate(),
            workspace: WorkspaceId::generate(),
            installation: InstallationId::generate(),
            second_installation: InstallationId::generate(),
            sibling: WorkspaceId::generate(),
            sibling_installation: InstallationId::generate(),
        };
        prepare(database.clone())(tenant).await;
        let store = SqliteAgentStore::new(database.clone());
        let org = tenant.organization_id;
        let new = NewAgent {
            name: "A".to_owned(),
            description: String::new(),
            model: "m".to_owned(),
            instructions: String::new(),
        };
        let here = store
            .create_agent(org, tenant.workspace, AgentId::generate(), &new, &|_| {
                zupflow_conformance::audit_store::by_suite_in(tenant.workspace)
            })
            .await
            .unwrap();
        let there = store
            .create_agent(org, tenant.sibling, AgentId::generate(), &new, &|_| {
                zupflow_conformance::audit_store::by_suite_in(tenant.sibling)
            })
            .await
            .unwrap();
        let insert = |agent: AgentId, installation: InstallationId| {
            let pool = database.pool().clone();
            async move {
                sqlx::query(
                    "INSERT INTO agent_grants (id, organization_id, workspace_id, agent_id, installation_id, tools, \
                     approval_required, created_at, updated_at) VALUES (?, ?, ?, ?, ?, '[\"t\"]', '[]', 0, 0)",
                )
                .bind(uuid::Uuid::now_v7())
                .bind(org.uuid())
                .bind(tenant.workspace.uuid())
                .bind(agent.uuid())
                .bind(installation.uuid())
                .execute(&pool)
                .await
            }
        };
        assert!(
            insert(there.id, tenant.installation).await.is_err(),
            "another workspace's agent"
        );
        assert!(
            insert(here.id, tenant.sibling_installation).await.is_err(),
            "another workspace's installation"
        );
        insert(here.id, tenant.installation).await.unwrap();
        assert!(
            insert(here.id, tenant.installation).await.is_err(),
            "one grant per agent and installation"
        );
        crate::SqliteWorkspaceStore::new(database.clone())
            .delete_workspace(org, tenant.workspace, &|_| {
                zupflow_conformance::audit_store::by_suite_in(tenant.workspace)
            })
            .await
            .unwrap();
        let left: Vec<(i64, i64)> =
            sqlx::query_as("SELECT (SELECT count(*) FROM agents), (SELECT count(*) FROM agent_grants)")
                .fetch_all(database.pool())
                .await
                .unwrap();
        assert_eq!(left, [(1, 0)], "only the sibling's agent is left");
        assert_eq!(
            store
                .find_agent(org, tenant.sibling, there.id)
                .await
                .unwrap()
                .map(|a| a.id),
            Some(there.id)
        );
    }
}
