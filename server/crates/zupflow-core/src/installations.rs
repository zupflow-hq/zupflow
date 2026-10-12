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
    #[serde(default)]
    pub response_iss_supported: bool,
    pub token_endpoint: String,
    pub client_id: String,
    pub resource: String,
}

/// An installation's short ID (decision 0047): six characters of lowercase
/// base32 (`a-z`, `2-7`), given when the installation is created, kept across
/// reconnects and renames, and never issued twice in its workspace. A label
/// for people and models, never an identifier: nothing is found by it.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize)]
#[serde(transparent)]
pub struct ShortId(String);

impl ShortId {
    const ALPHABET: &'static [u8; 32] = b"abcdefghijklmnopqrstuvwxyz234567";

    /// A short ID drawn from the operating system's random source.
    pub fn generate() -> Result<Self, getrandom::Error> {
        let mut bytes = [0u8; 4];
        getrandom::fill(&mut bytes)?;
        Ok(Self::from_bits(u32::from_le_bytes(bytes)))
    }

    /// The short ID the low 30 bits of `bits` spell, five bits a character.
    pub fn from_bits(bits: u32) -> Self {
        let spelled = (0..6)
            .map(|index| char::from(Self::ALPHABET[((bits >> (5 * index)) & 31) as usize]))
            .collect();
        Self(spelled)
    }

    /// `value` if it is a short ID's form.
    pub fn parse(value: &str) -> Option<Self> {
        let valid = value.len() == 6 && value.bytes().all(|byte| Self::ALPHABET.contains(&byte));
        valid.then(|| Self(value.to_owned()))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Installation {
    pub id: InstallationId,
    pub organization_id: OrganizationId,
    pub workspace_id: WorkspaceId,
    pub app: InstalledApp,
    /// Unique in the workspace, read-only (decision 0047).
    pub short_id: ShortId,
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

/// The durable claim of one credential version. A pending claim must never
/// be taken over: the server may already have rotated its refresh token.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RefreshClaim {
    Claimed,
    Pending { started_at: Timestamp },
    Changed,
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
    /// use an account it no longer holds. Records `audit`, an immutable
    /// installation.disconnected event and delivery jobs in that transaction,
    /// and queues the job that refuses the calls waiting for approval on it
    /// ([`crate::runs::store::refuse_waiting_job`], decision 0047).
    /// An already disconnected installation is returned unchanged, with no
    /// new audit, event or job.
    async fn disconnect_installation(
        &self,
        organization_id: OrganizationId,
        workspace_id: WorkspaceId,
        installation_id: InstallationId,
        audit: &Audit<'_, Installation>,
    ) -> Result<Installation, InstallationError>;

    /// Deletes it and its credential, recording `audit` of it as it was, and
    /// an immutable installation.disconnected event with its delivery jobs,
    /// and queues the job that refuses the calls waiting for approval on it
    /// ([`crate::runs::store::refuse_waiting_job`], decision 0047), all in
    /// one transaction.
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

    /// Claims a refresh only if connected with exactly `version`, atomically.
    /// The claim survives a crash. A loser re-reads the winner's credential.
    async fn claim_refresh(
        &self,
        organization_id: OrganizationId,
        workspace_id: WorkspaceId,
        installation_id: InstallationId,
        version: u32,
        now: Timestamp,
    ) -> Result<RefreshClaim, InstallationError>;

    /// Saves the next sealed version and expiry together. Succeeds only for
    /// the still-connected claimed version; reconnect/disconnect wins a race.
    async fn rotate_credential(
        &self,
        organization_id: OrganizationId,
        workspace_id: WorkspaceId,
        installation_id: InstallationId,
        credential: &NewCredential,
        expires_at: Option<Timestamp>,
        now: Timestamp,
    ) -> Result<bool, InstallationError>;

    /// Marks exactly this connected version as needing reconnect, with its
    /// audit event and webhook outbox in one transaction. Repeats, foreign
    /// scopes and stale failures change nothing and announce nothing.
    async fn require_reconnect(
        &self,
        organization_id: OrganizationId,
        workspace_id: WorkspaceId,
        installation_id: InstallationId,
        version: u32,
        now: Timestamp,
        audit: &Audit<'_, Installation>,
    ) -> Result<bool, InstallationError>;

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

#[cfg(test)]
mod tests {
    use super::ShortId;

    #[test]
    fn spells_six_lowercase_base32_characters() {
        assert_eq!(ShortId::from_bits(0).as_str(), "aaaaaa");
        assert_eq!(ShortId::from_bits(u32::MAX).as_str(), "777777");
        // Five bits a character, lowest first; the top two bits are unused.
        assert_eq!(ShortId::from_bits(1 | (26 << 5) | (3 << 30)).as_str(), "b2aaaa");
        for _ in 0..64 {
            let drawn = ShortId::generate().expect("the OS has randomness");
            assert_eq!(ShortId::parse(drawn.as_str()), Some(drawn.clone()));
        }
        for value in ["", "aaaaa", "aaaaaaa", "AAAAAA", "aaaa1a", "aaaa8a", "aaa-aa"] {
            assert_eq!(ShortId::parse(value), None, "{value}");
        }
    }
}
