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
/// conversations it's in, writing to them, knowing who wrote, and showing
/// that it's working (`assistant:write`). Sorted, so a manifest built from
/// them is the same bytes every time.
pub const SLACK_SCOPES: &[&str] = &[
    "app_mentions:read",
    "assistant:write",
    "channels:history",
    "channels:read",
    "chat:write",
    "chat:write.customize",
    "groups:history",
    "groups:read",
    "im:history",
    "im:read",
    "im:write",
    "mpim:history",
    "mpim:read",
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
    // Its own join to a channel, to catch up on the mention that added it.
    "member_joined_channel",
];

/// The longest app name a provider takes.
pub const MAX_APP_NAME: usize = 35;

/// The job that removes a deleted app at its provider: its bot token
/// revoked, then the app deleted (Slack's `apps.manifest.delete`). Queued
/// by the store in the transaction that deletes the app, and again when its
/// team's tokens are pasted, until the app is recorded released, so no app
/// made at the provider is forgotten there. Its payload names the app and
/// its last connection, never a secret.
pub const RELEASE_CHANNEL_APP: &str = "release_channel_app";

/// The job that sends an app's manifest again after its agent's name or
/// description changed: queued by the store in the transaction that changes
/// the agent. Its payload names the app.
pub const SYNC_CHANNEL_APP: &str = "sync_channel_app";

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

impl AppManifest {
    /// 64 hex characters of SHA-256 over every field, for telling whether
    /// the provider already has this manifest.
    pub fn digest(&self) -> String {
        use sha2::{Digest, Sha256};
        let canonical = serde_json::json!({
            "name": self.name, "description": self.description, "handle": self.handle,
            "scopes": self.scopes, "events": self.events,
            "events_url": self.events_url, "redirect_url": self.redirect_url,
        });
        Sha256::digest(canonical.to_string().as_bytes())
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect()
    }
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
    /// The provider asked to wait this long before calling again (Slack's
    /// `Retry-After`): try again then, as a job, never by sleeping in a
    /// request. Nothing was done.
    #[error("the provider asked to wait")]
    RateLimited { retry_after_secs: u32 },
}

/// What the provider's consent page sent back, to exchange for the install:
/// the code, with the app's own client credentials.
pub struct ConsentExchange<'a> {
    pub client_id: &'a str,
    pub client_secret: &'a str,
    pub code: &'a str,
    /// The redirect URL the consent was asked with, exactly.
    pub redirect_url: &'a str,
}

impl std::fmt::Debug for ConsentExchange<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ConsentExchange")
            .field("client_id", &self.client_id)
            .finish_non_exhaustive()
    }
}

/// An install the person allowed: the team, the identity and its token, and
/// which app the provider says it installed.
#[derive(Debug)]
pub struct ConsentedInstall {
    /// The provider's ID for the app installed (Slack: `A0…`), when it says.
    pub external_app_id: Option<String>,
    pub install: ProviderInstall,
    /// Only when the app rotates its tokens (generated Slack apps don't).
    pub refresh_token: Option<Zeroizing<String>>,
    pub expires_at: Option<Timestamp>,
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

    /// Sends the whole manifest. The app-making job calls it right after
    /// `create`, once the app's secrets are stored, as the step that
    /// subscribes its events: a provider that checks an events URL with a
    /// request signed by the app's own secret (Slack) is answered then. A
    /// provisioner may so make the app without its events in `create`.
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

    /// Exchanges what the consent page sent back for the install. A
    /// provisioner whose provider installs apps itself has nothing to
    /// exchange: `NotConfigured`.
    async fn complete_consent(
        &self,
        _http: &dyn OutboundHttp,
        _exchange: &ConsentExchange<'_>,
    ) -> Result<ConsentedInstall, ProvisionError> {
        Err(ProvisionError::NotConfigured)
    }

    /// Revokes an installed app's token (in Slack, `auth.revoke`), so a
    /// removed connection's token stops working at the provider too. One
    /// already revoked is done. By default, nothing to revoke.
    async fn revoke_install(&self, _http: &dyn OutboundHttp, _token: &str) -> Result<(), ProvisionError> {
        Ok(())
    }
}
