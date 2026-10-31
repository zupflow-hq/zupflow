//! The tools a channel session offers its agent (decision 0047's names):
//! its connection's channel tools, and `stay_silent`.
//!
//! Each channel tool is defined once here, with a fixed description and a
//! JSON schema, and offered as `<provider>_<short_id>_<tool>` (the
//! connection's short ID is drawn with installations', so a channel tool
//! never shares a name with an app's), its description naming the
//! connection's handle and short ID: never a label or a team's name, which
//! people rename for the Console's sake. Nothing in a definition changes
//! while a session lasts, so the session's tool list, fixed when it starts,
//! stays a stable prefix for the provider's cache. A tool is offered when
//! the agent channel grants it, the organization doesn't deny it, and the
//! provider can do it: `get_attachment` waits for stored attachments.
//!
//! IDs in and out are Zupflow's (`conv_…`, `chmsg_…`, `person_…`), found
//! only within the run's own connection; the provider's own IDs never reach
//! the model.
//!
//! `stay_silent` is the run's own tool: a call records why the agent posts
//! nothing, and the run ends once its other calls are settled. It's offered
//! in every channel session.

use serde_json::{Map, Value, json};

use super::{ChannelProviderKind, channel_tool};
use crate::apps::ToolEffect;
use crate::ids::ChannelConnectionId;
use crate::policy::ToolLevel;
use crate::runs::digest::{Sha256Digest, canonical_json};
use crate::runs::names::{OfferedTool, tool_name};

/// The run's own tool: post nothing, with why.
pub const STAY_SILENT: &str = "stay_silent";

/// The longest reason `stay_silent` keeps, in characters.
pub const MAX_SILENT_REASON: usize = 500;

/// What the model reads back once it stayed silent.
pub const STAYED_SILENT: &str = "Nothing will be posted for this run. It ends once your other calls are done.";

/// What the model reads back for `stay_silent` without a reason.
pub const SILENT_WITHOUT_REASON: &str =
    "Give stay_silent a short reason (up to 500 characters) for not answering, or answer the thread.";

/// The most messages one read returns.
pub const MAX_READ: u32 = 50;

/// The most matches a search or a lookup of people returns.
pub const MAX_FOUND: u32 = 20;

/// The longest text one post takes, in characters: longer is refused, so
/// what's posted is always what was decided on.
pub const MAX_POST: usize = 12_000;

/// The channel tools the provider can do now: every one but
/// `get_attachment`, which waits for stored attachments.
pub fn provider_offers(provider: ChannelProviderKind, tool: &str) -> bool {
    match provider {
        ChannelProviderKind::Slack => tool != "get_attachment" && channel_tool(tool).is_some(),
    }
}

/// Whether a call to `tool` posts a message.
pub fn posts(tool: &str) -> bool {
    matches!(tool, "reply" | "send_message" | "send_direct_message")
}

/// Whether a call to `tool` changes anything at the provider: it is held to
/// the audience rule and the rate limits.
pub fn writes(tool: &str) -> bool {
    channel_tool(tool).is_some_and(|(effect, _)| effect != ToolEffect::Read)
}

fn string(description: &str) -> Value {
    json!({ "type": "string", "description": description })
}

fn object(properties: Value, required: &[&str]) -> Value {
    json!({
        "type": "object",
        "properties": properties,
        "required": required,
        "additionalProperties": false,
    })
}

/// A channel tool's fixed description and input schema; `None` for a name
/// that isn't one.
pub fn definition(tool: &str) -> Option<(&'static str, Value)> {
    let conversation = || string("A conversation's ID, conv_…, as list_conversations gives it.");
    let message = || string("A message's ID, chmsg_…, as a read gives it.");
    let text = || string("Markdown, up to 12,000 characters.");
    Some(match tool {
        "list_conversations" => (
            "List the conversations this connection has seen, most recent first: each one's ID, name and kind, \
             and whether you're in it.",
            object(
                json!({
                    "query": string("Only conversations whose name contains this."),
                    "kind": {
                        "type": "string",
                        "enum": ["channel", "private_channel", "dm", "group_dm"],
                        "description": "Only conversations of this kind.",
                    },
                    "cursor": string("Where the last page ended, to read the next."),
                }),
                &[],
            ),
        ),
        "get_conversation" => (
            "Read one conversation's details: its name and kind, whether you're in it, and whether it's shared \
             with another organization.",
            object(json!({ "conversation": conversation() }), &["conversation"]),
        ),
        "read_messages" => (
            "Read a conversation's messages, newest first, from what this connection kept: up to 50 at a time. \
             Pass the cursor back for older ones.",
            object(
                json!({
                    "conversation": conversation(),
                    "limit": { "type": "integer", "minimum": 1, "maximum": 50 },
                    "cursor": string("Where the last page ended, to read older messages."),
                }),
                &["conversation"],
            ),
        ),
        "read_thread" => (
            "Read the thread a message is in: its first message, then its replies, oldest first.",
            object(json!({ "message": message() }), &["message"]),
        ),
        "search_messages" => (
            "Search the messages this connection kept for text, newest first: up to 20 matches.",
            object(
                json!({
                    "query": string("Text the message contains, in any case."),
                    "conversation": string("Only this conversation's messages: its ID, conv_…."),
                }),
                &["query"],
            ),
        ),
        "find_people" => (
            "Find people this connection has seen, by name or handle: up to 20.",
            object(json!({ "query": string("Part of their name or handle.") }), &["query"]),
        ),
        "get_person" => (
            "Read what this connection knows of one person: their name and handle, and whether they're an \
             automated account.",
            object(
                json!({ "person": string("A person's ID, person_…, as find_people gives it.") }),
                &["person"],
            ),
        ),
        "get_attachment" => (
            "Read an attachment of a message.",
            object(
                json!({ "message": message(), "attachment": string("The attachment's ID.") }),
                &["message", "attachment"],
            ),
        ),
        "reply" => (
            "Post a message in the thread you were asked in, now, before you finish.",
            object(json!({ "text": text() }), &["text"]),
        ),
        "send_message" => (
            "Post a message in a conversation: at its top level, or in the thread of one of its messages. \
             Where you may post is limited, and a person may need to approve it first.",
            object(
                json!({
                    "conversation": conversation(),
                    "text": text(),
                    "thread": string("A message in that conversation, chmsg_…, to reply in its thread."),
                }),
                &["conversation", "text"],
            ),
        ),
        "send_direct_message" => (
            "Send a direct message to one person. Where you may post is limited, and a person may need to \
             approve it first.",
            object(
                json!({ "person": string("A person's ID, person_…, as find_people gives it."), "text": text() }),
                &["person", "text"],
            ),
        ),
        "react" => (
            "Add an emoji reaction to a message, or take yours away.",
            object(
                json!({
                    "message": message(),
                    "emoji": string("The emoji's name, such as eyes or white_check_mark."),
                    "remove": { "type": "boolean", "description": "Take your reaction away instead." },
                }),
                &["message", "emoji"],
            ),
        ),
        "edit_message" => (
            "Change the text of a message you posted.",
            object(json!({ "message": message(), "text": text() }), &["message", "text"]),
        ),
        "delete_message" => (
            "Delete a message you posted.",
            object(json!({ "message": message() }), &["message"]),
        ),
        STAY_SILENT => (
            "Post nothing in reply to the thread, with a short reason why: when nothing in it needs an answer \
             from you. The run ends once your other calls are done.",
            object(
                json!({ "reason": string("Why you're not answering, for the people who run you: up to 500 characters.") }),
                &["reason"],
            ),
        ),
        _ => return None,
    })
}

/// The SHA-256 of a tool's definition (its name, description and schema)
/// as canonical JSON: the same for every connection, and for as long as the
/// definition is.
pub fn definition_sha256(tool: &str) -> Option<Sha256Digest> {
    let (description, schema) = definition(tool)?;
    let canonical = canonical_json(&json!({
        "name": tool,
        "description": description,
        "inputSchema": schema,
    }));
    Some(Sha256Digest::of(canonical.as_bytes()))
}

/// The connection a channel tool is offered through, as the session reads
/// it when it starts.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OfferingConnection<'a> {
    pub id: ChannelConnectionId,
    pub provider: ChannelProviderKind,
    pub short_id: &'a str,
    /// Kept on the offered tool for the Console; the model never reads it.
    pub label: &'a str,
    pub handle: &'a str,
}

/// The channel tools a session offers through `connection`: each tool the
/// agent channel grants that the provider can do and the organization
/// doesn't deny (`level` gives its level), in [`super::CHANNEL_TOOLS`]'s
/// order.
pub fn offered(
    connection: &OfferingConnection<'_>,
    granted: &[String],
    level: &dyn Fn(&str) -> ToolLevel,
) -> Vec<OfferedTool> {
    super::CHANNEL_TOOLS
        .iter()
        .filter(|(tool, _, _)| granted.iter().any(|granted| granted == tool))
        .filter(|(tool, _, _)| provider_offers(connection.provider, tool))
        .filter(|(tool, _, _)| level(tool) != ToolLevel::Deny)
        .filter_map(|(tool, _, _)| offered_tool(connection, tool))
        .collect()
}

/// One channel tool, as offered through `connection`.
pub fn offered_tool(connection: &OfferingConnection<'_>, tool: &str) -> Option<OfferedTool> {
    let (description, schema) = definition(tool)?;
    let name = tool_name(connection.provider.as_str(), connection.short_id, tool).ok()?;
    let handle = connection.handle.trim_start_matches('@');
    Some(OfferedTool {
        name,
        description: format!(
            "{}, as @{handle} ({}). {description}",
            connection.provider.name(),
            connection.short_id,
        ),
        input_schema: schema,
        installation_id: None,
        channel_connection_id: Some(connection.id),
        memory_type_id: None,
        installation_short_id: connection.short_id.to_owned(),
        installation_label: connection.label.to_owned(),
        app_key: connection.provider.as_str().to_owned(),
        app_name: connection.provider.name().to_owned(),
        tool: tool.to_owned(),
        definition_sha256: definition_sha256(tool)?,
    })
}

/// `stay_silent`, as every channel session offers it.
pub fn stay_silent() -> Option<OfferedTool> {
    let (description, schema) = definition(STAY_SILENT)?;
    Some(OfferedTool {
        name: STAY_SILENT.to_owned(),
        description: description.to_owned(),
        input_schema: schema,
        installation_id: None,
        channel_connection_id: None,
        memory_type_id: None,
        installation_short_id: String::new(),
        installation_label: String::new(),
        app_key: String::new(),
        app_name: String::new(),
        tool: STAY_SILENT.to_owned(),
        definition_sha256: definition_sha256(STAY_SILENT)?,
    })
}

/// The reason a `stay_silent` call gives, trimmed: `None` when it gives
/// none, or one longer than [`MAX_SILENT_REASON`].
pub fn silent_reason(arguments: &Map<String, Value>) -> Option<String> {
    let reason = arguments.get("reason")?.as_str()?.trim();
    (!reason.is_empty() && reason.chars().count() <= MAX_SILENT_REASON).then(|| reason.to_owned())
}

#[cfg(test)]
mod tests {
    use uuid::Uuid;

    use super::*;
    use crate::channels::CHANNEL_TOOLS;
    use crate::models::{Message, Turn, validate_turn};
    use crate::runs::names::{Offer, ToolHost};

    fn connection() -> OfferingConnection<'static> {
        OfferingConnection {
            id: ChannelConnectionId::from_uuid(Uuid::from_u128(5)),
            provider: ChannelProviderKind::Slack,
            short_id: "k7m2qd",
            label: "helper",
            handle: "helper",
        }
    }

    #[test]
    fn every_channel_tool_and_stay_silent_is_defined() {
        for (tool, _, _) in CHANNEL_TOOLS {
            let (description, schema) = definition(tool).expect("defined");
            assert!(!description.is_empty());
            assert_eq!(schema["type"], "object", "{tool}");
            assert!(definition_sha256(tool).is_some());
        }
        assert!(definition(STAY_SILENT).is_some());
        assert!(definition("post_everywhere").is_none());
    }

    #[test]
    fn offers_what_is_granted_offered_and_not_denied_in_order() {
        let granted: Vec<String> = [
            "delete_message",
            "reply",
            "get_attachment",
            "send_message",
            "list_conversations",
        ]
        .into_iter()
        .map(str::to_owned)
        .collect();
        let level = |tool: &str| {
            if tool == "send_message" {
                ToolLevel::Deny
            } else {
                ToolLevel::Allow
            }
        };
        let tools = offered(&connection(), &granted, &level);
        let names: Vec<&str> = tools.iter().map(|tool| tool.tool.as_str()).collect();
        assert_eq!(names, ["list_conversations", "reply", "delete_message"]);
        let reply = &tools[1];
        assert_eq!(reply.name, "slack_k7m2qd_reply");
        assert_eq!(
            reply.description,
            "Slack, as @helper (k7m2qd). Post a message in the thread you were asked in, now, \
             before you finish."
        );
        assert_eq!(reply.host(), ToolHost::Channel(connection().id));
        assert_eq!(reply.app_key, "slack");
        assert_eq!(reply.installation_label, "helper");
    }

    #[test]
    fn a_definition_is_the_same_everywhere_and_always() {
        let other = OfferingConnection {
            id: ChannelConnectionId::from_uuid(Uuid::from_u128(6)),
            short_id: "p3x7ta",
            ..connection()
        };
        let one = offered_tool(&connection(), "reply").expect("offered");
        let two = offered_tool(&other, "reply").expect("offered");
        assert_eq!(one.definition_sha256, two.definition_sha256);
        assert_ne!(one.name, two.name);
        assert_eq!(offered_tool(&connection(), "reply"), Some(one));
    }

    #[test]
    fn stay_silent_is_the_runs_own_and_takes_a_reason() {
        let tool = stay_silent().expect("offered");
        assert_eq!(tool.host(), ToolHost::Run);
        assert_eq!(tool.name, STAY_SILENT);
        let reason = |value: Value| silent_reason(value.as_object().expect("an object"));
        assert_eq!(
            reason(json!({ "reason": "  Not for me.  " })).as_deref(),
            Some("Not for me.")
        );
        assert_eq!(reason(json!({ "reason": "   " })), None);
        assert_eq!(reason(json!({})), None);
        assert_eq!(reason(json!({ "reason": 7 })), None);
        assert_eq!(reason(json!({ "reason": "x".repeat(MAX_SILENT_REASON + 1) })), None);
        assert!(reason(json!({ "reason": "x".repeat(MAX_SILENT_REASON) })).is_some());
    }

    #[test]
    fn a_session_offer_is_one_a_model_takes() {
        let granted: Vec<String> = CHANNEL_TOOLS.iter().map(|(tool, _, _)| (*tool).to_owned()).collect();
        let mut tools = offered(&connection(), &granted, &|_| ToolLevel::Allow);
        tools.extend(stay_silent());
        let offer = Offer::new(tools).expect("an offer");
        let specs = offer.specs();
        let messages = [Message::User { text: "hi".to_owned() }];
        let turn = Turn {
            model: "m",
            instructions: "",
            messages: &messages,
            tools: &specs,
            memory: None,
            cache_key: None,
        };
        assert_eq!(validate_turn(&turn), Ok(()));
        assert!(offer.find(STAY_SILENT).is_some());
        assert!(posts("reply") && posts("send_direct_message") && !posts("react"));
        assert!(writes("react") && writes("delete_message") && !writes("read_thread"));
    }
}
