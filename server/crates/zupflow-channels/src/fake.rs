//! The fake channel provider: Slack's wire format, with the team's chat in
//! memory.
//!
//! [`FakeChannelProvider`] verifies and reads deliveries exactly as Slack
//! sends them: signed `v0` with the app's signing secret over
//! `v0:{timestamp}:{body}`, the timestamp within
//! [`INBOUND_TOLERANCE_SECONDS`] of the clock, and events as Slack's Events
//! API shapes them. It normalizes what Zupflow acts on (a message, an
//! uninstall) and leaves the rest as an event recorded once and nothing
//! more: edits, deletes and other message subtypes are the real provider's
//! to normalize.
//!
//! [`FakeChat`] is the other side: a team's chat, in memory, that people
//! post to. Each post becomes the delivery Slack would make to an app, for
//! a test or a development server to send to the inbound endpoint.

use std::sync::Mutex;

use hmac::{Hmac, KeyInit, Mac};
use serde_json::{Map, Value, json};
use sha2::Sha256;
use zupflow_core::channels::provider::{
    ChannelProvider, INBOUND_TOLERANCE_SECONDS, InboundDelivery, InboundEvent, InboundRefusal, InboundRequest,
    NormalizedMessage, SignatureHeaders,
};
use zupflow_core::channels::{ChannelProviderKind, ConversationKind};
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

/// The fake provider: Slack's inbound side, in full.
#[derive(Debug, Clone, Copy, Default)]
pub struct FakeChannelProvider;

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
        let read = match event_kind {
            "app_uninstalled" | "tokens_revoked" => InboundEvent::Uninstalled,
            _ => message(event, shared, now).map_or(InboundEvent::Other, |read| InboundEvent::Message(Box::new(read))),
        };
        Ok(InboundDelivery::Event {
            event_id: event_id.to_owned(),
            kind: event_kind.to_owned(),
            team_id: Some(team_id.to_owned()),
            event: read,
        })
    }
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
    let sent_at = ts
        .parse::<f64>()
        .ok()
        .map(|seconds| (seconds * 1000.0).round())
        .filter(|millis| millis.is_finite() && *millis > 0.0 && *millis < 9.0e15)
        .map_or(now, |millis| {
            // Within i64, checked above.
            #[allow(clippy::cast_possible_truncation)]
            Timestamp::from_millis(millis as i64)
        });
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
    /// Channels shared with another organization's team.
    shared: std::collections::BTreeSet<String>,
    events: u64,
    ticks: u64,
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
        chat.ticks += 1;
        let seconds = now.millis().div_euclid(1000);
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
            ts: format!("{seconds}.{:06}", chat.ticks % 1_000_000),
            thread_ts: thread_ts.map(str::to_owned),
            text: text.to_owned(),
        };
        chat.messages.push(message.clone());
        message
    }

    /// Everything posted, in order.
    pub fn messages(&self) -> Vec<ChatMessage> {
        self.chat().messages.clone()
    }

    /// The delivery of `message` to one app: an `event_callback` with a new
    /// event ID, as Slack sends it.
    pub fn delivery(&self, message: &ChatMessage) -> Value {
        let mut chat = self.chat();
        chat.events += 1;
        json!({
            "type": "event_callback",
            "event_id": format!("Ev{:010}", chat.events),
            "team_id": self.team_id,
            "is_ext_shared_channel": message.shared,
            "event": message.event(),
        })
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use async_trait::async_trait;
    use zupflow_core::channels::provider::{OutgoingPost, ProviderCallError};
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
        let provider = FakeChannelProvider;
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
        let provider = FakeChannelProvider;
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
        } = FakeChannelProvider
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
        } = FakeChannelProvider
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
            }) = FakeChannelProvider.read(chat.delivery(posted).to_string().as_bytes(), now())
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
        }) = FakeChannelProvider.read(chat.delivery(posted).to_string().as_bytes(), now())
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
        let read = |body: Value| FakeChannelProvider.read(body.to_string().as_bytes(), now());
        assert_eq!(FakeChannelProvider.read(b"{", now()), Err(InboundRefusal::InvalidJson));
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
