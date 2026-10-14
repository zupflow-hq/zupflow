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
    /// Organization-scoped history (decision: keep history): starts like
    /// `Organization`, and its `workspace_id` names a workspace without
    /// referencing it, so a row outlives the workspace it happened in. The
    /// one exception to "a workspace column references workspaces"; the
    /// organization still always applies, and its rows go with it.
    History,
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
    // Every short ID issued in a workspace, kept after its installation goes.
    ("installation_short_ids", Scoping::Workspace),
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
    ("run_transitions", Scoping::Workspace),
    ("run_turns", Scoping::Workspace),
    ("run_model_attempts", Scoping::Workspace),
    ("run_call_attempts", Scoping::Workspace),
    // An event names its workspace, except the test button's, which has none.
    ("webhook_endpoints", Scoping::Organization),
    ("webhook_events", Scoping::Organization),
    ("webhook_deliveries", Scoping::Organization),
    ("webhook_delivery_attempts", Scoping::Organization),
    // Model providers: an organization's connections, their sealed keys, the
    // models added from them and each environment's default.
    ("model_connections", Scoping::Organization),
    ("model_connection_credentials", Scoping::Organization),
    ("models", Scoping::Organization),
    ("default_models", Scoping::Organization),
    // A deleted workspace's events stay in the log.
    ("audit_events", Scoping::History),
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
    on_delete: String,
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
    let rows = sqlx::query(
        "SELECT id, \"table\", \"from\", \"to\", on_delete FROM pragma_foreign_key_list(?) ORDER BY id, seq",
    )
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
                    on_delete: row.get("on_delete"),
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
    // Keeping history is a decision, not a convenience: one table does.
    let history: HashSet<&str> = TABLES
        .iter()
        .filter(|(_, scoping)| matches!(scoping, Scoping::History))
        .map(|(name, _)| *name)
        .collect();
    assert_eq!(
        history,
        HashSet::from(["audit_events"]),
        "only the audit log keeps history"
    );
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
            Scoping::Organization | Scoping::History => {
                starts_with(table, &columns, &["id", "organization_id"]);
                assert!(
                    has_key(&keys, "organizations", &[("organization_id", "id")]),
                    "{table}.organization_id references organizations"
                );
            }
            Scoping::Workspace => starts_with(table, &columns, &["id", "organization_id", "workspace_id"]),
        }

        // History names a workspace and never references it, so nothing
        // it holds goes when the workspace does.
        if matches!(scoping, Scoping::History) {
            let workspace = columns.iter().find(|column| column.name == "workspace_id");
            assert_eq!(
                workspace.map(|column| column.declared_type.as_str()),
                Some("UUID"),
                "{table} keeps history of workspaces, so it names one by UUID"
            );
            assert!(
                keys.iter().all(|key| key.parent == "organizations"),
                "{table} keeps history: it references its organization only"
            );
            assert!(
                keys.iter().all(|key| key.on_delete == "CASCADE"),
                "{table}'s rows go with their organization"
            );
        }
        // Otherwise a workspace column, required or not, always references
        // the workspace together with its organization.
        else if columns.iter().any(|column| column.name == "workspace_id") {
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

/// Migration 0027 rebuilds `apps`, `workspace_apps` and `installations` with
/// foreign keys off: what other tables hold for them stays, every slug
/// becomes a key, and every installation gets a short ID, recorded in its
/// workspace. Foreign keys are on again once it's done.
#[tokio::test]
async fn keeps_what_references_apps_and_installations_through_migration_0027() {
    use std::str::FromStr;

    use sqlx::sqlite::SqliteConnectOptions;

    let directory = TemporaryDirectory::new();
    let earlier = directory.0.join("migrations");
    std::fs::create_dir(&earlier).unwrap();
    let source = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("migrations");
    for entry in std::fs::read_dir(&source).unwrap() {
        let path = entry.unwrap().path();
        let name = path.file_name().unwrap().to_string_lossy().into_owned();
        if name.as_str() < "0027" {
            std::fs::copy(&path, earlier.join(&name)).unwrap();
        }
    }
    let options = SqliteConnectOptions::from_str("sqlite:")
        .unwrap()
        .filename(directory.file())
        .create_if_missing(true)
        .foreign_keys(true);
    let pool = sqlx::SqlitePool::connect_with(options).await.unwrap();
    sqlx::migrate::Migrator::new(earlier.as_path())
        .await
        .unwrap()
        .run(&pool)
        .await
        .unwrap();
    let hex = |bytes: &[u8]| bytes.iter().map(|byte| format!("{byte:02x}")).collect::<String>();
    let blob = |n: u8| {
        let mut id = [0u8; 16];
        id[15] = n;
        id.to_vec()
    };
    for statement in [
        "INSERT INTO organizations (id, name, slug, created_at) VALUES (?1, 'Org', 'org', 0)",
        "INSERT INTO workspaces (id, organization_id, environment, name, slug, created_at) \
         VALUES (?2, ?1, 'sandbox', 'W', 'w', 0)",
        "INSERT INTO apps (id, organization_id, kind, name, slug, description, icon_glyph, icon_color, auth, tools, \
         version, created_at, updated_at) \
         VALUES (?3, ?1, 'mcp', 'G', 'google-workspace', '', 'G', '#0E7490', '{}', '[]', 1, 0, 0)",
        "INSERT INTO apps (id, organization_id, kind, name, slug, description, icon_glyph, icon_color, auth, tools, \
         version, created_at, updated_at) \
         VALUES (?4, ?1, 'mcp', 'N', '9-lives', '', 'N', '#0E7490', '{}', '[]', 1, 0, 0)",
        "INSERT INTO workspace_apps (id, organization_id, workspace_id, kind, name, slug, description, icon_glyph, \
         icon_color, auth, tools, version, created_at, updated_at) \
         VALUES (?5, ?1, ?2, 'mcp', 'G', 'google-workspace', '', 'G', '#0E7490', '{}', '[]', 1, 0, 0)",
        "INSERT INTO installations (id, organization_id, workspace_id, catalog_app_id, label, status, \
         configuration_version, granted_scopes, created_at, updated_at) \
         VALUES (?6, ?1, ?2, ?3, 'Main', 'disconnected', 1, '[]', 0, 0)",
        "INSERT INTO installations (id, organization_id, workspace_id, workspace_app_id, label, status, \
         configuration_version, granted_scopes, created_at, updated_at) \
         VALUES (?7, ?1, ?2, ?5, 'Own', 'disconnected', 1, '[]', 0, 0)",
        "INSERT INTO installation_credentials (id, organization_id, workspace_id, installation_id, version, key_id, \
         wrapped_key, nonce, ciphertext, created_at) \
         VALUES (?8, ?1, ?2, ?6, 1, 'k', x'00', x'000000000000000000000000', x'00', 0)",
        "INSERT INTO app_tool_levels (id, organization_id, app_id, name, level, confirmed, destructive, updated_at) \
         VALUES (?9, ?1, ?3, 'search', 'allow', 1, 0, 0)",
        // Two workspaces held the same slug; the older keeps it as a key.
        "INSERT INTO workspaces (id, organization_id, environment, name, slug, created_at) \
         VALUES (?10, ?1, 'sandbox', 'V', 'v', 0)",
        "INSERT INTO workspace_apps (id, organization_id, workspace_id, kind, name, slug, description, icon_glyph, \
         icon_color, auth, tools, version, created_at, updated_at) \
         VALUES (?11, ?1, ?2, 'mcp', 'T', 'tools', '', 'T', '#0E7490', '{}', '[]', 1, 1, 1)",
        "INSERT INTO workspace_apps (id, organization_id, workspace_id, kind, name, slug, description, icon_glyph, \
         icon_color, auth, tools, version, created_at, updated_at) \
         VALUES (?12, ?1, ?10, 'mcp', 'T', 'tools', '', 'T', '#0E7490', '{}', '[]', 1, 2, 2)",
        "INSERT INTO installations (id, organization_id, workspace_id, workspace_app_id, label, status, \
         configuration_version, granted_scopes, created_at, updated_at) \
         VALUES (?13, ?1, ?10, ?12, 'Other', 'disconnected', 1, '[]', 0, 0)",
    ] {
        sqlx::query(statement)
            .bind(blob(1))
            .bind(blob(2))
            .bind(blob(3))
            .bind(blob(4))
            .bind(blob(5))
            .bind(blob(6))
            .bind(blob(7))
            .bind(blob(8))
            .bind(blob(9))
            .bind(blob(10))
            .bind(blob(11))
            .bind(blob(12))
            .bind(blob(13))
            .execute(&pool)
            .await
            .unwrap_or_else(|error| panic!("{statement}: {error}"));
    }
    pool.close().await;

    let database = Database::open(&directory.file()).await.unwrap();
    let pool = database.pool();
    let catalog: Vec<String> = sqlx::query_scalar("SELECT key FROM apps ORDER BY id")
        .fetch_all(pool)
        .await
        .unwrap();
    assert_eq!(catalog[0], "google_workspace", "hyphens become underscores");
    assert!(
        catalog[1].starts_with("app_") && zupflow_core::apps::is_app_key(&catalog[1]),
        "a slug that makes no key gets one: {}",
        catalog[1]
    );
    let own: Vec<String> = sqlx::query_scalar("SELECT key FROM workspace_apps ORDER BY id")
        .fetch_all(pool)
        .await
        .unwrap();
    assert_eq!(
        own[0],
        format!("app_{}", &hex(&blob(5))[12..]),
        "a workspace app never repeats a catalog key"
    );
    assert_eq!(own[1], "tools", "the oldest of a slug two workspaces held keeps it");
    assert_eq!(
        own[2],
        format!("app_{}", &hex(&blob(12))[12..]),
        "a younger one gets its own"
    );
    for key in &own {
        assert!(zupflow_core::apps::is_app_key(key), "{key}");
    }
    // Short IDs follow each workspace's order, through a bijection, so none collide.
    let short_ids: Vec<String> = sqlx::query_scalar("SELECT short_id FROM installations ORDER BY id")
        .fetch_all(pool)
        .await
        .unwrap();
    let spelled = |n: u32| zupflow_core::installations::ShortId::from_bits(n.wrapping_mul(625_341_585) & 0x3FFF_FFFF);
    assert_eq!(
        short_ids,
        [spelled(0).as_str(), spelled(1).as_str(), spelled(0).as_str()],
        "first and second in one workspace, first in the other"
    );
    let recorded: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM installation_short_ids s JOIN installations i \
         ON i.organization_id = s.organization_id AND i.workspace_id = s.workspace_id AND i.short_id = s.short_id",
    )
    .fetch_one(pool)
    .await
    .unwrap();
    assert_eq!(recorded, 3, "each short ID recorded in its workspace");
    for (table, expected) in [("installation_credentials", 1), ("app_tool_levels", 1)] {
        let count: i64 = sqlx::query_scalar(sqlx::AssertSqlSafe(format!("SELECT count(*) FROM {table}")))
            .fetch_one(pool)
            .await
            .unwrap();
        assert_eq!(count, expected, "{table} keeps its rows");
    }
    let violations: i64 = sqlx::query_scalar("SELECT count(*) FROM pragma_foreign_key_check")
        .fetch_one(pool)
        .await
        .unwrap();
    assert_eq!(violations, 0);
    // Every connection enforces foreign keys again, the migrating one
    // included: holding as many as the pool opens reaches each.
    let mut held = Vec::new();
    for _ in 0..8 {
        let mut connection = pool.acquire().await.unwrap();
        let enabled: i64 = sqlx::query_scalar("PRAGMA foreign_keys")
            .fetch_one(&mut *connection)
            .await
            .unwrap();
        assert_eq!(enabled, 1);
        held.push(connection);
    }
    drop(held);
    let removed = sqlx::query("DELETE FROM apps WHERE key = 'google_workspace'")
        .execute(pool)
        .await;
    assert!(removed.is_err(), "an installation still references the app");
}

#[tokio::test]
async fn keeps_agents_and_their_grants_through_migration_0031() {
    use std::str::FromStr;

    use sqlx::sqlite::SqliteConnectOptions;

    let directory = TemporaryDirectory::new();
    let earlier = directory.0.join("migrations");
    std::fs::create_dir(&earlier).unwrap();
    let source = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("migrations");
    for entry in std::fs::read_dir(&source).unwrap() {
        let path = entry.unwrap().path();
        let name = path.file_name().unwrap().to_string_lossy().into_owned();
        if name.as_str() < "0031" {
            std::fs::copy(&path, earlier.join(&name)).unwrap();
        }
    }
    let options = SqliteConnectOptions::from_str("sqlite:")
        .unwrap()
        .filename(directory.file())
        .create_if_missing(true)
        .foreign_keys(true);
    let pool = sqlx::SqlitePool::connect_with(options).await.unwrap();
    sqlx::migrate::Migrator::new(earlier.as_path())
        .await
        .unwrap()
        .run(&pool)
        .await
        .unwrap();
    let blob = |n: u8| {
        let mut id = [0u8; 16];
        id[15] = n;
        id.to_vec()
    };
    for statement in [
        "INSERT INTO organizations (id, name, slug, created_at) VALUES (?1, 'Org', 'org', 0)",
        "INSERT INTO workspaces (id, organization_id, environment, name, slug, created_at) \
         VALUES (?2, ?1, 'sandbox', 'W', 'w', 0)",
        "INSERT INTO apps (id, organization_id, kind, name, key, description, icon_glyph, icon_color, auth, tools, \
         version, created_at, updated_at) \
         VALUES (?3, ?1, 'mcp', 'T', 'tools', '', 'T', '#0E7490', '{}', '[]', 1, 0, 0)",
        "INSERT INTO installation_short_ids (id, organization_id, workspace_id, short_id, created_at) \
         VALUES (?4, ?1, ?2, 'abcdef', 0)",
        "INSERT INTO installations (id, organization_id, workspace_id, catalog_app_id, short_id, label, status, \
         configuration_version, granted_scopes, created_at, updated_at) \
         VALUES (?5, ?1, ?2, ?3, 'abcdef', 'Main', 'disconnected', 1, '[]', 0, 0)",
        // An agent on a deployment's model, by the ID it was listed under.
        "INSERT INTO agents (id, organization_id, workspace_id, name, description, model, instructions, status, \
         created_at, updated_at) \
         VALUES (?6, ?1, ?2, 'Helper', 'Helps.', 'model-balanced', 'Be brief.', 'paused', 1, 2)",
        "INSERT INTO agent_grants (id, organization_id, workspace_id, agent_id, installation_id, tools, \
         approval_required, created_at, updated_at) \
         VALUES (?7, ?1, ?2, ?6, ?5, '[\"lookup\"]', '[]', 0, 0)",
    ] {
        sqlx::query(statement)
            .bind(blob(1))
            .bind(blob(2))
            .bind(blob(3))
            .bind(blob(4))
            .bind(blob(5))
            .bind(blob(6))
            .bind(blob(7))
            .execute(&pool)
            .await
            .unwrap_or_else(|error| panic!("{statement}: {error}"));
    }
    pool.close().await;

    let database = Database::open(&directory.file()).await.unwrap();
    let pool = database.pool();
    type AgentRow = (Vec<u8>, String, String, Option<Vec<u8>>, String, String, i64, i64);
    let agent: AgentRow = sqlx::query_as(
        "SELECT id, name, description, model_id, instructions, status, created_at, updated_at FROM agents",
    )
    .fetch_one(pool)
    .await
    .unwrap();
    assert_eq!(
        agent,
        (
            blob(6),
            "Helper".to_owned(),
            "Helps.".to_owned(),
            None,
            "Be brief.".to_owned(),
            "paused".to_owned(),
            1,
            2
        ),
        "kept as it was, on its environment's default"
    );
    let grants: i64 = sqlx::query_scalar("SELECT count(*) FROM agent_grants")
        .fetch_one(pool)
        .await
        .unwrap();
    assert_eq!(grants, 1, "the grant survives the rebuilt table");
    let violations: i64 = sqlx::query_scalar("SELECT count(*) FROM pragma_foreign_key_check")
        .fetch_one(pool)
        .await
        .unwrap();
    assert_eq!(violations, 0);
    let mut indexes: Vec<String> =
        sqlx::query_scalar("SELECT name FROM pragma_index_list('agents') WHERE origin = 'c'")
            .fetch_all(pool)
            .await
            .unwrap();
    indexes.sort();
    assert_eq!(indexes, ["agents_by_model", "agents_by_name"]);
    let mut held = Vec::new();
    for _ in 0..8 {
        let mut connection = pool.acquire().await.unwrap();
        let enabled: i64 = sqlx::query_scalar("PRAGMA foreign_keys")
            .fetch_one(&mut *connection)
            .await
            .unwrap();
        assert_eq!(enabled, 1);
        held.push(connection);
    }
    drop(held);
    // The agent's grants still go with it, and the agent with its workspace.
    sqlx::query("DELETE FROM agents WHERE organization_id = ? AND id = ?")
        .bind(blob(1))
        .bind(blob(6))
        .execute(pool)
        .await
        .unwrap();
    let grants: i64 = sqlx::query_scalar("SELECT count(*) FROM agent_grants")
        .fetch_one(pool)
        .await
        .unwrap();
    assert_eq!(grants, 0, "a grant goes with its agent");
}

#[tokio::test]
async fn keeps_tools_through_migration_0033_and_holds_definitions_to_the_ceiling() {
    use std::str::FromStr;

    use sqlx::sqlite::SqliteConnectOptions;
    use zupflow_core::tools::MAX_DEFINITION_BYTES_CEILING;

    let directory = TemporaryDirectory::new();
    let earlier = directory.0.join("migrations");
    std::fs::create_dir(&earlier).unwrap();
    let source = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("migrations");
    for entry in std::fs::read_dir(&source).unwrap() {
        let path = entry.unwrap().path();
        let name = path.file_name().unwrap().to_string_lossy().into_owned();
        if name.as_str() < "0033" {
            std::fs::copy(&path, earlier.join(&name)).unwrap();
        }
    }
    let options = SqliteConnectOptions::from_str("sqlite:")
        .unwrap()
        .filename(directory.file())
        .create_if_missing(true)
        .foreign_keys(true);
    let pool = sqlx::SqlitePool::connect_with(options).await.unwrap();
    sqlx::migrate::Migrator::new(earlier.as_path())
        .await
        .unwrap()
        .run(&pool)
        .await
        .unwrap();
    let blob = |n: u8| {
        let mut id = [0u8; 16];
        id[15] = n;
        id.to_vec()
    };
    let hash = |n: u8| vec![n; 32];
    for statement in [
        "INSERT INTO organizations (id, name, slug, created_at) VALUES (?1, 'Org', 'org', 0)",
        "INSERT INTO workspaces (id, organization_id, environment, name, slug, created_at) \
         VALUES (?2, ?1, 'sandbox', 'W', 'w', 0)",
        "INSERT INTO apps (id, organization_id, kind, name, key, description, icon_glyph, icon_color, auth, tools, \
         version, created_at, updated_at) \
         VALUES (?3, ?1, 'mcp', 'T', 'tools', '', 'T', '#0E7490', '{}', '[]', 1, 0, 0)",
        "INSERT INTO installation_short_ids (id, organization_id, workspace_id, short_id, created_at) \
         VALUES (?4, ?1, ?2, 'abcdef', 0)",
        "INSERT INTO installations (id, organization_id, workspace_id, catalog_app_id, short_id, label, status, \
         configuration_version, granted_scopes, created_at, updated_at) \
         VALUES (?5, ?1, ?2, ?3, 'abcdef', 'Main', 'connected', 1, '[]', 0, 0)",
        "INSERT INTO installation_tools (id, organization_id, workspace_id, installation_id, name, effect, \
         pinned_definition, pinned_hash, latest_definition, latest_hash, first_seen_at, seen_at, changed_at, gone_at) \
         VALUES (?6, ?1, ?2, ?5, 'lookup', 'read', '{\"name\":\"lookup\"}', ?7, \
         '{\"description\":\"v2\",\"name\":\"lookup\"}', ?8, 1, 2, 3, NULL)",
    ] {
        sqlx::query(statement)
            .bind(blob(1))
            .bind(blob(2))
            .bind(blob(3))
            .bind(blob(4))
            .bind(blob(5))
            .bind(blob(6))
            .bind(hash(7))
            .bind(hash(8))
            .execute(&pool)
            .await
            .unwrap_or_else(|error| panic!("{statement}: {error}"));
    }
    pool.close().await;

    let database = Database::open(&directory.file()).await.unwrap();
    let pool = database.pool();
    type ToolRow = (
        Vec<u8>,
        String,
        String,
        String,
        Vec<u8>,
        Option<String>,
        Option<Vec<u8>>,
        i64,
        i64,
        Option<i64>,
    );
    let tool: ToolRow = sqlx::query_as(
        "SELECT id, name, effect, pinned_definition, pinned_hash, latest_definition, latest_hash, first_seen_at, \
         seen_at, changed_at FROM installation_tools",
    )
    .fetch_one(pool)
    .await
    .unwrap();
    assert_eq!(
        tool,
        (
            blob(6),
            "lookup".to_owned(),
            "read".to_owned(),
            r#"{"name":"lookup"}"#.to_owned(),
            hash(7),
            Some(r#"{"description":"v2","name":"lookup"}"#.to_owned()),
            Some(hash(8)),
            1,
            2,
            Some(3),
        ),
        "a tool is kept as it was"
    );
    assert!(foreign_keys(&database, "installation_tools").await.len() >= 2);

    // The store holds definitions to `MAX_DEFINITION_BYTES`; the table only
    // to the ceiling, counted in bytes, so the limit can rise without a
    // migration.
    let definition = |bytes: usize| {
        let wrapper = r#"{"description":""}"#.len();
        // A two-byte character, so a check counting characters would let this past.
        format!(r#"{{"description":"{}"}}"#, "é".repeat((bytes - wrapper) / 2))
    };
    let insert = |name: &'static str, definition: String| {
        sqlx::query(
            "INSERT INTO installation_tools (id, organization_id, workspace_id, installation_id, name, effect, \
             pinned_definition, pinned_hash, first_seen_at, seen_at) VALUES (?1, ?2, ?3, ?4, ?5, 'read', ?6, ?7, 0, 0)",
        )
        .bind(uuid::Uuid::now_v7().as_bytes().to_vec())
        .bind(blob(1))
        .bind(blob(2))
        .bind(blob(5))
        .bind(name)
        .bind(definition)
        .bind(hash(9))
        .execute(pool)
    };
    let at_ceiling = definition(MAX_DEFINITION_BYTES_CEILING);
    assert_eq!(at_ceiling.len(), MAX_DEFINITION_BYTES_CEILING);
    insert("at_ceiling", at_ceiling)
        .await
        .expect("a definition at the ceiling is kept");
    let over = definition(MAX_DEFINITION_BYTES_CEILING + 2);
    assert!(over.len() > MAX_DEFINITION_BYTES_CEILING);
    assert!(over.chars().count() < MAX_DEFINITION_BYTES_CEILING);
    assert!(
        insert("over", over).await.is_err(),
        "a definition over the ceiling is refused"
    );
}
