//! Zupflow's basic database: SQLite, embedded, in one file.
//!
//! - [`Database`]: opens the file, sets it up and applies the migrations.
//! - [`SqliteApiKeyStore`]: API keys, by keyed hash.
//! - [`SqliteJobQueue`]: the job queue, in the same database.
//! - [`SqliteOrganizationStore`]: organizations, members and users.
//! - [`SqliteWorkspaceStore`]: workspaces, listed by keyset.
//!
//! Migrations are in `migrations/`, applied in order at open and never edited
//! once merged: a change is a new migration. The schema test in `schema.rs`
//! holds every table to the tenant-isolation rules.

mod api_keys;
mod database;
mod job_queue;
mod organizations;
#[cfg(test)]
mod schema;
mod workspaces;

pub use api_keys::SqliteApiKeyStore;
pub use database::{Database, DatabaseError};
pub use job_queue::SqliteJobQueue;
pub use organizations::SqliteOrganizationStore;
pub use workspaces::SqliteWorkspaceStore;
