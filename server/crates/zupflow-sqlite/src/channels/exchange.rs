//! What a connection reads from its provider and sends back: the people its
//! messages name (kept as the connection's cache of them: a name once
//! known, and when the provider was last asked), and its runs' answers, queued by the transaction that ends the
//! run and read back by the reply job.
//!
//! Every statement names the organization and the workspace beside any
//! unique ID; the scope audit in `channels.rs` reads this file too.

use std::collections::HashMap;

use sqlx::{Row, SqliteConnection};
use uuid::Uuid;
use zupflow_core::channels::reply::{ChannelReply, lookup_stale, reply_job};
use zupflow_core::channels::store::{ChannelError, PersonLookup};
use zupflow_core::channels::{ConnectionStatus, ConversationKind};
use zupflow_core::ids::{ChannelConnectionId, ChannelMessageId, ChannelPersonId, OrganizationId, RunId, WorkspaceId};
use zupflow_core::runs::store::StoreError;
use zupflow_core::scope::Scope;
use zupflow_core::time::Timestamp;

use super::{SqliteChannelStore, failed, read, small, to_json};

/// `value` when its length in characters is within the column's bounds;
/// otherwise nothing is kept of it.
fn fitting(value: Option<&str>, min: usize, max: usize) -> Option<&str> {
    value.filter(|value| (min..=max).contains(&value.chars().count()))
}

pub(super) async fn people_to_read(
    store: &SqliteChannelStore,
    organization_id: OrganizationId,
    workspace_id: WorkspaceId,
    connection_id: ChannelConnectionId,
    names: &[String],
    emails: &[String],
    now: Timestamp,
) -> Result<Vec<String>, ChannelError> {
    if names.is_empty() && emails.is_empty() {
        return Ok(Vec::new());
    }
    let asked: Vec<&String> = names.iter().chain(emails).collect();
    let rows = sqlx::query(
        "SELECT external_id, display_name IS NOT NULL AS named, looked_up_at FROM channel_people \
         WHERE organization_id = ? AND workspace_id = ? AND connection_id = ? \
         AND external_id IN (SELECT value FROM json_each(?))",
    )
    .bind(organization_id.uuid())
    .bind(workspace_id.uuid())
    .bind(connection_id.uuid())
    .bind(to_json(&asked)?)
    .fetch_all(store.database.pool())
    .await
    .map_err(failed("people_to_read"))?;
    let mut known: HashMap<String, (bool, Option<Timestamp>)> = HashMap::new();
    for row in &rows {
        let looked_up_at: Option<i64> = row.try_get("looked_up_at").map_err(read)?;
        known.insert(
            row.try_get("external_id").map_err(read)?,
            (
                row.try_get("named").map_err(read)?,
                looked_up_at.map(Timestamp::from_millis),
            ),
        );
    }
    let due = |id: &String, for_email: bool| match known.get(id) {
        None => true,
        Some((named, looked_up_at)) => (for_email || !named) && lookup_stale(*looked_up_at, now),
    };
    let mut wanted: Vec<String> = Vec::new();
    for (id, for_email) in names
        .iter()
        .map(|id| (id, false))
        .chain(emails.iter().map(|id| (id, true)))
    {
        if due(id, for_email) && !wanted.contains(id) {
            wanted.push(id.clone());
        }
    }
    Ok(wanted)
}

pub(super) async fn record_people(
    store: &SqliteChannelStore,
    organization_id: OrganizationId,
    workspace_id: WorkspaceId,
    connection_id: ChannelConnectionId,
    people: &[PersonLookup],
    now: Timestamp,
) -> Result<(), ChannelError> {
    let mut transaction = store.immediate("record_people").await?;
    let live: Option<i64> = sqlx::query_scalar(
        "SELECT 1 FROM channel_connections WHERE organization_id = ? AND workspace_id = ? AND id = ?",
    )
    .bind(organization_id.uuid())
    .bind(workspace_id.uuid())
    .bind(connection_id.uuid())
    .fetch_optional(&mut *transaction)
    .await
    .map_err(failed("record_people"))?;
    if live.is_none() {
        return Err(ChannelError::ConnectionNotFound);
    }
    for lookup in people {
        if fitting(Some(&lookup.external_id), 1, 100).is_none() {
            continue;
        }
        let statement = match &lookup.found {
            // A handle and name are kept once known, so a name a run was
            // sent never changes under it; the email is what it said last.
            Some(person) => sqlx::query(
                "INSERT INTO channel_people (id, organization_id, workspace_id, connection_id, external_id, handle, \
                 display_name, email, is_automated, looked_up_at, first_seen_at, updated_at) \
                 VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?) \
                 ON CONFLICT (organization_id, workspace_id, connection_id, external_id) DO UPDATE \
                 SET handle = coalesce(channel_people.handle, excluded.handle), \
                 display_name = coalesce(channel_people.display_name, excluded.display_name), \
                 email = excluded.email, looked_up_at = excluded.looked_up_at, updated_at = excluded.updated_at",
            )
            .bind(ChannelPersonId::generate().uuid())
            .bind(organization_id.uuid())
            .bind(workspace_id.uuid())
            .bind(connection_id.uuid())
            .bind(&lookup.external_id)
            .bind(fitting(person.handle.as_deref(), 1, 100))
            .bind(fitting(person.display_name.as_deref(), 1, 300))
            .bind(fitting(person.email.as_deref(), 3, 320))
            .bind(person.is_automated),
            // Not someone it shows the connection: when it was asked, and no
            // email a sender list could still match.
            None => sqlx::query(
                "INSERT INTO channel_people (id, organization_id, workspace_id, connection_id, external_id, \
                 is_automated, looked_up_at, first_seen_at, updated_at) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?) \
                 ON CONFLICT (organization_id, workspace_id, connection_id, external_id) DO UPDATE \
                 SET email = NULL, looked_up_at = excluded.looked_up_at, updated_at = excluded.updated_at",
            )
            .bind(ChannelPersonId::generate().uuid())
            .bind(organization_id.uuid())
            .bind(workspace_id.uuid())
            .bind(connection_id.uuid())
            .bind(&lookup.external_id)
            .bind(false),
        };
        statement
            .bind(now.millis())
            .bind(now.millis())
            .bind(now.millis())
            .execute(&mut *transaction)
            .await
            .map_err(failed("record_people"))?;
    }
    transaction.commit().await.map_err(failed("record_people"))?;
    Ok(())
}

pub(super) async fn reply_of(
    store: &SqliteChannelStore,
    organization_id: OrganizationId,
    workspace_id: WorkspaceId,
    run_id: RunId,
) -> Result<Option<ChannelReply>, ChannelError> {
    let mut transaction = store.database.pool().begin().await.map_err(failed("reply_of"))?;
    let row = sqlx::query(
        "SELECT r.status, r.output, r.silent_reason, r.finished_at, r.channel_connection_id, r.channel_thread_key, \
         c.status AS connection_status, v.external_id AS conversation, v.kind, v.is_external \
         FROM runs r \
         JOIN channel_connections c ON c.organization_id = r.organization_id \
         AND c.workspace_id = r.workspace_id AND c.id = r.channel_connection_id \
         JOIN channel_conversations v ON v.organization_id = r.organization_id \
         AND v.workspace_id = r.workspace_id AND v.connection_id = r.channel_connection_id \
         AND v.id = r.channel_conversation_id \
         WHERE r.organization_id = ? AND r.workspace_id = ? AND r.id = ?",
    )
    .bind(organization_id.uuid())
    .bind(workspace_id.uuid())
    .bind(run_id.uuid())
    .fetch_optional(&mut *transaction)
    .await
    .map_err(failed("reply_of"))?;
    let Some(row) = row else {
        return Ok(None);
    };
    let status: String = row.try_get("status").map_err(read)?;
    let output: Option<String> = row.try_get("output").map_err(read)?;
    let silent: Option<String> = row.try_get("silent_reason").map_err(read)?;
    let text = match output {
        Some(text) if status == "completed" && silent.is_none() && !text.trim().is_empty() => text,
        _ => return Ok(None),
    };
    let connection_id = ChannelConnectionId::from_uuid(row.try_get("channel_connection_id").map_err(read)?);
    let hop: Option<i64> = sqlx::query_scalar(
        "SELECT max(agent_hop) FROM channel_dispatches WHERE organization_id = ? AND workspace_id = ? AND run_id = ?",
    )
    .bind(organization_id.uuid())
    .bind(workspace_id.uuid())
    .bind(run_id.uuid())
    .fetch_one(&mut *transaction)
    .await
    .map_err(failed("reply_of"))?;
    let sent: Option<Uuid> = sqlx::query_scalar(
        "SELECT id FROM channel_messages WHERE organization_id = ? AND workspace_id = ? AND connection_id = ? \
         AND run_id = ? AND direction = 'outbound' ORDER BY created_at, id LIMIT 1",
    )
    .bind(organization_id.uuid())
    .bind(workspace_id.uuid())
    .bind(connection_id.uuid())
    .bind(run_id.uuid())
    .fetch_optional(&mut *transaction)
    .await
    .map_err(failed("reply_of"))?;
    transaction.commit().await.map_err(failed("reply_of"))?;
    let connection_status: String = row.try_get("connection_status").map_err(read)?;
    let kind: String = row.try_get("kind").map_err(read)?;
    let finished_at: Option<i64> = row.try_get("finished_at").map_err(read)?;
    Ok(Some(ChannelReply {
        connection_id,
        connection_status: ConnectionStatus::parse(&connection_status)
            .ok_or_else(|| read("an unknown connection status"))?,
        conversation_external_id: row.try_get("conversation").map_err(read)?,
        conversation_kind: ConversationKind::parse(&kind).ok_or_else(|| read("an unknown conversation kind"))?,
        conversation_is_external: row.try_get("is_external").map_err(read)?,
        thread_key: row
            .try_get::<Option<String>, _>("channel_thread_key")
            .map_err(read)?
            .unwrap_or_default(),
        text,
        agent_hop: small(hop.unwrap_or(0))?.saturating_add(1),
        finished_at: Timestamp::from_millis(finished_at.unwrap_or_default()),
        sent: sent.map(ChannelMessageId::from_uuid),
    }))
}

/// Queues the reply job for a channel run that just ended, inside the
/// transaction that ends it, when it ended with an answer: the job reads
/// the rest ([`reply_of`]).
pub(crate) async fn queue_reply(
    connection: &mut SqliteConnection,
    organization_id: OrganizationId,
    workspace_id: WorkspaceId,
    run_id: RunId,
    now: Timestamp,
) -> Result<(), StoreError> {
    let answered: Option<i64> = sqlx::query_scalar(
        "SELECT 1 FROM runs WHERE organization_id = ? AND workspace_id = ? AND id = ? \
         AND channel_session_id IS NOT NULL AND status = 'completed' AND silent_reason IS NULL \
         AND trim(coalesce(output, '')) <> ''",
    )
    .bind(organization_id.uuid())
    .bind(workspace_id.uuid())
    .bind(run_id.uuid())
    .fetch_optional(&mut *connection)
    .await
    .map_err(|error| {
        tracing::error!(event = "channels.database_error", operation = "queue_reply", %error);
        StoreError::Unavailable
    })?;
    if answered.is_none() {
        return Ok(());
    }
    let job = reply_job(run_id).ok_or(StoreError::Unavailable)?;
    crate::job_queue::insert_job_at(
        &mut *connection,
        &Scope::Workspace(organization_id, workspace_id),
        &job.kind,
        job.payload,
        job.options,
        now.millis(),
    )
    .await
    .map_err(|error| {
        tracing::error!(event = "channels.reply_not_queued", %organization_id, %workspace_id, %run_id, %error);
        StoreError::Unavailable
    })?;
    Ok(())
}
