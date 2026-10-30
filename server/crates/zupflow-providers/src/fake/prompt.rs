//! A request's prompt as a provider caches it, and the providers' prompt
//! caches, faked.
//!
//! [`Prompt`] reads a turn's body the way the provider's cache sees it:
//! tools, then system, then messages, with every cache breakpoint taken
//! out and named apart ([`Prompt::markers`]). It refuses breakpoints where
//! the provider does: `cache_control` to OpenAI at all; to Anthropic and
//! Claude through OpenRouter, more than four, one that isn't
//! `{"type": "ephemeral"}` (with an optional `ttl` of `5m` or `1h`), or one
//! on an empty text.
//!
//! [`Caches`] answers how much of a prompt a provider read from its cache
//! and wrote to it, counting a token per four bytes of each part's JSON
//! (no minimum length, so short test prompts cache too). A part is a tool,
//! a system block or message, a Messages content block, or a chat
//! message, each of whose content parts is one when it has a list of them
//! (the first with the message's other fields), so a prompt that differs
//! only from a message's second part on still shares its first:
//!
//! - Anthropic, and Claude through OpenRouter, cache only at breakpoints.
//!   At each breakpoint the prompt up to it is looked up, and so is every
//!   shorter one up to [`LOOKBACK`] parts back, among those cached before
//!   at a breakpoint; the longest found is read, the rest up to the last
//!   breakpoint is written, and the prompt up to each breakpoint is kept.
//! - OpenAI and OpenRouter's other models cache every prompt whole, and
//!   read the longest part any earlier one shares with it from the start;
//!   they report nothing written. A `prompt_cache_key` keeps its own
//!   cache, as OpenAI routes by it.
//!
//! Caches are kept per kind and model (and `prompt_cache_key`), never
//! across them. The API key isn't part of it: a provider's cache is its
//! account's, so a rotated key reaches the same one.

use serde_json::{Map, Value, json};
use zupflow_core::model_providers::ProviderKind;

/// The most breakpoints a request may carry.
pub const MAX_BREAKPOINTS: usize = 4;

/// How many parts before a breakpoint Anthropic looks back for a prompt it
/// cached.
pub const LOOKBACK: usize = 20;

/// A request's prompt, in the order a provider caches it, its breakpoints
/// taken out.
#[derive(Debug, Clone, PartialEq)]
pub struct Prompt {
    /// Each tool as sent.
    pub tools: Vec<Value>,
    /// The system blocks (Anthropic) or system messages (chat).
    pub system: Vec<Value>,
    /// The other messages.
    pub messages: Vec<Value>,
    /// Where each breakpoint was, in order: `tools[i]`, `system[i]`, or
    /// `messages[i].content[j]` (`i` counting `messages` above, and
    /// `system[i]` a system message's own part to chat).
    pub markers: Vec<String>,
    /// The parts the cache sees, in order, and which carried a breakpoint.
    parts: Vec<(String, bool)>,
}

/// Whether `kind` caches `model`'s prompts only at breakpoints.
pub fn at_breakpoints(kind: ProviderKind, model: &str) -> bool {
    match kind {
        ProviderKind::Anthropic => true,
        ProviderKind::OpenRouter => model.starts_with("anthropic/"),
        ProviderKind::OpenAi => false,
    }
}

/// Takes a breakpoint out of `value`, checking it as `kind` would.
fn unmark(kind: ProviderKind, value: &mut Value) -> Result<bool, String> {
    let Value::Object(fields) = value else {
        return Ok(false);
    };
    let Some(control) = fields.remove("cache_control") else {
        return Ok(false);
    };
    if kind == ProviderKind::OpenAi {
        return Err("Unrecognized request argument supplied: cache_control".to_owned());
    }
    let Value::Object(control) = control else {
        return Err("cache_control: an object".to_owned());
    };
    if control.get("type") != Some(&json!("ephemeral")) {
        return Err("cache_control.type: ephemeral".to_owned());
    }
    if control.keys().any(|key| key != "type" && key != "ttl") {
        return Err("cache_control: unknown field".to_owned());
    }
    if let Some(ttl) = control.get("ttl")
        && ttl != "5m"
        && ttl != "1h"
    {
        return Err("cache_control.ttl: 5m or 1h".to_owned());
    }
    if fields.get("type") == Some(&json!("text")) && fields.get("text").and_then(Value::as_str) == Some("") {
        return Err("cache_control cannot be set for empty text blocks".to_owned());
    }
    Ok(true)
}

/// Takes the breakpoints out of a message's content parts: where they
/// were, as part indexes.
fn unmark_parts(kind: ProviderKind, message: &mut Value) -> Result<Vec<usize>, String> {
    let mut marked = Vec::new();
    if let Some(Value::Array(parts)) = message.get_mut("content") {
        for (index, part) in parts.iter_mut().enumerate() {
            if unmark(kind, part)? {
                marked.push(index);
            }
        }
    }
    if unmark(kind, message)? {
        return Err("cache_control: on a content block, not a message".to_owned());
    }
    Ok(marked)
}

fn part(value: &Value) -> String {
    value.to_string()
}

impl Prompt {
    /// The prompt of a turn's body to `kind`.
    pub fn read(kind: ProviderKind, body: &Value) -> Result<Self, String> {
        let model = body.get("model").and_then(Value::as_str).unwrap_or_default();
        let mut prompt = Self {
            tools: Vec::new(),
            system: Vec::new(),
            messages: Vec::new(),
            markers: Vec::new(),
            parts: Vec::new(),
        };
        for (index, tool) in body
            .get("tools")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .enumerate()
        {
            let mut tool = tool.clone();
            let marked = unmark(kind, &mut tool)?;
            if marked {
                if kind != ProviderKind::Anthropic {
                    return Err("cache_control: not on a tool here".to_owned());
                }
                prompt.markers.push(format!("tools[{index}]"));
            }
            prompt.parts.push((format!("tool:{}", part(&tool)), marked));
            prompt.tools.push(tool);
        }
        match kind {
            ProviderKind::Anthropic => prompt.read_messages(kind, body)?,
            ProviderKind::OpenAi | ProviderKind::OpenRouter => prompt.read_chat(kind, body)?,
        }
        if prompt.markers.len() > MAX_BREAKPOINTS {
            return Err(format!(
                "A maximum of {MAX_BREAKPOINTS} blocks with cache_control may be provided. Found {}.",
                prompt.markers.len()
            ));
        }
        if !at_breakpoints(kind, model) {
            // Breakpoints are passed over by a model that caches on its own.
            for (_, marked) in &mut prompt.parts {
                *marked = false;
            }
        }
        Ok(prompt)
    }

    fn read_messages(&mut self, kind: ProviderKind, body: &Value) -> Result<(), String> {
        match body.get("system") {
            None => {}
            Some(Value::String(text)) => {
                self.system.push(json!(text));
                self.parts.push((format!("system:{}", part(&json!(text))), false));
            }
            Some(Value::Array(blocks)) => {
                for (index, block) in blocks.iter().enumerate() {
                    let mut block = block.clone();
                    let marked = unmark(kind, &mut block)?;
                    if marked {
                        self.markers.push(format!("system[{index}]"));
                    }
                    self.parts.push((format!("system:{}", part(&block)), marked));
                    self.system.push(block);
                }
            }
            Some(_) => return Err("system: a string or text blocks".to_owned()),
        }
        for (index, message) in body
            .get("messages")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .enumerate()
        {
            let mut message = message.clone();
            let role = message
                .get("role")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_owned();
            let marked = unmark_parts(kind, &mut message)?;
            match message.get("content") {
                Some(Value::Array(blocks)) => {
                    for (block_index, block) in blocks.iter().enumerate() {
                        let marked = marked.contains(&block_index);
                        if marked {
                            self.markers.push(format!("messages[{index}].content[{block_index}]"));
                        }
                        self.parts.push((format!("{index}:{role}:{}", part(block)), marked));
                    }
                }
                content => {
                    let text = content.cloned().unwrap_or(Value::Null);
                    self.parts.push((format!("{index}:{role}:{}", part(&text)), false));
                }
            }
            self.messages.push(message);
        }
        Ok(())
    }

    fn read_chat(&mut self, kind: ProviderKind, body: &Value) -> Result<(), String> {
        let model = body.get("model").and_then(Value::as_str).unwrap_or_default();
        for message in body.get("messages").and_then(Value::as_array).into_iter().flatten() {
            let mut message = message.clone();
            // Claude, behind OpenRouter, refuses an empty text block.
            let empty = message
                .get("content")
                .and_then(Value::as_array)
                .is_some_and(|parts| parts.iter().any(|part| part.get("text") == Some(&json!(""))));
            if empty && at_breakpoints(kind, model) {
                return Err("text content blocks must be non-empty".to_owned());
            }
            let marked = unmark_parts(kind, &mut message)?;
            let system = matches!(
                message.get("role").and_then(Value::as_str),
                Some("system" | "developer")
            );
            if system {
                if !self.messages.is_empty() {
                    return Err("messages: the system message comes first".to_owned());
                }
                let index = self.system.len();
                self.markers.extend(marked.iter().map(|_| format!("system[{index}]")));
                self.parts
                    .push((format!("system:{}", part(&message)), !marked.is_empty()));
                self.system.push(message);
            } else {
                let index = self.messages.len();
                self.markers
                    .extend(marked.iter().map(|part| format!("messages[{index}].content[{part}]")));
                match message.get("content") {
                    // Each content part is a part of its own, the first
                    // with the message's other fields.
                    Some(Value::Array(content)) if !content.is_empty() => {
                        let mut others = message.as_object().cloned().unwrap_or_default();
                        others.remove("content");
                        for (part_index, content_part) in content.iter().enumerate() {
                            let key = if part_index == 0 {
                                format!("{index}:{}:0:{}", part(&Value::Object(others.clone())), part(content_part))
                            } else {
                                format!("{index}:{part_index}:{}", part(content_part))
                            };
                            self.parts.push((key, marked.contains(&part_index)));
                        }
                    }
                    _ => self
                        .parts
                        .push((format!("{index}:{}", part(&message)), !marked.is_empty())),
                }
                self.messages.push(message);
            }
        }
        Ok(())
    }

    /// Whether `self` continues `previous` exactly: the same tools and
    /// system, and its messages starting with all of `previous`'s. Compared
    /// as structure, breakpoints left out: sorted keys put `tools` after
    /// `messages` in the bytes.
    pub fn continues(&self, previous: &Self) -> Result<(), String> {
        if self.tools != previous.tools {
            return Err(format!(
                "the tools changed: {} then {}",
                Value::Array(previous.tools.clone()),
                Value::Array(self.tools.clone())
            ));
        }
        if self.system != previous.system {
            return Err(format!(
                "the system changed: {} then {}",
                Value::Array(previous.system.clone()),
                Value::Array(self.system.clone())
            ));
        }
        if self.messages.len() < previous.messages.len() {
            return Err(format!(
                "{} messages, fewer than the {} before",
                self.messages.len(),
                previous.messages.len()
            ));
        }
        for (index, (before, now)) in previous.messages.iter().zip(&self.messages).enumerate() {
            if before != now && !extends(before, now) {
                return Err(format!("messages[{index}] changed: {before} then {now}"));
            }
        }
        Ok(())
    }
}

/// Whether `now` is `before` with blocks added after its own: the last
/// message of one request gaining blocks in the next (Anthropic's tool
/// results joining a user message).
fn extends(before: &Value, now: &Value) -> bool {
    let (Some(before_blocks), Some(now_blocks)) = (
        before.get("content").and_then(Value::as_array),
        now.get("content").and_then(Value::as_array),
    ) else {
        return false;
    };
    let others = |message: &Value| -> Map<String, Value> {
        let mut fields = message.as_object().cloned().unwrap_or_default();
        fields.remove("content");
        fields
    };
    others(before) == others(now) && now_blocks.starts_with(before_blocks)
}

/// Whether `next` continues `previous`, both turns' bodies to `kind`.
pub fn check_prefix(kind: ProviderKind, previous: &Value, next: &Value) -> Result<(), String> {
    Prompt::read(kind, next)?.continues(&Prompt::read(kind, previous)?)
}

/// Where a turn's body to `kind` marks its breakpoints.
pub fn markers(kind: ProviderKind, body: &Value) -> Result<Vec<String>, String> {
    Ok(Prompt::read(kind, body)?.markers)
}

/// Tokens a part counts for: one per four bytes.
fn tokens(part: &str) -> u64 {
    (part.len() as u64).div_ceil(4)
}

fn tokens_of(parts: &[(String, bool)]) -> u64 {
    parts.iter().map(|(part, _)| tokens(part)).sum()
}

/// What a prompt cost, as its provider counts it.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct CacheUse {
    /// Every token read.
    pub input: u64,
    /// Those of them read from the cache.
    pub read: u64,
    /// Those of them written to it.
    pub written: u64,
}

/// Where a cached prompt is kept.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Scope {
    kind: ProviderKind,
    model: String,
    key: Option<String>,
}

/// The providers' prompt caches: what each kept, by scope.
#[derive(Debug, Default)]
pub struct Caches {
    kept: Vec<(Scope, Vec<String>)>,
}

impl Caches {
    /// Answers `prompt` from the cache, and keeps what it writes.
    pub fn answer(&mut self, kind: ProviderKind, body: &Value, prompt: &Prompt) -> CacheUse {
        let model = body.get("model").and_then(Value::as_str).unwrap_or_default().to_owned();
        let key = match kind {
            ProviderKind::Anthropic => None,
            ProviderKind::OpenAi | ProviderKind::OpenRouter => {
                body.get("prompt_cache_key").and_then(Value::as_str).map(str::to_owned)
            }
        };
        let breakpoints = at_breakpoints(kind, &model);
        let scope = Scope {
            kind,
            model,
            // Claude through OpenRouter caches at breakpoints, whatever the key.
            key: if breakpoints { None } else { key },
        };
        let parts: Vec<String> = prompt.parts.iter().map(|(part, _)| part.clone()).collect();
        let input = tokens_of(&prompt.parts);
        let kept: Vec<&Vec<String>> = self
            .kept
            .iter()
            .filter(|(kept, _)| *kept == scope)
            .map(|(_, parts)| parts)
            .collect();
        if !breakpoints {
            let shared = kept
                .iter()
                .map(|kept| kept.iter().zip(&parts).take_while(|(a, b)| a == b).count())
                .max()
                .unwrap_or(0);
            let read = tokens_of(&prompt.parts[..shared]);
            if !kept.iter().any(|kept| **kept == parts) {
                self.kept.push((scope, parts));
            }
            return CacheUse {
                input,
                read,
                written: 0,
            };
        }
        let ends: Vec<usize> = prompt
            .parts
            .iter()
            .enumerate()
            .filter(|(_, (_, marked))| *marked)
            .map(|(index, _)| index + 1)
            .collect();
        let Some(&last) = ends.last() else {
            return CacheUse {
                input,
                read: 0,
                written: 0,
            };
        };
        let found = ends
            .iter()
            .filter_map(|&end| {
                (end.saturating_sub(LOOKBACK).max(1)..=end)
                    .rev()
                    .find(|&length| kept.iter().any(|kept| kept.as_slice() == &parts[..length]))
            })
            .max()
            .unwrap_or(0);
        let read = tokens_of(&prompt.parts[..found]);
        let written = tokens_of(&prompt.parts[..last]).saturating_sub(read);
        for end in ends {
            let prefix = parts[..end].to_vec();
            if !self.kept.iter().any(|(kept, parts)| *kept == scope && *parts == prefix) {
                self.kept.push((scope.clone(), prefix));
            }
        }
        CacheUse { input, read, written }
    }
}
