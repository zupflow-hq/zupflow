//! [`ChannelStore`]: linked teams, generated apps, connections with what they
//! have seen, agent channels, and the organization's tool levels and settings.
//!
//! **Scope.** Every method takes the organization first, then (for anything
//! but tool levels and settings) the workspace, and reaches nothing outside
//! them: an ID of another workspace, even of the same organization, is the
//! same as one that doesn't exist. The statements that start without a
//! tenant are named and documented as such:
//! [`ChannelStore::attribute_app_by_inbound_ref_cross_tenant`] (a provider's
//! event names only its app's inbound reference) and
//! [`ChannelStore::reap_stranded_apps_cross_tenant`] (a sweep, like a
//! worker's claim, that then works on each app in its own scope).
//!
//! **Concurrency.** Every change takes the acting [`ChannelEditor`] and
//! checks, inside its own transaction, that they still may: an active member
//! of the role the change needs, or an unrevoked API key of the workspace's
//! environment. The rules that must hold across concurrent changes are held
//! there too, and by the database: one live app per agent per manager, one
//! live connection per app, an agent channel only on its own agent's app, a
//! token replaced only from the version read, one active run per session,
//! and the conversation breaker's count.
//!
//! **Secrets.** Tokens and secrets are stored sealed, one value per version,
//! under the context their kind's [`crate::keys::EncryptionContext`]
//! constructor builds; reads return them sealed, for the code that opens
//! them, and no view holds them.
//!
//! **Announcements.** Each status change commits with its webhook event
//! (`channel_app_manager.status_changed`, `channel_app.status_changed`,
//! `channel_connection.status_changed`, `channel_message.received` or
//! `.sent`, `channel_conversation.paused`), and each change a person or key
//! makes with its [`Audit`].

use async_trait::async_trait;
use uuid::Uuid;

use super::{
    AgentChannel, AgentChannelSettings, BreakerSettings, ChannelApp, ChannelAppManager, ChannelConnection,
    ChannelConversation, ChannelMessage, ChannelProviderKind, ChannelSettings, ChannelToolLevel, ConnectionTeam,
    ConversationKind, Direction, HandlerKind, Identity, InboundRef, ManagerMethod,
};
use crate::audit::Audit;
use crate::connect::ConnectSession;
use crate::ids::{
    AgentChannelId, AgentId, ApiKeyId, ChannelAppId, ChannelAppManagerId, ChannelConnectionId, ChannelConversationId,
    ChannelMessageId, ConnectSessionId, MemberId, OrganizationId, RunId, SessionId, ToolCallId, UserId, WorkspaceId,
};
use crate::keys::Sealed;
use crate::policy::ToolLevel;
use crate::runs::QueuedJob;
use crate::time::Timestamp;
use crate::workspaces::Page;

/// Who makes a change, rechecked inside its transaction. The caller can't
/// supply it through the API: it comes from the trust context.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChannelEditor {
    /// A member: a developer or stricter for a workspace's channels, an
    /// admin or owner for the organization's levels and settings.
    Member(MemberId),
    /// An API key: every permission, in its organization and environment.
    ApiKey(ApiKeyId),
}

/// An agent's session in one thread: the run session its runs there share.
/// Internal: never shown, so no prefix and not in the ID registry.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct ChannelSessionKey(Uuid);

impl ChannelSessionKey {
    pub fn generate() -> Self {
        Self(Uuid::now_v7())
    }

    /// For row mapping only.
    pub fn from_uuid(uuid: Uuid) -> Self {
        Self(uuid)
    }

    pub fn uuid(&self) -> Uuid {
        self.0
    }
}

/// A sealed value at the version it was sealed for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SealedVersion {
    pub version: u32,
    pub sealed: Sealed,
}

/// A team to link: its tokens sealed together at
/// [`super::FIRST_VERSION`] under `channel_app_manager_credential`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NewManager {
    pub provider: ChannelProviderKind,
    pub method: ManagerMethod,
    pub credential: Sealed,
    /// The configuration token's last four characters, for display.
    pub token_last4: String,
    pub token_expires_at: Option<Timestamp>,
    /// The person linking it; `None` for an API key or hosted Connect.
    pub created_by: Option<UserId>,
}

/// New tokens, sealed at `expected_version + 1`: stored only while the
/// manager's tokens are still at `expected_version`, so two writers (a
/// person, a rotation) never both spend one version.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TokenReplacement {
    pub expected_version: u32,
    pub credential: Sealed,
    pub token_last4: String,
    pub token_expires_at: Option<Timestamp>,
}

/// An app to make: the job that makes it with the provider is queued in the
/// same transaction.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NewApp {
    pub agent_id: AgentId,
    pub manager_id: ChannelAppManagerId,
    pub inbound_ref: InboundRef,
}

/// What the provider made: its secrets sealed together at
/// [`super::FIRST_VERSION`] under `channel_app_secret`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MadeApp {
    pub external_app_id: String,
    pub client_id: String,
    pub secrets: Sealed,
}

/// Where an app's making stands, for the job that makes it: read whatever
/// the app's status, deleted included, since what an attempt made at the
/// provider must still be deleted there.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AppCreation {
    pub app: ChannelApp,
    /// Where its events go; the provider looks an app up by it.
    pub inbound_ref: InboundRef,
    /// An attempt marked it before asking the provider to make it, and what
    /// that made isn't settled yet: neither recorded as made nor deleted at
    /// the provider. The provider may hold an app no row records.
    pub in_flight: bool,
}

/// What the inbound endpoint and the install need of an app: never shown.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AppSecretsRecord {
    pub inbound_ref: InboundRef,
    pub client_id: Option<String>,
    /// `None` until the provider has made the app.
    pub secrets: Option<SealedVersion>,
}

/// What a list of apps asks for.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct AppFilter {
    pub agent_id: Option<AgentId>,
    pub manager_id: Option<ChannelAppManagerId>,
    /// A filter naming something that can't exist (a malformed ID): an empty list.
    pub matches_nothing: bool,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ConnectionFilter {
    pub agent_id: Option<AgentId>,
    pub matches_nothing: bool,
}

/// A connection's changes; `None` keeps the field.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ConnectionChanges {
    pub label: Option<String>,
    pub handler: Option<HandlerKind>,
}

/// An install of an app: into `connection_id`, a new connection's ID, or the
/// app's live connection's on a reinstall. Its token is sealed under
/// `channel_connection_credential` for that ID at `credential.version`: 1 for
/// a new connection, the live one's plus one for a reinstall.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NewInstall {
    pub connection_id: ChannelConnectionId,
    pub team: ConnectionTeam,
    pub identity: Identity,
    pub granted_scopes: Vec<String>,
    /// A new connection's label; a reinstall keeps its own.
    pub label: String,
    pub credential: SealedVersion,
}

/// A conversation as a message names it; saved once per connection and external ID.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NewConversation {
    pub external_id: String,
    pub kind: ConversationKind,
    pub name: Option<String>,
    pub is_external: bool,
    pub identity_is_member: bool,
}

/// Someone a message is from; saved once per connection and external ID.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NewPerson {
    pub external_id: String,
    pub handle: Option<String>,
    pub display_name: Option<String>,
    pub email: Option<String>,
    pub is_automated: bool,
}

/// A message to record, once per conversation and external ID.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NewMessage {
    pub id: ChannelMessageId,
    pub conversation: NewConversation,
    pub direction: Direction,
    /// Inbound only: who wrote it.
    pub author: Option<NewPerson>,
    pub external_id: String,
    pub thread_key: String,
    pub text: String,
    pub mentions: Vec<String>,
    /// Outbound only.
    pub run_id: Option<RunId>,
    pub tool_call_id: Option<ToolCallId>,
    pub agent_hop: u32,
    pub sent_at: Timestamp,
}

/// A message recorded, or the one recorded before under its external ID.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RecordedMessage {
    pub message: ChannelMessage,
    pub new: bool,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ConversationFilter {
    /// Matches the name, any case.
    pub q: Option<String>,
    pub kind: Option<ConversationKind>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct MessageFilter {
    /// Only this thread's messages.
    pub thread: Option<String>,
}

/// An agent's session in one thread of one conversation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChannelSession {
    pub key: ChannelSessionKey,
    pub agent_channel_id: AgentChannelId,
    pub conversation_id: ChannelConversationId,
    pub thread_key: String,
    /// The runs' `session_id`.
    pub session_id: SessionId,
    /// Set while a run is queued or running: one at a time.
    pub active_run_id: Option<RunId>,
    pub last_run_id: Option<RunId>,
}

/// What the conversation breaker made of one more run.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BreakerOutcome {
    /// Counted, as the window's `count`th run: it may start.
    Counted { count: u32 },
    /// This run went over the limit: agents are paused here until then.
    Tripped { paused_until: Timestamp },
    /// Already paused until then.
    Paused { until: Timestamp },
}

/// A level to set on one channel tool.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LevelChange {
    pub tool: String,
    pub level: ToolLevel,
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ChannelError {
    #[error("the store is unavailable")]
    Unavailable,
    #[error("not a cursor from this list")]
    InvalidCursor,
    /// The editor can no longer make this change here.
    #[error("the caller can no longer make this change")]
    NotAllowed,
    #[error("channel app manager not found")]
    ManagerNotFound,
    #[error("channel app not found")]
    AppNotFound,
    #[error("channel connection not found")]
    ConnectionNotFound,
    #[error("conversation not found")]
    ConversationNotFound,
    #[error("agent not found")]
    AgentNotFound,
    #[error("agent channel not found")]
    AgentChannelNotFound,
    #[error("connect session not found")]
    SessionNotFound,
    /// The manager's tokens expired or it was disconnected.
    #[error("the manager isn't active")]
    ManagerInactive,
    /// The agent already has a live app from this manager.
    #[error("the agent already has an app in this team")]
    AppExists,
    /// The manager still has this many apps that aren't deleted.
    #[error("the manager still has apps")]
    ManagerInUse(u64),
    /// The tokens were replaced since they were read.
    #[error("the tokens changed meanwhile")]
    TokensChanged,
    /// The app isn't in the state the change needs (made, or still being made).
    #[error("the app isn't ready")]
    AppNotReady,
    /// An install into another team than the manager's.
    #[error("the install is for another team")]
    TeamMismatch,
    /// The app's live connection changed since it was read.
    #[error("the install changed meanwhile")]
    InstallChanged,
    /// The connection is another agent's app.
    #[error("the connection is another agent's app")]
    NotAgentsApp,
    #[error("the connection is disconnected")]
    ConnectionDisconnected,
    /// The organization denies this tool.
    #[error("the tool is denied")]
    ToolDenied(String),
    /// The organization requires approval for this tool, and it isn't in `approval_required`.
    #[error("the tool requires approval")]
    ApprovalRequiredByPolicy(String),
    /// A listed conversation isn't one this connection has seen.
    #[error("an unknown conversation")]
    UnknownConversation,
    /// The Connect session isn't pending any more, or has expired.
    #[error("the session isn't pending")]
    SessionNotPending,
}

/// See the module's documentation for the rules every method holds.
#[async_trait]
pub trait ChannelStore: Send + Sync {
    // --- Linked teams --------------------------------------------------------

    /// A page of the workspace's managers, newest first.
    async fn list_managers(
        &self,
        organization_id: OrganizationId,
        workspace_id: WorkspaceId,
        cursor: Option<&str>,
        limit: u32,
    ) -> Result<Page<ChannelAppManager>, ChannelError>;

    async fn find_manager(
        &self,
        organization_id: OrganizationId,
        workspace_id: WorkspaceId,
        manager_id: ChannelAppManagerId,
    ) -> Result<Option<ChannelAppManager>, ChannelError>;

    /// Links a team: an `active` manager with its tokens at version 1,
    /// announced as `active`.
    #[allow(clippy::too_many_arguments)]
    async fn create_manager(
        &self,
        organization_id: OrganizationId,
        workspace_id: WorkspaceId,
        manager_id: ChannelAppManagerId,
        editor: ChannelEditor,
        manager: &NewManager,
        now: Timestamp,
        audit: &Audit<'_, ChannelAppManager>,
    ) -> Result<ChannelAppManager, ChannelError>;

    /// Links a team through a pending `channel_app_manager` Connect session,
    /// as one: the manager (announced as `active`), then the session
    /// completed naming it (announced). `SessionNotPending` when the session
    /// isn't pending at `now`; nothing is written then.
    #[allow(clippy::too_many_arguments)]
    async fn link_team_through_session(
        &self,
        organization_id: OrganizationId,
        workspace_id: WorkspaceId,
        session_id: ConnectSessionId,
        manager_id: ChannelAppManagerId,
        manager: &NewManager,
        now: Timestamp,
        audit: &Audit<'_, (ConnectSession, ChannelAppManager)>,
    ) -> Result<(ConnectSession, ChannelAppManager), ChannelError>;

    /// The manager's current tokens, sealed, for the code that uses them.
    async fn manager_credential(
        &self,
        organization_id: OrganizationId,
        workspace_id: WorkspaceId,
        manager_id: ChannelAppManagerId,
    ) -> Result<Option<SealedVersion>, ChannelError>;

    /// Stores the next version of the tokens and forgets the earlier ones;
    /// the manager is `active` again (announced if it wasn't). `TokensChanged`
    /// unless the stored version is still `expected_version`. `editor` is
    /// `None` for Zupflow's own rotation: no audit is recorded then.
    #[allow(clippy::too_many_arguments)]
    async fn replace_manager_tokens(
        &self,
        organization_id: OrganizationId,
        workspace_id: WorkspaceId,
        manager_id: ChannelAppManagerId,
        editor: Option<ChannelEditor>,
        replacement: &TokenReplacement,
        now: Timestamp,
        audit: &Audit<'_, ChannelAppManager>,
    ) -> Result<ChannelAppManager, ChannelError>;

    /// Unlinks a team, forgetting its tokens. `ManagerInUse` while it has apps
    /// that aren't deleted.
    async fn delete_manager(
        &self,
        organization_id: OrganizationId,
        workspace_id: WorkspaceId,
        manager_id: ChannelAppManagerId,
        editor: ChannelEditor,
        audit: &Audit<'_, ChannelAppManager>,
    ) -> Result<(), ChannelError>;

    // --- Generated apps ------------------------------------------------------

    /// A page of the workspace's apps that aren't deleted, newest first.
    async fn list_apps(
        &self,
        organization_id: OrganizationId,
        workspace_id: WorkspaceId,
        filter: &AppFilter,
        cursor: Option<&str>,
        limit: u32,
    ) -> Result<Page<ChannelApp>, ChannelError>;

    /// The app, unless it's deleted.
    async fn find_app(
        &self,
        organization_id: OrganizationId,
        workspace_id: WorkspaceId,
        app_id: ChannelAppId,
    ) -> Result<Option<ChannelApp>, ChannelError>;

    /// Makes the agent's app in the manager's team as `creating`, and queues
    /// `job` with it. `AgentNotFound`, `ManagerNotFound` (each within the
    /// workspace), `ManagerInactive`, and `AppExists` while the agent has a
    /// live app from this manager.
    #[allow(clippy::too_many_arguments)]
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
    ) -> Result<ChannelApp, ChannelError>;

    /// Records what the provider made, its secrets sealed: the app is
    /// `awaiting_install` (announced) and no longer in flight. `AppNotFound`
    /// once it's deleted, and `AppNotReady` unless it's still `creating` and
    /// records no other provider ID: nothing is written then.
    async fn record_app_made(
        &self,
        organization_id: OrganizationId,
        workspace_id: WorkspaceId,
        app_id: ChannelAppId,
        made: &MadeApp,
        now: Timestamp,
    ) -> Result<ChannelApp, ChannelError>;

    /// Where the app's making stands, whatever its status (deleted
    /// included); `None` only when there's no such app in the workspace.
    async fn app_creation(
        &self,
        organization_id: OrganizationId,
        workspace_id: WorkspaceId,
        app_id: ChannelAppId,
    ) -> Result<Option<AppCreation>, ChannelError>;

    /// Marks the app in flight before an attempt asks the provider to make
    /// it. `AppNotReady` unless it's `creating` with no provider ID recorded
    /// (an earlier attempt's app is deleted and forgotten first).
    async fn begin_app_creation(
        &self,
        organization_id: OrganizationId,
        workspace_id: WorkspaceId,
        app_id: ChannelAppId,
        now: Timestamp,
    ) -> Result<(), ChannelError>;

    /// Records the provider's ID for what an attempt just made, before its
    /// secrets are sealed. `AppNotReady` unless the app is `creating`, in
    /// flight and has none recorded (deleted meanwhile, say): the caller
    /// deletes what it made at the provider then.
    async fn record_app_created(
        &self,
        organization_id: OrganizationId,
        workspace_id: WorkspaceId,
        app_id: ChannelAppId,
        external_app_id: &str,
        now: Timestamp,
    ) -> Result<(), ChannelError>;

    /// The provider holds nothing an earlier attempt made any more (deleted
    /// there, or never made): the app's provider ID, if it still records
    /// `external_app_id`, is cleared and it's no longer in flight. Nothing
    /// changes for an app that isn't in flight.
    async fn forget_app_creation(
        &self,
        organization_id: OrganizationId,
        workspace_id: WorkspaceId,
        app_id: ChannelAppId,
        external_app_id: Option<&str>,
        now: Timestamp,
    ) -> Result<(), ChannelError>;

    /// **Cross-tenant, by design:** a sweep over every workspace, like a
    /// worker's claim. Finds up to `limit` apps whose making job is gone or
    /// buried (its last attempt's lease ran out, say) and settles each in
    /// its own scope: one still `creating` is `failed` with
    /// `provisioning_abandoned` (announced), so the agent can be given
    /// another; one still in flight gets `cleanup(app)` queued, a job that
    /// deletes what was made at the provider. How many it settled.
    async fn reap_stranded_apps_cross_tenant(
        &self,
        cleanup: &(dyn Fn(ChannelAppId) -> QueuedJob + Send + Sync),
        limit: u32,
        now: Timestamp,
    ) -> Result<u32, ChannelError>;

    /// The provider refused to make it: `failed` with a stable code
    /// (announced), unless it's no longer `creating`.
    async fn record_app_failed(
        &self,
        organization_id: OrganizationId,
        workspace_id: WorkspaceId,
        app_id: ChannelAppId,
        code: &str,
        now: Timestamp,
    ) -> Result<(), ChannelError>;

    /// What the provider holds for the app, for the inbound endpoint and the install.
    async fn app_secrets(
        &self,
        organization_id: OrganizationId,
        workspace_id: WorkspaceId,
        app_id: ChannelAppId,
    ) -> Result<Option<AppSecretsRecord>, ChannelError>;

    /// **Cross-tenant, by design:** a provider's event names only the app's
    /// inbound reference, which is how its organization and workspace are
    /// learned. Reads one app that isn't deleted by that reference and
    /// nothing else; every later statement carries the scope it returns.
    async fn attribute_app_by_inbound_ref_cross_tenant(
        &self,
        inbound_ref: &InboundRef,
    ) -> Result<Option<(OrganizationId, WorkspaceId, ChannelAppId)>, ChannelError>;

    /// Removes the agent's app: its live connection is disconnected
    /// (announced) and the agent's channel on it removed, its history kept;
    /// the app is `deleted` (announced). `AppNotFound` once it's deleted.
    async fn delete_app(
        &self,
        organization_id: OrganizationId,
        workspace_id: WorkspaceId,
        app_id: ChannelAppId,
        editor: ChannelEditor,
        now: Timestamp,
        audit: &Audit<'_, ChannelApp>,
    ) -> Result<(), ChannelError>;

    /// Installs an app that's made (`AppNotReady` otherwise) whose manager is
    /// active (`ManagerInactive`): a new connection, or its live one again on
    /// a reinstall (`InstallChanged` per [`NewInstall`]); the agent's channel
    /// on it with the defaults if it has none; the app `installed`. The
    /// manager learns its team from the first install, and every later one
    /// must be into it (`TeamMismatch`). Each change announced.
    async fn install_app(
        &self,
        organization_id: OrganizationId,
        workspace_id: WorkspaceId,
        app_id: ChannelAppId,
        install: &NewInstall,
        now: Timestamp,
    ) -> Result<(ChannelConnection, AgentChannel), ChannelError>;

    // --- Connections and what they've seen -----------------------------------

    /// A page of the workspace's connections, newest first.
    async fn list_connections(
        &self,
        organization_id: OrganizationId,
        workspace_id: WorkspaceId,
        filter: &ConnectionFilter,
        cursor: Option<&str>,
        limit: u32,
    ) -> Result<Page<ChannelConnection>, ChannelError>;

    async fn find_connection(
        &self,
        organization_id: OrganizationId,
        workspace_id: WorkspaceId,
        connection_id: ChannelConnectionId,
    ) -> Result<Option<ChannelConnection>, ChannelError>;

    #[allow(clippy::too_many_arguments)]
    async fn update_connection(
        &self,
        organization_id: OrganizationId,
        workspace_id: WorkspaceId,
        connection_id: ChannelConnectionId,
        editor: ChannelEditor,
        changes: &ConnectionChanges,
        now: Timestamp,
        audit: &Audit<'_, ChannelConnection>,
    ) -> Result<ChannelConnection, ChannelError>;

    /// Disconnects (announced) and removes the connection with its
    /// conversations, people, messages, token and agent channel; its app, if
    /// installed, waits for a new install (announced).
    async fn delete_connection(
        &self,
        organization_id: OrganizationId,
        workspace_id: WorkspaceId,
        connection_id: ChannelConnectionId,
        editor: ChannelEditor,
        now: Timestamp,
        audit: &Audit<'_, ChannelConnection>,
    ) -> Result<(), ChannelError>;

    /// Records a message on a connection with its conversation and author,
    /// each saved once; a message seen before is returned as it was, with
    /// `new: false`. A new one is announced.
    async fn record_message(
        &self,
        organization_id: OrganizationId,
        workspace_id: WorkspaceId,
        connection_id: ChannelConnectionId,
        message: &NewMessage,
        now: Timestamp,
    ) -> Result<RecordedMessage, ChannelError>;

    /// Records a delivery to an app by the provider's event ID: `false` when
    /// it was recorded before, so a retried delivery has no effect.
    #[allow(clippy::too_many_arguments)]
    async fn record_inbound_event(
        &self,
        organization_id: OrganizationId,
        workspace_id: WorkspaceId,
        app_id: ChannelAppId,
        provider_event_id: &str,
        kind: &str,
        now: Timestamp,
    ) -> Result<bool, ChannelError>;

    /// A page of the connection's conversations, most recent message first.
    async fn list_conversations(
        &self,
        organization_id: OrganizationId,
        workspace_id: WorkspaceId,
        connection_id: ChannelConnectionId,
        filter: &ConversationFilter,
        cursor: Option<&str>,
        limit: u32,
    ) -> Result<Page<ChannelConversation>, ChannelError>;

    async fn find_conversation(
        &self,
        organization_id: OrganizationId,
        workspace_id: WorkspaceId,
        connection_id: ChannelConnectionId,
        conversation_id: ChannelConversationId,
    ) -> Result<Option<ChannelConversation>, ChannelError>;

    /// A page of the conversation's messages, newest first.
    #[allow(clippy::too_many_arguments)]
    async fn list_messages(
        &self,
        organization_id: OrganizationId,
        workspace_id: WorkspaceId,
        connection_id: ChannelConnectionId,
        conversation_id: ChannelConversationId,
        filter: &MessageFilter,
        cursor: Option<&str>,
        limit: u32,
    ) -> Result<Page<ChannelMessage>, ChannelError>;

    // --- Agent channels ------------------------------------------------------

    /// The agent's channels, oldest first. `AgentNotFound` without the agent.
    async fn list_agent_channels(
        &self,
        organization_id: OrganizationId,
        workspace_id: WorkspaceId,
        agent_id: AgentId,
    ) -> Result<Vec<AgentChannel>, ChannelError>;

    /// Sets the agent's channel on a connection of its own app, creating it
    /// if it isn't there. Checked in the transaction: the agent and the
    /// connection (`AgentNotFound`, `ConnectionNotFound`), the connection is
    /// its app's (`NotAgentsApp`) and not disconnected
    /// (`ConnectionDisconnected`), the organization's levels in force
    /// (`ToolDenied`, `ApprovalRequiredByPolicy`), and the listed
    /// conversations are this connection's (`UnknownConversation`).
    #[allow(clippy::too_many_arguments)]
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
    ) -> Result<AgentChannel, ChannelError>;

    async fn delete_agent_channel(
        &self,
        organization_id: OrganizationId,
        workspace_id: WorkspaceId,
        agent_id: AgentId,
        connection_id: ChannelConnectionId,
        editor: ChannelEditor,
        audit: &Audit<'_, AgentChannel>,
    ) -> Result<(), ChannelError>;

    // --- Runs in threads -----------------------------------------------------

    /// The agent channel's session in a thread, made on first use.
    async fn open_session(
        &self,
        organization_id: OrganizationId,
        workspace_id: WorkspaceId,
        agent_channel_id: AgentChannelId,
        conversation_id: ChannelConversationId,
        thread_key: &str,
        now: Timestamp,
    ) -> Result<ChannelSession, ChannelError>;

    /// Makes `run_id` the session's active run if it has none: `false` when
    /// another run holds it, so concurrent starts begin one run.
    async fn claim_session_run(
        &self,
        organization_id: OrganizationId,
        workspace_id: WorkspaceId,
        session: ChannelSessionKey,
        run_id: RunId,
        now: Timestamp,
    ) -> Result<bool, ChannelError>;

    /// Frees the session when `run_id` holds it, keeping it as the last run.
    async fn release_session_run(
        &self,
        organization_id: OrganizationId,
        workspace_id: WorkspaceId,
        session: ChannelSessionKey,
        run_id: RunId,
        now: Timestamp,
    ) -> Result<bool, ChannelError>;

    /// Counts one more run in the conversation against the organization's
    /// breaker, as one: a run over `breaker_runs` within the window pauses
    /// agents there for `breaker_pause_minutes` (announced).
    async fn count_breaker_run(
        &self,
        organization_id: OrganizationId,
        workspace_id: WorkspaceId,
        conversation_id: ChannelConversationId,
        now: Timestamp,
    ) -> Result<BreakerOutcome, ChannelError>;

    // --- The organization's levels and settings -------------------------------

    /// Every provider's every channel tool, in [`super::CHANNEL_TOOLS`]'s
    /// order: its level set, or else Zupflow's default.
    async fn list_tool_levels(&self, organization_id: OrganizationId) -> Result<Vec<ChannelToolLevel>, ChannelError>;

    /// Sets levels, each confirmed, as written by `updated_by`: an admin or
    /// owner, rechecked. Every tool is a channel tool, named once (the caller
    /// checks).
    #[allow(clippy::too_many_arguments)]
    async fn set_tool_levels(
        &self,
        organization_id: OrganizationId,
        editor: MemberId,
        provider: ChannelProviderKind,
        levels: &[LevelChange],
        updated_by: UserId,
        now: Timestamp,
        audit: &Audit<'_, Vec<ChannelToolLevel>>,
    ) -> Result<Vec<ChannelToolLevel>, ChannelError>;

    /// The organization's settings: the defaults until saved.
    async fn channel_settings(&self, organization_id: OrganizationId) -> Result<ChannelSettings, ChannelError>;

    /// Saves them, as written by `updated_by`: an admin or owner, rechecked.
    async fn save_channel_settings(
        &self,
        organization_id: OrganizationId,
        editor: MemberId,
        values: &BreakerSettings,
        updated_by: UserId,
        now: Timestamp,
        audit: &Audit<'_, ChannelSettings>,
    ) -> Result<ChannelSettings, ChannelError>;
}
