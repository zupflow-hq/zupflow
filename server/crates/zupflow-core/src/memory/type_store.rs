//! Memory types and the organization's default.
//!
//! A memory type is one organization's named setup of a kind: its settings,
//! as the kind checked them with its defaults filled in, and whether its
//! agents may write ([`MemoryWrites`]). Types and the default are the
//! organization's, the same in both environments. Memory is off until an
//! organization chooses a default: an agent on the default runs without
//! memory while there's none.
//!
//! Owners and admins change them, from the Console only. Every change takes
//! the acting member and checks, inside its own transaction, that they're
//! still an active owner or admin. The rules that hold across concurrent
//! changes are checked there too (a name is the organization's once, a type
//! the default or an agent names isn't deleted, a default is one of the
//! organization's types), and each change records its [`Audit`] there.
//!
//! An agent's memory is the agent's own ([`crate::agents::Agent::memory`]),
//! checked by the agent store in the transaction that writes the agent: a
//! type it names is one of its organization's.

use async_trait::async_trait;
use serde_json::{Map, Value};

use super::{MemoryKind, MemoryWrites, Settings};
use crate::audit::Audit;
use crate::ids::{AgentId, MemberId, MemoryTypeId, OrganizationId, WorkspaceId};
use crate::model_store::HoldingAgent;
use crate::organizations::User;
use crate::time::Timestamp;
use crate::workspaces::Page;

/// The longest type name, in characters.
pub const MAX_NAME: usize = 60;

/// The most types one page holds, and how many it holds by default.
pub const MAX_PAGE: u32 = 100;
pub const DEFAULT_PAGE: u32 = 25;

/// How many agents a refusal names before it counts the rest.
pub use crate::model_store::NAMED_HOLDERS;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MemoryType {
    pub id: MemoryTypeId,
    pub organization_id: OrganizationId,
    /// 1 to 60 characters, the organization's once.
    pub name: String,
    /// Fixed once the type is created.
    pub kind: MemoryKind,
    /// As the kind checked them when they were written, defaults filled in.
    pub settings: Map<String, Value>,
    pub writes: MemoryWrites,
    /// Agents that name it, counted when read; agents on the default aren't.
    pub agent_count: u64,
    /// The organization's default, read with it.
    pub is_default: bool,
    pub created_at: Timestamp,
    pub updated_at: Timestamp,
}

/// A type to create, checked by the caller: its settings by its kind.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NewMemoryType {
    pub name: String,
    pub kind: MemoryKind,
    pub settings: Settings,
    pub writes: MemoryWrites,
}

/// What an update changes; `None` keeps the stored value. A type's kind
/// never changes.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct MemoryTypeChanges {
    pub name: Option<String>,
    /// Checked by the type's kind.
    pub settings: Option<Settings>,
    pub writes: Option<MemoryWrites>,
}

/// The contract's `DefaultMemoryType`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DefaultMemoryType {
    pub memory_type: Option<MemoryType>,
    pub updated_at: Option<Timestamp>,
    /// `None` before anyone chose, or once the person is gone.
    pub updated_by: Option<User>,
}

/// What keeps a type from being deleted.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct MemoryTypeHolders {
    /// It's the organization's default.
    pub is_default: bool,
    /// The first [`NAMED_HOLDERS`] agents naming it, oldest first, by the
    /// names people know.
    pub agents: Vec<HoldingAgent>,
    /// How many agents in all.
    pub agent_total: u64,
}

impl MemoryTypeHolders {
    pub fn is_empty(&self) -> bool {
        !self.is_default && self.agent_total == 0
    }
}

/// The memory a run of an agent starts with: its type, and what the type
/// says.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RunMemory {
    pub memory_type_id: MemoryTypeId,
    pub kind: MemoryKind,
    /// As stored; the kind checks them again before it reads them.
    pub settings: Map<String, Value>,
    pub writes: MemoryWrites,
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum MemoryTypeError {
    #[error("the store is unavailable")]
    Unavailable,
    /// The type isn't the organization's.
    #[error("memory type not found")]
    NotFound,
    /// The member can no longer change memory types.
    #[error("the caller can no longer manage memory types")]
    NotAllowed,
    /// Another of the organization's types has the name.
    #[error("the organization has a memory type of that name")]
    NameTaken,
    /// A default naming no type of the organization.
    #[error("not a memory type of this organization")]
    UnknownType,
    /// The type is the default, or agents name it.
    #[error("the memory type is in use")]
    InUse(MemoryTypeHolders),
    #[error("not a cursor from this list")]
    InvalidCursor,
}

/// An organization's memory types and default. Every method takes the
/// organization first and reaches nothing outside it.
#[async_trait]
pub trait MemoryTypeStore: Send + Sync {
    /// A page of the organization's types, by name (any case), then ID. A
    /// cursor is only a position in that order (`InvalidCursor` when it
    /// isn't one): another organization's pages this organization's types
    /// from that position.
    async fn list_types(
        &self,
        organization_id: OrganizationId,
        cursor: Option<&str>,
        limit: u32,
    ) -> Result<Page<MemoryType>, MemoryTypeError>;

    async fn find_type(
        &self,
        organization_id: OrganizationId,
        memory_type_id: MemoryTypeId,
    ) -> Result<Option<MemoryType>, MemoryTypeError>;

    /// Creates a type, its name the organization's once (`NameTaken`).
    async fn create_type(
        &self,
        organization_id: OrganizationId,
        memory_type_id: MemoryTypeId,
        manager: MemberId,
        memory_type: &NewMemoryType,
        now: Timestamp,
        audit: &Audit<'_, MemoryType>,
    ) -> Result<MemoryType, MemoryTypeError>;

    /// Changes what `changes` names, a name held to the rule
    /// [`MemoryTypeStore::create_type`] holds it to.
    #[allow(clippy::too_many_arguments)]
    async fn update_type(
        &self,
        organization_id: OrganizationId,
        memory_type_id: MemoryTypeId,
        manager: MemberId,
        changes: &MemoryTypeChanges,
        now: Timestamp,
        audit: &Audit<'_, MemoryType>,
    ) -> Result<MemoryType, MemoryTypeError>;

    /// Deletes the type, recording `audit` of it as it was. `InUse` while
    /// it's the default or an agent names it. Its memories kept in the same
    /// database go with it, in the same transaction, which queues the job
    /// that has its kind forget them wherever it keeps them
    /// ([`super::Forget::job`]).
    async fn delete_type(
        &self,
        organization_id: OrganizationId,
        memory_type_id: MemoryTypeId,
        manager: MemberId,
        audit: &Audit<'_, MemoryType>,
    ) -> Result<(), MemoryTypeError>;

    async fn default_type(&self, organization_id: OrganizationId) -> Result<DefaultMemoryType, MemoryTypeError>;

    /// Sets the organization's default to one of its types (`UnknownType`
    /// otherwise), or clears it with `None`, as written by `manager`.
    async fn set_default_type(
        &self,
        organization_id: OrganizationId,
        manager: MemberId,
        memory_type_id: Option<MemoryTypeId>,
        now: Timestamp,
        audit: &Audit<'_, DefaultMemoryType>,
    ) -> Result<DefaultMemoryType, MemoryTypeError>;

    /// The memory a new run of the agent uses: its own type, or else the
    /// organization's default. `None` when its memory is off, when it's on
    /// the default and there's none, or when the agent isn't in this
    /// workspace.
    async fn run_memory(
        &self,
        organization_id: OrganizationId,
        workspace_id: WorkspaceId,
        agent_id: AgentId,
    ) -> Result<Option<RunMemory>, MemoryTypeError>;
}
