//! [`FakeProviders`]: OpenAI's, Anthropic's and OpenRouter's HTTP APIs,
//! faked in process for tests, behind the `OutboundHttp` seam the adapters
//! and the MCP tests already go through. No socket is opened and no real
//! provider is ever reached.
//!
//! It reads each request as the provider would, from its own reading of
//! the wire format (never the adapters' code), and is strict where the
//! providers are: the credential in the kind's own header, Anthropic's
//! `anthropic-version`, `max_tokens` and alternating roles with tool
//! results first, OpenAI's tool messages answering the calls before them,
//! an output cap under each kind's own name (`max_completion_tokens` to
//! OpenAI, `max_tokens` to OpenRouter and Anthropic), and exactly one
//! model named (never `models`, a `route` or `openrouter/auto`). A request
//! it can't take is `400`.
//!
//! It answers as the scripted model would (`call <tool> {…}` lines become
//! calls, results come back as text), in the kind's own answer format, for
//! every model of the scripted providers' catalogs that calls tools; and it
//! lists those catalogs as each provider lists models (Anthropic's in pages
//! of three, to exercise paging), with output limits as the providers list
//! them. A key containing `invalid`, or one [`FakeProviders::revoke`]d, is
//! `401`; a model [`FakeProviders::remove_model`]d is `404` (OpenRouter's
//! `400` naming it not a valid model ID) and no longer listed; queued
//! [`Fault`]s answer the next turns, and [`FakeProviders::before_next_turn`]
//! runs something while a turn is in flight. With [`FakeProviders::echo_key`] on,
//! it repeats the key it received in every answer and error, as a hostile
//! or careless provider might, and it never scrubs anything: whatever
//! leaves the adapters must already be clean.
//!
//! Every request is kept in its ledger ([`Received`]), its body's bytes as
//! sent, with where the key was found.
//!
//! Each kind's prompt cache is faked too ([`prompt`]): breakpoints are
//! checked where the provider checks them, and every answer's usage counts
//! a token per four bytes of the prompt, with what was read from the cache
//! (the part shared with an earlier request) and written to it, in the
//! kind's own fields: Anthropic's `cache_read_input_tokens` and
//! `cache_creation_input_tokens` beside an `input_tokens` that leaves them
//! out, chat's `prompt_tokens_details.cached_tokens` (and, from OpenRouter,
//! `cache_write_tokens`) within `prompt_tokens`.

use std::collections::{HashSet, VecDeque};
use std::future::Future;
use std::pin::Pin;
use std::sync::Mutex;

use async_trait::async_trait;
use serde_json::{Map, Value, json};
use zupflow_basic::scripted_model::ScriptedModel;
use zupflow_basic::scripted_providers::{ScriptedProviders, catalog};
use zupflow_core::model_providers::{ProviderKey, ProviderKind};
use zupflow_core::models::{
    Completion, Message, ModelProvider, ModelToolCall, ProviderAccess, ProviderError, Reply, ToolSpec, Turn,
};
use zupflow_core::outbound::{Limits, Method, OutboundError, OutboundHttp, OutboundRequest, OutboundResponse};

use crate::{ANTHROPIC_VERSION, endpoint};

pub mod prompt;

use prompt::{CacheUse, Caches, Prompt};

/// How many models a page of Anthropic's list holds here.
pub const ANTHROPIC_PAGE: usize = 3;

/// What the next turn gets instead of an answer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Fault {
    /// This status, with an error body.
    Status(u16),
    /// This status, with a `retry-after` of so many seconds.
    RetryAfter { status: u16, secs: u32 },
    /// No answer in time.
    TimedOut,
    /// No connection.
    Unreachable,
}

/// One request as it arrived.
#[derive(Debug, Clone, PartialEq)]
pub struct Received {
    /// The kind whose API it reached, if any.
    pub kind: Option<ProviderKind>,
    pub method: Method,
    pub url: String,
    pub headers: Vec<(String, String)>,
    /// The credential's header and value.
    pub credential: Option<(String, String)>,
    /// The key found anywhere but the credential: the URL, another header,
    /// the body.
    pub key_elsewhere: bool,
    pub limits: Limits,
    /// The JSON body, or `Null`.
    pub body: Value,
    /// The body's bytes, exactly as sent.
    pub raw: Vec<u8>,
    /// The status answered, or `None` for a fault without one.
    pub status: Option<u16>,
}

impl Received {
    /// Whether this was a turn sent to a model.
    pub fn is_turn(&self) -> bool {
        self.method == Method::Post
    }
}

#[derive(Default)]
struct State {
    revoked: HashSet<String>,
    removed: HashSet<(ProviderKind, String)>,
    faults: VecDeque<Fault>,
    echo_key: bool,
    ledger: Vec<Received>,
    before_next_turn: Option<Pin<Box<dyn Future<Output = ()> + Send>>>,
    /// Headers the answer being made carries besides its content type.
    headers: Vec<(String, String)>,
    caches: Caches,
}

pub struct FakeProviders {
    model: ScriptedModel,
    state: Mutex<State>,
}

impl Default for FakeProviders {
    fn default() -> Self {
        Self::new()
    }
}

impl FakeProviders {
    pub fn new() -> Self {
        Self {
            model: ScriptedModel::serving(ScriptedProviders::tool_models()),
            state: Mutex::new(State::default()),
        }
    }

    fn state(&self) -> std::sync::MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// From now on, `key` is refused, as a provider refuses a revoked key.
    pub fn revoke(&self, key: &str) {
        self.state().revoked.insert(key.to_owned());
    }

    /// From now on, `kind` no longer serves or lists `model`.
    pub fn remove_model(&self, kind: ProviderKind, model: &str) {
        self.state().removed.insert((kind, model.to_owned()));
    }

    /// From now on, `kind` serves and lists `model` again.
    pub fn restore_model(&self, kind: ProviderKind, model: &str) {
        self.state().removed.remove(&(kind, model.to_owned()));
    }

    /// The next turns, one fault each, get these instead of an answer.
    pub fn fail_next(&self, faults: &[Fault]) {
        self.state().faults.extend(faults.iter().copied());
    }

    /// Runs `work` when the next turn arrives, before it's answered: what
    /// happens while a turn is in flight.
    pub fn before_next_turn(&self, work: impl Future<Output = ()> + Send + 'static) {
        self.state().before_next_turn = Some(Box::pin(work));
    }

    /// From now on, every answer and error repeats the key received.
    pub fn echo_key(&self) {
        self.state().echo_key = true;
    }

    /// Every request received, in order.
    pub fn ledger(&self) -> Vec<Received> {
        self.state().ledger.clone()
    }

    /// The turns received, in order.
    pub fn turns(&self) -> Vec<Received> {
        self.ledger().into_iter().filter(Received::is_turn).collect()
    }

    fn accepts(&self, key: &str) -> bool {
        ScriptedProviders::accepts(&ProviderKey::new(key.to_owned())) && !self.state().revoked.contains(key)
    }
}

/// The kind an URL reaches, and whether it's a turn (or else a listing).
fn route(url: &str) -> Option<(ProviderKind, Route)> {
    let path_of = |url: &str| url.split('?').next().unwrap_or(url).to_owned();
    let path = path_of(url);
    ProviderKind::ALL.into_iter().find_map(|kind| {
        let target = endpoint(kind);
        if path == target.turn_url {
            Some((kind, Route::Turn))
        } else if path == target.models_url {
            Some((kind, Route::Models))
        } else if target.key_url == Some(path.as_str()) {
            Some((kind, Route::Key))
        } else {
            None
        }
    })
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Route {
    Turn,
    Models,
    Key,
}

fn header<'a>(headers: &'a [(String, String)], name: &str) -> Option<&'a str> {
    headers
        .iter()
        .find(|(header, _)| header.eq_ignore_ascii_case(name))
        .map(|(_, value)| value.as_str())
}

/// The key a request carries the way `kind` takes it, if it does.
fn key_of(kind: ProviderKind, credential: Option<&(String, String)>) -> Option<String> {
    let (header, value) = credential?;
    match kind {
        ProviderKind::Anthropic => header.eq_ignore_ascii_case("x-api-key").then(|| value.clone()),
        ProviderKind::OpenAi | ProviderKind::OpenRouter => header
            .eq_ignore_ascii_case("authorization")
            .then(|| value.strip_prefix("Bearer ").map(str::to_owned))
            .flatten(),
    }
}

/// An answer: status and JSON body.
type Answer = (u16, Value);

fn error(kind: ProviderKind, status: u16, message: &str) -> Answer {
    let body = match kind {
        ProviderKind::Anthropic => {
            let kind = match status {
                401 => "authentication_error",
                404 => "not_found_error",
                429 => "rate_limit_error",
                500..=599 => "api_error",
                _ => "invalid_request_error",
            };
            json!({ "type": "error", "error": { "type": kind, "message": message } })
        }
        ProviderKind::OpenAi | ProviderKind::OpenRouter => {
            json!({ "error": { "message": message, "type": "invalid_request_error", "code": status } })
        }
    };
    (status, body)
}

#[async_trait]
impl OutboundHttp for FakeProviders {
    async fn send(&self, request: OutboundRequest, limits: Limits) -> Result<OutboundResponse, OutboundError> {
        let credential = request
            .credential
            .as_ref()
            .map(|credential| (credential.header.clone(), credential.value.to_string()));
        let routed = route(&request.url);
        let key = routed.and_then(|(kind, _)| key_of(kind, credential.as_ref()));
        let body: Value = request
            .body
            .as_deref()
            .and_then(|body| serde_json::from_slice(body).ok())
            .unwrap_or(Value::Null);
        let key_elsewhere = key.as_deref().is_some_and(|key| {
            request.url.contains(key)
                || request
                    .headers
                    .iter()
                    .any(|(name, value)| name.contains(key) || value.contains(key))
                || request
                    .body
                    .as_deref()
                    .is_some_and(|body| String::from_utf8_lossy(body).contains(key))
        });
        let mut received = Received {
            kind: routed.map(|(kind, _)| kind),
            method: request.method,
            url: request.url.clone(),
            headers: request.headers.clone(),
            credential: credential.clone(),
            key_elsewhere,
            limits,
            body: body.clone(),
            raw: request.body.as_deref().unwrap_or_default().to_vec(),
            status: None,
        };
        let answer = self.answer(&request, routed, key.as_deref(), &body).await;
        let (status, mut body) = match answer {
            Ok(answer) => answer,
            Err(error) => {
                self.state().ledger.push(received);
                return Err(error);
            }
        };
        received.status = Some(status);
        let echo = self.state().echo_key;
        if echo && let Some(key) = &key {
            echo_into(&mut body, key);
        }
        let mut headers = vec![("content-type".to_owned(), "application/json".to_owned())];
        {
            let mut state = self.state();
            state.ledger.push(received);
            headers.append(&mut state.headers);
        }
        Ok(OutboundResponse {
            status,
            headers,
            body: body.to_string().into_bytes(),
        })
    }
}

/// Puts `key` into every string of an answer, and beside it.
fn echo_into(body: &mut Value, key: &str) {
    match body {
        Value::String(text) => text.push_str(&format!(" (you sent {key})")),
        Value::Array(items) => items.iter_mut().for_each(|item| echo_into(item, key)),
        Value::Object(fields) => {
            for (name, value) in fields.iter_mut() {
                // What a client matches on or reads as a value stays as it is.
                if ![
                    "id",
                    "type",
                    "role",
                    "name",
                    "arguments",
                    "input",
                    "finish_reason",
                    "stop_reason",
                    "tool_use_id",
                    "last_id",
                    "first_id",
                    "object",
                    "pricing",
                    "supported_parameters",
                    "architecture",
                    "created_at",
                ]
                .contains(&name.as_str())
                {
                    echo_into(value, key);
                }
            }
            fields.insert("received_key".to_owned(), json!(key));
        }
        _ => {}
    }
}

impl FakeProviders {
    async fn answer(
        &self,
        request: &OutboundRequest,
        routed: Option<(ProviderKind, Route)>,
        key: Option<&str>,
        body: &Value,
    ) -> Result<Answer, OutboundError> {
        let Some((kind, route)) = routed else {
            return Ok((404, json!({ "error": "no such API" })));
        };
        // What happens while the turn is in flight, before it's read.
        if route == Route::Turn {
            let work = self.state().before_next_turn.take();
            if let Some(work) = work {
                work.await;
            }
        }
        let Some(key) = key.filter(|key| self.accepts(key)) else {
            return Ok(error(kind, 401, "Invalid API key."));
        };
        if kind == ProviderKind::Anthropic && header(&request.headers, "anthropic-version") != Some(ANTHROPIC_VERSION) {
            return Ok(error(kind, 400, "anthropic-version: header is required"));
        }
        match (route, request.method) {
            (Route::Key, Method::Get) => Ok((
                200,
                json!({ "data": { "label": "sk-or-…", "limit": null, "usage": 0 } }),
            )),
            (Route::Models, Method::Get) => Ok(self.models(kind, &request.url)),
            (Route::Turn, Method::Post) => {
                if header(&request.headers, "content-type") != Some("application/json") {
                    return Ok(error(kind, 400, "Send JSON."));
                }
                let fault = self.state().faults.pop_front();
                if let Some(fault) = fault {
                    return match fault {
                        Fault::Status(status) => Ok(error(kind, status, "Something went wrong upstream.")),
                        Fault::RetryAfter { status, secs } => {
                            self.state().headers.push(("retry-after".to_owned(), secs.to_string()));
                            Ok(error(kind, status, "Slow down."))
                        }
                        Fault::TimedOut => Err(OutboundError::TimedOut),
                        Fault::Unreachable => Err(OutboundError::Unreachable),
                    };
                }
                Ok(self.turn(kind, key, body).await)
            }
            _ => Ok(error(kind, 405, "Method not allowed.")),
        }
    }

    fn listed(&self, kind: ProviderKind) -> Vec<zupflow_core::model_providers::ProviderModel> {
        let removed = self.state().removed.clone();
        catalog(kind)
            .into_iter()
            .filter(|model| !removed.contains(&(kind, model.provider_model_id.clone())))
            .collect()
    }

    fn models(&self, kind: ProviderKind, url: &str) -> Answer {
        let models = self.listed(kind);
        match kind {
            ProviderKind::OpenAi => (
                200,
                json!({
                    "object": "list",
                    "data": models.iter().rev().map(|model| json!({
                        "id": model.provider_model_id, "object": "model", "created": 1_700_000_000, "owned_by": "system",
                    })).collect::<Vec<_>>(),
                }),
            ),
            ProviderKind::Anthropic => {
                let after = url
                    .split_once("after_id=")
                    .map(|(_, after)| after.split('&').next().unwrap_or(after).to_owned());
                let start = match after {
                    Some(after) => match models.iter().position(|model| model.provider_model_id == after) {
                        Some(position) => position + 1,
                        None => return error(kind, 400, "after_id: unknown"),
                    },
                    None => 0,
                };
                let page: Vec<_> = models.iter().skip(start).take(ANTHROPIC_PAGE).collect();
                let has_more = start + page.len() < models.len();
                (
                    200,
                    json!({
                        "data": page.iter().map(|model| json!({
                            "type": "model",
                            "id": model.provider_model_id,
                            "display_name": model.name,
                            "created_at": "2025-01-01T00:00:00Z",
                            "max_input_tokens": model.context_window,
                            "max_tokens": max_output(&model.provider_model_id),
                        })).collect::<Vec<_>>(),
                        "has_more": has_more,
                        "first_id": page.first().map(|model| model.provider_model_id.clone()),
                        "last_id": page.last().map(|model| model.provider_model_id.clone()),
                    }),
                )
            }
            ProviderKind::OpenRouter => (
                200,
                json!({
                    "data": models.iter().map(|model| {
                        let pricing = model.pricing.map_or_else(
                            || json!({ "prompt": "-1", "completion": "-1" }),
                            |pricing| json!({
                                "prompt": (pricing.input_per_million_usd / 1_000_000.0).to_string(),
                                "completion": (pricing.output_per_million_usd / 1_000_000.0).to_string(),
                                "request": "0",
                            }),
                        );
                        let parameters = if model.supports_tools {
                            json!(["max_tokens", "temperature", "tools", "tool_choice"])
                        } else {
                            json!(["max_tokens", "temperature"])
                        };
                        json!({
                            "id": model.provider_model_id,
                            "canonical_slug": model.provider_model_id,
                            "name": format!("{}: {}", model.author, model.name),
                            "created": 1_700_000_000,
                            "description": "A model.",
                            "context_length": model.context_window,
                            "architecture": { "input_modalities": ["text"], "output_modalities": ["text"] },
                            "pricing": pricing,
                            "top_provider": {
                                "context_length": model.context_window,
                                "max_completion_tokens": max_output(&model.provider_model_id),
                                "is_moderated": false,
                            },
                            "supported_parameters": parameters,
                        })
                    }).collect::<Vec<_>>(),
                }),
            ),
        }
    }

    async fn turn(&self, kind: ProviderKind, key: &str, body: &Value) -> Answer {
        let model = body.get("model").and_then(Value::as_str).unwrap_or_default().to_owned();
        let read = match kind {
            ProviderKind::Anthropic => read_messages(body),
            ProviderKind::OpenAi | ProviderKind::OpenRouter => read_chat(kind, body),
        };
        let (instructions, messages, tools) = match read {
            Ok(read) => read,
            Err(why) => return error(kind, 400, why),
        };
        let prompt = match Prompt::read(kind, body) {
            Ok(prompt) => prompt,
            Err(why) => return error(kind, 400, &why),
        };
        if self.state().removed.contains(&(kind, model.clone())) {
            return match kind {
                ProviderKind::OpenRouter => error(kind, 400, &format!("{model} is not a valid model ID")),
                ProviderKind::OpenAi | ProviderKind::Anthropic => {
                    error(kind, 404, "The model does not exist or you do not have access to it.")
                }
            };
        }
        let key = ProviderKey::new(key.to_owned());
        let turn = Turn {
            model: &model,
            instructions: &instructions,
            messages: &messages,
            tools: &tools,
            cache_key: None,
        };
        let completion = match self.model.complete(&ProviderAccess { kind, key: &key }, &turn).await {
            Ok(completion) => completion,
            Err(ProviderError::UnknownModel) => return error(kind, 404, "No such model."),
            Err(ProviderError::KeyRefused) => return error(kind, 401, "Invalid API key."),
            Err(_) => return error(kind, 400, "That conversation can't be continued."),
        };
        let cached = self.state().caches.answer(kind, body, &prompt);
        (200, write_answer(kind, &model, &completion, cached))
    }
}

/// The most a model writes, as its provider lists it: Claude Haiku 3.5's
/// 8,192, and plenty for the rest.
pub fn max_output(model: &str) -> u64 {
    if model.contains("claude-3-5-haiku") || model.contains("claude-3.5-haiku") {
        8_192
    } else {
        64_000
    }
}

/// A chat message's text: a string, or `text` parts, never none.
fn text_content(content: Option<&Value>) -> Option<String> {
    match content? {
        Value::String(text) => Some(text.clone()),
        Value::Array(parts) if !parts.is_empty() => parts
            .iter()
            .map(|part| {
                (part.get("type").and_then(Value::as_str) == Some("text"))
                    .then(|| part.get("text").and_then(Value::as_str))
                    .flatten()
            })
            .collect(),
        _ => None,
    }
}

/// What Messages takes at the top of a request; anything else is refused.
const MESSAGES_FIELDS: &[&str] = &[
    "model",
    "max_tokens",
    "system",
    "messages",
    "tools",
    "tool_choice",
    "metadata",
    "stop_sequences",
    "temperature",
    "top_p",
    "top_k",
    "thinking",
    "stream",
    "service_tier",
    "cache_control",
];

/// A chat completions request, as OpenAI reads it.
fn read_chat(kind: ProviderKind, body: &Value) -> Result<(String, Vec<Message>, Vec<ToolSpec>), &'static str> {
    if body.get("model").and_then(Value::as_str).is_none_or(str::is_empty) {
        return Err("model: required");
    }
    if body.get("models").is_some() || body.get("route").is_some() || body["model"] == json!("openrouter/auto") {
        return Err("model: exactly one model, never a list or a router");
    }
    // Each kind's own output cap: OpenAI's reasoning models refuse `max_tokens`.
    let (cap, other) = match kind {
        ProviderKind::OpenRouter => ("max_tokens", "max_completion_tokens"),
        ProviderKind::OpenAi | ProviderKind::Anthropic => ("max_completion_tokens", "max_tokens"),
    };
    if body.get(other).is_some() {
        return Err("max_tokens: use max_completion_tokens with this model");
    }
    if body.get(cap).and_then(Value::as_u64).is_none_or(|max| max == 0) {
        return Err("an output cap: required here");
    }
    if let Some(key) = body.get("prompt_cache_key")
        && key.as_str().is_none_or(|key| key.is_empty() || key.len() > 64)
    {
        return Err("prompt_cache_key: a string of at most 64 characters");
    }
    let mut instructions = String::new();
    let mut messages = Vec::new();
    let mut open: Vec<String> = Vec::new();
    for message in body
        .get("messages")
        .and_then(Value::as_array)
        .ok_or("messages: required")?
    {
        let content = message.get("content");
        match message.get("role").and_then(Value::as_str) {
            Some("system" | "developer") => {
                instructions.push_str(&text_content(content).ok_or("system: content")?);
            }
            Some("user") => {
                if !open.is_empty() {
                    return Err("messages: tool calls must be answered by tool messages");
                }
                messages.push(Message::User {
                    text: text_content(content).ok_or("user: content")?,
                });
            }
            Some("assistant") => {
                if !open.is_empty() {
                    return Err("messages: tool calls must be answered by tool messages");
                }
                let text = match content {
                    None | Some(Value::Null) => None,
                    Some(Value::String(text)) => Some(text.clone()),
                    Some(_) => return Err("assistant: content"),
                };
                let mut tool_calls = Vec::new();
                if let Some(calls) = message.get("tool_calls") {
                    let calls = calls
                        .as_array()
                        .filter(|calls| !calls.is_empty())
                        .ok_or("tool_calls: empty")?;
                    for call in calls {
                        if call.get("type").and_then(Value::as_str) != Some("function") {
                            return Err("tool_calls: type");
                        }
                        let id = call.get("id").and_then(Value::as_str).ok_or("tool_calls: id")?;
                        let name = call
                            .pointer("/function/name")
                            .and_then(Value::as_str)
                            .ok_or("tool_calls: name")?;
                        let arguments = call
                            .pointer("/function/arguments")
                            .and_then(Value::as_str)
                            .ok_or("tool_calls: arguments must be a string")?;
                        let Ok(Value::Object(arguments)) = serde_json::from_str::<Value>(arguments) else {
                            return Err("tool_calls: arguments must be JSON");
                        };
                        open.push(id.to_owned());
                        tool_calls.push(ModelToolCall {
                            id: id.to_owned(),
                            name: name.to_owned(),
                            arguments,
                        });
                    }
                }
                messages.push(Message::Assistant { text, tool_calls });
            }
            Some("tool") => {
                let id = message
                    .get("tool_call_id")
                    .and_then(Value::as_str)
                    .ok_or("tool: tool_call_id")?;
                let Some(position) = open.iter().position(|open| open == id) else {
                    return Err("tool: answers no tool call before it");
                };
                open.remove(position);
                let content = text_content(content).ok_or("tool: content")?;
                let (content, is_error) = match content.strip_prefix(crate::chat::ERROR_PREFIX) {
                    Some(reason) => (reason.to_owned(), true),
                    None => (content.to_owned(), false),
                };
                messages.push(Message::ToolResult {
                    call_id: id.to_owned(),
                    content,
                    is_error,
                });
            }
            _ => return Err("messages: role"),
        }
    }
    let mut tools = Vec::new();
    for tool in body.get("tools").and_then(Value::as_array).into_iter().flatten() {
        if tool.get("type").and_then(Value::as_str) != Some("function") {
            return Err("tools: type");
        }
        tools.push(ToolSpec {
            name: tool
                .pointer("/function/name")
                .and_then(Value::as_str)
                .ok_or("tools: name")?
                .to_owned(),
            description: tool
                .pointer("/function/description")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_owned(),
            input_schema: tool
                .pointer("/function/parameters")
                .cloned()
                .ok_or("tools: parameters")?,
        });
    }
    Ok((instructions, messages, tools))
}

/// A Messages request, as Anthropic reads it.
fn read_messages(body: &Value) -> Result<(String, Vec<Message>, Vec<ToolSpec>), &'static str> {
    if body.get("model").and_then(Value::as_str).is_none_or(str::is_empty) {
        return Err("model: required");
    }
    if body
        .get("max_tokens")
        .and_then(Value::as_u64)
        .is_none_or(|max| max == 0)
    {
        return Err("max_tokens: required");
    }
    if let Value::Object(fields) = body
        && fields.keys().any(|field| !MESSAGES_FIELDS.contains(&field.as_str()))
    {
        return Err("Extra inputs are not permitted");
    }
    let instructions = match body.get("system") {
        None => String::new(),
        Some(Value::String(system)) => system.clone(),
        Some(Value::Array(blocks)) if !blocks.is_empty() => {
            let mut system = String::new();
            for block in blocks {
                if block.get("type").and_then(Value::as_str) != Some("text") {
                    return Err("system: text blocks");
                }
                match block.get("text").and_then(Value::as_str) {
                    Some(text) if !text.is_empty() => system.push_str(text),
                    _ => return Err("text content blocks must be non-empty"),
                }
            }
            system
        }
        Some(_) => return Err("system: a string or text blocks"),
    };
    let mut messages = Vec::new();
    let mut expected = "user";
    // The calls of the assistant message before, which the next user message answers.
    let mut open: Vec<String> = Vec::new();
    for message in body
        .get("messages")
        .and_then(Value::as_array)
        .ok_or("messages: required")?
    {
        let role = message.get("role").and_then(Value::as_str).ok_or("messages: role")?;
        if role != expected {
            return Err("messages: roles must alternate, starting with user");
        }
        expected = if role == "user" { "assistant" } else { "user" };
        let blocks = match message.get("content") {
            Some(Value::String(text)) => vec![json!({ "type": "text", "text": text })],
            Some(Value::Array(blocks)) if !blocks.is_empty() => blocks.clone(),
            _ => return Err("messages: content"),
        };
        if role == "user" {
            let mut seen_text = false;
            for block in &blocks {
                match block.get("type").and_then(Value::as_str) {
                    Some("text") => {
                        let text = block.get("text").and_then(Value::as_str).ok_or("text: text")?;
                        if text.is_empty() {
                            return Err("text content blocks must be non-empty");
                        }
                        seen_text = true;
                        messages.push(Message::User { text: text.to_owned() });
                    }
                    Some("tool_result") => {
                        if seen_text {
                            return Err("tool_result blocks must come first");
                        }
                        let id = block
                            .get("tool_use_id")
                            .and_then(Value::as_str)
                            .ok_or("tool_result: id")?;
                        let Some(position) = open.iter().position(|open| open == id) else {
                            return Err("tool_result: answers no tool_use in the message before");
                        };
                        open.remove(position);
                        let content = match block.get("content") {
                            None => String::new(),
                            Some(Value::String(text)) => text.clone(),
                            Some(Value::Array(parts)) => parts
                                .iter()
                                .filter_map(|part| part.get("text").and_then(Value::as_str))
                                .collect(),
                            Some(_) => return Err("tool_result: content"),
                        };
                        messages.push(Message::ToolResult {
                            call_id: id.to_owned(),
                            content,
                            is_error: block.get("is_error") == Some(&json!(true)),
                        });
                    }
                    _ => return Err("user: content block type"),
                }
            }
            if !open.is_empty() {
                return Err("tool_use ids were found without tool_result blocks immediately after");
            }
        } else {
            let mut text: Option<String> = None;
            let mut tool_calls = Vec::new();
            for block in &blocks {
                match block.get("type").and_then(Value::as_str) {
                    Some("text") => {
                        let part = block.get("text").and_then(Value::as_str).ok_or("text: text")?;
                        if part.is_empty() {
                            return Err("text content blocks must be non-empty");
                        }
                        text.get_or_insert_with(String::new).push_str(part);
                    }
                    Some("tool_use") => {
                        let id = block.get("id").and_then(Value::as_str).ok_or("tool_use: id")?;
                        if !id.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-') {
                            return Err("tool_use: id pattern");
                        }
                        let Some(Value::Object(input)) = block.get("input") else {
                            return Err("tool_use: input");
                        };
                        open.push(id.to_owned());
                        tool_calls.push(ModelToolCall {
                            id: id.to_owned(),
                            name: block
                                .get("name")
                                .and_then(Value::as_str)
                                .ok_or("tool_use: name")?
                                .to_owned(),
                            arguments: input.clone(),
                        });
                    }
                    _ => return Err("assistant: content block type"),
                }
            }
            messages.push(Message::Assistant { text, tool_calls });
        }
    }
    let mut tools = Vec::new();
    for tool in body.get("tools").and_then(Value::as_array).into_iter().flatten() {
        let schema = tool.get("input_schema").cloned().ok_or("tools: input_schema")?;
        if schema.get("type") != Some(&json!("object")) {
            return Err("tools: input_schema.type must be object");
        }
        tools.push(ToolSpec {
            name: tool
                .get("name")
                .and_then(Value::as_str)
                .ok_or("tools: name")?
                .to_owned(),
            description: tool
                .get("description")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_owned(),
            input_schema: schema,
        });
    }
    Ok((instructions, messages, tools))
}

fn write_answer(kind: ProviderKind, model: &str, completion: &Completion, cached: CacheUse) -> Value {
    let (text, calls) = match &completion.reply {
        Reply::Text(text) => (Some(text.clone()), Vec::new()),
        Reply::ToolCalls { text, calls } => (text.clone(), calls.clone()),
    };
    let usage = completion.usage;
    match kind {
        ProviderKind::Anthropic => {
            let mut content = Vec::new();
            if let Some(text) = &text {
                content.push(json!({ "type": "text", "text": text }));
            }
            for call in &calls {
                content.push(json!({
                    "type": "tool_use", "id": call.id, "name": call.name, "input": Value::Object(call.arguments.clone()),
                }));
            }
            json!({
                "id": "msg_fake",
                "type": "message",
                "role": "assistant",
                "model": model,
                "content": content,
                "stop_reason": if calls.is_empty() { "end_turn" } else { "tool_use" },
                "stop_sequence": null,
                "usage": {
                    "input_tokens": cached.input - cached.read - cached.written,
                    "output_tokens": usage.output_tokens,
                    "cache_creation_input_tokens": cached.written,
                    "cache_read_input_tokens": cached.read,
                },
            })
        }
        ProviderKind::OpenAi | ProviderKind::OpenRouter => {
            let details = match kind {
                ProviderKind::OpenRouter => {
                    json!({ "cached_tokens": cached.read, "cache_write_tokens": cached.written })
                }
                ProviderKind::OpenAi | ProviderKind::Anthropic => json!({ "cached_tokens": cached.read }),
            };
            let mut message = Map::new();
            message.insert("role".to_owned(), json!("assistant"));
            message.insert("content".to_owned(), text.map_or(Value::Null, Value::String));
            if !calls.is_empty() {
                message.insert(
                    "tool_calls".to_owned(),
                    Value::Array(
                        calls
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
                            .collect(),
                    ),
                );
            }
            json!({
                "id": "chatcmpl-fake",
                "object": "chat.completion",
                "created": 1_700_000_000,
                "model": model,
                "choices": [{
                    "index": 0,
                    "message": message,
                    "finish_reason": if calls.is_empty() { "stop" } else { "tool_calls" },
                }],
                "usage": {
                    "prompt_tokens": cached.input,
                    "completion_tokens": usage.output_tokens,
                    "total_tokens": cached.input + usage.output_tokens,
                    "prompt_tokens_details": details,
                },
            })
        }
    }
}
