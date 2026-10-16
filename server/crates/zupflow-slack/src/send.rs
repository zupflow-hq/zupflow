//! [`SlackClient`]: what an installed app does in its team, with its bot
//! token, through the safe outbound path: sending, editing and deleting its
//! own messages, reacting, opening DMs, reading people, conversations and
//! history, and showing that it's working.
//!
//! Every call is one request; none retries or sleeps. A `429` is
//! [`SlackError::RateLimited`] with Slack's `Retry-After`, for the caller's
//! job to wait out; a refusal is one of [`SlackCode`]'s stable codes, never
//! Slack's text.
//!
//! **Sends aren't deduplicated by Slack.** Each message Zupflow posts
//! carries its invocation ID in Slack's message metadata
//! (`event_type: zupflow_send`). A send whose answer was lost
//! ([`SlackError::outcome_unknown`]) is settled with [`SlackClient::find_sent`]
//! before it's tried again: if the message is in the thread, it was sent.

use serde_json::{Map, Value, json};
use zupflow_core::channels::ConversationKind;
use zupflow_core::outbound::OutboundHttp;
use zupflow_core::time::Timestamp;

use crate::api::{Auth, SlackApi, SlackCode, SlackError, call, next_cursor, object, text};

/// The metadata event type a Zupflow send carries.
pub const SEND_EVENT_TYPE: &str = "zupflow_send";
/// The most a `markdown` block holds; longer text is refused before it's sent.
pub const MARKDOWN_MAX: usize = 12_000;
/// How far before a lost send's attempt its settling read looks, for clocks
/// that differ.
const SETTLE_SLACK_MS: i64 = 60_000;
/// How many pages a settling read looks through.
const SETTLE_PAGES: usize = 3;
/// The most messages one history page asks for.
const PAGE_MAX: u32 = 200;

/// The installed app's side of Slack's Web API, reaching it at `api`.
#[derive(Debug, Clone, Default)]
pub struct SlackClient {
    api: SlackApi,
}

/// A message to post: Markdown, in a conversation, perhaps in a thread.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Post<'a> {
    pub channel: &'a str,
    /// The thread to reply in; `None` posts at the conversation's top level.
    pub thread_ts: Option<&'a str>,
    /// Markdown, at most [`MARKDOWN_MAX`] characters.
    pub text: &'a str,
    /// The call that sends it, kept in the message's metadata.
    pub invocation_id: Option<&'a str>,
    /// The name and avatar to post as (`chat:write.customize`).
    pub username: Option<&'a str>,
    pub icon_url: Option<&'a str>,
}

/// A message Slack holds: where it is and its `ts`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Posted {
    pub channel: String,
    pub ts: String,
}

/// A person, as `users.info` describes them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SlackPerson {
    pub id: String,
    /// Their username.
    pub handle: Option<String>,
    /// What Slack shows: their display name, else their real name, else their username.
    pub display_name: Option<String>,
    pub email: Option<String>,
    pub title: Option<String>,
    pub time_zone: Option<String>,
    /// A bot user, an app, or Slackbot.
    pub is_automated: bool,
    pub deleted: bool,
}

/// A conversation, as `conversations.info` and `.list` describe it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SlackConversation {
    pub id: String,
    pub name: Option<String>,
    pub kind: ConversationKind,
    /// Shared with another organization (Slack Connect).
    pub is_external: bool,
    /// Whether the app's bot user is in it.
    pub is_member: bool,
    pub topic: Option<String>,
    pub member_count: Option<u64>,
}

/// A message read from history.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SlackMessage {
    pub ts: String,
    pub thread_ts: Option<String>,
    pub user: Option<String>,
    pub bot_id: Option<String>,
    /// In Slack's markup.
    pub text: String,
    /// The invocation ID a Zupflow send carried, if it is one.
    pub invocation_id: Option<String>,
}

/// One page of a list, and where the next starts.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Page<T> {
    pub items: Vec<T>,
    pub next_cursor: Option<String>,
}

fn string(object: &Map<String, Value>, field: &str) -> Option<String> {
    text(object, field).map(str::to_owned)
}

fn flag(object: &Map<String, Value>, field: &str) -> bool {
    object.get(field).and_then(Value::as_bool).unwrap_or(false)
}

/// The message's blocks: one `markdown` block, which Slack renders as Markdown.
fn markdown_blocks(text: &str) -> String {
    json!([{ "type": "markdown", "text": text }]).to_string()
}

fn checked_markdown(text: &str) -> Result<(), SlackError> {
    if text.trim().is_empty() {
        Err(SlackError::Refused(SlackCode::NoText))
    } else if text.chars().count() > MARKDOWN_MAX {
        Err(SlackError::Refused(SlackCode::TooLong))
    } else {
        Ok(())
    }
}

/// A message from history, with the invocation ID a Zupflow send carried.
fn history_message(message: &Map<String, Value>) -> Option<SlackMessage> {
    let invocation_id = object(message, "metadata")
        .filter(|metadata| text(metadata, "event_type") == Some(SEND_EVENT_TYPE))
        .and_then(|metadata| object(metadata, "event_payload"))
        .and_then(|payload| string(payload, "invocation_id"));
    Some(SlackMessage {
        ts: string(message, "ts")?,
        thread_ts: string(message, "thread_ts"),
        user: string(message, "user"),
        bot_id: string(message, "bot_id"),
        text: message
            .get("text")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_owned(),
        invocation_id,
    })
}

fn messages_page(answer: &Map<String, Value>) -> Page<SlackMessage> {
    let items = answer
        .get("messages")
        .and_then(Value::as_array)
        .map(|messages| {
            messages
                .iter()
                .filter_map(Value::as_object)
                .filter_map(history_message)
                .collect()
        })
        .unwrap_or_default();
    Page {
        items,
        next_cursor: next_cursor(answer),
    }
}

fn conversation_of(channel: &Map<String, Value>) -> Option<SlackConversation> {
    let kind = if flag(channel, "is_im") {
        ConversationKind::Dm
    } else if flag(channel, "is_mpim") {
        ConversationKind::GroupDm
    } else if flag(channel, "is_private") || flag(channel, "is_group") {
        ConversationKind::PrivateChannel
    } else {
        ConversationKind::Channel
    };
    Some(SlackConversation {
        id: string(channel, "id")?,
        name: string(channel, "name"),
        kind,
        is_external: flag(channel, "is_ext_shared"),
        is_member: flag(channel, "is_member") || kind == ConversationKind::Dm,
        topic: object(channel, "topic").and_then(|topic| string(topic, "value")),
        member_count: channel.get("num_members").and_then(Value::as_u64),
    })
}

impl SlackClient {
    pub fn new(api: SlackApi) -> Self {
        Self { api }
    }

    /// `chat.postMessage`: one `markdown` block (and the same text as the
    /// notification's fallback), with the invocation ID in its metadata.
    pub async fn post_message(
        &self,
        http: &dyn OutboundHttp,
        token: &str,
        post: &Post<'_>,
    ) -> Result<Posted, SlackError> {
        checked_markdown(post.text)?;
        let blocks = markdown_blocks(post.text);
        let metadata = post
            .invocation_id
            .map(|id| json!({ "event_type": SEND_EVENT_TYPE, "event_payload": { "invocation_id": id } }).to_string());
        let mut form = vec![
            ("channel", post.channel),
            ("text", post.text),
            ("blocks", blocks.as_str()),
        ];
        if let Some(thread_ts) = post.thread_ts {
            form.push(("thread_ts", thread_ts));
        }
        if let Some(metadata) = &metadata {
            form.push(("metadata", metadata.as_str()));
        }
        if let Some(username) = post.username {
            form.push(("username", username));
        }
        if let Some(icon_url) = post.icon_url {
            form.push(("icon_url", icon_url));
        }
        let answer = call(http, &self.api, "chat.postMessage", Auth::Bearer(token), &form).await?;
        match (string(&answer, "channel"), string(&answer, "ts")) {
            (Some(channel), Some(ts)) => Ok(Posted { channel, ts }),
            // Sent, but which message it is can't be read: settle it.
            _ => Err(SlackError::Unreadable),
        }
    }

    /// `chat.update`: the app's own message, with new Markdown.
    pub async fn update_message(
        &self,
        http: &dyn OutboundHttp,
        token: &str,
        channel: &str,
        ts: &str,
        text: &str,
    ) -> Result<(), SlackError> {
        checked_markdown(text)?;
        let blocks = markdown_blocks(text);
        call(
            http,
            &self.api,
            "chat.update",
            Auth::Bearer(token),
            &[
                ("channel", channel),
                ("ts", ts),
                ("text", text),
                ("blocks", blocks.as_str()),
            ],
        )
        .await
        .map(|_| ())
    }

    /// `chat.delete`: the app's own message. One already gone is done.
    pub async fn delete_message(
        &self,
        http: &dyn OutboundHttp,
        token: &str,
        channel: &str,
        ts: &str,
    ) -> Result<(), SlackError> {
        match call(
            http,
            &self.api,
            "chat.delete",
            Auth::Bearer(token),
            &[("channel", channel), ("ts", ts)],
        )
        .await
        {
            Ok(_) | Err(SlackError::Refused(SlackCode::MessageNotFound)) => Ok(()),
            Err(error) => Err(error),
        }
    }

    /// `reactions.add` or `.remove`: an emoji by its name, with or without
    /// colons. Adding one already there, or removing one that isn't, is done.
    pub async fn react(
        &self,
        http: &dyn OutboundHttp,
        token: &str,
        channel: &str,
        ts: &str,
        emoji: &str,
        remove: bool,
    ) -> Result<(), SlackError> {
        let name = emoji.trim().trim_matches(':');
        if name.is_empty() {
            return Err(SlackError::Refused(SlackCode::InvalidEmoji));
        }
        let method = if remove { "reactions.remove" } else { "reactions.add" };
        match call(
            http,
            &self.api,
            method,
            Auth::Bearer(token),
            &[("channel", channel), ("timestamp", ts), ("name", name)],
        )
        .await
        {
            Ok(_) => Ok(()),
            Err(SlackError::Refused(SlackCode::AlreadyReacted)) if !remove => Ok(()),
            Err(SlackError::Refused(SlackCode::NoReaction)) if remove => Ok(()),
            Err(error) => Err(error),
        }
    }

    /// `conversations.open`: the DM with one person.
    pub async fn open_dm(&self, http: &dyn OutboundHttp, token: &str, user: &str) -> Result<String, SlackError> {
        let answer = call(
            http,
            &self.api,
            "conversations.open",
            Auth::Bearer(token),
            &[("users", user)],
        )
        .await?;
        object(&answer, "channel")
            .and_then(|channel| string(channel, "id"))
            .ok_or(SlackError::Unreadable)
    }

    /// `users.info`: a person's names, email and whether they're automated.
    pub async fn user_info(&self, http: &dyn OutboundHttp, token: &str, user: &str) -> Result<SlackPerson, SlackError> {
        let answer = call(http, &self.api, "users.info", Auth::Bearer(token), &[("user", user)]).await?;
        let person = object(&answer, "user").ok_or(SlackError::Unreadable)?;
        let profile = object(person, "profile");
        let from_profile = |field: &str| profile.and_then(|profile| string(profile, field));
        let id = string(person, "id").ok_or(SlackError::Unreadable)?;
        let handle = string(person, "name");
        Ok(SlackPerson {
            display_name: from_profile("display_name")
                .or_else(|| from_profile("real_name"))
                .or_else(|| string(person, "real_name"))
                .or_else(|| handle.clone()),
            email: from_profile("email"),
            title: from_profile("title"),
            time_zone: string(person, "tz"),
            is_automated: flag(person, "is_bot") || flag(person, "is_app_user") || id == "USLACKBOT",
            deleted: flag(person, "deleted"),
            handle,
            id,
        })
    }

    /// `conversations.info`.
    pub async fn conversation_info(
        &self,
        http: &dyn OutboundHttp,
        token: &str,
        channel: &str,
    ) -> Result<SlackConversation, SlackError> {
        let answer = call(
            http,
            &self.api,
            "conversations.info",
            Auth::Bearer(token),
            &[("channel", channel)],
        )
        .await?;
        object(&answer, "channel")
            .and_then(conversation_of)
            .ok_or(SlackError::Unreadable)
    }

    /// `conversations.list`: a page of the conversations the app can see,
    /// of every kind, archived ones left out.
    pub async fn list_conversations(
        &self,
        http: &dyn OutboundHttp,
        token: &str,
        cursor: Option<&str>,
        limit: u32,
    ) -> Result<Page<SlackConversation>, SlackError> {
        let limit = limit.clamp(1, PAGE_MAX).to_string();
        let mut form = vec![
            ("types", "public_channel,private_channel,mpim,im"),
            ("exclude_archived", "true"),
            ("limit", limit.as_str()),
        ];
        if let Some(cursor) = cursor {
            form.push(("cursor", cursor));
        }
        let answer = call(http, &self.api, "conversations.list", Auth::Bearer(token), &form).await?;
        let items = answer
            .get("channels")
            .and_then(Value::as_array)
            .map(|channels| {
                channels
                    .iter()
                    .filter_map(Value::as_object)
                    .filter_map(conversation_of)
                    .collect()
            })
            .unwrap_or_default();
        Ok(Page {
            items,
            next_cursor: next_cursor(&answer),
        })
    }

    /// `conversations.history`: a page of a conversation's top level, newest
    /// first, from `oldest` (a `ts`) on, with each message's metadata.
    pub async fn history(
        &self,
        http: &dyn OutboundHttp,
        token: &str,
        channel: &str,
        oldest: Option<&str>,
        cursor: Option<&str>,
        limit: u32,
    ) -> Result<Page<SlackMessage>, SlackError> {
        let limit = limit.clamp(1, PAGE_MAX).to_string();
        let mut form = vec![
            ("channel", channel),
            ("limit", limit.as_str()),
            ("include_all_metadata", "true"),
        ];
        if let Some(oldest) = oldest {
            form.push(("oldest", oldest));
        }
        if let Some(cursor) = cursor {
            form.push(("cursor", cursor));
        }
        let answer = call(http, &self.api, "conversations.history", Auth::Bearer(token), &form).await?;
        Ok(messages_page(&answer))
    }

    /// `conversations.history`'s first page between `oldest` and `latest`
    /// (each a `ts`, both included), newest first, each message as Slack
    /// wrote it: for reading them as the events they would have been.
    pub async fn history_objects(
        &self,
        http: &dyn OutboundHttp,
        token: &str,
        channel: &str,
        (oldest, latest): (&str, &str),
        limit: u32,
    ) -> Result<Vec<Map<String, Value>>, SlackError> {
        let limit = limit.clamp(1, PAGE_MAX).to_string();
        let form = vec![
            ("channel", channel),
            ("limit", limit.as_str()),
            ("oldest", oldest),
            ("latest", latest),
            ("inclusive", "true"),
        ];
        let answer = call(http, &self.api, "conversations.history", Auth::Bearer(token), &form).await?;
        Ok(answer
            .get("messages")
            .and_then(Value::as_array)
            .map(|messages| messages.iter().filter_map(Value::as_object).cloned().collect())
            .unwrap_or_default())
    }

    /// `conversations.replies`: a page of a thread, its root first, from
    /// `oldest` (a `ts`) on, with each message's metadata.
    #[allow(clippy::too_many_arguments)]
    pub async fn replies(
        &self,
        http: &dyn OutboundHttp,
        token: &str,
        channel: &str,
        thread_ts: &str,
        oldest: Option<&str>,
        cursor: Option<&str>,
        limit: u32,
    ) -> Result<Page<SlackMessage>, SlackError> {
        let limit = limit.clamp(1, PAGE_MAX).to_string();
        let mut form = vec![
            ("channel", channel),
            ("ts", thread_ts),
            ("limit", limit.as_str()),
            ("include_all_metadata", "true"),
        ];
        if let Some(oldest) = oldest {
            form.push(("oldest", oldest));
        }
        if let Some(cursor) = cursor {
            form.push(("cursor", cursor));
        }
        let answer = call(http, &self.api, "conversations.replies", Auth::Bearer(token), &form).await?;
        Ok(messages_page(&answer))
    }

    /// `assistant.threads.setStatus`: what the app shows while it works in a
    /// thread (`""` clears it). Best effort: whether it was shown, never an
    /// error, since Slack refuses it outside assistant threads and a status
    /// is never worth a retry.
    pub async fn set_status(
        &self,
        http: &dyn OutboundHttp,
        token: &str,
        channel: &str,
        thread_ts: &str,
        status: &str,
    ) -> bool {
        call(
            http,
            &self.api,
            "assistant.threads.setStatus",
            Auth::Bearer(token),
            &[("channel_id", channel), ("thread_ts", thread_ts), ("status", status)],
        )
        .await
        .is_ok()
    }

    /// Settles a send whose answer was lost: the `ts` of the message that
    /// carries `invocation_id` in its metadata, if Slack holds one, read from
    /// the thread (or the conversation's top level) since a minute before
    /// the send was attempted. `None` means it wasn't sent: send it again.
    /// Still unknown when more than three pages came since (`NoAnswer`).
    pub async fn find_sent(
        &self,
        http: &dyn OutboundHttp,
        token: &str,
        channel: &str,
        thread_ts: Option<&str>,
        invocation_id: &str,
        attempted_at: Timestamp,
    ) -> Result<Option<String>, SlackError> {
        let since = attempted_at.millis().saturating_sub(SETTLE_SLACK_MS).max(0);
        let oldest = format!("{}.{:06}", since / 1000, (since % 1000) * 1000);
        let mut cursor: Option<String> = None;
        for _ in 0..SETTLE_PAGES {
            let page = match thread_ts {
                Some(thread_ts) => {
                    self.replies(
                        http,
                        token,
                        channel,
                        thread_ts,
                        Some(&oldest),
                        cursor.as_deref(),
                        PAGE_MAX,
                    )
                    .await?
                }
                None => {
                    self.history(http, token, channel, Some(&oldest), cursor.as_deref(), PAGE_MAX)
                        .await?
                }
            };
            if let Some(found) = page
                .items
                .iter()
                .find(|message| message.invocation_id.as_deref() == Some(invocation_id))
            {
                return Ok(Some(found.ts.clone()));
            }
            match page.next_cursor {
                Some(next) => cursor = Some(next),
                None => return Ok(None),
            }
        }
        // More than three pages since the attempt: still unknown, so not sent again.
        Err(SlackError::NoAnswer)
    }
}
