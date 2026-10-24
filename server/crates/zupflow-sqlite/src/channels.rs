//! [`SqliteChannelStore`]: linked teams, generated apps, connections with
//! what they've seen, agent channels, sessions in threads, and the
//! organization's channel tool levels and settings.
//!
//! Every statement repeats the organization and, for anything but the
//! levels and settings, the workspace, joins and subqueries included, next
//! to any unique ID. The statements that start without them are named
//! `_cross_tenant`, each documented where it is declared.
//!
//! Each change is an immediate transaction that first checks, in it, that
//! the acting editor still may make it (an active developer or stricter, or
//! an unrevoked key of the workspace's environment; an admin or owner for
//! the organization's levels and settings), then the rules that must hold
//! across concurrent changes, then writes the change with its announcement
//! and audit event.

use async_trait::async_trait;
use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use sqlx::sqlite::SqliteArguments;
use sqlx::sqlite::SqliteRow;
use sqlx::{Arguments, QueryBuilder, Row, Sqlite, SqliteConnection, Transaction};
use uuid::Uuid;
use zupflow_core::audit::Audit;
use zupflow_core::channels::handling::{catch_up_job, inbound_job};
use zupflow_core::channels::provider::InboundEvent;
use zupflow_core::channels::store::{
    AppCreation, AppFilter, AppSecretsRecord, ChannelEditor, ChannelError, ChannelSession, ChannelStore,
    ConnectionChanges, ConnectionFilter, ConversationFilter, DeliveryOutcome, DispatchOutcome, InboundContext,
    KnownPerson, LevelChange, MadeApp, MessageFilter, NewApp, NewConversation, NewDelivery, NewDispatch, NewInstall,
    NewManager, NewMessage, NewPerson, RecordedMessage, RotationSweep, RunChannelContext, SealedVersion, SendCounts,
    SendSlot, SessionTemplate, Swept, TokenReplacement,
};
use zupflow_core::channels::thread::origin_keys;
use zupflow_core::channels::{
    AgentChannel, AgentChannelSettings, AgentMessages, AppStatus, Audience, BreakerSettings, CHANNEL_TOOLS, ChannelApp,
    ChannelAppManager, ChannelConnection, ChannelConversation, ChannelMessage, ChannelPerson, ChannelProviderKind,
    ChannelSettings, ChannelToolLevel, ConnectTarget, ConnectionStatus, ConnectionTeam, ConversationKind, Direction,
    FIRST_VERSION, HandlerKind, Identity, InboundRef, ManagerMethod, ManagerStatus, MidRun, Persona, Senders, Team,
    ThreadFollowUp, TokenState,
};
use zupflow_core::connect::{ConnectSession, ConnectSessionStatus};
use zupflow_core::ids::{
    AgentChannelId, AgentId, ChannelAppId, ChannelAppManagerId, ChannelConnectionId, ChannelConversationId,
    ChannelMessageId, ChannelPersonId, ConnectSessionId, MemberId, OrganizationId, RunId, ToolCallId, UserId,
    WorkspaceId,
};
use zupflow_core::installations::ShortId;
use zupflow_core::keys::{Sealed, WrappedKey};
use zupflow_core::organizations::User;
use zupflow_core::policy::ToolLevel;
use zupflow_core::runs::QueuedJob;
use zupflow_core::runs::store::StoreError;
use zupflow_core::runs::transition::Steer;
use zupflow_core::scope::Scope;
use zupflow_core::time::Timestamp;
use zupflow_core::webhooks::WebhookEventType;
use zupflow_core::workspaces::Page;

use crate::Database;
use crate::connect::session_columns;
use crate::job_queue::insert_job_at;

pub(crate) mod exchange;
mod lifecycle;

pub(crate) use lifecycle::queue_agent_app_syncs;

const MINUTE_MS: i64 = 60 * 1000;
/// Why an app whose making job is gone or buried failed.
const ABANDONED: &str = "provisioning_abandoned";
const DAY_MS: i64 = 24 * 60 * MINUTE_MS;

pub struct SqliteChannelStore {
    database: Database,
}

impl SqliteChannelStore {
    pub fn new(database: Database) -> Self {
        Self { database }
    }

    async fn immediate(&self, operation: &'static str) -> Result<Transaction<'static, Sqlite>, ChannelError> {
        self.database
            .pool()
            .begin_with("BEGIN IMMEDIATE")
            .await
            .map_err(failed(operation))
    }
}

fn failed(operation: &'static str) -> impl FnOnce(sqlx::Error) -> ChannelError {
    move |error| {
        tracing::error!(event = "channels.database_error", operation, %error);
        ChannelError::Unavailable
    }
}

fn read(error: impl std::fmt::Display) -> ChannelError {
    tracing::error!(event = "channels.unreadable_row", %error);
    ChannelError::Unavailable
}

// --- Columns ---------------------------------------------------------------
// Macros, so queries stay static strings. Each subquery and join holds to the
// row's own organization and workspace.

macro_rules! manager_select {
    () => {
        "SELECT m.id, m.organization_id, m.workspace_id, m.provider, m.method, m.team_id, m.team_name, m.status, \
         m.token_last4, m.token_updated_at, m.credential_version, m.token_expires_at, m.created_at, m.updated_at, \
         u.id AS user_id, u.name AS user_name, u.email AS user_email, u.avatar_url AS user_avatar_url, \
         (SELECT count(*) FROM channel_apps a WHERE a.organization_id = m.organization_id \
          AND a.workspace_id = m.workspace_id AND a.manager_id = m.id AND a.status <> 'deleted') AS app_count \
         FROM channel_app_managers m LEFT JOIN users u ON u.id = m.created_by_user_id"
    };
}

macro_rules! app_columns {
    () => {
        "SELECT a.id, a.organization_id, a.workspace_id, a.manager_id, a.agent_id, a.provider, a.external_app_id, \
         a.status, a.last_error, a.created_at, a.updated_at, \
         (SELECT c.id FROM channel_connections c WHERE c.organization_id = a.organization_id \
          AND c.workspace_id = a.workspace_id AND c.app_id = a.id AND c.status <> 'disconnected') AS connection_id"
    };
}

macro_rules! app_select {
    () => {
        concat!(app_columns!(), " FROM channel_apps a")
    };
}

macro_rules! connection_select {
    () => {
        "SELECT c.id, c.organization_id, c.workspace_id, c.provider, c.app_id, a.agent_id, c.short_id, c.label, \
         c.handler, c.status, c.team_id, c.team_name, c.enterprise_id, c.identity_external_id, c.handle, \
         c.identity_bot_id, c.granted_scopes, c.credential_version, c.connected_at, c.last_event_at, c.created_at, c.updated_at \
         FROM channel_connections c LEFT JOIN channel_apps a \
         ON a.organization_id = c.organization_id AND a.workspace_id = c.workspace_id AND a.id = c.app_id"
    };
}

macro_rules! conversation_select {
    () => {
        "SELECT v.id, v.organization_id, v.workspace_id, v.connection_id, v.external_id, v.kind, v.name, \
         v.is_external, v.identity_is_member, v.last_message_at, v.paused_until, v.created_at \
         FROM channel_conversations v"
    };
}

macro_rules! message_select {
    () => {
        "SELECT g.id, g.organization_id, g.workspace_id, g.connection_id, g.conversation_id, g.external_id, \
         g.thread_key, g.direction, g.text, g.edited_text, g.mentions, g.run_id, g.tool_call_id, g.agent_hop, \
         g.sent_at, g.edited_at, g.deleted_at, g.created_at, p.id AS person_id, p.handle AS person_handle, \
         p.display_name AS person_display_name, p.is_automated AS person_is_automated \
         FROM channel_messages g LEFT JOIN channel_people p ON p.organization_id = g.organization_id \
         AND p.workspace_id = g.workspace_id AND p.connection_id = g.connection_id AND p.id = g.author_person_id"
    };
}

macro_rules! agent_channel_select {
    () => {
        "SELECT h.id, h.organization_id, h.workspace_id, h.agent_id, h.connection_id, h.tools, h.approval_required, \
         h.audience, h.audience_conversations, h.persona, h.answers_dms, h.answers_mentions, h.thread_follow_up, \
         h.agent_messages, h.max_agent_hops, h.mid_run, h.senders, h.sender_list, h.allow_automated, \
         h.allow_external, h.reply_with_output, h.created_at, h.updated_at FROM agent_channels h"
    };
}

const FIND_MANAGER: &str = concat!(
    manager_select!(),
    " WHERE m.organization_id = ? AND m.workspace_id = ? AND m.id = ?"
);
const FIND_APP: &str = concat!(
    app_select!(),
    " WHERE a.organization_id = ? AND a.workspace_id = ? AND a.id = ? AND a.status <> 'deleted'"
);
/// An app whatever its status, with where its making stands.
const FIND_APP_CREATION: &str = concat!(
    app_columns!(),
    ", a.inbound_ref, a.creation_started_at FROM channel_apps a \
     WHERE a.organization_id = ? AND a.workspace_id = ? AND a.id = ?"
);
const FIND_CONNECTION: &str = concat!(
    connection_select!(),
    " WHERE c.organization_id = ? AND c.workspace_id = ? AND c.id = ?"
);
const FIND_CONVERSATION: &str = concat!(
    conversation_select!(),
    " WHERE v.organization_id = ? AND v.workspace_id = ? AND v.connection_id = ? AND v.id = ?"
);
const FIND_MESSAGE: &str = concat!(
    message_select!(),
    " WHERE g.organization_id = ? AND g.workspace_id = ? AND g.id = ?"
);
const FIND_AGENT_CHANNEL_ON: &str = concat!(
    agent_channel_select!(),
    " WHERE h.organization_id = ? AND h.workspace_id = ? AND h.connection_id = ?"
);

// After the column macros, which it uses.
mod tooling;

/// An active member who may change a workspace's channels: a developer or stricter.
const WORKSPACE_MEMBER: &str = "SELECT 1 FROM members mb JOIN workspaces w ON w.organization_id = mb.organization_id \
     WHERE mb.organization_id = ? AND w.organization_id = ? AND w.id = ? AND mb.id = ? \
     AND mb.status = 'active' AND mb.role IN ('owner', 'admin', 'developer')";
/// An unrevoked key of the workspace's environment.
const WORKSPACE_KEY: &str = "SELECT 1 FROM api_keys k JOIN workspaces w \
     ON w.organization_id = k.organization_id AND w.environment = k.environment \
     WHERE k.organization_id = ? AND w.organization_id = ? AND w.id = ? AND k.id = ? AND k.revoked_at IS NULL";
/// An active admin or owner, for the organization's levels and settings.
const ORGANIZATION_ADMIN: &str = "SELECT 1 FROM members WHERE organization_id = ? AND id = ? \
     AND status = 'active' AND role IN ('owner', 'admin')";

// --- Rows ------------------------------------------------------------------

fn text(row: &SqliteRow, column: &str) -> Result<String, ChannelError> {
    row.try_get(column).map_err(read)
}

fn at(row: &SqliteRow, column: &str) -> Result<Timestamp, ChannelError> {
    Ok(Timestamp::from_millis(row.try_get(column).map_err(read)?))
}

fn maybe_at(row: &SqliteRow, column: &str) -> Result<Option<Timestamp>, ChannelError> {
    let value: Option<i64> = row.try_get(column).map_err(read)?;
    Ok(value.map(Timestamp::from_millis))
}

fn uuid(row: &SqliteRow, column: &str) -> Result<Uuid, ChannelError> {
    row.try_get(column).map_err(read)
}

fn maybe_uuid(row: &SqliteRow, column: &str) -> Result<Option<Uuid>, ChannelError> {
    row.try_get(column).map_err(read)
}

fn small(value: i64) -> Result<u32, ChannelError> {
    u32::try_from(value).map_err(read)
}

fn parsed<T>(row: &SqliteRow, column: &str, parse: fn(&str) -> Option<T>) -> Result<T, ChannelError> {
    let value = text(row, column)?;
    parse(&value).ok_or_else(|| read(format!("an unknown {column}")))
}

fn json_list(row: &SqliteRow, column: &str) -> Result<Vec<String>, ChannelError> {
    serde_json::from_str(&text(row, column)?).map_err(read)
}

fn to_json(value: &impl serde::Serialize) -> Result<String, ChannelError> {
    serde_json::to_string(value).map_err(read)
}

fn user_of(row: &SqliteRow) -> Result<Option<User>, ChannelError> {
    let Some(id) = maybe_uuid(row, "user_id")? else {
        return Ok(None);
    };
    Ok(Some(User {
        id: UserId::from_uuid(id),
        name: text(row, "user_name")?,
        email: text(row, "user_email")?,
        avatar_url: row.try_get("user_avatar_url").map_err(read)?,
    }))
}

fn manager_from(row: &SqliteRow) -> Result<ChannelAppManager, ChannelError> {
    let app_count: i64 = row.try_get("app_count").map_err(read)?;
    let version: i64 = row.try_get("credential_version").map_err(read)?;
    Ok(ChannelAppManager {
        id: ChannelAppManagerId::from_uuid(uuid(row, "id")?),
        organization_id: OrganizationId::from_uuid(uuid(row, "organization_id")?),
        workspace_id: WorkspaceId::from_uuid(uuid(row, "workspace_id")?),
        provider: parsed(row, "provider", ChannelProviderKind::parse)?,
        method: parsed(row, "method", ManagerMethod::parse)?,
        team: Team {
            id: row.try_get("team_id").map_err(read)?,
            name: row.try_get("team_name").map_err(read)?,
        },
        status: parsed(row, "status", ManagerStatus::parse)?,
        configuration_token: TokenState {
            set: true,
            last4: Some(text(row, "token_last4")?),
            updated_at: Some(at(row, "token_updated_at")?),
        },
        credential_version: small(version)?,
        token_expires_at: maybe_at(row, "token_expires_at")?,
        app_count: u64::try_from(app_count).map_err(read)?,
        created_by: user_of(row)?,
        created_at: at(row, "created_at")?,
        updated_at: at(row, "updated_at")?,
    })
}

fn app_from(row: &SqliteRow) -> Result<ChannelApp, ChannelError> {
    Ok(ChannelApp {
        id: ChannelAppId::from_uuid(uuid(row, "id")?),
        organization_id: OrganizationId::from_uuid(uuid(row, "organization_id")?),
        workspace_id: WorkspaceId::from_uuid(uuid(row, "workspace_id")?),
        manager_id: ChannelAppManagerId::from_uuid(uuid(row, "manager_id")?),
        agent_id: AgentId::from_uuid(uuid(row, "agent_id")?),
        provider: parsed(row, "provider", ChannelProviderKind::parse)?,
        external_app_id: row.try_get("external_app_id").map_err(read)?,
        status: parsed(row, "status", AppStatus::parse)?,
        last_error: row.try_get("last_error").map_err(read)?,
        connection_id: maybe_uuid(row, "connection_id")?.map(ChannelConnectionId::from_uuid),
        created_at: at(row, "created_at")?,
        updated_at: at(row, "updated_at")?,
    })
}

fn connection_from(row: &SqliteRow) -> Result<ChannelConnection, ChannelError> {
    let version: Option<i64> = row.try_get("credential_version").map_err(read)?;
    Ok(ChannelConnection {
        id: ChannelConnectionId::from_uuid(uuid(row, "id")?),
        organization_id: OrganizationId::from_uuid(uuid(row, "organization_id")?),
        workspace_id: WorkspaceId::from_uuid(uuid(row, "workspace_id")?),
        provider: parsed(row, "provider", ChannelProviderKind::parse)?,
        app_id: maybe_uuid(row, "app_id")?.map(ChannelAppId::from_uuid),
        agent_id: maybe_uuid(row, "agent_id")?.map(AgentId::from_uuid),
        short_id: ShortId::parse(&text(row, "short_id")?).ok_or_else(|| read("an invalid short ID"))?,
        label: text(row, "label")?,
        handler: parsed(row, "handler", HandlerKind::parse)?,
        status: parsed(row, "status", ConnectionStatus::parse)?,
        team: ConnectionTeam {
            id: text(row, "team_id")?,
            name: row.try_get("team_name").map_err(read)?,
            enterprise_id: row.try_get("enterprise_id").map_err(read)?,
        },
        identity: Identity {
            external_id: text(row, "identity_external_id")?,
            handle: text(row, "handle")?,
            bot_id: row.try_get("identity_bot_id").map_err(read)?,
        },
        granted_scopes: json_list(row, "granted_scopes")?,
        credential_version: version.map(small).transpose()?,
        connected_at: maybe_at(row, "connected_at")?,
        last_event_at: maybe_at(row, "last_event_at")?,
        created_at: at(row, "created_at")?,
        updated_at: at(row, "updated_at")?,
    })
}

fn conversation_from(row: &SqliteRow) -> Result<ChannelConversation, ChannelError> {
    Ok(ChannelConversation {
        id: ChannelConversationId::from_uuid(uuid(row, "id")?),
        organization_id: OrganizationId::from_uuid(uuid(row, "organization_id")?),
        workspace_id: WorkspaceId::from_uuid(uuid(row, "workspace_id")?),
        connection_id: ChannelConnectionId::from_uuid(uuid(row, "connection_id")?),
        external_id: text(row, "external_id")?,
        kind: parsed(row, "kind", ConversationKind::parse)?,
        name: row.try_get("name").map_err(read)?,
        is_external: row.try_get("is_external").map_err(read)?,
        identity_is_member: row.try_get("identity_is_member").map_err(read)?,
        last_message_at: maybe_at(row, "last_message_at")?,
        paused_until: maybe_at(row, "paused_until")?,
        created_at: at(row, "created_at")?,
    })
}

fn message_from(row: &SqliteRow) -> Result<ChannelMessage, ChannelError> {
    let author = match maybe_uuid(row, "person_id")? {
        None => None,
        Some(id) => Some(ChannelPerson {
            id: ChannelPersonId::from_uuid(id),
            handle: row.try_get("person_handle").map_err(read)?,
            display_name: row.try_get("person_display_name").map_err(read)?,
            is_automated: row.try_get("person_is_automated").map_err(read)?,
        }),
    };
    let hop: i64 = row.try_get("agent_hop").map_err(read)?;
    Ok(ChannelMessage {
        id: ChannelMessageId::from_uuid(uuid(row, "id")?),
        organization_id: OrganizationId::from_uuid(uuid(row, "organization_id")?),
        workspace_id: WorkspaceId::from_uuid(uuid(row, "workspace_id")?),
        connection_id: ChannelConnectionId::from_uuid(uuid(row, "connection_id")?),
        conversation_id: ChannelConversationId::from_uuid(uuid(row, "conversation_id")?),
        external_id: text(row, "external_id")?,
        thread_key: text(row, "thread_key")?,
        direction: parsed(row, "direction", Direction::parse)?,
        author,
        text: text(row, "text")?,
        edited_text: row.try_get("edited_text").map_err(read)?,
        mentions: json_list(row, "mentions")?,
        run_id: maybe_uuid(row, "run_id")?.map(RunId::from_uuid),
        tool_call_id: maybe_uuid(row, "tool_call_id")?.map(ToolCallId::from_uuid),
        agent_hop: small(hop)?,
        sent_at: at(row, "sent_at")?,
        edited_at: maybe_at(row, "edited_at")?,
        deleted_at: maybe_at(row, "deleted_at")?,
        created_at: at(row, "created_at")?,
    })
}

fn agent_channel_from(row: &SqliteRow) -> Result<AgentChannel, ChannelError> {
    let conversations = json_list(row, "audience_conversations")?
        .iter()
        .map(|id| Uuid::parse_str(id).map(ChannelConversationId::from_uuid))
        .collect::<Result<Vec<_>, _>>()
        .map_err(read)?;
    let persona: Option<String> = row.try_get("persona").map_err(read)?;
    let hops: i64 = row.try_get("max_agent_hops").map_err(read)?;
    Ok(AgentChannel {
        id: AgentChannelId::from_uuid(uuid(row, "id")?),
        organization_id: OrganizationId::from_uuid(uuid(row, "organization_id")?),
        workspace_id: WorkspaceId::from_uuid(uuid(row, "workspace_id")?),
        agent_id: AgentId::from_uuid(uuid(row, "agent_id")?),
        connection_id: ChannelConnectionId::from_uuid(uuid(row, "connection_id")?),
        settings: AgentChannelSettings {
            tools: json_list(row, "tools")?,
            approval_required: json_list(row, "approval_required")?,
            audience: parsed(row, "audience", Audience::parse)?,
            audience_conversations: conversations,
            persona: persona
                .map(|persona| serde_json::from_str::<Persona>(&persona))
                .transpose()
                .map_err(read)?,
            answers_dms: row.try_get("answers_dms").map_err(read)?,
            answers_mentions: row.try_get("answers_mentions").map_err(read)?,
            thread_follow_up: parsed(row, "thread_follow_up", ThreadFollowUp::parse)?,
            agent_messages: parsed(row, "agent_messages", AgentMessages::parse)?,
            max_agent_hops: small(hops)?,
            mid_run: parsed(row, "mid_run", MidRun::parse)?,
            senders: parsed(row, "senders", Senders::parse)?,
            sender_list: json_list(row, "sender_list")?,
            allow_automated: row.try_get("allow_automated").map_err(read)?,
            allow_external: row.try_get("allow_external").map_err(read)?,
            reply_with_output: row.try_get("reply_with_output").map_err(read)?,
        },
        created_at: at(row, "created_at")?,
        updated_at: at(row, "updated_at")?,
    })
}

fn sealed_from(row: &SqliteRow) -> Result<SealedVersion, ChannelError> {
    let version: i64 = row.try_get("version").map_err(read)?;
    let nonce: Vec<u8> = row.try_get("nonce").map_err(read)?;
    Ok(SealedVersion {
        version: small(version)?,
        sealed: Sealed {
            wrapped_key: WrappedKey {
                key_id: text(row, "key_id")?,
                bytes: row.try_get("wrapped_key").map_err(read)?,
            },
            nonce: nonce.try_into().map_err(|_| read("a nonce that isn't 12 bytes"))?,
            ciphertext: row.try_get("ciphertext").map_err(read)?,
        },
    })
}

// --- Pages -----------------------------------------------------------------

/// A list query that starts with its whole scope, bound: `statement` ends
/// `WHERE x.organization_id = ? AND x.workspace_id = ?`, so the scope is in
/// the statement's own text, not only in what's pushed after it.
fn scoped(
    statement: &'static str,
    organization_id: OrganizationId,
    workspace_id: WorkspaceId,
) -> Result<QueryBuilder<Sqlite>, ChannelError> {
    let mut arguments = SqliteArguments::default();
    arguments.add(organization_id.uuid()).map_err(read)?;
    arguments.add(workspace_id.uuid()).map_err(read)?;
    Ok(QueryBuilder::with_arguments(statement, arguments))
}

/// A position in a list: its sort key, then its ID, newest first.
struct Position {
    key: i64,
    id: Uuid,
}

impl Position {
    /// The row's prefixed ID, never its bare UUID: a cursor leaves the server.
    fn encode(key: i64, id: impl std::fmt::Display) -> String {
        URL_SAFE_NO_PAD.encode(format!("{key}\n{id}"))
    }

    fn decode(cursor: Option<&str>, parse: fn(&str) -> Option<Uuid>) -> Result<Option<Self>, ChannelError> {
        let Some(cursor) = cursor else { return Ok(None) };
        let decoded = || -> Option<Self> {
            let raw = String::from_utf8(URL_SAFE_NO_PAD.decode(cursor).ok()?).ok()?;
            let (key, id) = raw.split_once('\n')?;
            Some(Self {
                key: key.parse().ok()?,
                id: parse(id)?,
            })
        };
        decoded().map(Some).ok_or(ChannelError::InvalidCursor)
    }

    /// `AND (key < ? OR (key = ? AND id < ?))`, for a list sorted by both descending.
    fn push_after(after: &Option<Self>, query: &mut QueryBuilder<Sqlite>, key: &str, id: &str) {
        if let Some(after) = after {
            query.push(format!(" AND ({key} < "));
            query.push_bind(after.key);
            query.push(format!(" OR ({key} = "));
            query.push_bind(after.key);
            query.push(format!(" AND {id} < "));
            query.push_bind(after.id);
            query.push("))");
        }
    }
}

fn limit_of(limit: u32) -> u32 {
    limit.clamp(1, zupflow_core::channels::MAX_PAGE)
}

/// The page out of `limit + 1` rows, with the cursor after its last item.
fn page_of<T>(
    mut items: Vec<T>,
    limit: u32,
    total: i64,
    position: impl Fn(&T) -> String,
) -> Result<Page<T>, ChannelError> {
    let next_cursor = if items.len() > limit as usize {
        items.truncate(limit as usize);
        items.last().map(position)
    } else {
        None
    };
    Ok(Page {
        items,
        next_cursor,
        total: u64::try_from(total).map_err(read)?,
    })
}

fn empty<T>() -> Page<T> {
    Page {
        items: Vec::new(),
        next_cursor: None,
        total: 0,
    }
}

// --- Checks inside a transaction ----------------------------------------------

/// Whether `editor` may still change the workspace's channels.
async fn editor_allowed(
    connection: &mut SqliteConnection,
    organization_id: OrganizationId,
    workspace_id: WorkspaceId,
    editor: ChannelEditor,
) -> Result<(), ChannelError> {
    let (query, actor) = match editor {
        ChannelEditor::Member(member) => (WORKSPACE_MEMBER, member.uuid()),
        ChannelEditor::ApiKey(key) => (WORKSPACE_KEY, key.uuid()),
    };
    let found = sqlx::query(query)
        .bind(organization_id.uuid())
        .bind(organization_id.uuid())
        .bind(workspace_id.uuid())
        .bind(actor)
        .fetch_optional(connection)
        .await
        .map_err(failed("editor_allowed"))?;
    found.map(|_| ()).ok_or(ChannelError::NotAllowed)
}

/// Whether `member` may still change the organization's levels and settings.
async fn admin_allowed(
    connection: &mut SqliteConnection,
    organization_id: OrganizationId,
    member: MemberId,
) -> Result<(), ChannelError> {
    let found = sqlx::query(ORGANIZATION_ADMIN)
        .bind(organization_id.uuid())
        .bind(member.uuid())
        .fetch_optional(connection)
        .await
        .map_err(failed("admin_allowed"))?;
    found.map(|_| ()).ok_or(ChannelError::NotAllowed)
}

async fn announce(
    transaction: &mut SqliteConnection,
    organization_id: OrganizationId,
    workspace_id: WorkspaceId,
    kind: WebhookEventType,
    data: serde_json::Value,
    at: Timestamp,
) -> Result<(), ChannelError> {
    crate::webhooks::record_lifecycle(transaction, organization_id, workspace_id, kind, data, at)
        .await
        .map_err(|error| {
            tracing::error!(event = "channels.announcement_not_recorded", %error);
            ChannelError::Unavailable
        })
}

async fn audit_in_workspace(
    connection: &mut SqliteConnection,
    organization_id: OrganizationId,
    workspace_id: WorkspaceId,
    entry: &zupflow_core::audit::AuditEntry,
) -> Result<(), ChannelError> {
    crate::audit::record_in_workspace(connection, organization_id, workspace_id, entry)
        .await
        .map_err(failed("audit"))?;
    Ok(())
}

async fn find_manager_in(
    connection: &mut SqliteConnection,
    organization_id: OrganizationId,
    workspace_id: WorkspaceId,
    manager_id: ChannelAppManagerId,
) -> Result<Option<ChannelAppManager>, ChannelError> {
    let row = sqlx::query(FIND_MANAGER)
        .bind(organization_id.uuid())
        .bind(workspace_id.uuid())
        .bind(manager_id.uuid())
        .fetch_optional(connection)
        .await
        .map_err(failed("find_manager"))?;
    row.as_ref().map(manager_from).transpose()
}

async fn find_app_in(
    connection: &mut SqliteConnection,
    organization_id: OrganizationId,
    workspace_id: WorkspaceId,
    app_id: ChannelAppId,
) -> Result<Option<ChannelApp>, ChannelError> {
    let row = sqlx::query(FIND_APP)
        .bind(organization_id.uuid())
        .bind(workspace_id.uuid())
        .bind(app_id.uuid())
        .fetch_optional(connection)
        .await
        .map_err(failed("find_app"))?;
    row.as_ref().map(app_from).transpose()
}

async fn find_connection_in(
    connection: &mut SqliteConnection,
    organization_id: OrganizationId,
    workspace_id: WorkspaceId,
    connection_id: ChannelConnectionId,
) -> Result<Option<ChannelConnection>, ChannelError> {
    let row = sqlx::query(FIND_CONNECTION)
        .bind(organization_id.uuid())
        .bind(workspace_id.uuid())
        .bind(connection_id.uuid())
        .fetch_optional(connection)
        .await
        .map_err(failed("find_connection"))?;
    row.as_ref().map(connection_from).transpose()
}

/// The app's connection that isn't disconnected, in the app's scope.
async fn live_connection_of(
    connection: &mut SqliteConnection,
    organization_id: OrganizationId,
    workspace_id: WorkspaceId,
    app_id: ChannelAppId,
) -> Result<Option<ChannelConnection>, ChannelError> {
    let row = sqlx::query(concat!(
        connection_select!(),
        " WHERE c.organization_id = ? AND c.workspace_id = ? AND c.app_id = ? AND c.status <> 'disconnected'"
    ))
    .bind(organization_id.uuid())
    .bind(workspace_id.uuid())
    .bind(app_id.uuid())
    .fetch_optional(connection)
    .await
    .map_err(failed("live_connection"))?;
    row.as_ref().map(connection_from).transpose()
}

async fn find_agent_channel_on(
    connection: &mut SqliteConnection,
    organization_id: OrganizationId,
    workspace_id: WorkspaceId,
    connection_id: ChannelConnectionId,
) -> Result<Option<AgentChannel>, ChannelError> {
    let row = sqlx::query(FIND_AGENT_CHANNEL_ON)
        .bind(organization_id.uuid())
        .bind(workspace_id.uuid())
        .bind(connection_id.uuid())
        .fetch_optional(connection)
        .await
        .map_err(failed("find_agent_channel"))?;
    row.as_ref().map(agent_channel_from).transpose()
}

async fn agent_exists(
    connection: &mut SqliteConnection,
    organization_id: OrganizationId,
    workspace_id: WorkspaceId,
    agent_id: AgentId,
) -> Result<bool, ChannelError> {
    let found = sqlx::query("SELECT 1 FROM agents WHERE organization_id = ? AND workspace_id = ? AND id = ?")
        .bind(organization_id.uuid())
        .bind(workspace_id.uuid())
        .bind(agent_id.uuid())
        .fetch_optional(connection)
        .await
        .map_err(failed("agent_exists"))?;
    Ok(found.is_some())
}

/// Inserts one sealed version of a secret into `table`, a credential table
/// whose owner column is `owner`.
#[allow(clippy::too_many_arguments)]
async fn insert_sealed(
    connection: &mut SqliteConnection,
    statement: &'static str,
    organization_id: OrganizationId,
    workspace_id: WorkspaceId,
    owner: Uuid,
    version: u32,
    sealed: &Sealed,
    now: Timestamp,
) -> Result<(), ChannelError> {
    sqlx::query(statement)
        .bind(Uuid::now_v7())
        .bind(organization_id.uuid())
        .bind(workspace_id.uuid())
        .bind(owner)
        .bind(i64::from(version))
        .bind(&sealed.wrapped_key.key_id)
        .bind(sealed.wrapped_key.bytes.as_slice())
        .bind(sealed.nonce.as_slice())
        .bind(sealed.ciphertext.as_slice())
        .bind(now.millis())
        .execute(connection)
        .await
        .map_err(failed("insert_sealed"))?;
    Ok(())
}

const INSERT_MANAGER_CREDENTIAL: &str = "INSERT INTO channel_app_manager_credentials (id, organization_id, \
     workspace_id, manager_id, version, key_id, wrapped_key, nonce, ciphertext, created_at) \
     VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?)";
const INSERT_APP_SECRET: &str = "INSERT INTO channel_app_secrets (id, organization_id, workspace_id, app_id, \
     version, key_id, wrapped_key, nonce, ciphertext, created_at) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?)";
const INSERT_CONNECTION_CREDENTIAL: &str = "INSERT INTO channel_connection_credentials (id, organization_id, \
     workspace_id, connection_id, version, key_id, wrapped_key, nonce, ciphertext, created_at) \
     VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?)";

fn last4(value: &str) -> Result<&str, ChannelError> {
    if (1..=4).contains(&value.chars().count()) {
        Ok(value)
    } else {
        Err(read("a token's last four that isn't one to four characters"))
    }
}

/// Inserts a manager and its first tokens, announced as `active`.
async fn insert_manager(
    transaction: &mut Transaction<'static, Sqlite>,
    organization_id: OrganizationId,
    workspace_id: WorkspaceId,
    manager_id: ChannelAppManagerId,
    manager: &NewManager,
    now: Timestamp,
) -> Result<ChannelAppManager, ChannelError> {
    sqlx::query(
        "INSERT INTO channel_app_managers (id, organization_id, workspace_id, provider, method, status, token_last4, \
         token_updated_at, credential_version, token_expires_at, created_by_user_id, created_at, updated_at) \
         VALUES (?, ?, ?, ?, ?, 'active', ?, ?, ?, ?, ?, ?, ?)",
    )
    .bind(manager_id.uuid())
    .bind(organization_id.uuid())
    .bind(workspace_id.uuid())
    .bind(manager.provider.as_str())
    .bind(manager.method.as_str())
    .bind(last4(&manager.token_last4)?)
    .bind(now.millis())
    .bind(i64::from(FIRST_VERSION))
    .bind(manager.token_expires_at.map(Timestamp::millis))
    .bind(manager.created_by.map(|user| user.uuid()))
    .bind(now.millis())
    .bind(now.millis())
    .execute(&mut **transaction)
    .await
    .map_err(failed("insert_manager"))?;
    insert_sealed(
        transaction,
        INSERT_MANAGER_CREDENTIAL,
        organization_id,
        workspace_id,
        manager_id.uuid(),
        FIRST_VERSION,
        &manager.credential,
        now,
    )
    .await?;
    announce(
        transaction,
        organization_id,
        workspace_id,
        WebhookEventType::ChannelAppManagerStatusChanged,
        serde_json::json!({ "object": "channel_app_manager", "id": manager_id, "status": "active" }),
        now,
    )
    .await?;
    find_manager_in(transaction, organization_id, workspace_id, manager_id)
        .await?
        .ok_or_else(|| read("a manager gone inside its own transaction"))
}

/// Sets an app's status, announced when it changes.
async fn set_app_status(
    transaction: &mut Transaction<'static, Sqlite>,
    organization_id: OrganizationId,
    workspace_id: WorkspaceId,
    app: &ChannelApp,
    status: AppStatus,
    now: Timestamp,
) -> Result<(), ChannelError> {
    if app.status == status {
        return Ok(());
    }
    sqlx::query(
        "UPDATE channel_apps SET status = ?, updated_at = ? WHERE organization_id = ? AND workspace_id = ? AND id = ?",
    )
    .bind(status.as_str())
    .bind(now.millis())
    .bind(organization_id.uuid())
    .bind(workspace_id.uuid())
    .bind(app.id.uuid())
    .execute(&mut **transaction)
    .await
    .map_err(failed("set_app_status"))?;
    announce(
        transaction,
        organization_id,
        workspace_id,
        WebhookEventType::ChannelAppStatusChanged,
        serde_json::json!({ "object": "channel_app", "id": app.id, "agent_id": app.agent_id, "status": status }),
        now,
    )
    .await
}

/// A `creating` app the provider didn't make: `failed` with a stable code,
/// announced. It stays in flight if it was: what an attempt made is still
/// to be deleted at the provider.
async fn fail_app(
    transaction: &mut Transaction<'static, Sqlite>,
    organization_id: OrganizationId,
    workspace_id: WorkspaceId,
    app: &ChannelApp,
    code: &str,
    now: Timestamp,
) -> Result<(), ChannelError> {
    sqlx::query(
        "UPDATE channel_apps SET last_error = ?, status = 'failed', updated_at = ? \
         WHERE organization_id = ? AND workspace_id = ? AND id = ? AND status = 'creating'",
    )
    .bind(code)
    .bind(now.millis())
    .bind(organization_id.uuid())
    .bind(workspace_id.uuid())
    .bind(app.id.uuid())
    .execute(&mut **transaction)
    .await
    .map_err(failed("fail_app"))?;
    announce(
        transaction,
        organization_id,
        workspace_id,
        WebhookEventType::ChannelAppStatusChanged,
        serde_json::json!({ "object": "channel_app", "id": app.id, "agent_id": app.agent_id, "status": "failed" }),
        now,
    )
    .await
}

/// Sets a connection's status, announced when it changes.
async fn set_connection_status(
    transaction: &mut SqliteConnection,
    organization_id: OrganizationId,
    workspace_id: WorkspaceId,
    connection: &ChannelConnection,
    status: ConnectionStatus,
    now: Timestamp,
) -> Result<(), ChannelError> {
    if connection.status == status {
        return Ok(());
    }
    sqlx::query(
        "UPDATE channel_connections SET status = ?, updated_at = ? \
         WHERE organization_id = ? AND workspace_id = ? AND id = ?",
    )
    .bind(status.as_str())
    .bind(now.millis())
    .bind(organization_id.uuid())
    .bind(workspace_id.uuid())
    .bind(connection.id.uuid())
    .execute(&mut *transaction)
    .await
    .map_err(failed("set_connection_status"))?;
    announce(
        transaction,
        organization_id,
        workspace_id,
        WebhookEventType::ChannelConnectionStatusChanged,
        serde_json::json!({
            "object": "channel_connection", "id": connection.id, "app_id": connection.app_id, "status": status,
        }),
        now,
    )
    .await
}

/// [`ChannelStore::install_app`]'s writes, in `transaction`.
pub(crate) async fn install_app_in(
    transaction: &mut Transaction<'static, Sqlite>,
    organization_id: OrganizationId,
    workspace_id: WorkspaceId,
    app_id: ChannelAppId,
    install: &NewInstall,
    now: Timestamp,
) -> Result<(ChannelConnection, AgentChannel), ChannelError> {
    let app = find_app_in(transaction, organization_id, workspace_id, app_id)
        .await?
        .ok_or(ChannelError::AppNotFound)?;
    if !app.status.is_installable() {
        return Err(ChannelError::AppNotReady);
    }
    let manager = find_manager_in(transaction, organization_id, workspace_id, app.manager_id)
        .await?
        .filter(|manager| manager.status == ManagerStatus::Active)
        .ok_or(ChannelError::ManagerInactive)?;
    match manager.team.id.as_deref() {
        Some(team) if team != install.team.id => return Err(ChannelError::TeamMismatch),
        _ => {}
    }
    sqlx::query(
        "UPDATE channel_app_managers SET team_id = ?, team_name = ?, updated_at = ? \
         WHERE organization_id = ? AND workspace_id = ? AND id = ?",
    )
    .bind(&install.team.id)
    .bind(install.team.name.as_deref())
    .bind(now.millis())
    .bind(organization_id.uuid())
    .bind(workspace_id.uuid())
    .bind(manager.id.uuid())
    .execute(&mut **transaction)
    .await
    .map_err(failed("install_app"))?;
    let scopes = to_json(&install.granted_scopes)?;
    let live = live_connection_of(transaction, organization_id, workspace_id, app_id).await?;
    let connection_id = match live {
        Some(live) => {
            let expected = live.credential_version.unwrap_or(0).checked_add(1);
            if live.id != install.connection_id || Some(install.credential.version) != expected {
                return Err(ChannelError::InstallChanged);
            }
            sqlx::query(
                // The same identity keeps the bot ID learned from its
                // posts when the install reports none.
                "UPDATE channel_connections SET team_name = ?, enterprise_id = ?, identity_external_id = ?, \
                 handle = ?, identity_bot_id = CASE WHEN identity_external_id = ? \
                 THEN coalesce(?, identity_bot_id) ELSE ? END, granted_scopes = ?, credential_version = ?, \
                 connected_at = ?, updated_at = ? \
                 WHERE organization_id = ? AND workspace_id = ? AND id = ?",
            )
            .bind(install.team.name.as_deref())
            .bind(install.team.enterprise_id.as_deref())
            .bind(&install.identity.external_id)
            .bind(&install.identity.handle)
            .bind(&install.identity.external_id)
            .bind(install.identity.bot_id.as_deref())
            .bind(install.identity.bot_id.as_deref())
            .bind(&scopes)
            .bind(i64::from(install.credential.version))
            .bind(now.millis())
            .bind(now.millis())
            .bind(organization_id.uuid())
            .bind(workspace_id.uuid())
            .bind(live.id.uuid())
            .execute(&mut **transaction)
            .await
            .map_err(failed("install_app"))?;
            set_connection_status(
                transaction,
                organization_id,
                workspace_id,
                &live,
                ConnectionStatus::Connected,
                now,
            )
            .await?;
            live.id
        }
        None => {
            if install.credential.version != FIRST_VERSION {
                return Err(ChannelError::InstallChanged);
            }
            let mut draw = crate::installations::random_short_id;
            let short_id = crate::installations::issue_short_id(
                transaction,
                organization_id,
                workspace_id,
                &mut draw,
                now.millis(),
            )
            .await
            .map_err(|_| ChannelError::Unavailable)?;
            sqlx::query(
                "INSERT INTO channel_connections (id, organization_id, workspace_id, provider, app_id, short_id, \
                 label, handler, status, team_id, team_name, enterprise_id, identity_external_id, handle, \
                 identity_bot_id, granted_scopes, credential_version, connected_at, created_at, updated_at) \
                 VALUES (?, ?, ?, ?, ?, ?, ?, 'mention', 'connected', ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
            )
            .bind(install.connection_id.uuid())
            .bind(organization_id.uuid())
            .bind(workspace_id.uuid())
            .bind(app.provider.as_str())
            .bind(app_id.uuid())
            .bind(short_id.as_str())
            .bind(&install.label)
            .bind(&install.team.id)
            .bind(install.team.name.as_deref())
            .bind(install.team.enterprise_id.as_deref())
            .bind(&install.identity.external_id)
            .bind(&install.identity.handle)
            .bind(install.identity.bot_id.as_deref())
            .bind(&scopes)
            .bind(i64::from(install.credential.version))
            .bind(now.millis())
            .bind(now.millis())
            .bind(now.millis())
            .execute(&mut **transaction)
            .await
            .map_err(failed("install_app"))?;
            announce(
                transaction,
                organization_id,
                workspace_id,
                WebhookEventType::ChannelConnectionStatusChanged,
                serde_json::json!({
                    "object": "channel_connection", "id": install.connection_id, "app_id": app_id,
                    "status": "connected",
                }),
                now,
            )
            .await?;
            install.connection_id
        }
    };
    insert_sealed(
        transaction,
        INSERT_CONNECTION_CREDENTIAL,
        organization_id,
        workspace_id,
        connection_id.uuid(),
        install.credential.version,
        &install.credential.sealed,
        now,
    )
    .await?;
    sqlx::query(
        "DELETE FROM channel_connection_credentials \
         WHERE organization_id = ? AND workspace_id = ? AND connection_id = ? AND version < ?",
    )
    .bind(organization_id.uuid())
    .bind(workspace_id.uuid())
    .bind(connection_id.uuid())
    .bind(i64::from(install.credential.version))
    .execute(&mut **transaction)
    .await
    .map_err(failed("install_app"))?;
    if find_agent_channel_on(transaction, organization_id, workspace_id, connection_id)
        .await?
        .is_none()
    {
        write_agent_channel(
            transaction,
            organization_id,
            workspace_id,
            None,
            app.agent_id,
            connection_id,
            &AgentChannelSettings::default(),
            now,
        )
        .await?;
    }
    set_app_status(
        transaction,
        organization_id,
        workspace_id,
        &app,
        AppStatus::Installed,
        now,
    )
    .await?;
    let connection = find_connection_in(transaction, organization_id, workspace_id, connection_id)
        .await?
        .ok_or_else(|| read("a connection gone inside its own transaction"))?;
    let channel = find_agent_channel_on(transaction, organization_id, workspace_id, connection_id)
        .await?
        .ok_or_else(|| read("an agent channel gone inside its own transaction"))?;
    Ok((connection, channel))
}

/// Removes an app that isn't deleted: its live connection disconnected and
/// the agent's channel on it removed, its history kept; the app `deleted`.
async fn remove_app(
    transaction: &mut Transaction<'static, Sqlite>,
    organization_id: OrganizationId,
    workspace_id: WorkspaceId,
    app: &ChannelApp,
    now: Timestamp,
) -> Result<(), ChannelError> {
    let live = live_connection_of(transaction, organization_id, workspace_id, app.id).await?;
    if let Some(live) = &live {
        set_connection_status(
            transaction,
            organization_id,
            workspace_id,
            live,
            ConnectionStatus::Disconnected,
            now,
        )
        .await?;
        sqlx::query("DELETE FROM agent_channels WHERE organization_id = ? AND workspace_id = ? AND connection_id = ?")
            .bind(organization_id.uuid())
            .bind(workspace_id.uuid())
            .bind(live.id.uuid())
            .execute(&mut **transaction)
            .await
            .map_err(failed("remove_app"))?;
    }
    // Made at its provider: removed there too (its live token revoked
    // first), by a job that commits with this; until then it's unreleased.
    if app.external_app_id.is_some() && app.status.is_installable() {
        let connection_id = live.as_ref().map(|live| live.id);
        lifecycle::queue_release(transaction, organization_id, workspace_id, app.id, connection_id, now).await?;
    } else {
        lifecycle::released_at_once(transaction, organization_id, workspace_id, app.id, now).await?;
    }
    set_app_status(transaction, organization_id, workspace_id, app, AppStatus::Deleted, now).await
}

/// An agent's apps go with it: called by the agent store inside the
/// transaction that deletes the agent, before it does.
pub(crate) async fn remove_agent_apps(
    transaction: &mut Transaction<'static, Sqlite>,
    organization_id: OrganizationId,
    workspace_id: WorkspaceId,
    agent_id: AgentId,
    now: Timestamp,
) -> Result<(), ChannelError> {
    let rows = sqlx::query(concat!(
        app_select!(),
        " WHERE a.organization_id = ? AND a.workspace_id = ? AND a.agent_id = ? AND a.status <> 'deleted'"
    ))
    .bind(organization_id.uuid())
    .bind(workspace_id.uuid())
    .bind(agent_id.uuid())
    .fetch_all(&mut **transaction)
    .await
    .map_err(failed("remove_agent_apps"))?;
    let apps = rows.iter().map(app_from).collect::<Result<Vec<_>, _>>()?;
    for app in &apps {
        remove_app(transaction, organization_id, workspace_id, app, now).await?;
    }
    Ok(())
}

/// The organization's settings, the defaults until saved.
async fn settings_in(
    connection: &mut SqliteConnection,
    organization_id: OrganizationId,
) -> Result<ChannelSettings, ChannelError> {
    let row = sqlx::query(
        "SELECT s.breaker_runs, s.breaker_window_minutes, s.breaker_pause_minutes, s.message_retention_days, \
         s.updated_at, u.id AS user_id, u.name AS user_name, u.email AS user_email, \
         u.avatar_url AS user_avatar_url \
         FROM channel_settings s LEFT JOIN users u ON u.id = s.updated_by_user_id WHERE s.organization_id = ?",
    )
    .bind(organization_id.uuid())
    .fetch_optional(connection)
    .await
    .map_err(failed("channel_settings"))?;
    let Some(row) = row else {
        return Ok(ChannelSettings {
            organization_id,
            values: BreakerSettings::default(),
            updated_at: None,
            updated_by: None,
        });
    };
    let number = |column: &str| -> Result<u32, ChannelError> { small(row.try_get(column).map_err(read)?) };
    Ok(ChannelSettings {
        organization_id,
        values: BreakerSettings {
            breaker_runs: number("breaker_runs")?,
            breaker_window_minutes: number("breaker_window_minutes")?,
            breaker_pause_minutes: number("breaker_pause_minutes")?,
            message_retention_days: number("message_retention_days")?,
        },
        updated_at: Some(at(&row, "updated_at")?),
        updated_by: user_of(&row)?,
    })
}

async fn levels_in(
    connection: &mut SqliteConnection,
    organization_id: OrganizationId,
) -> Result<Vec<ChannelToolLevel>, ChannelError> {
    let rows = sqlx::query(
        "SELECT l.provider, l.tool, l.level, l.updated_at, u.id AS user_id, u.name AS user_name, \
         u.email AS user_email, u.avatar_url AS user_avatar_url \
         FROM channel_tool_levels l LEFT JOIN users u ON u.id = l.updated_by_user_id WHERE l.organization_id = ?",
    )
    .bind(organization_id.uuid())
    .fetch_all(connection)
    .await
    .map_err(failed("tool_levels"))?;
    let mut set = Vec::with_capacity(rows.len());
    for row in &rows {
        set.push((
            parsed(row, "provider", ChannelProviderKind::parse)?,
            text(row, "tool")?,
            parsed(row, "level", ToolLevel::parse)?,
            at(row, "updated_at")?,
            user_of(row)?,
        ));
    }
    let mut levels = Vec::with_capacity(ChannelProviderKind::ALL.len() * CHANNEL_TOOLS.len());
    for provider in ChannelProviderKind::ALL {
        for (tool, effect, default) in CHANNEL_TOOLS {
            let found = set.iter().find(|(p, t, ..)| p == provider && t == tool);
            levels.push(ChannelToolLevel {
                provider: *provider,
                tool: (*tool).to_owned(),
                level: found.map_or(*default, |(_, _, level, ..)| *level),
                confirmed: found.is_some(),
                effect: *effect,
                updated_at: found.map(|(_, _, _, updated_at, _)| *updated_at),
                updated_by: found.and_then(|(.., user)| user.clone()),
            });
        }
    }
    Ok(levels)
}

/// The level in force for one tool: the one set, else Zupflow's default; an
/// unknown tool is denied.
async fn level_in_force(
    connection: &mut SqliteConnection,
    organization_id: OrganizationId,
    provider: ChannelProviderKind,
    tool: &str,
) -> Result<ToolLevel, ChannelError> {
    let set: Option<String> = sqlx::query_scalar(
        "SELECT level FROM channel_tool_levels WHERE organization_id = ? AND provider = ? AND tool = ?",
    )
    .bind(organization_id.uuid())
    .bind(provider.as_str())
    .bind(tool)
    .fetch_optional(connection)
    .await
    .map_err(failed("level_in_force"))?;
    match set {
        Some(level) => ToolLevel::parse(&level).ok_or_else(|| read("an unknown level")),
        None => Ok(zupflow_core::channels::channel_tool(tool).map_or(ToolLevel::Deny, |(_, level)| level)),
    }
}

/// Writes an agent channel's settings: a new row, or `id`'s.
#[allow(clippy::too_many_arguments)]
async fn write_agent_channel(
    connection: &mut SqliteConnection,
    organization_id: OrganizationId,
    workspace_id: WorkspaceId,
    existing: Option<AgentChannelId>,
    agent_id: AgentId,
    connection_id: ChannelConnectionId,
    settings: &AgentChannelSettings,
    now: Timestamp,
) -> Result<(), ChannelError> {
    // Stored as bare UUIDs, like every other column.
    let conversations: Vec<String> = settings
        .audience_conversations
        .iter()
        .map(|id| id.uuid().to_string())
        .collect();
    let persona = settings.persona.as_ref().map(to_json).transpose()?;
    let statement = if existing.is_some() {
        "UPDATE agent_channels SET tools = ?1, approval_required = ?2, audience = ?3, audience_conversations = ?4, \
         persona = ?5, answers_dms = ?6, answers_mentions = ?7, thread_follow_up = ?8, agent_messages = ?9, \
         max_agent_hops = ?10, mid_run = ?11, senders = ?12, sender_list = ?13, allow_automated = ?14, \
         allow_external = ?15, reply_with_output = ?16, updated_at = ?17 \
         WHERE organization_id = ?18 AND workspace_id = ?19 AND id = ?20 AND agent_id = ?21 AND connection_id = ?22"
    } else {
        "INSERT INTO agent_channels (tools, approval_required, audience, audience_conversations, persona, \
         answers_dms, answers_mentions, thread_follow_up, agent_messages, max_agent_hops, mid_run, senders, \
         sender_list, allow_automated, allow_external, reply_with_output, updated_at, organization_id, \
         workspace_id, id, agent_id, connection_id, created_at) \
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17, ?18, ?19, ?20, ?21, \
         ?22, ?17)"
    };
    let written = sqlx::query(statement)
        .bind(to_json(&settings.tools)?)
        .bind(to_json(&settings.approval_required)?)
        .bind(settings.audience.as_str())
        .bind(to_json(&conversations)?)
        .bind(persona)
        .bind(settings.answers_dms)
        .bind(settings.answers_mentions)
        .bind(settings.thread_follow_up.as_str())
        .bind(settings.agent_messages.as_str())
        .bind(i64::from(settings.max_agent_hops))
        .bind(settings.mid_run.as_str())
        .bind(settings.senders.as_str())
        .bind(to_json(&settings.sender_list)?)
        .bind(settings.allow_automated)
        .bind(settings.allow_external)
        .bind(settings.reply_with_output)
        .bind(now.millis())
        .bind(organization_id.uuid())
        .bind(workspace_id.uuid())
        .bind(existing.unwrap_or_else(AgentChannelId::generate).uuid())
        .bind(agent_id.uuid())
        .bind(connection_id.uuid())
        .execute(connection)
        .await
        .map_err(failed("write_agent_channel"))?;
    if written.rows_affected() == 1 {
        Ok(())
    } else {
        Err(ChannelError::AgentChannelNotFound)
    }
}

/// A conversation's ID, saved on first sight.
async fn conversation_id_for(
    connection: &mut SqliteConnection,
    organization_id: OrganizationId,
    workspace_id: WorkspaceId,
    connection_id: ChannelConnectionId,
    conversation: &zupflow_core::channels::store::NewConversation,
    now: Timestamp,
) -> Result<ChannelConversationId, ChannelError> {
    sqlx::query(
        "INSERT INTO channel_conversations (id, organization_id, workspace_id, connection_id, external_id, kind, \
         name, is_external, identity_is_member, created_at, updated_at) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?) \
         ON CONFLICT (organization_id, workspace_id, connection_id, external_id) DO UPDATE \
         SET is_external = 1, updated_at = excluded.updated_at \
         WHERE excluded.is_external AND NOT channel_conversations.is_external",
    )
    .bind(ChannelConversationId::generate().uuid())
    .bind(organization_id.uuid())
    .bind(workspace_id.uuid())
    .bind(connection_id.uuid())
    .bind(&conversation.external_id)
    .bind(conversation.kind.as_str())
    .bind(conversation.name.as_deref())
    .bind(conversation.is_external)
    .bind(conversation.identity_is_member)
    .bind(now.millis())
    .bind(now.millis())
    .execute(&mut *connection)
    .await
    .map_err(failed("record_conversation"))?;
    let id: Uuid = sqlx::query_scalar(
        "SELECT id FROM channel_conversations \
         WHERE organization_id = ? AND workspace_id = ? AND connection_id = ? AND external_id = ?",
    )
    .bind(organization_id.uuid())
    .bind(workspace_id.uuid())
    .bind(connection_id.uuid())
    .bind(&conversation.external_id)
    .fetch_one(connection)
    .await
    .map_err(failed("record_conversation"))?;
    Ok(ChannelConversationId::from_uuid(id))
}

/// A person's ID, saved on first sight.
async fn person_id_for(
    connection: &mut SqliteConnection,
    organization_id: OrganizationId,
    workspace_id: WorkspaceId,
    connection_id: ChannelConnectionId,
    person: &zupflow_core::channels::store::NewPerson,
    now: Timestamp,
) -> Result<Uuid, ChannelError> {
    sqlx::query(
        "INSERT INTO channel_people (id, organization_id, workspace_id, connection_id, external_id, handle, \
         display_name, email, is_automated, team_id, first_seen_at, updated_at) \
         VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?) \
         ON CONFLICT (organization_id, workspace_id, connection_id, external_id) DO UPDATE \
         SET team_id = excluded.team_id, updated_at = excluded.updated_at \
         WHERE excluded.team_id IS NOT NULL AND channel_people.team_id IS NOT excluded.team_id",
    )
    .bind(ChannelPersonId::generate().uuid())
    .bind(organization_id.uuid())
    .bind(workspace_id.uuid())
    .bind(connection_id.uuid())
    .bind(&person.external_id)
    .bind(person.handle.as_deref())
    .bind(person.display_name.as_deref())
    .bind(person.email.as_deref())
    .bind(person.is_automated)
    .bind(person.team_id.as_deref())
    .bind(now.millis())
    .bind(now.millis())
    .execute(&mut *connection)
    .await
    .map_err(failed("record_person"))?;
    sqlx::query_scalar(
        "SELECT id FROM channel_people \
         WHERE organization_id = ? AND workspace_id = ? AND connection_id = ? AND external_id = ?",
    )
    .bind(organization_id.uuid())
    .bind(workspace_id.uuid())
    .bind(connection_id.uuid())
    .bind(&person.external_id)
    .fetch_one(connection)
    .await
    .map_err(failed("record_person"))
}

async fn find_message_in(
    connection: &mut SqliteConnection,
    organization_id: OrganizationId,
    workspace_id: WorkspaceId,
    message_id: Uuid,
) -> Result<ChannelMessage, ChannelError> {
    let row = sqlx::query(FIND_MESSAGE)
        .bind(organization_id.uuid())
        .bind(workspace_id.uuid())
        .bind(message_id)
        .fetch_optional(connection)
        .await
        .map_err(failed("find_message"))?;
    message_from(&row.ok_or_else(|| read("a message gone inside its own transaction"))?)
}

/// Records a message with its conversation and author, each saved once,
/// inside the caller's transaction: [`ChannelStore::record_message`].
async fn record_message_in(
    transaction: &mut SqliteConnection,
    organization_id: OrganizationId,
    workspace_id: WorkspaceId,
    connection_id: ChannelConnectionId,
    message: &NewMessage,
    now: Timestamp,
) -> Result<RecordedMessage, ChannelError> {
    let inbound = message.direction == Direction::Inbound;
    if inbound != message.author.is_some() || (inbound && (message.run_id.is_some() || message.tool_call_id.is_some()))
    {
        return Err(read("a message whose direction doesn't match its author or run"));
    }
    if find_connection_in(&mut *transaction, organization_id, workspace_id, connection_id)
        .await?
        .is_none()
    {
        return Err(ChannelError::ConnectionNotFound);
    }
    let conversation_id = conversation_id_for(
        transaction,
        organization_id,
        workspace_id,
        connection_id,
        &message.conversation,
        now,
    )
    .await?;
    let seen: Option<Uuid> = sqlx::query_scalar(
        "SELECT id FROM channel_messages \
         WHERE organization_id = ? AND workspace_id = ? AND conversation_id = ? AND external_id = ?",
    )
    .bind(organization_id.uuid())
    .bind(workspace_id.uuid())
    .bind(conversation_id.uuid())
    .bind(&message.external_id)
    .fetch_optional(&mut *transaction)
    .await
    .map_err(failed("record_message"))?;
    if let Some(seen) = seen {
        if !inbound {
            // A send that meets its own echo, recorded first: the send
            // knows its run and its hops, and the higher hop holds, so a
            // late or repeated record never lowers it.
            sqlx::query(
                "UPDATE channel_messages SET agent_hop = max(agent_hop, ?), run_id = coalesce(run_id, ?), \
                 tool_call_id = coalesce(tool_call_id, ?) \
                 WHERE organization_id = ? AND workspace_id = ? AND conversation_id = ? AND id = ? \
                 AND direction = 'outbound'",
            )
            .bind(i64::from(message.agent_hop))
            .bind(message.run_id.map(|id| id.uuid()))
            .bind(message.tool_call_id.map(|id| id.uuid()))
            .bind(organization_id.uuid())
            .bind(workspace_id.uuid())
            .bind(conversation_id.uuid())
            .bind(seen)
            .execute(&mut *transaction)
            .await
            .map_err(failed("record_message"))?;
        }
        let message = find_message_in(transaction, organization_id, workspace_id, seen).await?;
        return Ok(RecordedMessage { message, new: false });
    }
    let author = match &message.author {
        Some(person) => {
            Some(person_id_for(transaction, organization_id, workspace_id, connection_id, person, now).await?)
        }
        None => None,
    };
    let retention = settings_in(&mut *transaction, organization_id)
        .await?
        .values
        .message_retention_days;
    let expires_at = now.millis().saturating_add(i64::from(retention).saturating_mul(DAY_MS));
    sqlx::query(
        "INSERT INTO channel_messages (id, organization_id, workspace_id, connection_id, conversation_id, \
         external_id, thread_key, direction, author_person_id, text, mentions, run_id, tool_call_id, agent_hop, \
         sent_at, created_at, expires_at) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
    )
    .bind(message.id.uuid())
    .bind(organization_id.uuid())
    .bind(workspace_id.uuid())
    .bind(connection_id.uuid())
    .bind(conversation_id.uuid())
    .bind(&message.external_id)
    .bind(&message.thread_key)
    .bind(message.direction.as_str())
    .bind(author)
    .bind(&message.text)
    .bind(to_json(&message.mentions)?)
    .bind(message.run_id.map(|id| id.uuid()))
    .bind(message.tool_call_id.map(|id| id.uuid()))
    .bind(i64::from(message.agent_hop))
    .bind(message.sent_at.millis())
    .bind(now.millis())
    .bind(expires_at)
    .execute(&mut *transaction)
    .await
    .map_err(failed("record_message"))?;
    sqlx::query(
        "UPDATE channel_conversations SET last_message_at = max(coalesce(last_message_at, ?1), ?1), \
         updated_at = ?2 WHERE organization_id = ?3 AND workspace_id = ?4 AND connection_id = ?5 AND id = ?6",
    )
    .bind(message.sent_at.millis())
    .bind(now.millis())
    .bind(organization_id.uuid())
    .bind(workspace_id.uuid())
    .bind(connection_id.uuid())
    .bind(conversation_id.uuid())
    .execute(&mut *transaction)
    .await
    .map_err(failed("record_message"))?;
    announce(
        transaction,
        organization_id,
        workspace_id,
        if inbound {
            WebhookEventType::ChannelMessageReceived
        } else {
            WebhookEventType::ChannelMessageSent
        },
        serde_json::json!({
            "object": "channel_message", "id": message.id, "connection_id": connection_id,
            "conversation_id": conversation_id,
        }),
        now,
    )
    .await?;
    let recorded = find_message_in(transaction, organization_id, workspace_id, message.id.uuid()).await?;
    Ok(RecordedMessage {
        message: recorded,
        new: true,
    })
}

/// The run store's error, as the channel store reports it.
fn from_run_store(error: StoreError) -> ChannelError {
    match error {
        StoreError::NotFound => ChannelError::ConversationNotFound,
        StoreError::AgentNotFound => ChannelError::AgentNotFound,
        StoreError::Unavailable => ChannelError::Unavailable,
        other => {
            tracing::error!(event = "channels.run_store_refused", error = %other);
            ChannelError::Unavailable
        }
    }
}

/// The organization's breaker settings, for the channel runs.
pub(crate) async fn breaker_settings(
    connection: &mut SqliteConnection,
    organization_id: OrganizationId,
) -> Result<BreakerSettings, StoreError> {
    settings_in(connection, organization_id)
        .await
        .map(|settings| settings.values)
        .map_err(|_| StoreError::Unavailable)
}

/// How a kept message changed at the provider.
enum Change<'a> {
    Edited { text: &'a str, at: Timestamp },
    Deleted { at: Timestamp },
}

/// Records an edit or a delete of a message the connection kept, found by
/// the provider's IDs within the connection: beside what it said, which is
/// never rewritten. A message it never kept, or one deleted already, is
/// left alone.
async fn changed(
    transaction: &mut SqliteConnection,
    (organization_id, workspace_id, connection_id): (OrganizationId, WorkspaceId, ChannelConnectionId),
    (conversation_external_id, external_id): (&str, &str),
    change: Change<'_>,
    now: Timestamp,
) -> Result<DeliveryOutcome, ChannelError> {
    let found: Option<Uuid> = sqlx::query_scalar(
        "SELECT g.id FROM channel_messages g JOIN channel_conversations c ON c.organization_id = g.organization_id \
         AND c.workspace_id = g.workspace_id AND c.connection_id = g.connection_id AND c.id = g.conversation_id \
         WHERE g.organization_id = ? AND g.workspace_id = ? AND g.connection_id = ? AND c.external_id = ? \
         AND g.external_id = ? AND g.deleted_at IS NULL",
    )
    .bind(organization_id.uuid())
    .bind(workspace_id.uuid())
    .bind(connection_id.uuid())
    .bind(conversation_external_id)
    .bind(external_id)
    .fetch_optional(&mut *transaction)
    .await
    .map_err(failed("record_delivery"))?;
    let Some(message) = found else {
        return Ok(DeliveryOutcome::Ignored);
    };
    let (edited_text, edited_at, deleted_at) = match change {
        // The provider's reader bounds the text, as it does a new message's.
        Change::Edited { text, at } => (Some(text), Some(at), None),
        Change::Deleted { at } => (None, None, Some(at)),
    };
    // An edit older than the one kept (deliveries can arrive out of
    // order) leaves the text as it is; either way, the change is noted as
    // learned now, on Zupflow's clock, which the session's turns compare.
    sqlx::query(
        "UPDATE channel_messages SET \
         edited_text = CASE WHEN ?1 IS NOT NULL AND (edited_at IS NULL OR ?2 >= edited_at) THEN ?1 \
         ELSE edited_text END, \
         edited_at = CASE WHEN ?2 IS NOT NULL AND (edited_at IS NULL OR ?2 >= edited_at) THEN ?2 \
         ELSE edited_at END, \
         deleted_at = coalesce(deleted_at, ?3), changed_at = ?4 \
         WHERE organization_id = ?5 AND workspace_id = ?6 AND connection_id = ?7 AND id = ?8",
    )
    .bind(edited_text)
    .bind(edited_at.map(Timestamp::millis))
    .bind(deleted_at.map(Timestamp::millis))
    .bind(now.millis())
    .bind(organization_id.uuid())
    .bind(workspace_id.uuid())
    .bind(connection_id.uuid())
    .bind(message)
    .execute(&mut *transaction)
    .await
    .map_err(failed("record_delivery"))?;
    Ok(DeliveryOutcome::Changed {
        connection_id,
        message_id: ChannelMessageId::from_uuid(message),
    })
}

/// A delivery recorded for the first time, through the app's live
/// connection: [`ChannelStore::record_delivery`].
async fn receive(
    transaction: &mut SqliteConnection,
    organization_id: OrganizationId,
    workspace_id: WorkspaceId,
    app_id: ChannelAppId,
    delivery: &NewDelivery,
    now: Timestamp,
) -> Result<DeliveryOutcome, ChannelError> {
    let Some(connection) = live_connection_of(&mut *transaction, organization_id, workspace_id, app_id).await? else {
        return Ok(DeliveryOutcome::Ignored);
    };
    // Another team's event, or one naming none, is never this connection's.
    if delivery.team_id.as_deref() != Some(connection.team.id.as_str()) {
        return Ok(DeliveryOutcome::Ignored);
    }
    sqlx::query(
        "UPDATE channel_connections SET last_event_at = ? WHERE organization_id = ? AND workspace_id = ? AND id = ?",
    )
    .bind(now.millis())
    .bind(organization_id.uuid())
    .bind(workspace_id.uuid())
    .bind(connection.id.uuid())
    .execute(&mut *transaction)
    .await
    .map_err(failed("record_delivery"))?;
    let message = match &delivery.event {
        InboundEvent::Uninstalled => {
            set_connection_status(
                transaction,
                organization_id,
                workspace_id,
                &connection,
                ConnectionStatus::ReconnectRequired,
                now,
            )
            .await?;
            return Ok(DeliveryOutcome::Uninstalled);
        }
        InboundEvent::Other => return Ok(DeliveryOutcome::Ignored),
        InboundEvent::Joined(joined) => {
            // Only the connection's own join: it's what a provider says when
            // the agent is added to a conversation it wasn't hearing.
            if connection.status != ConnectionStatus::Connected
                || joined.member_external_id != connection.identity.external_id
            {
                return Ok(DeliveryOutcome::Ignored);
            }
            let job = catch_up_job(app_id, connection.id, joined).ok_or(ChannelError::Unavailable)?;
            insert_job_at(
                &mut *transaction,
                &Scope::Workspace(organization_id, workspace_id),
                &job.kind,
                job.payload,
                job.options,
                now.millis(),
            )
            .await
            .map_err(|error| {
                tracing::error!(event = "channels.catch_up_job_not_queued", %error);
                ChannelError::Unavailable
            })?;
            return Ok(DeliveryOutcome::Joined {
                connection_id: connection.id,
            });
        }
        InboundEvent::Edited(edited) => {
            if connection.status != ConnectionStatus::Connected {
                return Ok(DeliveryOutcome::Ignored);
            }
            let change = Change::Edited {
                text: &edited.text,
                at: edited.edited_at,
            };
            return changed(
                transaction,
                (organization_id, workspace_id, connection.id),
                (&edited.conversation_external_id, &edited.external_id),
                change,
                now,
            )
            .await;
        }
        InboundEvent::Deleted(deleted) => {
            if connection.status != ConnectionStatus::Connected {
                return Ok(DeliveryOutcome::Ignored);
            }
            return changed(
                transaction,
                (organization_id, workspace_id, connection.id),
                (&deleted.conversation_external_id, &deleted.external_id),
                Change::Deleted { at: deleted.deleted_at },
                now,
            )
            .await;
        }
        InboundEvent::Message(message) => message,
    };
    if connection.status != ConnectionStatus::Connected {
        return Ok(DeliveryOutcome::Ignored);
    }
    // Its own post, known by its user or by its bot ID: either can come
    // alone (Slack leaves the user out of a post signed with a persona).
    let by_user = message.author_external_id.as_deref() == Some(connection.identity.external_id.as_str());
    let by_bot = message.author_bot_id.is_some() && message.author_bot_id == connection.identity.bot_id;
    let own = by_user || by_bot;
    if by_user
        && connection.identity.bot_id.is_none()
        && let Some(bot_id) = &message.author_bot_id
    {
        // Learned from the first post that carries both, so a later one
        // that carries the bot ID alone is still known as its own.
        sqlx::query(
            "UPDATE channel_connections SET identity_bot_id = ?, updated_at = ? \
             WHERE organization_id = ? AND workspace_id = ? AND id = ? AND identity_bot_id IS NULL",
        )
        .bind(bot_id)
        .bind(now.millis())
        .bind(organization_id.uuid())
        .bind(workspace_id.uuid())
        .bind(connection.id.uuid())
        .execute(&mut *transaction)
        .await
        .map_err(failed("record_delivery"))?;
    }
    let author = match (&message.author_external_id, own) {
        (_, true) => None,
        (Some(external_id), false) => Some(NewPerson {
            external_id: external_id.clone(),
            handle: None,
            display_name: None,
            email: None,
            is_automated: message.author_is_automated,
            team_id: message.author_team_id.clone(),
        }),
        // Nobody wrote it: nothing to record.
        (None, false) => return Ok(DeliveryOutcome::Ignored),
    };
    let new = NewMessage {
        id: ChannelMessageId::generate(),
        conversation: NewConversation {
            external_id: message.conversation_external_id.clone(),
            kind: message.conversation_kind,
            name: None,
            is_external: message.conversation_is_external,
            identity_is_member: true,
        },
        direction: if own { Direction::Outbound } else { Direction::Inbound },
        author,
        external_id: message.external_id.clone(),
        thread_key: message.thread_key.clone(),
        text: message.text.clone(),
        mentions: message.mentions.clone(),
        run_id: None,
        tool_call_id: None,
        agent_hop: 0,
        sent_at: message.sent_at,
    };
    let recorded = record_message_in(transaction, organization_id, workspace_id, connection.id, &new, now).await?;
    if recorded.new {
        let job = inbound_job(recorded.message.id).ok_or(ChannelError::Unavailable)?;
        insert_job_at(
            &mut *transaction,
            &Scope::Workspace(organization_id, workspace_id),
            &job.kind,
            job.payload,
            job.options,
            now.millis(),
        )
        .await
        .map_err(|error| {
            tracing::error!(event = "channels.inbound_job_not_queued", %error);
            ChannelError::Unavailable
        })?;
    }
    Ok(DeliveryOutcome::Message {
        connection_id: connection.id,
        message_id: recorded.message.id,
        new: recorded.new,
    })
}

/// [`ChannelStore::inbound_context`], on one read.
async fn inbound_context_in(
    transaction: &mut SqliteConnection,
    organization_id: OrganizationId,
    workspace_id: WorkspaceId,
    message_id: ChannelMessageId,
) -> Result<Option<InboundContext>, ChannelError> {
    let row = sqlx::query(
        "SELECT g.handled_at, p.external_id, p.email, p.team_id FROM channel_messages g LEFT JOIN channel_people p \
         ON p.organization_id = g.organization_id AND p.workspace_id = g.workspace_id \
         AND p.connection_id = g.connection_id AND p.id = g.author_person_id \
         WHERE g.organization_id = ? AND g.workspace_id = ? AND g.id = ?",
    )
    .bind(organization_id.uuid())
    .bind(workspace_id.uuid())
    .bind(message_id.uuid())
    .fetch_optional(&mut *transaction)
    .await
    .map_err(failed("inbound_context"))?;
    let Some(row) = row else {
        return Ok(None);
    };
    let handled = maybe_at(&row, "handled_at")?.is_some();
    let author_external_id: Option<String> = row.try_get("external_id").map_err(read)?;
    let author_email: Option<String> = row.try_get("email").map_err(read)?;
    let author_team_id: Option<String> = row.try_get("team_id").map_err(read)?;
    let message = find_message_in(&mut *transaction, organization_id, workspace_id, message_id.uuid()).await?;
    let connection = find_connection_in(&mut *transaction, organization_id, workspace_id, message.connection_id)
        .await?
        .ok_or_else(|| read("a message's connection gone inside a read"))?;
    let conversation = sqlx::query(FIND_CONVERSATION)
        .bind(organization_id.uuid())
        .bind(workspace_id.uuid())
        .bind(message.connection_id.uuid())
        .bind(message.conversation_id.uuid())
        .fetch_optional(&mut *transaction)
        .await
        .map_err(failed("inbound_context"))?
        .as_ref()
        .map(conversation_from)
        .transpose()?
        .ok_or_else(|| read("a message's conversation gone inside a read"))?;
    let channel = find_agent_channel_on(&mut *transaction, organization_id, workspace_id, connection.id).await?;
    let agent_active = match &channel {
        Some(channel) => {
            let status: Option<String> = sqlx::query_scalar(
                "SELECT status FROM agents WHERE organization_id = ? AND workspace_id = ? AND id = ?",
            )
            .bind(organization_id.uuid())
            .bind(workspace_id.uuid())
            .bind(channel.agent_id.uuid())
            .fetch_optional(&mut *transaction)
            .await
            .map_err(failed("inbound_context"))?;
            status.as_deref() == Some("active")
        }
        None => false,
    };
    // Another agent of the workspace in the same team: its own connection's
    // identity wrote the message (by its user or its bot ID), and its own
    // record of sending it says how many hops led to it. Until that send is
    // recorded, the hops are unknown, never none.
    let author_agent = match &author_external_id {
        Some(external_id) if *external_id != connection.identity.external_id => {
            let other = sqlx::query(
                "SELECT h.id AS agent_channel_id, c.id AS connection_id FROM channel_connections c \
                 JOIN agent_channels h ON h.organization_id = c.organization_id \
                 AND h.workspace_id = c.workspace_id AND h.connection_id = c.id \
                 WHERE c.organization_id = ? AND c.workspace_id = ? AND c.team_id = ? \
                 AND (c.identity_external_id = ? OR c.identity_bot_id = ?) AND c.status <> 'disconnected' \
                 AND c.id <> ?",
            )
            .bind(organization_id.uuid())
            .bind(workspace_id.uuid())
            .bind(&connection.team.id)
            .bind(external_id)
            .bind(external_id)
            .bind(connection.id.uuid())
            .fetch_optional(&mut *transaction)
            .await
            .map_err(failed("inbound_context"))?;
            match other {
                Some(other) => {
                    let hop: Option<i64> = sqlx::query_scalar(
                        "SELECT g.agent_hop FROM channel_messages g JOIN channel_conversations v \
                         ON v.organization_id = g.organization_id AND v.workspace_id = g.workspace_id \
                         AND v.id = g.conversation_id \
                         WHERE g.organization_id = ? AND g.workspace_id = ? AND g.connection_id = ? \
                         AND v.external_id = ? AND g.external_id = ? AND g.direction = 'outbound' \
                         AND g.run_id IS NOT NULL",
                    )
                    .bind(organization_id.uuid())
                    .bind(workspace_id.uuid())
                    .bind(uuid(&other, "connection_id")?)
                    .bind(&conversation.external_id)
                    .bind(&message.external_id)
                    .fetch_optional(&mut *transaction)
                    .await
                    .map_err(failed("inbound_context"))?;
                    Some((
                        AgentChannelId::from_uuid(uuid(&other, "agent_channel_id")?),
                        hop.map(small).transpose()?,
                    ))
                }
                None => None,
            }
        }
        _ => None,
    };
    let (_, thread_origin_key) = origin_keys(
        &connection.team.id,
        &conversation.external_id,
        &message.external_id,
        &message.thread_key,
    );
    let last_addressed: Option<Uuid> = sqlx::query_scalar(
        "SELECT agent_channel_id FROM channel_dispatches \
         WHERE organization_id = ? AND workspace_id = ? AND thread_origin_key = ? \
         AND reason IN ('direct_message', 'mentioned') ORDER BY created_at DESC, id DESC LIMIT 1",
    )
    .bind(organization_id.uuid())
    .bind(workspace_id.uuid())
    .bind(&thread_origin_key)
    .fetch_optional(&mut *transaction)
    .await
    .map_err(failed("inbound_context"))?;
    let in_thread = match &channel {
        Some(channel) => crate::channel_runs::find_session_in(
            &mut *transaction,
            organization_id,
            workspace_id,
            channel.id,
            conversation.id,
            &message.thread_key,
        )
        .await
        .map_err(from_run_store)?
        .map(|session| session.key),
        None => None,
    };
    Ok(Some(InboundContext {
        message,
        handled,
        connection,
        conversation,
        author_external_id,
        author_email,
        author_team_id,
        channel,
        agent_active,
        author_agent,
        last_addressed: last_addressed.map(AgentChannelId::from_uuid),
        in_thread,
    }))
}

#[async_trait]
impl ChannelStore for SqliteChannelStore {
    async fn list_managers(
        &self,
        organization_id: OrganizationId,
        workspace_id: WorkspaceId,
        cursor: Option<&str>,
        limit: u32,
    ) -> Result<Page<ChannelAppManager>, ChannelError> {
        let after = Position::decode(cursor, |id| ChannelAppManagerId::parse(id).ok().map(|id| id.uuid()))?;
        let limit = limit_of(limit);
        let total: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM channel_app_managers WHERE organization_id = ? AND workspace_id = ?",
        )
        .bind(organization_id.uuid())
        .bind(workspace_id.uuid())
        .fetch_one(self.database.pool())
        .await
        .map_err(failed("list_managers"))?;
        let mut query = scoped(
            concat!(manager_select!(), " WHERE m.organization_id = ? AND m.workspace_id = ?"),
            organization_id,
            workspace_id,
        )?;
        Position::push_after(&after, &mut query, "m.created_at", "m.id");
        query.push(" ORDER BY m.created_at DESC, m.id DESC LIMIT ");
        query.push_bind(i64::from(limit) + 1);
        let rows = query
            .build()
            .fetch_all(self.database.pool())
            .await
            .map_err(failed("list_managers"))?;
        let items = rows.iter().map(manager_from).collect::<Result<Vec<_>, _>>()?;
        page_of(items, limit, total, |m| Position::encode(m.created_at.millis(), m.id))
    }

    async fn find_manager(
        &self,
        organization_id: OrganizationId,
        workspace_id: WorkspaceId,
        manager_id: ChannelAppManagerId,
    ) -> Result<Option<ChannelAppManager>, ChannelError> {
        let mut connection = self.database.pool().acquire().await.map_err(failed("find_manager"))?;
        find_manager_in(&mut connection, organization_id, workspace_id, manager_id).await
    }

    async fn create_manager(
        &self,
        organization_id: OrganizationId,
        workspace_id: WorkspaceId,
        manager_id: ChannelAppManagerId,
        editor: ChannelEditor,
        manager: &NewManager,
        now: Timestamp,
        audit: &Audit<'_, ChannelAppManager>,
    ) -> Result<ChannelAppManager, ChannelError> {
        let mut transaction = self.immediate("create_manager").await?;
        editor_allowed(&mut transaction, organization_id, workspace_id, editor).await?;
        let created = insert_manager(
            &mut transaction,
            organization_id,
            workspace_id,
            manager_id,
            manager,
            now,
        )
        .await?;
        audit_in_workspace(&mut transaction, organization_id, workspace_id, &audit(&created)).await?;
        transaction.commit().await.map_err(failed("create_manager"))?;
        Ok(created)
    }

    async fn link_team_through_session(
        &self,
        organization_id: OrganizationId,
        workspace_id: WorkspaceId,
        session_id: ConnectSessionId,
        manager_id: ChannelAppManagerId,
        manager: &NewManager,
        now: Timestamp,
        audit: &Audit<'_, (ConnectSession, ChannelAppManager)>,
    ) -> Result<(ConnectSession, ChannelAppManager), ChannelError> {
        const FIND_SESSION: &str = concat!(
            "SELECT ",
            session_columns!(),
            " FROM connect_sessions WHERE organization_id = ? AND workspace_id = ? AND id = ?"
        );
        let mut transaction = self.immediate("link_team_through_session").await?;
        let row = sqlx::query(FIND_SESSION)
            .bind(organization_id.uuid())
            .bind(workspace_id.uuid())
            .bind(session_id.uuid())
            .fetch_optional(&mut *transaction)
            .await
            .map_err(failed("link_team_through_session"))?;
        let session = crate::connect::session_from(&row.ok_or(ChannelError::SessionNotFound)?)
            .map_err(|_| ChannelError::Unavailable)?;
        match session.target {
            Some(ConnectTarget::ChannelAppManager { provider, .. }) if provider == manager.provider => {}
            _ => return Err(ChannelError::SessionNotFound),
        }
        if session.status_at(now) != ConnectSessionStatus::Pending {
            return Err(ChannelError::SessionNotPending);
        }
        let linked = insert_manager(
            &mut transaction,
            organization_id,
            workspace_id,
            manager_id,
            manager,
            now,
        )
        .await?;
        let completed = sqlx::query(
            "UPDATE connect_sessions SET status = 'completed', completed_at = ?, target_manager_id = ? \
             WHERE organization_id = ? AND workspace_id = ? AND id = ? AND status = 'pending' \
             AND target_kind = 'channel_app_manager'",
        )
        .bind(now.millis())
        .bind(manager_id.uuid())
        .bind(organization_id.uuid())
        .bind(workspace_id.uuid())
        .bind(session_id.uuid())
        .execute(&mut *transaction)
        .await
        .map_err(failed("link_team_through_session"))?;
        if completed.rows_affected() != 1 {
            return Err(ChannelError::SessionNotPending);
        }
        let row = sqlx::query(FIND_SESSION)
            .bind(organization_id.uuid())
            .bind(workspace_id.uuid())
            .bind(session_id.uuid())
            .fetch_optional(&mut *transaction)
            .await
            .map_err(failed("link_team_through_session"))?;
        let session = crate::connect::session_from(&row.ok_or_else(|| read("a session gone in its transaction"))?)
            .map_err(|_| ChannelError::Unavailable)?;
        crate::connect::record_outcome(&mut transaction, organization_id, workspace_id, &session, now)
            .await
            .map_err(|_| ChannelError::Unavailable)?;
        let result = (session, linked);
        audit_in_workspace(&mut transaction, organization_id, workspace_id, &audit(&result)).await?;
        transaction
            .commit()
            .await
            .map_err(failed("link_team_through_session"))?;
        Ok(result)
    }

    async fn manager_credential(
        &self,
        organization_id: OrganizationId,
        workspace_id: WorkspaceId,
        manager_id: ChannelAppManagerId,
    ) -> Result<Option<SealedVersion>, ChannelError> {
        let row = sqlx::query(
            "SELECT k.version, k.key_id, k.wrapped_key, k.nonce, k.ciphertext \
             FROM channel_app_manager_credentials k JOIN channel_app_managers m \
             ON m.organization_id = k.organization_id AND m.workspace_id = k.workspace_id AND m.id = k.manager_id \
             AND m.credential_version = k.version \
             WHERE k.organization_id = ? AND m.organization_id = ? AND k.workspace_id = ? AND m.workspace_id = ? \
             AND k.manager_id = ?",
        )
        .bind(organization_id.uuid())
        .bind(organization_id.uuid())
        .bind(workspace_id.uuid())
        .bind(workspace_id.uuid())
        .bind(manager_id.uuid())
        .fetch_optional(self.database.pool())
        .await
        .map_err(failed("manager_credential"))?;
        row.as_ref().map(sealed_from).transpose()
    }

    async fn replace_manager_tokens(
        &self,
        organization_id: OrganizationId,
        workspace_id: WorkspaceId,
        manager_id: ChannelAppManagerId,
        editor: Option<ChannelEditor>,
        replacement: &TokenReplacement,
        now: Timestamp,
        audit: &Audit<'_, ChannelAppManager>,
    ) -> Result<ChannelAppManager, ChannelError> {
        let next = replacement
            .expected_version
            .checked_add(1)
            .ok_or(ChannelError::TokensChanged)?;
        let mut transaction = self.immediate("replace_manager_tokens").await?;
        if let Some(editor) = editor {
            editor_allowed(&mut transaction, organization_id, workspace_id, editor).await?;
        }
        let current = find_manager_in(&mut transaction, organization_id, workspace_id, manager_id)
            .await?
            .ok_or(ChannelError::ManagerNotFound)?;
        if current.credential_version != replacement.expected_version {
            return Err(ChannelError::TokensChanged);
        }
        insert_sealed(
            &mut transaction,
            INSERT_MANAGER_CREDENTIAL,
            organization_id,
            workspace_id,
            manager_id.uuid(),
            next,
            &replacement.credential,
            now,
        )
        .await?;
        // Only the current tokens are kept: a spent refresh token is never opened again.
        sqlx::query(
            "DELETE FROM channel_app_manager_credentials \
             WHERE organization_id = ? AND workspace_id = ? AND manager_id = ? AND version < ?",
        )
        .bind(organization_id.uuid())
        .bind(workspace_id.uuid())
        .bind(manager_id.uuid())
        .bind(i64::from(next))
        .execute(&mut *transaction)
        .await
        .map_err(failed("replace_manager_tokens"))?;
        let updated = sqlx::query(
            "UPDATE channel_app_managers SET token_last4 = ?, token_updated_at = ?, token_expires_at = ?, \
             credential_version = ?, status = 'active', updated_at = ? \
             WHERE organization_id = ? AND workspace_id = ? AND id = ? AND credential_version = ?",
        )
        .bind(last4(&replacement.token_last4)?)
        .bind(now.millis())
        .bind(replacement.token_expires_at.map(Timestamp::millis))
        .bind(i64::from(next))
        .bind(now.millis())
        .bind(organization_id.uuid())
        .bind(workspace_id.uuid())
        .bind(manager_id.uuid())
        .bind(i64::from(replacement.expected_version))
        .execute(&mut *transaction)
        .await
        .map_err(failed("replace_manager_tokens"))?;
        if updated.rows_affected() != 1 {
            return Err(ChannelError::TokensChanged);
        }
        if current.status != ManagerStatus::Active {
            announce(
                &mut transaction,
                organization_id,
                workspace_id,
                WebhookEventType::ChannelAppManagerStatusChanged,
                serde_json::json!({ "object": "channel_app_manager", "id": manager_id, "status": "active" }),
                now,
            )
            .await?;
        }
        let replaced = find_manager_in(&mut transaction, organization_id, workspace_id, manager_id)
            .await?
            .ok_or_else(|| read("a manager gone inside its own transaction"))?;
        if editor.is_some() {
            audit_in_workspace(&mut transaction, organization_id, workspace_id, &audit(&replaced)).await?;
            // Pasted tokens may be what a deleted app's removal was waiting for.
            lifecycle::requeue_releases(&mut transaction, organization_id, workspace_id, manager_id, now).await?;
        }
        transaction.commit().await.map_err(failed("replace_manager_tokens"))?;
        Ok(replaced)
    }

    async fn delete_manager(
        &self,
        organization_id: OrganizationId,
        workspace_id: WorkspaceId,
        manager_id: ChannelAppManagerId,
        editor: ChannelEditor,
        audit: &Audit<'_, ChannelAppManager>,
    ) -> Result<(), ChannelError> {
        let mut transaction = self.immediate("delete_manager").await?;
        editor_allowed(&mut transaction, organization_id, workspace_id, editor).await?;
        let manager = find_manager_in(&mut transaction, organization_id, workspace_id, manager_id)
            .await?
            .ok_or(ChannelError::ManagerNotFound)?;
        if manager.app_count > 0 {
            return Err(ChannelError::ManagerInUse(manager.app_count));
        }
        // Deleted apps not yet removed at the provider need its tokens.
        let unreleased: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM channel_apps WHERE organization_id = ? AND workspace_id = ? AND manager_id = ? \
             AND status = 'deleted' AND released_at IS NULL",
        )
        .bind(organization_id.uuid())
        .bind(workspace_id.uuid())
        .bind(manager_id.uuid())
        .fetch_one(&mut *transaction)
        .await
        .map_err(failed("delete_manager"))?;
        if unreleased > 0 {
            return Err(ChannelError::RemovalsPending(u64::try_from(unreleased).map_err(read)?));
        }
        sqlx::query("DELETE FROM channel_app_managers WHERE organization_id = ? AND workspace_id = ? AND id = ?")
            .bind(organization_id.uuid())
            .bind(workspace_id.uuid())
            .bind(manager_id.uuid())
            .execute(&mut *transaction)
            .await
            .map_err(failed("delete_manager"))?;
        audit_in_workspace(&mut transaction, organization_id, workspace_id, &audit(&manager)).await?;
        transaction.commit().await.map_err(failed("delete_manager"))?;
        Ok(())
    }

    async fn list_apps(
        &self,
        organization_id: OrganizationId,
        workspace_id: WorkspaceId,
        filter: &AppFilter,
        cursor: Option<&str>,
        limit: u32,
    ) -> Result<Page<ChannelApp>, ChannelError> {
        let after = Position::decode(cursor, |id| ChannelAppId::parse(id).ok().map(|id| id.uuid()))?;
        if filter.matches_nothing {
            return Ok(empty());
        }
        let limit = limit_of(limit);
        let conditions = |query: &mut QueryBuilder<Sqlite>| {
            query.push(" AND a.status <> 'deleted'");
            if let Some(agent_id) = filter.agent_id {
                query.push(" AND a.agent_id = ");
                query.push_bind(agent_id.uuid());
            }
            if let Some(manager_id) = filter.manager_id {
                query.push(" AND a.manager_id = ");
                query.push_bind(manager_id.uuid());
            }
        };
        let mut count = scoped(
            "SELECT count(*) FROM channel_apps a WHERE a.organization_id = ? AND a.workspace_id = ?",
            organization_id,
            workspace_id,
        )?;
        conditions(&mut count);
        let total: i64 = count
            .build_query_scalar()
            .fetch_one(self.database.pool())
            .await
            .map_err(failed("list_apps"))?;
        let mut query = scoped(
            concat!(app_select!(), " WHERE a.organization_id = ? AND a.workspace_id = ?"),
            organization_id,
            workspace_id,
        )?;
        conditions(&mut query);
        Position::push_after(&after, &mut query, "a.created_at", "a.id");
        query.push(" ORDER BY a.created_at DESC, a.id DESC LIMIT ");
        query.push_bind(i64::from(limit) + 1);
        let rows = query
            .build()
            .fetch_all(self.database.pool())
            .await
            .map_err(failed("list_apps"))?;
        let items = rows.iter().map(app_from).collect::<Result<Vec<_>, _>>()?;
        page_of(items, limit, total, |a| Position::encode(a.created_at.millis(), a.id))
    }

    async fn find_app(
        &self,
        organization_id: OrganizationId,
        workspace_id: WorkspaceId,
        app_id: ChannelAppId,
    ) -> Result<Option<ChannelApp>, ChannelError> {
        let mut connection = self.database.pool().acquire().await.map_err(failed("find_app"))?;
        find_app_in(&mut connection, organization_id, workspace_id, app_id).await
    }

    async fn create_app(
        &self,
        organization_id: OrganizationId,
        workspace_id: WorkspaceId,
        app_id: ChannelAppId,
        editor: ChannelEditor,
        app: &NewApp,
        job: &QueuedJob,
        now: Timestamp,
        audit: &Audit<'_, ChannelApp>,
    ) -> Result<ChannelApp, ChannelError> {
        let mut transaction = self.immediate("create_app").await?;
        editor_allowed(&mut transaction, organization_id, workspace_id, editor).await?;
        if !agent_exists(&mut transaction, organization_id, workspace_id, app.agent_id).await? {
            return Err(ChannelError::AgentNotFound);
        }
        let manager = find_manager_in(&mut transaction, organization_id, workspace_id, app.manager_id)
            .await?
            .ok_or(ChannelError::ManagerNotFound)?;
        if manager.status != ManagerStatus::Active {
            return Err(ChannelError::ManagerInactive);
        }
        let live = sqlx::query(
            "SELECT 1 FROM channel_apps WHERE organization_id = ? AND workspace_id = ? AND manager_id = ? \
             AND agent_id = ? AND status NOT IN ('deleted', 'failed')",
        )
        .bind(organization_id.uuid())
        .bind(workspace_id.uuid())
        .bind(app.manager_id.uuid())
        .bind(app.agent_id.uuid())
        .fetch_optional(&mut *transaction)
        .await
        .map_err(failed("create_app"))?;
        if live.is_some() {
            return Err(ChannelError::AppExists);
        }
        let job_id = crate::job_queue::insert_job_at(
            &mut *transaction,
            &Scope::Workspace(organization_id, workspace_id),
            &job.kind,
            job.payload.clone(),
            job.options,
            now.millis(),
        )
        .await
        .map_err(|error| {
            tracing::error!(event = "channels.job_not_queued", %error);
            ChannelError::Unavailable
        })?;
        sqlx::query(
            "INSERT INTO channel_apps (id, organization_id, workspace_id, manager_id, agent_id, provider, inbound_ref, \
             status, provision_job_id, created_at, updated_at) VALUES (?, ?, ?, ?, ?, ?, ?, 'creating', ?, ?, ?)",
        )
        .bind(app_id.uuid())
        .bind(organization_id.uuid())
        .bind(workspace_id.uuid())
        .bind(app.manager_id.uuid())
        .bind(app.agent_id.uuid())
        .bind(manager.provider.as_str())
        .bind(app.inbound_ref.expose())
        .bind(job_id.uuid())
        .bind(now.millis())
        .bind(now.millis())
        .execute(&mut *transaction)
        .await
        .map_err(failed("create_app"))?;
        let created = find_app_in(&mut transaction, organization_id, workspace_id, app_id)
            .await?
            .ok_or_else(|| read("an app gone inside its own transaction"))?;
        audit_in_workspace(&mut transaction, organization_id, workspace_id, &audit(&created)).await?;
        transaction.commit().await.map_err(failed("create_app"))?;
        Ok(created)
    }

    async fn record_app_made(
        &self,
        organization_id: OrganizationId,
        workspace_id: WorkspaceId,
        app_id: ChannelAppId,
        made: &MadeApp,
        now: Timestamp,
    ) -> Result<ChannelApp, ChannelError> {
        let mut transaction = self.immediate("record_app_made").await?;
        let app = find_app_in(&mut transaction, organization_id, workspace_id, app_id)
            .await?
            .ok_or(ChannelError::AppNotFound)?;
        if app.status != AppStatus::Creating {
            return Err(ChannelError::AppNotReady);
        }
        // Made by the attempt that recorded its ID, or by one that hadn't yet.
        let recorded = sqlx::query(
            "UPDATE channel_apps SET external_app_id = ?1, client_id = ?2, creation_started_at = NULL \
             WHERE organization_id = ?3 AND workspace_id = ?4 AND id = ?5 AND status = 'creating' \
             AND (external_app_id IS NULL OR external_app_id = ?1)",
        )
        .bind(&made.external_app_id)
        .bind(&made.client_id)
        .bind(organization_id.uuid())
        .bind(workspace_id.uuid())
        .bind(app_id.uuid())
        .execute(&mut *transaction)
        .await
        .map_err(failed("record_app_made"))?;
        if recorded.rows_affected() != 1 {
            return Err(ChannelError::AppNotReady);
        }
        insert_sealed(
            &mut transaction,
            INSERT_APP_SECRET,
            organization_id,
            workspace_id,
            app_id.uuid(),
            FIRST_VERSION,
            &made.secrets,
            now,
        )
        .await?;
        set_app_status(
            &mut transaction,
            organization_id,
            workspace_id,
            &app,
            AppStatus::AwaitingInstall,
            now,
        )
        .await?;
        let made = find_app_in(&mut transaction, organization_id, workspace_id, app_id)
            .await?
            .ok_or_else(|| read("an app gone inside its own transaction"))?;
        transaction.commit().await.map_err(failed("record_app_made"))?;
        Ok(made)
    }

    async fn app_creation(
        &self,
        organization_id: OrganizationId,
        workspace_id: WorkspaceId,
        app_id: ChannelAppId,
    ) -> Result<Option<AppCreation>, ChannelError> {
        let row = sqlx::query(FIND_APP_CREATION)
            .bind(organization_id.uuid())
            .bind(workspace_id.uuid())
            .bind(app_id.uuid())
            .fetch_optional(self.database.pool())
            .await
            .map_err(failed("app_creation"))?;
        let Some(row) = row else { return Ok(None) };
        Ok(Some(AppCreation {
            app: app_from(&row)?,
            inbound_ref: InboundRef::parse(&text(&row, "inbound_ref")?)
                .ok_or_else(|| read("an invalid inbound reference"))?,
            in_flight: maybe_at(&row, "creation_started_at")?.is_some(),
        }))
    }

    async fn begin_app_creation(
        &self,
        organization_id: OrganizationId,
        workspace_id: WorkspaceId,
        app_id: ChannelAppId,
        now: Timestamp,
    ) -> Result<(), ChannelError> {
        let begun = sqlx::query(
            "UPDATE channel_apps SET creation_started_at = ? WHERE organization_id = ? AND workspace_id = ? \
             AND id = ? AND status = 'creating' AND external_app_id IS NULL",
        )
        .bind(now.millis())
        .bind(organization_id.uuid())
        .bind(workspace_id.uuid())
        .bind(app_id.uuid())
        .execute(self.database.pool())
        .await
        .map_err(failed("begin_app_creation"))?;
        if begun.rows_affected() == 1 {
            Ok(())
        } else {
            Err(ChannelError::AppNotReady)
        }
    }

    async fn record_app_created(
        &self,
        organization_id: OrganizationId,
        workspace_id: WorkspaceId,
        app_id: ChannelAppId,
        external_app_id: &str,
        _now: Timestamp,
    ) -> Result<(), ChannelError> {
        let recorded = sqlx::query(
            "UPDATE channel_apps SET external_app_id = ? WHERE organization_id = ? AND workspace_id = ? AND id = ? \
             AND status = 'creating' AND creation_started_at IS NOT NULL AND external_app_id IS NULL",
        )
        .bind(external_app_id)
        .bind(organization_id.uuid())
        .bind(workspace_id.uuid())
        .bind(app_id.uuid())
        .execute(self.database.pool())
        .await
        .map_err(failed("record_app_created"))?;
        if recorded.rows_affected() == 1 {
            Ok(())
        } else {
            Err(ChannelError::AppNotReady)
        }
    }

    async fn forget_app_creation(
        &self,
        organization_id: OrganizationId,
        workspace_id: WorkspaceId,
        app_id: ChannelAppId,
        external_app_id: Option<&str>,
        _now: Timestamp,
    ) -> Result<(), ChannelError> {
        // Only what was deleted: an ID recorded since, by another attempt, stays.
        sqlx::query(
            "UPDATE channel_apps SET external_app_id = NULL, creation_started_at = NULL \
             WHERE organization_id = ? AND workspace_id = ? AND id = ? AND creation_started_at IS NOT NULL \
             AND (external_app_id IS NULL OR external_app_id = ?)",
        )
        .bind(organization_id.uuid())
        .bind(workspace_id.uuid())
        .bind(app_id.uuid())
        .bind(external_app_id)
        .execute(self.database.pool())
        .await
        .map_err(failed("forget_app_creation"))?;
        Ok(())
    }

    async fn reap_stranded_apps_cross_tenant(
        &self,
        cleanup: &(dyn Fn(ChannelAppId) -> QueuedJob + Send + Sync),
        limit: u32,
        now: Timestamp,
    ) -> Result<u32, ChannelError> {
        let mut transaction = self.immediate("reap_stranded_apps").await?;
        // Cross-tenant by design: a sweep over every workspace's unsettled
        // apps, as a worker's claim is. Each is then settled in its own scope.
        let stranded: Vec<(Uuid, Uuid, Uuid, bool)> = sqlx::query_as(
            "SELECT a.organization_id, a.workspace_id, a.id, a.creation_started_at IS NOT NULL FROM channel_apps a \
             WHERE (a.status = 'creating' OR a.creation_started_at IS NOT NULL) AND NOT EXISTS (SELECT 1 FROM jobs j \
             WHERE j.organization_id = a.organization_id AND j.workspace_id = a.workspace_id \
             AND j.id = a.provision_job_id AND j.state <> 'dead') LIMIT ?",
        )
        .bind(i64::from(limit))
        .fetch_all(&mut *transaction)
        .await
        .map_err(failed("reap_stranded_apps"))?;
        let mut settled = 0;
        for (organization, workspace, app, in_flight) in stranded {
            let (organization_id, workspace_id) = (
                OrganizationId::from_uuid(organization),
                WorkspaceId::from_uuid(workspace),
            );
            let app_id = ChannelAppId::from_uuid(app);
            let row = sqlx::query(FIND_APP_CREATION)
                .bind(organization_id.uuid())
                .bind(workspace_id.uuid())
                .bind(app_id.uuid())
                .fetch_one(&mut *transaction)
                .await
                .map_err(failed("reap_stranded_apps"))?;
            let found = app_from(&row)?;
            if found.status == AppStatus::Creating {
                tracing::warn!(event = "channels.app_abandoned", %organization_id, %workspace_id, %app_id);
                fail_app(&mut transaction, organization_id, workspace_id, &found, ABANDONED, now).await?;
            }
            if in_flight {
                let job = cleanup(app_id);
                let job_id = crate::job_queue::insert_job_at(
                    &mut *transaction,
                    &Scope::Workspace(organization_id, workspace_id),
                    &job.kind,
                    job.payload,
                    job.options,
                    now.millis(),
                )
                .await
                .map_err(|error| {
                    tracing::error!(event = "channels.job_not_queued", %error);
                    ChannelError::Unavailable
                })?;
                sqlx::query(
                    "UPDATE channel_apps SET provision_job_id = ? WHERE organization_id = ? AND workspace_id = ? \
                     AND id = ?",
                )
                .bind(job_id.uuid())
                .bind(organization_id.uuid())
                .bind(workspace_id.uuid())
                .bind(app_id.uuid())
                .execute(&mut *transaction)
                .await
                .map_err(failed("reap_stranded_apps"))?;
            }
            settled += 1;
        }
        transaction.commit().await.map_err(failed("reap_stranded_apps"))?;
        Ok(settled)
    }

    async fn record_app_failed(
        &self,
        organization_id: OrganizationId,
        workspace_id: WorkspaceId,
        app_id: ChannelAppId,
        code: &str,
        now: Timestamp,
    ) -> Result<(), ChannelError> {
        let mut transaction = self.immediate("record_app_failed").await?;
        let app = find_app_in(&mut transaction, organization_id, workspace_id, app_id)
            .await?
            .ok_or(ChannelError::AppNotFound)?;
        if app.status != AppStatus::Creating {
            return Err(ChannelError::AppNotReady);
        }
        fail_app(&mut transaction, organization_id, workspace_id, &app, code, now).await?;
        transaction.commit().await.map_err(failed("record_app_failed"))?;
        Ok(())
    }

    async fn app_secrets(
        &self,
        organization_id: OrganizationId,
        workspace_id: WorkspaceId,
        app_id: ChannelAppId,
    ) -> Result<Option<AppSecretsRecord>, ChannelError> {
        let row = sqlx::query(
            "SELECT a.provider, a.inbound_ref, a.client_id, a.manifest_digest, s.version, s.key_id, s.wrapped_key, s.nonce, \
             s.ciphertext \
             FROM channel_apps a LEFT JOIN channel_app_secrets s \
             ON s.organization_id = a.organization_id AND s.workspace_id = a.workspace_id AND s.app_id = a.id \
             WHERE a.organization_id = ? AND a.workspace_id = ? AND a.id = ? AND a.status <> 'deleted' \
             ORDER BY s.version DESC LIMIT 1",
        )
        .bind(organization_id.uuid())
        .bind(workspace_id.uuid())
        .bind(app_id.uuid())
        .fetch_optional(self.database.pool())
        .await
        .map_err(failed("app_secrets"))?;
        let Some(row) = row else { return Ok(None) };
        let version: Option<i64> = row.try_get("version").map_err(read)?;
        Ok(Some(AppSecretsRecord {
            provider: parsed(&row, "provider", ChannelProviderKind::parse)?,
            inbound_ref: InboundRef::parse(&text(&row, "inbound_ref")?)
                .ok_or_else(|| read("an invalid inbound reference"))?,
            client_id: row.try_get("client_id").map_err(read)?,
            secrets: version.map(|_| sealed_from(&row)).transpose()?,
            manifest_digest: row.try_get("manifest_digest").map_err(read)?,
        }))
    }

    async fn attribute_app_by_inbound_ref_cross_tenant(
        &self,
        inbound_ref: &InboundRef,
    ) -> Result<Option<(OrganizationId, WorkspaceId, ChannelAppId)>, ChannelError> {
        // Cross-tenant by design: the reference is unique across the deployment,
        // and is how a provider's event finds its organization and workspace.
        let row: Option<(Uuid, Uuid, Uuid)> = sqlx::query_as(
            "SELECT organization_id, workspace_id, id FROM channel_apps WHERE inbound_ref = ? AND status <> 'deleted'",
        )
        .bind(inbound_ref.expose())
        .fetch_optional(self.database.pool())
        .await
        .map_err(failed("attribute_app_by_inbound_ref_cross_tenant"))?;
        Ok(row.map(|(organization, workspace, app)| {
            (
                OrganizationId::from_uuid(organization),
                WorkspaceId::from_uuid(workspace),
                ChannelAppId::from_uuid(app),
            )
        }))
    }

    async fn delete_app(
        &self,
        organization_id: OrganizationId,
        workspace_id: WorkspaceId,
        app_id: ChannelAppId,
        editor: ChannelEditor,
        now: Timestamp,
        audit: &Audit<'_, ChannelApp>,
    ) -> Result<(), ChannelError> {
        let mut transaction = self.immediate("delete_app").await?;
        editor_allowed(&mut transaction, organization_id, workspace_id, editor).await?;
        let app = find_app_in(&mut transaction, organization_id, workspace_id, app_id)
            .await?
            .ok_or(ChannelError::AppNotFound)?;
        remove_app(&mut transaction, organization_id, workspace_id, &app, now).await?;
        audit_in_workspace(&mut transaction, organization_id, workspace_id, &audit(&app)).await?;
        transaction.commit().await.map_err(failed("delete_app"))?;
        Ok(())
    }

    async fn install_app(
        &self,
        organization_id: OrganizationId,
        workspace_id: WorkspaceId,
        app_id: ChannelAppId,
        install: &NewInstall,
        now: Timestamp,
    ) -> Result<(ChannelConnection, AgentChannel), ChannelError> {
        let mut transaction = self.immediate("install_app").await?;
        let installed = install_app_in(&mut transaction, organization_id, workspace_id, app_id, install, now).await?;
        transaction.commit().await.map_err(failed("install_app"))?;
        Ok(installed)
    }

    async fn install_app_through_session(
        &self,
        organization_id: OrganizationId,
        workspace_id: WorkspaceId,
        session_id: ConnectSessionId,
        app_id: ChannelAppId,
        install: &NewInstall,
        now: Timestamp,
    ) -> Result<(ConnectSession, ChannelConnection, AgentChannel), ChannelError> {
        lifecycle::install_app_through_session(self, organization_id, workspace_id, session_id, app_id, install, now)
            .await
    }

    async fn connection_credential(
        &self,
        organization_id: OrganizationId,
        workspace_id: WorkspaceId,
        connection_id: ChannelConnectionId,
    ) -> Result<Option<SealedVersion>, ChannelError> {
        lifecycle::connection_credential(self, organization_id, workspace_id, connection_id).await
    }

    async fn queue_due_rotations_cross_tenant(
        &self,
        rotate: &(dyn Fn(ChannelAppManagerId, u32) -> QueuedJob + Send + Sync),
        due_by: Timestamp,
        limit: u32,
        now: Timestamp,
    ) -> Result<RotationSweep, ChannelError> {
        lifecycle::queue_due_rotations_cross_tenant(self, rotate, due_by, limit, now).await
    }

    async fn count_teams_by_provider_cross_tenant(&self) -> Result<Vec<(ChannelProviderKind, u64)>, ChannelError> {
        lifecycle::count_teams_by_provider_cross_tenant(self).await
    }

    async fn record_app_released(
        &self,
        organization_id: OrganizationId,
        workspace_id: WorkspaceId,
        app_id: ChannelAppId,
        now: Timestamp,
    ) -> Result<bool, ChannelError> {
        lifecycle::record_app_released(self, organization_id, workspace_id, app_id, now).await
    }

    async fn record_app_synced(
        &self,
        organization_id: OrganizationId,
        workspace_id: WorkspaceId,
        app_id: ChannelAppId,
        digest: &str,
    ) -> Result<bool, ChannelError> {
        lifecycle::record_app_synced(self, organization_id, workspace_id, app_id, digest).await
    }

    async fn expire_manager_tokens(
        &self,
        organization_id: OrganizationId,
        workspace_id: WorkspaceId,
        manager_id: ChannelAppManagerId,
        expected_version: u32,
        now: Timestamp,
    ) -> Result<bool, ChannelError> {
        lifecycle::expire_manager_tokens(self, organization_id, workspace_id, manager_id, expected_version, now).await
    }

    async fn list_connections(
        &self,
        organization_id: OrganizationId,
        workspace_id: WorkspaceId,
        filter: &ConnectionFilter,
        cursor: Option<&str>,
        limit: u32,
    ) -> Result<Page<ChannelConnection>, ChannelError> {
        let after = Position::decode(cursor, |id| ChannelConnectionId::parse(id).ok().map(|id| id.uuid()))?;
        if filter.matches_nothing {
            return Ok(empty());
        }
        let limit = limit_of(limit);
        let conditions = |query: &mut QueryBuilder<Sqlite>| {
            if let Some(agent_id) = filter.agent_id {
                query.push(" AND a.agent_id = ");
                query.push_bind(agent_id.uuid());
            }
        };
        let mut count = scoped(
            "SELECT count(*) FROM channel_connections c LEFT JOIN channel_apps a \
             ON a.organization_id = c.organization_id AND a.workspace_id = c.workspace_id AND a.id = c.app_id \
             WHERE c.organization_id = ? AND c.workspace_id = ?",
            organization_id,
            workspace_id,
        )?;
        conditions(&mut count);
        let total: i64 = count
            .build_query_scalar()
            .fetch_one(self.database.pool())
            .await
            .map_err(failed("list_connections"))?;
        let mut query = scoped(
            concat!(
                connection_select!(),
                " WHERE c.organization_id = ? AND c.workspace_id = ?"
            ),
            organization_id,
            workspace_id,
        )?;
        conditions(&mut query);
        Position::push_after(&after, &mut query, "c.created_at", "c.id");
        query.push(" ORDER BY c.created_at DESC, c.id DESC LIMIT ");
        query.push_bind(i64::from(limit) + 1);
        let rows = query
            .build()
            .fetch_all(self.database.pool())
            .await
            .map_err(failed("list_connections"))?;
        let items = rows.iter().map(connection_from).collect::<Result<Vec<_>, _>>()?;
        page_of(items, limit, total, |c| Position::encode(c.created_at.millis(), c.id))
    }

    async fn find_connection(
        &self,
        organization_id: OrganizationId,
        workspace_id: WorkspaceId,
        connection_id: ChannelConnectionId,
    ) -> Result<Option<ChannelConnection>, ChannelError> {
        let mut connection = self
            .database
            .pool()
            .acquire()
            .await
            .map_err(failed("find_connection"))?;
        find_connection_in(&mut connection, organization_id, workspace_id, connection_id).await
    }

    async fn update_connection(
        &self,
        organization_id: OrganizationId,
        workspace_id: WorkspaceId,
        connection_id: ChannelConnectionId,
        editor: ChannelEditor,
        changes: &ConnectionChanges,
        now: Timestamp,
        audit: &Audit<'_, ChannelConnection>,
    ) -> Result<ChannelConnection, ChannelError> {
        let mut transaction = self.immediate("update_connection").await?;
        editor_allowed(&mut transaction, organization_id, workspace_id, editor).await?;
        let updated = sqlx::query(
            "UPDATE channel_connections SET label = coalesce(?, label), handler = coalesce(?, handler), \
             updated_at = ? WHERE organization_id = ? AND workspace_id = ? AND id = ?",
        )
        .bind(changes.label.as_deref())
        .bind(changes.handler.map(HandlerKind::as_str))
        .bind(now.millis())
        .bind(organization_id.uuid())
        .bind(workspace_id.uuid())
        .bind(connection_id.uuid())
        .execute(&mut *transaction)
        .await
        .map_err(failed("update_connection"))?;
        if updated.rows_affected() == 0 {
            return Err(ChannelError::ConnectionNotFound);
        }
        let connection = find_connection_in(&mut transaction, organization_id, workspace_id, connection_id)
            .await?
            .ok_or(ChannelError::ConnectionNotFound)?;
        audit_in_workspace(&mut transaction, organization_id, workspace_id, &audit(&connection)).await?;
        transaction.commit().await.map_err(failed("update_connection"))?;
        Ok(connection)
    }

    async fn delete_connection(
        &self,
        organization_id: OrganizationId,
        workspace_id: WorkspaceId,
        connection_id: ChannelConnectionId,
        editor: ChannelEditor,
        now: Timestamp,
        audit: &Audit<'_, ChannelConnection>,
    ) -> Result<(), ChannelError> {
        let mut transaction = self.immediate("delete_connection").await?;
        editor_allowed(&mut transaction, organization_id, workspace_id, editor).await?;
        let connection = find_connection_in(&mut transaction, organization_id, workspace_id, connection_id)
            .await?
            .ok_or(ChannelError::ConnectionNotFound)?;
        set_connection_status(
            &mut transaction,
            organization_id,
            workspace_id,
            &connection,
            ConnectionStatus::Disconnected,
            now,
        )
        .await?;
        if let Some(app_id) = connection.app_id
            && let Some(app) = find_app_in(&mut transaction, organization_id, workspace_id, app_id).await?
            && app.status == AppStatus::Installed
        {
            set_app_status(
                &mut transaction,
                organization_id,
                workspace_id,
                &app,
                AppStatus::AwaitingInstall,
                now,
            )
            .await?;
        }
        // Messages first, since they name their authors; everything else the
        // connection holds goes with it by its cascading keys.
        for statement in [
            "DELETE FROM channel_messages WHERE organization_id = ? AND workspace_id = ? AND connection_id = ?",
            "DELETE FROM channel_connections WHERE organization_id = ? AND workspace_id = ? AND id = ?",
        ] {
            sqlx::query(statement)
                .bind(organization_id.uuid())
                .bind(workspace_id.uuid())
                .bind(connection_id.uuid())
                .execute(&mut *transaction)
                .await
                .map_err(failed("delete_connection"))?;
        }
        audit_in_workspace(&mut transaction, organization_id, workspace_id, &audit(&connection)).await?;
        transaction.commit().await.map_err(failed("delete_connection"))?;
        Ok(())
    }

    async fn record_message(
        &self,
        organization_id: OrganizationId,
        workspace_id: WorkspaceId,
        connection_id: ChannelConnectionId,
        message: &NewMessage,
        now: Timestamp,
    ) -> Result<RecordedMessage, ChannelError> {
        let mut transaction = self.immediate("record_message").await?;
        let recorded = record_message_in(
            &mut transaction,
            organization_id,
            workspace_id,
            connection_id,
            message,
            now,
        )
        .await?;
        transaction.commit().await.map_err(failed("record_message"))?;
        Ok(recorded)
    }

    async fn record_inbound_event(
        &self,
        organization_id: OrganizationId,
        workspace_id: WorkspaceId,
        app_id: ChannelAppId,
        provider_event_id: &str,
        kind: &str,
        now: Timestamp,
    ) -> Result<bool, ChannelError> {
        let mut transaction = self.immediate("record_inbound_event").await?;
        let app = sqlx::query("SELECT 1 FROM channel_apps WHERE organization_id = ? AND workspace_id = ? AND id = ?")
            .bind(organization_id.uuid())
            .bind(workspace_id.uuid())
            .bind(app_id.uuid())
            .fetch_optional(&mut *transaction)
            .await
            .map_err(failed("record_inbound_event"))?;
        if app.is_none() {
            return Err(ChannelError::AppNotFound);
        }
        let recorded = sqlx::query(
            "INSERT INTO channel_inbound_events (id, organization_id, workspace_id, app_id, provider_event_id, kind, \
             received_at) VALUES (?, ?, ?, ?, ?, ?, ?) \
             ON CONFLICT (organization_id, workspace_id, app_id, provider_event_id) DO NOTHING",
        )
        .bind(Uuid::now_v7())
        .bind(organization_id.uuid())
        .bind(workspace_id.uuid())
        .bind(app_id.uuid())
        .bind(provider_event_id)
        .bind(kind)
        .bind(now.millis())
        .execute(&mut *transaction)
        .await
        .map_err(failed("record_inbound_event"))?;
        transaction.commit().await.map_err(failed("record_inbound_event"))?;
        Ok(recorded.rows_affected() == 1)
    }

    async fn list_conversations(
        &self,
        organization_id: OrganizationId,
        workspace_id: WorkspaceId,
        connection_id: ChannelConnectionId,
        filter: &ConversationFilter,
        cursor: Option<&str>,
        limit: u32,
    ) -> Result<Page<ChannelConversation>, ChannelError> {
        let after = Position::decode(cursor, |id| ChannelConversationId::parse(id).ok().map(|id| id.uuid()))?;
        let limit = limit_of(limit);
        let q = filter
            .q
            .as_deref()
            .map(str::trim)
            .filter(|q| !q.is_empty())
            .map(str::to_lowercase);
        let conditions = |query: &mut QueryBuilder<Sqlite>| {
            query.push(" AND v.connection_id = ");
            query.push_bind(connection_id.uuid());
            if let Some(kind) = filter.kind {
                query.push(" AND v.kind = ");
                query.push_bind(kind.as_str());
            }
            if let Some(q) = &q {
                query.push(" AND instr(lower(coalesce(v.name, '')), ");
                query.push_bind(q.clone());
                query.push(") > 0");
            }
        };
        let mut count = scoped(
            "SELECT count(*) FROM channel_conversations v WHERE v.organization_id = ? AND v.workspace_id = ?",
            organization_id,
            workspace_id,
        )?;
        conditions(&mut count);
        let total: i64 = count
            .build_query_scalar()
            .fetch_one(self.database.pool())
            .await
            .map_err(failed("list_conversations"))?;
        let mut query = scoped(
            concat!(
                conversation_select!(),
                " WHERE v.organization_id = ? AND v.workspace_id = ?"
            ),
            organization_id,
            workspace_id,
        )?;
        conditions(&mut query);
        // Most recent message first; a conversation with none last.
        Position::push_after(&after, &mut query, "coalesce(v.last_message_at, -1)", "v.id");
        query.push(" ORDER BY coalesce(v.last_message_at, -1) DESC, v.id DESC LIMIT ");
        query.push_bind(i64::from(limit) + 1);
        let rows = query
            .build()
            .fetch_all(self.database.pool())
            .await
            .map_err(failed("list_conversations"))?;
        let items = rows.iter().map(conversation_from).collect::<Result<Vec<_>, _>>()?;
        page_of(items, limit, total, |v| {
            Position::encode(v.last_message_at.map_or(-1, Timestamp::millis), v.id)
        })
    }

    async fn find_conversation(
        &self,
        organization_id: OrganizationId,
        workspace_id: WorkspaceId,
        connection_id: ChannelConnectionId,
        conversation_id: ChannelConversationId,
    ) -> Result<Option<ChannelConversation>, ChannelError> {
        let row = sqlx::query(FIND_CONVERSATION)
            .bind(organization_id.uuid())
            .bind(workspace_id.uuid())
            .bind(connection_id.uuid())
            .bind(conversation_id.uuid())
            .fetch_optional(self.database.pool())
            .await
            .map_err(failed("find_conversation"))?;
        row.as_ref().map(conversation_from).transpose()
    }

    async fn list_messages(
        &self,
        organization_id: OrganizationId,
        workspace_id: WorkspaceId,
        connection_id: ChannelConnectionId,
        conversation_id: ChannelConversationId,
        filter: &MessageFilter,
        cursor: Option<&str>,
        limit: u32,
    ) -> Result<Page<ChannelMessage>, ChannelError> {
        let after = Position::decode(cursor, |id| ChannelMessageId::parse(id).ok().map(|id| id.uuid()))?;
        let limit = limit_of(limit);
        let conditions = |query: &mut QueryBuilder<Sqlite>| {
            query.push(" AND g.connection_id = ");
            query.push_bind(connection_id.uuid());
            query.push(" AND g.conversation_id = ");
            query.push_bind(conversation_id.uuid());
            if let Some(thread) = &filter.thread {
                query.push(" AND g.thread_key = ");
                query.push_bind(thread.clone());
            }
        };
        let mut count = scoped(
            "SELECT count(*) FROM channel_messages g WHERE g.organization_id = ? AND g.workspace_id = ?",
            organization_id,
            workspace_id,
        )?;
        conditions(&mut count);
        let total: i64 = count
            .build_query_scalar()
            .fetch_one(self.database.pool())
            .await
            .map_err(failed("list_messages"))?;
        let mut query = scoped(
            concat!(message_select!(), " WHERE g.organization_id = ? AND g.workspace_id = ?"),
            organization_id,
            workspace_id,
        )?;
        conditions(&mut query);
        Position::push_after(&after, &mut query, "g.sent_at", "g.id");
        query.push(" ORDER BY g.sent_at DESC, g.id DESC LIMIT ");
        query.push_bind(i64::from(limit) + 1);
        let rows = query
            .build()
            .fetch_all(self.database.pool())
            .await
            .map_err(failed("list_messages"))?;
        let items = rows.iter().map(message_from).collect::<Result<Vec<_>, _>>()?;
        page_of(items, limit, total, |g| Position::encode(g.sent_at.millis(), g.id))
    }

    async fn list_agent_channels(
        &self,
        organization_id: OrganizationId,
        workspace_id: WorkspaceId,
        agent_id: AgentId,
    ) -> Result<Vec<AgentChannel>, ChannelError> {
        let mut connection = self
            .database
            .pool()
            .acquire()
            .await
            .map_err(failed("list_agent_channels"))?;
        if !agent_exists(&mut connection, organization_id, workspace_id, agent_id).await? {
            return Err(ChannelError::AgentNotFound);
        }
        let rows = sqlx::query(concat!(
            agent_channel_select!(),
            " WHERE h.organization_id = ? AND h.workspace_id = ? AND h.agent_id = ? ORDER BY h.created_at, h.id"
        ))
        .bind(organization_id.uuid())
        .bind(workspace_id.uuid())
        .bind(agent_id.uuid())
        .fetch_all(&mut *connection)
        .await
        .map_err(failed("list_agent_channels"))?;
        rows.iter().map(agent_channel_from).collect()
    }

    async fn put_agent_channel(
        &self,
        organization_id: OrganizationId,
        workspace_id: WorkspaceId,
        agent_id: AgentId,
        connection_id: ChannelConnectionId,
        editor: ChannelEditor,
        settings: &AgentChannelSettings,
        now: Timestamp,
        audit: &Audit<'_, AgentChannel>,
    ) -> Result<AgentChannel, ChannelError> {
        let mut transaction = self.immediate("put_agent_channel").await?;
        editor_allowed(&mut transaction, organization_id, workspace_id, editor).await?;
        if !agent_exists(&mut transaction, organization_id, workspace_id, agent_id).await? {
            return Err(ChannelError::AgentNotFound);
        }
        let connection = find_connection_in(&mut transaction, organization_id, workspace_id, connection_id)
            .await?
            .ok_or(ChannelError::ConnectionNotFound)?;
        if connection.agent_id != Some(agent_id) {
            return Err(ChannelError::NotAgentsApp);
        }
        if connection.status == ConnectionStatus::Disconnected {
            return Err(ChannelError::ConnectionDisconnected);
        }
        for tool in &settings.tools {
            match level_in_force(&mut transaction, organization_id, connection.provider, tool).await? {
                ToolLevel::Deny => return Err(ChannelError::ToolDenied(tool.clone())),
                ToolLevel::Approve if !settings.approval_required.contains(tool) => {
                    return Err(ChannelError::ApprovalRequiredByPolicy(tool.clone()));
                }
                _ => {}
            }
        }
        for conversation_id in &settings.audience_conversations {
            let known = sqlx::query(
                "SELECT 1 FROM channel_conversations \
                 WHERE organization_id = ? AND workspace_id = ? AND connection_id = ? AND id = ?",
            )
            .bind(organization_id.uuid())
            .bind(workspace_id.uuid())
            .bind(connection_id.uuid())
            .bind(conversation_id.uuid())
            .fetch_optional(&mut *transaction)
            .await
            .map_err(failed("put_agent_channel"))?;
            if known.is_none() {
                return Err(ChannelError::UnknownConversation);
            }
        }
        let existing = find_agent_channel_on(&mut transaction, organization_id, workspace_id, connection_id).await?;
        write_agent_channel(
            &mut transaction,
            organization_id,
            workspace_id,
            existing.map(|channel| channel.id),
            agent_id,
            connection_id,
            settings,
            now,
        )
        .await?;
        let channel = find_agent_channel_on(&mut transaction, organization_id, workspace_id, connection_id)
            .await?
            .ok_or_else(|| read("an agent channel gone inside its own transaction"))?;
        audit_in_workspace(&mut transaction, organization_id, workspace_id, &audit(&channel)).await?;
        transaction.commit().await.map_err(failed("put_agent_channel"))?;
        Ok(channel)
    }

    async fn delete_agent_channel(
        &self,
        organization_id: OrganizationId,
        workspace_id: WorkspaceId,
        agent_id: AgentId,
        connection_id: ChannelConnectionId,
        editor: ChannelEditor,
        audit: &Audit<'_, AgentChannel>,
    ) -> Result<(), ChannelError> {
        let mut transaction = self.immediate("delete_agent_channel").await?;
        editor_allowed(&mut transaction, organization_id, workspace_id, editor).await?;
        let channel = find_agent_channel_on(&mut transaction, organization_id, workspace_id, connection_id)
            .await?
            .filter(|channel| channel.agent_id == agent_id)
            .ok_or(ChannelError::AgentChannelNotFound)?;
        sqlx::query(
            "DELETE FROM agent_channels WHERE organization_id = ? AND workspace_id = ? AND agent_id = ? \
             AND connection_id = ? AND id = ?",
        )
        .bind(organization_id.uuid())
        .bind(workspace_id.uuid())
        .bind(agent_id.uuid())
        .bind(connection_id.uuid())
        .bind(channel.id.uuid())
        .execute(&mut *transaction)
        .await
        .map_err(failed("delete_agent_channel"))?;
        audit_in_workspace(&mut transaction, organization_id, workspace_id, &audit(&channel)).await?;
        transaction.commit().await.map_err(failed("delete_agent_channel"))?;
        Ok(())
    }

    async fn record_delivery(
        &self,
        organization_id: OrganizationId,
        workspace_id: WorkspaceId,
        app_id: ChannelAppId,
        delivery: &NewDelivery,
        now: Timestamp,
    ) -> Result<DeliveryOutcome, ChannelError> {
        let mut transaction = self.immediate("record_delivery").await?;
        let app = sqlx::query("SELECT 1 FROM channel_apps WHERE organization_id = ? AND workspace_id = ? AND id = ?")
            .bind(organization_id.uuid())
            .bind(workspace_id.uuid())
            .bind(app_id.uuid())
            .fetch_optional(&mut *transaction)
            .await
            .map_err(failed("record_delivery"))?;
        if app.is_none() {
            return Err(ChannelError::AppNotFound);
        }
        let recorded = sqlx::query(
            "INSERT INTO channel_inbound_events (id, organization_id, workspace_id, app_id, provider_event_id, kind, \
             received_at) VALUES (?, ?, ?, ?, ?, ?, ?) \
             ON CONFLICT (organization_id, workspace_id, app_id, provider_event_id) DO NOTHING",
        )
        .bind(Uuid::now_v7())
        .bind(organization_id.uuid())
        .bind(workspace_id.uuid())
        .bind(app_id.uuid())
        .bind(&delivery.provider_event_id)
        .bind(&delivery.kind)
        .bind(now.millis())
        .execute(&mut *transaction)
        .await
        .map_err(failed("record_delivery"))?;
        let outcome = if recorded.rows_affected() == 0 {
            DeliveryOutcome::Duplicate
        } else {
            receive(&mut transaction, organization_id, workspace_id, app_id, delivery, now).await?
        };
        transaction.commit().await.map_err(failed("record_delivery"))?;
        Ok(outcome)
    }

    async fn inbound_context(
        &self,
        organization_id: OrganizationId,
        workspace_id: WorkspaceId,
        message_id: ChannelMessageId,
    ) -> Result<Option<InboundContext>, ChannelError> {
        let mut transaction = self.database.pool().begin().await.map_err(failed("inbound_context"))?;
        let context = inbound_context_in(&mut transaction, organization_id, workspace_id, message_id).await?;
        transaction.commit().await.map_err(failed("inbound_context"))?;
        Ok(context)
    }

    async fn people_to_read(
        &self,
        organization_id: OrganizationId,
        workspace_id: WorkspaceId,
        connection_id: ChannelConnectionId,
        names: &[String],
        emails: &[String],
        now: Timestamp,
    ) -> Result<Vec<String>, ChannelError> {
        exchange::people_to_read(self, organization_id, workspace_id, connection_id, names, emails, now).await
    }

    async fn record_people(
        &self,
        organization_id: OrganizationId,
        workspace_id: WorkspaceId,
        connection_id: ChannelConnectionId,
        people: &[zupflow_core::channels::store::PersonLookup],
        now: Timestamp,
    ) -> Result<(), ChannelError> {
        exchange::record_people(self, organization_id, workspace_id, connection_id, people, now).await
    }

    async fn reply_of(
        &self,
        organization_id: OrganizationId,
        workspace_id: WorkspaceId,
        run_id: RunId,
    ) -> Result<Option<zupflow_core::channels::reply::ChannelReply>, ChannelError> {
        exchange::reply_of(self, organization_id, workspace_id, run_id).await
    }

    async fn mark_handled(
        &self,
        organization_id: OrganizationId,
        workspace_id: WorkspaceId,
        message_id: ChannelMessageId,
        now: Timestamp,
    ) -> Result<(), ChannelError> {
        sqlx::query(
            "UPDATE channel_messages SET handled_at = coalesce(handled_at, ?) \
             WHERE organization_id = ? AND workspace_id = ? AND id = ?",
        )
        .bind(now.millis())
        .bind(organization_id.uuid())
        .bind(workspace_id.uuid())
        .bind(message_id.uuid())
        .execute(self.database.pool())
        .await
        .map_err(failed("mark_handled"))?;
        Ok(())
    }

    async fn dispatch(
        &self,
        organization_id: OrganizationId,
        workspace_id: WorkspaceId,
        dispatch: &NewDispatch,
        template: Option<&SessionTemplate>,
        now: Timestamp,
    ) -> Result<DispatchOutcome, ChannelError> {
        let mut transaction = self.immediate("dispatch").await?;
        let outcome =
            crate::channel_runs::dispatch_in(&mut transaction, organization_id, workspace_id, dispatch, template, now)
                .await
                .map_err(from_run_store)?;
        transaction.commit().await.map_err(failed("dispatch"))?;
        Ok(outcome)
    }

    async fn steerable(
        &self,
        organization_id: OrganizationId,
        workspace_id: WorkspaceId,
        run_id: RunId,
    ) -> Result<Option<Steer>, ChannelError> {
        let mut transaction = self.database.pool().begin().await.map_err(failed("steerable"))?;
        let steer = crate::channel_runs::steerable_in(&mut transaction, organization_id, workspace_id, run_id)
            .await
            .map_err(from_run_store)?;
        transaction.commit().await.map_err(failed("steerable"))?;
        Ok(steer)
    }

    async fn run_channel(
        &self,
        organization_id: OrganizationId,
        workspace_id: WorkspaceId,
        run_id: RunId,
    ) -> Result<Option<RunChannelContext>, ChannelError> {
        tooling::run_channel(self, organization_id, workspace_id, run_id).await
    }

    async fn find_message(
        &self,
        organization_id: OrganizationId,
        workspace_id: WorkspaceId,
        connection_id: ChannelConnectionId,
        message_id: ChannelMessageId,
    ) -> Result<Option<ChannelMessage>, ChannelError> {
        tooling::find_message(self, organization_id, workspace_id, connection_id, message_id).await
    }

    async fn sent_by_call(
        &self,
        organization_id: OrganizationId,
        workspace_id: WorkspaceId,
        run_id: RunId,
        call_id: ToolCallId,
    ) -> Result<Option<ChannelMessage>, ChannelError> {
        tooling::sent_by_call(self, organization_id, workspace_id, run_id, call_id).await
    }

    async fn find_person(
        &self,
        organization_id: OrganizationId,
        workspace_id: WorkspaceId,
        connection_id: ChannelConnectionId,
        person_id: ChannelPersonId,
    ) -> Result<Option<KnownPerson>, ChannelError> {
        tooling::find_person(self, organization_id, workspace_id, connection_id, person_id).await
    }

    async fn find_people(
        &self,
        organization_id: OrganizationId,
        workspace_id: WorkspaceId,
        connection_id: ChannelConnectionId,
        query: &str,
        limit: u32,
    ) -> Result<Vec<KnownPerson>, ChannelError> {
        tooling::find_people(self, organization_id, workspace_id, connection_id, query, limit).await
    }

    async fn search_messages(
        &self,
        organization_id: OrganizationId,
        workspace_id: WorkspaceId,
        connection_id: ChannelConnectionId,
        conversation_id: Option<ChannelConversationId>,
        readable_dm: ChannelConversationId,
        query: &str,
        limit: u32,
    ) -> Result<Vec<ChannelMessage>, ChannelError> {
        tooling::search_messages(
            self,
            organization_id,
            workspace_id,
            connection_id,
            conversation_id,
            readable_dm,
            query,
            limit,
        )
        .await
    }

    async fn mention_names(
        &self,
        organization_id: OrganizationId,
        workspace_id: WorkspaceId,
        connection_id: ChannelConnectionId,
        people: &[String],
        conversations: &[String],
    ) -> Result<zupflow_core::channels::thread::MentionNames, ChannelError> {
        tooling::mention_names(
            self,
            (organization_id, workspace_id, connection_id),
            people,
            conversations,
        )
        .await
    }

    async fn send_counts(
        &self,
        organization_id: OrganizationId,
        workspace_id: WorkspaceId,
        connection_id: ChannelConnectionId,
        now: Timestamp,
    ) -> Result<SendCounts, ChannelError> {
        tooling::send_counts(self, (organization_id, workspace_id, connection_id), now).await
    }

    async fn take_send_slot(
        &self,
        organization_id: OrganizationId,
        workspace_id: WorkspaceId,
        connection_id: ChannelConnectionId,
        run_id: RunId,
        call_id: ToolCallId,
        now: Timestamp,
    ) -> Result<SendSlot, ChannelError> {
        tooling::take_send_slot(
            self,
            (organization_id, workspace_id, connection_id),
            (run_id, call_id),
            now,
        )
        .await
    }

    async fn record_own_edit(
        &self,
        organization_id: OrganizationId,
        workspace_id: WorkspaceId,
        connection_id: ChannelConnectionId,
        message_id: ChannelMessageId,
        text: &str,
        now: Timestamp,
    ) -> Result<(), ChannelError> {
        let scope = (organization_id, workspace_id, connection_id);
        tooling::record_own_change(self, scope, message_id, Some(text), now).await
    }

    async fn record_own_delete(
        &self,
        organization_id: OrganizationId,
        workspace_id: WorkspaceId,
        connection_id: ChannelConnectionId,
        message_id: ChannelMessageId,
        now: Timestamp,
    ) -> Result<(), ChannelError> {
        let scope = (organization_id, workspace_id, connection_id);
        tooling::record_own_change(self, scope, message_id, None, now).await
    }

    async fn retention_due_cross_tenant(
        &self,
        now: Timestamp,
        limit: u32,
    ) -> Result<Vec<(OrganizationId, WorkspaceId)>, ChannelError> {
        tooling::retention_due_cross_tenant(self, now, limit).await
    }

    async fn sweep_retention(
        &self,
        organization_id: OrganizationId,
        workspace_id: WorkspaceId,
        now: Timestamp,
    ) -> Result<Swept, ChannelError> {
        tooling::sweep_retention(self, organization_id, workspace_id, now).await
    }

    async fn find_session(
        &self,
        organization_id: OrganizationId,
        workspace_id: WorkspaceId,
        agent_channel_id: AgentChannelId,
        conversation_id: ChannelConversationId,
        thread_key: &str,
    ) -> Result<Option<ChannelSession>, ChannelError> {
        let mut connection = self.database.pool().acquire().await.map_err(failed("find_session"))?;
        crate::channel_runs::find_session_in(
            &mut connection,
            organization_id,
            workspace_id,
            agent_channel_id,
            conversation_id,
            thread_key,
        )
        .await
        .map_err(from_run_store)
    }

    async fn list_tool_levels(&self, organization_id: OrganizationId) -> Result<Vec<ChannelToolLevel>, ChannelError> {
        let mut connection = self.database.pool().acquire().await.map_err(failed("tool_levels"))?;
        levels_in(&mut connection, organization_id).await
    }

    async fn set_tool_levels(
        &self,
        organization_id: OrganizationId,
        editor: MemberId,
        provider: ChannelProviderKind,
        levels: &[LevelChange],
        updated_by: UserId,
        now: Timestamp,
        audit: &Audit<'_, Vec<ChannelToolLevel>>,
    ) -> Result<Vec<ChannelToolLevel>, ChannelError> {
        let mut transaction = self.immediate("set_tool_levels").await?;
        admin_allowed(&mut transaction, organization_id, editor).await?;
        for change in levels {
            sqlx::query(
                "INSERT INTO channel_tool_levels (id, organization_id, provider, tool, level, updated_at, \
                 updated_by_user_id) VALUES (?, ?, ?, ?, ?, ?, ?) \
                 ON CONFLICT (organization_id, provider, tool) DO UPDATE SET level = excluded.level, \
                 updated_at = excluded.updated_at, updated_by_user_id = excluded.updated_by_user_id",
            )
            .bind(Uuid::now_v7())
            .bind(organization_id.uuid())
            .bind(provider.as_str())
            .bind(&change.tool)
            .bind(change.level.as_str())
            .bind(now.millis())
            .bind(updated_by.uuid())
            .execute(&mut *transaction)
            .await
            .map_err(failed("set_tool_levels"))?;
        }
        let levels = levels_in(&mut transaction, organization_id).await?;
        crate::audit::record(&mut transaction, organization_id, &audit(&levels))
            .await
            .map_err(failed("audit"))?;
        transaction.commit().await.map_err(failed("set_tool_levels"))?;
        Ok(levels)
    }

    async fn channel_settings(&self, organization_id: OrganizationId) -> Result<ChannelSettings, ChannelError> {
        let mut connection = self
            .database
            .pool()
            .acquire()
            .await
            .map_err(failed("channel_settings"))?;
        settings_in(&mut connection, organization_id).await
    }

    async fn save_channel_settings(
        &self,
        organization_id: OrganizationId,
        editor: MemberId,
        values: &BreakerSettings,
        updated_by: UserId,
        now: Timestamp,
        audit: &Audit<'_, ChannelSettings>,
    ) -> Result<ChannelSettings, ChannelError> {
        let mut transaction = self.immediate("save_channel_settings").await?;
        admin_allowed(&mut transaction, organization_id, editor).await?;
        sqlx::query(
            "INSERT INTO channel_settings (id, organization_id, breaker_runs, breaker_window_minutes, \
             breaker_pause_minutes, message_retention_days, updated_at, updated_by_user_id) \
             VALUES (?, ?, ?, ?, ?, ?, ?, ?) \
             ON CONFLICT (organization_id) DO UPDATE SET breaker_runs = excluded.breaker_runs, \
             breaker_window_minutes = excluded.breaker_window_minutes, \
             breaker_pause_minutes = excluded.breaker_pause_minutes, \
             message_retention_days = excluded.message_retention_days, updated_at = excluded.updated_at, \
             updated_by_user_id = excluded.updated_by_user_id",
        )
        .bind(Uuid::now_v7())
        .bind(organization_id.uuid())
        .bind(i64::from(values.breaker_runs))
        .bind(i64::from(values.breaker_window_minutes))
        .bind(i64::from(values.breaker_pause_minutes))
        .bind(i64::from(values.message_retention_days))
        .bind(now.millis())
        .bind(updated_by.uuid())
        .execute(&mut *transaction)
        .await
        .map_err(failed("save_channel_settings"))?;
        let settings = settings_in(&mut transaction, organization_id).await?;
        crate::audit::record(&mut transaction, organization_id, &audit(&settings))
            .await
            .map_err(failed("audit"))?;
        transaction.commit().await.map_err(failed("save_channel_settings"))?;
        Ok(settings)
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use std::collections::{HashMap, HashSet};
    use std::sync::Arc;

    use zupflow_conformance::channel_store::{Prepare, Stores, Tenant};

    use super::*;
    use crate::schema::{Temporary, temporary};

    /// Makes the tenant's rows straight in the database.
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
                for (workspace, environment) in [
                    (tenant.sandbox, "sandbox"),
                    (tenant.sibling, "sandbox"),
                    (tenant.production, "production"),
                ] {
                    sqlx::query(
                        "INSERT INTO workspaces (id, organization_id, environment, name, slug, created_at) \
                         VALUES (?, ?, ?, ?, ?, 0)",
                    )
                    .bind(workspace.uuid())
                    .bind(org)
                    .bind(environment)
                    .bind(format!("W {}", &workspace.uuid().simple().to_string()[24..]))
                    .bind(format!("w-{}", workspace.uuid().simple()))
                    .execute(pool)
                    .await
                    .unwrap();
                }
                for (member, user, role) in [
                    (tenant.admin, tenant.admin_user, "admin"),
                    (tenant.developer, tenant.developer_user, "developer"),
                    (tenant.viewer, tenant.viewer_user, "viewer"),
                ] {
                    sqlx::query("INSERT INTO users (id, name, email, created_at) VALUES (?, 'Jordan', ?, 0)")
                        .bind(user.uuid())
                        .bind(format!("{}@northwind.example", user.uuid().simple()))
                        .execute(pool)
                        .await
                        .unwrap();
                    sqlx::query(
                        "INSERT INTO members (id, organization_id, user_id, role, status, invited_at, joined_at) \
                         VALUES (?, ?, ?, ?, 'active', 0, 0)",
                    )
                    .bind(member.uuid())
                    .bind(org)
                    .bind(user.uuid())
                    .bind(role)
                    .execute(pool)
                    .await
                    .unwrap();
                }
                for (key, environment) in [(tenant.sandbox_key, "sandbox"), (tenant.production_key, "production")] {
                    sqlx::query(
                        "INSERT INTO api_keys (id, organization_id, environment, name, prefix, last4, secret_hash, \
                         created_by_user_id, created_at) VALUES (?, ?, ?, 'Test', 'test', 'abcd', ?, ?, 0)",
                    )
                    .bind(key.uuid())
                    .bind(org)
                    .bind(environment)
                    .bind(key.uuid().as_bytes().repeat(2))
                    .bind(tenant.admin_user.uuid())
                    .execute(pool)
                    .await
                    .unwrap();
                }
                for (agent, workspace) in [
                    (tenant.agent, tenant.sandbox),
                    (tenant.other_agent, tenant.sandbox),
                    (tenant.sibling_agent, tenant.sibling),
                    (tenant.production_agent, tenant.production),
                ] {
                    sqlx::query(
                        "INSERT INTO agents (id, organization_id, workspace_id, name, description, instructions, \
                         status, created_at, updated_at) VALUES (?, ?, ?, 'Scribe', '', '', 'active', 0, 0)",
                    )
                    .bind(agent.uuid())
                    .bind(org)
                    .bind(workspace.uuid())
                    .execute(pool)
                    .await
                    .unwrap();
                }
                for run in tenant.runs {
                    sqlx::query(
                        "INSERT INTO runs (id, organization_id, workspace_id, agent_id, session_id, trigger, job_id, \
                         status, input, model, instructions, input_tokens, output_tokens, tool_calls, created_at) \
                         VALUES (?1, ?2, ?3, ?4, ?1, 'api', ?1, 'queued', 'hi', 'm', '', 0, 0, 0, 0)",
                    )
                    .bind(run.uuid())
                    .bind(org)
                    .bind(tenant.sandbox.uuid())
                    .bind(tenant.agent.uuid())
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
        zupflow_conformance::channel_store::run(
            Stores {
                channels: &SqliteChannelStore::new(database.clone()),
                agents: &crate::SqliteAgentStore::new(database.clone()),
                connect: &crate::SqliteConnectStore::new(database.clone()),
                jobs: &crate::SqliteJobQueue::new(database.clone()),
                runs: &crate::SqliteRunStore::new(database.clone()),
            },
            prepare(database),
        )
        .await;
    }

    /// One token of Rust source: enough of a lexer to find every SQL string.
    #[derive(Debug, Clone, PartialEq)]
    enum Token {
        Text(String),
        Word(String),
        Mark(char),
    }

    fn lex(source: &str) -> Vec<Token> {
        let chars: Vec<char> = source.chars().collect();
        let at = |i: usize| chars.get(i).copied().unwrap_or('\0');
        let mut tokens = Vec::new();
        let mut i = 0;
        while i < chars.len() {
            let c = chars[i];
            if c.is_whitespace() {
                i += 1;
            } else if c == '/' && at(i + 1) == '/' {
                while i < chars.len() && chars[i] != '\n' {
                    i += 1;
                }
            } else if c == '/' && at(i + 1) == '*' {
                i += 2;
                while i < chars.len() && !(chars[i] == '*' && at(i + 1) == '/') {
                    i += 1;
                }
                i += 2;
            } else if c == 'r' && (at(i + 1) == '"' || (at(i + 1) == '#' && at(i + 2) == '"')) {
                let hashes = usize::from(at(i + 1) == '#');
                i += 2 + hashes;
                let mut text = String::new();
                while !(chars[i] == '"' && (hashes == 0 || at(i + 1) == '#')) {
                    text.push(chars[i]);
                    i += 1;
                }
                i += 1 + hashes;
                tokens.push(Token::Text(text));
            } else if c == '"' {
                i += 1;
                let mut text = String::new();
                loop {
                    let c = chars[i];
                    i += 1;
                    match c {
                        '"' => break,
                        '\\' => {
                            let escaped = chars[i];
                            i += 1;
                            match escaped {
                                // A line continuation: the next line's indent goes too.
                                '\n' => {
                                    while chars[i].is_whitespace() {
                                        i += 1;
                                    }
                                }
                                'n' => text.push('\n'),
                                't' => text.push('\t'),
                                other => text.push(other),
                            }
                        }
                        c => text.push(c),
                    }
                }
                tokens.push(Token::Text(text));
            } else if c == '\'' {
                // A character ('x', '\n', '"'), or a lifetime, whose name follows as a word.
                if at(i + 1) == '\\' {
                    i += 3;
                    while chars[i] != '\'' {
                        i += 1;
                    }
                    i += 1;
                } else if at(i + 2) == '\'' {
                    i += 3;
                } else {
                    i += 1;
                }
            } else if c.is_alphanumeric() || c == '_' {
                let mut word = String::new();
                while i < chars.len() && (chars[i].is_alphanumeric() || chars[i] == '_') {
                    word.push(chars[i]);
                    i += 1;
                }
                tokens.push(Token::Word(word));
            } else {
                tokens.push(Token::Mark(c));
                i += 1;
            }
        }
        tokens
    }

    /// Where the group opened at `open` closes.
    fn close(tokens: &[Token], open: usize) -> usize {
        let mut depth = 0;
        for (i, token) in tokens.iter().enumerate().skip(open) {
            match token {
                Token::Mark('(' | '[' | '{') => depth += 1,
                Token::Mark(')' | ']' | '}') => {
                    depth -= 1;
                    if depth == 0 {
                        return i;
                    }
                }
                _ => {}
            }
        }
        panic!("a group that never closes");
    }

    /// The SQL text this file spells, each piece on its own: a string, a
    /// `concat!`, a column macro or a `&str` constant.
    struct Spelled {
        macros: HashMap<String, String>,
        constants: HashMap<String, String>,
    }

    impl Spelled {
        fn word(tokens: &[Token], i: usize) -> Option<&str> {
            match tokens.get(i) {
                Some(Token::Word(word)) => Some(word),
                _ => None,
            }
        }

        /// The piece starting at `i`, and where the next token is.
        fn piece(&self, tokens: &[Token], i: usize) -> Option<(String, usize)> {
            match &tokens[i] {
                Token::Text(text) => Some((text.clone(), i + 1)),
                Token::Word(word) if tokens.get(i + 1) == Some(&Token::Mark('!')) => {
                    let open = i + 2;
                    if word == "concat" {
                        let end = close(tokens, open);
                        Some((self.pieces(tokens, open + 1, end).concat(), end + 1))
                    } else {
                        let body = self.macros.get(word)?;
                        Some((body.clone(), close(tokens, open) + 1))
                    }
                }
                Token::Word(word) => self.constants.get(word).map(|text| (text.clone(), i + 1)),
                Token::Mark(_) => None,
            }
        }

        fn pieces(&self, tokens: &[Token], from: usize, to: usize) -> Vec<String> {
            let mut pieces = Vec::new();
            let mut i = from;
            while i < to {
                match self.piece(tokens, i) {
                    Some((text, next)) => {
                        pieces.push(text);
                        i = next;
                    }
                    None => i += 1,
                }
            }
            pieces
        }
    }

    /// Statements that read without a tenant, by the function they're in:
    /// each documented as cross-tenant where it's declared.
    const CROSS_TENANT: &[&str] = &[
        // A provider's event names only its app's inbound reference.
        "attribute_app_by_inbound_ref_cross_tenant",
        // A sweep over every workspace's unsettled apps, like a worker's claim.
        "reap_stranded_apps_cross_tenant",
        // A sweep over every workspace's linked teams for tokens to rotate.
        "queue_due_rotations_cross_tenant",
        // A sweep over every workspace's channel data for what's past
        // retention, each then swept in its own scope.
        "retention_due_cross_tenant",
        // A count of every workspace's linked teams by provider, naming none.
        "count_teams_by_provider_cross_tenant",
    ];

    /// Statements on a workspace's tables that hold to the organization
    /// alone, by the function they're in: none so far. The organization's
    /// own tables (`channel_tool_levels`, `channel_settings`) and its role
    /// checks (`members`, `api_keys`) aren't workspace-scoped, so they need
    /// no entry here.
    const ORGANIZATION_ONLY: &[&str] = &[];

    /// What [`audit`] found in a source: how many statements, which
    /// exceptions were used, and every statement that breaks the rule.
    struct Audited {
        checked: usize,
        exempted: HashSet<String>,
        problems: Vec<String>,
    }

    /// Reads every SQL string in `source`, each on its own, for its scope:
    /// bound (`organization_id = ?`, and `workspace_id = ?` when it reaches
    /// a workspace-scoped table) after its `WHERE`, or as columns of an
    /// `INSERT`. A statement is whole where it's written: a list query
    /// starts with its scope bound ([`scoped`]), and anything pushed after
    /// is a fragment, never a statement.
    fn audit(source: &str) -> Audited {
        let tokens = lex(source);
        let mut spelled = Spelled {
            macros: Default::default(),
            constants: Default::default(),
        };
        // Macros and constants first, in order, so each can use those above it.
        let mut skip = Vec::new();
        for i in 0..tokens.len() {
            if Spelled::word(&tokens, i) == Some("macro_rules") {
                let name = Spelled::word(&tokens, i + 2).unwrap().to_owned();
                let end = close(&tokens, i + 3);
                let arrow = (i..end)
                    .find(|&j| tokens[j] == Token::Mark('=') && tokens[j + 1] == Token::Mark('>'))
                    .unwrap();
                let body = spelled.pieces(&tokens, arrow + 2, end);
                assert_eq!(body.len(), 1, "{name}! spells one piece");
                spelled.macros.insert(name, body.concat());
                skip.push((i, end));
            }
            if Spelled::word(&tokens, i) == Some("const")
                && tokens.get(i + 2) == Some(&Token::Mark(':'))
                && tokens.get(i + 3) == Some(&Token::Mark('&'))
                && Spelled::word(&tokens, i + 4) == Some("str")
            {
                let name = Spelled::word(&tokens, i + 1).unwrap().to_owned();
                let end = (i..tokens.len()).find(|&j| tokens[j] == Token::Mark(';')).unwrap();
                let value = spelled.pieces(&tokens, i + 6, end);
                assert_eq!(value.len(), 1, "{name} spells one piece");
                spelled.constants.insert(name, value.concat());
            }
        }
        // Then every piece, with the function or constant it's in.
        let mut audited = Audited {
            checked: 0,
            exempted: HashSet::new(),
            problems: Vec::new(),
        };
        let mut within: Vec<(String, usize)> = Vec::new();
        let mut i = 0;
        while i < tokens.len() {
            if let Some(&(_, end)) = skip.iter().find(|(start, _)| *start == i) {
                i = end + 1;
                continue;
            }
            within.retain(|(_, end)| *end > i);
            if matches!(Spelled::word(&tokens, i), Some("fn" | "const"))
                && let Some(name) = Spelled::word(&tokens, i + 1)
            {
                let body = (i..tokens.len())
                    .find(|&j| matches!(tokens[j], Token::Mark('{' | ';')))
                    .unwrap();
                let end = if tokens[body] == Token::Mark('{') {
                    close(&tokens, body)
                } else {
                    body
                };
                within.push((name.to_owned(), end));
                // Past the name: a constant's own name isn't a use of it.
                i += 2;
                continue;
            }
            let Some((text, next)) = spelled.piece(&tokens, i) else {
                i += 1;
                continue;
            };
            i = next;
            let function = within.last().map(|(name, _)| name.clone()).unwrap_or_default();
            let upper = text.trim_start().to_uppercase();
            let statement = ["SELECT ", "INSERT ", "UPDATE ", "DELETE ", "WITH "]
                .iter()
                .any(|verb| upper.starts_with(verb));
            if !statement {
                let padded = format!(" {upper}");
                if [" FROM ", " INSERT ", " UPDATE ", " DELETE "]
                    .iter()
                    .any(|word| padded.contains(word))
                {
                    audited
                        .problems
                        .push(format!("{function}: a statement built from fragments: {text}"));
                }
                continue;
            }
            audited.checked += 1;
            let words: Vec<String> = text
                .split(|c: char| !(c.is_alphanumeric() || c == '_'))
                .filter(|word| !word.is_empty())
                .map(str::to_owned)
                .collect();
            let tables: Vec<String> = words
                .windows(2)
                .filter(|pair| matches!(pair[0].to_uppercase().as_str(), "FROM" | "JOIN" | "INTO" | "UPDATE"))
                .map(|pair| pair[1].clone())
                .filter(|table| crate::schema::workspace_scoped(table).is_some())
                .collect();
            if tables.is_empty() {
                audited
                    .problems
                    .push(format!("{function}: a statement on no declared table: {text}"));
                continue;
            }
            let workspace_table = tables
                .iter()
                .any(|table| crate::schema::workspace_scoped(table) == Some(true));
            let insert = upper.starts_with("INSERT ");
            let scope = if insert {
                text.clone()
            } else {
                upper.find("WHERE").map(|at| text[at..].to_owned()).unwrap_or_default()
            };
            let names = |column: &str| {
                if insert {
                    scope.contains(column)
                } else {
                    scope.contains(&format!("{column} = ?")) || scope.contains(&format!("{column} IS ?"))
                }
            };
            if CROSS_TENANT.contains(&function.as_str()) && !names("organization_id") {
                audited.exempted.insert(function);
                continue;
            }
            if !names("organization_id") {
                audited
                    .problems
                    .push(format!("{function}: a statement without its organization: {text}"));
            } else if workspace_table && !names("workspace_id") {
                if ORGANIZATION_ONLY.contains(&function.as_str()) {
                    audited.exempted.insert(function);
                } else {
                    audited.problems.push(format!(
                        "{function}: a statement on {tables:?} without its workspace: {text}"
                    ));
                }
            }
        }
        audited
    }

    #[test]
    fn every_statement_names_its_organization_and_workspace() {
        let source = include_str!("channels.rs");
        // The store's own file and what its modules add.
        let source = [
            &source[..source.find("#[cfg(test)]").unwrap()],
            include_str!("channels/lifecycle.rs"),
            include_str!("channels/exchange.rs"),
            include_str!("channels/tooling.rs"),
        ]
        .concat();
        let audited = audit(&source);
        assert!(audited.problems.is_empty(), "{:#?}", audited.problems);
        for name in CROSS_TENANT.iter().chain(ORGANIZATION_ONLY) {
            assert!(
                audited.exempted.contains(*name),
                "{name} is listed, but needs no exception"
            );
        }
        assert!(audited.checked > 80, "found only {} statements", audited.checked);
        // The channel runs, which both stores call into, hold to the same
        // rules, with no exception at all.
        let runs = audit(include_str!("channel_runs.rs"));
        assert!(runs.problems.is_empty(), "{:#?}", runs.problems);
        assert!(runs.exempted.is_empty(), "{:#?}", runs.exempted);
        assert!(runs.checked > 25, "found only {} statements", runs.checked);
    }

    /// The audit reads each statement on its own: one beside a scoped one,
    /// one in a local, one passed to a helper, a list query whose scope is
    /// only pushed after it, and a cross-tenant read outside its named
    /// function are all found.
    #[test]
    fn the_statement_audit_finds_what_drops_its_scope() {
        let source = r#"
            const INSERT_PERSON: &str = "INSERT INTO channel_people (id, organization_id, connection_id) VALUES (?, ?, ?)";
            fn helper(statement: &'static str) {}
            fn delete_connection() {
                for statement in [
                    "DELETE FROM channel_messages WHERE organization_id = ? AND workspace_id = ? AND id = ?",
                    "DELETE FROM channel_connections WHERE organization_id = ? AND id = ?",
                ] {}
                let statement = if x { "UPDATE channel_apps SET status = ? WHERE id = ?" } else { "x" };
                helper(INSERT_PERSON);
                let mut query = QueryBuilder::<Sqlite>::new("SELECT count(*) FROM channel_apps a WHERE a.organization_id = ");
                query.push(" AND a.workspace_id = ");
                let joined = concat!("SELECT 1", " FROM channel_apps");
                let fragment = query.push(" FROM channel_apps WHERE organization_id = ? AND workspace_id = ?");
            }
            fn somewhere_else() {
                sqlx::query("SELECT organization_id, workspace_id FROM channel_apps WHERE inbound_ref = ?");
            }
        "#;
        let audited = audit(source);
        let found = audited
            .problems
            .iter()
            .map(|problem| problem.split(": ").nth(1).unwrap())
            .collect::<Vec<_>>();
        assert_eq!(
            found,
            [
                // INSERT_PERSON: no workspace column.
                "a statement on [\"channel_people\"] without its workspace",
                // Beside a scoped DELETE, its own lacks the workspace.
                "a statement on [\"channel_connections\"] without its workspace",
                // A local's text.
                "a statement without its organization",
                // The constant again, where it's passed to the helper.
                "a statement on [\"channel_people\"] without its workspace",
                // A list query whose scope is only pushed after it.
                "a statement without its organization",
                // A statement with no scope at all, joined from pieces.
                "a statement without its organization",
                // A pushed fragment that is a statement.
                "a statement built from fragments",
                // An unscoped read outside the named cross-tenant functions.
                "a statement without its organization",
            ],
            "{:#?}",
            audited.problems
        );
    }
}
