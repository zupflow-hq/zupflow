//! [`SqliteNoteStore`]: the `notes` kind's notes, in `memory_notes`. Every
//! statement repeats the full scope (organization, workspace, agent and
//! type) beside a note's ID or short ID.
//!
//! Each change is one transaction. A save is an immediate one that checks,
//! in it, what must hold across concurrent saves: the scope's agent and type
//! are there, the scope keeps fewer notes than its limit, and a short ID is
//! held once in it. An update writes the text, who wrote it and when, or,
//! with the text the note already holds, nothing at all. What a change
//! records ([`Record`]) is written in its transaction: each webhook event
//! with its deliveries and their jobs, as the outbox writes one, and the
//! audit event, each only in the scope's own workspace.
//!
//! A search is FTS5's: the notes holding every word, by word, in any case
//! and accent, ranked by BM25, then newest first. BM25 weighs a word by how
//! many of its table's rows hold it, so a table shared by every scope would
//! rank one scope's notes by every organization's. Each search indexes the
//! notes it may find, and only those, in a temporary FTS5 table of its own
//! connection, in a read transaction it then rolls back: its order depends
//! on nothing else, and no index keeps a note that's gone.

use async_trait::async_trait;
use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use sqlx::pool::PoolConnection;
use sqlx::sqlite::{SqliteArguments, SqliteRow};
use sqlx::{Connection, QueryBuilder, Row, Sqlite, SqliteConnection, Transaction};
use uuid::Uuid;
use zupflow_core::audit::Audit;
use zupflow_core::ids::{AgentId, ChannelConversationId, MemoryId, MemoryTypeId, OrganizationId, WorkspaceId};
use zupflow_core::installations::ShortId;
use zupflow_core::memory::notes::{NewNote, NoteRef, NoteStore, Reach, Recent};
use zupflow_core::memory::{
    Audience, Forget, Memory, MemoryChange, MemoryError, MemoryScope, Origin, Record, Recorded, Rewritten,
};
use zupflow_core::time::Timestamp;
use zupflow_core::workspaces::Page;

use crate::Database;

pub struct SqliteNoteStore {
    database: Database,
}

impl SqliteNoteStore {
    pub fn new(database: Database) -> Self {
        Self { database }
    }

    async fn immediate(&self, operation: &'static str) -> Result<Transaction<'static, Sqlite>, MemoryError> {
        self.database
            .pool()
            .begin_with("BEGIN IMMEDIATE")
            .await
            .map_err(failed(operation))
    }

    /// A connection whose temporary search table is there, and empty.
    async fn searcher(&self, operation: &'static str) -> Result<PoolConnection<Sqlite>, MemoryError> {
        let mut connection = self.database.pool().acquire().await.map_err(failed(operation))?;
        sqlx::query(FOUND_TABLE)
            .execute(&mut *connection)
            .await
            .map_err(failed(operation))?;
        Ok(connection)
    }
}

fn failed(operation: &'static str) -> impl FnOnce(sqlx::Error) -> MemoryError {
    move |error| {
        tracing::error!(event = "memory_notes.database_error", operation, %error);
        MemoryError::Unavailable
    }
}

/// For a statement holding a search's words: logs the error's code only, as
/// SQLite's message can quote them.
fn failed_quietly(operation: &'static str) -> impl FnOnce(sqlx::Error) -> MemoryError {
    move |error| {
        let code = error
            .as_database_error()
            .and_then(|error| error.code())
            .map(|code| code.into_owned());
        tracing::error!(event = "memory_notes.database_error", operation, code);
        MemoryError::Unavailable
    }
}

fn read(error: impl std::fmt::Display) -> MemoryError {
    tracing::error!(event = "memory_notes.unreadable_row", %error);
    MemoryError::Unavailable
}

/// Writes what a change records, in its transaction: each webhook event, with
/// a delivery and its job for every endpoint subscribed to it, and the audit
/// event. Either placed anywhere but the scope's workspace fails the change.
async fn write_records(
    connection: &mut SqliteConnection,
    scope: &MemoryScope,
    recorded: &Recorded,
    operation: &'static str,
) -> Result<(), MemoryError> {
    for event in &recorded.events {
        if event.workspace_id != Some(scope.workspace_id) {
            tracing::error!(event = "memory_notes.event_misplaced", operation);
            return Err(MemoryError::Unavailable);
        }
        crate::webhooks::record_event(&mut *connection, scope.organization_id, event)
            .await
            .map_err(|error| {
                tracing::error!(event = "memory_notes.event_not_recorded", operation, %error);
                MemoryError::Unavailable
            })?;
    }
    if let Some(entry) = &recorded.audit {
        crate::audit::record_in_workspace(&mut *connection, scope.organization_id, scope.workspace_id, entry)
            .await
            .map_err(failed(operation))?;
    }
    Ok(())
}

type Query<'q> = sqlx::query::Query<'q, Sqlite, SqliteArguments>;

/// A note's columns, as [`note_from`] reads them.
macro_rules! columns {
    () => {
        "id, organization_id, workspace_id, agent_id, memory_type_id, short_id, text, channel_conversation_id, \
         origin, created_at, updated_at"
    };
}

/// The scope, bound as `?1` to `?4` ([`bind_scope`]).
macro_rules! in_scope {
    () => {
        "organization_id = ?1 AND workspace_id = ?2 AND agent_id = ?3 AND memory_type_id = ?4"
    };
}

/// The notes an audience is shown, bound as `?5` (its conversation, or
/// null): the workspace's, and its own conversation's ([`bind_shown`]).
macro_rules! shown {
    () => {
        "(channel_conversation_id IS NULL OR channel_conversation_id = ?5)"
    };
}

/// The notes a [`Reach`] changes, bound as `?5` (every one) and `?6` (the
/// one conversation, or null for the workspace's): a direct conversation
/// changes its own notes only ([`bind_written`]).
macro_rules! written {
    () => {
        "(?5 OR channel_conversation_id IS ?6)"
    };
}

/// Whether the scope's agent and type are there.
const SCOPE_PRESENT: &str = "SELECT EXISTS (SELECT 1 FROM agents WHERE organization_id = ?1 AND workspace_id = ?2 \
                             AND id = ?3) AND EXISTS (SELECT 1 FROM memory_types WHERE organization_id = ?1 \
                             AND id = ?4)";
const COUNT: &str = concat!("SELECT count(*) FROM memory_notes WHERE ", in_scope!());
/// The short IDs of `?5`, a JSON array, that the scope's notes hold.
const TAKEN: &str = concat!(
    "SELECT short_id FROM memory_notes WHERE ",
    in_scope!(),
    " AND short_id IN (SELECT value FROM json_each(?5))"
);
const INSERT: &str = concat!(
    "INSERT INTO memory_notes (id, organization_id, workspace_id, agent_id, memory_type_id, short_id, text, \
     channel_conversation_id, origin, created_at, updated_at) VALUES (?5, ?1, ?2, ?3, ?4, ?6, ?7, ?8, ?9, ?10, ?10) \
     RETURNING ",
    columns!()
);
/// A note by its ID (`?7`) or its short ID (`?8`), the other bound null,
/// given new text (`?9`), who wrote it (`?10`) and when (`?11`): none when
/// it already holds that text.
const UPDATE: &str = concat!(
    "UPDATE memory_notes SET text = ?9, origin = ?10, updated_at = ?11 WHERE ",
    in_scope!(),
    " AND ",
    written!(),
    " AND (id = ?7 OR short_id = ?8) AND text <> ?9 RETURNING ",
    columns!()
);
/// The note an update names, as [`UPDATE`] finds it.
const WRITTEN: &str = concat!(
    "SELECT ",
    columns!(),
    " FROM memory_notes WHERE ",
    in_scope!(),
    " AND ",
    written!(),
    " AND (id = ?7 OR short_id = ?8)"
);
/// A note by its ID (`?5`), whatever its audience.
const FIND: &str = concat!(
    "SELECT ",
    columns!(),
    " FROM memory_notes WHERE ",
    in_scope!(),
    " AND id = ?5"
);
const DELETE: &str = concat!(
    "DELETE FROM memory_notes WHERE ",
    in_scope!(),
    " AND ",
    written!(),
    " AND (id = ?7 OR short_id = ?8) RETURNING ",
    columns!()
);
const COUNT_SHOWN: &str = concat!(
    "SELECT count(*) FROM memory_notes WHERE ",
    in_scope!(),
    " AND ",
    shown!()
);
/// The notes last changed, newest first; `?6` is the limit.
const RECENT: &str = concat!(
    "SELECT ",
    columns!(),
    " FROM memory_notes WHERE ",
    in_scope!(),
    " AND ",
    shown!(),
    " ORDER BY updated_at DESC, id DESC LIMIT ?6"
);
/// The temporary table a search indexes its notes in: one per connection,
/// empty outside a search.
const FOUND_TABLE: &str = "CREATE VIRTUAL TABLE IF NOT EXISTS temp.memory_notes_found \
                           USING fts5 (text, tokenize = 'unicode61 remove_diacritics 2')";
const EMPTY_FOUND: &str = "DELETE FROM temp.memory_notes_found";
/// Indexes the notes a search may find, the ones its audience is shown, by
/// their rows in `memory_notes`.
const FILL_FOUND: &str = concat!(
    "INSERT INTO temp.memory_notes_found (rowid, text) SELECT rowid, text FROM memory_notes WHERE ",
    in_scope!(),
    " AND ",
    shown!()
);
/// Indexes every note of the scope, for a list.
const FILL_LISTED: &str = concat!(
    "INSERT INTO temp.memory_notes_found (rowid, text) SELECT rowid, text FROM memory_notes WHERE ",
    in_scope!()
);
/// The indexed notes holding every word of `?6`: best first, then newest;
/// `?7` is the limit.
const SEARCH: &str = concat!(
    "SELECT ",
    columns!(),
    " FROM memory_notes JOIN (SELECT rowid AS found, bm25(memory_notes_found) AS rank \
     FROM temp.memory_notes_found WHERE memory_notes_found MATCH ?6) ON memory_notes.rowid = found WHERE ",
    in_scope!(),
    " AND ",
    shown!(),
    " ORDER BY rank, updated_at DESC, id DESC LIMIT ?7"
);
/// The scope's `?5` oldest notes, by when each was saved, then ID, and then
/// their removal.
const SCOPE_NOTES: &str = concat!(
    "SELECT ",
    columns!(),
    " FROM memory_notes WHERE ",
    in_scope!(),
    " ORDER BY created_at, id LIMIT ?5"
);
const DELETE_SCOPE: &str = concat!(
    "DELETE FROM memory_notes WHERE ",
    in_scope!(),
    " AND id IN (SELECT id FROM memory_notes WHERE ",
    in_scope!(),
    " ORDER BY created_at, id LIMIT ?5)"
);
const FORGET_AGENT: &str = "DELETE FROM memory_notes WHERE organization_id = ? AND workspace_id = ? AND agent_id = ?";
const FORGET_WORKSPACE: &str = "DELETE FROM memory_notes WHERE organization_id = ? AND workspace_id = ?";
const FORGET_TYPE: &str = "DELETE FROM memory_notes WHERE organization_id = ? AND memory_type_id = ?";
const FORGET_ORGANIZATION: &str = "DELETE FROM memory_notes WHERE organization_id = ?";

/// Binds the scope as `?1` to `?4`.
fn bind_scope<'q>(query: Query<'q>, scope: &MemoryScope) -> Query<'q> {
    query
        .bind(scope.organization_id.uuid())
        .bind(scope.workspace_id.uuid())
        .bind(scope.agent_id.uuid())
        .bind(scope.memory_type_id.uuid())
}

/// Binds the scope as `?1` to `?4`, and the audience's conversation as `?5`.
fn bind_shown<'q>(query: Query<'q>, scope: &MemoryScope, audience: &Audience) -> Query<'q> {
    bind_scope(query, scope).bind(audience.conversation().map(|conversation| conversation.uuid()))
}

/// Binds the scope as `?1` to `?4`, and what `reach` changes as `?5` and
/// `?6`.
fn bind_written<'q>(query: Query<'q>, scope: &MemoryScope, reach: Reach<'_>) -> Query<'q> {
    let (everything, conversation) = match reach {
        Reach::Everything => (true, None),
        Reach::Run(audience) => (false, audience.conversation()),
    };
    bind_scope(query, scope)
        .bind(everything)
        .bind(conversation.map(|conversation| conversation.uuid()))
}

/// Binds a note as `?7` (its ID) or `?8` (its short ID).
fn bind_note<'q>(query: Query<'q>, note: NoteRef<'_>) -> Query<'q> {
    match note {
        NoteRef::Id(id) => query.bind(Some(id.uuid())).bind(None::<String>),
        NoteRef::ShortId(short_id) => query.bind(None::<Uuid>).bind(Some(short_id.as_str().to_owned())),
    }
}

/// Appends the scope's conditions, every one, then with `words` (an FTS5
/// query) only the notes the search table holds that match it.
fn push_listed(query: &mut QueryBuilder<Sqlite>, scope: &MemoryScope, words: Option<&str>) {
    query.push(" WHERE organization_id = ");
    query.push_bind(scope.organization_id.uuid());
    query.push(" AND workspace_id = ");
    query.push_bind(scope.workspace_id.uuid());
    query.push(" AND agent_id = ");
    query.push_bind(scope.agent_id.uuid());
    query.push(" AND memory_type_id = ");
    query.push_bind(scope.memory_type_id.uuid());
    if let Some(words) = words {
        query.push(" AND rowid IN (SELECT rowid FROM temp.memory_notes_found WHERE memory_notes_found MATCH ");
        query.push_bind(words.to_owned());
        query.push(")");
    }
}

/// An FTS5 query for notes holding every word: each a phrase in double
/// quotes (a quote in it doubled), so none is read as an operator.
fn every_word(words: &[String]) -> String {
    words
        .iter()
        .map(|word| format!("\"{}\"", word.replace('"', "\"\"")))
        .collect::<Vec<_>>()
        .join(" ")
}

fn note_from(row: &SqliteRow) -> Result<Memory, MemoryError> {
    let short_id: String = row.try_get("short_id").map_err(read)?;
    let origin: String = row.try_get("origin").map_err(read)?;
    let conversation: Option<Uuid> = row.try_get("channel_conversation_id").map_err(read)?;
    Ok(Memory {
        id: MemoryId::from_uuid(row.try_get("id").map_err(read)?),
        organization_id: OrganizationId::from_uuid(row.try_get("organization_id").map_err(read)?),
        workspace_id: WorkspaceId::from_uuid(row.try_get("workspace_id").map_err(read)?),
        agent_id: AgentId::from_uuid(row.try_get("agent_id").map_err(read)?),
        memory_type_id: MemoryTypeId::from_uuid(row.try_get("memory_type_id").map_err(read)?),
        short_id: ShortId::parse(&short_id).ok_or_else(|| read("a malformed short ID"))?,
        text: row.try_get("text").map_err(read)?,
        channel_conversation_id: conversation.map(ChannelConversationId::from_uuid),
        origin: serde_json::from_str(&origin).map_err(read)?,
        created_at: Timestamp::from_millis(row.try_get("created_at").map_err(read)?),
        updated_at: Timestamp::from_millis(row.try_get("updated_at").map_err(read)?),
    })
}

/// A position in a list: newest first, then by ID.
struct Position {
    created_at: i64,
    id: Uuid,
}

impl Position {
    /// The note's prefixed ID, never its bare UUID: a cursor leaves the server.
    fn encode(&self) -> String {
        URL_SAFE_NO_PAD.encode(format!("{}\n{}", self.created_at, MemoryId::from_uuid(self.id)))
    }

    fn decode(cursor: &str) -> Option<Self> {
        let raw = String::from_utf8(URL_SAFE_NO_PAD.decode(cursor).ok()?).ok()?;
        let (created_at, id) = raw.split_once('\n')?;
        Some(Self {
            created_at: created_at.parse().ok()?,
            id: MemoryId::parse(id).ok()?.uuid(),
        })
    }
}

fn count(total: i64) -> Result<u64, MemoryError> {
    u64::try_from(total).map_err(read)
}

#[async_trait]
impl NoteStore for SqliteNoteStore {
    async fn insert(
        &self,
        scope: &MemoryScope,
        note: &NewNote<'_>,
        max_notes: u32,
        now: Timestamp,
        record: &Record<'_>,
    ) -> Result<Memory, MemoryError> {
        let origin = serde_json::to_string(note.origin).map_err(read)?;
        let candidates: Vec<&str> = note.short_ids.iter().map(ShortId::as_str).collect();
        let candidates = serde_json::to_string(&candidates).map_err(read)?;
        let mut transaction = self.immediate("insert").await?;
        let present: bool = sqlx::query_scalar(SCOPE_PRESENT)
            .bind(scope.organization_id.uuid())
            .bind(scope.workspace_id.uuid())
            .bind(scope.agent_id.uuid())
            .bind(scope.memory_type_id.uuid())
            .fetch_one(&mut *transaction)
            .await
            .map_err(failed("insert"))?;
        if !present {
            return Err(MemoryError::NotFound);
        }
        let kept: i64 = sqlx::query_scalar(COUNT)
            .bind(scope.organization_id.uuid())
            .bind(scope.workspace_id.uuid())
            .bind(scope.agent_id.uuid())
            .bind(scope.memory_type_id.uuid())
            .fetch_one(&mut *transaction)
            .await
            .map_err(failed("insert"))?;
        if count(kept)? >= u64::from(max_notes) {
            return Err(MemoryError::Full);
        }
        let taken: Vec<String> = sqlx::query_scalar(TAKEN)
            .bind(scope.organization_id.uuid())
            .bind(scope.workspace_id.uuid())
            .bind(scope.agent_id.uuid())
            .bind(scope.memory_type_id.uuid())
            .bind(candidates)
            .fetch_all(&mut *transaction)
            .await
            .map_err(failed("insert"))?;
        let Some(short_id) = note
            .short_ids
            .iter()
            .find(|short_id| !taken.iter().any(|taken| taken == short_id.as_str()))
        else {
            tracing::error!(event = "memory_notes.short_ids_exhausted", drawn = note.short_ids.len());
            return Err(MemoryError::Unavailable);
        };
        let row = bind_scope(sqlx::query(INSERT), scope)
            .bind(note.id.uuid())
            .bind(short_id.as_str())
            .bind(note.text)
            .bind(note.channel_conversation_id.map(|conversation| conversation.uuid()))
            .bind(origin)
            .bind(now.millis())
            .fetch_one(&mut *transaction)
            .await
            .map_err(failed("insert"))?;
        let saved = MemoryChange::Saved(note_from(&row)?);
        write_records(&mut transaction, scope, &record(&saved), "insert").await?;
        transaction.commit().await.map_err(failed("insert"))?;
        Ok(saved.into_memory())
    }

    async fn update(
        &self,
        scope: &MemoryScope,
        note: NoteRef<'_>,
        reach: Reach<'_>,
        origin: &Origin,
        text: &str,
        now: Timestamp,
        record: &Record<'_>,
    ) -> Result<Rewritten, MemoryError> {
        let origin = serde_json::to_string(origin).map_err(read)?;
        let mut transaction = self.immediate("update").await?;
        let row = bind_note(bind_written(sqlx::query(UPDATE), scope, reach), note)
            .bind(text)
            .bind(origin)
            .bind(now.millis())
            .fetch_optional(&mut *transaction)
            .await
            .map_err(failed("update"))?;
        let rewritten = match row {
            Some(row) => {
                let updated = MemoryChange::Updated(note_from(&row)?);
                write_records(&mut transaction, scope, &record(&updated), "update").await?;
                Rewritten {
                    memory: updated.into_memory(),
                    changed: true,
                }
            }
            // It holds that text already, or there's no such note to change.
            None => {
                let row = bind_note(bind_written(sqlx::query(WRITTEN), scope, reach), note)
                    .fetch_optional(&mut *transaction)
                    .await
                    .map_err(failed("update"))?;
                Rewritten {
                    memory: row.as_ref().map(note_from).transpose()?.ok_or(MemoryError::NotFound)?,
                    changed: false,
                }
            }
        };
        transaction.commit().await.map_err(failed("update"))?;
        Ok(rewritten)
    }

    async fn delete(
        &self,
        scope: &MemoryScope,
        note: NoteRef<'_>,
        reach: Reach<'_>,
        record: &Record<'_>,
    ) -> Result<Memory, MemoryError> {
        let mut transaction = self.immediate("delete").await?;
        let row = bind_note(bind_written(sqlx::query(DELETE), scope, reach), note)
            .fetch_optional(&mut *transaction)
            .await
            .map_err(failed("delete"))?;
        let deleted = MemoryChange::Forgotten(row.as_ref().map(note_from).transpose()?.ok_or(MemoryError::NotFound)?);
        write_records(&mut transaction, scope, &record(&deleted), "delete").await?;
        transaction.commit().await.map_err(failed("delete"))?;
        Ok(deleted.into_memory())
    }

    async fn delete_scope(
        &self,
        scope: &MemoryScope,
        at_most: u32,
        record: &Record<'_>,
        counted: &Audit<'_, u64>,
    ) -> Result<u64, MemoryError> {
        let mut transaction = self.immediate("delete_scope").await?;
        let rows = bind_scope(sqlx::query(SCOPE_NOTES), scope)
            .bind(at_most)
            .fetch_all(&mut *transaction)
            .await
            .map_err(failed("delete_scope"))?;
        let deleted = bind_scope(sqlx::query(DELETE_SCOPE), scope)
            .bind(at_most)
            .execute(&mut *transaction)
            .await
            .map_err(failed("delete_scope"))?;
        if deleted.rows_affected() != rows.len() as u64 {
            // The transaction holds the database: nothing can come between.
            tracing::error!(event = "memory_notes.scope_moved", operation = "delete_scope");
            return Err(MemoryError::Unavailable);
        }
        for row in &rows {
            let forgotten = MemoryChange::Forgotten(note_from(row)?);
            write_records(&mut transaction, scope, &record(&forgotten), "delete_scope").await?;
        }
        let count = rows.len() as u64;
        if count > 0 {
            crate::audit::record_in_workspace(
                &mut transaction,
                scope.organization_id,
                scope.workspace_id,
                &counted(&count),
            )
            .await
            .map_err(failed("delete_scope"))?;
        }
        transaction.commit().await.map_err(failed("delete_scope"))?;
        Ok(count)
    }

    async fn get(&self, scope: &MemoryScope, id: MemoryId) -> Result<Memory, MemoryError> {
        let row = bind_scope(sqlx::query(FIND), scope)
            .bind(id.uuid())
            .fetch_optional(self.database.pool())
            .await
            .map_err(failed("get"))?;
        row.as_ref().map(note_from).transpose()?.ok_or(MemoryError::NotFound)
    }

    async fn recent(&self, scope: &MemoryScope, audience: &Audience, limit: u32) -> Result<Recent, MemoryError> {
        // One read transaction, so the count and the notes are of one moment.
        let mut transaction = self.database.pool().begin().await.map_err(failed("recent"))?;
        let total: i64 = bind_shown(sqlx::query(COUNT_SHOWN), scope, audience)
            .fetch_one(&mut *transaction)
            .await
            .map_err(failed("recent"))?
            .try_get(0)
            .map_err(read)?;
        let rows = bind_shown(sqlx::query(RECENT), scope, audience)
            .bind(i64::from(limit))
            .fetch_all(&mut *transaction)
            .await
            .map_err(failed("recent"))?;
        transaction.commit().await.map_err(failed("recent"))?;
        Ok(Recent {
            notes: rows.iter().map(note_from).collect::<Result<_, _>>()?,
            total: count(total)?,
        })
    }

    async fn search(
        &self,
        scope: &MemoryScope,
        audience: &Audience,
        words: &[String],
        limit: u32,
    ) -> Result<Vec<Memory>, MemoryError> {
        if words.is_empty() {
            return Ok(Vec::new());
        }
        let mut connection = self.searcher("search").await?;
        let mut transaction = connection.begin().await.map_err(failed("search"))?;
        sqlx::query(EMPTY_FOUND)
            .execute(&mut *transaction)
            .await
            .map_err(failed("search"))?;
        bind_shown(sqlx::query(FILL_FOUND), scope, audience)
            .execute(&mut *transaction)
            .await
            .map_err(failed("search"))?;
        let rows = bind_shown(sqlx::query(SEARCH), scope, audience)
            .bind(every_word(words))
            .bind(i64::from(limit))
            .fetch_all(&mut *transaction)
            .await
            .map_err(failed_quietly("search"))?;
        // Nothing it indexed outlives it.
        transaction.rollback().await.map_err(failed("search"))?;
        rows.iter().map(note_from).collect()
    }

    async fn list(
        &self,
        scope: &MemoryScope,
        words: Option<&[String]>,
        cursor: Option<&str>,
        limit: u32,
    ) -> Result<Page<Memory>, MemoryError> {
        let after = cursor
            .map(|cursor| Position::decode(cursor).ok_or(MemoryError::InvalidCursor))
            .transpose()?;
        if words.is_some_and(<[String]>::is_empty) {
            return Ok(Page {
                items: Vec::new(),
                next_cursor: None,
                total: 0,
            });
        }
        let limit = limit.clamp(1, zupflow_core::memory::MAX_PAGE);
        let query = words.map(every_word);
        let mut connection = self.searcher("list").await?;
        // One read transaction, so the count and the page are of one moment.
        let mut transaction = connection.begin().await.map_err(failed("list"))?;
        if query.is_some() {
            sqlx::query(EMPTY_FOUND)
                .execute(&mut *transaction)
                .await
                .map_err(failed("list"))?;
            bind_scope(sqlx::query(FILL_LISTED), scope)
                .execute(&mut *transaction)
                .await
                .map_err(failed("list"))?;
        }
        let mut counted = QueryBuilder::<Sqlite>::new("SELECT count(*) FROM memory_notes");
        push_listed(&mut counted, scope, query.as_deref());
        let total: i64 = counted
            .build_query_scalar()
            .fetch_one(&mut *transaction)
            .await
            .map_err(failed_quietly("list"))?;
        let mut listed = QueryBuilder::<Sqlite>::new(concat!("SELECT ", columns!(), " FROM memory_notes"));
        push_listed(&mut listed, scope, query.as_deref());
        if let Some(after) = &after {
            listed.push(" AND (created_at < ");
            listed.push_bind(after.created_at);
            listed.push(" OR (created_at = ");
            listed.push_bind(after.created_at);
            listed.push(" AND id < ");
            listed.push_bind(after.id);
            listed.push("))");
        }
        listed.push(" ORDER BY created_at DESC, id DESC LIMIT ");
        listed.push_bind(i64::from(limit) + 1);
        let rows = listed
            .build()
            .fetch_all(&mut *transaction)
            .await
            .map_err(failed_quietly("list"))?;
        // Nothing it indexed outlives it.
        transaction.rollback().await.map_err(failed("list"))?;
        let mut items = rows.iter().map(note_from).collect::<Result<Vec<_>, _>>()?;
        let more = items.len() > limit as usize;
        items.truncate(limit as usize);
        let next_cursor = more
            .then(|| {
                items.last().map(|last| {
                    Position {
                        created_at: last.created_at.millis(),
                        id: last.id.uuid(),
                    }
                    .encode()
                })
            })
            .flatten();
        Ok(Page {
            items,
            next_cursor,
            total: count(total)?,
        })
    }

    async fn delete_all(&self, what: &Forget) -> Result<u64, MemoryError> {
        let query = match *what {
            Forget::Agent {
                organization_id,
                workspace_id,
                agent_id,
            } => sqlx::query(FORGET_AGENT)
                .bind(organization_id.uuid())
                .bind(workspace_id.uuid())
                .bind(agent_id.uuid()),
            Forget::Workspace {
                organization_id,
                workspace_id,
            } => sqlx::query(FORGET_WORKSPACE)
                .bind(organization_id.uuid())
                .bind(workspace_id.uuid()),
            Forget::Type {
                organization_id,
                memory_type_id,
            } => sqlx::query(FORGET_TYPE)
                .bind(organization_id.uuid())
                .bind(memory_type_id.uuid()),
            Forget::Organization { organization_id } => sqlx::query(FORGET_ORGANIZATION).bind(organization_id.uuid()),
        };
        let deleted = query
            .execute(self.database.pool())
            .await
            .map_err(failed("delete_all"))?;
        Ok(deleted.rows_affected())
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use std::sync::Arc;

    use zupflow_conformance::note_store::{Logs, Prepare, Tenant};
    use zupflow_core::ids::{RunId, ToolCallId};
    use zupflow_core::memory::record_nothing;

    use super::*;
    use crate::schema::{Temporary, temporary};
    use crate::{SqliteAuditStore, SqliteWebhookStore};

    /// A tenant whose IDs start at `n`.
    fn tenant(n: u128) -> Tenant {
        Tenant {
            organization_id: OrganizationId::from_uuid(Uuid::from_u128(n)),
            workspace: WorkspaceId::from_uuid(Uuid::from_u128(n + 1)),
            agent: AgentId::from_uuid(Uuid::from_u128(n + 2)),
            other_agent: AgentId::from_uuid(Uuid::from_u128(n + 3)),
            sibling: WorkspaceId::from_uuid(Uuid::from_u128(n + 4)),
            sibling_agent: AgentId::from_uuid(Uuid::from_u128(n + 5)),
            memory_type: MemoryTypeId::from_uuid(Uuid::from_u128(n + 6)),
            other_type: MemoryTypeId::from_uuid(Uuid::from_u128(n + 7)),
        }
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

    async fn save(
        store: &SqliteNoteStore,
        scope: &MemoryScope,
        conversation: Option<ChannelConversationId>,
        n: u32,
        text: &str,
        millis: i64,
    ) -> Memory {
        store
            .insert(
                scope,
                &NewNote {
                    id: MemoryId::generate(),
                    text,
                    channel_conversation_id: conversation,
                    origin: &run_origin(),
                    short_ids: &[ShortId::from_bits(n)],
                },
                200,
                at(millis),
                &record_nothing,
            )
            .await
            .unwrap()
    }

    async fn found(store: &SqliteNoteStore, scope: &MemoryScope, audience: &Audience, words: &[&str]) -> Vec<String> {
        let words: Vec<String> = words.iter().map(|word| (*word).to_owned()).collect();
        store
            .search(scope, audience, &words, 20)
            .await
            .unwrap()
            .into_iter()
            .map(|note| note.text)
            .collect()
    }

    /// Makes the tenant's rows straight in the database: an organization, its
    /// two workspaces and three agents, and two `notes` types.
    fn prepare(database: Database) -> Prepare {
        Arc::new(move |tenant: Tenant| {
            let database = database.clone();
            Box::pin(async move {
                let pool = database.pool();
                let org = tenant.organization_id.uuid();
                sqlx::query("INSERT INTO organizations (id, name, slug, created_at) VALUES (?, 'Test', ?, 0)")
                    .bind(org)
                    .bind(format!("org-{}", org.simple()))
                    .execute(pool)
                    .await
                    .unwrap();
                for (workspace, environment) in [(tenant.workspace, "sandbox"), (tenant.sibling, "production")] {
                    sqlx::query(
                        "INSERT INTO workspaces (id, organization_id, environment, name, slug, created_at) \
                         VALUES (?, ?, ?, 'W', ?, 0)",
                    )
                    .bind(workspace.uuid())
                    .bind(org)
                    .bind(environment)
                    .bind(format!("w-{}", workspace.uuid().simple()))
                    .execute(pool)
                    .await
                    .unwrap();
                }
                for (agent, workspace) in [
                    (tenant.agent, tenant.workspace),
                    (tenant.other_agent, tenant.workspace),
                    (tenant.sibling_agent, tenant.sibling),
                ] {
                    sqlx::query(
                        "INSERT INTO agents (id, organization_id, workspace_id, name, description, instructions, \
                         status, created_at, updated_at) VALUES (?, ?, ?, 'A', '', '', 'active', 0, 0)",
                    )
                    .bind(agent.uuid())
                    .bind(org)
                    .bind(workspace.uuid())
                    .execute(pool)
                    .await
                    .unwrap();
                }
                for (memory_type, name) in [(tenant.memory_type, "Notes"), (tenant.other_type, "Other notes")] {
                    sqlx::query(
                        "INSERT INTO memory_types (id, organization_id, name, kind, settings, writes, created_at, \
                         updated_at) VALUES (?, ?, ?, 'notes', '{\"max_notes\":200,\"recall_characters\":8000}', \
                         'allow', 0, 0)",
                    )
                    .bind(memory_type.uuid())
                    .bind(org)
                    .bind(name)
                    .execute(pool)
                    .await
                    .unwrap();
                }
            })
        })
    }

    #[tokio::test]
    async fn conforms() {
        let Temporary {
            database,
            directory: _directory,
        } = temporary().await;
        zupflow_conformance::note_store::run(
            &SqliteNoteStore::new(database.clone()),
            Logs {
                audit: &SqliteAuditStore::new(database.clone()),
                webhooks: &SqliteWebhookStore::new(database.clone()),
            },
            prepare(database),
        )
        .await;
    }

    /// BM25 over the notes a search may find: the note holding the word
    /// most, for its length, first; two ranked the same, the newest first.
    #[tokio::test]
    async fn ranks_best_first_then_newest() {
        let Temporary {
            database,
            directory: _directory,
        } = temporary().await;
        let t = tenant(100);
        prepare(database.clone())(t).await;
        let store = SqliteNoteStore::new(database);
        let scope = t.scope();
        let workspace = Audience::Workspace;
        save(&store, &scope, None, 1, "invoice invoice invoice", 1).await;
        save(&store, &scope, None, 2, "An invoice is due for the team this week.", 2).await;
        assert_eq!(
            found(&store, &scope, &workspace, &["invoice"]).await,
            ["invoice invoice invoice", "An invoice is due for the team this week."],
            "best first"
        );
        // Ranked the same by these notes: alpha and beta are each in two.
        save(&store, &scope, None, 3, "alpha alpha beta", 3).await;
        save(&store, &scope, None, 4, "alpha beta beta", 4).await;
        for (n, filler) in (10..).zip(["gamma one", "gamma two", "gamma three", "gamma four"]) {
            save(&store, &scope, None, n, filler, 5).await;
        }
        assert_eq!(
            found(&store, &scope, &workspace, &["alpha", "beta"]).await,
            ["alpha beta beta", "alpha alpha beta"],
            "the newest first"
        );
        // A direct conversation's search weighs its own notes beside the
        // workspace's: the shorter note first.
        let conversation = ChannelConversationId::generate();
        save(&store, &scope, None, 20, "The finance team meets on Mondays.", 6).await;
        save(&store, &scope, Some(conversation), 21, "Finance prefers email.", 7).await;
        assert_eq!(
            found(&store, &scope, &Audience::Direct(conversation), &["finance"]).await,
            ["Finance prefers email.", "The finance team meets on Mondays."]
        );
        // Changed, a note ranks by its new text, from when it changed.
        let invoice = store
            .list(&scope, Some(&["invoice".to_owned()]), None, 10)
            .await
            .unwrap()
            .items;
        let first = invoice.last().unwrap();
        store
            .update(
                &scope,
                NoteRef::Id(first.id),
                Reach::Everything,
                &run_origin(),
                "One invoice, then nothing.",
                at(8),
                &record_nothing,
            )
            .await
            .unwrap();
        assert_eq!(
            found(&store, &scope, &workspace, &["invoice"]).await,
            [
                "One invoice, then nothing.",
                "An invoice is due for the team this week."
            ],
            "by its new text: the shorter first"
        );
    }

    #[tokio::test]
    async fn serves_the_notes_kind() {
        let Temporary {
            database,
            directory: _directory,
        } = temporary().await;
        zupflow_conformance::memory_provider::run_notes(
            Arc::new(SqliteNoteStore::new(database.clone())),
            Logs {
                audit: &SqliteAuditStore::new(database.clone()),
                webhooks: &SqliteWebhookStore::new(database.clone()),
            },
            prepare(database),
        )
        .await;
    }

    /// The database itself keeps a note to its scope: its agent in its
    /// workspace, its type in its organization, and it goes with any of
    /// them.
    #[tokio::test]
    async fn refuses_dangling_references_itself() {
        let Temporary {
            database,
            directory: _directory,
        } = temporary().await;
        let pool = database.pool();
        let (t, other) = (tenant(100), tenant(200));
        let prepare = prepare(database.clone());
        prepare(t).await;
        prepare(other).await;
        let note = |organization: OrganizationId, workspace: WorkspaceId, agent: AgentId, memory_type: MemoryTypeId| {
            sqlx::query(
                "INSERT INTO memory_notes (id, organization_id, workspace_id, agent_id, memory_type_id, short_id, \
                 text, origin, created_at, updated_at) \
                 VALUES (?, ?, ?, ?, ?, 'abcdef', 'A note.', '{\"type\":\"member\"}', 0, 0)",
            )
            .bind(Uuid::now_v7())
            .bind(organization.uuid())
            .bind(workspace.uuid())
            .bind(agent.uuid())
            .bind(memory_type.uuid())
            .execute(pool)
        };
        let org = t.organization_id;
        assert!(
            note(org, t.workspace, t.sibling_agent, t.memory_type).await.is_err(),
            "an agent of another workspace"
        );
        assert!(
            note(org, t.workspace, t.agent, other.memory_type).await.is_err(),
            "another organization's type"
        );
        assert!(
            note(other.organization_id, t.workspace, t.agent, t.memory_type)
                .await
                .is_err()
        );
        note(org, t.workspace, t.agent, t.memory_type).await.unwrap();
        assert!(
            note(org, t.workspace, t.agent, t.memory_type).await.is_err(),
            "a short ID once in its scope"
        );
        note(org, t.workspace, t.agent, t.other_type).await.unwrap();
        note(org, t.workspace, t.other_agent, t.memory_type).await.unwrap();
        note(org, t.sibling, t.sibling_agent, t.memory_type).await.unwrap();
        note(other.organization_id, other.workspace, other.agent, other.memory_type)
            .await
            .unwrap();
        let count = |organization: OrganizationId| async move {
            sqlx::query_scalar::<_, i64>("SELECT count(*) FROM memory_notes WHERE organization_id = ?")
                .bind(organization.uuid())
                .fetch_one(pool)
                .await
                .unwrap()
        };
        assert_eq!(count(org).await, 4);
        for (statement, id, left) in [
            (
                "DELETE FROM agents WHERE organization_id = ? AND id = ?",
                t.agent.uuid(),
                2,
            ),
            (
                "DELETE FROM workspaces WHERE organization_id = ? AND id = ?",
                t.sibling.uuid(),
                1,
            ),
            (
                "DELETE FROM memory_types WHERE organization_id = ? AND id = ?",
                t.memory_type.uuid(),
                0,
            ),
        ] {
            sqlx::query(statement)
                .bind(org.uuid())
                .bind(id)
                .execute(pool)
                .await
                .unwrap();
            assert_eq!(count(org).await, left, "{statement}");
        }
        note(org, t.workspace, t.other_agent, t.other_type).await.unwrap();
        sqlx::query("DELETE FROM organizations WHERE id = ?")
            .bind(org.uuid())
            .execute(pool)
            .await
            .unwrap();
        assert_eq!(count(org).await, 0, "and with its organization");
        assert_eq!(count(other.organization_id).await, 1, "never another's");
    }
}
