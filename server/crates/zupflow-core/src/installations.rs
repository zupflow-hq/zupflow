//! Installations: an app connected to one account in one workspace, and the
//! credential that account granted (decision 0034). An installation names
//! exactly one app, a catalog app or the workspace's own (decision 0031).
//!
//! The credential is sealed before it reaches the store, under the
//! `installation_credential` context of its version, and only its version
//! is ever shown. Every [`InstallationStore`] method takes the organization,
//! then the workspace.

use async_trait::async_trait;
use serde::{Deserialize, Serialize};

use crate::apps::AuthTemplate;
use crate::audit::Audit;
use crate::environment::Environment;
use crate::ids::{AppId, InstallationId, OrganizationId, WorkspaceId};
use crate::keys::Sealed;
use crate::time::Timestamp;
use crate::workspaces::Page;

/// Which app an installation is of: one of the organization's catalog, or
/// the installation's own workspace's.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InstalledApp {
    Catalog(AppId),
    Workspace(AppId),
}

impl InstalledApp {
    pub fn id(self) -> AppId {
        match self {
            Self::Catalog(id) | Self::Workspace(id) => id,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum InstallationStatus {
    Connected,
    ReconnectRequired,
    Disconnected,
}

impl InstallationStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Connected => "connected",
            Self::ReconnectRequired => "reconnect_required",
            Self::Disconnected => "disconnected",
        }
    }

    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "connected" => Some(Self::Connected),
            "reconnect_required" => Some(Self::ReconnectRequired),
            "disconnected" => Some(Self::Disconnected),
            _ => None,
        }
    }
}

/// The provider account an installation acts as.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ConnectedAccount {
    pub external_id: String,
    pub display_name: String,
    pub email: Option<String>,
}

/// The contract's `AccountConnection`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct AccountConnection {
    pub account: ConnectedAccount,
    pub granted_scopes: Vec<String>,
    pub credential_version: u32,
    pub connected_at: Timestamp,
    pub expires_at: Option<Timestamp>,
}

/// The app's authentication template and version as they were when the
/// installation last connected (decision 0036). The credential service uses
/// only this: a later template change never redirects a stored credential.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TemplateSnapshot {
    pub auth: AuthTemplate,
    pub app_version: u32,
    /// For an `mcp_oauth` app, what its sign-in discovered and signed in as;
    /// `None` for every other type.
    pub client: Option<DiscoveredClient>,
}

/// Where an `mcp_oauth` installation signed in (decision 0039): the
/// authorization server's issuer and token endpoint as discovered, the
/// client registered there, and the MCP server the token was requested for
/// (RFC 8707). A refresh goes here, and the token goes only to `resource`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DiscoveredClient {
    pub issuer: String,
    pub token_endpoint: String,
    pub client_id: String,
    pub resource: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Installation {
    pub id: InstallationId,
    pub organization_id: OrganizationId,
    pub workspace_id: WorkspaceId,
    pub app: InstalledApp,
    pub label: String,
    pub status: InstallationStatus,
    /// The app configuration's version it connected under.
    pub configuration_version: u32,
    pub connection: Option<AccountConnection>,
    /// `None` only for an installation that connected before snapshots were kept.
    pub snapshot: Option<TemplateSnapshot>,
    pub created_at: Timestamp,
    pub updated_at: Timestamp,
}

/// A credential, sealed under the `installation_credential` context of
/// `version`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NewCredential {
    pub sealed: Sealed,
    pub version: u32,
}

/// A new, connected installation and its credential.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NewInstallation {
    pub app: InstalledApp,
    pub label: String,
    pub configuration_version: u32,
    pub account: ConnectedAccount,
    pub granted_scopes: Vec<String>,
    pub expires_at: Option<Timestamp>,
    pub credential: NewCredential,
    pub snapshot: TemplateSnapshot,
}

/// An installation seen from its catalog app's page: the installation and the
/// workspace it is in.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AppInstallation {
    pub installation: Installation,
    pub workspace_name: String,
    pub workspace_slug: String,
    pub environment: Environment,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum InstallationError {
    #[error("the store is unavailable")]
    Unavailable,
    #[error("not found")]
    NotFound,
    /// A cursor this store didn't issue.
    #[error("not a cursor from this list")]
    InvalidCursor,
}

#[async_trait]
pub trait InstallationStore: Send + Sync {
    /// The workspace's installations, newest first.
    async fn list_installations(
        &self,
        organization_id: OrganizationId,
        workspace_id: WorkspaceId,
    ) -> Result<Vec<Installation>, InstallationError>;

    async fn find_installation(
        &self,
        organization_id: OrganizationId,
        workspace_id: WorkspaceId,
        installation_id: InstallationId,
    ) -> Result<Option<Installation>, InstallationError>;

    /// Creates a connected installation with its credential, as one. `NotFound`
    /// unless the app is the organization's catalog app or this workspace's own.
    async fn create_installation(
        &self,
        organization_id: OrganizationId,
        workspace_id: WorkspaceId,
        installation_id: InstallationId,
        installation: &NewInstallation,
    ) -> Result<Installation, InstallationError>;

    /// Renames it, recording `audit` of it, renamed.
    async fn rename_installation(
        &self,
        organization_id: OrganizationId,
        workspace_id: WorkspaceId,
        installation_id: InstallationId,
        label: &str,
        audit: &Audit<'_, Installation>,
    ) -> Result<Installation, InstallationError>;

    /// Marks it disconnected and deletes its credential, as one: nothing can
    /// use an account it no longer holds. Records `audit` of it, disconnected.
    async fn disconnect_installation(
        &self,
        organization_id: OrganizationId,
        workspace_id: WorkspaceId,
        installation_id: InstallationId,
        audit: &Audit<'_, Installation>,
    ) -> Result<Installation, InstallationError>;

    /// Deletes it and its credential, recording `audit` of it as it was.
    async fn delete_installation(
        &self,
        organization_id: OrganizationId,
        workspace_id: WorkspaceId,
        installation_id: InstallationId,
        audit: &Audit<'_, Installation>,
    ) -> Result<(), InstallationError>;

    /// The sealed credential, while the installation holds one.
    async fn find_credential(
        &self,
        organization_id: OrganizationId,
        workspace_id: WorkspaceId,
        installation_id: InstallationId,
    ) -> Result<Option<NewCredential>, InstallationError>;

    /// A catalog app's installations across the organization's workspaces,
    /// newest first, at most `limit` (1–100), in one environment if given.
    async fn list_app_installations(
        &self,
        organization_id: OrganizationId,
        app_id: AppId,
        environment: Option<Environment>,
        cursor: Option<&str>,
        limit: u32,
    ) -> Result<Page<AppInstallation>, InstallationError>;

    /// How many installations each of the organization's apps has, catalog
    /// and workspace apps alike, in workspaces of `environment` if given.
    /// Apps with none are left out.
    async fn count_by_app(
        &self,
        organization_id: OrganizationId,
        environment: Option<Environment>,
    ) -> Result<Vec<(AppId, u64)>, InstallationError>;
}
