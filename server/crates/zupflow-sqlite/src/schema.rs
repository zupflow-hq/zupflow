//! The schema test: every table is declared here with how it's scoped, and
//! the migrated database is held to the tenant-isolation rules. A new table
//! that isn't declared fails the test, so nobody adds one without deciding.

#![allow(clippy::unwrap_used)]

use std::collections::HashSet;
use std::path::PathBuf;

use sqlx::Row;

use crate::Database;

enum Scoping {
    /// The root every scoped row hangs from: `organizations`.
    Root,
    /// Starts `id UUID NOT NULL PRIMARY KEY, organization_id UUID NOT NULL`.
    Organization,
    /// Starts `id UUID NOT NULL PRIMARY KEY, organization_id UUID NOT NULL, workspace_id UUID NOT NULL`.
    #[allow(dead_code)]
    Workspace,
    /// Not tenant data, declared by name: users, who belong to many
    /// organizations, the identities they sign in with, and the installation's
    /// own settings. Holds no
    /// organization or workspace column and references no scoped table.
    Global,
}

const TABLES: &[(&str, Scoping)] = &[
    ("organizations", Scoping::Root),
    ("workspaces", Scoping::Organization),
    ("jobs", Scoping::Organization),
    ("users", Scoping::Global),
    ("user_identities", Scoping::Global),
    ("members", Scoping::Organization),
    ("api_keys", Scoping::Organization),
    ("instance_settings", Scoping::Global),
    ("idempotency_keys", Scoping::Organization),
    // A user's own keys for creating an organization, before there is one.
    ("user_idempotency_keys", Scoping::Global),
    ("apps", Scoping::Organization),
    ("workspace_apps", Scoping::Workspace),
    ("app_configurations", Scoping::Organization),
    ("workspace_app_configurations", Scoping::Workspace),
    ("installations", Scoping::Workspace),
    ("installation_credentials", Scoping::Workspace),
    ("connect_settings", Scoping::Organization),
    ("connect_sessions", Scoping::Workspace),
    ("connect_authorizations", Scoping::Workspace),
    ("installation_tools", Scoping::Workspace),
    ("oauth_client_registrations", Scoping::Organization),
    ("workspace_oauth_client_registrations", Scoping::Workspace),
    ("agents", Scoping::Workspace),
    ("agent_grants", Scoping::Workspace),
    ("tool_policies", Scoping::Organization),
    ("app_tool_levels", Scoping::Organization),
    ("runs", Scoping::Workspace),
    ("run_events", Scoping::Workspace),
    ("run_tool_calls", Scoping::Workspace),
    // An event names its workspace, except the test button's, which has none.
    ("webhook_endpoints", Scoping::Organization),
    ("webhook_events", Scoping::Organization),
    ("webhook_deliveries", Scoping::Organization),
];

struct Column {
    name: String,
    declared_type: String,
    not_null: bool,
    primary_key: bool,
}

/// One foreign key: the parent table and its (child, parent) column pairs.
struct ForeignKey {
    parent: String,
    columns: Vec<(String, String)>,
}

/// A database in a directory of its own, removed with everything in it
/// (the WAL and shared-memory files too) when the test is done.
pub(crate) struct Temporary {
    pub(crate) database: Database,
    pub(crate) directory: TemporaryDirectory,
}

pub(crate) struct TemporaryDirectory(pub(crate) PathBuf);

impl TemporaryDirectory {
    pub(crate) fn new() -> Self {
        let path = std::env::temp_dir().join(format!("zupflow-db-{}", uuid::Uuid::now_v7().simple()));
        std::fs::create_dir(&path).unwrap();
        Self(path)
    }

    pub(crate) fn file(&self) -> PathBuf {
        self.0.join("zupflow.sqlite")
    }
}

impl Drop for TemporaryDirectory {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

pub(crate) async fn temporary() -> Temporary {
    let directory = TemporaryDirectory::new();
    let database = Database::open(&directory.file()).await.unwrap();
    Temporary { database, directory }
}

async fn tables(database: &Database) -> Vec<String> {
    sqlx::query_scalar(
        "SELECT name FROM sqlite_schema WHERE type = 'table' AND name NOT LIKE 'sqlite_%' \
         AND name != '_sqlx_migrations' ORDER BY name",
    )
    .fetch_all(database.pool())
    .await
    .unwrap()
}

async fn columns(database: &Database, table: &str) -> Vec<Column> {
    sqlx::query("SELECT name, type, \"notnull\", pk FROM pragma_table_info(?) ORDER BY cid")
        .bind(table)
        .fetch_all(database.pool())
        .await
        .unwrap()
        .iter()
        .map(|row| Column {
            name: row.get("name"),
            declared_type: row.get("type"),
            not_null: row.get::<i64, _>("notnull") == 1,
            primary_key: row.get::<i64, _>("pk") > 0,
        })
        .collect()
}

async fn foreign_keys(database: &Database, table: &str) -> Vec<ForeignKey> {
    let rows = sqlx::query("SELECT id, \"table\", \"from\", \"to\" FROM pragma_foreign_key_list(?) ORDER BY id, seq")
        .bind(table)
        .fetch_all(database.pool())
        .await
        .unwrap();
    let mut keys: Vec<(i64, ForeignKey)> = Vec::new();
    for row in rows {
        let id: i64 = row.get("id");
        let pair = (row.get("from"), row.get("to"));
        match keys.last_mut() {
            Some((last, key)) if *last == id => key.columns.push(pair),
            _ => keys.push((
                id,
                ForeignKey {
                    parent: row.get("table"),
                    columns: vec![pair],
                },
            )),
        }
    }
    keys.into_iter().map(|(_, key)| key).collect()
}

fn starts_with(table: &str, columns: &[Column], expected: &[&str]) {
    let names: Vec<&str> = columns.iter().map(|column| column.name.as_str()).collect();
    assert!(
        names.starts_with(expected),
        "{table} must start with exactly {expected:?}, starts {names:?}"
    );
    for column in &columns[..expected.len()] {
        assert_eq!(column.declared_type, "UUID", "{table}.{} is UUID", column.name);
        // Declared, not implied: SQLite lets a PRIMARY KEY column hold NULL.
        assert!(column.not_null, "{table}.{} is declared NOT NULL", column.name);
    }
    assert!(columns[0].primary_key, "{table}.id is the primary key");
    assert_eq!(
        columns.iter().filter(|column| column.primary_key).count(),
        1,
        "{table} has `id` alone as its primary key"
    );
}

fn has_key(keys: &[ForeignKey], parent: &str, pairs: &[(&str, &str)]) -> bool {
    keys.iter().any(|key| {
        key.parent == parent
            && key.columns.len() == pairs.len()
            && pairs
                .iter()
                .all(|(from, to)| key.columns.iter().any(|(f, t)| f == from && t == to))
    })
}

#[tokio::test]
async fn holds_every_table_to_the_isolation_rules() {
    let Temporary {
        database,
        directory: _directory,
    } = temporary().await;
    let declared: HashSet<&str> = TABLES.iter().map(|(name, _)| *name).collect();
    let actual = tables(&database).await;
    for table in &actual {
        assert!(declared.contains(table.as_str()), "{table} isn't declared in schema.rs");
    }
    assert_eq!(actual.len(), TABLES.len(), "every declared table exists");

    let global: HashSet<&str> = TABLES
        .iter()
        .filter(|(_, scoping)| matches!(scoping, Scoping::Global))
        .map(|(name, _)| *name)
        .collect();
    let workspace_scoped: HashSet<&str> = TABLES
        .iter()
        .filter(|(_, scoping)| matches!(scoping, Scoping::Workspace))
        .map(|(name, _)| *name)
        .collect();

    for (table, scoping) in TABLES {
        let columns = columns(&database, table).await;
        let keys = foreign_keys(&database, table).await;
        match scoping {
            Scoping::Root => starts_with(table, &columns, &["id"]),
            Scoping::Global => {
                for column in &columns {
                    assert!(
                        column.name != "organization_id" && column.name != "workspace_id",
                        "{table} is global, so it holds no {}",
                        column.name
                    );
                }
                for key in &keys {
                    assert!(
                        global.contains(key.parent.as_str()),
                        "{table} is global, so it references only global tables, not {}",
                        key.parent
                    );
                }
            }
            Scoping::Organization => {
                starts_with(table, &columns, &["id", "organization_id"]);
                assert!(
                    has_key(&keys, "organizations", &[("organization_id", "id")]),
                    "{table}.organization_id references organizations"
                );
            }
            Scoping::Workspace => starts_with(table, &columns, &["id", "organization_id", "workspace_id"]),
        }

        // A workspace column, required or not, always references the
        // workspace together with its organization.
        if columns.iter().any(|column| column.name == "workspace_id") {
            assert!(
                has_key(
                    &keys,
                    "workspaces",
                    &[("organization_id", "organization_id"), ("workspace_id", "id")]
                ),
                "{table} references workspaces by (organization_id, workspace_id)"
            );
        }

        // No reference crosses a boundary: every key to a scoped table other
        // than the root carries the organization, and the workspace too when
        // the parent is workspace-scoped.
        for key in &keys {
            if key.parent == "organizations" || global.contains(key.parent.as_str()) {
                continue;
            }
            assert!(
                key.columns
                    .iter()
                    .any(|(from, to)| from == "organization_id" && to == "organization_id"),
                "{table} → {} carries organization_id",
                key.parent
            );
            if workspace_scoped.contains(key.parent.as_str()) {
                assert!(
                    key.columns
                        .iter()
                        .any(|(from, to)| from == "workspace_id" && to == "workspace_id"),
                    "{table} → {} carries workspace_id",
                    key.parent
                );
            }
        }
    }
}

#[tokio::test]
async fn enforces_foreign_keys() {
    let Temporary {
        database,
        directory: _directory,
    } = temporary().await;
    let enabled: i64 = sqlx::query_scalar("PRAGMA foreign_keys")
        .fetch_one(database.pool())
        .await
        .unwrap();
    assert_eq!(enabled, 1);
}

#[cfg(unix)]
#[tokio::test]
async fn creates_the_file_for_its_owner_only() {
    use std::os::unix::fs::PermissionsExt;
    let Temporary {
        database: _database,
        directory,
    } = temporary().await;
    let mode = std::fs::metadata(directory.file()).unwrap().permissions().mode();
    assert_eq!(mode & 0o777, 0o600);
}

/// A migration already applied is never edited: the migrator refuses a
/// database whose record of one doesn't match the file.
#[tokio::test]
async fn refuses_a_migration_changed_after_it_was_applied() {
    let Temporary { database, directory } = temporary().await;
    sqlx::query("UPDATE _sqlx_migrations SET checksum = x'00' WHERE version = 1")
        .execute(database.pool())
        .await
        .unwrap();
    database.pool().close().await;
    assert!(matches!(
        Database::open(&directory.file()).await,
        Err(crate::DatabaseError::Migrate(_))
    ));
}
