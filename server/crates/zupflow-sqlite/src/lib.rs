//! Zupflow's basic database: SQLite, embedded, in one file.
//!
//! - [`Database`]: opens the file, sets it up and applies the migrations.
//! - [`SqliteAgentStore`]: agents and their grants, held to the tool registry.
//! - [`SqliteApiKeyStore`]: API keys, by keyed hash.
//! - [`SqliteAuditStore`]: the audit log, which every store writes with the change it records.
//! - [`SqliteAppStore`]: catalog apps and workspace apps.
//! - [`SqliteConnectStore`]: Connect settings and sessions.
//! - [`SqliteDurableRunStore`]: runs as the durable engine advances them,
//!   one transition at a time.
//! - [`SqliteIdempotencyStore`]: idempotency keys for `POST`.
//! - [`SqliteInstallationStore`]: installations and their sealed credentials.
//! - [`SqliteJobQueue`]: the job queue, in the same database.
//! - [`SqliteOrganizationStore`]: organizations, members and users.
//! - [`SqliteRunStore`]: runs, their timelines and their tool calls.
//! - [`SqliteToolPolicyStore`]: the organization's tool policy and its levels per catalog app.
//! - [`SqliteToolRegistry`]: the tools apps' MCP servers list, pinned.
//! - [`SqliteWebhookStore`]: webhook endpoints, their events and deliveries.
//! - [`SqliteWorkspaceStore`]: workspaces, listed by keyset.
//!
//! Migrations are in `migrations/`, applied in order at open and never edited
//! once merged: a change is a new migration. The schema test in `schema.rs`
//! holds every table to the tenant-isolation rules.

mod agents;
mod api_keys;
mod apps;
mod audit;
mod connect;
mod database;
mod durable_runs;
mod idempotency;
mod installations;
mod job_queue;
mod organizations;
mod runs;
#[cfg(test)]
mod schema;
mod tool_policy;
mod tools;
mod webhooks;
mod workspaces;

pub use agents::SqliteAgentStore;
pub use api_keys::SqliteApiKeyStore;
pub use apps::SqliteAppStore;
pub use audit::SqliteAuditStore;
pub use connect::SqliteConnectStore;
pub use database::{Database, DatabaseError};
pub use durable_runs::SqliteDurableRunStore;
pub use idempotency::SqliteIdempotencyStore;
pub use installations::SqliteInstallationStore;
pub use job_queue::SqliteJobQueue;
pub use organizations::SqliteOrganizationStore;
pub use runs::SqliteRunStore;
pub use tool_policy::SqliteToolPolicyStore;
pub use tools::SqliteToolRegistry;
pub use webhooks::SqliteWebhookStore;
pub use workspaces::SqliteWorkspaceStore;
