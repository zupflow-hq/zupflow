//! Calling a model ([`ModelProvider`]): a turn of a conversation, with the
//! tools the agent may call, in; text or tool calls out, with the tokens it
//! took. Which models an organization can use, through which connection and
//! key, is [`crate::model_store`]; each turn goes to the run's connection's
//! provider with its key, opened for that call alone ([`ProviderAccess`]).
//! The adapters for OpenAI, Anthropic and OpenRouter are `zupflow-providers`
//! (decision 0049); `zupflow-basic` has a scripted model for tests.

use std::time::Duration;

use async_trait::async_trait;

use crate::ids::{AgentId, OrganizationId, WorkspaceId};
use crate::model_providers::{ProviderKey, ProviderKind};
use crate::runs::digest::digest_json;

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
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct ModelToolCall {
    /// The model's own ID for the call, unique in the conversation: what a
    /// tool result answers. Opaque; never shown in the API.
    pub id: String,
    /// The name the tool was given by.
    pub name: String,
    pub arguments: serde_json::Map<String, serde_json::Value>,
}

/// One message of a conversation with a model. Kept as JSON once a channel
/// run ends (its transcript), and read back exactly as it was.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(tag = "role", rename_all = "snake_case")]
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
///
/// A run's turns only ever grow at the end: the same model, instructions,
/// tools and memory (frozen at the run's first model attempt), and the
/// conversation of the turn before with what came of it after. That keeps
/// every turn's request a prefix of the next one, which providers cache.
#[derive(Debug, Clone, Copy)]
pub struct Turn<'a> {
    /// The provider's own model ID, as the run's model names it.
    pub model: &'a str,
    /// The agent's instructions, as written.
    pub instructions: &'a str,
    pub messages: &'a [Message],
    pub tools: &'a [ToolSpec],
    /// What the run recalled from its agent's memory, framed: sent as the
    /// first block of the first user message, before what the run was
    /// started with, and never in the instructions, so a changed memory
    /// leaves the instructions and tools before it cached. The same bytes on
    /// every turn of a run. `None` (nothing recalled) sends no block; never
    /// empty.
    pub memory: Option<&'a str>,
    /// What a provider that caches prompts by prefix on its own routes the
    /// turn by, so turns sharing a prefix reach the same cache
    /// ([`prompt_cache_key`]): never a secret, and the same for every run of
    /// an agent with the same tools. `None` sends none.
    pub cache_key: Option<&'a str>,
}

/// The domain of [`prompt_cache_key`]'s digest.
pub const PROMPT_CACHE_KEY_DOMAIN: &str = "zupflow.prompt_cache_key.v1";

/// The longest [`prompt_cache_key`]: within what providers take.
pub const MAX_PROMPT_CACHE_KEY: usize = 64;

/// The prompt cache key of an agent's turns offering `tools`: `zf_` and 32
/// hex characters of a digest over the organization, the workspace, the
/// agent and the tools, so every run of the agent with the same tools
/// shares it, and nothing else does. A digest, so it names nothing the
/// provider could read; no key, credential or input is in it.
pub fn prompt_cache_key(
    organization_id: OrganizationId,
    workspace_id: WorkspaceId,
    agent_id: AgentId,
    tools: &[ToolSpec],
) -> String {
    let tools: Vec<serde_json::Value> = tools
        .iter()
        .map(|tool| {
            serde_json::json!({
                "name": tool.name,
                "description": tool.description,
                "input_schema": tool.input_schema,
            })
        })
        .collect();
    let digest = digest_json(
        PROMPT_CACHE_KEY_DOMAIN,
        &serde_json::json!({
            "organization_id": organization_id,
            "workspace_id": workspace_id,
            "agent_id": agent_id,
            "tools": tools,
        }),
    );
    format!("zf_{}", &digest.to_hex()[..32])
}

/// Where a turn goes: the run's connection's provider and its key, opened
/// for this call and dropped (wiped) after it. An adapter sends the key as
/// the provider's credential and nowhere else.
#[derive(Debug, Clone, Copy)]
pub struct ProviderAccess<'a> {
    pub kind: ProviderKind,
    pub key: &'a ProviderKey,
}

/// Tokens a turn took, as the provider counts them.
///
/// `input_tokens` is every token the model read, whichever provider: those
/// it read from the provider's prompt cache and those it wrote to it are
/// parts of it, never added on top. Anthropic reports the three apart (its
/// own `input_tokens` is only what's after the last cache breakpoint), so
/// its adapter adds them up; OpenAI's and OpenRouter's `prompt_tokens`
/// already count them. Build one with [`TokenUsage::new`], which keeps the
/// parts within the whole.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct TokenUsage {
    pub input_tokens: u64,
    pub output_tokens: u64,
    /// The part of `input_tokens` read from the provider's prompt cache.
    pub cache_read_input_tokens: u64,
    /// The part of `input_tokens` written to the provider's prompt cache.
    /// Providers that cache on their own (OpenAI) report none.
    pub cache_write_input_tokens: u64,
}

impl TokenUsage {
    /// Usage as a provider reported it: `input` in all, `read` of it from
    /// the cache and `written` to it. Parts a provider reported past the
    /// whole are cut to fit, so read and written never add up to more than
    /// the input.
    pub fn new(input: u64, output: u64, read: u64, written: u64) -> Self {
        let read = read.min(input);
        Self {
            input_tokens: input,
            output_tokens: output,
            cache_read_input_tokens: read,
            cache_write_input_tokens: written.min(input - read),
        }
    }
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

/// The longest wait a provider's `Retry-After` is taken at: a longer one is
/// cut to this, so a run is never parked for long on a provider's word.
pub const MAX_RETRY_AFTER: Duration = Duration::from_secs(60);

#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum ProviderError {
    /// No answer, or not yet (a rate limit, a `5xx`, no connection): worth
    /// trying again later. Nothing a model is sent acts on anything, so a
    /// turn is safe to repeat.
    #[error("the model provider is unavailable")]
    Unavailable,
    /// As `Unavailable`, and the provider said when to try again (its
    /// `Retry-After`, at most [`MAX_RETRY_AFTER`]).
    #[error("the model provider asked to wait")]
    RetryAfter(Duration),
    /// The turn was sent and no answer came in time. The provider may have
    /// worked on it, and billed it, all the same: tried again at most once.
    #[error("the model provider timed out")]
    TimedOut,
    /// The provider doesn't serve this model (it answered `404`, or said so
    /// in a way its kind is known to).
    #[error("the model isn't served")]
    UnknownModel,
    /// The provider refused the key (it answered `401`).
    #[error("the provider refused the key")]
    KeyRefused,
    /// The provider knows the key but refused this request with it (`403`):
    /// a moderation flag, a region, a permission. The key stays as it is.
    #[error("the provider refused the request")]
    Forbidden,
    /// The provider account behind the key can't pay for the turn (`402`).
    #[error("the provider account is out of credits")]
    OutOfCredits,
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
/// with the tokens it took. Replaceable whole: the provider adapters
/// (`zupflow-providers`) reach each kind of provider through `OutboundHttp`;
/// the scripted model answers tests and local work. No key or vendor
/// configuration lives in this repository: keys are organizations' own.
///
/// Every implementation refuses a turn [`validate_turn`] refuses
/// (`InvalidTurn`), and answers a [`Completion`] that
/// [`validate_completion`] accepts; a key the provider refuses is
/// `KeyRefused` and a model it doesn't serve `UnknownModel`; and the key
/// is never in what it answers or logs.
#[async_trait]
pub trait ModelProvider: Send + Sync {
    /// Recorded with failures; never shown to the model.
    fn name(&self) -> &'static str;

    async fn complete(&self, access: &ProviderAccess<'_>, turn: &Turn<'_>) -> Result<Completion, ProviderError>;
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
    if turn.memory.is_some_and(str::is_empty) {
        return Err("an empty memory");
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
            memory: None,
            cache_key: None,
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
            memory: None,
            cache_key: None,
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

    #[test]
    fn keeps_cache_counts_within_the_input() {
        assert_eq!(
            TokenUsage::new(100, 7, 60, 30),
            TokenUsage {
                input_tokens: 100,
                output_tokens: 7,
                cache_read_input_tokens: 60,
                cache_write_input_tokens: 30,
            }
        );
        let past = TokenUsage::new(100, 7, 90, 30);
        assert_eq!((past.cache_read_input_tokens, past.cache_write_input_tokens), (90, 10));
        let over = TokenUsage::new(10, 0, 50, 50);
        assert_eq!((over.cache_read_input_tokens, over.cache_write_input_tokens), (10, 0));
    }

    #[test]
    fn keys_a_prompt_cache_by_agent_and_tools_only() {
        use crate::ids::{AgentId, OrganizationId, WorkspaceId};

        let (org, ws, agent) = (OrganizationId::generate(), WorkspaceId::generate(), AgentId::generate());
        let tools = [tool("search")];
        let key = prompt_cache_key(org, ws, agent, &tools);
        assert!(
            key.starts_with("zf_") && key.len() == 35 && key.len() <= MAX_PROMPT_CACHE_KEY,
            "{key}"
        );
        assert!(key[3..].bytes().all(|b| b.is_ascii_hexdigit()), "{key}");
        assert_eq!(key, prompt_cache_key(org, ws, agent, &tools), "the same every run");
        for id in [org.to_string(), ws.to_string(), agent.to_string()] {
            assert!(!key.contains(&id), "names nothing the provider could read");
        }
        assert_ne!(
            key,
            prompt_cache_key(org, ws, AgentId::generate(), &tools),
            "another agent"
        );
        assert_ne!(
            key,
            prompt_cache_key(org, WorkspaceId::generate(), agent, &tools),
            "another workspace"
        );
        assert_ne!(
            key,
            prompt_cache_key(OrganizationId::generate(), ws, agent, &tools),
            "another organization"
        );
        assert_ne!(key, prompt_cache_key(org, ws, agent, &[tool("other")]), "other tools");
        let mut described = tool("search");
        described.description = "Searches.".to_owned();
        assert_ne!(
            key,
            prompt_cache_key(org, ws, agent, &[described]),
            "a changed description"
        );
    }
}
