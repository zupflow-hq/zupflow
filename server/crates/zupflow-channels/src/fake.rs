//! The fake channel provider: Slack's wire format, with the team's chat in
//! memory.
//!
//! [`FakeChannelProvider`] verifies and reads deliveries exactly as Slack
//! sends them: signed `v0` with the app's signing secret over
//! `v0:{timestamp}:{body}`, the timestamp within
//! [`INBOUND_TOLERANCE_SECONDS`] of the clock, and events as Slack's Events
//! API shapes them. It normalizes what Zupflow acts on (a message, an
//! uninstall, an edit by its author, a deletion) and leaves the rest as an
//! event recorded once and nothing more.
//!
//! [`FakeChat`] is the other side: a team's chat, in memory, that people
//! post to, edit and delete in. Each change becomes the delivery Slack would
//! make to an app, for a test or a development server to send to the
//! inbound endpoint.
//!
//! A provider made [`FakeChannelProvider::in_chat`] also acts in that chat
//! with a connection's token, as the app the chat knows by it
//! ([`FakeChat::install`]): it posts (carrying the send's ID, so a post
//! whose answer was lost is found), edits and deletes its own posts, reacts,
//! opens a DM and shows that it's working, and records each for a test to
//! read. It still calls no one. Made without a chat, it reads deliveries and
//! does nothing else.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use hmac::{Hmac, KeyInit, Mac};
use serde_json::{Map, Value, json};
use sha2::Sha256;
use zupflow_core::channels::provider::{
    ChannelProvider, DeletedMessage, EditedMessage, INBOUND_TOLERANCE_SECONDS, InboundDelivery, InboundEvent,
    InboundRefusal, InboundRequest, MessageRef, NormalizedMessage, OutgoingPost, ProviderCallError, SignatureHeaders,
};
use zupflow_core::channels::{ChannelProviderKind, ConversationKind};
use zupflow_core::outbound::OutboundHttp;
use zupflow_core::time::Timestamp;

/// Slack's signing headers.
pub const SLACK_HEADERS: SignatureHeaders = SignatureHeaders {
    timestamp: "X-Slack-Request-Timestamp",
    signature: "X-Slack-Signature",
};

/// The longest event ID, channel, user and timestamp Zupflow reads.
const MAX_EVENT_ID: usize = 200;
const MAX_ID: usize = 100;
/// The longest message text Zupflow reads.
const MAX_TEXT: usize = 40_000;

/// Slack's `v0` signature of a body sent at `timestamp`.
pub fn slack_signature(signing_secret: &[u8], timestamp: &str, body: &[u8]) -> String {
    let Ok(mut mac) = Hmac::<Sha256>::new_from_slice(signing_secret) else {
        // HMAC takes a key of any length.
        return String::new();
    };
    mac.update(b"v0:");
    mac.update(timestamp.as_bytes());
    mac.update(b":");
    mac.update(body);
    let digest = mac.finalize().into_bytes();
    let mut signature = String::with_capacity(3 + digest.len() * 2);
    signature.push_str("v0=");
    for byte in digest {
        signature.push_str(&format!("{byte:02x}"));
    }
    signature
}

fn hex(text: &str) -> Option<Vec<u8>> {
    if !text.len().is_multiple_of(2) {
        return None;
    }
    (0..text.len())
        .step_by(2)
        .map(|at| u8::from_str_radix(text.get(at..at + 2)?, 16).ok())
        .collect()
}

/// The fake provider: Slack's inbound side, in full, and, in a chat, the
/// connection's own side of it.
#[derive(Debug, Clone, Default)]
pub struct FakeChannelProvider {
    chat: Option<Arc<FakeChat>>,
}

impl FakeChannelProvider {
    /// A provider that reads deliveries and acts nowhere: anything it's
    /// asked to do is `Unsupported`.
    pub fn inbound_only() -> Self {
        Self::default()
    }

    /// A provider that acts in `chat`, as the app each token names there.
    pub fn in_chat(chat: Arc<FakeChat>) -> Self {
        Self { chat: Some(chat) }
    }

    fn chat(&self) -> Result<&FakeChat, ProviderCallError> {
        self.chat.as_deref().ok_or(ProviderCallError::Unsupported)
    }
}

#[async_trait]
impl ChannelProvider for FakeChannelProvider {
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
        let given = request
            .signature
            .and_then(|signature| signature.strip_prefix("v0="))
            .and_then(hex)
            .ok_or(InboundRefusal::InvalidSignature)?;
        let mut mac = Hmac::<Sha256>::new_from_slice(signing_secret).map_err(|_| InboundRefusal::InvalidSignature)?;
        mac.update(b"v0:");
        mac.update(timestamp.as_bytes());
        mac.update(b":");
        mac.update(request.body);
        // Constant time.
        mac.verify_slice(&given).map_err(|_| InboundRefusal::InvalidSignature)
    }

    fn read(&self, body: &[u8], now: Timestamp) -> Result<InboundDelivery, InboundRefusal> {
        let value: Value = serde_json::from_slice(body).map_err(|_| InboundRefusal::InvalidJson)?;
        let kind = value
            .get("type")
            .and_then(Value::as_str)
            .ok_or(InboundRefusal::Unreadable)?;
        if kind == "url_verification" {
            let challenge = value.get("challenge").and_then(Value::as_str).unwrap_or_default();
            return Ok(InboundDelivery::UrlVerification {
                challenge: challenge.to_owned(),
            });
        }
        let text = |field: &str| value.get(field).and_then(Value::as_str);
        // Slack names the team on every event: one without it is ignored,
        // never read as the connection's.
        let (Some(event_id), Some(team_id), Some(event)) = (
            text("event_id").filter(|id| (1..=MAX_EVENT_ID).contains(&id.len())),
            text("team_id").filter(|id| !id.is_empty()),
            value.get("event").and_then(Value::as_object),
        ) else {
            return Ok(InboundDelivery::Ignored);
        };
        if kind != "event_callback" {
            return Ok(InboundDelivery::Ignored);
        }
        let Some(event_kind) = event.get("type").and_then(Value::as_str) else {
            return Ok(InboundDelivery::Ignored);
        };
        // Slack marks a delivery from a channel shared with another
        // organization's team (Slack Connect).
        let shared = value
            .get("is_ext_shared_channel")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        let subtype = event.get("subtype").and_then(Value::as_str);
        let read = match (event_kind, subtype) {
            ("app_uninstalled" | "tokens_revoked", _) => InboundEvent::Uninstalled,
            ("message", Some("message_changed")) => {
                edited(event, now).map_or(InboundEvent::Other, |edited| InboundEvent::Edited(Box::new(edited)))
            }
            ("message", Some("message_deleted")) => {
                deleted(event, now).map_or(InboundEvent::Other, |deleted| InboundEvent::Deleted(Box::new(deleted)))
            }
            _ => message(event, shared, now).map_or(InboundEvent::Other, |read| InboundEvent::Message(Box::new(read))),
        };
        Ok(InboundDelivery::Event {
            event_id: event_id.to_owned(),
            kind: event_kind.to_owned(),
            team_id: Some(team_id.to_owned()),
            event: read,
        })
    }

    async fn post(
        &self,
        _http: &dyn OutboundHttp,
        token: &str,
        post: &OutgoingPost<'_>,
    ) -> Result<String, ProviderCallError> {
        let (chat, poster) = self.poster(token)?;
        chat.app_post(&poster, post)
    }

    async fn find_sent(
        &self,
        _http: &dyn OutboundHttp,
        token: &str,
        post: &OutgoingPost<'_>,
        _attempted_at: Timestamp,
    ) -> Result<Option<String>, ProviderCallError> {
        let (chat, poster) = self.poster(token)?;
        if chat.chat().searches_lag {
            return Ok(None);
        }
        Ok(chat
            .chat()
            .messages
            .iter()
            .find(|message| {
                message.channel == post.conversation_external_id
                    && message.user == poster.user
                    && message.send_id.as_deref() == Some(post.send_id)
            })
            .map(|message| message.ts.clone()))
    }

    async fn edit(
        &self,
        _http: &dyn OutboundHttp,
        token: &str,
        message: &MessageRef<'_>,
        text: &str,
    ) -> Result<(), ProviderCallError> {
        let (chat, poster) = self.poster(token)?;
        let mut held = chat.chat();
        let found = held
            .messages
            .iter_mut()
            .find(|held| held.channel == message.conversation_external_id && held.ts == message.external_id)
            .ok_or(ProviderCallError::Refused("message_not_found"))?;
        if found.user != poster.user {
            return Err(ProviderCallError::Refused("cant_update_message"));
        }
        found.text = text.to_owned();
        found.edited = true;
        Ok(())
    }

    async fn delete(
        &self,
        _http: &dyn OutboundHttp,
        token: &str,
        message: &MessageRef<'_>,
    ) -> Result<(), ProviderCallError> {
        let (chat, poster) = self.poster(token)?;
        let mut held = chat.chat();
        let Some(at) = held
            .messages
            .iter()
            .position(|held| held.channel == message.conversation_external_id && held.ts == message.external_id)
        else {
            // Already gone: deleted.
            return Ok(());
        };
        if held.messages[at].user != poster.user {
            return Err(ProviderCallError::Refused("cant_delete_message"));
        }
        let removed = held.messages.remove(at);
        held.deleted.push(removed);
        Ok(())
    }

    async fn react(
        &self,
        _http: &dyn OutboundHttp,
        token: &str,
        message: &MessageRef<'_>,
        emoji: &str,
        remove: bool,
    ) -> Result<(), ProviderCallError> {
        let (chat, poster) = self.poster(token)?;
        let mut held = chat.chat();
        if !held
            .messages
            .iter()
            .any(|held| held.channel == message.conversation_external_id && held.ts == message.external_id)
        {
            return Err(ProviderCallError::Refused("message_not_found"));
        }
        let reaction = Reaction {
            channel: message.conversation_external_id.to_owned(),
            ts: message.external_id.to_owned(),
            emoji: emoji.to_owned(),
            user: poster.user.unwrap_or_default(),
        };
        if remove {
            held.reactions.remove(&reaction);
        } else {
            held.reactions.insert(reaction);
        }
        Ok(())
    }

    async fn open_direct(
        &self,
        _http: &dyn OutboundHttp,
        token: &str,
        person: &str,
    ) -> Result<String, ProviderCallError> {
        self.poster(token)?;
        // The chat's DM with `U0ANA` is `D0ANA`.
        match person.strip_prefix('U') {
            Some(rest) if !rest.is_empty() => Ok(format!("D{rest}")),
            _ => Err(ProviderCallError::Refused("user_not_found")),
        }
    }

    async fn show_working(
        &self,
        _http: &dyn OutboundHttp,
        token: &str,
        conversation_external_id: &str,
        thread_key: &str,
        working: bool,
    ) -> bool {
        let Ok((chat, _)) = self.poster(token) else {
            return false;
        };
        if thread_key.is_empty() {
            return false;
        }
        chat.chat()
            .working
            .push((conversation_external_id.to_owned(), thread_key.to_owned(), working));
        true
    }
}

/// The provider's side, in its chat: what an app does with its token.
impl FakeChannelProvider {
    fn poster(&self, token: &str) -> Result<(&FakeChat, Poster), ProviderCallError> {
        let chat = self.chat()?;
        let poster = chat
            .chat()
            .apps
            .get(token)
            .cloned()
            .ok_or(ProviderCallError::TokenRefused)?;
        Ok((chat, poster))
    }
}

/// A text field of at most `max` bytes, not empty.
fn field<'a>(object: &'a Map<String, Value>, name: &str, max: usize) -> Option<&'a str> {
    object
        .get(name)
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty() && value.len() <= max)
}

/// The instant a Slack timestamp (`seconds.sequence`) names.
fn ts_time(ts: &str) -> Option<Timestamp> {
    ts.parse::<f64>()
        .ok()
        .map(|seconds| (seconds * 1000.0).round())
        .filter(|millis| millis.is_finite() && *millis > 0.0 && *millis < 9.0e15)
        .map(|millis| {
            // Within i64, checked above.
            #[allow(clippy::cast_possible_truncation)]
            Timestamp::from_millis(millis as i64)
        })
}

/// An edit by the message's author (`message.edited`); Slack also sends
/// `message_changed` when it unfurls a link, which isn't one.
fn edited(event: &Map<String, Value>, now: Timestamp) -> Option<EditedMessage> {
    let channel = field(event, "channel", MAX_ID)?;
    let message = event.get("message").and_then(Value::as_object)?;
    let edit = message.get("edited").and_then(Value::as_object)?;
    let ts = field(message, "ts", MAX_ID)?;
    let text = match message.get("text") {
        None => "",
        Some(Value::String(text)) if text.len() <= MAX_TEXT => text,
        Some(_) => return None,
    };
    Some(EditedMessage {
        conversation_external_id: channel.to_owned(),
        external_id: ts.to_owned(),
        text: text.to_owned(),
        edited_at: field(edit, "ts", MAX_ID).and_then(ts_time).unwrap_or(now),
    })
}

/// A deletion: the message it removed, and when.
fn deleted(event: &Map<String, Value>, now: Timestamp) -> Option<DeletedMessage> {
    let channel = field(event, "channel", MAX_ID)?;
    let ts = field(event, "deleted_ts", MAX_ID)?;
    Some(DeletedMessage {
        conversation_external_id: channel.to_owned(),
        external_id: ts.to_owned(),
        deleted_at: field(event, "ts", MAX_ID).and_then(ts_time).unwrap_or(now),
    })
}

/// Slack's conversation kinds, by `channel_type`; a `D` channel is a DM.
fn conversation_kind(channel: &str, channel_type: Option<&str>) -> ConversationKind {
    match channel_type {
        Some("channel") => ConversationKind::Channel,
        Some("group") => ConversationKind::PrivateChannel,
        Some("im") => ConversationKind::Dm,
        Some("mpim") => ConversationKind::GroupDm,
        _ if channel.starts_with('D') => ConversationKind::Dm,
        _ => ConversationKind::Channel,
    }
}

/// The user IDs a Slack message mentions, in order: `<@U123>` or `<@U123|name>`.
pub fn mentions(text: &str) -> Vec<String> {
    let mut found = Vec::new();
    let mut rest = text;
    while let Some(at) = rest.find("<@") {
        let after = &rest[at + 2..];
        let id: String = after
            .chars()
            .take_while(|c| c.is_ascii_uppercase() || c.is_ascii_digit())
            .collect();
        let tail = &after[id.len()..];
        let closes = tail.starts_with('>') || (tail.starts_with('|') && tail.contains('>'));
        if !id.is_empty() && closes {
            found.push(id);
        }
        rest = after;
    }
    found
}

/// A `message` or `app_mention` event, normalized; `None` for a subtype
/// other than an app's message, or a message with no author. Any post that
/// carries a bot ID is an app's, whether or not it names a user; the
/// author's team is `user_team`, or the event's `team`.
fn message(event: &Map<String, Value>, shared: bool, now: Timestamp) -> Option<NormalizedMessage> {
    let text = |field: &str| event.get(field).and_then(Value::as_str);
    let bounded = |field: &str, max: usize| match event.get(field) {
        None => Some(None),
        Some(Value::String(value)) if value.len() <= max => Some(Some(value.as_str())),
        Some(_) => None,
    };
    if !matches!(text("type"), Some("message" | "app_mention")) {
        return None;
    }
    let subtype = bounded("subtype", usize::MAX)?;
    if subtype.is_some_and(|subtype| subtype != "bot_message") {
        return None;
    }
    let channel = bounded("channel", MAX_ID)?.filter(|channel| !channel.is_empty())?;
    let user = bounded("user", MAX_ID)?.filter(|user| !user.is_empty());
    let app = bounded("bot_id", MAX_ID)?.filter(|app| !app.is_empty());
    let team = match bounded("user_team", MAX_ID)? {
        Some(team) => Some(team),
        None => bounded("team", MAX_ID)?,
    }
    .filter(|team| !team.is_empty());
    let body = bounded("text", MAX_TEXT)?.unwrap_or_default();
    let ts = bounded("ts", MAX_ID)?.filter(|ts| !ts.is_empty())?;
    let thread_ts = bounded("thread_ts", MAX_ID)?;
    let channel_type = bounded("channel_type", usize::MAX)?;
    let author = user.or(app)?;
    let kind = conversation_kind(channel, channel_type);
    let sent_at = ts_time(ts).unwrap_or(now);
    Some(NormalizedMessage {
        conversation_external_id: channel.to_owned(),
        conversation_kind: kind,
        conversation_is_external: shared,
        author_external_id: Some(author.to_owned()),
        author_bot_id: app.map(str::to_owned),
        author_team_id: team.map(str::to_owned),
        author_is_automated: user.is_none() || app.is_some() || subtype == Some("bot_message"),
        external_id: ts.to_owned(),
        thread_key: match thread_ts {
            Some(thread) => thread.to_owned(),
            None if kind == ConversationKind::Dm => String::new(),
            None => ts.to_owned(),
        },
        text: body.to_owned(),
        mentions: mentions(body),
        sent_at,
    })
}

/// A delivery as Slack would send it to one app: the body, and the headers
/// it's signed with.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SignedDelivery {
    pub body: Vec<u8>,
    pub timestamp: String,
    pub signature: String,
}

/// Signs a delivery body for an app, as sent at `now`.
pub fn sign(body: &Value, signing_secret: &[u8], now: Timestamp) -> SignedDelivery {
    let body = body.to_string().into_bytes();
    let timestamp = (now.millis() / 1000).to_string();
    let signature = slack_signature(signing_secret, &timestamp, &body);
    SignedDelivery {
        body,
        timestamp,
        signature,
    }
}

/// A message in the fake chat.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChatMessage {
    pub channel: String,
    pub channel_type: String,
    /// Who posted it: a person's user ID, or an app's user; `None` for an
    /// app posting with its bot ID alone.
    pub user: Option<String>,
    /// An app's post carries its bot ID.
    pub bot_id: Option<String>,
    /// Slack's `bot_message` subtype.
    pub automated: bool,
    /// The team the poster belongs to.
    pub user_team: String,
    /// Posted in a channel shared with another organization's team (by
    /// then).
    pub shared: bool,
    pub ts: String,
    pub thread_ts: Option<String>,
    pub text: String,
    /// What an app's post carries to be found by: its send's ID.
    pub send_id: Option<String>,
    /// Its text was changed since it was posted.
    pub edited: bool,
}

/// An app's reaction to a message.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct Reaction {
    pub channel: String,
    pub ts: String,
    pub emoji: String,
    /// The app's user.
    pub user: String,
}

impl ChatMessage {
    /// The `event` of a delivery of this message.
    pub fn event(&self) -> Value {
        let mut event = json!({
            "type": "message",
            "channel": self.channel,
            "channel_type": self.channel_type,
            "text": self.text,
            "ts": self.ts,
            "user_team": self.user_team,
        });
        if let Some(user) = &self.user {
            event["user"] = json!(user);
        }
        if let Some(bot_id) = &self.bot_id {
            event["bot_id"] = json!(bot_id);
        }
        if let Some(thread) = &self.thread_ts {
            event["thread_ts"] = json!(thread);
        }
        if self.automated {
            event["subtype"] = json!("bot_message");
        }
        event
    }
}

/// Who posts, and how: [`FakeChat::post_as`].
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Poster {
    /// A person's user ID, or an app's user; `None` for an app posting
    /// with its bot ID alone (as Slack sends a post signed with a persona).
    pub user: Option<String>,
    pub bot_id: Option<String>,
    /// As Slack's `bot_message` subtype.
    pub automated: bool,
    /// Another organization's team, for someone in a shared channel; the
    /// chat's own when `None`.
    pub team: Option<String>,
}

impl Poster {
    /// A person of the chat's team.
    pub fn person(user: &str) -> Self {
        Self {
            user: Some(user.to_owned()),
            ..Self::default()
        }
    }

    /// An app posting as its user, with its bot ID: what Slack delivers
    /// for an app's `chat.postMessage`.
    pub fn app(user: &str, bot_id: &str) -> Self {
        Self {
            user: Some(user.to_owned()),
            bot_id: Some(bot_id.to_owned()),
            ..Self::default()
        }
    }

    /// An app posting with its bot ID alone, as Slack's `bot_message`.
    pub fn app_without_user(bot_id: &str) -> Self {
        Self {
            bot_id: Some(bot_id.to_owned()),
            automated: true,
            ..Self::default()
        }
    }

    /// Someone of another organization's team, in a shared channel.
    pub fn external(user: &str, team: &str) -> Self {
        Self {
            user: Some(user.to_owned()),
            team: Some(team.to_owned()),
            ..Self::default()
        }
    }
}

#[derive(Debug, Default)]
struct Chat {
    messages: Vec<ChatMessage>,
    deleted: Vec<ChatMessage>,
    /// Channels shared with another organization's team.
    shared: BTreeSet<String>,
    events: u64,
    ticks: u64,
    /// The latest second anything was posted at: an app's post is stamped
    /// then, as the provider has no clock of its own.
    latest_second: i64,
    /// Each installed app's token, and who it posts as.
    apps: BTreeMap<String, Poster>,
    reactions: BTreeSet<Reaction>,
    /// Each time an app showed it was working, or stopped: the
    /// conversation, the thread, and which.
    working: Vec<(String, String, bool)>,
    /// Answers to an app's posts to lose: the post is made, its answer
    /// never arrives.
    lose: u32,
    /// While set, a search for an app's post finds nothing yet, as a
    /// provider's search lags its posts.
    searches_lag: bool,
}

impl Chat {
    /// A timestamp unique in the chat, at `seconds`.
    fn next_ts(&mut self, seconds: i64) -> String {
        self.ticks += 1;
        self.latest_second = self.latest_second.max(seconds);
        format!("{seconds}.{:06}", self.ticks % 1_000_000)
    }
}

/// One team's chat, in memory: people post, and each post is the delivery
/// Slack would make to an app in the team.
#[derive(Debug)]
pub struct FakeChat {
    team_id: String,
    chat: Mutex<Chat>,
}

impl FakeChat {
    pub fn new(team_id: &str) -> Self {
        Self {
            team_id: team_id.to_owned(),
            chat: Mutex::new(Chat::default()),
        }
    }

    pub fn team_id(&self) -> &str {
        &self.team_id
    }

    fn chat(&self) -> std::sync::MutexGuard<'_, Chat> {
        // A test that panicked while holding it leaves the chat as it was.
        self.chat.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// Posts a message, at `now` with a timestamp unique in the chat, and
    /// returns it.
    pub fn post(
        &self,
        channel: &str,
        channel_type: &str,
        user: &str,
        text: &str,
        thread_ts: Option<&str>,
        now: Timestamp,
    ) -> ChatMessage {
        self.post_as(channel, channel_type, &Poster::person(user), text, thread_ts, now)
    }

    /// Posts a message as an integration, Slack's `bot_message` with the
    /// user it names and a bot ID of its own.
    pub fn post_automated(
        &self,
        channel: &str,
        channel_type: &str,
        user: &str,
        text: &str,
        thread_ts: Option<&str>,
        now: Timestamp,
    ) -> ChatMessage {
        let poster = Poster {
            user: Some(user.to_owned()),
            bot_id: Some(format!("B{user}")),
            automated: true,
            team: None,
        };
        self.post_as(channel, channel_type, &poster, text, thread_ts, now)
    }

    /// Posts a message as `poster`. Someone of another team posting shares
    /// the channel: every message in it is delivered as in a shared one.
    pub fn post_as(
        &self,
        channel: &str,
        channel_type: &str,
        poster: &Poster,
        text: &str,
        thread_ts: Option<&str>,
        now: Timestamp,
    ) -> ChatMessage {
        let mut chat = self.chat();
        let ts = chat.next_ts(now.millis().div_euclid(1000));
        if poster.team.as_ref().is_some_and(|team| *team != self.team_id) {
            chat.shared.insert(channel.to_owned());
        }
        let message = ChatMessage {
            channel: channel.to_owned(),
            channel_type: channel_type.to_owned(),
            user: poster.user.clone(),
            bot_id: poster.bot_id.clone(),
            automated: poster.automated,
            user_team: poster.team.clone().unwrap_or_else(|| self.team_id.clone()),
            shared: chat.shared.contains(channel),
            ts,
            thread_ts: thread_ts.map(str::to_owned),
            text: text.to_owned(),
            send_id: None,
            edited: false,
        };
        chat.messages.push(message.clone());
        message
    }

    /// Everything posted and not deleted, in order, as it reads now.
    pub fn messages(&self) -> Vec<ChatMessage> {
        self.chat().messages.clone()
    }

    /// What apps posted, in order, as it reads now.
    pub fn posted_by_apps(&self) -> Vec<ChatMessage> {
        self.chat()
            .messages
            .iter()
            .filter(|message| message.bot_id.is_some())
            .cloned()
            .collect()
    }

    /// Everything deleted, in the order it was.
    pub fn deleted(&self) -> Vec<ChatMessage> {
        self.chat().deleted.clone()
    }

    /// Every reaction apps have on messages now.
    pub fn reactions(&self) -> Vec<Reaction> {
        self.chat().reactions.iter().cloned().collect()
    }

    /// Each time an app showed it was working in a thread (`true`), or
    /// stopped.
    pub fn working(&self) -> Vec<(String, String, bool)> {
        self.chat().working.clone()
    }

    /// Installs an app: a provider in this chat acting with `token` posts as
    /// `poster`. Any other token is refused.
    pub fn install(&self, token: &str, poster: Poster) {
        self.chat().apps.insert(token.to_owned(), poster);
    }

    /// Loses the answers to the next `count` posts an app makes: each is
    /// posted, and its sender never hears so.
    pub fn lose_answers(&self, count: u32) {
        self.chat().lose = count;
    }

    /// While `lagging`, a search for an app's post finds nothing, though
    /// it was posted.
    pub fn lag_searches(&self, lagging: bool) {
        self.chat().searches_lag = lagging;
    }

    /// An app posts, carrying the send's ID: in a thread unless its key is
    /// empty.
    fn app_post(&self, poster: &Poster, post: &OutgoingPost<'_>) -> Result<String, ProviderCallError> {
        let mut chat = self.chat();
        let seconds = chat.latest_second;
        let ts = chat.next_ts(seconds);
        let channel = post.conversation_external_id;
        let message = ChatMessage {
            channel: channel.to_owned(),
            channel_type: if channel.starts_with('D') { "im" } else { "channel" }.to_owned(),
            user: poster.user.clone(),
            bot_id: poster.bot_id.clone(),
            automated: poster.automated,
            user_team: self.team_id.clone(),
            shared: chat.shared.contains(channel),
            ts: ts.clone(),
            thread_ts: (!post.thread_key.is_empty()).then(|| post.thread_key.to_owned()),
            text: post.text.to_owned(),
            send_id: Some(post.send_id.to_owned()),
            edited: false,
        };
        chat.messages.push(message);
        if chat.lose > 0 {
            chat.lose -= 1;
            return Err(ProviderCallError::OutcomeUnknown);
        }
        Ok(ts)
    }

    /// Its author changes `message`'s text at `now`: the delivery of the
    /// edit, as Slack sends it to an app.
    pub fn edit(&self, message: &ChatMessage, text: &str, now: Timestamp) -> Value {
        let ts = {
            let mut chat = self.chat();
            let ts = chat.next_ts(now.millis().div_euclid(1000));
            if let Some(held) = chat
                .messages
                .iter_mut()
                .find(|held| held.channel == message.channel && held.ts == message.ts)
            {
                held.text = text.to_owned();
                held.edited = true;
            }
            ts
        };
        let mut changed = message.event();
        changed["text"] = json!(text);
        changed["edited"] = json!({ "user": message.user, "ts": ts });
        self.change(
            json!({
                "type": "message",
                "subtype": "message_changed",
                "hidden": true,
                "channel": message.channel,
                "channel_type": message.channel_type,
                "ts": ts,
                "message": changed,
            }),
            message.shared,
        )
    }

    /// Its author deletes `message` at `now`: the delivery of the
    /// deletion, as Slack sends it to an app.
    pub fn delete(&self, message: &ChatMessage, now: Timestamp) -> Value {
        let ts = {
            let mut chat = self.chat();
            let ts = chat.next_ts(now.millis().div_euclid(1000));
            if let Some(at) = chat
                .messages
                .iter()
                .position(|held| held.channel == message.channel && held.ts == message.ts)
            {
                let removed = chat.messages.remove(at);
                chat.deleted.push(removed);
            }
            ts
        };
        self.change(
            json!({
                "type": "message",
                "subtype": "message_deleted",
                "hidden": true,
                "channel": message.channel,
                "channel_type": message.channel_type,
                "ts": ts,
                "deleted_ts": message.ts,
            }),
            message.shared,
        )
    }

    fn change(&self, event: Value, shared: bool) -> Value {
        let mut chat = self.chat();
        chat.events += 1;
        json!({
            "type": "event_callback",
            "event_id": format!("Ev{:010}", chat.events),
            "team_id": self.team_id,
            "is_ext_shared_channel": shared,
            "event": event,
        })
    }

    /// The delivery of `message` to one app: an `event_callback` with a new
    /// event ID, as Slack sends it.
    pub fn delivery(&self, message: &ChatMessage) -> Value {
        self.change(message.event(), message.shared)
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use async_trait::async_trait;
    use zupflow_core::outbound::{Limits, OutboundError, OutboundHttp, OutboundRequest, OutboundResponse};

    use super::*;

    /// The fake provider calls no one: anything sent fails the test.
    struct NoRequests;

    #[async_trait]
    impl OutboundHttp for NoRequests {
        async fn send(&self, _: OutboundRequest, _: Limits) -> Result<OutboundResponse, OutboundError> {
            panic!("the fake provider sent a request");
        }
    }

    /// A provider that can't look people up or find a lost post says so:
    /// never "no such person" (which is remembered) nor "not sent" (which
    /// would post an answer twice).
    #[tokio::test]
    async fn says_what_it_cannot_do() {
        let provider = FakeChannelProvider::inbound_only();
        let post = OutgoingPost {
            conversation_external_id: "C0GENERAL",
            thread_key: "1.0",
            text: "hi",
            send_id: "run_1",
        };
        assert_eq!(
            provider.person(&NoRequests, "token", "U0ANA").await,
            Err(ProviderCallError::Unsupported)
        );
        assert_eq!(
            provider.find_sent(&NoRequests, "token", &post, now()).await,
            Err(ProviderCallError::Unsupported)
        );
        assert_eq!(
            provider.post(&NoRequests, "token", &post).await,
            Err(ProviderCallError::Unsupported)
        );
    }

    /// In a chat, it acts as the app its token names there, and only on
    /// what that app may: a post carries its send's ID, and is found by it
    /// when its answer is lost.
    #[tokio::test]
    async fn acts_in_its_chat_as_the_app_its_token_names() {
        let chat = Arc::new(FakeChat::new("T1"));
        chat.install("xoxb-helper", Poster::app("U0HELPER", "B0HELPER"));
        let provider = FakeChannelProvider::in_chat(chat.clone());
        let asked = chat.post("C1", "channel", "U0ANA", "hi", None, now());
        let post = OutgoingPost {
            conversation_external_id: "C1",
            thread_key: &asked.ts,
            text: "hello",
            send_id: "call_1",
        };
        assert_eq!(
            provider.post(&NoRequests, "xoxb-other", &post).await,
            Err(ProviderCallError::TokenRefused)
        );
        let ts = provider.post(&NoRequests, "xoxb-helper", &post).await.unwrap();
        let posted = chat.posted_by_apps();
        assert_eq!(posted.len(), 1);
        assert_eq!(
            (
                posted[0].ts.as_str(),
                posted[0].thread_ts.as_deref(),
                posted[0].send_id.as_deref()
            ),
            (ts.as_str(), Some(asked.ts.as_str()), Some("call_1"))
        );

        // Its answer lost: posted all the same, and found by its send's ID.
        chat.lose_answers(1);
        let lost = OutgoingPost {
            send_id: "call_2",
            ..post
        };
        assert_eq!(
            provider.post(&NoRequests, "xoxb-helper", &lost).await,
            Err(ProviderCallError::OutcomeUnknown)
        );
        let found = provider
            .find_sent(&NoRequests, "xoxb-helper", &lost, now())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(chat.posted_by_apps()[1].ts, found);
        let never = OutgoingPost {
            send_id: "call_3",
            ..post
        };
        assert_eq!(
            provider.find_sent(&NoRequests, "xoxb-helper", &never, now()).await,
            Ok(None)
        );

        // Its own posts only, edited and deleted.
        let own = MessageRef {
            conversation_external_id: "C1",
            external_id: &ts,
        };
        let theirs = MessageRef {
            conversation_external_id: "C1",
            external_id: &asked.ts,
        };
        provider
            .edit(&NoRequests, "xoxb-helper", &own, "hello again")
            .await
            .unwrap();
        assert_eq!(
            provider.edit(&NoRequests, "xoxb-helper", &theirs, "no").await,
            Err(ProviderCallError::Refused("cant_update_message"))
        );
        assert_eq!(
            provider.delete(&NoRequests, "xoxb-helper", &theirs).await,
            Err(ProviderCallError::Refused("cant_delete_message"))
        );
        assert!(chat.posted_by_apps()[0].edited);
        provider
            .react(&NoRequests, "xoxb-helper", &theirs, "eyes", false)
            .await
            .unwrap();
        assert_eq!(chat.reactions().len(), 1);
        provider
            .react(&NoRequests, "xoxb-helper", &theirs, "eyes", true)
            .await
            .unwrap();
        assert!(chat.reactions().is_empty());
        provider.delete(&NoRequests, "xoxb-helper", &own).await.unwrap();
        provider.delete(&NoRequests, "xoxb-helper", &own).await.unwrap();
        assert_eq!(chat.deleted().len(), 1);
        assert_eq!(
            provider.react(&NoRequests, "xoxb-helper", &own, "eyes", false).await,
            Err(ProviderCallError::Refused("message_not_found"))
        );

        assert_eq!(
            provider.open_direct(&NoRequests, "xoxb-helper", "U0ANA").await,
            Ok("D0ANA".to_owned())
        );
        assert_eq!(
            provider.open_direct(&NoRequests, "xoxb-helper", "C1").await,
            Err(ProviderCallError::Refused("user_not_found"))
        );
        assert!(
            provider
                .show_working(&NoRequests, "xoxb-helper", "C1", &asked.ts, true)
                .await
        );
        assert!(
            !provider
                .show_working(&NoRequests, "xoxb-helper", "D0ANA", "", true)
                .await
        );
        assert!(
            !provider
                .show_working(&NoRequests, "xoxb-other", "C1", &asked.ts, true)
                .await
        );
        assert_eq!(chat.working(), vec![("C1".to_owned(), asked.ts.clone(), true)]);
    }

    #[test]
    fn reads_an_edit_and_a_deletion_as_slack_sends_them() {
        let chat = FakeChat::new("T1");
        let posted = chat.post("C1", "channel", "U0ANA", "the build is red", None, now());
        let later = Timestamp::from_millis(now().millis() + 5_000);
        let read = |body: Value| FakeChannelProvider::default().read(body.to_string().as_bytes(), now());
        let Ok(InboundDelivery::Event {
            event: InboundEvent::Edited(edited),
            ..
        }) = read(chat.edit(&posted, "the build is green", later))
        else {
            panic!("an edit");
        };
        assert_eq!(
            (
                edited.conversation_external_id.as_str(),
                edited.external_id.as_str(),
                edited.text.as_str()
            ),
            ("C1", posted.ts.as_str(), "the build is green")
        );
        assert_eq!(edited.edited_at.millis() / 1000, later.millis() / 1000);
        assert!(chat.messages()[0].edited);
        let Ok(InboundDelivery::Event {
            event: InboundEvent::Deleted(deleted),
            ..
        }) = read(chat.delete(&posted, later))
        else {
            panic!("a deletion");
        };
        assert_eq!(deleted.external_id, posted.ts);
        assert!(chat.messages().is_empty());
    }

    const SECRET: &[u8] = b"signing-secret";

    fn now() -> Timestamp {
        Timestamp::from_millis(1_800_000_000_000)
    }

    fn request<'a>(signed: &'a SignedDelivery) -> InboundRequest<'a> {
        InboundRequest {
            timestamp: Some(&signed.timestamp),
            signature: Some(&signed.signature),
            body: &signed.body,
        }
    }

    #[test]
    fn accepts_only_what_the_app_signed_lately() {
        let provider = FakeChannelProvider::default();
        let signed = sign(&json!({ "type": "url_verification", "challenge": "c" }), SECRET, now());
        assert_eq!(provider.verify(&request(&signed), SECRET, now()), Ok(()));
        assert_eq!(
            provider.verify(&request(&signed), b"another", now()),
            Err(InboundRefusal::InvalidSignature)
        );
        let late = Timestamp::from_millis(now().millis() + 301_000);
        assert_eq!(
            provider.verify(&request(&signed), SECRET, late),
            Err(InboundRefusal::Stale)
        );
        let mut tampered = signed.clone();
        tampered.body.push(b' ');
        assert_eq!(
            provider.verify(&request(&tampered), SECRET, now()),
            Err(InboundRefusal::InvalidSignature)
        );
        for timestamp in ["", "12a", "1234567890123"] {
            let odd = InboundRequest {
                timestamp: Some(timestamp),
                ..request(&signed)
            };
            assert_eq!(provider.check_timestamp(&odd, now()), Err(InboundRefusal::Stale));
        }
        assert_eq!(
            provider.read(&signed.body, now()),
            Ok(InboundDelivery::UrlVerification {
                challenge: "c".to_owned()
            })
        );
    }

    #[test]
    fn reads_a_message_as_slack_sends_it() {
        let chat = FakeChat::new("T1");
        let posted = chat.post("C1", "channel", "U1", "<@U9|ag> hi <@U8>", None, now());
        let reply = chat.post("C1", "channel", "U2", "more", Some(&posted.ts), now());
        let delivery = chat.delivery(&posted);
        let InboundDelivery::Event {
            event_id,
            kind,
            team_id,
            event: InboundEvent::Message(message),
        } = FakeChannelProvider::default()
            .read(delivery.to_string().as_bytes(), now())
            .unwrap()
        else {
            panic!("a message");
        };
        assert_eq!(
            (event_id.as_str(), kind.as_str(), team_id.as_deref()),
            ("Ev0000000001", "message", Some("T1"))
        );
        assert_eq!(message.mentions, vec!["U9", "U8"]);
        assert_eq!(message.thread_key, posted.ts);
        assert_eq!(message.sent_at, now());
        assert!(!message.author_is_automated);
        let InboundDelivery::Event {
            event: InboundEvent::Message(reply),
            ..
        } = FakeChannelProvider::default()
            .read(chat.delivery(&reply).to_string().as_bytes(), now())
            .unwrap()
        else {
            panic!("a message");
        };
        assert_eq!(reply.thread_key, posted.ts);
        let dm = chat.post("D1", "im", "U1", "hello", None, now());
        let app = chat.post_automated("C1", "channel", "U7", "build green", None, now());
        for (posted, thread_key, automated) in [(&dm, "", false), (&app, app.ts.as_str(), true)] {
            let Ok(InboundDelivery::Event {
                event: InboundEvent::Message(read),
                ..
            }) = FakeChannelProvider::default().read(chat.delivery(posted).to_string().as_bytes(), now())
            else {
                panic!("a message");
            };
            assert_eq!(
                (read.thread_key.as_str(), read.author_is_automated),
                (thread_key, automated)
            );
        }
        assert_eq!(chat.messages().len(), 4);
    }

    fn read(chat: &FakeChat, posted: &ChatMessage) -> NormalizedMessage {
        let Ok(InboundDelivery::Event {
            event: InboundEvent::Message(read),
            ..
        }) = FakeChannelProvider::default().read(chat.delivery(posted).to_string().as_bytes(), now())
        else {
            panic!("a message");
        };
        *read
    }

    #[test]
    fn reads_an_apps_post_by_its_bot_id_and_another_teams_author() {
        let chat = FakeChat::new("T1");
        // An app's post names its user and its bot ID: automated, though
        // it's no `bot_message`.
        let app = read(
            &chat,
            &chat.post_as("C1", "channel", &Poster::app("U0APP", "B0APP"), "hi", None, now()),
        );
        assert_eq!(
            (
                app.author_external_id.as_deref(),
                app.author_bot_id.as_deref(),
                app.author_is_automated
            ),
            (Some("U0APP"), Some("B0APP"), true)
        );
        // With its bot ID alone, the bot ID is the author.
        let alone = read(
            &chat,
            &chat.post_as("C1", "channel", &Poster::app_without_user("B0APP"), "hi", None, now()),
        );
        assert_eq!(
            (
                alone.author_external_id.as_deref(),
                alone.author_bot_id.as_deref(),
                alone.author_is_automated
            ),
            (Some("B0APP"), Some("B0APP"), true)
        );
        let person = read(&chat, &chat.post("C1", "channel", "U1", "hi", None, now()));
        assert_eq!(
            (
                person.author_team_id.as_deref(),
                person.author_bot_id,
                person.conversation_is_external
            ),
            (Some("T1"), None, false)
        );
        // Someone of another organization's team shares the channel.
        let guest = read(
            &chat,
            &chat.post_as("C1", "channel", &Poster::external("U0GUEST", "T9"), "hi", None, now()),
        );
        assert_eq!(
            (
                guest.author_team_id.as_deref(),
                guest.conversation_is_external,
                guest.author_is_automated
            ),
            (Some("T9"), true, false)
        );
        let after = read(&chat, &chat.post("C1", "channel", "U1", "again", None, now()));
        assert!(after.conversation_is_external, "the channel stays shared");
        let elsewhere = read(&chat, &chat.post("C2", "channel", "U1", "hi", None, now()));
        assert!(!elsewhere.conversation_is_external);
    }

    #[test]
    fn ignores_what_it_does_not_act_on() {
        let read = |body: Value| FakeChannelProvider::default().read(body.to_string().as_bytes(), now());
        assert_eq!(
            FakeChannelProvider::default().read(b"{", now()),
            Err(InboundRefusal::InvalidJson)
        );
        assert_eq!(read(json!({ "challenge": "c" })), Err(InboundRefusal::Unreadable));
        assert_eq!(
            read(json!({ "type": "app_rate_limited" })),
            Ok(InboundDelivery::Ignored)
        );
        let teamless = json!({ "type": "event_callback", "event_id": "Ev1", "event": { "type": "message" } });
        assert_eq!(read(teamless), Ok(InboundDelivery::Ignored));
        let edit = json!({
            "type": "event_callback", "event_id": "Ev2", "team_id": "T1",
            "event": { "type": "message", "subtype": "message_changed", "channel": "C1", "ts": "1.0" },
        });
        assert!(matches!(
            read(edit),
            Ok(InboundDelivery::Event {
                event: InboundEvent::Other,
                ..
            })
        ));
        let uninstall = json!({
            "type": "event_callback", "event_id": "Ev3", "team_id": "T1", "event": { "type": "tokens_revoked" },
        });
        assert!(matches!(
            read(uninstall),
            Ok(InboundDelivery::Event {
                event: InboundEvent::Uninstalled,
                ..
            })
        ));
    }
}
