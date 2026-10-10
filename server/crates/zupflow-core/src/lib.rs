//! Types every part of the Zupflow backend shares.
//!
//! - [`api_keys`]: API keys, their hashing and their store.
//! - [`apps`]: catalog apps and workspace apps, and their store.
//! - [`connect`]: Connect settings and sessions, and their store.
//! - [`ids`]: typed, prefixed identifiers (`run_…`) over UUIDv7.
//! - [`error`]: the API's error codes and the envelope's `error` field.
//! - [`environment`]: Sandbox and Production.
//! - [`keys`]: envelope encryption and the [`keys::KeyService`] trait.
//! - [`scope`]: the scope stored values belong to, and the names inside it.
//! - [`storage`]: the [`storage::Cache`] and [`storage::ObjectStore`] traits.
//! - [`events`]: the [`events::EventBus`] trait.
//! - [`jobs`]: the [`jobs::JobQueue`] trait.
//! - [`idempotency`]: idempotency keys for `POST`, and their store.
//! - [`identity`]: Console sign-in, the [`identity::Identity`] trait.
//! - [`installations`]: connected accounts, their sealed credentials and their store.
//! - [`outbound`]: the one path every outbound call takes, and which addresses are public.
//! - [`organizations`]: organizations, members and users, and their store.
//! - [`time`]: instants as stored and as the API writes them.
//! - [`tools`]: the tools apps' MCP servers list, pinned, and their registry.
//! - [`workspaces`]: workspaces and their store.

pub mod api_keys;
pub mod apps;
pub mod connect;
pub mod environment;
pub mod error;
pub mod events;
pub mod idempotency;
pub mod identity;
pub mod ids;
pub mod installations;
pub mod jobs;
pub mod keys;
pub mod organizations;
pub mod outbound;
pub mod scope;
pub mod storage;
pub mod time;
pub mod tools;
pub mod workspaces;

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
