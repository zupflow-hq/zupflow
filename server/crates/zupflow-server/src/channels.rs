//! Channels: a workspace's agents reached in a team's chat (Slack first), at
//! `/v1/workspaces/{ws}/channel-…`, `/v1/workspaces/{ws}/agents/{agent}/channels…`
//! and the organization's `/v1/organizations/{org}/channel-tool-levels` and
//! `/channel-settings`; and channel Connect sessions, which link a team or
//! install an agent's app on the hosted page.
//!
//! Every workspace route resolves with [`WorkspaceAccess::resolve`], so a key
//! reaches its own organization and environment only, and every lookup names
//! the organization and the workspace. Changes need a developer or stricter
//! (the organization's levels and settings an admin, from the Console), and
//! the Console can't link teams or make apps in Production: the customer's
//! product does that with a key. Each change is checked again by the store
//! inside its transaction, with the editor's role read again.
//!
//! An app is made by a job (`provision_channel_app`) after `202`: the worker
//! here opens the team's tokens, asks the [`AppProvisioner`] to make it, and
//! seals its secrets. Tokens and secrets are sealed under their own
//! [`EncryptionContext`] and never shown, logged or put in a job.
//!
//! The provider's inbound endpoint and the job that answers a new message
//! are [`crate::channel_inbound`]'s, with a worker of their own. The
//! consent callback that installs an app is `crate::channel_install`'s;
//! rotating a team's tokens, keeping an app in step with its agent and
//! removing a deleted one at the provider are `crate::channel_upkeep`'s,
//! on this worker.

use std::collections::{BTreeMap, HashSet};
use std::time::Duration;

use axum::Router;
use axum::extract::{Path, RawQuery, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use zeroize::{Zeroize, ZeroizeOnDrop, Zeroizing};
use zupflow_core::apps::ToolEffect;
use zupflow_core::audit::{AuditAction, AuditActor, AuditEntry, AuditObject, AuditPlace, AuditTarget};
use zupflow_core::channels::provision::{
    AppManifest, InstallStep, MAX_APP_NAME, ManagerTokens, ProvisionError, SLACK_EVENTS, SLACK_SCOPES,
};
use zupflow_core::channels::store::{
    AppCreation, AppFilter, ChannelEditor, ChannelError, ConnectionChanges, ConnectionFilter, ConversationFilter,
    LevelChange, MadeApp, MessageFilter, NewApp, NewInstall, NewManager, SealedVersion, TokenReplacement,
};
use zupflow_core::channels::{
    AgentChannel, AgentChannelSettings, AgentMessages, AppStatus, Audience, BreakerSettings, CHANNEL_TOOLS, ChannelApp,
    ChannelAppManager, ChannelConnection, ChannelConversation, ChannelMessage, ChannelPerson, ChannelProviderKind,
    ChannelSettings, ChannelToolLevel, ConnectTarget, ConnectionStatus, ConnectionTeam, ConversationKind, Direction,
    FIRST_VERSION, HandlerKind, Identity, InboundRef, MAX_AGENT_HOPS, MAX_LABEL, MIN_AGENT_HOPS, ManagerMethod,
    ManagerStatus, MidRun, Persona, Senders, Team, ThreadFollowUp, TokenState, channel_tool,
};
use zupflow_core::connect::{ConnectIntent, ConnectSession, NewConnectSession, Reference};
use zupflow_core::environment::Environment;
use zupflow_core::error::ApiError;
use zupflow_core::ids::{
    AgentChannelId, AgentId, ChannelAppId, ChannelAppManagerId, ChannelConnectionId, ChannelConversationId,
    ChannelMessageId, ConnectSessionId, OrganizationId, RunId, ToolCallId, WorkspaceId,
};
use zupflow_core::jobs::{Job, JobOptions};
use zupflow_core::keys::{self, EncryptionContext};
use zupflow_core::organizations::{MemberRole, User};
use zupflow_core::policy::ToolLevel;
use zupflow_core::runs::QueuedJob;
use zupflow_core::scope::{Name, Scope};
use zupflow_core::time::Timestamp;
use zupflow_core::workspaces::Page;

use crate::auth::{Access, Actor, AppState, Caller, ConsoleUser, OrganizationAccess, WorkspaceAccess};
use crate::http::{Failure, envelope, envelope_with_meta};
use crate::input::{Body, Fields, Object};

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/v1/workspaces/{ws}/channel-providers", get(list_providers))
        .route(
            "/v1/workspaces/{ws}/channel-app-managers",
            get(list_managers).post(create_manager),
        )
        .route(
            "/v1/workspaces/{ws}/channel-app-managers/{manager}",
            get(get_manager).patch(update_manager).delete(delete_manager),
        )
        .route("/v1/workspaces/{ws}/channel-apps", get(list_apps).post(create_app))
        .route(
            "/v1/workspaces/{ws}/channel-apps/{app}",
            get(get_app).delete(delete_app),
        )
        .route("/v1/workspaces/{ws}/channel-connections", get(list_connections))
        .route(
            "/v1/workspaces/{ws}/channel-connections/{channel_connection}",
            get(get_connection).patch(update_connection).delete(delete_connection),
        )
        .route(
            "/v1/workspaces/{ws}/channel-connections/{channel_connection}/conversations",
            get(list_conversations),
        )
        .route(
            "/v1/workspaces/{ws}/channel-connections/{channel_connection}/conversations/{conversation}/messages",
            get(list_messages),
        )
        .route("/v1/workspaces/{ws}/agents/{agent}/channels", get(list_agent_channels))
        .route(
            "/v1/workspaces/{ws}/agents/{agent}/channels/{channel_connection}",
            axum::routing::put(put_agent_channel).delete(delete_agent_channel),
        )
        .route(
            "/v1/organizations/{org}/channel-tool-levels",
            get(list_tool_levels).put(update_tool_levels),
        )
        .route(
            "/v1/organizations/{org}/channel-settings",
            get(get_settings).put(update_settings),
        )
}

/// The job that makes an app with its provider.
pub const PROVISION_CHANNEL_APP: &str = "provision_channel_app";

/// How long a claim holds a provisioning: longer than the provider's calls.
const LEASE: Duration = Duration::from_secs(5 * 60);
/// How often an idle worker looks for work.
const IDLE: Duration = Duration::from_millis(500);
const ATTEMPTS: u32 = 5;
/// How often a worker looks for apps whose making job is gone or buried.
const REAP_EVERY: Duration = Duration::from_secs(30);
/// How many such apps one sweep settles.
const REAP_LIMIT: u32 = 100;

/// The longest token a team admin pastes.
const TOKEN_MAX: usize = 4096;
/// The longest description a provider shows for an app.
const DESCRIPTION_MAX: usize = 140;
const PERSONA_NAME_MAX: usize = 80;
const SENDERS_MAX: usize = 500;
const EMAIL_MAX: usize = 320;

// --- Views ------------------------------------------------------------------

/// The contract's `ChannelAppManager`: never its tokens.
#[derive(Serialize)]
struct ManagerView {
    id: ChannelAppManagerId,
    workspace_id: WorkspaceId,
    provider: ChannelProviderKind,
    method: ManagerMethod,
    team: Team,
    status: ManagerStatus,
    configuration_token: TokenState,
    token_expires_at: Option<Timestamp>,
    app_count: u64,
    created_by: Option<User>,
    created_at: Timestamp,
    updated_at: Timestamp,
}

fn manager_view(manager: ChannelAppManager) -> ManagerView {
    ManagerView {
        id: manager.id,
        workspace_id: manager.workspace_id,
        provider: manager.provider,
        method: manager.method,
        team: manager.team,
        status: manager.status,
        configuration_token: manager.configuration_token,
        token_expires_at: manager.token_expires_at,
        app_count: manager.app_count,
        created_by: manager.created_by,
        created_at: manager.created_at,
        updated_at: manager.updated_at,
    }
}

/// The contract's `ChannelApp`: never its secrets or inbound reference.
#[derive(Serialize)]
struct AppView {
    id: ChannelAppId,
    workspace_id: WorkspaceId,
    manager_id: ChannelAppManagerId,
    agent_id: AgentId,
    provider: ChannelProviderKind,
    external_app_id: Option<String>,
    status: AppStatus,
    last_error: Option<String>,
    connection_id: Option<ChannelConnectionId>,
    created_at: Timestamp,
    updated_at: Timestamp,
}

fn app_view(app: ChannelApp) -> AppView {
    AppView {
        id: app.id,
        workspace_id: app.workspace_id,
        manager_id: app.manager_id,
        agent_id: app.agent_id,
        provider: app.provider,
        external_app_id: app.external_app_id,
        status: app.status,
        last_error: app.last_error,
        connection_id: app.connection_id,
        created_at: app.created_at,
        updated_at: app.updated_at,
    }
}

/// The contract's `ChannelConnection`: never its tokens.
#[derive(Serialize)]
struct ConnectionView {
    id: ChannelConnectionId,
    workspace_id: WorkspaceId,
    provider: ChannelProviderKind,
    app_id: Option<ChannelAppId>,
    agent_id: Option<AgentId>,
    short_id: zupflow_core::installations::ShortId,
    label: String,
    handler: HandlerKind,
    status: ConnectionStatus,
    team: ConnectionTeam,
    identity: Identity,
    granted_scopes: Vec<String>,
    connected_at: Option<Timestamp>,
    last_event_at: Option<Timestamp>,
    created_at: Timestamp,
    updated_at: Timestamp,
}

fn connection_view(connection: ChannelConnection) -> ConnectionView {
    ConnectionView {
        id: connection.id,
        workspace_id: connection.workspace_id,
        provider: connection.provider,
        app_id: connection.app_id,
        agent_id: connection.agent_id,
        short_id: connection.short_id,
        label: connection.label,
        handler: connection.handler,
        status: connection.status,
        team: connection.team,
        identity: connection.identity,
        granted_scopes: connection.granted_scopes,
        connected_at: connection.connected_at,
        last_event_at: connection.last_event_at,
        created_at: connection.created_at,
        updated_at: connection.updated_at,
    }
}

#[derive(Serialize)]
struct ConversationView {
    id: ChannelConversationId,
    workspace_id: WorkspaceId,
    connection_id: ChannelConnectionId,
    external_id: String,
    kind: ConversationKind,
    name: Option<String>,
    is_external: bool,
    identity_is_member: bool,
    last_message_at: Option<Timestamp>,
    paused_until: Option<Timestamp>,
    created_at: Timestamp,
}

fn conversation_view(conversation: ChannelConversation) -> ConversationView {
    ConversationView {
        id: conversation.id,
        workspace_id: conversation.workspace_id,
        connection_id: conversation.connection_id,
        external_id: conversation.external_id,
        kind: conversation.kind,
        name: conversation.name,
        is_external: conversation.is_external,
        identity_is_member: conversation.identity_is_member,
        last_message_at: conversation.last_message_at,
        paused_until: conversation.paused_until,
        created_at: conversation.created_at,
    }
}

#[derive(Serialize)]
struct MessageView {
    id: ChannelMessageId,
    workspace_id: WorkspaceId,
    connection_id: ChannelConnectionId,
    conversation_id: ChannelConversationId,
    external_id: String,
    thread_key: String,
    direction: Direction,
    author: Option<ChannelPerson>,
    text: String,
    mentions: Vec<String>,
    run_id: Option<RunId>,
    tool_call_id: Option<ToolCallId>,
    agent_hop: u32,
    sent_at: Timestamp,
    edited_at: Option<Timestamp>,
    deleted_at: Option<Timestamp>,
    created_at: Timestamp,
}

/// A deleted message reads as empty text: what it said is gone for readers.
fn message_view(message: ChannelMessage) -> MessageView {
    let text = if message.deleted_at.is_some() {
        String::new()
    } else {
        message.text
    };
    MessageView {
        id: message.id,
        workspace_id: message.workspace_id,
        connection_id: message.connection_id,
        conversation_id: message.conversation_id,
        external_id: message.external_id,
        thread_key: message.thread_key,
        direction: message.direction,
        author: message.author,
        text,
        mentions: message.mentions,
        run_id: message.run_id,
        tool_call_id: message.tool_call_id,
        agent_hop: message.agent_hop,
        sent_at: message.sent_at,
        edited_at: message.edited_at,
        deleted_at: message.deleted_at,
        created_at: message.created_at,
    }
}

#[derive(Serialize)]
struct AgentChannelView {
    id: AgentChannelId,
    workspace_id: WorkspaceId,
    agent_id: AgentId,
    connection_id: ChannelConnectionId,
    #[serde(flatten)]
    settings: AgentChannelSettings,
    created_at: Timestamp,
    updated_at: Timestamp,
}

fn agent_channel_view(channel: AgentChannel) -> AgentChannelView {
    AgentChannelView {
        id: channel.id,
        workspace_id: channel.workspace_id,
        agent_id: channel.agent_id,
        connection_id: channel.connection_id,
        settings: channel.settings,
        created_at: channel.created_at,
        updated_at: channel.updated_at,
    }
}

#[derive(Serialize)]
struct ToolLevelView {
    provider: ChannelProviderKind,
    tool: String,
    level: ToolLevel,
    confirmed: bool,
    effect: ToolEffect,
    updated_at: Option<Timestamp>,
    updated_by: Option<User>,
}

fn tool_level_view(level: ChannelToolLevel) -> ToolLevelView {
    ToolLevelView {
        provider: level.provider,
        tool: level.tool,
        level: level.level,
        confirmed: level.confirmed,
        effect: level.effect,
        updated_at: level.updated_at,
        updated_by: level.updated_by,
    }
}

#[derive(Serialize)]
struct SettingsView {
    organization_id: OrganizationId,
    breaker_runs: u32,
    breaker_window_minutes: u32,
    breaker_pause_minutes: u32,
    message_retention_days: u32,
    updated_at: Option<Timestamp>,
    updated_by: Option<User>,
}

fn settings_view(settings: ChannelSettings) -> SettingsView {
    SettingsView {
        organization_id: settings.organization_id,
        breaker_runs: settings.values.breaker_runs,
        breaker_window_minutes: settings.values.breaker_window_minutes,
        breaker_pause_minutes: settings.values.breaker_pause_minutes,
        message_retention_days: settings.values.message_retention_days,
        updated_at: settings.updated_at,
        updated_by: settings.updated_by,
    }
}

/// A page, with the contract's pagination.
fn page_response<T, V: Serialize>(page: Page<T>, view: impl Fn(T) -> V) -> Response {
    let meta = json!({ "pagination": { "next_cursor": page.next_cursor, "total": page.total } });
    envelope_with_meta(
        StatusCode::OK,
        page.items.into_iter().map(view).collect::<Vec<_>>(),
        meta,
    )
}

// --- Refusals ---------------------------------------------------------------

fn unavailable() -> Failure {
    ApiError::new(
        503,
        "unavailable",
        "The service is unavailable right now. Try again shortly.",
    )
    .into()
}

/// A refusal about one field, with the message under it too.
fn field_error(status: u16, code: &'static str, field: &str, message: String) -> Failure {
    Failure(ApiError {
        fields: Some(BTreeMap::from([(field.to_owned(), message.clone())])),
        ..ApiError::new(status, code, message)
    })
}

fn manager_inactive() -> Failure {
    ApiError::new(
        409,
        "channel_app_manager_inactive",
        "The team's configuration token has expired or was removed. Paste a new one, then try again.",
    )
    .into()
}

fn app_not_ready() -> Failure {
    ApiError::new(
        409,
        "channel_app_not_ready",
        "This app isn't ready to install. Wait until it's made, or make it again.",
    )
    .into()
}

fn connection_disconnected() -> Failure {
    ApiError::new(
        409,
        "channel_connection_disconnected",
        "This connection is disconnected. Install the agent's app again first.",
    )
    .into()
}

fn not_agents_app(agent: &str, provider: ChannelProviderKind) -> Failure {
    ApiError::new(
        422,
        "not_agents_app",
        format!(
            "This connection is another agent's app. Add {agent} to {} to give it its own.",
            provider.name()
        ),
    )
    .into()
}

pub(crate) fn app_not_eligible() -> Failure {
    ApiError::new(
        422,
        "app_not_eligible",
        "This app can't be installed any more. Start again from the agent.",
    )
    .into()
}

fn in_use(apps: u64) -> Failure {
    let message = if apps == 1 {
        "This team still has an agent's app. Remove it from the agents first.".to_owned()
    } else {
        format!("This team still has {apps} agents' apps. Remove them from the agents first.")
    };
    ApiError::new(409, "in_use", message).into()
}

/// The store's refusal as the API answers it, where the route has nothing to add.
fn channel_failure(error: ChannelError) -> Failure {
    match error {
        ChannelError::Unavailable => unavailable(),
        ChannelError::InvalidCursor => crate::workspaces::invalid_cursor(),
        ChannelError::NotAllowed => ApiError::forbidden(
            "You can no longer make this change here. Ask an organization owner for the access you need.",
        )
        .into(),
        ChannelError::ManagerNotFound => ApiError::not_found("Channel app manager").into(),
        ChannelError::AppNotFound => ApiError::not_found("Channel app").into(),
        ChannelError::ConnectionNotFound => ApiError::not_found("Channel connection").into(),
        ChannelError::ConversationNotFound => ApiError::not_found("Conversation").into(),
        ChannelError::MessageNotFound => ApiError::not_found("Message").into(),
        ChannelError::AgentNotFound => ApiError::not_found("Agent").into(),
        ChannelError::AgentChannelNotFound => ApiError::not_found("Agent channel").into(),
        ChannelError::SessionNotFound => ApiError::not_found("Connect session").into(),
        ChannelError::ManagerInactive => manager_inactive(),
        ChannelError::AppExists => {
            ApiError::new(409, "channel_app_exists", "This agent already has an app in this team.").into()
        }
        ChannelError::ManagerInUse(apps) => in_use(apps),
        ChannelError::RemovalsPending(_) => ApiError::new(
            409,
            "in_use",
            "Apps removed from this team are still being deleted. Try again in a minute; if the team's tokens \
             expired, replace them first.",
        )
        .into(),
        ChannelError::TokensChanged => ApiError::new(
            409,
            "tokens_changed",
            "The team's tokens were replaced at the same time. Check the team, then try again.",
        )
        .into(),
        ChannelError::AppNotReady => app_not_ready(),
        ChannelError::TeamMismatch => ApiError::new(
            409,
            "team_mismatch",
            "The app was installed in another team than the one linked. Install it in the linked team.",
        )
        .into(),
        ChannelError::InstallChanged => ApiError::new(
            409,
            "install_changed",
            "This app was installed again at the same time. Check its connection, then try again.",
        )
        .into(),
        ChannelError::NotAgentsApp => ApiError::new(
            422,
            "not_agents_app",
            "This connection is another agent's app. Add the agent to the team to give it its own.",
        )
        .into(),
        ChannelError::ConnectionDisconnected => connection_disconnected(),
        ChannelError::ToolDenied(tool) => tool_denied(&tool),
        ChannelError::ApprovalRequiredByPolicy(tool) => approval_required_by_policy(&tool),
        ChannelError::UnknownConversation => unknown_conversation(),
        ChannelError::SessionNotPending => {
            crate::connect::not_pending(zupflow_core::connect::ConnectSessionStatus::Completed)
        }
    }
}

fn tool_denied(tool: &str) -> Failure {
    field_error(
        422,
        "tool_denied",
        "tools",
        format!("Your organization blocks {tool}. Remove it from the channel's tools."),
    )
}

fn approval_required_by_policy(tool: &str) -> Failure {
    field_error(
        422,
        "approval_required_by_policy",
        "approval_required",
        format!("Your organization requires approval for {tool}. Add it to approval required."),
    )
}

fn unknown_conversation() -> Failure {
    field_error(
        422,
        "unknown_conversation",
        "audience_conversations",
        "One of the listed conversations isn't one this connection has seen. Pick from its conversations.".to_owned(),
    )
}

/// The provisioner couldn't check or use the tokens: refused, or not asked.
pub(crate) fn provision_failure(error: ProvisionError, provider: ChannelProviderKind) -> Failure {
    let name = provider.name();
    match error {
        ProvisionError::TokensRefused | ProvisionError::Refused(_) => field_error(
            422,
            "invalid_configuration_token",
            "configuration_token",
            format!(
                "{name} refused that configuration token. Generate a new one in {name}'s app settings and paste it \
                 with its refresh token."
            ),
        ),
        ProvisionError::NotConfigured => ApiError::new(
            501,
            "channel_providers_not_configured",
            format!(
                "This deployment can't reach {name}, so it can't link teams there. {}",
                setup_hint(provider)
            ),
        )
        .into(),
        // Checking tokens and building a consent URL name no app: answered unexpectedly.
        ProvisionError::Unavailable | ProvisionError::AppNotFound => {
            ApiError::new(503, "unavailable", format!("{name} didn't answer. Try again shortly.")).into()
        }
        ProvisionError::RateLimited { retry_after_secs } => ApiError::new(
            503,
            "unavailable",
            format!("{name} asked to wait {retry_after_secs} seconds before it's asked again. Try again then."),
        )
        .into(),
    }
}

/// What a deployment's operator does so the server reaches a provider.
fn setup_hint(provider: ChannelProviderKind) -> &'static str {
    match provider {
        ChannelProviderKind::Slack => {
            "Ask its operator to set ZUPFLOW_PUBLIC_URL to the https:// address Slack reaches it at."
        }
    }
}

/// In Production the Console only observes: the customer's product links
/// teams and makes apps there, with a key.
fn console_may_act(access: &WorkspaceAccess, action: &str) -> Result<(), Failure> {
    if matches!(access.access.actor, Actor::Member(_)) && access.workspace.environment == Environment::Production {
        return Err(ApiError::new(
            403,
            "console_read_only_in_production",
            format!("The Console can't {action} in Production. Your product does that through the API."),
        )
        .into());
    }
    Ok(())
}

/// Who makes a change, as the store checks them again in its transaction.
fn editor(access: &Access) -> ChannelEditor {
    match &access.actor {
        Actor::Member(member) => ChannelEditor::Member(member.member.id),
        Actor::ApiKey(key) => ChannelEditor::ApiKey(key.id),
    }
}

fn workspace_audit(access: &WorkspaceAccess, action: AuditAction, target: AuditTarget) -> AuditEntry {
    AuditEntry::new(
        access.access.audit_actor(),
        action,
        target,
        AuditPlace::Workspace(access.workspace.id),
    )
}

// --- Lookups, each within the request's organization and workspace ---------

fn path_id<T, E>(value: &str, parse: impl Fn(&str) -> Result<T, E>, what: &str) -> Result<T, Failure> {
    parse(value).map_err(|_| ApiError::not_found(what).into())
}

async fn find_manager(state: &AppState, access: &WorkspaceAccess, manager: &str) -> Result<ChannelAppManager, Failure> {
    let id = path_id(manager, ChannelAppManagerId::parse, "Channel app manager")?;
    state
        .channels
        .find_manager(access.access.organization_id, access.workspace.id, id)
        .await
        .map_err(channel_failure)?
        .ok_or_else(|| ApiError::not_found("Channel app manager").into())
}

async fn find_app(state: &AppState, access: &WorkspaceAccess, app: &str) -> Result<ChannelApp, Failure> {
    let id = path_id(app, ChannelAppId::parse, "Channel app")?;
    state
        .channels
        .find_app(access.access.organization_id, access.workspace.id, id)
        .await
        .map_err(channel_failure)?
        .ok_or_else(|| ApiError::not_found("Channel app").into())
}

async fn find_connection(
    state: &AppState,
    access: &WorkspaceAccess,
    connection: &str,
) -> Result<ChannelConnection, Failure> {
    let id = path_id(connection, ChannelConnectionId::parse, "Channel connection")?;
    state
        .channels
        .find_connection(access.access.organization_id, access.workspace.id, id)
        .await
        .map_err(channel_failure)?
        .ok_or_else(|| ApiError::not_found("Channel connection").into())
}

async fn find_agent(
    state: &AppState,
    access: &WorkspaceAccess,
    agent: &str,
) -> Result<zupflow_core::agents::Agent, Failure> {
    let id = path_id(agent, AgentId::parse, "Agent")?;
    state
        .agents
        .find_agent(access.access.organization_id, access.workspace.id, id)
        .await
        .map_err(|_| unavailable())?
        .ok_or_else(|| ApiError::not_found("Agent").into())
}

/// The agent's name for the audit log and refusals, read in the workspace.
async fn agent_name(
    state: &AppState,
    organization_id: OrganizationId,
    workspace_id: WorkspaceId,
    agent: AgentId,
) -> String {
    match state.agents.find_agent(organization_id, workspace_id, agent).await {
        Ok(Some(agent)) => agent.name,
        _ => "Deleted agent".to_owned(),
    }
}

// --- Query strings ------------------------------------------------------------

fn query(raw: Option<&str>) -> Result<BTreeMap<String, String>, Failure> {
    crate::runs::query(raw)
}

/// 1 to 100, 25 by default; a value that isn't an integer means the default.
fn page_limit(query: &BTreeMap<String, String>) -> u32 {
    let max = i64::from(zupflow_core::channels::MAX_PAGE);
    let fallback = i64::from(zupflow_core::channels::DEFAULT_PAGE);
    u32::try_from(crate::workspaces::clamped_int(
        query.get("limit").map(String::as_str),
        fallback,
        max,
    ))
    .unwrap_or(zupflow_core::channels::DEFAULT_PAGE)
}

fn cursor(query: &BTreeMap<String, String>) -> Option<&str> {
    query
        .get("cursor")
        .map(String::as_str)
        .filter(|cursor| !cursor.is_empty())
}

/// A filter by ID: absent or empty is no filter; a malformed ID matches nothing.
fn filter_id<T>(query: &BTreeMap<String, String>, name: &str, parse: impl Fn(&str) -> Option<T>) -> (Option<T>, bool) {
    match query.get(name).map(String::as_str).filter(|value| !value.is_empty()) {
        None => (None, false),
        Some(value) => match parse(value) {
            Some(id) => (Some(id), false),
            None => (None, true),
        },
    }
}

// --- Linked teams -------------------------------------------------------------

/// The providers this server reaches, so the workspace links teams only
/// there: the same for every workspace, read by anyone who can read one.
async fn list_providers(
    State(state): State<AppState>,
    caller: Caller,
    headers: HeaderMap,
    Path(ws): Path<String>,
) -> Result<Response, Failure> {
    WorkspaceAccess::resolve(&state, caller, &headers, &ws).await?;
    let providers = &state.channel_providers;
    let available = providers
        .available()
        .into_iter()
        .map(|kind| {
            json!({
                "provider": kind.as_str(),
                "methods": [providers.provisioner(kind).method().as_str()],
            })
        })
        .collect::<Vec<_>>();
    Ok(envelope(StatusCode::OK, available))
}

async fn list_managers(
    State(state): State<AppState>,
    caller: Caller,
    headers: HeaderMap,
    Path(ws): Path<String>,
    RawQuery(raw): RawQuery,
) -> Result<Response, Failure> {
    let access = WorkspaceAccess::resolve(&state, caller, &headers, &ws).await?;
    let query = query(raw.as_deref())?;
    let page = state
        .channels
        .list_managers(
            access.access.organization_id,
            access.workspace.id,
            cursor(&query),
            page_limit(&query),
        )
        .await
        .map_err(channel_failure)?;
    Ok(page_response(page, manager_view))
}

async fn get_manager(
    State(state): State<AppState>,
    caller: Caller,
    headers: HeaderMap,
    Path((ws, manager)): Path<(String, String)>,
) -> Result<Response, Failure> {
    let access = WorkspaceAccess::resolve(&state, caller, &headers, &ws).await?;
    let manager = find_manager(&state, &access, &manager).await?;
    Ok(envelope(StatusCode::OK, manager_view(manager)))
}

/// A pasted token: trimmed, 1 to 4,096 characters.
fn checked_token(body: &Object, field: &str, key: &str, fields: &mut Fields) -> Option<Zeroizing<String>> {
    let value = Zeroizing::new(body.string(field, fields)?);
    let token = value.trim();
    if token.is_empty() {
        fields.invalid(key, "Required.");
        None
    } else if token.chars().count() > TOKEN_MAX {
        fields.invalid(key, "Use 4,096 characters or fewer.");
        None
    } else {
        Some(Zeroizing::new(token.to_owned()))
    }
}

/// Both tokens from a body, with field errors under `prefix`.
fn tokens_from(body: &Object, prefix: &str, fields: &mut Fields) -> Option<ManagerTokens> {
    let key = |name: &str| format!("{prefix}{name}");
    let configuration_token = checked_token(body, "configuration_token", &key("configuration_token"), fields);
    let refresh_token = checked_token(body, "refresh_token", &key("refresh_token"), fields);
    Some(ManagerTokens {
        configuration_token: configuration_token?,
        refresh_token: refresh_token?,
    })
}

#[derive(Serialize)]
struct SealingTokens<'a> {
    configuration_token: &'a str,
    refresh_token: &'a str,
}

/// Wiped when dropped: what's kept is moved out, never copied.
#[derive(Deserialize, Zeroize, ZeroizeOnDrop)]
struct OpenedTokens {
    configuration_token: String,
    refresh_token: String,
}

/// A workspace with its organization and its environment, as the sealing
/// contexts of its secrets name them.
#[derive(Debug, Clone, Copy)]
pub(crate) struct SealingScope {
    pub(crate) organization_id: OrganizationId,
    pub(crate) workspace_id: WorkspaceId,
    pub(crate) environment: Environment,
}

/// A team's tokens sealed together, as one value at `version`.
pub(crate) async fn seal_tokens(
    state: &AppState,
    scope: SealingScope,
    manager_id: ChannelAppManagerId,
    version: u32,
    tokens: &ManagerTokens,
) -> Result<keys::Sealed, Failure> {
    let plaintext = Zeroizing::new(
        serde_json::to_vec(&SealingTokens {
            configuration_token: &tokens.configuration_token,
            refresh_token: &tokens.refresh_token,
        })
        .map_err(|_| Failure(ApiError::internal()))?,
    );
    let SealingScope {
        organization_id,
        workspace_id,
        environment,
    } = scope;
    let context = EncryptionContext::channel_app_manager_credential(
        organization_id,
        workspace_id,
        environment,
        manager_id,
        version,
    );
    keys::seal(state.keys.as_ref(), &context, &plaintext)
        .await
        .map_err(|error| {
            tracing::error!(event = "channels.seal_failed", %organization_id, %workspace_id, %manager_id, %error);
            unavailable()
        })
}

/// A team linked with checked, sealed tokens.
async fn new_manager(
    state: &AppState,
    scope: SealingScope,
    manager_id: ChannelAppManagerId,
    provider: ChannelProviderKind,
    tokens: &ManagerTokens,
    created_by: Option<zupflow_core::ids::UserId>,
) -> Result<NewManager, Failure> {
    let provisioner = state.channel_providers.provisioner(provider);
    let checked = provisioner
        .check_tokens(state.outbound.as_ref(), provider, tokens)
        .await
        .map_err(|error| provision_failure(error, provider))?;
    let credential = seal_tokens(state, scope, manager_id, FIRST_VERSION, tokens).await?;
    Ok(NewManager {
        provider,
        method: provisioner.method(),
        credential,
        token_last4: crate::connect::last4(&tokens.configuration_token),
        token_expires_at: checked.expires_at,
        created_by,
    })
}

/// A provider by its name; anything else, or nothing, asks for one.
fn checked_provider(body: &Object, field: &str, key: &str, fields: &mut Fields) -> Option<ChannelProviderKind> {
    let provider = body
        .0
        .get(field)
        .and_then(Value::as_str)
        .and_then(ChannelProviderKind::parse);
    if provider.is_none() {
        fields.invalid(key, "Choose a provider.");
    }
    provider
}

async fn create_manager(
    State(state): State<AppState>,
    caller: Caller,
    headers: HeaderMap,
    Path(ws): Path<String>,
    body: Body,
) -> Result<Response, Failure> {
    let access = WorkspaceAccess::resolve(&state, caller, &headers, &ws).await?;
    access.access.require(MemberRole::is_developer, "link teams")?;
    console_may_act(&access, "link teams")?;
    let body = body.object()?;
    let mut fields = Fields::default();
    let provider = checked_provider(&body, "provider", "provider", &mut fields);
    let method = body.string("method", &mut fields).and_then(|value| {
        let method = ManagerMethod::parse(&value);
        if method.is_none() {
            fields.invalid("method", "Choose configuration_token.");
        }
        method
    });
    let tokens = tokens_from(&body, "", &mut fields);
    fields.finish()?;
    let (Some(provider), Some(_), Some(tokens)) = (provider, method, tokens) else {
        return Err(ApiError::internal().into());
    };
    let (organization_id, workspace_id) = (access.access.organization_id, access.workspace.id);
    let manager_id = ChannelAppManagerId::generate();
    let created_by = match &access.access.actor {
        Actor::Member(member) => Some(member.user.id),
        Actor::ApiKey(_) => None,
    };
    let manager = new_manager(
        &state,
        SealingScope {
            organization_id,
            workspace_id,
            environment: access.workspace.environment,
        },
        manager_id,
        provider,
        &tokens,
        created_by,
    )
    .await?;
    let created = state
        .channels
        .create_manager(
            organization_id,
            workspace_id,
            manager_id,
            editor(&access.access),
            &manager,
            Timestamp::now(),
            &|manager: &ChannelAppManager| {
                workspace_audit(
                    &access,
                    AuditAction::ChannelAppManagerCreated,
                    AuditTarget::new(AuditObject::ChannelAppManager(manager.id), manager.provider.name()),
                )
            },
        )
        .await
        .map_err(channel_failure)?;
    tracing::info!(event = "channels.team_linked", %organization_id, %workspace_id, manager_id = %created.id);
    Ok(envelope(StatusCode::CREATED, manager_view(created)))
}

/// What the audit log calls a linked team: its name once learned.
fn team_name(manager: &ChannelAppManager) -> String {
    manager
        .team
        .name
        .clone()
        .unwrap_or_else(|| manager.provider.name().to_owned())
}

async fn update_manager(
    State(state): State<AppState>,
    caller: Caller,
    headers: HeaderMap,
    Path((ws, manager)): Path<(String, String)>,
    body: Body,
) -> Result<Response, Failure> {
    let access = WorkspaceAccess::resolve(&state, caller, &headers, &ws).await?;
    access
        .access
        .require(MemberRole::is_developer, "replace a team's tokens")?;
    console_may_act(&access, "replace a team's tokens")?;
    let manager = find_manager(&state, &access, &manager).await?;
    let body = body.object()?;
    let mut fields = Fields::default();
    let tokens = tokens_from(&body, "", &mut fields);
    fields.finish()?;
    let Some(tokens) = tokens else {
        return Err(ApiError::internal().into());
    };
    let checked = state
        .channel_providers
        .provisioner(manager.provider)
        .check_tokens(state.outbound.as_ref(), manager.provider, &tokens)
        .await
        .map_err(|error| provision_failure(error, manager.provider))?;
    let (organization_id, workspace_id) = (access.access.organization_id, access.workspace.id);
    let version = manager.credential_version.saturating_add(1);
    let credential = seal_tokens(
        &state,
        SealingScope {
            organization_id,
            workspace_id,
            environment: access.workspace.environment,
        },
        manager.id,
        version,
        &tokens,
    )
    .await?;
    let replaced = state
        .channels
        .replace_manager_tokens(
            organization_id,
            workspace_id,
            manager.id,
            Some(editor(&access.access)),
            &TokenReplacement {
                expected_version: manager.credential_version,
                credential,
                token_last4: crate::connect::last4(&tokens.configuration_token),
                token_expires_at: checked.expires_at,
            },
            Timestamp::now(),
            &|manager: &ChannelAppManager| {
                workspace_audit(
                    &access,
                    AuditAction::ChannelAppManagerUpdated,
                    AuditTarget::new(AuditObject::ChannelAppManager(manager.id), team_name(manager)),
                )
            },
        )
        .await
        .map_err(channel_failure)?;
    tracing::info!(event = "channels.tokens_replaced", %organization_id, %workspace_id, manager_id = %replaced.id);
    Ok(envelope(StatusCode::OK, manager_view(replaced)))
}

async fn delete_manager(
    State(state): State<AppState>,
    caller: Caller,
    headers: HeaderMap,
    Path((ws, manager)): Path<(String, String)>,
) -> Result<Response, Failure> {
    let access = WorkspaceAccess::resolve(&state, caller, &headers, &ws).await?;
    access.access.require(MemberRole::is_developer, "unlink teams")?;
    let manager = find_manager(&state, &access, &manager).await?;
    state
        .channels
        .delete_manager(
            access.access.organization_id,
            access.workspace.id,
            manager.id,
            editor(&access.access),
            &|manager: &ChannelAppManager| {
                workspace_audit(
                    &access,
                    AuditAction::ChannelAppManagerDeleted,
                    AuditTarget::new(AuditObject::ChannelAppManager(manager.id), team_name(manager)),
                )
            },
        )
        .await
        .map_err(channel_failure)?;
    Ok(StatusCode::NO_CONTENT.into_response())
}

// --- Generated apps -------------------------------------------------------------

async fn list_apps(
    State(state): State<AppState>,
    caller: Caller,
    headers: HeaderMap,
    Path(ws): Path<String>,
    RawQuery(raw): RawQuery,
) -> Result<Response, Failure> {
    let access = WorkspaceAccess::resolve(&state, caller, &headers, &ws).await?;
    let query = query(raw.as_deref())?;
    let (agent_id, bad_agent) = filter_id(&query, "agent_id", |value| AgentId::parse(value).ok());
    let (manager_id, bad_manager) = filter_id(&query, "manager_id", |value| ChannelAppManagerId::parse(value).ok());
    let filter = AppFilter {
        agent_id,
        manager_id,
        matches_nothing: bad_agent || bad_manager,
    };
    let page = state
        .channels
        .list_apps(
            access.access.organization_id,
            access.workspace.id,
            &filter,
            cursor(&query),
            page_limit(&query),
        )
        .await
        .map_err(channel_failure)?;
    Ok(page_response(page, app_view))
}

async fn get_app(
    State(state): State<AppState>,
    caller: Caller,
    headers: HeaderMap,
    Path((ws, app)): Path<(String, String)>,
) -> Result<Response, Failure> {
    let access = WorkspaceAccess::resolve(&state, caller, &headers, &ws).await?;
    let app = find_app(&state, &access, &app).await?;
    Ok(envelope(StatusCode::OK, app_view(app)))
}

fn provision_kind() -> Result<Name, Failure> {
    Name::new(PROVISION_CHANNEL_APP).map_err(|_| {
        tracing::error!(event = "channels.kind_invalid", kind = PROVISION_CHANNEL_APP);
        Failure(ApiError::internal())
    })
}

/// The job that makes an app, or cleans up after one.
fn provision_job(kind: &Name, app_id: ChannelAppId) -> QueuedJob {
    QueuedJob {
        kind: kind.clone(),
        payload: json!({ "app_id": app_id.to_string() }).to_string().into_bytes(),
        options: JobOptions {
            delay: Duration::ZERO,
            max_attempts: ATTEMPTS,
        },
    }
}

async fn create_app(
    State(state): State<AppState>,
    caller: Caller,
    headers: HeaderMap,
    Path(ws): Path<String>,
    body: Body,
) -> Result<Response, Failure> {
    let access = WorkspaceAccess::resolve(&state, caller, &headers, &ws).await?;
    access.access.require(MemberRole::is_developer, "add agents to teams")?;
    console_may_act(&access, "add agents to teams")?;
    let body = body.object()?;
    let mut fields = Fields::default();
    let agent_id = body.string("agent_id", &mut fields);
    let manager_id = body.string("manager_id", &mut fields);
    fields.finish()?;
    let (Some(agent_id), Some(manager_id)) = (agent_id, manager_id) else {
        return Err(ApiError::internal().into());
    };
    // As the mock reads them: the agent, then the team, each within the workspace.
    let agent = find_agent(&state, &access, &agent_id).await?;
    let manager = find_manager(&state, &access, &manager_id).await?;
    if manager.status != ManagerStatus::Active {
        return Err(manager_inactive());
    }
    let (organization_id, workspace_id) = (access.access.organization_id, access.workspace.id);
    let app_id = ChannelAppId::generate();
    let inbound_ref = InboundRef::generate().map_err(|_| {
        tracing::error!(event = "channels.random_unavailable");
        Failure(ApiError::internal())
    })?;
    let job = provision_job(&provision_kind()?, app_id);
    let created = state
        .channels
        .create_app(
            organization_id,
            workspace_id,
            app_id,
            editor(&access.access),
            &NewApp {
                agent_id: agent.id,
                manager_id: manager.id,
                inbound_ref,
            },
            &job,
            Timestamp::now(),
            &|app: &ChannelApp| {
                workspace_audit(
                    &access,
                    AuditAction::ChannelAppCreated,
                    AuditTarget::new(AuditObject::ChannelApp(app.id), &agent.name),
                )
            },
        )
        .await
        .map_err(|error| match error {
            ChannelError::AppExists => ApiError::new(
                409,
                "channel_app_exists",
                format!("{} already has an app in this team.", agent.name),
            )
            .into(),
            other => channel_failure(other),
        })?;
    tracing::info!(event = "channels.app_requested", %organization_id, %workspace_id, app_id = %created.id);
    // Accepted: the job makes it with the provider. A worker is woken for it
    // now rather than at its next poll; it claims the job with a lease, like
    // any worker, so a job is still worked once at a time.
    let woken = state.clone();
    tokio::spawn(async move { work_once(&woken).await });
    Ok(envelope(StatusCode::ACCEPTED, app_view(created)))
}

async fn delete_app(
    State(state): State<AppState>,
    caller: Caller,
    headers: HeaderMap,
    Path((ws, app)): Path<(String, String)>,
) -> Result<Response, Failure> {
    let access = WorkspaceAccess::resolve(&state, caller, &headers, &ws).await?;
    access
        .access
        .require(MemberRole::is_developer, "remove agents from teams")?;
    let app = find_app(&state, &access, &app).await?;
    let (organization_id, workspace_id) = (access.access.organization_id, access.workspace.id);
    let name = agent_name(&state, organization_id, workspace_id, app.agent_id).await;
    state
        .channels
        .delete_app(
            organization_id,
            workspace_id,
            app.id,
            editor(&access.access),
            Timestamp::now(),
            &|app: &ChannelApp| {
                workspace_audit(
                    &access,
                    AuditAction::ChannelAppDeleted,
                    AuditTarget::new(AuditObject::ChannelApp(app.id), &name),
                )
            },
        )
        .await
        .map_err(channel_failure)?;
    tracing::info!(event = "channels.app_deleted", %organization_id, %workspace_id, app_id = %app.id);
    // Its removal at the provider was queued with the deletion: a worker is
    // woken for it now, so its team can be unlinked soon after.
    let woken = state.clone();
    tokio::spawn(async move { work_once(&woken).await });
    Ok(StatusCode::NO_CONTENT.into_response())
}

// --- Connections and what they've seen ---------------------------------------------

async fn list_connections(
    State(state): State<AppState>,
    caller: Caller,
    headers: HeaderMap,
    Path(ws): Path<String>,
    RawQuery(raw): RawQuery,
) -> Result<Response, Failure> {
    let access = WorkspaceAccess::resolve(&state, caller, &headers, &ws).await?;
    let query = query(raw.as_deref())?;
    let (agent_id, bad_agent) = filter_id(&query, "agent_id", |value| AgentId::parse(value).ok());
    let filter = ConnectionFilter {
        agent_id,
        matches_nothing: bad_agent,
    };
    let page = state
        .channels
        .list_connections(
            access.access.organization_id,
            access.workspace.id,
            &filter,
            cursor(&query),
            page_limit(&query),
        )
        .await
        .map_err(channel_failure)?;
    Ok(page_response(page, connection_view))
}

async fn get_connection(
    State(state): State<AppState>,
    caller: Caller,
    headers: HeaderMap,
    Path((ws, connection)): Path<(String, String)>,
) -> Result<Response, Failure> {
    let access = WorkspaceAccess::resolve(&state, caller, &headers, &ws).await?;
    let connection = find_connection(&state, &access, &connection).await?;
    Ok(envelope(StatusCode::OK, connection_view(connection)))
}

async fn update_connection(
    State(state): State<AppState>,
    caller: Caller,
    headers: HeaderMap,
    Path((ws, connection)): Path<(String, String)>,
    body: Body,
) -> Result<Response, Failure> {
    let access = WorkspaceAccess::resolve(&state, caller, &headers, &ws).await?;
    access
        .access
        .require(MemberRole::is_developer, "change channel connections")?;
    let connection = find_connection(&state, &access, &connection).await?;
    let body = body.object()?;
    let mut fields = Fields::default();
    let label = body.optional_string("label", &mut fields).and_then(|label| {
        let label = label.trim().to_owned();
        if label.is_empty() {
            fields.invalid("label", "Required.");
            None
        } else if label.chars().count() > MAX_LABEL {
            fields.invalid("label", "Use 40 characters or fewer.");
            None
        } else {
            Some(label)
        }
    });
    let handler = body.optional_string("handler", &mut fields).and_then(|value| {
        let handler = HandlerKind::parse(&value);
        if handler.is_none() {
            fields.invalid("handler", "Choose mention.");
        }
        handler
    });
    fields.finish()?;
    let updated = state
        .channels
        .update_connection(
            access.access.organization_id,
            access.workspace.id,
            connection.id,
            editor(&access.access),
            &ConnectionChanges { label, handler },
            Timestamp::now(),
            &|connection: &ChannelConnection| {
                workspace_audit(
                    &access,
                    AuditAction::ChannelConnectionUpdated,
                    AuditTarget::new(AuditObject::ChannelConnection(connection.id), &connection.label),
                )
            },
        )
        .await
        .map_err(channel_failure)?;
    Ok(envelope(StatusCode::OK, connection_view(updated)))
}

async fn delete_connection(
    State(state): State<AppState>,
    caller: Caller,
    headers: HeaderMap,
    Path((ws, connection)): Path<(String, String)>,
) -> Result<Response, Failure> {
    let access = WorkspaceAccess::resolve(&state, caller, &headers, &ws).await?;
    access.access.require(MemberRole::is_developer, "disconnect channels")?;
    let connection = find_connection(&state, &access, &connection).await?;
    let (organization_id, workspace_id) = (access.access.organization_id, access.workspace.id);
    // Its token, opened before the store forgets it, to revoke at the provider after.
    let token = connection_token(&state, &access, &connection).await;
    state
        .channels
        .delete_connection(
            organization_id,
            workspace_id,
            connection.id,
            editor(&access.access),
            Timestamp::now(),
            &|connection: &ChannelConnection| {
                workspace_audit(
                    &access,
                    AuditAction::ChannelConnectionDeleted,
                    AuditTarget::new(AuditObject::ChannelConnection(connection.id), &connection.label),
                )
            },
        )
        .await
        .map_err(channel_failure)?;
    tracing::info!(event = "channels.connection_deleted", %organization_id, %workspace_id, connection_id = %connection.id);
    if let Some(token) = token
        && let Err(error) = state
            .channel_providers
            .provisioner(connection.provider)
            .revoke_install(state.outbound.as_ref(), &token)
            .await
    {
        // Removed here all the same; at the provider, the app stays installed until someone removes it there.
        tracing::warn!(event = "channels.token_not_revoked", %organization_id, %workspace_id, connection_id = %connection.id, %error);
    }
    Ok(StatusCode::NO_CONTENT.into_response())
}

/// What a connection's sealed credential holds: its token. Wiped when
/// dropped: what's kept is moved out, never copied.
#[derive(Deserialize, Zeroize, ZeroizeOnDrop)]
pub(crate) struct OpenedConnection {
    pub(crate) token: String,
}

/// A connection's current token, opened, or `None` when it has none or it
/// can't be read (logged: the connection is removed all the same).
async fn connection_token(
    state: &AppState,
    access: &WorkspaceAccess,
    connection: &ChannelConnection,
) -> Option<Zeroizing<String>> {
    let (organization_id, workspace_id) = (access.access.organization_id, access.workspace.id);
    let sealed = match state
        .channels
        .connection_credential(organization_id, workspace_id, connection.id)
        .await
    {
        Ok(sealed) => sealed?,
        Err(error) => {
            tracing::warn!(event = "channels.token_unreadable", %organization_id, %workspace_id, connection_id = %connection.id, %error);
            return None;
        }
    };
    let context = EncryptionContext::channel_connection_credential(
        organization_id,
        workspace_id,
        access.workspace.environment,
        connection.id,
        sealed.version,
    );
    let opened = keys::open(state.keys.as_ref(), &context, &sealed.sealed).await.ok();
    let token = opened
        .and_then(|plaintext| serde_json::from_slice::<OpenedConnection>(plaintext.expose()).ok())
        .map(|mut opened| Zeroizing::new(std::mem::take(&mut opened.token)));
    if token.is_none() {
        tracing::warn!(event = "channels.token_unopenable", %organization_id, %workspace_id, connection_id = %connection.id);
    }
    token
}

async fn list_conversations(
    State(state): State<AppState>,
    caller: Caller,
    headers: HeaderMap,
    Path((ws, connection)): Path<(String, String)>,
    RawQuery(raw): RawQuery,
) -> Result<Response, Failure> {
    let access = WorkspaceAccess::resolve(&state, caller, &headers, &ws).await?;
    let connection = find_connection(&state, &access, &connection).await?;
    let query = query(raw.as_deref())?;
    let kind = match query.get("kind") {
        None => None,
        Some(value) => Some(ConversationKind::parse(value).ok_or_else(|| {
            let message = "That isn't a kind of conversation. Use channel, private_channel, dm or group_dm.";
            field_error(400, "validation_failed", "kind", message.to_owned())
        })?),
    };
    let filter = ConversationFilter {
        q: query.get("q").map(|q| q.trim().to_owned()).filter(|q| !q.is_empty()),
        kind,
    };
    let page = state
        .channels
        .list_conversations(
            access.access.organization_id,
            access.workspace.id,
            connection.id,
            &filter,
            cursor(&query),
            page_limit(&query),
        )
        .await
        .map_err(channel_failure)?;
    Ok(page_response(page, conversation_view))
}

async fn list_messages(
    State(state): State<AppState>,
    caller: Caller,
    headers: HeaderMap,
    Path((ws, connection, conversation)): Path<(String, String, String)>,
    RawQuery(raw): RawQuery,
) -> Result<Response, Failure> {
    let access = WorkspaceAccess::resolve(&state, caller, &headers, &ws).await?;
    let connection = find_connection(&state, &access, &connection).await?;
    let conversation = path_id(&conversation, ChannelConversationId::parse, "Conversation")?;
    let (organization_id, workspace_id) = (access.access.organization_id, access.workspace.id);
    let conversation = state
        .channels
        .find_conversation(organization_id, workspace_id, connection.id, conversation)
        .await
        .map_err(channel_failure)?
        .ok_or_else(|| Failure(ApiError::not_found("Conversation")))?;
    let query = query(raw.as_deref())?;
    // An empty thread is a DM's top level, not "no filter".
    let filter = MessageFilter {
        thread: query.get("thread").cloned(),
    };
    let page = state
        .channels
        .list_messages(
            organization_id,
            workspace_id,
            connection.id,
            conversation.id,
            &filter,
            cursor(&query),
            page_limit(&query),
        )
        .await
        .map_err(channel_failure)?;
    Ok(page_response(page, message_view))
}

// --- Agent channels ----------------------------------------------------------------

async fn list_agent_channels(
    State(state): State<AppState>,
    caller: Caller,
    headers: HeaderMap,
    Path((ws, agent)): Path<(String, String)>,
) -> Result<Response, Failure> {
    let access = WorkspaceAccess::resolve(&state, caller, &headers, &ws).await?;
    let agent = path_id(&agent, AgentId::parse, "Agent")?;
    let channels = state
        .channels
        .list_agent_channels(access.access.organization_id, access.workspace.id, agent)
        .await
        .map_err(channel_failure)?;
    Ok(envelope(
        StatusCode::OK,
        channels.into_iter().map(agent_channel_view).collect::<Vec<_>>(),
    ))
}

/// A list of strings, or a field error.
fn strings(body: &Object, field: &str, message: &str, fields: &mut Fields) -> Option<Vec<String>> {
    match body.0.get(field) {
        None => {
            fields.invalid(field, "Required.");
            None
        }
        Some(Value::Array(items)) => {
            let values: Option<Vec<String>> = items.iter().map(|item| item.as_str().map(str::to_owned)).collect();
            if values.is_none() {
                fields.invalid(field, message);
            }
            values
        }
        Some(_) => {
            fields.invalid(field, message);
            None
        }
    }
}

fn boolean(body: &Object, field: &str, fields: &mut Fields) -> Option<bool> {
    match body.0.get(field) {
        Some(Value::Bool(value)) => Some(*value),
        None => {
            fields.invalid(field, "Required.");
            None
        }
        Some(_) => {
            fields.invalid(field, "Must be true or false.");
            None
        }
    }
}

/// One of a closed set of values, by its name.
fn named<T>(
    body: &Object,
    field: &str,
    parse: impl Fn(&str) -> Option<T>,
    choose: &str,
    fields: &mut Fields,
) -> Option<T> {
    let value = body.string(field, fields)?;
    let parsed = parse(&value);
    if parsed.is_none() {
        fields.invalid(field, choose);
    }
    parsed
}

/// A whole number in `min..=max`.
fn whole(body: &Object, field: &str, min: u32, max: u32, range: &str, fields: &mut Fields) -> Option<u32> {
    let Some(value) = body.0.get(field) else {
        fields.invalid(field, "Required.");
        return None;
    };
    let Some(number) = value.as_f64() else {
        fields.invalid(field, "Must be a number.");
        return None;
    };
    if number.fract() != 0.0 {
        fields.invalid(field, "Use a whole number.");
        return None;
    }
    if number < f64::from(min) || number > f64::from(max) {
        fields.invalid(field, range);
        return None;
    }
    // Whole and in range: `3` and `3.0` alike.
    (min..=max).find(|candidate| f64::from(*candidate) == number)
}

fn persona(body: &Object, fields: &mut Fields) -> Option<Option<Persona>> {
    match body.0.get("persona") {
        None => {
            fields.invalid("persona", "Required; send null for none.");
            None
        }
        Some(Value::Null) => Some(None),
        Some(Value::Object(persona)) => {
            let name = match persona.get("name") {
                Some(Value::String(name)) => {
                    let name = name.trim();
                    if name.is_empty() {
                        fields.invalid("persona.name", "Required.");
                        None
                    } else if name.chars().count() > PERSONA_NAME_MAX {
                        fields.invalid("persona.name", "Use 80 characters or fewer.");
                        None
                    } else {
                        Some(name.to_owned())
                    }
                }
                _ => {
                    fields.invalid("persona.name", "Required.");
                    None
                }
            };
            let avatar_url = match persona.get("avatar_url") {
                Some(Value::Null) => Some(None),
                Some(Value::String(url))
                    if url
                        .strip_prefix("https://")
                        .is_some_and(|rest| !rest.is_empty() && !rest.chars().any(char::is_whitespace)) =>
                {
                    Some(Some(url.clone()))
                }
                _ => {
                    fields.invalid("persona.avatar_url", "Use an https:// URL.");
                    None
                }
            };
            Some(Some(Persona {
                name: name?,
                avatar_url: avatar_url?,
            }))
        }
        Some(_) => {
            fields.invalid("persona", "Must be an object or null.");
            None
        }
    }
}

/// The body's settings, in their shape only: the rules between them are
/// checked after, in the mock's order.
struct SettingsInput {
    settings: AgentChannelSettings,
    /// As sent: an ID that can't be one of this connection's is an unknown conversation.
    audience_conversations: Vec<String>,
}

fn settings_input(body: &Object, fields: &mut Fields) -> Option<SettingsInput> {
    let tools = strings(body, "tools", "Must be a list of tool names.", fields);
    let approval_required = strings(body, "approval_required", "Must be a list of tool names.", fields);
    let audience = named(
        body,
        "audience",
        Audience::parse,
        "Choose origin, listed or anywhere.",
        fields,
    );
    let audience_conversations = strings(
        body,
        "audience_conversations",
        "Must be a list of conversation IDs.",
        fields,
    );
    let persona = persona(body, fields);
    let answers_dms = boolean(body, "answers_dms", fields);
    let answers_mentions = boolean(body, "answers_mentions", fields);
    let thread_follow_up = named(
        body,
        "thread_follow_up",
        ThreadFollowUp::parse,
        "Choose off, when_last_addressed or always.",
        fields,
    );
    let agent_messages = named(
        body,
        "agent_messages",
        AgentMessages::parse,
        "Choose ignore or mentions.",
        fields,
    );
    let max_agent_hops = whole(
        body,
        "max_agent_hops",
        MIN_AGENT_HOPS,
        MAX_AGENT_HOPS,
        "Use 1 to 5.",
        fields,
    );
    let mid_run = named(body, "mid_run", MidRun::parse, "Choose steer or collect.", fields);
    let senders = named(body, "senders", Senders::parse, "Choose anyone or listed.", fields);
    let sender_list = strings(body, "sender_list", "Must be a list of email addresses.", fields).filter(|list| {
        if list.len() > SENDERS_MAX {
            fields.invalid(
                "sender_list",
                "List 500 addresses or fewer. To let more people in, use anyone.",
            );
            return false;
        }
        if list.iter().any(|email| email.chars().count() > EMAIL_MAX) {
            fields.invalid("sender_list", "Use 320 characters or fewer.");
            return false;
        }
        if list.iter().any(|email| !crate::organizations::valid_email(email)) {
            fields.invalid("sender_list", "Enter an email address.");
            return false;
        }
        true
    });
    let allow_automated = boolean(body, "allow_automated", fields);
    let allow_external = boolean(body, "allow_external", fields);
    let reply_with_output = boolean(body, "reply_with_output", fields);
    Some(SettingsInput {
        settings: AgentChannelSettings {
            tools: tools?,
            approval_required: approval_required?,
            audience: audience?,
            audience_conversations: Vec::new(),
            persona: persona?,
            answers_dms: answers_dms?,
            answers_mentions: answers_mentions?,
            thread_follow_up: thread_follow_up?,
            agent_messages: agent_messages?,
            max_agent_hops: max_agent_hops?,
            mid_run: mid_run?,
            senders: senders?,
            sender_list: sender_list?,
            allow_automated: allow_automated?,
            allow_external: allow_external?,
            reply_with_output: reply_with_output?,
        },
        audience_conversations: audience_conversations?,
    })
}

/// Each value once, in the order first sent.
fn once(values: Vec<String>) -> Vec<String> {
    let mut seen = HashSet::new();
    values.into_iter().filter(|value| seen.insert(value.clone())).collect()
}

/// The rules an agent channel's settings hold to beyond their shape, in the
/// mock's order. The levels and conversations are read here to answer as the
/// mock does; the store checks them again in the transaction that writes.
async fn checked_settings(
    state: &AppState,
    access: &WorkspaceAccess,
    connection: &ChannelConnection,
    input: SettingsInput,
) -> Result<AgentChannelSettings, Failure> {
    let mut settings = input.settings;
    settings.tools = once(settings.tools);
    settings.approval_required = once(settings.approval_required);
    if let Some(tool) = settings.tools.iter().find(|tool| channel_tool(tool).is_none()) {
        return Err(field_error(
            422,
            "unknown_tool",
            "tools",
            format!("{tool} isn't a channel tool. Use the names channel tool levels list."),
        ));
    }
    if let Some(tool) = settings
        .approval_required
        .iter()
        .find(|tool| !settings.tools.contains(tool))
    {
        return Err(field_error(
            422,
            "approval_outside_grant",
            "approval_required",
            format!("{tool} requires approval but isn't granted."),
        ));
    }
    let organization_id = access.access.organization_id;
    let levels = state
        .channels
        .list_tool_levels(organization_id)
        .await
        .map_err(channel_failure)?;
    let level_of = |tool: &str| {
        levels
            .iter()
            .find(|level| level.provider == connection.provider && level.tool == tool)
            .map_or(ToolLevel::Deny, |level| level.level)
    };
    if let Some(tool) = settings.tools.iter().find(|tool| level_of(tool) == ToolLevel::Deny) {
        return Err(tool_denied(tool));
    }
    if let Some(tool) = settings
        .tools
        .iter()
        .find(|tool| level_of(tool) == ToolLevel::Approve && !settings.approval_required.contains(tool))
    {
        return Err(approval_required_by_policy(tool));
    }
    settings.sender_list = settings
        .sender_list
        .iter()
        .map(|email| email.trim().to_owned())
        .collect();
    if settings.senders == Senders::Listed && settings.sender_list.is_empty() {
        return Err(field_error(
            422,
            "sender_list_required",
            "sender_list",
            "Add the email addresses of the people who may talk to the agent.".to_owned(),
        ));
    }
    let mut seen = HashSet::new();
    if let Some(email) = settings
        .sender_list
        .iter()
        .find(|email| !seen.insert(email.to_lowercase()))
    {
        return Err(field_error(
            422,
            "duplicate_sender",
            "sender_list",
            format!("{email} is listed twice. List each address once."),
        ));
    }
    let mut conversations = Vec::new();
    for id in once(input.audience_conversations) {
        let Ok(id) = ChannelConversationId::parse(&id) else {
            return Err(unknown_conversation());
        };
        let found = state
            .channels
            .find_conversation(organization_id, access.workspace.id, connection.id, id)
            .await
            .map_err(channel_failure)?;
        if found.is_none() {
            return Err(unknown_conversation());
        }
        conversations.push(id);
    }
    settings.audience_conversations = conversations;
    Ok(settings)
}

async fn put_agent_channel(
    State(state): State<AppState>,
    caller: Caller,
    headers: HeaderMap,
    Path((ws, agent, connection)): Path<(String, String, String)>,
    body: Body,
) -> Result<Response, Failure> {
    let access = WorkspaceAccess::resolve(&state, caller, &headers, &ws).await?;
    access
        .access
        .require(MemberRole::is_developer, "change agents' channels")?;
    let agent = find_agent(&state, &access, &agent).await?;
    let connection = find_connection(&state, &access, &connection).await?;
    let body = body.object()?;
    // As the mock answers: whose connection it is, and whether it's connected, before the settings.
    if connection.agent_id != Some(agent.id) {
        return Err(not_agents_app(&agent.name, connection.provider));
    }
    if connection.status == ConnectionStatus::Disconnected {
        return Err(connection_disconnected());
    }
    let mut fields = Fields::default();
    let input = settings_input(&body, &mut fields);
    fields.finish()?;
    let Some(input) = input else {
        return Err(ApiError::internal().into());
    };
    let settings = checked_settings(&state, &access, &connection, input).await?;
    let label = connection.label.clone();
    let saved = state
        .channels
        .put_agent_channel(
            access.access.organization_id,
            access.workspace.id,
            agent.id,
            connection.id,
            editor(&access.access),
            &settings,
            Timestamp::now(),
            &|channel: &AgentChannel| {
                workspace_audit(
                    &access,
                    AuditAction::AgentChannelUpdated,
                    AuditTarget::new(
                        AuditObject::AgentChannel(channel.id),
                        format!("{} · {label}", agent.name),
                    ),
                )
            },
        )
        .await
        .map_err(|error| match error {
            ChannelError::NotAgentsApp => not_agents_app(&agent.name, connection.provider),
            other => channel_failure(other),
        })?;
    Ok(envelope(StatusCode::OK, agent_channel_view(saved)))
}

async fn delete_agent_channel(
    State(state): State<AppState>,
    caller: Caller,
    headers: HeaderMap,
    Path((ws, agent, connection)): Path<(String, String, String)>,
) -> Result<Response, Failure> {
    let access = WorkspaceAccess::resolve(&state, caller, &headers, &ws).await?;
    access
        .access
        .require(MemberRole::is_developer, "change agents' channels")?;
    let agent = find_agent(&state, &access, &agent).await?;
    let connection = find_connection(&state, &access, &connection).await?;
    let label = connection.label.clone();
    state
        .channels
        .delete_agent_channel(
            access.access.organization_id,
            access.workspace.id,
            agent.id,
            connection.id,
            editor(&access.access),
            &|channel: &AgentChannel| {
                workspace_audit(
                    &access,
                    AuditAction::AgentChannelDeleted,
                    AuditTarget::new(
                        AuditObject::AgentChannel(channel.id),
                        format!("{} · {label}", agent.name),
                    ),
                )
            },
        )
        .await
        .map_err(channel_failure)?;
    Ok(StatusCode::NO_CONTENT.into_response())
}

// --- The organization's levels and settings -------------------------------------------

async fn list_tool_levels(
    State(state): State<AppState>,
    caller: Caller,
    Path(org): Path<String>,
) -> Result<Response, Failure> {
    let access = Access::organization(&state, caller, &org).await?;
    let levels = state
        .channels
        .list_tool_levels(access.organization_id)
        .await
        .map_err(channel_failure)?;
    Ok(envelope(
        StatusCode::OK,
        levels.into_iter().map(tool_level_view).collect::<Vec<_>>(),
    ))
}

/// `UpdateChannelToolLevelsInput`'s levels in their shape: 1 to 14, each a
/// tool name and a level. Every problem is reported under `levels`.
fn levels_input(body: &Object, fields: &mut Fields) -> Option<Vec<(String, ToolLevel)>> {
    let Some(value) = body.0.get("levels") else {
        fields.invalid("levels", "Required.");
        return None;
    };
    let Value::Array(items) = value else {
        fields.invalid("levels", "Must be a list of levels.");
        return None;
    };
    if items.is_empty() {
        fields.invalid("levels", "Set at least one level.");
        return None;
    }
    if items.len() > CHANNEL_TOOLS.len() {
        fields.invalid("levels", &format!("Set {} levels or fewer.", CHANNEL_TOOLS.len()));
        return None;
    }
    let mut levels = Vec::with_capacity(items.len());
    for item in items {
        let tool = item.get("tool").and_then(Value::as_str);
        let level = item.get("level").and_then(Value::as_str).and_then(ToolLevel::parse);
        match (tool, level) {
            (None, _) => {
                fields.invalid("levels", "Name the tool.");
                return None;
            }
            (_, None) => {
                fields.invalid("levels", "Choose allow, approve or deny.");
                return None;
            }
            (Some(tool), Some(level)) => levels.push((tool.to_owned(), level)),
        }
    }
    Some(levels)
}

async fn update_tool_levels(
    State(state): State<AppState>,
    ConsoleUser(user): ConsoleUser,
    Path(org): Path<String>,
    body: Body,
) -> Result<Response, Failure> {
    let access = OrganizationAccess::resolve(&state, user, &org).await?;
    access.require(MemberRole::is_admin, "set channel tool levels")?;
    let body = body.object()?;
    let mut fields = Fields::default();
    let provider = checked_provider(&body, "provider", "provider", &mut fields);
    let levels = levels_input(&body, &mut fields);
    fields.finish()?;
    let (Some(provider), Some(levels)) = (provider, levels) else {
        return Err(ApiError::internal().into());
    };
    // In order, as the mock checks them: a tool that isn't one, then one named twice.
    let mut named: Vec<&str> = Vec::with_capacity(levels.len());
    for (tool, _) in &levels {
        if channel_tool(tool).is_none() {
            return Err(field_error(
                422,
                "unknown_tool",
                "levels",
                format!("{tool} isn't a channel tool. Use the names this list answers with."),
            ));
        }
        if named.contains(&tool.as_str()) {
            return Err(ApiError::validation(BTreeMap::from([(
                "levels".to_owned(),
                "Name each tool once.".to_owned(),
            )]))
            .into());
        }
        named.push(tool);
    }
    let changes: Vec<LevelChange> = levels
        .into_iter()
        .map(|(tool, level)| LevelChange { tool, level })
        .collect();
    let set = state
        .channels
        .set_tool_levels(
            access.organization.id,
            access.member.id,
            provider,
            &changes,
            access.user.id,
            Timestamp::now(),
            &|_: &Vec<ChannelToolLevel>| {
                AuditEntry::new(
                    access.audit_actor(),
                    AuditAction::ChannelToolLevelsUpdated,
                    AuditTarget::new(AuditObject::ChannelToolLevels, format!("{} tools", provider.name())),
                    AuditPlace::Organization,
                )
            },
        )
        .await
        .map_err(channel_failure)?;
    tracing::info!(
        event = "channels.tool_levels_set",
        organization_id = %access.organization.id,
        user_id = %access.user.id,
        levels = changes.len(),
    );
    Ok(envelope(
        StatusCode::OK,
        set.into_iter().map(tool_level_view).collect::<Vec<_>>(),
    ))
}

async fn get_settings(
    State(state): State<AppState>,
    caller: Caller,
    Path(org): Path<String>,
) -> Result<Response, Failure> {
    let access = Access::organization(&state, caller, &org).await?;
    let settings = state
        .channels
        .channel_settings(access.organization_id)
        .await
        .map_err(channel_failure)?;
    Ok(envelope(StatusCode::OK, settings_view(settings)))
}

async fn update_settings(
    State(state): State<AppState>,
    ConsoleUser(user): ConsoleUser,
    Path(org): Path<String>,
    body: Body,
) -> Result<Response, Failure> {
    let access = OrganizationAccess::resolve(&state, user, &org).await?;
    access.require(MemberRole::is_admin, "change channel settings")?;
    let body = body.object()?;
    let mut fields = Fields::default();
    let runs = whole(&body, "breaker_runs", 1, 200, "Use 1 to 200.", &mut fields);
    let window = whole(&body, "breaker_window_minutes", 1, 1440, "Use 1 to 1,440.", &mut fields);
    let pause = whole(&body, "breaker_pause_minutes", 1, 1440, "Use 1 to 1,440.", &mut fields);
    let retention = whole(&body, "message_retention_days", 1, 365, "Use 1 to 365.", &mut fields);
    fields.finish()?;
    let (Some(breaker_runs), Some(breaker_window_minutes), Some(breaker_pause_minutes), Some(message_retention_days)) =
        (runs, window, pause, retention)
    else {
        return Err(ApiError::internal().into());
    };
    let saved = state
        .channels
        .save_channel_settings(
            access.organization.id,
            access.member.id,
            &BreakerSettings {
                breaker_runs,
                breaker_window_minutes,
                breaker_pause_minutes,
                message_retention_days,
            },
            access.user.id,
            Timestamp::now(),
            &|_: &ChannelSettings| {
                AuditEntry::new(
                    access.audit_actor(),
                    AuditAction::ChannelSettingsUpdated,
                    AuditTarget::new(AuditObject::ChannelSettings, "Channel settings"),
                    AuditPlace::Organization,
                )
            },
        )
        .await
        .map_err(channel_failure)?;
    Ok(envelope(StatusCode::OK, settings_view(saved)))
}

// --- Channel Connect sessions ------------------------------------------------------------

/// A channel session's target, as a body names it.
enum TargetInput {
    Manager(ChannelProviderKind),
    App(String),
}

fn target_input(body: &Object, fields: &mut Fields) -> Option<TargetInput> {
    let Some(Value::Object(target)) = body.0.get("target") else {
        fields.invalid("target", "Choose channel_app_manager or channel_app.");
        return None;
    };
    let target = Object(target.clone());
    match target.0.get("kind").and_then(Value::as_str) {
        Some("channel_app_manager") => {
            checked_provider(&target, "provider", "target.provider", fields).map(TargetInput::Manager)
        }
        Some("channel_app") => match target.0.get("app") {
            Some(Value::String(app)) => Some(TargetInput::App(app.clone())),
            _ => {
                fields.invalid("target.app", "Required.");
                None
            }
        },
        _ => {
            fields.invalid("target", "Choose channel_app_manager or channel_app.");
            None
        }
    }
}

/// Starts a channel session: it links a team, or installs an agent's app
/// that's made and whose team is active. The caller's role and environment
/// were checked by `createConnectSession` before the body was read.
pub(crate) async fn create_channel_session(
    state: &AppState,
    access: &WorkspaceAccess,
    body: &Object,
) -> Result<Response, Failure> {
    let mut fields = Fields::default();
    let target = target_input(body, &mut fields);
    let return_url = match body.0.get("return_url") {
        None => {
            fields.invalid("return_url", "Required; send null for none.");
            None
        }
        Some(Value::Null) => Some(None),
        Some(Value::String(value)) if url::Url::parse(value).is_ok() => Some(Some(value.clone())),
        Some(_) => {
            fields.invalid("return_url", "Enter a full URL.");
            None
        }
    };
    fields.finish()?;
    let (Some(target), Some(return_url)) = (target, return_url) else {
        return Err(ApiError::internal().into());
    };
    let (target, label) = match target {
        TargetInput::Manager(provider) => (
            ConnectTarget::ChannelAppManager {
                provider,
                manager: None,
            },
            provider.name().to_owned(),
        ),
        TargetInput::App(app) => {
            let app = find_app(state, access, &app).await?;
            if !app.status.is_installable() {
                return Err(app_not_ready());
            }
            let manager = state
                .channels
                .find_manager(access.access.organization_id, access.workspace.id, app.manager_id)
                .await
                .map_err(channel_failure)?;
            if manager.is_none_or(|manager| manager.status != ManagerStatus::Active) {
                return Err(manager_inactive());
            }
            let agent = state
                .agents
                .find_agent(access.access.organization_id, access.workspace.id, app.agent_id)
                .await
                .map_err(|_| unavailable())?
                .ok_or_else(|| Failure(ApiError::not_found("Agent")))?;
            (
                ConnectTarget::ChannelApp {
                    provider: app.provider,
                    app: app.id,
                    connection: None,
                },
                agent.name.chars().take(MAX_LABEL).collect(),
            )
        }
    };
    crate::connect::allowed_return_url(state, access, return_url.as_deref()).await?;
    let reference = Reference::generate().map_err(crate::connect::connect_failure)?;
    let session = state
        .connect
        .create_session(
            access.access.organization_id,
            access.workspace.id,
            ConnectSessionId::generate(),
            &NewConnectSession {
                environment: access.workspace.environment,
                app: None,
                target: Some(target),
                configuration_version: 0,
                intent: ConnectIntent::Create,
                installation_id: None,
                label,
                return_url,
                requested_scopes: Vec::new(),
                reference_hash: reference.hash(),
            },
            Timestamp::now(),
        )
        .await
        .map_err(crate::connect::connect_failure)?;
    // The link is shown once, here: its reference authorizes the hosted page.
    let url = format!("{}/connect/{}", state.public_url, reference.expose());
    Ok(envelope(
        StatusCode::CREATED,
        crate::connect::session_view(state, session, false, Some(url)).await?,
    ))
}

#[derive(Serialize)]
struct ManagerConnectResult {
    session: crate::connect::SessionView,
    channel_app_manager: ManagerView,
}

/// Completes a pending team-linking session with the pasted tokens, checked
/// with the provider and sealed, as one with the session's completion. An
/// app's session is installed on the provider's page instead.
pub(crate) async fn submit_channel_credentials(
    state: &AppState,
    session: &ConnectSession,
    body: Body,
) -> Result<Response, Failure> {
    let Some(ConnectTarget::ChannelAppManager { provider, .. }) = session.target else {
        let provider = match session.target {
            Some(ConnectTarget::ChannelApp { provider, .. }) => provider,
            _ => ChannelProviderKind::Slack,
        };
        return Err(ApiError::new(
            422,
            "wrong_auth_type",
            format!(
                "This agent's app is installed by allowing it in {}, not with tokens.",
                provider.name()
            ),
        )
        .into());
    };
    let body = body.object()?;
    let mut fields = Fields::default();
    let tokens = match body.0.get("values") {
        Some(Value::Object(values)) => tokens_from(&Object(values.clone()), "values.", &mut fields),
        _ => {
            fields.invalid("values", "Required.");
            None
        }
    };
    fields.finish()?;
    let Some(tokens) = tokens else {
        return Err(ApiError::internal().into());
    };
    let (organization_id, workspace_id) = (session.organization_id, session.workspace_id);
    let manager_id = ChannelAppManagerId::generate();
    let manager = new_manager(
        state,
        SealingScope {
            organization_id,
            workspace_id,
            environment: session.environment,
        },
        manager_id,
        provider,
        &tokens,
        None,
    )
    .await?;
    let (completed, linked) = state
        .channels
        .link_team_through_session(
            organization_id,
            workspace_id,
            session.id,
            manager_id,
            &manager,
            Timestamp::now(),
            &|(_, manager): &(ConnectSession, ChannelAppManager)| {
                AuditEntry::new(
                    AuditActor::System {
                        name: "Connect".to_owned(),
                    },
                    AuditAction::ChannelAppManagerCreated,
                    AuditTarget::new(AuditObject::ChannelAppManager(manager.id), manager.provider.name()),
                    AuditPlace::Workspace(manager.workspace_id),
                )
            },
        )
        .await
        .map_err(channel_failure)?;
    tracing::info!(event = "channels.team_linked", %organization_id, %workspace_id, manager_id = %linked.id, session_id = %session.id);
    Ok(envelope(
        StatusCode::OK,
        ManagerConnectResult {
            session: crate::connect::session_view(state, completed, false, None).await?,
            channel_app_manager: manager_view(linked),
        },
    ))
}

// --- Making apps -----------------------------------------------------------------------------

/// What came of one attempt.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Outcome {
    /// Done, made or failed: trying again wouldn't help.
    Done,
    /// Worth another attempt later, with why.
    Retry(&'static str),
    /// The provider asked to wait this long (its rate limit): tried again
    /// then, without using up an attempt. Nothing was done.
    Wait(Duration),
}

/// How long the provider asked to wait.
fn wait(retry_after_secs: u32) -> Outcome {
    Outcome::Wait(Duration::from_secs(u64::from(retry_after_secs.max(1))))
}

/// The app's name, handle and description, from the agent: never a request's text.
pub fn manifest_for(agent: &zupflow_core::agents::Agent, events_url: String, redirect_url: String) -> AppManifest {
    let name: String = agent.name.trim().chars().take(MAX_APP_NAME).collect();
    let mut handle = String::new();
    for character in name.to_lowercase().chars() {
        if character.is_ascii_alphanumeric() {
            handle.push(character);
        } else if !handle.ends_with('-') {
            handle.push('-');
        }
    }
    let handle = handle.trim_matches('-');
    AppManifest {
        name,
        description: agent.description.trim().chars().take(DESCRIPTION_MAX).collect(),
        handle: if handle.is_empty() {
            "agent".to_owned()
        } else {
            handle.to_owned()
        },
        scopes: SLACK_SCOPES,
        events: SLACK_EVENTS,
        events_url,
        redirect_url,
    }
}

#[derive(Serialize)]
struct SealingSecrets<'a> {
    client_secret: &'a str,
    signing_secret: &'a str,
}

pub(crate) async fn open_tokens(
    state: &AppState,
    organization_id: OrganizationId,
    workspace_id: WorkspaceId,
    environment: Environment,
    manager_id: ChannelAppManagerId,
    credential: &SealedVersion,
) -> Option<ManagerTokens> {
    let context = EncryptionContext::channel_app_manager_credential(
        organization_id,
        workspace_id,
        environment,
        manager_id,
        credential.version,
    );
    let opened = keys::open(state.keys.as_ref(), &context, &credential.sealed)
        .await
        .map_err(|error| tracing::error!(event = "channels.open_failed", %organization_id, %workspace_id, %manager_id, %error))
        .ok()?;
    let mut tokens: OpenedTokens = serde_json::from_slice(opened.expose()).ok()?;
    Some(ManagerTokens {
        configuration_token: Zeroizing::new(std::mem::take(&mut tokens.configuration_token)),
        refresh_token: Zeroizing::new(std::mem::take(&mut tokens.refresh_token)),
    })
}

/// One app's scope, for the job that makes it.
#[derive(Debug, Clone, Copy)]
struct AppScope {
    organization_id: OrganizationId,
    workspace_id: WorkspaceId,
    app_id: ChannelAppId,
}

/// Deletes at the provider what an earlier attempt made for the app and
/// didn't record as made (its secrets are lost with that attempt), then
/// forgets it. `Err` with why when the provider can't be reached now. What
/// the provider refuses to delete or find is left there, logged.
async fn clean_up(
    state: &AppState,
    scope: AppScope,
    tokens: &ManagerTokens,
    creation: &AppCreation,
    events_url: &str,
) -> Result<(), Outcome> {
    let AppScope {
        organization_id,
        workspace_id,
        app_id,
    } = scope;
    let provisioner = state.channel_providers.provisioner(creation.app.provider);
    let http = state.outbound.as_ref();
    let recorded = creation.app.external_app_id.as_deref();
    let made = match recorded {
        Some(external_app_id) => Some(external_app_id.to_owned()),
        // Its answer was lost: found by where its events go.
        None => match provisioner.find(http, tokens, events_url).await {
            Ok(found) => found,
            Err(ProvisionError::Unavailable) => return Err(Outcome::Retry("provider_unavailable")),
            Err(ProvisionError::RateLimited { retry_after_secs }) => return Err(wait(retry_after_secs)),
            Err(error) => {
                tracing::warn!(event = "channels.orphan_not_found", %organization_id, %workspace_id, %app_id, %error);
                None
            }
        },
    };
    if let Some(external_app_id) = &made {
        match provisioner.delete(http, tokens, external_app_id).await {
            Ok(()) | Err(ProvisionError::AppNotFound) => {
                tracing::info!(event = "channels.orphan_deleted", %organization_id, %workspace_id, %app_id);
            }
            Err(ProvisionError::Unavailable) => return Err(Outcome::Retry("provider_unavailable")),
            Err(ProvisionError::RateLimited { retry_after_secs }) => return Err(wait(retry_after_secs)),
            Err(error) => {
                tracing::warn!(event = "channels.orphan_kept", %organization_id, %workspace_id, %app_id, %error);
            }
        }
    }
    state
        .channels
        .forget_app_creation(organization_id, workspace_id, app_id, recorded, Timestamp::now())
        .await
        .map_err(|_| Outcome::Retry("store_unavailable"))
}

/// What this attempt made, deleted at the provider and forgotten: for an app
/// deleted or failed meanwhile, or whose secrets couldn't be kept. Left for
/// the next attempt when the provider can't be reached.
async fn discard(
    state: &AppState,
    scope: AppScope,
    provider: ChannelProviderKind,
    tokens: &ManagerTokens,
    external_app_id: &str,
) {
    let AppScope {
        organization_id,
        workspace_id,
        app_id,
    } = scope;
    match state
        .channel_providers
        .provisioner(provider)
        .delete(state.outbound.as_ref(), tokens, external_app_id)
        .await
    {
        Ok(()) | Err(ProvisionError::AppNotFound) => {
            let forgotten = state
                .channels
                .forget_app_creation(
                    organization_id,
                    workspace_id,
                    app_id,
                    Some(external_app_id),
                    Timestamp::now(),
                )
                .await;
            if let Err(error) = forgotten {
                tracing::warn!(event = "channels.discard_not_recorded", %organization_id, %workspace_id, %app_id, %error);
            }
        }
        Err(error) => {
            tracing::warn!(event = "channels.discard_failed", %organization_id, %workspace_id, %app_id, %error);
        }
    }
}

/// Makes one app with its provider, in its own scope, at least once. What
/// an earlier attempt made and didn't record as made is deleted at the
/// provider first; then the app is marked in flight, made, its provider ID
/// recorded at once, its secrets sealed, and it's recorded as made. An app
/// deleted or failed while in flight only has what was made for it deleted.
async fn provision(
    state: &AppState,
    organization_id: OrganizationId,
    workspace_id: WorkspaceId,
    app_id: ChannelAppId,
    last_attempt: bool,
) -> Outcome {
    let scope = AppScope {
        organization_id,
        workspace_id,
        app_id,
    };
    // Failed: still in flight if it was, so a sweep queues its clean-up.
    let fail = |code: &'static str| async move {
        tracing::warn!(event = "channels.app_failed", %organization_id, %workspace_id, %app_id, code);
        match state
            .channels
            .record_app_failed(organization_id, workspace_id, app_id, code, Timestamp::now())
            .await
        {
            Ok(()) | Err(ChannelError::AppNotReady | ChannelError::AppNotFound) => Outcome::Done,
            Err(_) => Outcome::Retry("store_unavailable"),
        }
    };
    let retry = |why: &'static str, code: &'static str| async move {
        if last_attempt {
            fail(code).await
        } else {
            Outcome::Retry(why)
        }
    };
    let creation = match state.channels.app_creation(organization_id, workspace_id, app_id).await {
        Ok(Some(creation)) => creation,
        Ok(None) => return Outcome::Done,
        Err(_) => return retry("store_unavailable", "unavailable").await,
    };
    let making = creation.app.status == AppStatus::Creating;
    if !making && !creation.in_flight {
        // Made, failed or deleted, with nothing of an attempt left at the
        // provider; a made app's events subscribed, if that's still to do.
        return subscribe(state, &creation.app, last_attempt).await;
    }
    let environment = match state.workspaces.find_workspace(organization_id, workspace_id).await {
        Ok(Some(workspace)) => workspace.environment,
        Ok(None) => return Outcome::Done,
        Err(_) => return retry("store_unavailable", "unavailable").await,
    };
    let (manager, credential) = match tokio::join!(
        state
            .channels
            .find_manager(organization_id, workspace_id, creation.app.manager_id),
        state
            .channels
            .manager_credential(organization_id, workspace_id, creation.app.manager_id),
    ) {
        (Ok(manager), Ok(credential)) => (manager, credential),
        _ => return retry("store_unavailable", "unavailable").await,
    };
    let reachable = match (manager, credential) {
        (Some(manager), Some(credential)) if manager.status == ManagerStatus::Active => Some((manager, credential)),
        _ => None,
    };
    let Some((manager, credential)) = reachable else {
        if making {
            return fail("manager_inactive").await;
        }
        // Nothing to reach the provider with: what was made stays there.
        tracing::warn!(event = "channels.orphan_unreachable", %organization_id, %workspace_id, %app_id);
        let recorded = creation.app.external_app_id.as_deref();
        return match state
            .channels
            .forget_app_creation(organization_id, workspace_id, app_id, recorded, Timestamp::now())
            .await
        {
            Ok(()) => Outcome::Done,
            Err(_) => Outcome::Retry("store_unavailable"),
        };
    };
    let Some(tokens) = open_tokens(
        state,
        organization_id,
        workspace_id,
        environment,
        manager.id,
        &credential,
    )
    .await
    else {
        return retry("keys_unavailable", "unavailable").await;
    };
    let events_url = format!(
        "{}/channels/inbound/{}",
        state.public_url,
        creation.inbound_ref.expose()
    );
    if creation.in_flight {
        match clean_up(state, scope, &tokens, &creation, &events_url).await {
            Ok(()) => {}
            Err(Outcome::Retry(why)) if making => return retry(why, why).await,
            Err(later) => return later,
        }
    }
    if !making {
        return subscribe(state, &creation.app, last_attempt).await;
    }
    let agent = match state
        .agents
        .find_agent(organization_id, workspace_id, creation.app.agent_id)
        .await
    {
        Ok(Some(agent)) => agent,
        Ok(None) => return Outcome::Done,
        Err(_) => return retry("store_unavailable", "unavailable").await,
    };
    // Marked before the provider is asked: whatever happens next, a later
    // attempt knows the provider may hold an app.
    match state
        .channels
        .begin_app_creation(organization_id, workspace_id, app_id, Timestamp::now())
        .await
    {
        Ok(()) => {}
        Err(ChannelError::AppNotReady | ChannelError::AppNotFound) => return Outcome::Done,
        Err(_) => return retry("store_unavailable", "unavailable").await,
    }
    let redirect_url = crate::app_configurations::redirect_uri(&state.public_url);
    let manifest = manifest_for(&agent, events_url, redirect_url);
    let created = match state
        .channel_providers
        .provisioner(creation.app.provider)
        .create(state.outbound.as_ref(), &tokens, &manifest)
        .await
    {
        Ok(created) => created,
        Err(ProvisionError::TokensRefused) => return fail("tokens_refused").await,
        Err(ProvisionError::Refused(code)) => return fail(code).await,
        Err(ProvisionError::NotConfigured) => return fail("provisioner_unavailable").await,
        // Nothing was made: asked again once Slack's wait is over.
        Err(ProvisionError::RateLimited { retry_after_secs }) => return wait(retry_after_secs),
        // Perhaps made all the same: the next attempt finds it by its events URL.
        Err(ProvisionError::Unavailable | ProvisionError::AppNotFound) => {
            return retry("provider_unavailable", "provider_unavailable").await;
        }
    };
    match state
        .channels
        .record_app_created(
            organization_id,
            workspace_id,
            app_id,
            &created.external_app_id,
            Timestamp::now(),
        )
        .await
    {
        Ok(()) => {}
        Err(ChannelError::AppNotReady | ChannelError::AppNotFound) => {
            // Deleted or failed while the provider made it.
            tracing::warn!(event = "channels.app_made_after_removal", %organization_id, %workspace_id, %app_id);
            discard(state, scope, creation.app.provider, &tokens, &created.external_app_id).await;
            return Outcome::Done;
        }
        Err(_) => {
            discard(state, scope, creation.app.provider, &tokens, &created.external_app_id).await;
            return retry("store_unavailable", "unavailable").await;
        }
    }
    let plaintext = match serde_json::to_vec(&SealingSecrets {
        client_secret: &created.secrets.client_secret,
        signing_secret: &created.secrets.signing_secret,
    }) {
        Ok(bytes) => Zeroizing::new(bytes),
        Err(_) => {
            discard(state, scope, creation.app.provider, &tokens, &created.external_app_id).await;
            return fail("unavailable").await;
        }
    };
    let context =
        EncryptionContext::channel_app_secret(organization_id, workspace_id, environment, app_id, FIRST_VERSION);
    let Ok(sealed) = keys::seal(state.keys.as_ref(), &context, &plaintext).await else {
        // Its secrets can't be kept, so neither can it.
        discard(state, scope, creation.app.provider, &tokens, &created.external_app_id).await;
        return retry("keys_unavailable", "unavailable").await;
    };
    let made = MadeApp {
        external_app_id: created.external_app_id.clone(),
        client_id: created.client_id.clone(),
        secrets: sealed,
    };
    match state
        .channels
        .record_app_made(organization_id, workspace_id, app_id, &made, Timestamp::now())
        .await
    {
        Ok(_) => {}
        Err(ChannelError::AppNotReady | ChannelError::AppNotFound) => {
            tracing::warn!(event = "channels.app_made_after_removal", %organization_id, %workspace_id, %app_id);
            discard(state, scope, creation.app.provider, &tokens, &created.external_app_id).await;
            return Outcome::Done;
        }
        // Perhaps recorded all the same: the next attempt reads which.
        Err(_) => return retry("store_unavailable", "unavailable").await,
    }
    tracing::info!(event = "channels.app_made", %organization_id, %workspace_id, %app_id);
    if let InstallStep::Installed(install) = created.install {
        install_made(
            state,
            organization_id,
            workspace_id,
            environment,
            &creation.app,
            &agent.name,
            install,
        )
        .await;
    }
    // Its secrets are stored: now its events, whose URL the provider checks
    // with a challenge signed by them. A retry of this job does only this.
    subscribe(state, &creation.app, last_attempt).await
}

/// The second step of making an app: the whole manifest sent, which
/// subscribes its events, once its secrets are stored to answer the
/// provider's check of their URL. Done once the store records the digest
/// sent; the app is read again, so one deleted or failed meanwhile has
/// nothing to subscribe.
async fn subscribe(state: &AppState, app: &ChannelApp, last_attempt: bool) -> Outcome {
    let (organization_id, workspace_id, app_id) = (app.organization_id, app.workspace_id, app.id);
    match crate::channel_upkeep::sync(state, organization_id, workspace_id, app_id).await {
        crate::channel_upkeep::Next::Done => Outcome::Done,
        crate::channel_upkeep::Next::Wait(after) => Outcome::Wait(after),
        crate::channel_upkeep::Next::Retry(why) if !last_attempt => Outcome::Retry(why),
        crate::channel_upkeep::Next::Retry(why) => {
            // Made, but deaf: a change to its agent sends the manifest again.
            tracing::error!(event = "channels.events_not_subscribed", %organization_id, %workspace_id, %app_id, why);
            Outcome::Done
        }
    }
}

/// A provider that installed the app itself: its connection, now.
async fn install_made(
    state: &AppState,
    organization_id: OrganizationId,
    workspace_id: WorkspaceId,
    environment: Environment,
    app: &ChannelApp,
    agent_name: &str,
    install: zupflow_core::channels::provision::ProviderInstall,
) {
    let connection_id = ChannelConnectionId::generate();
    let context = EncryptionContext::channel_connection_credential(
        organization_id,
        workspace_id,
        environment,
        connection_id,
        FIRST_VERSION,
    );
    // All the connection's tokens together, as one value: just its token, for now.
    let Ok(plaintext) = serde_json::to_vec(&crate::channel_install::SealingConnection {
        token: &install.token,
        refresh_token: None,
        expires_at: None,
    }) else {
        return;
    };
    let plaintext = Zeroizing::new(plaintext);
    let Ok(sealed) = keys::seal(state.keys.as_ref(), &context, &plaintext).await else {
        tracing::warn!(event = "channels.install_seal_failed", %organization_id, %workspace_id, app_id = %app.id);
        return;
    };
    let installed = state
        .channels
        .install_app(
            organization_id,
            workspace_id,
            app.id,
            &NewInstall {
                connection_id,
                team: install.team.clone(),
                identity: install.identity.clone(),
                granted_scopes: install.granted_scopes.clone(),
                label: agent_name.chars().take(MAX_LABEL).collect(),
                credential: SealedVersion {
                    version: FIRST_VERSION,
                    sealed,
                },
            },
            Timestamp::now(),
        )
        .await;
    if let Err(error) = installed {
        tracing::warn!(event = "channels.install_failed", %organization_id, %workspace_id, app_id = %app.id, %error);
    }
}

/// Claims and handles one channel job, if one is due: making an app, or
/// keeping one in step with its provider. Whether one was.
pub async fn work_once(state: &AppState) -> bool {
    let Ok(kind) = Name::new(PROVISION_CHANNEL_APP) else {
        return false;
    };
    let mut kinds = vec![kind];
    kinds.extend(crate::channel_upkeep::kinds());
    // Cross-tenant by design: a worker takes the next job of any scope.
    let job = match state.jobs.claim_next(&kinds, LEASE).await {
        Ok(Some(job)) => job,
        Ok(None) => return false,
        Err(error) => {
            tracing::warn!(event = "channels.claim_failed", %error);
            return false;
        }
    };
    if job.kind.as_str() == PROVISION_CHANNEL_APP {
        handle(state, job).await;
    } else {
        crate::channel_upkeep::handle(state, job).await;
    }
    true
}

async fn handle(state: &AppState, job: Job) {
    let Job {
        id,
        scope,
        payload,
        attempt,
        lease,
        ..
    } = job;
    let app_id = serde_json::from_slice::<Value>(&payload)
        .ok()
        .and_then(|payload| payload.get("app_id")?.as_str().map(str::to_owned))
        .and_then(|id| ChannelAppId::parse(&id).ok());
    let (Scope::Workspace(organization_id, workspace_id), Some(app_id)) = (scope, app_id) else {
        tracing::error!(event = "channels.malformed_job", kind = PROVISION_CHANNEL_APP);
        if let Ok(reason) = Name::new("malformed") {
            let _ = state.jobs.bury(&scope, id, &lease, &reason).await;
        }
        return;
    };
    let finished = match provision(state, organization_id, workspace_id, app_id, attempt >= ATTEMPTS).await {
        Outcome::Done => state.jobs.complete(&scope, id, &lease).await.map(|_| ()),
        Outcome::Retry(reason) => match Name::new(reason) {
            Ok(reason) => {
                let after = Duration::from_secs(15 * u64::from(attempt.max(1)));
                state.jobs.retry(&scope, id, &lease, after, &reason).await.map(|_| ())
            }
            Err(_) => state.jobs.complete(&scope, id, &lease).await,
        },
        Outcome::Wait(after) => state.jobs.postpone(&scope, id, &lease, after).await,
    };
    if let Err(error) = finished {
        tracing::warn!(event = "channels.finish_failed", %organization_id, %workspace_id, %app_id, %error);
    }
}

/// Settles apps whose making job is gone or buried (a last attempt whose
/// lease ran out, say): one still being made is failed, so its agent can be
/// given another, and one still in flight gets a job to delete what was
/// made at the provider. How many it settled.
pub async fn reap_once(state: &AppState) -> u32 {
    let Ok(kind) = Name::new(PROVISION_CHANNEL_APP) else {
        return 0;
    };
    let cleanup = |app_id: ChannelAppId| provision_job(&kind, app_id);
    // Cross-tenant by design: a sweep over every workspace, like a claim.
    match state
        .channels
        .reap_stranded_apps_cross_tenant(&cleanup, REAP_LIMIT, Timestamp::now())
        .await
    {
        Ok(settled) => {
            if settled > 0 {
                tracing::info!(event = "channels.apps_reaped", settled);
            }
            settled
        }
        Err(error) => {
            tracing::warn!(event = "channels.reap_failed", %error);
            0
        }
    }
}

/// Queues the rotations of linked teams' tokens that are due, and expires
/// those that lapsed unrotated.
pub async fn rotate_due(state: &AppState) {
    crate::channel_upkeep::sweep(state).await;
}

/// Queues the retention sweeps that are due, one per workspace: how many.
pub async fn retention_due(state: &AppState) -> usize {
    crate::channel_upkeep::retention_due(state).await
}

/// Makes apps and keeps them in step until the process stops, and sweeps
/// for stranded apps and tokens due for rotation. New messages have their
/// own worker ([`crate::channel_inbound::run_worker`]), so a slow provider
/// never holds them up.
pub async fn run_worker(state: AppState) {
    let mut next_sweep = tokio::time::Instant::now();
    loop {
        if tokio::time::Instant::now() >= next_sweep {
            reap_once(&state).await;
            rotate_due(&state).await;
            retention_due(&state).await;
            next_sweep = tokio::time::Instant::now() + REAP_EVERY;
        }
        if !work_once(&state).await {
            tokio::time::sleep(IDLE).await;
        }
    }
}
