//! Channel runs: sessions in threads, dispatches, and the runs they start,
//! continue and collect. The channel store and the run store both call in
//! here, inside their own transactions, so what must hold across concurrent
//! changes (one active run per session, the breaker's count, a waiting
//! message taken in once) holds under SQLite's write lock.
//!
//! Every statement repeats the organization and the workspace beside any
//! unique ID; the scope audit in `channels.rs` reads this file too.
//!
//! A session's runs share one conversation. A run is admitted with its
//! place in the session (`session_sequence`), the earliest run its prompt
//! includes (`history_start_run_id`) and the session's tools (`offer`), and
//! its prompt is the transcripts of the runs from that start, each kept as
//! sent when its run ended, then its own ([`session_context`]): every run
//! after the first starts with what the one before sent, so the provider's
//! cache holds across runs. The start
//! moves only when the history would go over the session's budget, by
//! whole runs and a quarter of the budget at least
//! ([`zupflow_core::runs::history::trimmed_start`]), so the cache misses
//! once per trim.

use std::collections::{BTreeMap, BTreeSet, HashSet};

use serde_json::json;
use sqlx::sqlite::SqliteRow;
use sqlx::{Row, SqliteConnection};
use uuid::Uuid;
use zupflow_core::channels::handling::DispatchReason;
use zupflow_core::channels::store::{
    BreakerOutcome, ChannelSession, ChannelSessionKey, DispatchOutcome, NewDispatch, SessionTemplate,
};
use zupflow_core::channels::thread::{
    LineAuthor, MAX_TAKEN, MentionNames, STEER_HEADER, ThreadLine, change_note, conversation_mentions, plain_text,
    render_turn,
};
use zupflow_core::channels::{ChannelProviderKind, ConversationKind, MidRun};
use zupflow_core::ids::{
    AgentChannelId, AgentId, ChannelConnectionId, ChannelConversationId, ChannelMessageId, ModelConnectionId, ModelId,
    OrganizationId, RunId, SessionId, WorkspaceId,
};
use zupflow_core::model_store::RunModel;
use zupflow_core::models::Message;
use zupflow_core::runs::history::{transcript, trimmed_start, weight as weight_of};
use zupflow_core::runs::names::Offer;
use zupflow_core::runs::reducer::admit;
use zupflow_core::runs::state::{ChannelOrigin, SessionContext, SteeredTurn};
use zupflow_core::runs::store::StoreError;
use zupflow_core::runs::transition::{SeenMessage, Steer};
use zupflow_core::runs::{ChannelAdmission, NewRun, RunStatus, RunTrigger, TakenIn};
use zupflow_core::time::Timestamp;
use zupflow_core::webhooks::WebhookEventType;

const MINUTE_MS: i64 = 60 * 1000;

/// The most thread messages one rendered turn reads before the newest it
/// takes in: older ones wouldn't fit in a turn anyway.
const MAX_CONTEXT_MESSAGES: i64 = 200;

fn failed(operation: &'static str) -> impl FnOnce(sqlx::Error) -> StoreError {
    move |error| {
        tracing::error!(event = "channel_runs.database_error", operation, %error);
        StoreError::Unavailable
    }
}

fn corrupt(what: &'static str) -> StoreError {
    tracing::error!(event = "channel_runs.unreadable_row", what);
    StoreError::Unavailable
}

fn unreadable(error: impl std::fmt::Display) -> StoreError {
    tracing::error!(event = "channel_runs.unreadable_row", %error);
    StoreError::Unavailable
}

fn get<'r, T: sqlx::Decode<'r, sqlx::Sqlite> + sqlx::Type<sqlx::Sqlite>>(
    row: &'r SqliteRow,
    column: &str,
) -> Result<T, StoreError> {
    row.try_get(column).map_err(unreadable)
}

fn small(value: i64) -> Result<u32, StoreError> {
    u32::try_from(value).map_err(unreadable)
}

fn to_json(value: &impl serde::Serialize) -> Result<String, StoreError> {
    serde_json::to_string(value).map_err(unreadable)
}

// --- Sessions --------------------------------------------------------------

/// A session as stored, with what's fixed for its runs.
struct SessionRow {
    session: ChannelSession,
    template: Option<SessionTemplate>,
    seen: Option<(Uuid, i64)>,
    /// When its turns last noted the thread's edits and deletes.
    changes_noted_at: Option<i64>,
}

macro_rules! session_select {
    () => {
        "SELECT s.id, s.agent_channel_id, s.conversation_id, s.thread_key, s.session_id, s.active_run_id, \
         s.last_run_id, s.run_count, s.model, s.model_id, s.model_connection_id, s.instructions, s.offer, \
         s.history_budget, s.seen_message_id, s.seen_at, s.changes_noted_at FROM channel_sessions s"
    };
}

fn session_from(row: &SqliteRow) -> Result<SessionRow, StoreError> {
    let template = match (
        get::<Option<String>>(row, "model")?,
        get::<Option<Uuid>>(row, "model_id")?,
        get::<Option<Uuid>>(row, "model_connection_id")?,
        get::<Option<String>>(row, "instructions")?,
        get::<Option<String>>(row, "offer")?,
        get::<Option<i64>>(row, "history_budget")?,
    ) {
        (Some(model), Some(model_id), Some(connection_id), Some(instructions), Some(offer), Some(budget)) => {
            Some(SessionTemplate {
                model: RunModel {
                    model_id: ModelId::from_uuid(model_id),
                    connection_id: ModelConnectionId::from_uuid(connection_id),
                    provider_model_id: model,
                },
                instructions,
                offer: serde_json::from_str::<Offer>(&offer).map_err(unreadable)?,
                history_budget: u64::try_from(budget).map_err(unreadable)?,
            })
        }
        (None, None, None, None, None, None) => None,
        _ => return Err(corrupt("a session's template")),
    };
    let seen = match (
        get::<Option<Uuid>>(row, "seen_message_id")?,
        get::<Option<i64>>(row, "seen_at")?,
    ) {
        (Some(id), Some(at)) => Some((id, at)),
        (None, None) => None,
        _ => return Err(corrupt("a session's last message")),
    };
    Ok(SessionRow {
        session: ChannelSession {
            key: ChannelSessionKey::from_uuid(get(row, "id")?),
            agent_channel_id: AgentChannelId::from_uuid(get(row, "agent_channel_id")?),
            conversation_id: ChannelConversationId::from_uuid(get(row, "conversation_id")?),
            thread_key: get(row, "thread_key")?,
            session_id: SessionId::from_uuid(get(row, "session_id")?),
            active_run_id: get::<Option<Uuid>>(row, "active_run_id")?.map(RunId::from_uuid),
            last_run_id: get::<Option<Uuid>>(row, "last_run_id")?.map(RunId::from_uuid),
            run_count: small(get(row, "run_count")?)?,
        },
        template,
        seen,
        changes_noted_at: get(row, "changes_noted_at")?,
    })
}

async fn session_by_key(
    connection: &mut SqliteConnection,
    organization_id: OrganizationId,
    workspace_id: WorkspaceId,
    key: Uuid,
) -> Result<Option<SessionRow>, StoreError> {
    let row = sqlx::query(concat!(
        session_select!(),
        " WHERE s.organization_id = ? AND s.workspace_id = ? AND s.id = ?"
    ))
    .bind(organization_id.uuid())
    .bind(workspace_id.uuid())
    .bind(key)
    .fetch_optional(&mut *connection)
    .await
    .map_err(failed("find_session"))?;
    row.as_ref().map(session_from).transpose()
}

/// The agent channel's session in a thread, if it has one.
pub(crate) async fn find_session_in(
    connection: &mut SqliteConnection,
    organization_id: OrganizationId,
    workspace_id: WorkspaceId,
    agent_channel_id: AgentChannelId,
    conversation_id: ChannelConversationId,
    thread_key: &str,
) -> Result<Option<ChannelSession>, StoreError> {
    let row = sqlx::query(concat!(
        session_select!(),
        " WHERE s.organization_id = ? AND s.workspace_id = ? AND s.agent_channel_id = ? \
         AND s.conversation_id = ? AND s.thread_key = ?"
    ))
    .bind(organization_id.uuid())
    .bind(workspace_id.uuid())
    .bind(agent_channel_id.uuid())
    .bind(conversation_id.uuid())
    .bind(thread_key)
    .fetch_optional(&mut *connection)
    .await
    .map_err(failed("find_session"))?;
    Ok(row.as_ref().map(session_from).transpose()?.map(|row| row.session))
}

/// The session, made on first use.
async fn open_session(
    connection: &mut SqliteConnection,
    organization_id: OrganizationId,
    workspace_id: WorkspaceId,
    agent_channel_id: AgentChannelId,
    conversation_id: ChannelConversationId,
    thread_key: &str,
    now: Timestamp,
) -> Result<SessionRow, StoreError> {
    sqlx::query(
        "INSERT INTO channel_sessions (id, organization_id, workspace_id, agent_channel_id, conversation_id, \
         thread_key, session_id, created_at, updated_at) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?) \
         ON CONFLICT (organization_id, workspace_id, agent_channel_id, conversation_id, thread_key) DO NOTHING",
    )
    .bind(ChannelSessionKey::generate().uuid())
    .bind(organization_id.uuid())
    .bind(workspace_id.uuid())
    .bind(agent_channel_id.uuid())
    .bind(conversation_id.uuid())
    .bind(thread_key)
    .bind(SessionId::generate().uuid())
    .bind(now.millis())
    .bind(now.millis())
    .execute(&mut *connection)
    .await
    .map_err(failed("open_session"))?;
    let row = sqlx::query(concat!(
        session_select!(),
        " WHERE s.organization_id = ? AND s.workspace_id = ? AND s.agent_channel_id = ? \
         AND s.conversation_id = ? AND s.thread_key = ?"
    ))
    .bind(organization_id.uuid())
    .bind(workspace_id.uuid())
    .bind(agent_channel_id.uuid())
    .bind(conversation_id.uuid())
    .bind(thread_key)
    .fetch_one(&mut *connection)
    .await
    .map_err(failed("open_session"))?;
    session_from(&row)
}

// --- The breaker -------------------------------------------------------------

/// Counts one more dispatch in the conversation against the organization's
/// breaker: one over `breaker_runs` within the window pauses agents there
/// for `breaker_pause_minutes` (announced).
async fn count_breaker(
    connection: &mut SqliteConnection,
    organization_id: OrganizationId,
    workspace_id: WorkspaceId,
    conversation_id: ChannelConversationId,
    now: Timestamp,
) -> Result<BreakerOutcome, StoreError> {
    let row = sqlx::query(
        "SELECT connection_id, breaker_window_start, breaker_count, paused_until FROM channel_conversations \
         WHERE organization_id = ? AND workspace_id = ? AND id = ?",
    )
    .bind(organization_id.uuid())
    .bind(workspace_id.uuid())
    .bind(conversation_id.uuid())
    .fetch_optional(&mut *connection)
    .await
    .map_err(failed("count_breaker"))?
    .ok_or(StoreError::NotFound)?;
    if let Some(until) = get::<Option<i64>>(&row, "paused_until")?
        && until > now.millis()
    {
        return Ok(BreakerOutcome::Paused {
            until: Timestamp::from_millis(until),
        });
    }
    let settings = crate::channels::breaker_settings(connection, organization_id).await?;
    let window_start: Option<i64> = get(&row, "breaker_window_start")?;
    let count: i64 = get(&row, "breaker_count")?;
    let window_ms = i64::from(settings.breaker_window_minutes) * MINUTE_MS;
    let in_window = window_start.is_some_and(|start| start.saturating_add(window_ms) > now.millis());
    let (start, count) = if in_window {
        (window_start.unwrap_or(now.millis()), count.saturating_add(1))
    } else {
        (now.millis(), 1)
    };
    let outcome = if count > i64::from(settings.breaker_runs) {
        BreakerOutcome::Tripped {
            paused_until: Timestamp::from_millis(
                now.millis()
                    .saturating_add(i64::from(settings.breaker_pause_minutes) * MINUTE_MS),
            ),
        }
    } else {
        BreakerOutcome::Counted { count: small(count)? }
    };
    // A trip starts a new window once the pause is over.
    let (start, count, paused_until) = match outcome {
        BreakerOutcome::Tripped { paused_until } => (None, 0, Some(paused_until.millis())),
        _ => (Some(start), count, None),
    };
    sqlx::query(
        "UPDATE channel_conversations SET breaker_window_start = ?, breaker_count = ?, paused_until = ?, \
         updated_at = ? WHERE organization_id = ? AND workspace_id = ? AND id = ?",
    )
    .bind(start)
    .bind(count)
    .bind(paused_until)
    .bind(now.millis())
    .bind(organization_id.uuid())
    .bind(workspace_id.uuid())
    .bind(conversation_id.uuid())
    .execute(&mut *connection)
    .await
    .map_err(failed("count_breaker"))?;
    if let BreakerOutcome::Tripped { paused_until } = outcome {
        let connection_id = ChannelConnectionId::from_uuid(get(&row, "connection_id")?);
        crate::webhooks::record_lifecycle(
            connection,
            organization_id,
            workspace_id,
            WebhookEventType::ChannelConversationPaused,
            json!({
                "object": "channel_conversation", "id": conversation_id, "connection_id": connection_id,
                "paused_until": paused_until,
            }),
            now,
        )
        .await
        .map_err(|error| {
            tracing::error!(event = "channel_runs.announcement_not_recorded", %error);
            StoreError::Unavailable
        })?;
    }
    Ok(outcome)
}

// --- Dispatches --------------------------------------------------------------

async fn insert_dispatch(
    connection: &mut SqliteConnection,
    organization_id: OrganizationId,
    workspace_id: WorkspaceId,
    dispatch: &NewDispatch,
    session: Option<ChannelSessionKey>,
    outcome: &str,
    now: Timestamp,
) -> Result<(), StoreError> {
    sqlx::query(
        "INSERT INTO channel_dispatches (id, organization_id, workspace_id, message_id, agent_channel_id, \
         channel_session_id, origin_key, thread_origin_key, agent_hop, handler, reason, outcome, created_at, \
         updated_at) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
    )
    .bind(Uuid::now_v7())
    .bind(organization_id.uuid())
    .bind(workspace_id.uuid())
    .bind(dispatch.message_id.uuid())
    .bind(dispatch.agent_channel_id.uuid())
    .bind(session.map(|session| session.uuid()))
    .bind(&dispatch.origin_key)
    .bind(&dispatch.thread_origin_key)
    .bind(i64::from(dispatch.agent_hop))
    .bind(dispatch.handler.as_str())
    .bind(dispatch.reason.as_str())
    .bind(outcome)
    .bind(now.millis())
    .bind(now.millis())
    .execute(&mut *connection)
    .await
    .map_err(failed("insert_dispatch"))?;
    Ok(())
}

/// One dispatch, as [`zupflow_core::channels::store::ChannelStore::dispatch`]
/// documents, inside the caller's transaction.
pub(crate) async fn dispatch_in(
    connection: &mut SqliteConnection,
    organization_id: OrganizationId,
    workspace_id: WorkspaceId,
    dispatch: &NewDispatch,
    template: Option<&SessionTemplate>,
    now: Timestamp,
) -> Result<DispatchOutcome, StoreError> {
    let seen = sqlx::query(
        "SELECT 1 FROM channel_dispatches \
         WHERE organization_id = ? AND workspace_id = ? AND agent_channel_id = ? AND origin_key = ?",
    )
    .bind(organization_id.uuid())
    .bind(workspace_id.uuid())
    .bind(dispatch.agent_channel_id.uuid())
    .bind(&dispatch.origin_key)
    .fetch_optional(&mut *connection)
    .await
    .map_err(failed("dispatch"))?;
    if seen.is_some() {
        return Ok(DispatchOutcome::Duplicate);
    }
    // The message is on the agent channel's own connection, in this scope,
    // and, read again under the write lock, its agent is still active and
    // its connection connected: the job decided on an earlier read.
    let message = sqlx::query(
        "SELECT g.conversation_id, g.thread_key, g.created_at, a.status AS agent_status, \
         c.status AS connection_status, v.paused_until FROM channel_messages g JOIN agent_channels h \
         ON h.organization_id = g.organization_id AND h.workspace_id = g.workspace_id \
         AND h.connection_id = g.connection_id \
         JOIN agents a ON a.organization_id = h.organization_id AND a.workspace_id = h.workspace_id \
         AND a.id = h.agent_id \
         JOIN channel_connections c ON c.organization_id = h.organization_id AND c.workspace_id = h.workspace_id \
         AND c.id = h.connection_id \
         JOIN channel_conversations v ON v.organization_id = g.organization_id AND v.workspace_id = g.workspace_id \
         AND v.id = g.conversation_id \
         WHERE g.organization_id = ? AND g.workspace_id = ? AND g.id = ? AND h.id = ?",
    )
    .bind(organization_id.uuid())
    .bind(workspace_id.uuid())
    .bind(dispatch.message_id.uuid())
    .bind(dispatch.agent_channel_id.uuid())
    .fetch_optional(&mut *connection)
    .await
    .map_err(failed("dispatch"))?
    .ok_or(StoreError::NotFound)?;
    if get::<String>(&message, "agent_status")? != "active"
        || get::<String>(&message, "connection_status")? != "connected"
    {
        return Ok(DispatchOutcome::Inactive);
    }
    let conversation_id = ChannelConversationId::from_uuid(get(&message, "conversation_id")?);
    let thread_key: String = get(&message, "thread_key")?;
    let at: i64 = get(&message, "created_at")?;
    if let Some(until) = get::<Option<i64>>(&message, "paused_until")?
        && until > now.millis()
    {
        insert_dispatch(connection, organization_id, workspace_id, dispatch, None, "paused", now).await?;
        return Ok(DispatchOutcome::Paused {
            until: Timestamp::from_millis(until),
            tripped: false,
        });
    }
    let session = open_session(
        connection,
        organization_id,
        workspace_id,
        dispatch.agent_channel_id,
        conversation_id,
        &thread_key,
        now,
    )
    .await?;
    // An earlier turn read it already (its job ran late): that turn, which
    // saw it as it is, answered it.
    if session
        .seen
        .is_some_and(|(id, seen_at)| (at, dispatch.message_id.uuid()) <= (seen_at, id))
    {
        insert_dispatch(
            connection,
            organization_id,
            workspace_id,
            dispatch,
            Some(session.session.key),
            "read",
            now,
        )
        .await?;
        return Ok(DispatchOutcome::AlreadyRead);
    }
    insert_dispatch(
        connection,
        organization_id,
        workspace_id,
        dispatch,
        Some(session.session.key),
        "waiting",
        now,
    )
    .await?;
    if session.session.active_run_id.is_some() {
        return Ok(DispatchOutcome::Waiting);
    }
    Ok(
        match start_session_run(
            connection,
            organization_id,
            workspace_id,
            &session,
            template,
            TakenIn::Started,
            now,
        )
        .await?
        {
            Start::Started(run_id) => DispatchOutcome::Started { run_id },
            Start::Nothing => DispatchOutcome::Waiting,
            Start::Paused { until, tripped } => DispatchOutcome::Paused { until, tripped },
        },
    )
}

/// A waiting dispatch, with its message's place in the thread.
struct Waiting {
    id: Uuid,
    message: Uuid,
    reason: DispatchReason,
    at: i64,
}

/// Up to [`MAX_TAKEN`] dispatches waiting in the session, oldest message first.
async fn waiting_in(
    connection: &mut SqliteConnection,
    organization_id: OrganizationId,
    workspace_id: WorkspaceId,
    session: ChannelSessionKey,
) -> Result<Vec<Waiting>, StoreError> {
    sqlx::query(
        "SELECT d.id, d.message_id, d.reason, g.created_at FROM channel_dispatches d JOIN channel_messages g \
         ON g.organization_id = d.organization_id AND g.workspace_id = d.workspace_id AND g.id = d.message_id \
         WHERE d.organization_id = ? AND d.workspace_id = ? AND d.channel_session_id = ? AND d.outcome = 'waiting' \
         ORDER BY g.created_at, g.id LIMIT ?",
    )
    .bind(organization_id.uuid())
    .bind(workspace_id.uuid())
    .bind(session.uuid())
    .bind(i64::from(MAX_TAKEN))
    .fetch_all(&mut *connection)
    .await
    .map_err(failed("waiting"))?
    .iter()
    .map(|row| {
        let reason: String = get(row, "reason")?;
        Ok(Waiting {
            id: get(row, "id")?,
            message: get(row, "message_id")?,
            reason: DispatchReason::parse(&reason).ok_or_else(|| corrupt("a dispatch's reason"))?,
            at: get(row, "created_at")?,
        })
    })
    .collect()
}

/// Marks the taken dispatches as taken in by `run_id`, each only while it
/// still waits in the session: all of them, or the store refuses.
async fn take(
    connection: &mut SqliteConnection,
    organization_id: OrganizationId,
    workspace_id: WorkspaceId,
    session: ChannelSessionKey,
    dispatches: &[Uuid],
    (outcome, run_id): (TakenIn, RunId),
    now: Timestamp,
) -> Result<(), StoreError> {
    let outcome = match outcome {
        TakenIn::Started => "started",
        TakenIn::Steered => "steered",
        TakenIn::Collected => "collected",
    };
    // Matched by their hex, as the JSON list carries them.
    let ids: Vec<String> = dispatches.iter().map(|id| id.simple().to_string()).collect();
    let taken = sqlx::query(
        "UPDATE channel_dispatches SET outcome = ?, run_id = ?, updated_at = ? \
         WHERE organization_id = ? AND workspace_id = ? AND channel_session_id = ? AND outcome = 'waiting' \
         AND lower(hex(id)) IN (SELECT value FROM json_each(?))",
    )
    .bind(outcome)
    .bind(run_id.uuid())
    .bind(now.millis())
    .bind(organization_id.uuid())
    .bind(workspace_id.uuid())
    .bind(session.uuid())
    .bind(to_json(&ids)?)
    .execute(&mut *connection)
    .await
    .map_err(failed("take_dispatches"))?;
    if taken.rows_affected() != u64::try_from(dispatches.len()).unwrap_or(u64::MAX) {
        return Err(StoreError::Refused("thread messages taken in that no longer wait"));
    }
    Ok(())
}

/// Splits the waiting dispatches into those whose message the run session
/// read in an earlier turn (at or before `seen`, its last message: its job
/// ran late) and the rest, each kept in order.
fn unread(waiting: Vec<Waiting>, seen: Option<(Uuid, i64)>) -> (Vec<Waiting>, Vec<Waiting>) {
    waiting
        .into_iter()
        .partition(|waiting| seen.is_some_and(|(id, at)| (waiting.at, waiting.message) <= (at, id)))
}

/// Settles dispatches still waiting in the session without a run: `read`
/// or `paused`. All of them, or `None` for every one waiting there.
async fn settle_waiting(
    connection: &mut SqliteConnection,
    organization_id: OrganizationId,
    workspace_id: WorkspaceId,
    session: ChannelSessionKey,
    dispatches: Option<&[Uuid]>,
    outcome: &'static str,
    now: Timestamp,
) -> Result<(), StoreError> {
    let ids: Option<Vec<String>> = dispatches.map(|ids| ids.iter().map(|id| id.simple().to_string()).collect());
    sqlx::query(
        "UPDATE channel_dispatches SET outcome = ?, updated_at = ? \
         WHERE organization_id = ? AND workspace_id = ? AND channel_session_id = ? AND outcome = 'waiting' \
         AND (? IS NULL OR lower(hex(id)) IN (SELECT value FROM json_each(?)))",
    )
    .bind(outcome)
    .bind(now.millis())
    .bind(organization_id.uuid())
    .bind(workspace_id.uuid())
    .bind(session.uuid())
    .bind(ids.as_ref().map(to_json).transpose()?)
    .bind(ids.as_ref().map(to_json).transpose()?)
    .execute(&mut *connection)
    .await
    .map_err(failed("settle_dispatches"))?;
    Ok(())
}

// --- Rendering ---------------------------------------------------------------

/// What a session renders thread messages with: its connection's provider
/// and identity, and the identities of the workspace's other agents in the
/// same team.
struct Place {
    provider: ChannelProviderKind,
    connection_id: ChannelConnectionId,
    agent_id: AgentId,
    /// Every live identity in the team, by provider ID (its user and its
    /// bot ID): this agent's and the other agents'.
    identities: BTreeMap<String, String>,
    own_identity: String,
    own_bot_id: Option<String>,
    mid_run: MidRun,
}

async fn place_of(
    connection: &mut SqliteConnection,
    organization_id: OrganizationId,
    workspace_id: WorkspaceId,
    agent_channel_id: AgentChannelId,
) -> Result<Place, StoreError> {
    let row = sqlx::query(
        "SELECT c.id, c.provider, c.team_id, c.identity_external_id, c.identity_bot_id, h.agent_id, h.mid_run \
         FROM agent_channels h JOIN channel_connections c ON c.organization_id = h.organization_id \
         AND c.workspace_id = h.workspace_id AND c.id = h.connection_id \
         WHERE h.organization_id = ? AND h.workspace_id = ? AND h.id = ?",
    )
    .bind(organization_id.uuid())
    .bind(workspace_id.uuid())
    .bind(agent_channel_id.uuid())
    .fetch_optional(&mut *connection)
    .await
    .map_err(failed("place"))?
    .ok_or(StoreError::NotFound)?;
    let provider: String = get(&row, "provider")?;
    let mid_run: String = get(&row, "mid_run")?;
    let team: String = get(&row, "team_id")?;
    let identities = team_identities(connection, organization_id, workspace_id, &team).await?;
    Ok(Place {
        provider: ChannelProviderKind::parse(&provider).ok_or_else(|| corrupt("a connection's provider"))?,
        connection_id: ChannelConnectionId::from_uuid(get(&row, "id")?),
        agent_id: AgentId::from_uuid(get(&row, "agent_id")?),
        identities,
        own_identity: get(&row, "identity_external_id")?,
        own_bot_id: get(&row, "identity_bot_id")?,
        mid_run: MidRun::parse(&mid_run).ok_or_else(|| corrupt("an agent channel's mid-run rule"))?,
    })
}

/// Every live identity of the workspace's connections in `team`, by provider
/// ID (its user and its bot ID), each named by its handle.
pub(crate) async fn team_identities(
    connection: &mut SqliteConnection,
    organization_id: OrganizationId,
    workspace_id: WorkspaceId,
    team: &str,
) -> Result<BTreeMap<String, String>, StoreError> {
    let mut identities = BTreeMap::new();
    for row in sqlx::query(
        "SELECT identity_external_id, identity_bot_id, handle FROM channel_connections \
         WHERE organization_id = ? AND workspace_id = ? AND team_id = ? AND status <> 'disconnected'",
    )
    .bind(organization_id.uuid())
    .bind(workspace_id.uuid())
    .bind(team)
    .fetch_all(&mut *connection)
    .await
    .map_err(failed("place"))?
    {
        let handle: String = get(&row, "handle")?;
        if let Some(bot_id) = get::<Option<String>>(&row, "identity_bot_id")? {
            identities.insert(bot_id, handle.clone());
        }
        identities.insert(get::<String>(&row, "identity_external_id")?, handle);
    }
    Ok(identities)
}

/// The names a connection's messages are rendered with: `identities` (the
/// team's), then the people the connection knows by the provider IDs in
/// `people`, and the conversations it knows by name among `channels`.
pub(crate) async fn mention_names_in(
    connection: &mut SqliteConnection,
    (organization_id, workspace_id, connection_id): (OrganizationId, WorkspaceId, ChannelConnectionId),
    identities: BTreeMap<String, String>,
    people: &[&String],
    channels: &BTreeSet<String>,
) -> Result<MentionNames, StoreError> {
    let mut names = MentionNames {
        people: identities,
        conversations: BTreeMap::new(),
    };
    if !people.is_empty() {
        let rows = sqlx::query(
            "SELECT external_id, display_name, handle FROM channel_people \
             WHERE organization_id = ? AND workspace_id = ? AND connection_id = ? \
             AND external_id IN (SELECT value FROM json_each(?))",
        )
        .bind(organization_id.uuid())
        .bind(workspace_id.uuid())
        .bind(connection_id.uuid())
        .bind(to_json(&people)?)
        .fetch_all(&mut *connection)
        .await
        .map_err(failed("render"))?;
        for row in &rows {
            let external_id: String = get(row, "external_id")?;
            let name: Option<String> = get::<Option<String>>(row, "display_name")?.or(get(row, "handle")?);
            if let Some(name) = name {
                names.people.entry(external_id).or_insert(name);
            }
        }
    }
    if !channels.is_empty() {
        let rows = sqlx::query(
            "SELECT external_id, name FROM channel_conversations \
             WHERE organization_id = ? AND workspace_id = ? AND connection_id = ? AND name IS NOT NULL \
             AND external_id IN (SELECT value FROM json_each(?))",
        )
        .bind(organization_id.uuid())
        .bind(workspace_id.uuid())
        .bind(connection_id.uuid())
        .bind(to_json(channels)?)
        .fetch_all(&mut *connection)
        .await
        .map_err(failed("render"))?;
        for row in &rows {
            names.conversations.insert(get(row, "external_id")?, get(row, "name")?);
        }
    }
    Ok(names)
}

/// A thread message as read for a turn.
struct Read {
    id: Uuid,
    at: i64,
    outbound: bool,
    text: String,
    mentions: Vec<String>,
    author: Option<(String, Option<String>, Option<String>, bool)>,
}

fn read_from(row: &SqliteRow) -> Result<Read, StoreError> {
    let direction: String = get(row, "direction")?;
    let mentions: String = get(row, "mentions")?;
    let author = match get::<Option<String>>(row, "person_external_id")? {
        Some(external_id) => Some((
            external_id,
            get(row, "person_display_name")?,
            get(row, "person_handle")?,
            get::<Option<bool>>(row, "person_is_automated")?.unwrap_or(false),
        )),
        None => None,
    };
    Ok(Read {
        id: get(row, "id")?,
        at: get(row, "created_at")?,
        outbound: direction == "outbound",
        text: get(row, "text")?,
        mentions: serde_json::from_str(&mentions).map_err(unreadable)?,
        author,
    })
}

macro_rules! thread_select {
    () => {
        "SELECT g.id, g.created_at, g.direction, coalesce(g.edited_text, g.text) AS text, g.mentions, \
         p.external_id AS person_external_id, \
         p.display_name AS person_display_name, p.handle AS person_handle, p.is_automated AS person_is_automated \
         FROM channel_messages g LEFT JOIN channel_people p ON p.organization_id = g.organization_id \
         AND p.workspace_id = g.workspace_id AND p.connection_id = g.connection_id AND p.id = g.author_person_id"
    };
}

/// The turn the taken dispatches make: the thread's messages after the
/// session's last one up to the newest taken, and the newest message the
/// turn includes. Each message is in one turn of a run session at most:
/// the taken ones must be after the session's last one ([`unread`]). A
/// message is to the agent by what it is, whichever turn reads it: in a
/// DM, mentioning the agent, or taken by this turn's dispatches.
#[allow(clippy::too_many_arguments)]
async fn render(
    connection: &mut SqliteConnection,
    organization_id: OrganizationId,
    workspace_id: WorkspaceId,
    place: &Place,
    session: &SessionRow,
    seen: Option<(Uuid, i64)>,
    taken: &[Waiting],
    header: Option<&str>,
) -> Result<(String, SeenMessage, Option<Timestamp>), StoreError> {
    let newest = taken
        .iter()
        .max_by_key(|waiting| (waiting.at, waiting.message))
        .ok_or(StoreError::Refused("a turn of no messages"))?;
    let (seen_id, seen_at) = seen.unwrap_or((Uuid::nil(), i64::MIN));
    let mut reads = sqlx::query(concat!(
        thread_select!(),
        " WHERE g.organization_id = ?1 AND g.workspace_id = ?2 AND g.connection_id = ?3 AND g.conversation_id = ?4 \
         AND g.thread_key = ?5 AND g.deleted_at IS NULL \
         AND (g.created_at > ?6 OR (g.created_at = ?6 AND g.id > ?7)) \
         AND (g.created_at < ?8 OR (g.created_at = ?8 AND g.id <= ?9)) \
         ORDER BY g.created_at DESC, g.id DESC LIMIT ?10"
    ))
    .bind(organization_id.uuid())
    .bind(workspace_id.uuid())
    .bind(place.connection_id.uuid())
    .bind(session.session.conversation_id.uuid())
    .bind(&session.session.thread_key)
    .bind(seen_at)
    .bind(seen_id)
    .bind(newest.at)
    .bind(newest.message)
    .bind(MAX_CONTEXT_MESSAGES)
    .fetch_all(&mut *connection)
    .await
    .map_err(failed("render"))?
    .iter()
    .map(read_from)
    .collect::<Result<Vec<_>, StoreError>>()?;
    let mut have: HashSet<Uuid> = reads.iter().map(|read| read.id).collect();
    for waiting in taken {
        // Past the window's newest messages, never one read before.
        if (waiting.at, waiting.message) > (seen_at, seen_id) && have.insert(waiting.message) {
            let row = sqlx::query(concat!(
                thread_select!(),
                " WHERE g.organization_id = ? AND g.workspace_id = ? AND g.connection_id = ? AND g.id = ?"
            ))
            .bind(organization_id.uuid())
            .bind(workspace_id.uuid())
            .bind(place.connection_id.uuid())
            .bind(waiting.message)
            .fetch_optional(&mut *connection)
            .await
            .map_err(failed("render"))?;
            if let Some(row) = row {
                reads.push(read_from(&row)?);
            }
        }
    }
    reads.sort_by_key(|read| (read.at, read.id));
    let kind: String = sqlx::query_scalar(
        "SELECT kind FROM channel_conversations WHERE organization_id = ? AND workspace_id = ? AND id = ?",
    )
    .bind(organization_id.uuid())
    .bind(workspace_id.uuid())
    .bind(session.session.conversation_id.uuid())
    .fetch_one(&mut *connection)
    .await
    .map_err(failed("render"))?;
    let direct = kind == ConversationKind::Dm.as_str();
    let taken_ids: HashSet<Uuid> = taken.iter().map(|waiting| waiting.message).collect();
    let to_you = |read: &Read| direct || taken_ids.contains(&read.id) || read.mentions.contains(&place.own_identity);
    // The names mentions are rendered with: the team's identities, then
    // the people the connection knows, and the conversations it knows by
    // name.
    let mentioned: Vec<&String> = reads.iter().flat_map(|read| read.mentions.iter()).collect();
    let channels: BTreeSet<String> = reads
        .iter()
        .flat_map(|read| conversation_mentions(place.provider, &read.text))
        .collect();
    let names = mention_names_in(
        connection,
        (organization_id, workspace_id, place.connection_id),
        place.identities.clone(),
        &mentioned,
        &channels,
    )
    .await?;
    // What the agent said before is in the session's history; only a
    // session's first turn reads it from the thread.
    let first = seen.is_none();
    let lines: Vec<ThreadLine> = reads
        .iter()
        .filter_map(|read| {
            let author = if read.outbound {
                if !first {
                    return None;
                }
                LineAuthor::You
            } else {
                let (external_id, display_name, handle, automated) = read.author.as_ref()?;
                let name = display_name
                    .clone()
                    .or_else(|| handle.clone())
                    .unwrap_or_else(|| external_id.clone());
                if *external_id == place.own_identity || place.own_bot_id.as_ref() == Some(external_id) {
                    LineAuthor::You
                } else if let Some(agent) = place.identities.get(external_id) {
                    LineAuthor::Agent(agent.clone())
                } else if *automated {
                    LineAuthor::App(name)
                } else {
                    LineAuthor::Person(name)
                }
            };
            Some(ThreadLine {
                author,
                text: read.text.clone(),
                to_you: to_you(read),
            })
        })
        .collect();
    let provider = place.provider;
    let plain = |text: &str| plain_text(provider, text, &names);
    // What changed since its turns last looked, in messages it read before:
    // noted at the start of this turn, never rewritten where it was sent.
    let notes = match seen {
        Some(seen) => change_notes(connection, organization_id, workspace_id, place, session, seen)
            .await?
            .iter()
            .map(|(author, edited)| change_note(author, edited.as_deref().map(&plain).as_deref()))
            .collect(),
        None => Vec::new(),
    };
    let head = match (header, notes.is_empty()) {
        (header, true) => header.map(str::to_owned),
        (Some(header), false) => Some(format!("{header}\n{}", notes.join("\n"))),
        (None, false) => Some(notes.join("\n")),
    };
    let text = render_turn(head.as_deref(), &lines, &plain);
    let seen = match seen {
        Some((id, at)) if (at, id) > (newest.at, newest.message) => SeenMessage {
            id: ChannelMessageId::from_uuid(id),
            at: Timestamp::from_millis(at),
        },
        _ => SeenMessage {
            id: ChannelMessageId::from_uuid(newest.message),
            at: Timestamp::from_millis(newest.at),
        },
    };
    // Every change learned of so far to a message up to `seen` is in this
    // turn: noted above, or read as it now says. What's learned of later
    // is noted in a later turn.
    let noted: Option<i64> = sqlx::query_scalar(
        "SELECT max(changed_at) FROM channel_messages \
         WHERE organization_id = ?1 AND workspace_id = ?2 AND connection_id = ?3 AND conversation_id = ?4 \
         AND thread_key = ?5 AND direction = 'inbound' \
         AND (created_at < ?6 OR (created_at = ?6 AND id <= ?7))",
    )
    .bind(organization_id.uuid())
    .bind(workspace_id.uuid())
    .bind(place.connection_id.uuid())
    .bind(session.session.conversation_id.uuid())
    .bind(&session.session.thread_key)
    .bind(seen.at.millis())
    .bind(seen.id.uuid())
    .fetch_one(&mut *connection)
    .await
    .map_err(failed("render"))?;
    Ok((text, seen, noted.map(Timestamp::from_millis)))
}

/// The most edits and deletes one turn notes.
const MAX_NOTES: i64 = 20;

/// The thread's messages from others that the session read before (up to
/// `seen`) and that were edited or deleted since its turns last noted
/// changes, oldest first: each one's author, and its new text unless it
/// was deleted.
async fn change_notes(
    connection: &mut SqliteConnection,
    organization_id: OrganizationId,
    workspace_id: WorkspaceId,
    place: &Place,
    session: &SessionRow,
    (seen_id, seen_at): (Uuid, i64),
) -> Result<Vec<(String, Option<String>)>, StoreError> {
    let since = session.changes_noted_at.unwrap_or(seen_at);
    let rows = sqlx::query(
        "SELECT g.edited_text, g.deleted_at, p.external_id, p.display_name, p.handle FROM channel_messages g \
         LEFT JOIN channel_people p ON p.organization_id = g.organization_id AND p.workspace_id = g.workspace_id \
         AND p.connection_id = g.connection_id AND p.id = g.author_person_id \
         WHERE g.organization_id = ?1 AND g.workspace_id = ?2 AND g.connection_id = ?3 AND g.conversation_id = ?4 \
         AND g.thread_key = ?5 AND g.direction = 'inbound' \
         AND (g.created_at < ?6 OR (g.created_at = ?6 AND g.id <= ?7)) \
         AND g.changed_at > ?8 \
         ORDER BY g.created_at, g.id LIMIT ?9",
    )
    .bind(organization_id.uuid())
    .bind(workspace_id.uuid())
    .bind(place.connection_id.uuid())
    .bind(session.session.conversation_id.uuid())
    .bind(&session.session.thread_key)
    .bind(seen_at)
    .bind(seen_id)
    .bind(since)
    .bind(MAX_NOTES)
    .fetch_all(&mut *connection)
    .await
    .map_err(failed("change_notes"))?;
    let mut notes = Vec::with_capacity(rows.len());
    for row in &rows {
        let external_id: Option<String> = get(row, "external_id")?;
        let name = get::<Option<String>>(row, "display_name")?
            .or(get(row, "handle")?)
            .or(external_id)
            .unwrap_or_else(|| "Someone".to_owned());
        let deleted: Option<i64> = get(row, "deleted_at")?;
        let edited: Option<String> = get(row, "edited_text")?;
        match (deleted, edited) {
            (Some(_), _) => notes.push((name, None)),
            (None, Some(text)) => notes.push((name, Some(text))),
            (None, None) => {}
        }
    }
    Ok(notes)
}

// --- Starting runs -----------------------------------------------------------

/// A run of the session, as its history reads it: its transcript's weight
/// as kept when it ended.
struct SessionRun {
    id: RunId,
    history_start: Option<RunId>,
    input: String,
    weight: Option<u64>,
}

/// The runs of one run session, in order.
async fn session_runs(
    connection: &mut SqliteConnection,
    organization_id: OrganizationId,
    workspace_id: WorkspaceId,
    session: ChannelSessionKey,
    session_id: SessionId,
) -> Result<Vec<SessionRun>, StoreError> {
    sqlx::query(
        "SELECT id, history_start_run_id, input, transcript_weight FROM runs \
         WHERE organization_id = ? AND workspace_id = ? AND channel_session_id = ? AND session_id = ? \
         ORDER BY session_sequence",
    )
    .bind(organization_id.uuid())
    .bind(workspace_id.uuid())
    .bind(session.uuid())
    .bind(session_id.uuid())
    .fetch_all(&mut *connection)
    .await
    .map_err(failed("session_runs"))?
    .iter()
    .map(|row| {
        Ok(SessionRun {
            id: RunId::from_uuid(get(row, "id")?),
            history_start: get::<Option<Uuid>>(row, "history_start_run_id")?.map(RunId::from_uuid),
            input: get(row, "input")?,
            weight: get::<Option<i64>>(row, "transcript_weight")?
                .map(|weight| u64::try_from(weight).map_err(unreadable))
                .transpose()?,
        })
    })
    .collect()
}

async fn steered_turns(
    connection: &mut SqliteConnection,
    organization_id: OrganizationId,
    workspace_id: WorkspaceId,
    run_id: RunId,
) -> Result<Vec<SteeredTurn>, StoreError> {
    sqlx::query(
        "SELECT turn, text FROM run_steered_turns WHERE organization_id = ? AND workspace_id = ? AND run_id = ? \
         ORDER BY turn",
    )
    .bind(organization_id.uuid())
    .bind(workspace_id.uuid())
    .bind(run_id.uuid())
    .fetch_all(&mut *connection)
    .await
    .map_err(failed("steered_turns"))?
    .iter()
    .map(|row| {
        Ok(SteeredTurn {
            turn: small(get(row, "turn")?)?,
            text: get(row, "text")?,
        })
    })
    .collect()
}

/// What a run of the session leaves in its conversation, built from its
/// turns, calls and steered turns.
async fn build_transcript(
    connection: &mut SqliteConnection,
    organization_id: OrganizationId,
    workspace_id: WorkspaceId,
    run_id: RunId,
    input: &str,
) -> Result<Vec<Message>, StoreError> {
    let turns = crate::runs::committed_turns(connection, organization_id, workspace_id, run_id).await?;
    let calls = crate::runs::recorded_calls(connection, organization_id, workspace_id, run_id).await?;
    let steers = steered_turns(connection, organization_id, workspace_id, run_id).await?;
    // Only a completed run's last turn is final whatever its calls: one
    // that stayed silent ends on its stay_silent call.
    let status: Option<String> =
        sqlx::query_scalar("SELECT status FROM runs WHERE organization_id = ? AND workspace_id = ? AND id = ?")
            .bind(organization_id.uuid())
            .bind(workspace_id.uuid())
            .bind(run_id.uuid())
            .fetch_optional(&mut *connection)
            .await
            .map_err(failed("build_transcript"))?;
    let completed = status.as_deref() == Some(RunStatus::Completed.as_str());
    Ok(transcript(input, &turns, &calls, &steers, completed))
}

/// The transcripts of a run session's runs from `from` up to before
/// `before`, in order: each as kept when it ended, built again only for one
/// that has none kept.
async fn transcripts(
    connection: &mut SqliteConnection,
    organization_id: OrganizationId,
    workspace_id: WorkspaceId,
    (session, session_id): (ChannelSessionKey, SessionId),
    from: u32,
    before: u32,
) -> Result<Vec<Message>, StoreError> {
    let rows = sqlx::query(
        "SELECT id, input, transcript FROM runs \
         WHERE organization_id = ? AND workspace_id = ? AND channel_session_id = ? AND session_id = ? \
         AND session_sequence >= ? AND session_sequence < ? ORDER BY session_sequence",
    )
    .bind(organization_id.uuid())
    .bind(workspace_id.uuid())
    .bind(session.uuid())
    .bind(session_id.uuid())
    .bind(i64::from(from))
    .bind(i64::from(before))
    .fetch_all(&mut *connection)
    .await
    .map_err(failed("transcripts"))?;
    let mut history = Vec::new();
    for row in &rows {
        match get::<Option<String>>(row, "transcript")? {
            Some(kept) => history.extend(serde_json::from_str::<Vec<Message>>(&kept).map_err(unreadable)?),
            None => {
                let run_id = RunId::from_uuid(get(row, "id")?);
                tracing::warn!(event = "channel_runs.transcript_rebuilt", %organization_id, %workspace_id, %run_id);
                let input: String = get(row, "input")?;
                history.extend(build_transcript(connection, organization_id, workspace_id, run_id, &input).await?);
            }
        }
    }
    Ok(history)
}

/// Keeps an ended channel run's transcript, and its weight, for the runs
/// after it: built once, in the transaction that ends it.
async fn keep_transcript(
    connection: &mut SqliteConnection,
    organization_id: OrganizationId,
    workspace_id: WorkspaceId,
    run_id: RunId,
    input: &str,
) -> Result<(), StoreError> {
    let messages = build_transcript(connection, organization_id, workspace_id, run_id, input).await?;
    sqlx::query(
        "UPDATE runs SET transcript = ?, transcript_weight = ? \
         WHERE organization_id = ? AND workspace_id = ? AND id = ? AND transcript IS NULL",
    )
    .bind(to_json(&messages)?)
    .bind(i64::try_from(weight_of(&messages)).map_err(unreadable)?)
    .bind(organization_id.uuid())
    .bind(workspace_id.uuid())
    .bind(run_id.uuid())
    .execute(&mut *connection)
    .await
    .map_err(failed("keep_transcript"))?;
    Ok(())
}

/// What came of starting a session's next run.
enum Start {
    Started(RunId),
    /// Nothing waits, or the session has nothing to start a run with.
    Nothing,
    /// The conversation's breaker stopped it: what waited is paused.
    Paused {
        until: Timestamp,
        tripped: bool,
    },
}

/// Starts the session's next run with every message waiting in it, up to
/// [`MAX_TAKEN`], counting it against the conversation's breaker: a run
/// that goes over it starts nothing, and what waited is paused. A message
/// an earlier turn of the run session read already is settled as read, and
/// starts nothing on its own. The session must be idle.
async fn start_session_run(
    connection: &mut SqliteConnection,
    organization_id: OrganizationId,
    workspace_id: WorkspaceId,
    session: &SessionRow,
    fresh: Option<&SessionTemplate>,
    taken_in: TakenIn,
    now: Timestamp,
) -> Result<Start, StoreError> {
    if session.session.active_run_id.is_some() {
        return Err(StoreError::Refused("a run started in a busy session"));
    }
    // What changed for the session's runs, as their model reads them,
    // starts a new run session: its history starts over, and it reads the
    // thread anew. A label or a team renamed doesn't.
    let (session_id, template, seen) = match (fresh, &session.template) {
        (Some(fresh), Some(own)) if !fresh.same_for_the_model(own) => (SessionId::generate(), fresh, None),
        (Some(fresh), _) => (session.session.session_id, fresh, session.seen),
        (None, Some(own)) => (session.session.session_id, own, session.seen),
        (None, None) => return Ok(Start::Nothing),
    };
    let key = session.session.key;
    let (read, taken) = unread(waiting_in(connection, organization_id, workspace_id, key).await?, seen);
    if !read.is_empty() {
        let ids: Vec<Uuid> = read.iter().map(|waiting| waiting.id).collect();
        settle_waiting(connection, organization_id, workspace_id, key, Some(&ids), "read", now).await?;
    }
    let Some(origin) = taken.last() else {
        return Ok(Start::Nothing);
    };
    match count_breaker(
        connection,
        organization_id,
        workspace_id,
        session.session.conversation_id,
        now,
    )
    .await?
    {
        BreakerOutcome::Counted { .. } => {}
        BreakerOutcome::Paused { until } => {
            settle_waiting(connection, organization_id, workspace_id, key, None, "paused", now).await?;
            return Ok(Start::Paused { until, tripped: false });
        }
        BreakerOutcome::Tripped { paused_until } => {
            settle_waiting(connection, organization_id, workspace_id, key, None, "paused", now).await?;
            return Ok(Start::Paused {
                until: paused_until,
                tripped: true,
            });
        }
    }
    let place = place_of(
        connection,
        organization_id,
        workspace_id,
        session.session.agent_channel_id,
    )
    .await?;
    let (input, seen, noted) = render(
        connection,
        organization_id,
        workspace_id,
        &place,
        session,
        seen,
        &taken,
        None,
    )
    .await?;
    let run_id = RunId::generate();
    let sequence = session
        .session
        .run_count
        .checked_add(1)
        .ok_or(StoreError::Refused("a session's runs are exhausted"))?;
    // Where the new run's history starts: where the last run's did, moved
    // forward by whole runs when the history would go over the budget,
    // each run weighed as kept when it ended.
    let runs = session_runs(connection, organization_id, workspace_id, key, session_id).await?;
    let start = runs
        .last()
        .and_then(|last| last.history_start)
        .and_then(|start| runs.iter().position(|run| run.id == start))
        .unwrap_or(runs.len());
    let mut weights = Vec::with_capacity(runs.len().saturating_sub(start));
    for run in &runs[start..] {
        weights.push(match run.weight {
            Some(weight) => weight,
            None => weight_of(&build_transcript(connection, organization_id, workspace_id, run.id, &run.input).await?),
        });
    }
    let incoming = weight_of(&[Message::User { text: input.clone() }]);
    let cut = trimmed_start(&weights, incoming, template.history_budget);
    let history_start = runs.get(start + cut).map_or(run_id, |run| run.id);
    if cut > 0 {
        tracing::info!(
            event = "channels.history_trimmed",
            %organization_id,
            %workspace_id,
            %run_id,
            runs_left_out = cut,
            from_run_id = %runs[start].id,
        );
    }
    let message_ids: Vec<ChannelMessageId> = taken
        .iter()
        .map(|waiting| ChannelMessageId::from_uuid(waiting.message))
        .collect();
    let new_run = NewRun {
        agent_id: place.agent_id,
        session_id,
        trigger: RunTrigger::Channel,
        input,
        model: template.model.clone(),
        instructions: template.instructions.clone(),
        channel: Some(ChannelAdmission {
            session: key,
            connection_id: place.connection_id,
            conversation_id: session.session.conversation_id,
            thread_key: session.session.thread_key.clone(),
            origin_message_id: ChannelMessageId::from_uuid(origin.message),
            reason: origin.reason,
            session_sequence: sequence,
            history_start_run_id: history_start,
            offer: template.offer.clone(),
            taken_in,
            message_ids,
        }),
    };
    let request = admit(organization_id, workspace_id, run_id, &new_run);
    crate::runs::admit_in(connection, &request, now).await?;
    let ids: Vec<Uuid> = taken.iter().map(|waiting| waiting.id).collect();
    take(
        connection,
        organization_id,
        workspace_id,
        key,
        &ids,
        (taken_in, run_id),
        now,
    )
    .await?;
    let offer = to_json(&template.offer)?;
    let started = sqlx::query(
        "UPDATE channel_sessions SET session_id = ?1, active_run_id = ?2, run_count = ?3, seen_message_id = ?4, \
         seen_at = ?5, model = ?6, model_id = ?7, model_connection_id = ?8, instructions = ?9, offer = ?10, \
         history_budget = ?11, updated_at = ?12, changes_noted_at = CASE WHEN ?13 IS NULL THEN changes_noted_at \
         WHEN changes_noted_at IS NULL OR changes_noted_at < ?13 THEN ?13 ELSE changes_noted_at END \
         WHERE organization_id = ?14 AND workspace_id = ?15 AND id = ?16 AND active_run_id IS NULL",
    )
    .bind(session_id.uuid())
    .bind(run_id.uuid())
    .bind(i64::from(sequence))
    .bind(seen.id.uuid())
    .bind(seen.at.millis())
    .bind(&template.model.provider_model_id)
    .bind(template.model.model_id.uuid())
    .bind(template.model.connection_id.uuid())
    .bind(&template.instructions)
    .bind(offer)
    .bind(i64::try_from(template.history_budget).map_err(unreadable)?)
    .bind(now.millis())
    .bind(noted.map(Timestamp::millis))
    .bind(organization_id.uuid())
    .bind(workspace_id.uuid())
    .bind(key.uuid())
    .execute(&mut *connection)
    .await
    .map_err(failed("start_session_run"))?;
    if started.rows_affected() != 1 {
        return Err(StoreError::Refused("a run started in a busy session"));
    }
    Ok(Start::Started(run_id))
}

/// Ends a channel run's hold on its session, in the transaction that ends
/// the run: its transcript is kept for the runs after it, the session is
/// free, the run is its last, and what waited for it starts the next run,
/// as collected, with the session's own template, counted against the
/// conversation's breaker. Nothing starts while the agent is paused or
/// gone, the connection isn't connected, or the conversation is paused (its
/// waiting messages are paused too). Any other run: nothing.
pub(crate) async fn run_ended(
    connection: &mut SqliteConnection,
    organization_id: OrganizationId,
    workspace_id: WorkspaceId,
    run_id: RunId,
    now: Timestamp,
) -> Result<(), StoreError> {
    let run = sqlx::query(
        "SELECT channel_session_id, input FROM runs WHERE organization_id = ? AND workspace_id = ? AND id = ?",
    )
    .bind(organization_id.uuid())
    .bind(workspace_id.uuid())
    .bind(run_id.uuid())
    .fetch_optional(&mut *connection)
    .await
    .map_err(failed("run_ended"))?;
    let Some(run) = run else {
        return Ok(());
    };
    let Some(key) = get::<Option<Uuid>>(&run, "channel_session_id")? else {
        return Ok(());
    };
    let input: String = get(&run, "input")?;
    keep_transcript(connection, organization_id, workspace_id, run_id, &input).await?;
    let freed = sqlx::query(
        "UPDATE channel_sessions SET active_run_id = NULL, last_run_id = ?1, updated_at = ?2 \
         WHERE organization_id = ?3 AND workspace_id = ?4 AND id = ?5 AND active_run_id = ?1",
    )
    .bind(run_id.uuid())
    .bind(now.millis())
    .bind(organization_id.uuid())
    .bind(workspace_id.uuid())
    .bind(key)
    .execute(&mut *connection)
    .await
    .map_err(failed("run_ended"))?;
    if freed.rows_affected() == 0 {
        // Its session is gone, with its agent channel.
        return Ok(());
    }
    // Its answer is posted by its own job, queued here, before the next run.
    crate::channels::exchange::queue_reply(connection, organization_id, workspace_id, run_id, now).await?;
    let Some(session) = session_by_key(connection, organization_id, workspace_id, key).await? else {
        return Ok(());
    };
    let state = sqlx::query(
        "SELECT a.status AS agent_status, c.status AS connection_status \
         FROM agent_channels h \
         JOIN agents a ON a.organization_id = h.organization_id AND a.workspace_id = h.workspace_id \
         AND a.id = h.agent_id \
         JOIN channel_connections c ON c.organization_id = h.organization_id \
         AND c.workspace_id = h.workspace_id AND c.id = h.connection_id \
         JOIN channel_conversations v ON v.organization_id = h.organization_id \
         AND v.workspace_id = h.workspace_id AND v.connection_id = h.connection_id \
         WHERE h.organization_id = ? AND h.workspace_id = ? AND h.id = ? AND v.id = ?",
    )
    .bind(organization_id.uuid())
    .bind(workspace_id.uuid())
    .bind(session.session.agent_channel_id.uuid())
    .bind(session.session.conversation_id.uuid())
    .fetch_optional(&mut *connection)
    .await
    .map_err(failed("run_ended"))?;
    let Some(state) = state else {
        return Ok(());
    };
    let agent: String = get(&state, "agent_status")?;
    let connected: String = get(&state, "connection_status")?;
    if agent != "active" || connected != "connected" {
        // They wait for the agent's next run in the thread.
        return Ok(());
    }
    // A paused conversation pauses what waited, as the breaker does.
    start_session_run(
        connection,
        organization_id,
        workspace_id,
        &session,
        None,
        TakenIn::Collected,
        now,
    )
    .await?;
    Ok(())
}

// --- Steering ------------------------------------------------------------------

/// The session a run holds as its active run, with the run's connection
/// and conversation; `None` for any other run.
async fn held_session(
    connection: &mut SqliteConnection,
    organization_id: OrganizationId,
    workspace_id: WorkspaceId,
    run_id: RunId,
) -> Result<Option<SessionRow>, StoreError> {
    let row = sqlx::query(concat!(
        session_select!(),
        " WHERE s.organization_id = ? AND s.workspace_id = ? AND s.active_run_id = ?"
    ))
    .bind(organization_id.uuid())
    .bind(workspace_id.uuid())
    .bind(run_id.uuid())
    .fetch_optional(&mut *connection)
    .await
    .map_err(failed("held_session"))?;
    row.as_ref().map(session_from).transpose()
}

/// The waiting messages a run would take in at its next turn boundary.
pub(crate) async fn steerable_in(
    connection: &mut SqliteConnection,
    organization_id: OrganizationId,
    workspace_id: WorkspaceId,
    run_id: RunId,
) -> Result<Option<Steer>, StoreError> {
    let Some(session) = held_session(connection, organization_id, workspace_id, run_id).await? else {
        return Ok(None);
    };
    let place = place_of(
        connection,
        organization_id,
        workspace_id,
        session.session.agent_channel_id,
    )
    .await?;
    if place.mid_run != MidRun::Steer {
        return Ok(None);
    }
    // One the session read already waits for the next run to settle it.
    let (_, taken) = unread(
        waiting_in(connection, organization_id, workspace_id, session.session.key).await?,
        session.seen,
    );
    if taken.is_empty() {
        return Ok(None);
    }
    let (text, seen, noted) = render(
        connection,
        organization_id,
        workspace_id,
        &place,
        &session,
        session.seen,
        &taken,
        Some(STEER_HEADER),
    )
    .await?;
    Ok(Some(Steer {
        text,
        dispatches: taken
            .iter()
            .map(|waiting| zupflow_core::channels::store::ChannelDispatchKey::from_uuid(waiting.id))
            .collect(),
        message_ids: taken
            .iter()
            .map(|waiting| ChannelMessageId::from_uuid(waiting.message))
            .collect(),
        seen,
        changes_noted_at: noted,
    }))
}

/// Takes a steered turn into the run, in the transition that prepares the
/// turn: the run must hold its session, and every dispatch still wait in
/// it. The run's connection and conversation, for its timeline.
pub(crate) async fn take_steer(
    connection: &mut SqliteConnection,
    organization_id: OrganizationId,
    workspace_id: WorkspaceId,
    run_id: RunId,
    turn: u32,
    steer: &Steer,
    now: Timestamp,
) -> Result<(ChannelConnectionId, ChannelConversationId), StoreError> {
    let Some(session) = held_session(connection, organization_id, workspace_id, run_id).await? else {
        return Err(StoreError::Refused(
            "thread messages joining a run that doesn't hold its session",
        ));
    };
    let place = place_of(
        connection,
        organization_id,
        workspace_id,
        session.session.agent_channel_id,
    )
    .await?;
    let ids: Vec<Uuid> = steer.dispatches.iter().map(|dispatch| dispatch.uuid()).collect();
    if ids.is_empty() {
        return Err(StoreError::Refused("a steered turn of no messages"));
    }
    take(
        connection,
        organization_id,
        workspace_id,
        session.session.key,
        &ids,
        (TakenIn::Steered, run_id),
        now,
    )
    .await?;
    sqlx::query(
        "INSERT INTO run_steered_turns (id, organization_id, workspace_id, run_id, turn, text, created_at) \
         VALUES (?, ?, ?, ?, ?, ?, ?)",
    )
    .bind(Uuid::now_v7())
    .bind(organization_id.uuid())
    .bind(workspace_id.uuid())
    .bind(run_id.uuid())
    .bind(i64::from(turn))
    .bind(&steer.text)
    .bind(now.millis())
    .execute(&mut *connection)
    .await
    .map_err(failed("take_steer"))?;
    // The steered turn noted what changed up to when it was rendered: a
    // change learned of since is noted in a later turn.
    // The watermark moves forward, never back.
    sqlx::query(
        "UPDATE channel_sessions SET changes_noted_at = CASE WHEN ?1 IS NULL THEN changes_noted_at \
         WHEN changes_noted_at IS NULL OR changes_noted_at < ?1 THEN ?1 ELSE changes_noted_at END \
         WHERE organization_id = ?2 AND workspace_id = ?3 AND id = ?4",
    )
    .bind(steer.changes_noted_at.map(Timestamp::millis))
    .bind(organization_id.uuid())
    .bind(workspace_id.uuid())
    .bind(session.session.key.uuid())
    .execute(&mut *connection)
    .await
    .map_err(failed("take_steer"))?;
    let newer = session
        .seen
        .is_none_or(|(id, at)| (steer.seen.at.millis(), steer.seen.id.uuid()) > (at, id));
    if newer {
        sqlx::query(
            "UPDATE channel_sessions SET seen_message_id = ?, seen_at = ?, updated_at = ? \
             WHERE organization_id = ? AND workspace_id = ? AND id = ?",
        )
        .bind(steer.seen.id.uuid())
        .bind(steer.seen.at.millis())
        .bind(now.millis())
        .bind(organization_id.uuid())
        .bind(workspace_id.uuid())
        .bind(session.session.key.uuid())
        .execute(&mut *connection)
        .await
        .map_err(failed("take_steer"))?;
    }
    Ok((place.connection_id, session.session.conversation_id))
}

// --- A run's session, for its snapshot --------------------------------------------

/// A channel run's session context: the transcripts of its session's runs
/// from its history start, as kept when each ended, the session's tools,
/// and its own steered turns.
/// `None` for any other run.
pub(crate) async fn session_context(
    connection: &mut SqliteConnection,
    organization_id: OrganizationId,
    workspace_id: WorkspaceId,
    run_id: RunId,
) -> Result<Option<SessionContext>, StoreError> {
    let row = sqlx::query(
        "SELECT channel_session_id, session_id, session_sequence, history_start_run_id, offer, \
         channel_connection_id, channel_conversation_id, channel_thread_key FROM runs \
         WHERE organization_id = ? AND workspace_id = ? AND id = ?",
    )
    .bind(organization_id.uuid())
    .bind(workspace_id.uuid())
    .bind(run_id.uuid())
    .fetch_optional(&mut *connection)
    .await
    .map_err(failed("session_context"))?
    .ok_or(StoreError::NotFound)?;
    let Some(key) = get::<Option<Uuid>>(&row, "channel_session_id")? else {
        return Ok(None);
    };
    let (
        Some(sequence),
        Some(start),
        Some(offer),
        Some(origin_connection),
        Some(origin_conversation),
        Some(thread_key),
    ) = (
        get::<Option<i64>>(&row, "session_sequence")?,
        get::<Option<Uuid>>(&row, "history_start_run_id")?,
        get::<Option<String>>(&row, "offer")?,
        get::<Option<Uuid>>(&row, "channel_connection_id")?,
        get::<Option<Uuid>>(&row, "channel_conversation_id")?,
        get::<Option<String>>(&row, "channel_thread_key")?,
    )
    else {
        return Err(corrupt("a channel run without its place in its session"));
    };
    let sequence = small(sequence)?;
    let session_id = SessionId::from_uuid(get(&row, "session_id")?);
    let key = ChannelSessionKey::from_uuid(key);
    let start = RunId::from_uuid(start);
    let from = if start == run_id {
        sequence
    } else {
        let from: Option<i64> = sqlx::query_scalar(
            "SELECT session_sequence FROM runs \
             WHERE organization_id = ? AND workspace_id = ? AND channel_session_id = ? AND session_id = ? AND id = ?",
        )
        .bind(organization_id.uuid())
        .bind(workspace_id.uuid())
        .bind(key.uuid())
        .bind(session_id.uuid())
        .bind(start.uuid())
        .fetch_optional(&mut *connection)
        .await
        .map_err(failed("session_context"))?
        .flatten();
        small(from.ok_or_else(|| corrupt("a history that starts outside its session"))?)?
    };
    let history = transcripts(
        connection,
        organization_id,
        workspace_id,
        (key, session_id),
        from,
        sequence,
    )
    .await?;
    Ok(Some(SessionContext {
        origin: ChannelOrigin {
            connection_id: ChannelConnectionId::from_uuid(origin_connection),
            conversation_id: ChannelConversationId::from_uuid(origin_conversation),
            thread_key,
        },
        history,
        offer: serde_json::from_str::<Offer>(&offer).map_err(unreadable)?,
        steers: steered_turns(connection, organization_id, workspace_id, run_id).await?,
    }))
}
