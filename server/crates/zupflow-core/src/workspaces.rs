//! Workspaces: an organization's own customers, each in one environment. The
//! domain's types and its [`WorkspaceStore`], every method of which takes the
//! organization first.

use async_trait::async_trait;
use serde::Serialize;

use crate::audit::Audit;
use crate::environment::Environment;
use crate::ids::{OrganizationId, WorkspaceId};
use crate::time::Timestamp;

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Workspace {
    pub id: WorkspaceId,
    pub organization_id: OrganizationId,
    pub name: String,
    pub slug: String,
    /// Fixed at creation.
    pub environment: Environment,
    /// The customer's ID in the organization's own platform. Searchable,
    /// never used for authorization.
    pub external_id: Option<String>,
    pub created_at: Timestamp,
}

/// The contract's `WorkspaceStats`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize)]
pub struct WorkspaceStats {
    pub agents: u64,
    pub installations: u64,
    pub runs_last_24h: u64,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum WorkspaceSort {
    /// By name, case-insensitively, then environment.
    #[default]
    Name,
    /// Most runs in the last 24 hours first, then by name.
    Activity,
}

/// What a list asks for. `query` matches name, slug or External ID,
/// case-insensitively, as a substring.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct WorkspaceFilter {
    pub environment: Option<Environment>,
    pub query: Option<String>,
    pub sort: WorkspaceSort,
}

/// One page of a list, and where the next one starts.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Page<T> {
    pub items: Vec<T>,
    /// Opaque; `None` on the last page.
    pub next_cursor: Option<String>,
    /// How many match the filter in all.
    pub total: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum WorkspaceError {
    #[error("the store is unavailable")]
    Unavailable,
    #[error("that slug is taken in this organization")]
    SlugTaken,
    #[error("not found")]
    NotFound,
    /// A cursor this store didn't issue.
    #[error("not a cursor from this list")]
    InvalidCursor,
}

/// A new workspace's fields.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NewWorkspace {
    pub name: String,
    pub slug: String,
    pub environment: Environment,
    pub external_id: Option<String>,
}

#[async_trait]
pub trait WorkspaceStore: Send + Sync {
    /// A page of the organization's workspaces, at most `limit` (1–100).
    async fn list_workspaces(
        &self,
        organization_id: OrganizationId,
        filter: &WorkspaceFilter,
        cursor: Option<&str>,
        limit: u32,
    ) -> Result<Page<Workspace>, WorkspaceError>;

    /// Creates the workspace, recording `audit` of it with it.
    async fn create_workspace(
        &self,
        organization_id: OrganizationId,
        workspace_id: WorkspaceId,
        workspace: &NewWorkspace,
        audit: &Audit<'_, Workspace>,
    ) -> Result<Workspace, WorkspaceError>;

    /// The workspace, in this organization only.
    async fn find_workspace(
        &self,
        organization_id: OrganizationId,
        workspace_id: WorkspaceId,
    ) -> Result<Option<Workspace>, WorkspaceError>;

    /// Renames it and/or changes its External ID (`Some(None)` clears it),
    /// recording `audit` of the workspace as changed.
    async fn update_workspace(
        &self,
        organization_id: OrganizationId,
        workspace_id: WorkspaceId,
        name: Option<&str>,
        external_id: Option<Option<&str>>,
        audit: &Audit<'_, Workspace>,
    ) -> Result<Workspace, WorkspaceError>;

    /// Deletes the workspace and everything in it but its audit history,
    /// recording `audit` of the workspace as it was.
    async fn delete_workspace(
        &self,
        organization_id: OrganizationId,
        workspace_id: WorkspaceId,
        audit: &Audit<'_, Workspace>,
    ) -> Result<(), WorkspaceError>;

    /// What the workspace holds: its agents, its installations and the
    /// runs started in it in the last 24 hours.
    async fn load_stats(
        &self,
        organization_id: OrganizationId,
        workspace_id: WorkspaceId,
    ) -> Result<WorkspaceStats, WorkspaceError>;
}
