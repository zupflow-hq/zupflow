//! What a channel run's tools read and record through its own connection,
//! and the retention sweep.
//!
//! Every statement names the organization and the workspace, and the
//! connection beside any message, conversation or person: a run reaches only
//! its own connection's, even with another's ID in hand. The one exception
//! is [`retention_due_cross_tenant`], a sweep like a worker's claim, which
//! finds the workspaces to sweep and then sweeps each in its own scope. The
//! scope audit in `channels.rs` reads this file too.

use sqlx::{Row, SqliteConnection};
use uuid::Uuid;
use zupflow_core::channels::store::{
    ChannelError, KnownPerson, MAX_SEND_WAIT_MS, RunChannelContext, SENDS_PER_HOUR, SENDS_PER_SECOND, SendCounts,
    SendSlot, Swept,
};
use zupflow_core::channels::thread::MentionNames;
use zupflow_core::channels::{ChannelMessage, INBOUND_EVENT_RETENTION_DAYS, RETENTION_JOB};
use zupflow_core::ids::{
    AgentChannelId, ChannelConnectionId, ChannelConversationId, ChannelMessageId, ChannelPersonId, OrganizationId,
    RunId, SessionId, ToolCallId, WorkspaceId,
};
use zupflow_core::time::Timestamp;

use super::{
    DAY_MS, FIND_CONVERSATION, SqliteChannelStore, agent_channel_from, conversation_from, failed, find_connection_in,
    message_from, read, settings_in, small,
};

/// What a run's input reads once the retention sweep removed it.
pub(crate) const REMOVED_INPUT: &str = "(Removed after the retention period.)";

/// The most channel runs one sweep clears in a workspace; the next sweep
/// takes the rest.
const RUNS_PER_SWEEP: i64 = 200;

pub(super) async fn run_channel(
    store: &SqliteChannelStore,
    organization_id: OrganizationId,
    workspace_id: WorkspaceId,
    run_id: RunId,
) -> Result<Option<RunChannelContext>, ChannelError> {
    let mut transaction = store.database.pool().begin().await.map_err(failed("run_channel"))?;
    let row = sqlx::query(
        "SELECT r.channel_connection_id, r.channel_conversation_id, r.channel_thread_key, s.agent_channel_id \
         FROM runs r JOIN channel_sessions s ON s.organization_id = r.organization_id \
         AND s.workspace_id = r.workspace_id AND s.id = r.channel_session_id \
         WHERE r.organization_id = ? AND r.workspace_id = ? AND r.id = ?",
    )
    .bind(organization_id.uuid())
    .bind(workspace_id.uuid())
    .bind(run_id.uuid())
    .fetch_optional(&mut *transaction)
    .await
    .map_err(failed("run_channel"))?;
    let Some(row) = row else {
        return Ok(None);
    };
    let connection_id = ChannelConnectionId::from_uuid(row.try_get("channel_connection_id").map_err(read)?);
    let conversation_id = ChannelConversationId::from_uuid(row.try_get("channel_conversation_id").map_err(read)?);
    let thread_key: String = row.try_get("channel_thread_key").map_err(read)?;
    let agent_channel_id = AgentChannelId::from_uuid(row.try_get("agent_channel_id").map_err(read)?);
    let channel = sqlx::query(concat!(
        agent_channel_select!(),
        " WHERE h.organization_id = ? AND h.workspace_id = ? AND h.id = ? AND h.connection_id = ?"
    ))
    .bind(organization_id.uuid())
    .bind(workspace_id.uuid())
    .bind(agent_channel_id.uuid())
    .bind(connection_id.uuid())
    .fetch_optional(&mut *transaction)
    .await
    .map_err(failed("run_channel"))?;
    let Some(channel) = channel.as_ref().map(agent_channel_from).transpose()? else {
        return Ok(None);
    };
    let Some(connection) = find_connection_in(&mut transaction, organization_id, workspace_id, connection_id).await?
    else {
        return Ok(None);
    };
    let origin = sqlx::query(FIND_CONVERSATION)
        .bind(organization_id.uuid())
        .bind(workspace_id.uuid())
        .bind(connection_id.uuid())
        .bind(conversation_id.uuid())
        .fetch_optional(&mut *transaction)
        .await
        .map_err(failed("run_channel"))?;
    let Some(origin) = origin.as_ref().map(conversation_from).transpose()? else {
        return Ok(None);
    };
    let hop: Option<i64> = sqlx::query_scalar(
        "SELECT max(agent_hop) FROM channel_dispatches WHERE organization_id = ? AND workspace_id = ? AND run_id = ?",
    )
    .bind(organization_id.uuid())
    .bind(workspace_id.uuid())
    .bind(run_id.uuid())
    .fetch_one(&mut *transaction)
    .await
    .map_err(failed("run_channel"))?;
    transaction.commit().await.map_err(failed("run_channel"))?;
    Ok(Some(RunChannelContext {
        agent_channel: channel,
        connection,
        origin,
        thread_key,
        hop: small(hop.unwrap_or(0))?.saturating_add(1),
    }))
}

pub(super) async fn find_message(
    store: &SqliteChannelStore,
    organization_id: OrganizationId,
    workspace_id: WorkspaceId,
    connection_id: ChannelConnectionId,
    message_id: ChannelMessageId,
) -> Result<Option<ChannelMessage>, ChannelError> {
    let row = sqlx::query(concat!(
        message_select!(),
        " WHERE g.organization_id = ? AND g.workspace_id = ? AND g.connection_id = ? AND g.id = ?"
    ))
    .bind(organization_id.uuid())
    .bind(workspace_id.uuid())
    .bind(connection_id.uuid())
    .bind(message_id.uuid())
    .fetch_optional(store.database.pool())
    .await
    .map_err(failed("find_message"))?;
    row.as_ref().map(message_from).transpose()
}

pub(super) async fn sent_by_call(
    store: &SqliteChannelStore,
    organization_id: OrganizationId,
    workspace_id: WorkspaceId,
    run_id: RunId,
    call_id: ToolCallId,
) -> Result<Option<ChannelMessage>, ChannelError> {
    let row = sqlx::query(concat!(
        message_select!(),
        " WHERE g.organization_id = ? AND g.workspace_id = ? AND g.run_id = ? AND g.tool_call_id = ? \
         AND g.direction = 'outbound' ORDER BY g.created_at, g.id LIMIT 1"
    ))
    .bind(organization_id.uuid())
    .bind(workspace_id.uuid())
    .bind(run_id.uuid())
    .bind(call_id.uuid())
    .fetch_optional(store.database.pool())
    .await
    .map_err(failed("sent_by_call"))?;
    row.as_ref().map(message_from).transpose()
}

fn person_from(row: &sqlx::sqlite::SqliteRow) -> Result<KnownPerson, ChannelError> {
    Ok(KnownPerson {
        id: ChannelPersonId::from_uuid(row.try_get("id").map_err(read)?),
        external_id: row.try_get("external_id").map_err(read)?,
        handle: row.try_get("handle").map_err(read)?,
        display_name: row.try_get("display_name").map_err(read)?,
        is_automated: row.try_get("is_automated").map_err(read)?,
        team_id: row.try_get("team_id").map_err(read)?,
    })
}

pub(super) async fn find_person(
    store: &SqliteChannelStore,
    organization_id: OrganizationId,
    workspace_id: WorkspaceId,
    connection_id: ChannelConnectionId,
    person_id: ChannelPersonId,
) -> Result<Option<KnownPerson>, ChannelError> {
    let row = sqlx::query(
        "SELECT id, external_id, handle, display_name, is_automated, team_id FROM channel_people \
         WHERE organization_id = ? AND workspace_id = ? AND connection_id = ? AND id = ?",
    )
    .bind(organization_id.uuid())
    .bind(workspace_id.uuid())
    .bind(connection_id.uuid())
    .bind(person_id.uuid())
    .fetch_optional(store.database.pool())
    .await
    .map_err(failed("find_person"))?;
    row.as_ref().map(person_from).transpose()
}

pub(super) async fn find_people(
    store: &SqliteChannelStore,
    organization_id: OrganizationId,
    workspace_id: WorkspaceId,
    connection_id: ChannelConnectionId,
    query: &str,
    limit: u32,
) -> Result<Vec<KnownPerson>, ChannelError> {
    let rows = sqlx::query(
        "SELECT id, external_id, handle, display_name, is_automated, team_id FROM channel_people \
         WHERE organization_id = ? AND workspace_id = ? AND connection_id = ? \
         AND (instr(lower(coalesce(display_name, '')), lower(?)) > 0 OR instr(lower(coalesce(handle, '')), lower(?)) > 0) \
         ORDER BY coalesce(display_name, handle), id LIMIT ?",
    )
    .bind(organization_id.uuid())
    .bind(workspace_id.uuid())
    .bind(connection_id.uuid())
    .bind(query)
    .bind(query)
    .bind(i64::from(limit))
    .fetch_all(store.database.pool())
    .await
    .map_err(failed("find_people"))?;
    rows.iter().map(person_from).collect()
}

#[allow(clippy::too_many_arguments)]
pub(super) async fn search_messages(
    store: &SqliteChannelStore,
    organization_id: OrganizationId,
    workspace_id: WorkspaceId,
    connection_id: ChannelConnectionId,
    conversation_id: Option<ChannelConversationId>,
    readable_dm: ChannelConversationId,
    query: &str,
    limit: u32,
) -> Result<Vec<ChannelMessage>, ChannelError> {
    // Its text contains the query, or has markup (`<…>`) or an escape
    // (`&…;`) a rendering could turn into it: the caller renders and matches again. A direct message
    // is searched only in the conversation the run began in.
    let rows = sqlx::query(concat!(
        message_select!(),
        " JOIN channel_conversations k ON k.organization_id = g.organization_id \
         AND k.workspace_id = g.workspace_id AND k.connection_id = g.connection_id AND k.id = g.conversation_id \
         WHERE g.organization_id = ?1 AND g.workspace_id = ?2 AND g.connection_id = ?3 \
         AND (?4 IS NULL OR g.conversation_id = ?4) AND g.deleted_at IS NULL \
         AND (k.kind NOT IN ('dm', 'group_dm') OR k.id = ?5) \
         AND (instr(lower(coalesce(g.edited_text, g.text)), lower(?6)) > 0 \
         OR instr(coalesce(g.edited_text, g.text), '<') > 0 OR instr(coalesce(g.edited_text, g.text), '&') > 0) \
         ORDER BY g.sent_at DESC, g.id DESC LIMIT ?7"
    ))
    .bind(organization_id.uuid())
    .bind(workspace_id.uuid())
    .bind(connection_id.uuid())
    .bind(conversation_id.map(|id| id.uuid()))
    .bind(readable_dm.uuid())
    .bind(query)
    .bind(i64::from(limit))
    .fetch_all(store.database.pool())
    .await
    .map_err(failed("search_messages"))?;
    rows.iter().map(message_from).collect()
}

pub(super) async fn mention_names(
    store: &SqliteChannelStore,
    (organization_id, workspace_id, connection_id): (OrganizationId, WorkspaceId, ChannelConnectionId),
    people: &[String],
    conversations: &[String],
) -> Result<MentionNames, ChannelError> {
    let mut connection = store.database.pool().acquire().await.map_err(failed("mention_names"))?;
    let team: Option<String> = sqlx::query_scalar(
        "SELECT team_id FROM channel_connections WHERE organization_id = ? AND workspace_id = ? AND id = ?",
    )
    .bind(organization_id.uuid())
    .bind(workspace_id.uuid())
    .bind(connection_id.uuid())
    .fetch_optional(&mut *connection)
    .await
    .map_err(failed("mention_names"))?;
    let Some(team) = team else {
        return Ok(MentionNames::default());
    };
    let identities = crate::channel_runs::team_identities(&mut connection, organization_id, workspace_id, &team)
        .await
        .map_err(store_error)?;
    let people: Vec<&String> = people.iter().collect();
    let conversations: std::collections::BTreeSet<String> = conversations.iter().cloned().collect();
    crate::channel_runs::mention_names_in(
        &mut connection,
        (organization_id, workspace_id, connection_id),
        identities,
        &people,
        &conversations,
    )
    .await
    .map_err(store_error)
}

fn store_error(error: zupflow_core::runs::store::StoreError) -> ChannelError {
    tracing::warn!(event = "channels.mention_names_failed", %error);
    ChannelError::Unavailable
}

/// An hour, in milliseconds.
const HOUR_MS: i64 = 60 * 60 * 1000;

async fn counts_in(
    connection: &mut SqliteConnection,
    (organization_id, workspace_id, connection_id): (OrganizationId, WorkspaceId, ChannelConnectionId),
    now: Timestamp,
) -> Result<(SendCounts, Option<i64>), ChannelError> {
    let row = sqlx::query(
        "SELECT count(*) AS hour, coalesce(sum(taken_at > ?), 0) AS second, max(taken_at) AS last \
         FROM channel_send_slots WHERE organization_id = ? AND workspace_id = ? AND connection_id = ? \
         AND taken_at > ?",
    )
    .bind(now.millis().saturating_sub(1000))
    .bind(organization_id.uuid())
    .bind(workspace_id.uuid())
    .bind(connection_id.uuid())
    .bind(now.millis().saturating_sub(HOUR_MS))
    .fetch_one(&mut *connection)
    .await
    .map_err(failed("send_counts"))?;
    Ok((
        SendCounts {
            second: small(row.try_get("second").map_err(read)?)?,
            hour: small(row.try_get("hour").map_err(read)?)?,
        },
        row.try_get("last").map_err(read)?,
    ))
}

pub(super) async fn send_counts(
    store: &SqliteChannelStore,
    scope: (OrganizationId, WorkspaceId, ChannelConnectionId),
    now: Timestamp,
) -> Result<SendCounts, ChannelError> {
    let mut connection = store.database.pool().acquire().await.map_err(failed("send_counts"))?;
    Ok(counts_in(&mut connection, scope, now).await?.0)
}

pub(super) async fn take_send_slot(
    store: &SqliteChannelStore,
    (organization_id, workspace_id, connection_id): (OrganizationId, WorkspaceId, ChannelConnectionId),
    (run_id, call_id): (RunId, ToolCallId),
    now: Timestamp,
) -> Result<SendSlot, ChannelError> {
    // Immediate: one taker at a time counts and takes.
    let mut transaction = store.immediate("take_send_slot").await?;
    sqlx::query(
        "DELETE FROM channel_send_slots WHERE organization_id = ? AND workspace_id = ? AND connection_id = ? \
         AND taken_at <= ?",
    )
    .bind(organization_id.uuid())
    .bind(workspace_id.uuid())
    .bind(connection_id.uuid())
    .bind(now.millis().saturating_sub(HOUR_MS))
    .execute(&mut *transaction)
    .await
    .map_err(failed("take_send_slot"))?;
    let scope = (organization_id, workspace_id, connection_id);
    let (counts, last) = counts_in(&mut transaction, scope, now).await?;
    if counts.hour >= SENDS_PER_HOUR {
        return Ok(SendSlot::HourFull);
    }
    // Spaced after the latest reserved slot, or now.
    let spacing = 1000 / i64::from(SENDS_PER_SECOND.max(1));
    let at = last.map_or(now.millis(), |last| last.saturating_add(spacing).max(now.millis()));
    let wait = at.saturating_sub(now.millis());
    if wait > MAX_SEND_WAIT_MS {
        return Ok(SendSlot::Busy);
    }
    sqlx::query(
        "INSERT INTO channel_send_slots (id, organization_id, workspace_id, connection_id, run_id, tool_call_id, \
         taken_at) VALUES (?, ?, ?, ?, ?, ?, ?)",
    )
    .bind(Uuid::now_v7())
    .bind(organization_id.uuid())
    .bind(workspace_id.uuid())
    .bind(connection_id.uuid())
    .bind(run_id.uuid())
    .bind(call_id.uuid())
    .bind(at)
    .execute(&mut *transaction)
    .await
    .map_err(failed("take_send_slot"))?;
    transaction.commit().await.map_err(failed("take_send_slot"))?;
    Ok(SendSlot::Taken {
        wait_millis: u64::try_from(wait).unwrap_or(0),
    })
}

/// Records a change to the connection's own message: its new text, or that
/// it's deleted.
pub(super) async fn record_own_change(
    store: &SqliteChannelStore,
    (organization_id, workspace_id, connection_id): (OrganizationId, WorkspaceId, ChannelConnectionId),
    message_id: ChannelMessageId,
    edited: Option<&str>,
    now: Timestamp,
) -> Result<(), ChannelError> {
    let (edited_at, deleted_at) = match edited {
        Some(_) => (Some(now.millis()), None),
        None => (None, Some(now.millis())),
    };
    let updated = sqlx::query(
        "UPDATE channel_messages SET edited_text = coalesce(?, edited_text), edited_at = coalesce(?, edited_at), \
         deleted_at = coalesce(deleted_at, ?) \
         WHERE organization_id = ? AND workspace_id = ? AND connection_id = ? AND id = ? AND direction = 'outbound'",
    )
    .bind(edited)
    .bind(edited_at)
    .bind(deleted_at)
    .bind(organization_id.uuid())
    .bind(workspace_id.uuid())
    .bind(connection_id.uuid())
    .bind(message_id.uuid())
    .execute(store.database.pool())
    .await
    .map_err(failed("record_own_change"))?;
    if updated.rows_affected() == 1 {
        Ok(())
    } else {
        Err(ChannelError::MessageNotFound)
    }
}

/// Cross-tenant by design, like a worker's claim: the workspaces with
/// channel data past retention, each then swept in its own scope.
pub(super) async fn retention_due_cross_tenant(
    store: &SqliteChannelStore,
    now: Timestamp,
    limit: u32,
) -> Result<Vec<(OrganizationId, WorkspaceId)>, ChannelError> {
    let events_cutoff = now
        .millis()
        .saturating_sub(i64::from(INBOUND_EVENT_RETENTION_DAYS).saturating_mul(DAY_MS));
    // A workspace whose sweep is queued or running already is left to it.
    let rows: Vec<(Uuid, Uuid)> = sqlx::query_as(
        "SELECT d.organization_id, d.workspace_id FROM ( \
         SELECT g.organization_id, g.workspace_id FROM channel_messages g \
         LEFT JOIN channel_settings s ON s.organization_id = g.organization_id \
         WHERE g.expires_at <= ?1 OR g.sent_at <= ?1 - coalesce(s.message_retention_days, 30) * ?2 \
         UNION SELECT e.organization_id, e.workspace_id FROM channel_inbound_events e WHERE e.received_at <= ?3 \
         UNION SELECT r.organization_id, r.workspace_id FROM runs r \
         LEFT JOIN channel_settings s ON s.organization_id = r.organization_id \
         WHERE r.channel_session_id IS NOT NULL AND r.channel_text_removed_at IS NULL \
         AND r.finished_at <= ?1 - coalesce(s.message_retention_days, 30) * ?2 \
         ) d WHERE NOT EXISTS (SELECT 1 FROM jobs j WHERE j.organization_id = d.organization_id \
         AND j.workspace_id = d.workspace_id AND j.kind = ?5 AND j.state <> 'dead') LIMIT ?4",
    )
    .bind(now.millis())
    .bind(DAY_MS)
    .bind(events_cutoff)
    .bind(i64::from(limit))
    .bind(RETENTION_JOB)
    .fetch_all(store.database.pool())
    .await
    .map_err(failed("retention_due"))?;
    Ok(rows
        .into_iter()
        .map(|(organization, workspace)| {
            (
                OrganizationId::from_uuid(organization),
                WorkspaceId::from_uuid(workspace),
            )
        })
        .collect())
}

pub(super) async fn sweep_retention(
    store: &SqliteChannelStore,
    organization_id: OrganizationId,
    workspace_id: WorkspaceId,
    now: Timestamp,
) -> Result<Swept, ChannelError> {
    let mut transaction = store.immediate("sweep_retention").await?;
    let days = settings_in(&mut transaction, organization_id)
        .await?
        .values
        .message_retention_days;
    let cutoff = now.millis().saturating_sub(i64::from(days).saturating_mul(DAY_MS));
    let events_cutoff = now
        .millis()
        .saturating_sub(i64::from(INBOUND_EVENT_RETENTION_DAYS).saturating_mul(DAY_MS));
    // What was dispatched for a message goes with it.
    let messages = sqlx::query(
        "DELETE FROM channel_messages WHERE organization_id = ? AND workspace_id = ? \
         AND (expires_at <= ? OR sent_at <= ?)",
    )
    .bind(organization_id.uuid())
    .bind(workspace_id.uuid())
    .bind(now.millis())
    .bind(cutoff)
    .execute(&mut *transaction)
    .await
    .map_err(failed("sweep_retention"))?
    .rows_affected();
    let inbound_events = sqlx::query(
        "DELETE FROM channel_inbound_events WHERE organization_id = ? AND workspace_id = ? AND received_at <= ?",
    )
    .bind(organization_id.uuid())
    .bind(workspace_id.uuid())
    .bind(events_cutoff)
    .execute(&mut *transaction)
    .await
    .map_err(failed("sweep_retention"))?
    .rows_affected();
    // Channel runs that finished before the cutoff, in sessions with no run
    // in progress: a run in progress reads its session's history.
    let runs: Vec<(Uuid, Uuid, Uuid, i64)> = sqlx::query_as(
        "SELECT r.id, r.channel_session_id, r.session_id, r.session_sequence FROM runs r \
         WHERE r.organization_id = ? AND r.workspace_id = ? AND r.channel_session_id IS NOT NULL \
         AND r.channel_text_removed_at IS NULL AND r.finished_at <= ? \
         AND NOT EXISTS (SELECT 1 FROM channel_sessions s WHERE s.organization_id = r.organization_id \
         AND s.workspace_id = r.workspace_id AND s.id = r.channel_session_id AND s.active_run_id IS NOT NULL) \
         ORDER BY r.finished_at, r.id LIMIT ?",
    )
    .bind(organization_id.uuid())
    .bind(workspace_id.uuid())
    .bind(cutoff)
    .bind(RUNS_PER_SWEEP)
    .fetch_all(&mut *transaction)
    .await
    .map_err(failed("sweep_retention"))?;
    // Only a run its session's next run would read back (one of its current
    // run session, from where that session's history starts) starts the
    // session anew; an older one's text goes without touching it.
    let mut sessions: Vec<Uuid> = Vec::new();
    for (run, session, session_id, sequence) in &runs {
        let live = in_live_history(
            &mut transaction,
            (organization_id, workspace_id),
            (*session, *session_id),
            *sequence,
        )
        .await?;
        clear_run(&mut transaction, organization_id, workspace_id, *run, now).await?;
        if live && !sessions.contains(session) {
            sessions.push(*session);
        }
    }
    // Part of its history is gone: the session's next run starts a new run
    // session, and reads its thread anew.
    for session in sessions {
        sqlx::query(
            "UPDATE channel_sessions SET session_id = ?, seen_message_id = NULL, seen_at = NULL, \
             changes_noted_at = NULL, updated_at = ? \
             WHERE organization_id = ? AND workspace_id = ? AND id = ? AND active_run_id IS NULL",
        )
        .bind(SessionId::generate().uuid())
        .bind(now.millis())
        .bind(organization_id.uuid())
        .bind(workspace_id.uuid())
        .bind(session)
        .execute(&mut *transaction)
        .await
        .map_err(failed("sweep_retention"))?;
    }
    transaction.commit().await.map_err(failed("sweep_retention"))?;
    Ok(Swept {
        messages,
        inbound_events,
        runs: u64::try_from(runs.len()).unwrap_or(u64::MAX),
    })
}

/// Whether a run of `session_id` at `sequence` is in what the channel
/// session's next run reads back: `session_id` is the session's current
/// one, and the run is at or after where its latest run's history starts.
async fn in_live_history(
    transaction: &mut SqliteConnection,
    (organization_id, workspace_id): (OrganizationId, WorkspaceId),
    (session, session_id): (Uuid, Uuid),
    sequence: i64,
) -> Result<bool, ChannelError> {
    let live: Option<i64> = sqlx::query_scalar(
        "SELECT 1 FROM channel_sessions s \
         JOIN runs l ON l.organization_id = s.organization_id AND l.workspace_id = s.workspace_id \
         AND l.channel_session_id = s.id AND l.session_id = s.session_id \
         LEFT JOIN runs h ON h.organization_id = l.organization_id AND h.workspace_id = l.workspace_id \
         AND h.id = l.history_start_run_id \
         WHERE s.organization_id = ?1 AND s.workspace_id = ?2 AND s.id = ?3 AND s.session_id = ?4 \
         AND l.session_sequence = (SELECT max(m.session_sequence) FROM runs m WHERE m.organization_id = ?1 \
         AND m.workspace_id = ?2 AND m.channel_session_id = ?3 AND m.session_id = ?4) \
         AND ?5 >= coalesce(h.session_sequence, l.session_sequence)",
    )
    .bind(organization_id.uuid())
    .bind(workspace_id.uuid())
    .bind(session)
    .bind(session_id)
    .bind(sequence)
    .fetch_optional(&mut *transaction)
    .await
    .map_err(failed("sweep_retention"))?;
    Ok(live.is_some())
}

/// Clears one channel run's text: what it was asked, answered and read
/// through its tools, why it stayed silent, its turns, steered turns and
/// kept transcript. What it did (its events, calls, usage and outcome)
/// stays, with their text gone.
async fn clear_run(
    transaction: &mut SqliteConnection,
    organization_id: OrganizationId,
    workspace_id: WorkspaceId,
    run: Uuid,
    now: Timestamp,
) -> Result<(), ChannelError> {
    let statements = [
        "DELETE FROM run_steered_turns WHERE organization_id = ? AND workspace_id = ? AND run_id = ?",
        "UPDATE run_turns SET text = NULL WHERE organization_id = ? AND workspace_id = ? AND run_id = ?",
        "UPDATE run_tool_calls SET arguments = '{}', outcome = CASE WHEN outcome IS NULL THEN NULL ELSE '' END \
         WHERE organization_id = ? AND workspace_id = ? AND run_id = ?",
        "UPDATE run_events SET data = json_set(data, '$.text', '') \
         WHERE organization_id = ? AND workspace_id = ? AND run_id = ? AND type = 'message.assistant'",
        "UPDATE run_events SET data = json_set(data, '$.arguments', json('{}')) \
         WHERE organization_id = ? AND workspace_id = ? AND run_id = ? AND json_type(data, '$.arguments') IS NOT NULL",
        "UPDATE run_events SET data = json_set(data, '$.result', json('{}')) \
         WHERE organization_id = ? AND workspace_id = ? AND run_id = ? AND json_type(data, '$.result') IS NOT NULL",
        "UPDATE run_events SET data = json_set(data, '$.reason', '') \
         WHERE organization_id = ? AND workspace_id = ? AND run_id = ? AND type = 'run.stayed_silent'",
        "UPDATE run_call_attempts SET outcome = '{}' \
         WHERE organization_id = ? AND workspace_id = ? AND run_id = ? AND outcome IS NOT NULL",
    ];
    for statement in statements {
        sqlx::query(statement)
            .bind(organization_id.uuid())
            .bind(workspace_id.uuid())
            .bind(run)
            .execute(&mut *transaction)
            .await
            .map_err(failed("sweep_retention"))?;
    }
    sqlx::query(
        "UPDATE runs SET input = ?, output = NULL, silent_reason = NULL, transcript = '[]', transcript_weight = 0, \
         channel_text_removed_at = ? \
         WHERE organization_id = ? AND workspace_id = ? AND id = ? AND channel_session_id IS NOT NULL",
    )
    .bind(REMOVED_INPUT)
    .bind(now.millis())
    .bind(organization_id.uuid())
    .bind(workspace_id.uuid())
    .bind(run)
    .execute(&mut *transaction)
    .await
    .map_err(failed("sweep_retention"))?;
    Ok(())
}
