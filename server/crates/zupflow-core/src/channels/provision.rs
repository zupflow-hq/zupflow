//! Generated apps: every agent on a channel gets its own app in the team, so
//! it's someone people there can mention and message. Customers never build
//! one by hand: Zupflow creates it from a manifest through an
//! [`AppProvisioner`], with the team's linked manager's tokens.
//!
//! The method behind a provisioner can change (a pasted configuration token
//! now; a provider's partner install later) without touching the tables, the
//! inbound job or the Console: both end in the same rows, an app, then a
//! connection.
//!
//! Tokens and secrets cross this trait as [`Zeroizing`] values, wiped on
//! drop and never printed. A provisioner's refusal is a stable code, never
//! the provider's own text. Every call that reaches the provider is given
//! the [`OutboundHttp`] it must make it through: a provisioner never builds
//! an HTTP client of its own.
//!
//! Making an app is at least once: a job's attempt can end between the
//! provider's answer and the row that records it. So the job marks the app
//! before it calls [`AppProvisioner::create`], records the provider's ID as
//! soon as it answers, and on a later attempt deletes what an earlier one
//! made ([`AppProvisioner::delete`], or [`AppProvisioner::find`] when the
//! answer was lost) before it makes another: an app's secrets are shown only
//! by the call that makes it, so one whose answer is lost can't be kept.

use async_trait::async_trait;
use zeroize::Zeroizing;

use super::{ChannelProviderKind, ConnectionTeam, Identity, ManagerMethod};
use crate::outbound::OutboundHttp;
use crate::time::Timestamp;

/// The scopes a generated app asks for: Slack's bot scopes for reading the
/// conversations it's in and writing to them.
pub const SLACK_SCOPES: &[&str] = &[
    "app_mentions:read",
    "channels:history",
    "channels:read",
    "chat:write",
    "groups:history",
    "im:history",
    "im:write",
    "mpim:history",
    "reactions:write",
    "users:read",
    "users:read.email",
];

/// The events a generated app subscribes to.
pub const SLACK_EVENTS: &[&str] = &[
    "app_mention",
    "message.channels",
    "message.groups",
    "message.im",
    "message.mpim",
    "app_uninstalled",
    "tokens_revoked",
];

/// The longest app name a provider takes.
pub const MAX_APP_NAME: usize = 35;

/// A linked team's tokens, as pasted or last rotated.
pub struct ManagerTokens {
    pub configuration_token: Zeroizing<String>,
    pub refresh_token: Zeroizing<String>,
}

impl std::fmt::Debug for ManagerTokens {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("ManagerTokens(..)")
    }
}

/// What the provider said of a pair of tokens it accepted.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CheckedTokens {
    /// When the configuration token must next be rotated, when the provider says.
    pub expires_at: Option<Timestamp>,
}

/// A rotated pair: the old refresh token is spent.
#[derive(Debug)]
pub struct RotatedTokens {
    pub tokens: ManagerTokens,
    pub expires_at: Option<Timestamp>,
}

/// An app's own secrets: its OAuth client secret, and the secret its events
/// are signed with. Sealed together, in the transaction that saves the app.
pub struct AppSecrets {
    pub client_secret: Zeroizing<String>,
    pub signing_secret: Zeroizing<String>,
}

impl std::fmt::Debug for AppSecrets {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("AppSecrets(..)")
    }
}

/// An app's description, built by Zupflow from the agent, never from a
/// request's text.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AppManifest {
    /// The agent's name, up to [`MAX_APP_NAME`] characters.
    pub name: String,
    pub description: String,
    /// The handle its identity posts as (in Slack, the bot user's name).
    pub handle: String,
    pub scopes: &'static [&'static str],
    pub events: &'static [&'static str],
    /// `{public_url}/channels/inbound/{inbound_ref}`: this app's own endpoint.
    pub events_url: String,
    /// Hosted Connect's callback, where an install comes back.
    pub redirect_url: String,
}

/// An install the provider made itself, with the connection's token.
pub struct ProviderInstall {
    pub team: ConnectionTeam,
    pub identity: Identity,
    pub granted_scopes: Vec<String>,
    pub token: Zeroizing<String>,
}

impl std::fmt::Debug for ProviderInstall {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ProviderInstall")
            .field("team", &self.team)
            .field("identity", &self.identity)
            .finish_non_exhaustive()
    }
}

/// What follows an app's creation.
#[derive(Debug)]
pub enum InstallStep {
    /// Someone in the team allows it on the provider's page, reached from a
    /// `channel_app` Connect session ([`AppProvisioner::consent_url`]).
    NeedsConsent,
    /// The provider installed it already (a partner install).
    Installed(ProviderInstall),
}

/// A newly made app.
#[derive(Debug)]
pub struct CreatedApp {
    /// The provider's ID for it (Slack: `A0…`).
    pub external_app_id: String,
    pub client_id: String,
    pub secrets: AppSecrets,
    pub install: InstallStep,
}

/// What the consent page is asked for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConsentRequest<'a> {
    pub client_id: &'a str,
    pub scopes: &'static [&'static str],
    pub redirect_url: &'a str,
    /// Names this attempt; comes back with the person.
    pub state: &'a str,
}

/// Why a provisioner didn't do what it was asked.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum ProvisionError {
    /// The provider refused the manager's tokens: expired, revoked or wrong.
    #[error("the provider refused the tokens")]
    TokensRefused,
    /// The provider refused the request itself; a stable code to show.
    #[error("the provider refused: {0}")]
    Refused(&'static str),
    /// The provider couldn't be reached, or answered unexpectedly; try again.
    /// A `create` that ends so may still have made the app.
    #[error("the provider is unavailable")]
    Unavailable,
    /// The provider holds no such app: deleted there already.
    #[error("the provider has no such app")]
    AppNotFound,
    /// This server has no provisioner for the provider.
    #[error("no provisioner is configured")]
    NotConfigured,
}

/// Creates, changes and deletes one agent's app in one team. Every call is
/// made with the manager's current tokens, through `http`; none is retried
/// here.
#[async_trait]
pub trait AppProvisioner: Send + Sync {
    fn method(&self) -> ManagerMethod;

    /// Checks a pasted pair with the provider before it's stored.
    async fn check_tokens(
        &self,
        http: &dyn OutboundHttp,
        provider: ChannelProviderKind,
        tokens: &ManagerTokens,
    ) -> Result<CheckedTokens, ProvisionError>;

    /// Spends the refresh token for a new pair. Called by one leased job per
    /// manager at a time; the new pair is stored only if the stored version
    /// is still the one these tokens were.
    async fn rotate_tokens(
        &self,
        http: &dyn OutboundHttp,
        tokens: &ManagerTokens,
    ) -> Result<RotatedTokens, ProvisionError>;

    async fn create(
        &self,
        http: &dyn OutboundHttp,
        tokens: &ManagerTokens,
        manifest: &AppManifest,
    ) -> Result<CreatedApp, ProvisionError>;

    /// The provider's ID for the app an earlier [`Self::create`] made with
    /// this events URL (unique to one Zupflow app), if it still holds one:
    /// how an attempt whose answer was lost is found and deleted. A
    /// provisioner whose provider can't look an app up so must make
    /// `create` itself answer the same app for the same events URL.
    async fn find(
        &self,
        http: &dyn OutboundHttp,
        tokens: &ManagerTokens,
        events_url: &str,
    ) -> Result<Option<String>, ProvisionError>;

    async fn update(
        &self,
        http: &dyn OutboundHttp,
        tokens: &ManagerTokens,
        external_app_id: &str,
        manifest: &AppManifest,
    ) -> Result<(), ProvisionError>;

    /// `AppNotFound` when the provider holds no such app.
    async fn delete(
        &self,
        http: &dyn OutboundHttp,
        tokens: &ManagerTokens,
        external_app_id: &str,
    ) -> Result<(), ProvisionError>;

    /// Where the person allows an app in their team: a URL built here,
    /// without reaching the provider.
    fn consent_url(&self, request: &ConsentRequest<'_>) -> Result<String, ProvisionError>;
}
