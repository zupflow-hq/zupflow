//! Connect (decision 0005): an organization's allowlists for its users'
//! browsers, per environment, and sessions that let one of its users connect
//! an account to a workspace, on Zupflow's hosted page.
//!
//! A session's link carries a random **reference** (at least 190 bits), never
//! its ID. Only its SHA-256 is stored, so the database can't rebuild a link,
//! and the hosted page finds its session by that hash alone. That lookup is
//! the one statement here that starts without a tenant
//! ([`ConnectStore::attribute_session_by_reference`], declared cross-tenant);
//! everything after it carries the session's organization and workspace.

use async_trait::async_trait;
use serde::Serialize;
use sha2::{Digest, Sha256};

use crate::environment::Environment;
use crate::ids::{ConnectSessionId, InstallationId, OrganizationId, WorkspaceId};
use crate::installations::{ConnectedAccount, Installation, InstalledApp, NewCredential};
use crate::time::Timestamp;

/// How long a session's link works.
pub const SESSION_TTL_MS: i64 = 15 * 60 * 1000;

/// The contract's `ConnectSettings`, without its environment.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ConnectSettings {
    /// Exact URLs Connect may send a user back to.
    pub return_urls: Vec<String>,
    /// Origins that may embed Connect.
    pub allowed_origins: Vec<String>,
    /// `None` until first written.
    pub updated_at: Option<Timestamp>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ConnectIntent {
    Create,
    Reconnect,
}

impl ConnectIntent {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Create => "create",
            Self::Reconnect => "reconnect",
        }
    }

    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "create" => Some(Self::Create),
            "reconnect" => Some(Self::Reconnect),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ConnectSessionStatus {
    Pending,
    Completed,
    Expired,
    Cancelled,
    Failed,
}

impl ConnectSessionStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::Completed => "completed",
            Self::Expired => "expired",
            Self::Cancelled => "cancelled",
            Self::Failed => "failed",
        }
    }

    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "pending" => Some(Self::Pending),
            "completed" => Some(Self::Completed),
            "expired" => Some(Self::Expired),
            "cancelled" => Some(Self::Cancelled),
            "failed" => Some(Self::Failed),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConnectSession {
    pub id: ConnectSessionId,
    pub organization_id: OrganizationId,
    pub workspace_id: WorkspaceId,
    /// The workspace's, when the session started.
    pub environment: Environment,
    pub app: InstalledApp,
    /// The app configuration's version when the session started.
    pub configuration_version: u32,
    pub intent: ConnectIntent,
    /// The installation a reconnect renews, or the one a completed create
    /// made; `None` for a create until it completes.
    pub installation_id: Option<InstallationId>,
    pub label: String,
    /// As stored. A pending session past `expires_at` reads as expired:
    /// use [`ConnectSession::status_at`].
    pub status: ConnectSessionStatus,
    pub return_url: Option<String>,
    pub requested_scopes: Vec<String>,
    pub created_at: Timestamp,
    pub expires_at: Timestamp,
    pub completed_at: Option<Timestamp>,
    pub failure_code: Option<String>,
}

impl ConnectSession {
    /// The status as of `now`: a pending session past its expiry is expired,
    /// whoever reads it.
    pub fn status_at(&self, now: Timestamp) -> ConnectSessionStatus {
        if self.status == ConnectSessionStatus::Pending && now.millis() >= self.expires_at.millis() {
            ConnectSessionStatus::Expired
        } else {
            self.status
        }
    }
}

/// A new session: everything but its ID, scope, status and times.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NewConnectSession {
    pub environment: Environment,
    pub app: InstalledApp,
    pub configuration_version: u32,
    pub intent: ConnectIntent,
    pub installation_id: Option<InstallationId>,
    pub label: String,
    pub return_url: Option<String>,
    pub requested_scopes: Vec<String>,
    pub reference_hash: ReferenceHash,
}

/// What completing a session writes: the installation it makes (a create)
/// or renews (a reconnect), with the credential already sealed for it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Completion {
    /// For a create, the new installation's ID; for a reconnect, the
    /// session's own installation. The credential is sealed for this ID.
    pub installation_id: InstallationId,
    /// The app configuration's version it connects under.
    pub configuration_version: u32,
    pub account: ConnectedAccount,
    pub granted_scopes: Vec<String>,
    pub expires_at: Option<Timestamp>,
    /// Version 1 for a create; for a reconnect, the installation's stored
    /// version plus one (1 if it never held one).
    pub credential: NewCredential,
}

/// A link's random reference: 32 base62 characters, about 190 bits.
pub struct Reference(String);

const BASE62: &[u8; 62] = b"0123456789ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz";

impl Reference {
    pub fn generate() -> Result<Self, ConnectError> {
        let mut out = String::with_capacity(32);
        while out.len() < 32 {
            let mut bytes = [0u8; 48];
            getrandom::fill(&mut bytes).map_err(|_| ConnectError::Unavailable)?;
            // 248 is the largest multiple of 62 below 256: skipping above it keeps every character equally likely.
            for byte in bytes {
                if byte < 248 && out.len() < 32 {
                    out.push(char::from(BASE62[usize::from(byte % 62)]));
                }
            }
        }
        Ok(Self(out))
    }

    pub fn expose(&self) -> &str {
        &self.0
    }

    pub fn hash(&self) -> ReferenceHash {
        ReferenceHash::of(&self.0)
    }
}

impl std::fmt::Debug for Reference {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Reference(..)")
    }
}

/// What's stored of a reference: its SHA-256. A reference is random and long,
/// so an unsalted hash is enough to keep the database from holding links.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ReferenceHash(pub [u8; 32]);

impl ReferenceHash {
    /// The hash of a reference as a request presents it.
    pub fn of(reference: &str) -> Self {
        Self(Sha256::digest(reference.as_bytes()).into())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum ConnectError {
    #[error("the store is unavailable")]
    Unavailable,
    #[error("not found")]
    NotFound,
    /// The session isn't pending any more (or has expired).
    #[error("the session isn't pending")]
    NotPending,
    /// The installation a completion names isn't the one it can make or
    /// renew: a reconnect's installation is gone, or another credential
    /// version was stored since it was read.
    #[error("the installation changed")]
    InstallationChanged,
}

#[async_trait]
pub trait ConnectStore: Send + Sync {
    /// The organization's settings in one environment; the empty defaults
    /// until first written.
    async fn find_settings(
        &self,
        organization_id: OrganizationId,
        environment: Environment,
    ) -> Result<ConnectSettings, ConnectError>;

    async fn save_settings(
        &self,
        organization_id: OrganizationId,
        environment: Environment,
        return_urls: &[String],
        allowed_origins: &[String],
    ) -> Result<ConnectSettings, ConnectError>;

    /// `NotFound` unless the workspace is in the organization and in the
    /// session's environment.
    async fn create_session(
        &self,
        organization_id: OrganizationId,
        workspace_id: WorkspaceId,
        session_id: ConnectSessionId,
        session: &NewConnectSession,
        now: Timestamp,
    ) -> Result<ConnectSession, ConnectError>;

    /// The workspace's sessions, newest first.
    async fn list_sessions(
        &self,
        organization_id: OrganizationId,
        workspace_id: WorkspaceId,
    ) -> Result<Vec<ConnectSession>, ConnectError>;

    async fn find_session(
        &self,
        organization_id: OrganizationId,
        workspace_id: WorkspaceId,
        session_id: ConnectSessionId,
    ) -> Result<Option<ConnectSession>, ConnectError>;

    /// **Cross-tenant, by design:** the hosted page knows only its link's
    /// reference, so this is how a session's organization and workspace are
    /// learned. It reads one session by the hash of its reference and nothing
    /// else; every later statement carries the scope it returns.
    async fn attribute_session_by_reference(
        &self,
        reference_hash: &ReferenceHash,
    ) -> Result<Option<ConnectSession>, ConnectError>;

    /// Completes a pending, unexpired session, as one: for a create, a new
    /// connected installation of the session's app, with the session's label,
    /// which the session then names; for a reconnect, the session's
    /// installation connected again under the next credential version.
    /// `NotPending` when the session isn't pending or has expired at `now`,
    /// and `InstallationChanged` per [`ConnectError::InstallationChanged`];
    /// either way nothing is written.
    async fn complete_session(
        &self,
        organization_id: OrganizationId,
        workspace_id: WorkspaceId,
        session_id: ConnectSessionId,
        completion: &Completion,
        now: Timestamp,
    ) -> Result<(ConnectSession, Installation), ConnectError>;

    /// Cancels a pending, unexpired session. `NotPending` otherwise.
    async fn cancel_session(
        &self,
        organization_id: OrganizationId,
        workspace_id: WorkspaceId,
        session_id: ConnectSessionId,
        now: Timestamp,
    ) -> Result<ConnectSession, ConnectError>;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn references_are_long_random_and_base62() {
        let one = Reference::generate().unwrap_or_else(|_| unreachable!());
        let two = Reference::generate().unwrap_or_else(|_| unreachable!());
        assert_eq!(one.expose().len(), 32);
        assert!(one.expose().bytes().all(|b| b.is_ascii_alphanumeric()));
        assert_ne!(one.expose(), two.expose());
        assert_eq!(one.hash(), ReferenceHash::of(one.expose()));
        assert_ne!(one.hash(), two.hash());
        assert_eq!(format!("{one:?}"), "Reference(..)", "never printed");
    }
}
