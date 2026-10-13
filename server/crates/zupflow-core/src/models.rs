//! Calling a model ([`ModelProvider`]): a turn of a conversation, with the
//! tools the agent may call, in; text or tool calls out, with the tokens it
//! took. Which models an organization can use, through which connection and
//! key, is [`crate::model_store`]; calling a real provider is a deployment's
//! own adapter, and this repository ships only a scripted model.

use async_trait::async_trait;

// ---------------------------------------------------------------------------
// Calling a model

/// The longest name a tool is given to a model by: what providers accept.
pub const MAX_MODEL_TOOL_NAME: usize = 64;

/// The longest ID a model may give a tool call.
pub const MAX_CALL_ID: usize = 256;

/// The longest tool name a model's call may carry, in characters. A call
/// naming a tool it wasn't given is refused; one past this is no answer.
pub const MAX_CALL_NAME: usize = 256;

/// A tool as a model is given it: a name it calls it by, what it does and
/// the JSON Schema of its arguments. Built by the run engine from a tool's
/// pinned definition, never from a changed one.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolSpec {
    /// `[A-Za-z0-9_-]`, 1 to [`MAX_MODEL_TOOL_NAME`] characters, unique in a turn.
    pub name: String,
    pub description: String,
    /// A JSON object.
    pub input_schema: serde_json::Value,
}

/// A tool call a model asked for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModelToolCall {
    /// The model's own ID for the call, unique in the conversation: what a
    /// tool result answers. Opaque; never shown in the API.
    pub id: String,
    /// The name the tool was given by.
    pub name: String,
    pub arguments: serde_json::Map<String, serde_json::Value>,
}

/// One message of a conversation with a model.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Message {
    /// What the run was started with.
    User { text: String },
    /// What the model answered: text, tool calls, or both.
    Assistant {
        text: Option<String>,
        tool_calls: Vec<ModelToolCall>,
    },
    /// The outcome of one of the previous assistant message's calls. A
    /// refusal, a denial or a failure is an error result, with its reason.
    ToolResult {
        call_id: String,
        content: String,
        is_error: bool,
    },
}

/// One turn: everything a model is sent.
#[derive(Debug, Clone, Copy)]
pub struct Turn<'a> {
    /// The provider's own model ID, as the run's model names it.
    pub model: &'a str,
    /// The agent's instructions, as written.
    pub instructions: &'a str,
    pub messages: &'a [Message],
    pub tools: &'a [ToolSpec],
}

/// Tokens a turn took, as the provider counts them.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct TokenUsage {
    pub input_tokens: u64,
    pub output_tokens: u64,
}

/// What a model answered.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Reply {
    /// A final answer: the run is done.
    Text(String),
    /// Calls to make, with any text that came with them.
    ToolCalls {
        text: Option<String>,
        calls: Vec<ModelToolCall>,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Completion {
    pub reply: Reply,
    pub usage: TokenUsage,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum ProviderError {
    /// No answer, or not yet (a timeout, a rate limit, a `5xx`): worth
    /// trying again later. Nothing a model is sent acts on anything, so a
    /// turn is safe to repeat.
    #[error("the model provider is unavailable")]
    Unavailable,
    /// The provider doesn't serve this model.
    #[error("the model isn't served")]
    UnknownModel,
    /// This deployment has no model provider: no run can be answered.
    #[error("no model provider is configured")]
    NotConfigured,
    /// The turn breaks [`validate_turn`]: a bug on our side.
    #[error("the turn is invalid: {0}")]
    InvalidTurn(&'static str),
    /// The provider refused the turn or answered something unusable, with
    /// what was wrong. Never the provider's own words. Not worth repeating.
    #[error("the model provider failed: {0}")]
    Failed(&'static str),
}

/// Calls a model: one turn of a conversation in, text or tool calls out,
/// with the tokens it took. Replaceable whole; the basic implementation is a
/// scripted model, and a deployment brings adapters for real providers
/// (decision 0014): no provider, key or vendor configuration lives in this
/// repository.
///
/// Every implementation refuses a turn [`validate_turn`] refuses
/// (`InvalidTurn`), and answers a [`Completion`] that
/// [`validate_completion`] accepts.
#[async_trait]
pub trait ModelProvider: Send + Sync {
    /// Recorded with failures; never shown to the model.
    fn name(&self) -> &'static str;

    async fn complete(&self, turn: &Turn<'_>) -> Result<Completion, ProviderError>;
}

fn valid_tool_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= MAX_MODEL_TOOL_NAME
        && name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
}

fn valid_call_id(id: &str) -> bool {
    !id.is_empty() && id.len() <= MAX_CALL_ID && !id.chars().any(char::is_control)
}

/// What every provider holds a turn to: tools with valid, unique names and
/// object schemas; a conversation that starts with the user; every tool
/// result answering a call of the assistant message before it, each call
/// answered once and before anything else follows; and a conversation that
/// ends waiting for the model (a user message, or every call answered).
pub fn validate_turn(turn: &Turn<'_>) -> Result<(), &'static str> {
    if turn.model.is_empty() {
        return Err("no model");
    }
    let mut names = std::collections::HashSet::new();
    for tool in turn.tools {
        if !valid_tool_name(&tool.name) {
            return Err("a tool name a model can't call");
        }
        if !names.insert(tool.name.as_str()) {
            return Err("a tool name given twice");
        }
        if !tool.input_schema.is_object() {
            return Err("a tool schema that isn't an object");
        }
    }
    let Some(Message::User { .. }) = turn.messages.first() else {
        return Err("a conversation that doesn't start with the user");
    };
    // The calls of the last assistant message not answered yet.
    let mut unanswered: Vec<&str> = Vec::new();
    let mut seen_calls = std::collections::HashSet::new();
    for message in turn.messages {
        match message {
            Message::User { .. } => {
                if !unanswered.is_empty() {
                    return Err("a message before every call was answered");
                }
            }
            Message::Assistant { text, tool_calls } => {
                if !unanswered.is_empty() {
                    return Err("a message before every call was answered");
                }
                if text.is_none() && tool_calls.is_empty() {
                    return Err("an empty assistant message");
                }
                for call in tool_calls {
                    if !valid_call_id(&call.id) || !seen_calls.insert(call.id.as_str()) {
                        return Err("a call ID that is empty, too long or repeated");
                    }
                    unanswered.push(&call.id);
                }
            }
            Message::ToolResult { call_id, .. } => {
                let Some(position) = unanswered.iter().position(|id| id == call_id) else {
                    return Err("a tool result that answers no open call");
                };
                unanswered.remove(position);
            }
        }
    }
    match turn.messages.last() {
        Some(Message::Assistant { .. }) => Err("a conversation already answered"),
        _ if !unanswered.is_empty() => Err("a call not answered"),
        _ => Ok(()),
    }
}

/// What every provider's answer is held to: tool calls, when there are
/// any, with IDs that are valid, unique and new to the conversation, and
/// names. A call naming a tool the turn didn't give is a model's mistake,
/// not a provider's: the run engine answers it with an error.
pub fn validate_completion(turn: &Turn<'_>, completion: &Completion) -> Result<(), &'static str> {
    let Reply::ToolCalls { calls, .. } = &completion.reply else {
        return Ok(());
    };
    if calls.is_empty() {
        return Err("tool calls without a call");
    }
    let mut seen: std::collections::HashSet<&str> = turn
        .messages
        .iter()
        .flat_map(|message| match message {
            Message::Assistant { tool_calls, .. } => tool_calls.iter().map(|call| call.id.as_str()).collect(),
            _ => Vec::new(),
        })
        .collect();
    for call in calls {
        if !valid_call_id(&call.id) || !seen.insert(call.id.as_str()) {
            return Err("a call ID that is empty, too long or repeated");
        }
        if call.name.is_empty() {
            return Err("a call without a tool name");
        }
        if call.name.chars().count() > MAX_CALL_NAME {
            return Err("a call naming a tool longer than any");
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    fn tool(name: &str) -> ToolSpec {
        ToolSpec {
            name: name.to_owned(),
            description: String::new(),
            input_schema: json!({ "type": "object" }),
        }
    }

    fn user(text: &str) -> Message {
        Message::User { text: text.to_owned() }
    }

    fn calls(ids: &[&str]) -> Message {
        Message::Assistant {
            text: None,
            tool_calls: ids
                .iter()
                .map(|id| ModelToolCall {
                    id: (*id).to_owned(),
                    name: "search".to_owned(),
                    arguments: serde_json::Map::new(),
                })
                .collect(),
        }
    }

    fn result(id: &str) -> Message {
        Message::ToolResult {
            call_id: id.to_owned(),
            content: "ok".to_owned(),
            is_error: false,
        }
    }

    fn check(messages: &[Message], tools: &[ToolSpec]) -> Result<(), &'static str> {
        validate_turn(&Turn {
            model: "m",
            instructions: "",
            messages,
            tools,
        })
    }

    #[test]
    fn holds_a_turn_to_the_conversation_rules() {
        let tools = [tool("search")];
        assert_eq!(check(&[user("hi")], &tools), Ok(()));
        assert_eq!(
            check(&[user("hi"), calls(&["a", "b"]), result("b"), result("a")], &tools),
            Ok(())
        );
        assert!(check(&[], &tools).is_err(), "empty");
        assert!(
            check(&[calls(&["a"]), result("a")], &tools).is_err(),
            "not the user first"
        );
        assert!(
            check(&[user("hi"), result("a")], &tools).is_err(),
            "a result for no call"
        );
        assert!(
            check(&[user("hi"), calls(&["a", "b"]), result("a")], &tools).is_err(),
            "one unanswered"
        );
        assert!(
            check(&[user("hi"), calls(&["a"]), user("again")], &tools).is_err(),
            "interrupted"
        );
        assert!(
            check(&[user("hi"), calls(&["a"]), result("a"), result("a")], &tools).is_err(),
            "twice"
        );
        assert!(
            check(
                &[user("hi"), calls(&["a"]), result("a"), calls(&["a"]), result("a")],
                &tools
            )
            .is_err(),
            "an ID reused"
        );
        assert!(check(&[user("hi"), calls(&["a"])], &tools).is_err(), "waiting on calls");
        assert!(
            check(&[user("hi")], &[tool("search"), tool("search")]).is_err(),
            "names twice"
        );
        assert!(
            check(&[user("hi")], &[tool("has space")]).is_err(),
            "a name a model can't call"
        );
        assert!(check(&[user("hi")], &[tool(&"x".repeat(65))]).is_err(), "too long");
    }

    #[test]
    fn holds_a_completion_to_new_unique_call_ids() {
        let messages = [user("hi"), calls(&["a"]), result("a")];
        let turn = Turn {
            model: "m",
            instructions: "",
            messages: &messages,
            tools: &[],
        };
        let answer = |ids: &[&str]| Completion {
            reply: match calls(ids) {
                Message::Assistant { tool_calls, .. } => Reply::ToolCalls {
                    text: None,
                    calls: tool_calls,
                },
                _ => Reply::Text(String::new()),
            },
            usage: TokenUsage::default(),
        };
        assert_eq!(validate_completion(&turn, &answer(&["b", "c"])), Ok(()));
        assert!(validate_completion(&turn, &answer(&[])).is_err());
        assert!(validate_completion(&turn, &answer(&["b", "b"])).is_err());
        assert!(validate_completion(&turn, &answer(&["a"])).is_err(), "already used");
        assert!(validate_completion(&turn, &answer(&[""])).is_err());
        let named = |name: String| Completion {
            reply: Reply::ToolCalls {
                text: None,
                calls: vec![ModelToolCall {
                    id: "b".to_owned(),
                    name,
                    arguments: serde_json::Map::new(),
                }],
            },
            usage: TokenUsage::default(),
        };
        assert_eq!(validate_completion(&turn, &named("x".repeat(MAX_CALL_NAME))), Ok(()));
        assert!(validate_completion(&turn, &named("x".repeat(MAX_CALL_NAME + 1))).is_err());
    }
}
