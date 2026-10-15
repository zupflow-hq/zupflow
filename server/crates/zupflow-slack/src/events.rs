//! [`SlackChannelProvider`]: what Slack's Events API delivers to one app's
//! inbound URL, checked and read into Zupflow's terms.
//!
//! **Verifying.** Slack signs each delivery with the app's own signing
//! secret: `X-Slack-Signature` is `v0=` and the hex HMAC-SHA256 of
//! `v0:{X-Slack-Request-Timestamp}:{raw body}`. A timestamp more than five
//! minutes from the clock is refused before the signature is checked, and
//! the signature is compared in constant time over the body exactly as it
//! arrived. Nothing is read from a body before it's verified.
//!
//! **Reading.** A body that isn't JSON is [`InboundRefusal::InvalidJson`];
//! one without a `type`, [`InboundRefusal::Unreadable`].
//! `url_verification` is answered with its challenge. Each `event_callback`
//! is an [`InboundDelivery::Event`], recorded once by its `event_id`, with
//! its kind (the event's `type`) and the envelope's `team_id`, the team the
//! app is installed in (never the event's own `team`, which is the author's
//! in a shared channel; an envelope without one names no team):
//!
//! | Slack event | Zupflow |
//! | --- | --- |
//! | `message` (no subtype, `thread_broadcast`, `file_share`, `me_message`, `bot_message`) | [`InboundEvent::Message`] |
//! | `app_uninstalled`, `tokens_revoked` | [`InboundEvent::Uninstalled`] |
//! | `app_mention`, edits, deletions, anything else (joins, topic changes, unfurls, `app_rate_limited`) | [`InboundEvent::Other`] |
//!
//! A message's conversation is its `channel`, of the kind its
//! `channel_type` names (a message that names none is `Other`: its kind is
//! never guessed from the ID), shared with another organization when the
//! envelope says `is_ext_shared_channel` (Slack Connect); its ID is its
//! `ts`; its thread key the `thread_ts` it replies in, else its own `ts`
//! (or `""` at a DM's top level, so a DM is one conversation); its author
//! the `user` (or, for an app posting without one, its `bot_id`), of the
//! team in its `user_team`, else its `team`; automated when it carries a
//! `bot_id` (any app's post, with or without a user) or names no user; and
//! its mentions every `<@U…>` in order. A mention arrives twice, as
//! `app_mention` and as `message`, with different event IDs and the same
//! `ts`: only the `message` is read (it names its `channel_type`), so one
//! mention never wakes the agent twice. The connection's own messages (its
//! bot user's, or carrying its `bot_id`) are told apart by the store, which
//! knows the identity. A field longer than Slack sends makes the event
//! `Other`.
//!
//! Text stays in Slack's markup; a run renders it when it takes it in
//! ([`zupflow_core::channels::thread::plain_text`]).

use async_trait::async_trait;
use hmac::{Hmac, KeyInit, Mac};
use serde_json::{Map, Value};
use sha2::Sha256;
use zupflow_core::channels::provider::{
    ChannelProvider, INBOUND_TOLERANCE_SECONDS, InboundDelivery, InboundEvent, InboundRefusal, InboundRequest,
    NormalizedMessage, OutgoingPost, ProviderCallError, ProviderPerson, SignatureHeaders,
};
use zupflow_core::channels::{ChannelProviderKind, ConversationKind};
use zupflow_core::outbound::OutboundHttp;
use zupflow_core::time::Timestamp;

use crate::api::{SlackApi, SlackCode, SlackError};
use crate::send::{MARKDOWN_MAX, Post, SlackClient};
use crate::text;

/// The longest challenge, event ID, team or conversation ID read.
const FIELD_MAX: usize = 200;
/// The longest message text Slack sends, and Zupflow stores.
const TEXT_MAX: usize = 40_000;

/// The headers Slack signs a delivery with.
pub const SLACK_HEADERS: SignatureHeaders = SignatureHeaders {
    timestamp: "X-Slack-Request-Timestamp",
    signature: "X-Slack-Signature",
};

/// Slack, for every generated app: its Events API, and the Web API calls an
/// installed app makes with its bot token ([`SlackClient`]).
#[derive(Debug, Clone, Default)]
pub struct SlackChannelProvider {
    client: SlackClient,
}

/// Lowercase hex, as Slack writes a signature.
fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

/// `v0=` and 64 hex characters, as bytes; anything else is no signature.
fn signature_bytes(signature: &str) -> Option<[u8; 32]> {
    let digits = signature.strip_prefix("v0=")?;
    if digits.len() != 64 {
        return None;
    }
    let mut out = [0u8; 32];
    for (i, pair) in digits.as_bytes().chunks(2).enumerate() {
        let high = char::from(pair[0]).to_digit(16)?;
        let low = char::from(pair[1]).to_digit(16)?;
        out[i] = u8::try_from(high * 16 + low).ok()?;
    }
    Some(out)
}

/// Slack's signature for a delivery: `v0=` and the hex HMAC-SHA256 of
/// `v0:{timestamp}:{body}` under `signing_secret`. For tests and fakes that
/// sign as Slack does.
pub fn sign(signing_secret: &[u8], timestamp: &str, body: &[u8]) -> String {
    let mut mac = Hmac::<Sha256>::new_from_slice(signing_secret).expect("HMAC takes a key of any length");
    mac.update(b"v0:");
    mac.update(timestamp.as_bytes());
    mac.update(b":");
    mac.update(body);
    format!("v0={}", hex(&mac.finalize().into_bytes()))
}

/// A string field, non-empty and at most `max` characters.
fn field<'a>(object: &'a Map<String, Value>, name: &str, max: usize) -> Option<&'a str> {
    object
        .get(name)
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty() && value.chars().count() <= max)
}

/// A Slack `ts` (`"1712345678.000200"`) as a time: its seconds and microseconds.
pub fn ts_time(ts: &str) -> Option<Timestamp> {
    let (seconds, fraction) = ts.split_once('.').unwrap_or((ts, ""));
    let seconds: i64 = seconds.parse().ok()?;
    let micros: i64 = if fraction.is_empty() {
        0
    } else if fraction.len() <= 6 && fraction.bytes().all(|byte| byte.is_ascii_digit()) {
        format!("{fraction:0<6}").parse().ok()?
    } else {
        return None;
    };
    seconds
        .checked_mul(1000)
        .and_then(|millis| millis.checked_add(micros / 1000))
        .map(Timestamp::from_millis)
}

/// The kind a message names (`channel_type`); `None` when it names none
/// Zupflow knows, and the message isn't read.
fn conversation_kind(event: &Map<String, Value>) -> Option<ConversationKind> {
    match event.get("channel_type").and_then(Value::as_str)? {
        "channel" => Some(ConversationKind::Channel),
        "group" => Some(ConversationKind::PrivateChannel),
        "im" => Some(ConversationKind::Dm),
        "mpim" => Some(ConversationKind::GroupDm),
        _ => None,
    }
}

/// A thread key: the thread a message replies in, else its own `ts`, or
/// `""` at a DM's top level.
fn thread_key(message: &Map<String, Value>, ts: &str, kind: ConversationKind) -> String {
    match field(message, "thread_ts", FIELD_MAX) {
        Some(thread) => thread.to_owned(),
        None if kind == ConversationKind::Dm => String::new(),
        None => ts.to_owned(),
    }
}

/// The message's text, as Slack wrote it: `None` past [`TEXT_MAX`]
/// characters.
fn message_text(message: &Map<String, Value>) -> Option<String> {
    let text = message.get("text").and_then(Value::as_str).unwrap_or_default();
    (text.chars().count() <= TEXT_MAX).then(|| text.to_owned())
}

/// A field that may be absent (or empty): `None` when it's present but not
/// a string within `max`, which leaves the event unread.
fn optional<'a>(object: &'a Map<String, Value>, name: &str, max: usize) -> Option<Option<&'a str>> {
    match object.get(name) {
        None | Some(Value::Null) => Some(None),
        Some(Value::String(value)) if value.is_empty() => Some(None),
        Some(Value::String(value)) if value.chars().count() <= max => Some(Some(value.as_str())),
        Some(_) => None,
    }
}

/// A new message, or `None` for one Zupflow doesn't read: no conversation
/// kind, no author, or a field longer than Slack sends. `shared` is the
/// envelope's `is_ext_shared_channel`; `now` stands in for a `ts` that
/// isn't a time.
fn new_message(event: &Map<String, Value>, shared: bool, now: Timestamp) -> Option<NormalizedMessage> {
    let channel = field(event, "channel", FIELD_MAX)?;
    let ts = field(event, "ts", FIELD_MAX)?;
    let kind = conversation_kind(event)?;
    let text = message_text(event)?;
    let user = optional(event, "user", FIELD_MAX)?;
    let bot_id = optional(event, "bot_id", FIELD_MAX)?;
    let team = match optional(event, "user_team", FIELD_MAX)? {
        Some(team) => Some(team),
        None => optional(event, "team", FIELD_MAX)?,
    };
    let author = user.or(bot_id)?;
    Some(NormalizedMessage {
        conversation_external_id: channel.to_owned(),
        conversation_kind: kind,
        conversation_is_external: shared,
        author_external_id: Some(author.to_owned()),
        author_bot_id: bot_id.map(str::to_owned),
        author_team_id: team.map(str::to_owned),
        author_is_automated: bot_id.is_some() || user.is_none(),
        external_id: ts.to_owned(),
        thread_key: thread_key(event, ts, kind),
        mentions: text::mentions(&text),
        text,
        sent_at: ts_time(ts).unwrap_or(now),
    })
}

/// One event of an `event_callback`, in Zupflow's terms.
fn event(event: &Map<String, Value>, shared: bool, now: Timestamp) -> InboundEvent {
    match event.get("type").and_then(Value::as_str) {
        // Every mention also arrives as the `message` it is, which names its
        // conversation's kind: read once, from there.
        Some("message") => match event.get("subtype").and_then(Value::as_str) {
            None | Some("thread_broadcast" | "file_share" | "me_message" | "bot_message") => {
                new_message(event, shared, now)
                    .map_or(InboundEvent::Other, |message| InboundEvent::Message(Box::new(message)))
            }
            // Edits and deletions are recorded once, and change nothing
            // stored, or sent to a model, before.
            Some(_) => InboundEvent::Other,
        },
        Some("app_uninstalled" | "tokens_revoked") => InboundEvent::Uninstalled,
        _ => InboundEvent::Other,
    }
}

/// A call's failure in stable terms; `wrote` when the call may have
/// changed something though it failed, so it's settled before it's tried
/// again.
fn call_error(error: SlackError, wrote: bool) -> ProviderCallError {
    match error {
        SlackError::RateLimited { retry_after } => ProviderCallError::RateLimited {
            retry_after_secs: retry_after.as_secs().max(1),
        },
        SlackError::Refused(SlackCode::InvalidAuth) => ProviderCallError::TokenRefused,
        // Slack failing on its side, not refusing: as a `5xx` is.
        SlackError::Refused(code) if !code.transient() => ProviderCallError::Refused(code.as_str()),
        error if wrote && error.outcome_unknown() => ProviderCallError::OutcomeUnknown,
        _ => ProviderCallError::Unavailable,
    }
}

/// A Slack user's ID (`U…`, or `W…` in an Enterprise Grid): what
/// `users.info` reads. A bot ID (`B…`) names no user.
fn is_user_id(id: &str) -> bool {
    id.len() <= FIELD_MAX
        && (id.starts_with('U') || id.starts_with('W'))
        && id
            .bytes()
            .all(|byte| byte.is_ascii_uppercase() || byte.is_ascii_digit())
}

/// `text`, cut to what one `markdown` block holds, the cut marked.
fn fitted(text: &str) -> std::borrow::Cow<'_, str> {
    if text.chars().count() <= MARKDOWN_MAX {
        return text.into();
    }
    let mut kept: String = text.chars().take(MARKDOWN_MAX - 1).collect();
    kept.push('…');
    kept.into()
}

impl SlackChannelProvider {
    /// Calls Slack's Web API at `api`.
    pub fn new(api: SlackApi) -> Self {
        Self {
            client: SlackClient::new(api),
        }
    }

    /// Reads a verified body: see the module's table.
    pub fn read_delivery(body: &[u8], now: Timestamp) -> Result<InboundDelivery, InboundRefusal> {
        let value: Value = serde_json::from_slice(body).map_err(|_| InboundRefusal::InvalidJson)?;
        let Value::Object(envelope) = value else {
            return Err(InboundRefusal::Unreadable);
        };
        match envelope.get("type").and_then(Value::as_str) {
            Some("url_verification") => {
                let challenge = field(&envelope, "challenge", FIELD_MAX).ok_or(InboundRefusal::Unreadable)?;
                Ok(InboundDelivery::UrlVerification {
                    challenge: challenge.to_owned(),
                })
            }
            Some("event_callback") => {
                let Some((event_id, inner, kind)) = field(&envelope, "event_id", FIELD_MAX).and_then(|event_id| {
                    let inner = envelope.get("event").and_then(Value::as_object)?;
                    Some((event_id, inner, field(inner, "type", FIELD_MAX)?))
                }) else {
                    return Ok(InboundDelivery::Ignored);
                };
                let shared = envelope
                    .get("is_ext_shared_channel")
                    .and_then(Value::as_bool)
                    .unwrap_or(false);
                Ok(InboundDelivery::Event {
                    event_id: event_id.to_owned(),
                    kind: kind.to_owned(),
                    // The team the app is installed in; none is never read
                    // as the connection's.
                    team_id: field(&envelope, "team_id", FIELD_MAX).map(str::to_owned),
                    event: event(inner, shared, now),
                })
            }
            Some(_) => Ok(InboundDelivery::Ignored),
            None => Err(InboundRefusal::Unreadable),
        }
    }
}

#[async_trait]
impl ChannelProvider for SlackChannelProvider {
    fn kind(&self) -> ChannelProviderKind {
        ChannelProviderKind::Slack
    }

    fn signature_headers(&self) -> SignatureHeaders {
        SLACK_HEADERS
    }

    fn check_timestamp(&self, request: &InboundRequest<'_>, now: Timestamp) -> Result<(), InboundRefusal> {
        let timestamp = request.timestamp.unwrap_or_default();
        if timestamp.is_empty() || timestamp.len() > 12 || !timestamp.bytes().all(|byte| byte.is_ascii_digit()) {
            return Err(InboundRefusal::Stale);
        }
        let seconds: i64 = timestamp.parse().map_err(|_| InboundRefusal::Stale)?;
        if (now.millis() / 1000).abs_diff(seconds) > INBOUND_TOLERANCE_SECONDS.unsigned_abs() {
            return Err(InboundRefusal::Stale);
        }
        Ok(())
    }

    fn verify(
        &self,
        request: &InboundRequest<'_>,
        signing_secret: &[u8],
        now: Timestamp,
    ) -> Result<(), InboundRefusal> {
        self.check_timestamp(request, now)?;
        let timestamp = request.timestamp.unwrap_or_default();
        let expected = request
            .signature
            .and_then(signature_bytes)
            .ok_or(InboundRefusal::InvalidSignature)?;
        let mut mac = Hmac::<Sha256>::new_from_slice(signing_secret).map_err(|_| InboundRefusal::InvalidSignature)?;
        mac.update(b"v0:");
        mac.update(timestamp.as_bytes());
        mac.update(b":");
        mac.update(request.body);
        // Constant time.
        mac.verify_slice(&expected)
            .map_err(|_| InboundRefusal::InvalidSignature)
    }

    fn read(&self, body: &[u8], now: Timestamp) -> Result<InboundDelivery, InboundRefusal> {
        Self::read_delivery(body, now)
    }

    /// `users.info`; a bot ID, or a user Slack doesn't know or doesn't show
    /// the connection, is no one. Any other refusal says nothing of them.
    async fn person(
        &self,
        http: &dyn OutboundHttp,
        token: &str,
        external_id: &str,
    ) -> Result<Option<ProviderPerson>, ProviderCallError> {
        if !is_user_id(external_id) {
            return Ok(None);
        }
        match self.client.user_info(http, token, external_id).await {
            Ok(person) => Ok(Some(ProviderPerson {
                handle: person.handle,
                display_name: person.display_name,
                email: person.email,
                is_automated: person.is_automated,
            })),
            Err(SlackError::Refused(SlackCode::UserNotFound)) => Ok(None),
            Err(error) => Err(call_error(error, false)),
        }
    }

    /// `chat.postMessage`, in the thread, as one `markdown` block carrying
    /// the run's ID in its metadata.
    async fn post(
        &self,
        http: &dyn OutboundHttp,
        token: &str,
        post: &OutgoingPost<'_>,
    ) -> Result<String, ProviderCallError> {
        let text = fitted(post.text);
        let posted = self
            .client
            .post_message(
                http,
                token,
                &Post {
                    channel: post.conversation_external_id,
                    thread_ts: Some(post.thread_key).filter(|thread| !thread.is_empty()),
                    text: &text,
                    invocation_id: Some(post.send_id),
                    username: None,
                    icon_url: None,
                },
            )
            .await
            .map_err(|error| call_error(error, true))?;
        Ok(posted.ts)
    }

    /// Looks through the thread (or the conversation's top level) for the
    /// message carrying the run's ID.
    async fn find_sent(
        &self,
        http: &dyn OutboundHttp,
        token: &str,
        post: &OutgoingPost<'_>,
        attempted_at: Timestamp,
    ) -> Result<Option<String>, ProviderCallError> {
        self.client
            .find_sent(
                http,
                token,
                post.conversation_external_id,
                Some(post.thread_key).filter(|thread| !thread.is_empty()),
                post.send_id,
                attempted_at,
            )
            .await
            .map_err(|error| match error {
                SlackError::NoAnswer => ProviderCallError::OutcomeUnknown,
                error => call_error(error, false),
            })
    }
}
