//! Types every part of the Zupflow backend shares.
//!
//! - [`ids`]: typed, prefixed identifiers (`run_…`) over UUIDv7.
//! - [`error`]: the API's error codes and the envelope's `error` field.
//! - [`environment`]: Sandbox and Production.
//! - [`keys`]: envelope encryption and the [`keys::KeyService`] trait.
//! - [`scope`]: the scope stored values belong to, and the names inside it.
//! - [`storage`]: the [`storage::Cache`] and [`storage::ObjectStore`] traits.

pub mod environment;
pub mod error;
pub mod ids;
pub mod keys;
pub mod scope;
pub mod storage;

/// Typed IDs make scope mistakes fail to compile. Swapping the organization
/// and the workspace doesn't compile:
///
/// ```compile_fail
/// use zupflow_core::ids::{OrganizationId, WorkspaceId};
/// fn scoped(_organization: OrganizationId, _workspace: WorkspaceId) {}
/// let (organization, workspace) = (OrganizationId::generate(), WorkspaceId::generate());
/// scoped(workspace, organization);
/// ```
///
/// Nor does passing another object's ID as a workspace's:
///
/// ```compile_fail
/// use zupflow_core::ids::{AgentId, WorkspaceId};
/// fn in_workspace(_workspace: WorkspaceId) {}
/// in_workspace(AgentId::generate());
/// ```
///
/// While the right arguments do:
///
/// ```
/// use zupflow_core::ids::{OrganizationId, WorkspaceId};
/// fn scoped(_organization: OrganizationId, _workspace: WorkspaceId) {}
/// scoped(OrganizationId::generate(), WorkspaceId::generate());
/// ```
pub mod scope_by_type {}
