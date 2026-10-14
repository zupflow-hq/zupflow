//! The tool registry: the tools an installation's MCP server lists to it,
//! pinned per installation. An installation lists through its own account's
//! credential, and what a server lists can depend on the account, so one
//! installation's listing is never another's, not even of the same app in
//! the same organization (no workspace reaches another).
//!
//! A tool is keyed by its name. The first definition seen is **pinned**: it
//! is what calls use and what the model reads. A later listing whose
//! definition hashes differently is kept beside it as the **latest**, marked
//! changed and unreviewed, and the pinned one stays in force until someone
//! accepts the change ([`ToolRegistry::accept_change`]). A tool missing
//! from a complete listing is **gone**. Nothing a server lists ever replaces
//! a pinned definition on its own.
//!
//! A definition is everything the model reads (name, title, description,
//! input schema) and the annotations, as canonical JSON, hashed with SHA-256.
//! The registry stores what it's given; building and hashing a definition is
//! the MCP client's job.

use async_trait::async_trait;

use crate::apps::ToolEffect;
use crate::audit::Audit;
use crate::ids::{InstallationId, OrganizationId, WorkspaceId};
use crate::time::Timestamp;

/// The most tools one listing records; a server listing more is refused.
pub const MAX_TOOLS: usize = 500;

/// The longest tool name kept.
pub const MAX_NAME: usize = 128;

/// The largest definition kept, in bytes of canonical JSON. Servers ship
/// tools whose descriptions and schemas run well past 64 KiB, and one tool
/// over the limit refuses its whole listing, so this is generous. It's the one
/// place to change it: the SQLite store holds only a far higher backstop
/// ([`MAX_DEFINITION_BYTES_CEILING`]) and the MCP client's page limit follows
/// from it.
pub const MAX_DEFINITION_BYTES: usize = 1 << 20;

/// What no store may keep, whatever [`MAX_DEFINITION_BYTES`] says: the
/// SQLite store's check is this, so raising the limit needs no migration up
/// to here.
pub const MAX_DEFINITION_BYTES_CEILING: usize = 16 << 20;

const _: () = assert!(MAX_DEFINITION_BYTES <= MAX_DEFINITION_BYTES_CEILING);

/// The most gone tools an installation keeps: beyond it, a listing deletes
/// those gone longest, so a server can't grow the registry without bound.
pub const MAX_GONE: usize = MAX_TOOLS;

/// The largest page [`ToolRegistry::list_tools`] answers.
pub const MAX_PAGE: u32 = 100;

/// One definition as listed: its canonical JSON and that JSON's SHA-256.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolRevision {
    pub definition: String,
    pub hash: [u8; 32],
}

/// One tool of a listing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ListedTool {
    pub name: String,
    pub revision: ToolRevision,
    /// Seeded from the server's annotations, used only when the tool is new.
    pub effect: ToolEffect,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ToolStatus {
    /// Listed last time, as pinned.
    Current,
    /// Listed last time with another definition, not reviewed yet.
    Changed,
    /// Missing from the last listing.
    Gone,
}

/// A tool as the registry holds it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RegisteredTool {
    pub name: String,
    pub effect: ToolEffect,
    pub pinned: ToolRevision,
    /// The unreviewed definition last listed, when it differs from the pinned one.
    pub latest: Option<ToolRevision>,
    pub first_seen_at: Timestamp,
    /// The last listing that named it.
    pub seen_at: Timestamp,
    /// When the latest definition was first listed.
    pub changed_at: Option<Timestamp>,
    pub gone_at: Option<Timestamp>,
}

impl RegisteredTool {
    pub fn status(&self) -> ToolStatus {
        if self.gone_at.is_some() {
            ToolStatus::Gone
        } else if self.latest.is_some() {
            ToolStatus::Changed
        } else {
            ToolStatus::Current
        }
    }
}

/// What a listing changed, by tool name, each sorted.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ListingChanges {
    pub added: Vec<String>,
    /// Newly changed, or changed again to another definition.
    pub changed: Vec<String>,
    pub gone: Vec<String>,
    /// Gone before, listed again.
    pub returned: Vec<String>,
}

impl ListingChanges {
    pub fn is_empty(&self) -> bool {
        self.added.is_empty() && self.changed.is_empty() && self.gone.is_empty() && self.returned.is_empty()
    }
}

/// One page of an installation's tools, by name.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolPage {
    pub items: Vec<RegisteredTool>,
    /// The last tool's name when more follow: the next page's `after`.
    pub next_after: Option<String>,
    /// How many tools the installation has in all.
    pub total: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum ToolError {
    #[error("the store is unavailable")]
    Unavailable,
    /// The installation isn't in this scope.
    #[error("not found")]
    NotFound,
    /// More than [`MAX_TOOLS`], a name repeated, empty or longer than
    /// [`MAX_NAME`], or a definition over [`MAX_DEFINITION_BYTES`].
    #[error("the listing can't be recorded")]
    InvalidListing,
    /// The installation has no tool of that name.
    #[error("tool not found")]
    ToolNotFound,
    /// The tool has no change to accept: it's current, or gone.
    #[error("the tool has no change to accept")]
    NotChanged,
    /// The tool's latest definition isn't the one reviewed.
    #[error("the tool changed again")]
    Stale,
}

#[async_trait]
pub trait ToolRegistry: Send + Sync {
    /// Records a complete listing of the installation's tools, in one transaction:
    /// new names pinned as listed; a known name whose hash differs from its
    /// pinned one kept as its latest; one listed as pinned cleared of any
    /// latest; every name not listed marked gone. Effects of known tools are
    /// never changed by a listing. Then, in the same transaction, gone tools
    /// beyond [`MAX_GONE`] are deleted, those gone longest first.
    /// Suggested catalog-app levels are saved in the same transaction;
    /// an existing level is never overwritten.
    async fn record_listing(
        &self,
        organization_id: OrganizationId,
        workspace_id: WorkspaceId,
        installation_id: InstallationId,
        listed: &[ListedTool],
        now: Timestamp,
    ) -> Result<ListingChanges, ToolError>;

    /// A page of the installation's tools, by name (byte order): those whose
    /// name sorts after `after`, at most `limit` (clamped to 1 to [`MAX_PAGE`]).
    async fn list_tools(
        &self,
        organization_id: OrganizationId,
        workspace_id: WorkspaceId,
        installation_id: InstallationId,
        after: Option<&str>,
        limit: u32,
    ) -> Result<ToolPage, ToolError>;

    /// Accepts a changed tool's latest definition, in one transaction: it
    /// becomes the pinned one and the tool is current again. In this order:
    /// the installation (`NotFound`), the tool (`ToolNotFound`), a change to
    /// accept (`NotChanged`: none, or the tool is gone), then the latest
    /// definition's hash against `expected`, the hash reviewed (`Stale`;
    /// `None`, for what can't be a hash, matches none). The tool's effect
    /// stays as first seen. Records `audit` of the tool as accepted.
    async fn accept_change(
        &self,
        organization_id: OrganizationId,
        workspace_id: WorkspaceId,
        installation_id: InstallationId,
        name: &str,
        expected: Option<[u8; 32]>,
        audit: &Audit<'_, RegisteredTool>,
    ) -> Result<RegisteredTool, ToolError>;
}

/// Checks a listing against the limits every registry holds it to.
pub fn validate_listing(listed: &[ListedTool]) -> Result<(), ToolError> {
    if listed.len() > MAX_TOOLS {
        return Err(ToolError::InvalidListing);
    }
    let mut names = std::collections::HashSet::new();
    for tool in listed {
        if tool.name.is_empty()
            || tool.name.chars().count() > MAX_NAME
            || tool.revision.definition.len() > MAX_DEFINITION_BYTES
            || !names.insert(tool.name.as_str())
        {
            return Err(ToolError::InvalidListing);
        }
    }
    Ok(())
}
