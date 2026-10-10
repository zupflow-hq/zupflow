//! Zupflow's basic database: SQLite, embedded, in one file.
//!
//! - [`Database`]: opens the file, sets it up and applies the migrations.
//! - [`SqliteAgentStore`]: agents and their grants, held to the tool registry.
//! - [`SqliteApiKeyStore`]: API keys, by keyed hash.
//! - [`SqliteAppStore`]: catalog apps and workspace apps.
//! - [`SqliteConnectStore`]: Connect settings and sessions.
//! - [`SqliteIdempotencyStore`]: idempotency keys for `POST`.
//! - [`SqliteInstallationStore`]: installations and their sealed credentials.
//! - [`SqliteJobQueue`]: the job queue, in the same database.
//! - [`SqliteOrganizationStore`]: organizations, members and users.
//! - [`SqliteToolRegistry`]: the tools apps' MCP servers list, pinned.
//! - [`SqliteWorkspaceStore`]: workspaces, listed by keyset.
//!
//! Migrations are in `migrations/`, applied in order at open and never edited
//! once merged: a change is a new migration. The schema test in `schema.rs`
//! holds every table to the tenant-isolation rules.

mod agents;
mod api_keys;
mod apps;
mod connect;
mod database;
mod idempotency;
mod installations;
mod job_queue;
mod organizations;
#[cfg(test)]
mod schema;
mod tools;
mod workspaces;

pub use agents::SqliteAgentStore;
pub use api_keys::SqliteApiKeyStore;
pub use apps::SqliteAppStore;
pub use connect::SqliteConnectStore;
pub use database::{Database, DatabaseError};
pub use idempotency::SqliteIdempotencyStore;
pub use installations::SqliteInstallationStore;
pub use job_queue::SqliteJobQueue;
pub use organizations::SqliteOrganizationStore;
pub use tools::SqliteToolRegistry;
pub use workspaces::SqliteWorkspaceStore;
