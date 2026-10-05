//! Zupflow's basic database: SQLite, embedded, in one file.
//!
//! - [`Database`]: opens the file, sets it up and applies the migrations.
//! - [`SqliteJobQueue`]: the job queue, in the same database.
//!
//! Migrations are in `migrations/`, applied in order at open and never edited
//! once merged: a change is a new migration. The schema test in `schema.rs`
//! holds every table to the tenant-isolation rules.

mod database;
mod job_queue;
#[cfg(test)]
mod schema;

pub use database::{Database, DatabaseError};
pub use job_queue::SqliteJobQueue;
