//! OpenAI's chat completions, as OpenAI and OpenRouter take them: a turn as
//! a request body, and the answer as a [`Completion`].
//!
//! - The agent's instructions are a `system` message (none when empty);
//!   the run's input a `user` message; the model's answers `assistant`
//!   messages with their `tool_calls` (arguments as a JSON string); each
//!   call's outcome a `tool` message naming its `tool_call_id`, an error
//!   outcome led by `Error: `, since the protocol has no flag for it.
//! - What the run recalled from its memory ([`Turn::memory`]) is a `text`
//!   part of its own, the first of the first user message, before the
//!   input's, and never in the system message: a changed memory leaves the
//!   tools and instructions before it cached. No recall, no part, and the
//!   message is as it would be without memory.
//! - Tools are `function` tools, offered with `tool_choice: auto`.
//! - Caching: OpenAI caches a prompt's prefix on its own, and OpenRouter
//!   does for most hosts; both are sent the turn's `prompt_cache_key`
//!   (OpenAI routes by it, OpenRouter keeps a key's turns on one host), so
//!   an agent's runs reach the same cache. Claude through OpenRouter
//!   (`anthropic/` models) caches only at breakpoints, as Messages does
//!   ([`crate::messages`]): its system, user and tool messages go as `text`
//!   parts, and `cache_control` marks the system part, the recall's, the
//!   last part of the last message and the last part of the message before
//!   the latest answer, where the turn before put its last one: four at
//!   most. Two gaps against
//!   Messages: tools are never marked (OpenRouter documents breakpoints on
//!   content parts only), so an agent without instructions gets no
//!   breakpoint shared across its runs; and empty text stays a string,
//!   unmarked, since Messages refuses an empty `text` block, so a turn
//!   ending with an empty tool result leaves that breakpoint out and the
//!   next turn reads only what an earlier breakpoint cached.
//! - The request names exactly one model, the run's, and never `models`
//!   or `openrouter/auto`: through OpenRouter it may be served by any host
//!   of that model, never by another model.
//! - Output is capped at [`MAX_OUTPUT_TOKENS`]: `max_completion_tokens` to
//!   OpenAI (which its reasoning models require), `max_tokens` to
//!   OpenRouter (which it maps for each host).
//! - The answer is `choices[0]`: its `tool_calls`, else its text.
//!   `finish_reason` `length` or `content_filter` fails the turn (a cut-off
//!   call or answer isn't one), `error` (OpenRouter) is worth trying again.
//!   Tokens are `usage.prompt_tokens` (which counts those read from and
//!   written to a cache: `prompt_tokens_details.cached_tokens` and, from
//!   OpenRouter, `cache_write_tokens`) and `usage.completion_tokens`.

use crate::MAX_OUTPUT_TOKENS;
use serde_json::{Map, Value, json};
use zupflow_core::model_providers::ProviderKind;
use zupflow_core::models::{Completion, Message, ModelToolCall, ProviderError, Reply, TokenUsage, Turn};

/// Whether a refusal's body is OpenRouter saying it has no such model
/// (`{"error": {"message": "<model> is not a valid model ID"}}`), as it
/// answers `400` for a model it never had or withdrew. Matched on that one
/// phrase, never passed on.
pub fn names_an_unknown_model(body: &[u8]) -> bool {
    serde_json::from_slice::<Value>(body).ok().is_some_and(|body| {
        body.pointer("/error/message")
            .and_then(Value::as_str)
            .is_some_and(|message| message.to_ascii_lowercase().contains("is not a valid model id"))
    })
}

/// What an error outcome is led by, for a model to tell it from a result.
pub const ERROR_PREFIX: &str = "Error: ";

/// A message's text as `content`: a string, or one `text` part when the
/// model caches at breakpoints, which mark parts. Empty text stays a
/// string: Messages refuses an empty `text` block.
fn content(text: &str, parts: bool) -> Value {
    if parts && !text.is_empty() {
        json!([{ "type": "text", "text": text }])
    } else {
        json!(text)
    }
}

fn message(message: &Message, parts: bool) -> Value {
    match message {
        Message::User { text } => json!({ "role": "user", "content": content(text, parts) }),
        Message::Assistant { text, tool_calls } => {
            let mut out = Map::new();
            out.insert("role".to_owned(), json!("assistant"));
            out.insert(
                "content".to_owned(),
                text.as_ref().map_or(Value::Null, |text| json!(text)),
            );
            if !tool_calls.is_empty() {
                let calls: Vec<Value> = tool_calls
                    .iter()
                    .map(|call| {
                        json!({
                            "id": call.id,
                            "type": "function",
                            "function": {
                                "name": call.name,
                                "arguments": Value::Object(call.arguments.clone()).to_string(),
                            },
                        })
                    })
                    .collect();
                out.insert("tool_calls".to_owned(), Value::Array(calls));
            }
            Value::Object(out)
        }
        Message::ToolResult {
            call_id,
            content: text,
            is_error,
        } => {
            let text = if *is_error {
                format!("{ERROR_PREFIX}{text}")
            } else {
                text.clone()
            };
            json!({ "role": "tool", "tool_call_id": call_id, "content": content(&text, parts) })
        }
    }
}

/// Whether a model takes cache breakpoints on its messages' parts: Claude,
/// through OpenRouter.
fn caches_at_breakpoints(kind: ProviderKind, model: &str) -> bool {
    kind == ProviderKind::OpenRouter && model.starts_with("anthropic/")
}

/// Marks a part as a cache breakpoint, unless its text is empty.
fn mark_part(part: &mut Value) {
    if part
        .get("text")
        .and_then(Value::as_str)
        .is_some_and(|text| !text.is_empty())
        && let Value::Object(part) = part
    {
        part.insert("cache_control".to_owned(), json!({ "type": "ephemeral" }));
    }
}

/// Marks a message's last part as a cache breakpoint, unless it has no
/// parts or its text is empty.
fn mark(message: &mut Value) {
    if let Some(part) = message
        .get_mut("content")
        .and_then(Value::as_array_mut)
        .and_then(|parts| parts.last_mut())
    {
        mark_part(part);
    }
}

/// The first user message with the recall as its first part, marked when
/// the model caches at breakpoints, and the input as the next, unless it's
/// empty.
fn with_recall(message: &mut Value, memory: &str, breakpoints: bool) {
    let mut recall = json!({ "type": "text", "text": memory });
    if breakpoints {
        mark_part(&mut recall);
    }
    let mut parts = vec![recall];
    match message.get_mut("content").map(Value::take) {
        Some(Value::Array(input)) => parts.extend(input),
        Some(Value::String(input)) if !input.is_empty() => parts.push(json!({ "type": "text", "text": input })),
        _ => {}
    }
    if let Value::Object(message) = message {
        message.insert("content".to_owned(), Value::Array(parts));
    }
}

/// The request body for a turn, to `kind`.
pub fn request(kind: ProviderKind, turn: &Turn<'_>) -> Value {
    let breakpoints = caches_at_breakpoints(kind, turn.model);
    let mut messages = Vec::with_capacity(turn.messages.len() + 1);
    if !turn.instructions.is_empty() {
        messages.push(json!({ "role": "system", "content": content(turn.instructions, breakpoints) }));
    }
    let first = messages.len();
    messages.extend(turn.messages.iter().map(|each| message(each, breakpoints)));
    // The recall leads the first user message, the conversation's first.
    if let Some(memory) = turn.memory.filter(|memory| !memory.is_empty())
        && let Some(message) = messages.get_mut(first)
    {
        with_recall(message, memory, breakpoints);
    }
    if breakpoints {
        if !turn.instructions.is_empty()
            && let Some(system) = messages.first_mut()
        {
            mark(system);
        }
        if let Some(answered) = messages
            .iter()
            .rposition(|message| message.get("role").and_then(Value::as_str) == Some("assistant"))
            && let Some(before) = answered.checked_sub(1)
            && let Some(message) = messages.get_mut(before)
        {
            mark(message);
        }
        if let Some(last) = messages.last_mut() {
            mark(last);
        }
    }
    let mut body = Map::new();
    body.insert("model".to_owned(), json!(turn.model));
    body.insert("messages".to_owned(), Value::Array(messages));
    if !turn.tools.is_empty() {
        let tools: Vec<Value> = turn
            .tools
            .iter()
            .map(|tool| {
                json!({
                    "type": "function",
                    "function": {
                        "name": tool.name,
                        "description": tool.description,
                        "parameters": tool.input_schema,
                    },
                })
            })
            .collect();
        body.insert("tools".to_owned(), Value::Array(tools));
        body.insert("tool_choice".to_owned(), json!("auto"));
    }
    let cap = match kind {
        ProviderKind::OpenRouter => "max_tokens",
        ProviderKind::OpenAi | ProviderKind::Anthropic => "max_completion_tokens",
    };
    body.insert(cap.to_owned(), json!(MAX_OUTPUT_TOKENS));
    match kind {
        ProviderKind::OpenAi | ProviderKind::OpenRouter => {
            if let Some(key) = turn.cache_key {
                body.insert("prompt_cache_key".to_owned(), json!(key));
            }
        }
        ProviderKind::Anthropic => {}
    }
    Value::Object(body)
}

fn tokens(value: &Value) -> u64 {
    value.as_u64().unwrap_or(0)
}

/// A call's arguments: a JSON object, written as a string (empty for none).
fn arguments(value: &Value) -> Result<Map<String, Value>, ProviderError> {
    let text = match value {
        Value::String(text) => text.as_str(),
        Value::Object(arguments) => return Ok(arguments.clone()),
        Value::Null => "",
        _ => return Err(ProviderError::Failed("tool call arguments that aren't a JSON object")),
    };
    if text.trim().is_empty() {
        return Ok(Map::new());
    }
    match serde_json::from_str::<Value>(text) {
        Ok(Value::Object(arguments)) => Ok(arguments),
        _ => Err(ProviderError::Failed("tool call arguments that aren't a JSON object")),
    }
}

/// The text of a message's `content`: a string, or parts with `text`.
fn content_text(content: &Value) -> Option<String> {
    match content {
        Value::String(text) => Some(text.clone()),
        Value::Array(parts) => {
            let text: String = parts
                .iter()
                .filter_map(|part| part.get("text").and_then(Value::as_str))
                .collect();
            Some(text)
        }
        _ => None,
    }
    .filter(|text| !text.trim().is_empty())
}

/// The answer in a `2xx` body.
pub fn completion(body: &[u8]) -> Result<Completion, ProviderError> {
    let answer: Value = serde_json::from_slice(body).map_err(|_| ProviderError::Failed("an answer that isn't JSON"))?;
    let Some(choice) = answer
        .get("choices")
        .and_then(Value::as_array)
        .and_then(|choices| choices.first())
    else {
        // OpenRouter can answer `200` with an error instead of a choice.
        if answer.get("error").is_some() {
            return Err(ProviderError::Unavailable);
        }
        return Err(ProviderError::Failed("an answer without a choice"));
    };
    match choice.get("finish_reason").and_then(Value::as_str) {
        Some("length") => return Err(ProviderError::Failed("an answer cut off at its length limit")),
        Some("content_filter") => return Err(ProviderError::Failed("an answer the provider filtered")),
        Some("error") => return Err(ProviderError::Unavailable),
        _ => {}
    }
    let message = choice.get("message").unwrap_or(&Value::Null);
    let text = message.get("content").and_then(content_text);
    let mut calls = Vec::new();
    for call in message
        .get("tool_calls")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
    {
        let function = call.get("function").unwrap_or(&Value::Null);
        let (Some(id), Some(name)) = (
            call.get("id").and_then(Value::as_str),
            function.get("name").and_then(Value::as_str),
        ) else {
            return Err(ProviderError::Failed("a tool call without an ID or a name"));
        };
        calls.push(ModelToolCall {
            id: id.to_owned(),
            name: name.to_owned(),
            arguments: arguments(function.get("arguments").unwrap_or(&Value::Null))?,
        });
    }
    let usage = answer.get("usage").unwrap_or(&Value::Null);
    let details = usage.get("prompt_tokens_details").unwrap_or(&Value::Null);
    let usage = TokenUsage::new(
        tokens(usage.get("prompt_tokens").unwrap_or(&Value::Null)),
        tokens(usage.get("completion_tokens").unwrap_or(&Value::Null)),
        tokens(details.get("cached_tokens").unwrap_or(&Value::Null)),
        tokens(details.get("cache_write_tokens").unwrap_or(&Value::Null)),
    );
    let reply = if !calls.is_empty() {
        Reply::ToolCalls { text, calls }
    } else {
        Reply::Text(text.ok_or(ProviderError::Failed("an empty answer"))?)
    };
    Ok(Completion { reply, usage })
}
