//! OpenAI's chat completions, as OpenAI and OpenRouter take them: a turn as
//! a request body, and the answer as a [`Completion`].
//!
//! - The agent's instructions are a `system` message (none when empty);
//!   the run's input a `user` message; the model's answers `assistant`
//!   messages with their `tool_calls` (arguments as a JSON string); each
//!   call's outcome a `tool` message naming its `tool_call_id`, an error
//!   outcome led by `Error: `, since the protocol has no flag for it.
//! - Tools are `function` tools, offered with `tool_choice: auto`.
//! - The request names exactly one model, the run's, and never `models`
//!   or `openrouter/auto`: through OpenRouter it may be served by any host
//!   of that model, never by another model.
//! - Output is capped at [`MAX_OUTPUT_TOKENS`]: `max_completion_tokens` to
//!   OpenAI (which its reasoning models require), `max_tokens` to
//!   OpenRouter (which it maps for each host).
//! - The answer is `choices[0]`: its `tool_calls`, else its text.
//!   `finish_reason` `length` or `content_filter` fails the turn (a cut-off
//!   call or answer isn't one), `error` (OpenRouter) is worth trying again.
//!   Tokens are `usage.prompt_tokens` and `usage.completion_tokens`.

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

fn message(message: &Message) -> Value {
    match message {
        Message::User { text } => json!({ "role": "user", "content": text }),
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
            content,
            is_error,
        } => {
            let content = if *is_error {
                format!("{ERROR_PREFIX}{content}")
            } else {
                content.clone()
            };
            json!({ "role": "tool", "tool_call_id": call_id, "content": content })
        }
    }
}

/// The request body for a turn, to `kind`.
pub fn request(kind: ProviderKind, turn: &Turn<'_>) -> Value {
    let mut messages = Vec::with_capacity(turn.messages.len() + 1);
    if !turn.instructions.is_empty() {
        messages.push(json!({ "role": "system", "content": turn.instructions }));
    }
    messages.extend(turn.messages.iter().map(message));
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
    let usage = TokenUsage {
        input_tokens: tokens(usage.get("prompt_tokens").unwrap_or(&Value::Null)),
        output_tokens: tokens(usage.get("completion_tokens").unwrap_or(&Value::Null)),
    };
    let reply = if !calls.is_empty() {
        Reply::ToolCalls { text, calls }
    } else {
        Reply::Text(text.ok_or(ProviderError::Failed("an empty answer"))?)
    };
    Ok(Completion { reply, usage })
}
