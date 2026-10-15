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
    /// Signed, but not something the provider sends.
    #[error("validation_failed")]
    Unreadable,
}

/// A verified delivery, in Zupflow's terms.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum InboundDelivery {
    /// The provider checking the endpoint: answered with its challenge.
    UrlVerification { challenge: String },
    /// An event, once per `event_id`.
    Event {
        event_id: String,
        team_id: String,
        event: InboundEvent,
    },
    /// Anything the provider sends that Zupflow doesn't act on.
    Ignored,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum InboundEvent {
    Message(NormalizedMessage),
    /// The app was removed from the team, or its tokens revoked.
    Uninstalled,
}

/// A message as the provider normalized it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NormalizedMessage {
    pub conversation_external_id: String,
    pub conversation_kind: ConversationKind,
    /// The author's provider ID; `None` for an app posting without a user.
    pub author_external_id: Option<String>,
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

    /// Accepts a delivery only when it's signed with `signing_secret` within
    /// [`INBOUND_TOLERANCE_SECONDS`] of `now`. Compares in constant time.
    fn verify(&self, request: &InboundRequest<'_>, signing_secret: &[u8], now: Timestamp)
    -> Result<(), InboundRefusal>;

    /// Reads a verified delivery.
    fn read(&self, body: &[u8]) -> Result<InboundDelivery, InboundRefusal>;
}
