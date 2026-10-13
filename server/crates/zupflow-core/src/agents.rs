//! Agents and their grants (decision 0040). An agent belongs to one
//! workspace; a grant gives it tools of one installation in that workspace,
//! at most one grant per agent and installation, with `approval_required` a
//! subset of `tools`.
//!
//! A grant names tools in its installation's registry ([`crate::tools`]): a
//! tool that isn't there, or is gone, is refused; a changed one can be
//! granted, and calls are held to its pinned definition. The store checks
//! this in the same transaction that writes the grant, so a listing recorded
//! meanwhile can't slip a gone tool in. A grant already holding a tool that
//! later goes isn't rewritten: the run engine refuses the call.
//!
//! A grant is held inside the organization's levels ([`crate::policy`]): a
//! tool the organization denies is refused, and `approval_required` must
//! name every tool it sets to approve. The store reads the levels in the
//! transaction that writes the grant, so a level tightened meanwhile can't be
//! slipped past. A grant isn't rewritten when a level tightens later: the run
//! engine holds the call to it.
//!
//! Deleting an agent deletes its grants, and deleting an installation
//! deletes the grants of it, each as one change. Every [`AgentStore`] method
//! takes the organization, then the workspace.

use async_trait::async_trait;

use crate::audit::Audit;
use crate::ids::{AgentId, GrantId, InstallationId, ModelId, OrganizationId, WorkspaceId};
use crate::policy::{GrantLevels, LevelRefusal, level_refusal};
use crate::time::Timestamp;

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum AgentStatus {
    Active,
    Paused,
}

impl AgentStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Active => "active",
            Self::Paused => "paused",
        }
    }

    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "active" => Some(Self::Active),
            "paused" => Some(Self::Paused),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Agent {
    pub id: AgentId,
    pub organization_id: OrganizationId,
    pub workspace_id: WorkspaceId,
    pub name: String,
    pub description: String,
    /// The model it overrides its environment's default with: one of the
    /// organization's, added in the workspace's environment. `None` runs on
    /// the default.
    pub model_id: Option<ModelId>,
    pub instructions: String,
    pub status: AgentStatus,
    /// How many grants it holds, counted when read.
    pub grant_count: u64,
    pub created_at: Timestamp,
    pub updated_at: Timestamp,
}

/// A new agent, checked by the caller; it starts active.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NewAgent {
    pub name: String,
    pub description: String,
    /// Held to the same rule as [`Agent::model_id`] by the store.
    pub model_id: Option<ModelId>,
    pub instructions: String,
}

/// What an update changes; `None` keeps the stored value.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct AgentChanges {
    pub name: Option<String>,
    pub description: Option<String>,
    /// `Some(None)` goes back to the environment's default.
    pub model_id: Option<Option<ModelId>>,
    pub instructions: Option<String>,
    pub status: Option<AgentStatus>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Grant {
    pub id: GrantId,
    pub organization_id: OrganizationId,
    pub workspace_id: WorkspaceId,
    pub agent_id: AgentId,
    pub installation_id: InstallationId,
    /// Tools the agent may call through the installation, as given.
    pub tools: Vec<String>,
    /// The subset of `tools` that waits for a person's approval.
    pub approval_required: Vec<String>,
    pub created_at: Timestamp,
    pub updated_at: Timestamp,
}

/// A grant's tools and approvals, as written.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GrantTools {
    pub tools: Vec<String>,
    pub approval_required: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum AgentError {
    #[error("the store is unavailable")]
    Unavailable,
    /// The agent isn't in this scope.
    #[error("agent not found")]
    AgentNotFound,
    /// The model named isn't one of the organization's in the workspace's environment.
    #[error("not a model of this workspace's environment")]
    UnknownModel,
    /// The grant isn't the agent's, in this scope.
    #[error("grant not found")]
    GrantNotFound,
    /// The installation isn't in this scope.
    #[error("installation not found")]
    InstallationNotFound,
    /// The agent already has a grant of this installation.
    #[error("the agent already has a grant of this installation")]
    GrantExists,
    /// A tool the installation's registry doesn't hold: the first, in the order given.
    #[error("{0} isn't in the installation's registry")]
    UnknownTool(String),
    /// A tool the installation's server no longer lists: the first, in the order given.
    #[error("{0} is gone")]
    ToolGone(String),
    /// A tool requiring approval that isn't granted: the first, in the order given.
    #[error("{0} requires approval but isn't granted")]
    ApprovalOutsideGrant(String),
    /// A tool the organization denies: the first, in the order given.
    #[error("the organization denies {0}")]
    ToolDenied(String),
    /// A tool the organization sets to approve that doesn't require approval: the first, in the order given.
    #[error("the organization requires approval for {0}")]
    ApprovalRequiredByPolicy(String),
}

/// Every change to an agent or a grant records its [`Audit`] in the
/// change's transaction.
#[async_trait]
pub trait AgentStore: Send + Sync {
    /// The workspace's agents, by name.
    async fn list_agents(
        &self,
        organization_id: OrganizationId,
        workspace_id: WorkspaceId,
    ) -> Result<Vec<Agent>, AgentError>;

    async fn find_agent(
        &self,
        organization_id: OrganizationId,
        workspace_id: WorkspaceId,
        agent_id: AgentId,
    ) -> Result<Option<Agent>, AgentError>;

    /// Creates an active agent in a workspace the caller has resolved in its
    /// organization. Its model, if it names one, must be the organization's,
    /// added in the workspace's environment (`UnknownModel`), checked in the
    /// transaction that writes the agent.
    async fn create_agent(
        &self,
        organization_id: OrganizationId,
        workspace_id: WorkspaceId,
        agent_id: AgentId,
        agent: &NewAgent,
        audit: &Audit<'_, Agent>,
    ) -> Result<Agent, AgentError>;

    /// Changes what `changes` names; a model is held to the rule
    /// [`AgentStore::create_agent`] holds it to, in the same transaction.
    async fn update_agent(
        &self,
        organization_id: OrganizationId,
        workspace_id: WorkspaceId,
        agent_id: AgentId,
        changes: &AgentChanges,
        audit: &Audit<'_, Agent>,
    ) -> Result<Agent, AgentError>;

    /// Deletes the agent and its grants, as one, recording `audit` of the
    /// agent as it was.
    async fn delete_agent(
        &self,
        organization_id: OrganizationId,
        workspace_id: WorkspaceId,
        agent_id: AgentId,
        audit: &Audit<'_, Agent>,
    ) -> Result<(), AgentError>;

    /// The agent's grants, oldest first. `AgentNotFound` when the agent isn't here.
    async fn list_grants(
        &self,
        organization_id: OrganizationId,
        workspace_id: WorkspaceId,
        agent_id: AgentId,
    ) -> Result<Vec<Grant>, AgentError>;

    /// Grants the agent tools of an installation of the same workspace. In
    /// one transaction, and in this order: the agent (`AgentNotFound`), the
    /// installation (`InstallationNotFound`), each tool against the
    /// installation's registry (`UnknownTool`, `ToolGone`), the approvals
    /// (`ApprovalOutsideGrant`), the organization's levels in force for a
    /// catalog app's tools, read in the same transaction ([`levels_refusal`]:
    /// `ToolDenied`, `ApprovalRequiredByPolicy`), then one grant per agent
    /// and installation (`GrantExists`). A workspace app's tools have no
    /// organization level.
    #[allow(clippy::too_many_arguments)]
    async fn create_grant(
        &self,
        organization_id: OrganizationId,
        workspace_id: WorkspaceId,
        agent_id: AgentId,
        grant_id: GrantId,
        installation_id: InstallationId,
        tools: &GrantTools,
        audit: &Audit<'_, Grant>,
    ) -> Result<Grant, AgentError>;

    /// Replaces a grant's tools and approvals, checked as
    /// [`AgentStore::create_grant`] checks them, in one transaction.
    async fn update_grant(
        &self,
        organization_id: OrganizationId,
        workspace_id: WorkspaceId,
        agent_id: AgentId,
        grant_id: GrantId,
        tools: &GrantTools,
        audit: &Audit<'_, Grant>,
    ) -> Result<Grant, AgentError>;

    async fn delete_grant(
        &self,
        organization_id: OrganizationId,
        workspace_id: WorkspaceId,
        agent_id: AgentId,
        grant_id: GrantId,
        audit: &Audit<'_, ()>,
    ) -> Result<(), AgentError>;

    /// How many grants each of the workspace's installations has; those
    /// with none are left out.
    async fn count_grants_by_installation(
        &self,
        organization_id: OrganizationId,
        workspace_id: WorkspaceId,
    ) -> Result<Vec<(InstallationId, u64)>, AgentError>;
}

/// The first approval that isn't granted, if any: every store checks it.
pub fn approval_outside(tools: &GrantTools) -> Option<&str> {
    tools
        .approval_required
        .iter()
        .find(|name| !tools.tools.contains(name))
        .map(String::as_str)
}

/// The organization's levels a grant breaks, as the store's error: every
/// store checks it after the registry's rules, with `levels` read in its
/// transaction ([`GrantLevels::in_force`]; [`GrantLevels::none`] for a
/// workspace app).
pub fn levels_refusal(tools: &GrantTools, levels: &GrantLevels) -> Option<AgentError> {
    level_refusal(&tools.tools, &tools.approval_required, levels).map(|refusal| match refusal {
        LevelRefusal::Denied(tool) => AgentError::ToolDenied(tool),
        LevelRefusal::ApprovalRequired(tool) => AgentError::ApprovalRequiredByPolicy(tool),
    })
}
