//! Channels: a workspace's agents reached in a team's chat (Slack first).
//!
//! Every agent on a channel has its own app in the team, which Zupflow
//! generates through an [`provision::AppProvisioner`] from the team's
//! linked **app manager**. Installing that app makes a **connection**: one
//! team, one identity (in Slack, the app's bot user), one agent. The agent's
//! settings on its connection are its **agent channel**. A connection keeps
//! the conversations, people and messages it has seen.
//!
//! - [`handling`]: [`handling::MessageHandler`], which decides which agents
//!   answer a new message, and [`handling::MentionHandler`], the one built now.
//! - [`provider`]: [`provider::ChannelProvider`], what a provider does with
//!   an installed connection: verifying and reading its events.
//! - [`provision`]: [`provision::AppProvisioner`], which creates, changes
//!   and deletes one agent's app in one team.
//! - [`store`]: [`store::ChannelStore`], everything above as stored.
//!
//! Everything is workspace-scoped except the tool levels and settings, which
//! are the organization's. Zupflow's own names avoid "bot": a connection acts
//! as its identity, and people or apps posting automatically are automated
//! accounts. "Bot" is only ever Slack's own term (bot token, bot user).

pub mod handling;
pub mod provider;
pub mod provision;
pub mod store;
pub mod thread;

use serde::Serialize;

use crate::apps::ToolEffect;
use crate::ids::{
    AgentChannelId, AgentId, ChannelAppId, ChannelAppManagerId, ChannelConnectionId, ChannelConversationId,
    ChannelMessageId, ChannelPersonId, OrganizationId, RunId, ToolCallId, WorkspaceId,
};
use crate::installations::ShortId;
use crate::organizations::User;
use crate::policy::ToolLevel;
use crate::time::Timestamp;

/// A closed set of values stored and written as their snake_case names.
macro_rules! named_values {
    ($(#[$doc:meta])* $name:ident { $($(#[$vdoc:meta])* $variant:ident => $text:literal,)* }) => {
        $(#[$doc])*
        #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize)]
        pub enum $name {
            $($(#[$vdoc])* #[serde(rename = $text)] $variant,)*
        }

        impl $name {
            pub const ALL: &'static [$name] = &[$(Self::$variant),*];

            pub fn as_str(self) -> &'static str {
                match self {
                    $(Self::$variant => $text,)*
                }
            }

            pub fn parse(value: &str) -> Option<Self> {
                match value {
                    $($text => Some(Self::$variant),)*
                    _ => None,
                }
            }
        }
    };
}

named_values! {
    /// The channel providers: a registry, like model providers.
    ChannelProviderKind {
        Slack => "slack",
    }
}

impl ChannelProviderKind {
    /// The provider's name, as people know it.
    pub fn name(self) -> &'static str {
        match self {
            Self::Slack => "Slack",
        }
    }
}

named_values! {
    /// How Zupflow creates apps in a team: an app configuration token and its
    /// refresh token, pasted by a team admin.
    ManagerMethod {
        ConfigurationToken => "configuration_token",
    }
}

named_values! {
    /// `token_expired`: the token can't be rotated any more; a new one must be pasted.
    ManagerStatus {
        Active => "active",
        TokenExpired => "token_expired",
        Disconnected => "disconnected",
    }
}

named_values! {
    /// An app's life: made by a job, then waiting for its install.
    AppStatus {
        Creating => "creating",
        AwaitingInstall => "awaiting_install",
        Installed => "installed",
        Updating => "updating",
        Deleting => "deleting",
        Deleted => "deleted",
        Failed => "failed",
    }
}

impl AppStatus {
    /// Holds its agent's place in its team: neither deleted nor failed.
    pub fn is_live(self) -> bool {
        !matches!(self, Self::Deleted | Self::Failed)
    }

    /// Can be installed: made, and waiting for its install or installed already (a reinstall).
    pub fn is_installable(self) -> bool {
        matches!(self, Self::AwaitingInstall | Self::Installed)
    }
}

named_values! {
    ConnectionStatus {
        Connected => "connected",
        ReconnectRequired => "reconnect_required",
        Disconnected => "disconnected",
    }
}

named_values! {
    /// Which messages wake a connection's agent. Only `mention` is built.
    HandlerKind {
        Mention => "mention",
    }
}

named_values! {
    ConversationKind {
        Channel => "channel",
        PrivateChannel => "private_channel",
        Dm => "dm",
        GroupDm => "group_dm",
    }
}

named_values! {
    Direction {
        Inbound => "inbound",
        Outbound => "outbound",
    }
}

named_values! {
    /// Where an agent may send: the conversation it was woken in, that and the
    /// listed ones, or anywhere its identity is a member.
    Audience {
        Origin => "origin",
        Listed => "listed",
        Anywhere => "anywhere",
    }
}

named_values! {
    /// Whether replies in a thread wake the agent without a mention.
    ThreadFollowUp {
        Off => "off",
        WhenLastAddressed => "when_last_addressed",
        Always => "always",
    }
}

named_values! {
    /// Other agents' messages: ignored, or answered when they mention this one.
    AgentMessages {
        Ignore => "ignore",
        Mentions => "mentions",
    }
}

named_values! {
    /// Messages arriving while a run is busy in the thread: joined into it, or answered after it.
    MidRun {
        Steer => "steer",
        Collect => "collect",
    }
}

named_values! {
    /// Who may wake the agent: anyone in the team, or only the listed people.
    Senders {
        Anyone => "anyone",
        Listed => "listed",
    }
}

/// The tools every channel gives an agent, with what each does and its level
/// until the organization sets one, in the order they're listed. Sending
/// anywhere but the thread waits for approval; deleting is denied.
pub const CHANNEL_TOOLS: &[(&str, ToolEffect, ToolLevel)] = &[
    ("list_conversations", ToolEffect::Read, ToolLevel::Allow),
    ("get_conversation", ToolEffect::Read, ToolLevel::Allow),
    ("read_messages", ToolEffect::Read, ToolLevel::Allow),
    ("read_thread", ToolEffect::Read, ToolLevel::Allow),
    ("search_messages", ToolEffect::Read, ToolLevel::Allow),
    ("find_people", ToolEffect::Read, ToolLevel::Allow),
    ("get_person", ToolEffect::Read, ToolLevel::Allow),
    ("get_attachment", ToolEffect::Read, ToolLevel::Allow),
    ("reply", ToolEffect::Write, ToolLevel::Allow),
    ("send_message", ToolEffect::Write, ToolLevel::Approve),
    ("send_direct_message", ToolEffect::Write, ToolLevel::Approve),
    ("react", ToolEffect::Write, ToolLevel::Allow),
    ("edit_message", ToolEffect::Write, ToolLevel::Allow),
    ("delete_message", ToolEffect::Destructive, ToolLevel::Deny),
];

/// A channel tool's effect and default level; `None` for a name that isn't one.
pub fn channel_tool(name: &str) -> Option<(ToolEffect, ToolLevel)> {
    CHANNEL_TOOLS
        .iter()
        .find(|(tool, _, _)| *tool == name)
        .map(|(_, effect, level)| (*effect, *level))
}

/// The organization's channel settings until it saves its own.
pub const DEFAULT_BREAKER_RUNS: u32 = 20;
pub const DEFAULT_BREAKER_WINDOW_MINUTES: u32 = 10;
pub const DEFAULT_BREAKER_PAUSE_MINUTES: u32 = 10;
pub const DEFAULT_MESSAGE_RETENTION_DAYS: u32 = 30;

/// How long a configuration token lasts before it must be rotated, as Slack's do.
pub const TOKEN_LIFETIME_MS: i64 = 12 * 60 * 60 * 1000;

/// The first version of a sealed credential or secret; each replacement seals the next.
pub const FIRST_VERSION: u32 = 1;

/// The longest label a connection takes, in characters.
pub const MAX_LABEL: usize = 40;
/// The most items one page holds, and how many it holds by default.
pub const MAX_PAGE: u32 = 100;
pub const DEFAULT_PAGE: u32 = 25;

/// The provider's team, learned from the first install.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Team {
    pub id: Option<String>,
    pub name: Option<String>,
}

/// The contract's `SecretState` for a manager's configuration token: always set.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct TokenState {
    pub set: bool,
    pub last4: Option<String>,
    pub updated_at: Option<Timestamp>,
}

/// How Zupflow makes apps for one workspace's agents in one team. Never its tokens.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChannelAppManager {
    pub id: ChannelAppManagerId,
    pub organization_id: OrganizationId,
    pub workspace_id: WorkspaceId,
    pub provider: ChannelProviderKind,
    pub method: ManagerMethod,
    pub team: Team,
    pub status: ManagerStatus,
    pub configuration_token: TokenState,
    /// The version its tokens are sealed at.
    pub credential_version: u32,
    pub token_expires_at: Option<Timestamp>,
    /// Apps it made that aren't deleted, counted when read.
    pub app_count: u64,
    /// `None` when an API key or hosted Connect linked it, or the person is gone.
    pub created_by: Option<User>,
    pub created_at: Timestamp,
    pub updated_at: Timestamp,
}

/// One agent's generated app in one team. Never its secrets or inbound reference.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChannelApp {
    pub id: ChannelAppId,
    pub organization_id: OrganizationId,
    pub workspace_id: WorkspaceId,
    pub manager_id: ChannelAppManagerId,
    pub agent_id: AgentId,
    pub provider: ChannelProviderKind,
    pub external_app_id: Option<String>,
    pub status: AppStatus,
    /// A stable code, never the provider's text.
    pub last_error: Option<String>,
    /// Its connection that isn't disconnected, read with it.
    pub connection_id: Option<ChannelConnectionId>,
    pub created_at: Timestamp,
    pub updated_at: Timestamp,
}

/// The team a connection was installed in.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ConnectionTeam {
    pub id: String,
    pub name: Option<String>,
    pub enterprise_id: Option<String>,
}

/// Who the agent is in the team: the provider's ID and handle.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Identity {
    pub external_id: String,
    pub handle: String,
    /// Where the provider has one, the bot ID its posts may carry instead
    /// of `external_id` (Slack's `bot_id`, beside its `bot_user_id`): as the
    /// install reported it, or learned from its first post that carried
    /// both. Its own posts are known by either. Not shown.
    #[serde(skip)]
    pub bot_id: Option<String>,
}

/// An installed app: one team, one identity, one agent. Never its tokens.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChannelConnection {
    pub id: ChannelConnectionId,
    pub organization_id: OrganizationId,
    pub workspace_id: WorkspaceId,
    pub provider: ChannelProviderKind,
    pub app_id: Option<ChannelAppId>,
    /// Its app's agent, read with it.
    pub agent_id: Option<AgentId>,
    pub short_id: ShortId,
    pub label: String,
    pub handler: HandlerKind,
    pub status: ConnectionStatus,
    pub team: ConnectionTeam,
    pub identity: Identity,
    pub granted_scopes: Vec<String>,
    pub credential_version: Option<u32>,
    pub connected_at: Option<Timestamp>,
    pub last_event_at: Option<Timestamp>,
    pub created_at: Timestamp,
    pub updated_at: Timestamp,
}

/// A conversation the connection has seen a message in.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChannelConversation {
    pub id: ChannelConversationId,
    pub organization_id: OrganizationId,
    pub workspace_id: WorkspaceId,
    pub connection_id: ChannelConnectionId,
    pub external_id: String,
    pub kind: ConversationKind,
    pub name: Option<String>,
    pub is_external: bool,
    pub identity_is_member: bool,
    pub last_message_at: Option<Timestamp>,
    pub paused_until: Option<Timestamp>,
    pub created_at: Timestamp,
}

/// Who wrote a message, as the connection saw them.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ChannelPerson {
    pub id: ChannelPersonId,
    pub handle: Option<String>,
    pub display_name: Option<String>,
    pub is_automated: bool,
}

/// A message, as stored: its text rendered once and never rewritten.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChannelMessage {
    pub id: ChannelMessageId,
    pub organization_id: OrganizationId,
    pub workspace_id: WorkspaceId,
    pub connection_id: ChannelConnectionId,
    pub conversation_id: ChannelConversationId,
    pub external_id: String,
    pub thread_key: String,
    pub direction: Direction,
    /// `None` for the connection's own (outbound) messages.
    pub author: Option<ChannelPerson>,
    /// Exactly as received; a reader shows `""` once it's deleted.
    pub text: String,
    pub mentions: Vec<String>,
    pub run_id: Option<RunId>,
    pub tool_call_id: Option<ToolCallId>,
    pub agent_hop: u32,
    pub sent_at: Timestamp,
    pub edited_at: Option<Timestamp>,
    pub deleted_at: Option<Timestamp>,
    pub created_at: Timestamp,
}

/// How an agent's messages are signed, where the provider allows it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, serde::Deserialize)]
pub struct Persona {
    pub name: String,
    pub avatar_url: Option<String>,
}

/// An agent's settings on one connection, as checked by the caller.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct AgentChannelSettings {
    pub tools: Vec<String>,
    pub approval_required: Vec<String>,
    pub audience: Audience,
    pub audience_conversations: Vec<ChannelConversationId>,
    pub persona: Option<Persona>,
    pub answers_dms: bool,
    pub answers_mentions: bool,
    pub thread_follow_up: ThreadFollowUp,
    pub agent_messages: AgentMessages,
    pub max_agent_hops: u32,
    pub mid_run: MidRun,
    pub senders: Senders,
    pub sender_list: Vec<String>,
    pub allow_automated: bool,
    pub allow_external: bool,
    pub reply_with_output: bool,
}

impl Default for AgentChannelSettings {
    /// What an agent channel starts with when an install creates it.
    fn default() -> Self {
        Self {
            tools: Vec::new(),
            approval_required: Vec::new(),
            audience: Audience::Origin,
            audience_conversations: Vec::new(),
            persona: None,
            answers_dms: true,
            answers_mentions: true,
            thread_follow_up: ThreadFollowUp::WhenLastAddressed,
            agent_messages: AgentMessages::Ignore,
            max_agent_hops: 3,
            mid_run: MidRun::Steer,
            senders: Senders::Anyone,
            sender_list: Vec::new(),
            allow_automated: false,
            allow_external: false,
            reply_with_output: true,
        }
    }
}

/// The fewest and most agent hops a channel allows.
pub const MIN_AGENT_HOPS: u32 = 1;
pub const MAX_AGENT_HOPS: u32 = 5;

/// An agent on one of its own connections.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentChannel {
    pub id: AgentChannelId,
    pub organization_id: OrganizationId,
    pub workspace_id: WorkspaceId,
    pub agent_id: AgentId,
    pub connection_id: ChannelConnectionId,
    pub settings: AgentChannelSettings,
    pub created_at: Timestamp,
    pub updated_at: Timestamp,
}

/// The organization's level for one channel tool, in both environments.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChannelToolLevel {
    pub provider: ChannelProviderKind,
    pub tool: String,
    pub level: ToolLevel,
    /// Set by a person; `false` while it's Zupflow's default.
    pub confirmed: bool,
    pub effect: ToolEffect,
    pub updated_at: Option<Timestamp>,
    pub updated_by: Option<User>,
}

/// The organization's conversation breaker and message retention.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BreakerSettings {
    pub breaker_runs: u32,
    pub breaker_window_minutes: u32,
    pub breaker_pause_minutes: u32,
    pub message_retention_days: u32,
}

impl Default for BreakerSettings {
    fn default() -> Self {
        Self {
            breaker_runs: DEFAULT_BREAKER_RUNS,
            breaker_window_minutes: DEFAULT_BREAKER_WINDOW_MINUTES,
            breaker_pause_minutes: DEFAULT_BREAKER_PAUSE_MINUTES,
            message_retention_days: DEFAULT_MESSAGE_RETENTION_DAYS,
        }
    }
}

/// The contract's `ChannelSettings`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChannelSettings {
    pub organization_id: OrganizationId,
    pub values: BreakerSettings,
    /// `None` until saved.
    pub updated_at: Option<Timestamp>,
    pub updated_by: Option<User>,
}

/// What a channel Connect session sets up, instead of an app's installation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ConnectTarget {
    /// Links a team; `manager` is the manager it made, once completed.
    ChannelAppManager {
        provider: ChannelProviderKind,
        manager: Option<ChannelAppManagerId>,
    },
    /// Installs one agent's app; `connection` is the connection it made, once completed.
    ChannelApp {
        provider: ChannelProviderKind,
        app: ChannelAppId,
        connection: Option<ChannelConnectionId>,
    },
}

/// An app's inbound reference: 32 random base32 characters (`a-z`, `2-7`),
/// unique across every tenant. The provider's events for the app go to
/// `/channels/inbound/{reference}`, which is how an event finds its tenant.
/// Not an ID, and never shown by the API.
#[derive(Clone, PartialEq, Eq)]
pub struct InboundRef(String);

impl InboundRef {
    const ALPHABET: &'static [u8; 32] = b"abcdefghijklmnopqrstuvwxyz234567";
    pub const LENGTH: usize = 32;

    /// A new reference from the operating system's random source: 160 bits.
    pub fn generate() -> Result<Self, getrandom::Error> {
        let mut bytes = [0u8; Self::LENGTH];
        getrandom::fill(&mut bytes)?;
        // 256 is a multiple of 32: every character is equally likely.
        Ok(Self(
            bytes
                .iter()
                .map(|byte| char::from(Self::ALPHABET[usize::from(byte % 32)]))
                .collect(),
        ))
    }

    /// `value` if it has a reference's form.
    pub fn parse(value: &str) -> Option<Self> {
        let valid = value.len() == Self::LENGTH && value.bytes().all(|byte| Self::ALPHABET.contains(&byte));
        valid.then(|| Self(value.to_owned()))
    }

    pub fn expose(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Debug for InboundRef {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("InboundRef(..)")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_each_value_once_and_reads_it_back() {
        for status in AppStatus::ALL {
            assert_eq!(AppStatus::parse(status.as_str()), Some(*status));
        }
        assert_eq!(AppStatus::parse("any"), None);
        assert!(AppStatus::Installed.is_live() && !AppStatus::Failed.is_live() && !AppStatus::Deleted.is_live());
        assert_eq!(
            serde_json::to_value(ThreadFollowUp::WhenLastAddressed).ok(),
            Some(serde_json::json!("when_last_addressed"))
        );
    }

    #[test]
    fn lists_the_contracts_channel_tools() {
        assert_eq!(CHANNEL_TOOLS.len(), 14);
        assert_eq!(
            channel_tool("send_message"),
            Some((ToolEffect::Write, ToolLevel::Approve))
        );
        assert_eq!(
            channel_tool("delete_message"),
            Some((ToolEffect::Destructive, ToolLevel::Deny))
        );
        assert_eq!(channel_tool("post_anywhere"), None);
    }

    #[test]
    fn writes_a_target_as_the_contract_does() {
        let app = ChannelAppId::generate();
        assert_eq!(
            serde_json::to_value(ConnectTarget::ChannelApp {
                provider: ChannelProviderKind::Slack,
                app,
                connection: None
            })
            .ok(),
            Some(
                serde_json::json!({ "kind": "channel_app", "provider": "slack", "app": app.to_string(), "connection": null })
            )
        );
    }

    #[test]
    fn makes_long_random_inbound_references() {
        let one = InboundRef::generate().unwrap_or_else(|_| unreachable!());
        let two = InboundRef::generate().unwrap_or_else(|_| unreachable!());
        assert_eq!(one.expose().len(), 32);
        assert_ne!(one.expose(), two.expose());
        assert_eq!(InboundRef::parse(one.expose()), Some(one.clone()));
        assert_eq!(InboundRef::parse("A".repeat(32).as_str()), None);
        assert_eq!(InboundRef::parse("abc"), None);
        assert_eq!(format!("{one:?}"), "InboundRef(..)", "never printed");
    }
}
