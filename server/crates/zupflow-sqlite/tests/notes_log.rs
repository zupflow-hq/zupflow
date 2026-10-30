//! What the `notes` kind and its SQLite store log, which never holds a
//! note's text. In its own binary: the log is captured with a thread-local
//! subscriber, and tracing caches each callsite's interest process-wide, so
//! another test reaching the same callsite first could hide the line from it.

#![allow(clippy::unwrap_used)]

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use serde_json::{Map, Value, json};
use uuid::Uuid;
use zupflow_core::ids::{
    AgentId, ChannelConversationId, MemoryId, MemoryTypeId, OrganizationId, RunId, ToolCallId, WorkspaceId,
};
use zupflow_core::installations::ShortId;
use zupflow_core::memory::notes::{self, NewNote, NoteRef, NoteStore, Notes, Reach};
use zupflow_core::memory::{
    Audience, CallContext, MemoryChange, MemoryError, MemoryFilter, MemoryProvider, MemoryScope, MemoryWrites, Origin,
    record_nothing,
};
use zupflow_core::time::Timestamp;
use zupflow_sqlite::{Database, SqliteNoteStore};

/// A directory for the test's database, removed with everything in it.
struct TemporaryDirectory(PathBuf);

impl TemporaryDirectory {
    fn new() -> Self {
        let path = std::env::temp_dir().join(format!("zupflow-db-{}", Uuid::now_v7().simple()));
        std::fs::create_dir(&path).unwrap();
        Self(path)
    }

    fn file(&self) -> PathBuf {
        self.0.join("zupflow.sqlite")
    }
}

impl Drop for TemporaryDirectory {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// Makes a scope's rows straight in the database: an organization, its
/// workspace and agent, and a `notes` type. On a pool of its own, since
/// [`Database`] keeps its pool to the crate.
async fn prepare(file: &Path) -> MemoryScope {
    let scope = MemoryScope {
        organization_id: OrganizationId::generate(),
        workspace_id: WorkspaceId::generate(),
        agent_id: AgentId::generate(),
        memory_type_id: MemoryTypeId::generate(),
    };
    let pool = sqlx::SqlitePool::connect(&format!("sqlite:{}", file.display()))
        .await
        .unwrap();
    let org = scope.organization_id.uuid();
    let workspace = scope.workspace_id.uuid();
    sqlx::query("INSERT INTO organizations (id, name, slug, created_at) VALUES (?, 'Test', ?, 0)")
        .bind(org)
        .bind(format!("org-{}", org.simple()))
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query(
        "INSERT INTO workspaces (id, organization_id, environment, name, slug, created_at) \
         VALUES (?, ?, 'sandbox', 'W', ?, 0)",
    )
    .bind(workspace)
    .bind(org)
    .bind(format!("w-{}", workspace.simple()))
    .execute(&pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO agents (id, organization_id, workspace_id, name, description, instructions, \
         status, created_at, updated_at) VALUES (?, ?, ?, 'A', '', '', 'active', 0, 0)",
    )
    .bind(scope.agent_id.uuid())
    .bind(org)
    .bind(workspace)
    .execute(&pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO memory_types (id, organization_id, name, kind, settings, writes, created_at, \
         updated_at) VALUES (?, ?, 'Notes', 'notes', '{\"max_notes\":200,\"recall_characters\":8000}', \
         'allow', 0, 0)",
    )
    .bind(scope.memory_type_id.uuid())
    .bind(org)
    .execute(&pool)
    .await
    .unwrap();
    pool.close().await;
    scope
}

fn at(millis: i64) -> Timestamp {
    Timestamp::from_millis(1_800_000_000_000 + millis)
}

fn run_origin() -> Origin {
    Origin::Run {
        run_id: RunId::generate(),
        tool_call_id: ToolCallId::generate(),
        after_app_content: false,
    }
}

/// Captures what's logged on this thread, at every level.
#[derive(Clone, Default)]
struct Captured(Arc<Mutex<Vec<u8>>>);

impl std::io::Write for Captured {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(bytes);
        Ok(bytes.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl Captured {
    fn text(&self) -> String {
        String::from_utf8_lossy(&self.0.lock().unwrap()).into_owned()
    }
}

fn arguments(value: Value) -> Map<String, Value> {
    let Value::Object(arguments) = value else {
        panic!("arguments are an object");
    };
    arguments
}

/// No note's text reaches a log: not as it's saved, searched, recalled,
/// listed, changed or forgotten, nor when the database refuses it.
#[tokio::test]
async fn keeps_every_notes_text_out_of_the_logs() {
    const SECRET: &str = "quokka5501";
    let directory = TemporaryDirectory::new();
    let database = Database::open(&directory.file()).await.unwrap();
    let scope = prepare(&directory.file()).await;
    let store = Arc::new(SqliteNoteStore::new(database));
    let kind = Notes::new(store.clone());
    let captured = Captured::default();
    let writer = captured.clone();
    let subscriber = tracing_subscriber::fmt()
        .with_max_level(tracing::Level::TRACE)
        .with_ansi(false)
        .with_writer(move || writer.clone())
        .finish();
    let _logged = tracing::subscriber::set_default(subscriber);

    let origin = run_origin();
    let settings = notes::check_settings(&Map::new()).unwrap();
    let conversation = Audience::Direct(ChannelConversationId::generate());
    let context = |audience, now| CallContext {
        origin: &origin,
        audience,
        settings: &settings,
        writes: MemoryWrites::Allow,
        now,
    };
    let workspace = Audience::Workspace;
    let saved = kind
        .call(
            &scope,
            &context(&workspace, at(1)),
            notes::SAVE_MEMORY,
            &arguments(json!({ "text": format!("The vault code is {SECRET}.") })),
            &record_nothing,
        )
        .await
        .unwrap();
    let Some(MemoryChange::Saved(note)) = saved.change else {
        panic!("saved: {saved:?}");
    };
    let short = note.short_id.as_str();
    // A save whose answer was lost is found by its origin, and logs nothing of it.
    let settled = kind
        .settle(
            &scope,
            &context(&workspace, at(1)),
            notes::SAVE_MEMORY,
            &arguments(json!({ "text": format!("The vault code is {SECRET}.") })),
            &record_nothing,
        )
        .await
        .unwrap()
        .expect("found");
    assert!(!settled.is_error && settled.change.is_none(), "{settled:?}");
    kind.call(
        &scope,
        &context(&conversation, at(2)),
        notes::SAVE_MEMORY,
        &arguments(json!({ "text": format!("Here only: {SECRET}.") })),
        &record_nothing,
    )
    .await
    .unwrap();
    let found = kind
        .call(
            &scope,
            &context(&conversation, at(3)),
            notes::SEARCH_MEMORY,
            &arguments(json!({ "query": format!("\"{SECRET}\"*") })),
            &record_nothing,
        )
        .await
        .unwrap();
    assert!(!found.is_error && found.content.contains(SECRET), "{found:?}");
    let recalled = kind.recall(&scope, &conversation, &settings).await.unwrap();
    assert!(recalled.framed().unwrap().contains(SECRET));
    let filter = MemoryFilter {
        q: Some(SECRET.to_owned()),
        limit: 10,
        ..MemoryFilter::default()
    };
    assert_eq!(kind.list(&scope, &filter).await.unwrap().total, 2);
    kind.call(
        &scope,
        &context(&workspace, at(4)),
        notes::UPDATE_MEMORY,
        &arguments(json!({ "memory": short, "text": format!("The vault code is {SECRET}, still.") })),
        &record_nothing,
    )
    .await
    .unwrap();
    kind.change(
        &scope,
        &origin,
        note.id,
        &format!("The vault code was {SECRET}."),
        at(5),
        &record_nothing,
    )
    .await
    .unwrap();
    kind.call(
        &scope,
        &context(&workspace, at(6)),
        notes::FORGET_MEMORY,
        &arguments(json!({ "memory": short })),
        &record_nothing,
    )
    .await
    .unwrap();
    // The database refuses what the kind never sends: text too long.
    let long = format!("{SECRET} ").repeat(60);
    let refused = store
        .insert(
            &scope,
            &NewNote {
                id: MemoryId::generate(),
                text: &long,
                channel_conversation_id: None,
                origin: &origin,
                short_ids: &[ShortId::from_bits(7)],
            },
            200,
            at(7),
            &record_nothing,
        )
        .await;
    assert_eq!(refused, Err(MemoryError::Unavailable));
    let kept = store.list(&scope, None, None, 10).await.unwrap().items;
    let refused = store
        .update(
            &scope,
            NoteRef::Id(kept[0].id),
            Reach::Everything,
            &origin,
            &long,
            at(8),
            &record_nothing,
        )
        .await;
    assert_eq!(refused, Err(MemoryError::Unavailable));

    let logged = captured.text();
    assert!(
        logged.contains("memory_notes.database_error"),
        "the refusals are logged: {logged}"
    );
    assert!(!logged.to_lowercase().contains(SECRET), "never a note's text: {logged}");
}
