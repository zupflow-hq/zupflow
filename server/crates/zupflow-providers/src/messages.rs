//! Anthropic's Messages: a turn as a request body, and the answer as a
//! [`Completion`].
//!
//! - The agent's instructions are `system` (absent when empty), and
//!   `max_tokens` is [`MAX_OUTPUT_TOKENS`]: Messages requires one.
//! - Roles alternate: consecutive messages of one role are one message of
//!   content blocks. The run's input is a `text` block; the model's answers
//!   `text` and `tool_use` blocks (an empty text dropped, which Messages
//!   refuses); each call's outcome a `tool_result` block naming its
//!   `tool_use_id`, with `is_error` for an error outcome, in the `user`
//!   message after the calls, before any text.
//! - Tools are `{ name, description, input_schema }`.
//! - The answer is its `tool_use` blocks, else its text; a `thinking` block
//!   is not kept, so later turns go without it, as Messages accepts when
//!   every one is left out. `stop_reason` `max_tokens` (a cut-off call or
//!   answer isn't one), `refusal` and any other that isn't an answer fail
//!   the turn. Tokens read include those read from or written to a cache.

use crate::MAX_OUTPUT_TOKENS;
use serde_json::{Map, Value, json};
use zupflow_core::models::{Completion, Message, ModelToolCall, ProviderError, Reply, TokenUsage, Turn};

fn blocks(message: &Message) -> (&'static str, Vec<Value>) {
    match message {
        Message::User { text } => ("user", vec![json!({ "type": "text", "text": text })]),
        Message::Assistant { text, tool_calls } => {
            let mut blocks = Vec::with_capacity(tool_calls.len() + 1);
            if let Some(text) = text.as_ref().filter(|text| !text.trim().is_empty()) {
                blocks.push(json!({ "type": "text", "text": text }));
            }
            blocks.extend(tool_calls.iter().map(|call| {
                json!({
                    "type": "tool_use",
                    "id": call.id,
                    "name": call.name,
                    "input": Value::Object(call.arguments.clone()),
                })
            }));
            ("assistant", blocks)
        }
        Message::ToolResult {
            call_id,
            content,
            is_error,
        } => {
            let mut block = Map::new();
            block.insert("type".to_owned(), json!("tool_result"));
            block.insert("tool_use_id".to_owned(), json!(call_id));
            if !content.is_empty() {
                block.insert("content".to_owned(), json!(content));
            }
            if *is_error {
                block.insert("is_error".to_owned(), json!(true));
            }
            ("user", vec![Value::Object(block)])
        }
    }
}

/// The request body for a turn.
pub fn request(turn: &Turn<'_>) -> Value {
    let mut messages: Vec<(&'static str, Vec<Value>)> = Vec::new();
    for message in turn.messages {
        let (role, content) = blocks(message);
        match messages.last_mut() {
            Some((last, blocks)) if *last == role => blocks.extend(content),
            _ => messages.push((role, content)),
        }
    }
    let messages: Vec<Value> = messages
        .into_iter()
        .map(|(role, content)| json!({ "role": role, "content": content }))
        .collect();
    let mut body = Map::new();
    body.insert("model".to_owned(), json!(turn.model));
    body.insert("max_tokens".to_owned(), json!(MAX_OUTPUT_TOKENS));
    if !turn.instructions.is_empty() {
        body.insert("system".to_owned(), json!(turn.instructions));
    }
    body.insert("messages".to_owned(), Value::Array(messages));
    if !turn.tools.is_empty() {
        let tools: Vec<Value> = turn
            .tools
            .iter()
            .map(|tool| {
                let mut out = Map::new();
                out.insert("name".to_owned(), json!(tool.name));
                if !tool.description.is_empty() {
                    out.insert("description".to_owned(), json!(tool.description));
                }
                out.insert("input_schema".to_owned(), tool.input_schema.clone());
                Value::Object(out)
            })
            .collect();
        body.insert("tools".to_owned(), Value::Array(tools));
    }
    Value::Object(body)
}

fn tokens(usage: &Value, field: &str) -> u64 {
    usage.get(field).and_then(Value::as_u64).unwrap_or(0)
}

/// The answer in a `2xx` body.
pub fn completion(body: &[u8]) -> Result<Completion, ProviderError> {
    let answer: Value = serde_json::from_slice(body).map_err(|_| ProviderError::Failed("an answer that isn't JSON"))?;
    match answer.get("stop_reason").and_then(Value::as_str) {
        Some("end_turn" | "tool_use" | "stop_sequence") => {}
        Some("max_tokens" | "model_context_window_exceeded") => {
            return Err(ProviderError::Failed("an answer cut off at its length limit"));
        }
        Some("refusal") => return Err(ProviderError::Failed("the model declined to answer")),
        _ => return Err(ProviderError::Failed("an answer that didn't finish")),
    }
    let mut text = String::new();
    let mut calls = Vec::new();
    for block in answer.get("content").and_then(Value::as_array).into_iter().flatten() {
        match block.get("type").and_then(Value::as_str) {
            Some("text") => text.push_str(block.get("text").and_then(Value::as_str).unwrap_or_default()),
            Some("tool_use") => {
                let (Some(id), Some(name)) = (
                    block.get("id").and_then(Value::as_str),
                    block.get("name").and_then(Value::as_str),
                ) else {
                    return Err(ProviderError::Failed("a tool call without an ID or a name"));
                };
                let arguments = match block.get("input") {
                    Some(Value::Object(input)) => input.clone(),
                    None | Some(Value::Null) => Map::new(),
                    Some(_) => return Err(ProviderError::Failed("tool call arguments that aren't a JSON object")),
                };
                calls.push(ModelToolCall {
                    id: id.to_owned(),
                    name: name.to_owned(),
                    arguments,
                });
            }
            // Thinking, and anything newer: not part of the conversation kept.
            _ => {}
        }
    }
    let usage = answer.get("usage").unwrap_or(&Value::Null);
    let usage = TokenUsage {
        input_tokens: tokens(usage, "input_tokens")
            .saturating_add(tokens(usage, "cache_creation_input_tokens"))
            .saturating_add(tokens(usage, "cache_read_input_tokens")),
        output_tokens: tokens(usage, "output_tokens"),
    };
    let text = Some(text).filter(|text| !text.trim().is_empty());
    let reply = if !calls.is_empty() {
        Reply::ToolCalls { text, calls }
    } else {
        Reply::Text(text.ok_or(ProviderError::Failed("an empty answer"))?)
    };
    Ok(Completion { reply, usage })
}
