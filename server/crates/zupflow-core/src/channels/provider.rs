//! What a channel provider does with an installed connection: check that a
//! delivery to an app's inbound endpoint was signed by that app, and read
//! what it says into Zupflow's own terms. The inbound route and job call it;
//! sending goes through the provider too once runs reach channels.
//!
//! A provider never trusts a delivery's body before [`ChannelProvider::verify`]
//! accepts its signature, and never passes the other side's text on in an
//! error: a refusal is one of [`InboundRefusal`]'s stable codes.

use async_trait::async_trait;

use super::{ChannelProviderKind, ConversationKind};
use crate::time::Timestamp;

/// How far a signed delivery's timestamp may be from the clock.
pub const INBOUND_TOLERANCE_SECONDS: i64 = 5 * 60;

/// A delivery as it arrived: the headers the provider signs with, and the
/// body exactly as sent.
#[derive(Debug, Clone, Copy)]
pub struct InboundRequest<'a> {
    pub timestamp: Option<&'a str>,
    pub signature: Option<&'a str>,
    pub body: &'a [u8],
}

/// Why a delivery is refused before anything is stored.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum InboundRefusal {
    /// The timestamp is missing or too far from the clock.
    #[error("stale_request")]
    Stale,
    /// Not signed by the app it was sent to.
    #[error("invalid_signature")]
    InvalidSignature,
    /// Signed, but not JSON.
    #[error("invalid_json")]
    InvalidJson,
    /// Signed, but not something the provider sends.
    #[error("validation_failed")]
    Unreadable,
}

/// The headers a provider signs its deliveries with.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SignatureHeaders {
    pub timestamp: &'static str,
    pub signature: &'static str,
}

/// A verified delivery, in Zupflow's terms.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum InboundDelivery {
    /// The provider checking the endpoint: answered with its challenge.
    UrlVerification { challenge: String },
    /// An event, once per `event_id`. `kind` is the provider's name for
    /// it; `team_id` is `None` when the event names no team, which is never
    /// read as the connection's.
    Event {
        event_id: String,
        kind: String,
        team_id: Option<String>,
        event: InboundEvent,
    },
    /// Anything the provider sends that Zupflow doesn't act on.
    Ignored,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum InboundEvent {
    Message(Box<NormalizedMessage>),
    /// The app was removed from the team, or its tokens revoked.
    Uninstalled,
    /// Something Zupflow doesn't act on: recorded once, nothing more.
    Other,
}

/// A message as the provider normalized it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NormalizedMessage {
    pub conversation_external_id: String,
    pub conversation_kind: ConversationKind,
    /// Shared with another organization's team (Slack Connect).
    pub conversation_is_external: bool,
    /// The author's provider ID: its user, or, for an app posting without
    /// one, its bot ID.
    pub author_external_id: Option<String>,
    /// The bot ID an app's post carries (Slack's `bot_id`), with or without
    /// a user: how a connection knows its own posts by either.
    pub author_bot_id: Option<String>,
    /// The team the author belongs to, when the provider says: another
    /// organization's in a shared conversation.
    pub author_team_id: Option<String>,
    /// Posted by an app or an integration: any post that carries a bot ID.
    pub author_is_automated: bool,
    pub external_id: String,
    pub thread_key: String,
    pub text: String,
    pub mentions: Vec<String>,
    pub sent_at: Timestamp,
}

/// One provider's inbound side.
#[async_trait]
pub trait ChannelProvider: Send + Sync {
    fn kind(&self) -> ChannelProviderKind;

    /// Where a delivery carries its timestamp and signature.
    fn signature_headers(&self) -> SignatureHeaders;

    /// Whether the delivery's timestamp is within
    /// [`INBOUND_TOLERANCE_SECONDS`] of `now`: checked first, before the
    /// app it names is looked up.
    fn check_timestamp(&self, request: &InboundRequest<'_>, now: Timestamp) -> Result<(), InboundRefusal>;

    /// Accepts a delivery only when its timestamp is fresh and it's signed
    /// with `signing_secret`. Compares in constant time.
    fn verify(&self, request: &InboundRequest<'_>, signing_secret: &[u8], now: Timestamp)
    -> Result<(), InboundRefusal>;

    /// Reads a verified delivery; `now` stands in for a time the event
    /// doesn't give.
    fn read(&self, body: &[u8], now: Timestamp) -> Result<InboundDelivery, InboundRefusal>;
}
