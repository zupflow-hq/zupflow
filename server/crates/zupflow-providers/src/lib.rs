//! The model provider adapters (decision 0049): plain HTTP to each provider's
//! own API, through `OutboundHttp` alone, with no vendor SDK.
//!
//! - [`HttpModelProvider`] answers a run's turns: OpenAI's and OpenRouter's
//!   chat completions ([`chat`]), Anthropic's Messages ([`messages`]).
//! - [`HttpProviderCatalog`] checks a key and lists what a provider serves
//!   ([`catalog`]).
//!
//! Every request names its provider's fixed origin ([`Endpoint`]); the key
//! is the request's credential, set by the outbound path after every other
//! header and scrubbed from what comes back, and it is scrubbed again here
//! before anything is read. What a provider answers is classified by its
//! status: `401` refuses the key; `402` is an account out of credits; `403`
//! refuses the request (not the key); `404` the model; `408`, `409`, `425`,
//! `429` and `5xx` (and no answer) are worth trying again, after the
//! provider's `Retry-After` if it gave one; a timeout is tried again once at
//! most, since the provider may have billed it; anything else fails the
//! turn. The one body read on a refusal is OpenRouter's `400` for a model it
//! doesn't know, matched narrowly. A provider's own words are never passed
//! on or logged.
//!
//! Model calls run under [`MODEL_CALL_LIMITS`]; listing, which a person
//! waits on, under the tighter [`LISTING_LIMITS`].

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use zeroize::Zeroizing;
use zupflow_core::model_providers::{CatalogError, ModelProviderCatalog, ProviderKey, ProviderKind, ProviderModel};
use zupflow_core::models::{
    Completion, MAX_RETRY_AFTER, ModelProvider, ProviderAccess, ProviderError, Turn, validate_turn,
};
use zupflow_core::outbound::{
    Credential, Limits, Method, OutboundError, OutboundHttp, OutboundRequest, OutboundResponse, Refusal, scrub,
};

pub mod catalog;
pub mod chat;
#[cfg(any(test, feature = "fake"))]
pub mod fake;
pub mod messages;

/// Recorded with failures.
pub const NAME: &str = "providers";

/// How long one model turn may take, and how much it may answer.
pub const MODEL_CALL_LIMITS: Limits = Limits {
    timeout: Duration::from_secs(120),
    max_response_bytes: 4 << 20,
};

/// How long checking a key or listing models may take, and how much a list
/// may hold: a person is waiting on it.
pub const LISTING_LIMITS: Limits = Limits {
    timeout: Duration::from_secs(20),
    max_response_bytes: 4 << 20,
};

/// The most tokens a turn may write, sent with every turn: Anthropic
/// requires a cap, and an explicit one bounds what a turn can cost. A model
/// known to write less isn't listed as calling tools ([`catalog`]), since
/// a provider refuses a cap past the model's own; a turn that needs more
/// fails as cut off.
pub const MAX_OUTPUT_TOKENS: u64 = 16_000;

/// The wire protocol a kind speaks.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Protocol {
    /// OpenAI's chat completions, which OpenRouter speaks too.
    ChatCompletions,
    /// Anthropic's Messages.
    AnthropicMessages,
}

/// Where a kind of provider is reached, and how. Adding a kind is one entry
/// here, plus an adapter if it speaks a new protocol.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Endpoint {
    pub kind: ProviderKind,
    pub protocol: Protocol,
    /// Where a turn is sent.
    pub turn_url: &'static str,
    /// Where models are listed.
    pub models_url: &'static str,
    /// Where a key is checked, when listing models doesn't check it.
    pub key_url: Option<&'static str>,
}

/// The registry: every kind's endpoint.
pub fn endpoint(kind: ProviderKind) -> Endpoint {
    match kind {
        ProviderKind::OpenAi => Endpoint {
            kind,
            protocol: Protocol::ChatCompletions,
            turn_url: "https://api.openai.com/v1/chat/completions",
            models_url: "https://api.openai.com/v1/models",
            key_url: None,
        },
        ProviderKind::Anthropic => Endpoint {
            kind,
            protocol: Protocol::AnthropicMessages,
            turn_url: "https://api.anthropic.com/v1/messages",
            models_url: "https://api.anthropic.com/v1/models",
            key_url: None,
        },
        // OpenRouter lists its models to anyone, so the key is checked first.
        ProviderKind::OpenRouter => Endpoint {
            kind,
            protocol: Protocol::ChatCompletions,
            turn_url: "https://openrouter.ai/api/v1/chat/completions",
            models_url: "https://openrouter.ai/api/v1/models",
            key_url: Some("https://openrouter.ai/api/v1/key"),
        },
    }
}

/// The `anthropic-version` every Anthropic request names.
pub const ANTHROPIC_VERSION: &str = "2023-06-01";

/// The key as the kind's credential: `Authorization: Bearer …`, or
/// Anthropic's `x-api-key`.
fn credential(kind: ProviderKind, key: &ProviderKey) -> Credential {
    match endpoint(kind).protocol {
        Protocol::ChatCompletions => Credential {
            header: "Authorization".to_owned(),
            value: Zeroizing::new(format!("Bearer {}", key.expose())),
        },
        Protocol::AnthropicMessages => Credential {
            header: "x-api-key".to_owned(),
            value: Zeroizing::new(key.expose().to_owned()),
        },
    }
}

/// The headers every request to the kind carries, besides its credential.
fn headers(kind: ProviderKind, json_body: bool) -> Vec<(String, String)> {
    let mut headers = vec![("Accept".to_owned(), "application/json".to_owned())];
    if json_body {
        headers.push(("Content-Type".to_owned(), "application/json".to_owned()));
    }
    if endpoint(kind).protocol == Protocol::AnthropicMessages {
        headers.push(("anthropic-version".to_owned(), ANTHROPIC_VERSION.to_owned()));
    }
    headers
}

/// What a provider's status says, before its body is read.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Status {
    /// `2xx`: an answer to read.
    Answered,
    /// `401`: the key is refused.
    KeyRefused,
    /// `402`: the account behind the key can't pay.
    OutOfCredits,
    /// `403`: the key is known, and this request refused with it (a
    /// moderation flag, a region, a permission). Says nothing of the key.
    Forbidden,
    /// `404`: no such model (or route).
    NotFound,
    /// `408`, `409`, `425`, `429`, `5xx`: try again later.
    Retry,
    /// Anything else: the request was refused, and repeating it won't help.
    Refused,
}

/// Classifies a status, never a body.
pub fn classify(status: u16) -> Status {
    match status {
        200..=299 => Status::Answered,
        401 => Status::KeyRefused,
        402 => Status::OutOfCredits,
        403 => Status::Forbidden,
        404 => Status::NotFound,
        408 | 409 | 425 | 429 | 500..=599 => Status::Retry,
        _ => Status::Refused,
    }
}

/// How long the provider asked to wait, from `retry-after-ms` or a
/// `retry-after` in seconds (a date is ignored), at most
/// [`MAX_RETRY_AFTER`].
pub fn retry_after(headers: &[(String, String)]) -> Option<Duration> {
    let value = |name: &str| {
        headers
            .iter()
            .find(|(header, _)| header.eq_ignore_ascii_case(name))
            .and_then(|(_, value)| value.trim().parse::<f64>().ok())
            .filter(|value| value.is_finite() && *value >= 0.0)
    };
    let wait = value("retry-after-ms")
        .map(|ms| ms / 1000.0)
        .or_else(|| value("retry-after"))?;
    Some(Duration::from_secs_f64(wait.min(MAX_RETRY_AFTER.as_secs_f64())))
}

/// Sends `request` with the key as its credential, and takes the key out
/// of whatever comes back again, whatever the transport did.
async fn send(
    outbound: &dyn OutboundHttp,
    kind: ProviderKind,
    key: &ProviderKey,
    method: Method,
    url: String,
    body: Option<Vec<u8>>,
    limits: Limits,
) -> Result<OutboundResponse, OutboundError> {
    let request = OutboundRequest {
        method,
        url,
        headers: headers(kind, body.is_some()),
        body,
        credential: Some(credential(kind, key)),
    };
    let mut response = outbound.send(request, limits).await?;
    scrub(&mut response.body, key.expose().as_bytes());
    for (_, value) in &mut response.headers {
        if value.contains(key.expose()) {
            *value = value.replace(key.expose(), "[redacted]");
        }
    }
    Ok(response)
}

/// Why a turn wasn't sent or no answer came: what the run engine can do about it.
fn turn_failure(error: OutboundError) -> ProviderError {
    match error {
        // It may have been sent and worked on: tried again at most once.
        OutboundError::TimedOut => ProviderError::TimedOut,
        OutboundError::Unreachable | OutboundError::Failed | OutboundError::Refused(Refusal::Unresolvable) => {
            ProviderError::Unavailable
        }
        OutboundError::TooLarge => ProviderError::Failed("an answer larger than allowed"),
        OutboundError::Redirected => ProviderError::Failed("an answer that redirects"),
        OutboundError::Refused(_) => ProviderError::Failed("a request the outbound path refused"),
    }
}

/// Answers runs' turns from the provider each turn's connection names.
pub struct HttpModelProvider {
    outbound: Arc<dyn OutboundHttp>,
}

impl HttpModelProvider {
    pub fn new(outbound: Arc<dyn OutboundHttp>) -> Self {
        Self { outbound }
    }
}

#[async_trait]
impl ModelProvider for HttpModelProvider {
    fn name(&self) -> &'static str {
        NAME
    }

    async fn complete(&self, access: &ProviderAccess<'_>, turn: &Turn<'_>) -> Result<Completion, ProviderError> {
        validate_turn(turn).map_err(ProviderError::InvalidTurn)?;
        let target = endpoint(access.kind);
        let body = match target.protocol {
            Protocol::ChatCompletions => chat::request(access.kind, turn),
            Protocol::AnthropicMessages => messages::request(turn),
        };
        let body = serde_json::to_vec(&body).map_err(|_| ProviderError::Failed("a request that can't be written"))?;
        let response = send(
            self.outbound.as_ref(),
            access.kind,
            access.key,
            Method::Post,
            target.turn_url.to_owned(),
            Some(body),
            MODEL_CALL_LIMITS,
        )
        .await
        .map_err(turn_failure)?;
        match classify(response.status) {
            Status::Answered => {}
            Status::KeyRefused => return Err(ProviderError::KeyRefused),
            Status::OutOfCredits => return Err(ProviderError::OutOfCredits),
            Status::Forbidden => {
                tracing::warn!(event = "providers.turn_forbidden", kind = access.kind.as_str());
                return Err(ProviderError::Forbidden);
            }
            Status::NotFound => return Err(ProviderError::UnknownModel),
            Status::Retry => {
                return Err(
                    retry_after(&response.headers).map_or(ProviderError::Unavailable, ProviderError::RetryAfter)
                );
            }
            // OpenRouter answers an unknown or withdrawn model `400`.
            Status::Refused
                if access.kind == ProviderKind::OpenRouter && chat::names_an_unknown_model(&response.body) =>
            {
                return Err(ProviderError::UnknownModel);
            }
            Status::Refused => {
                tracing::warn!(
                    event = "providers.turn_refused",
                    kind = access.kind.as_str(),
                    status = response.status
                );
                return Err(ProviderError::Failed("the provider refused the turn"));
            }
        }
        let completion = match target.protocol {
            Protocol::ChatCompletions => chat::completion(&response.body),
            Protocol::AnthropicMessages => messages::completion(&response.body),
        };
        if let Err(error) = &completion {
            tracing::warn!(event = "providers.answer_unusable", kind = access.kind.as_str(), %error);
        }
        completion
    }
}

/// Checks keys and lists models by asking each provider.
pub struct HttpProviderCatalog {
    outbound: Arc<dyn OutboundHttp>,
}

impl HttpProviderCatalog {
    pub fn new(outbound: Arc<dyn OutboundHttp>) -> Self {
        Self { outbound }
    }

    /// One `GET` of a listing: its body, or why not.
    async fn get(&self, kind: ProviderKind, key: &ProviderKey, url: String) -> Result<Vec<u8>, CatalogError> {
        let response = send(
            self.outbound.as_ref(),
            kind,
            key,
            Method::Get,
            url,
            None,
            LISTING_LIMITS,
        )
        .await
        .map_err(|_| CatalogError::Unavailable)?;
        match classify(response.status) {
            Status::Answered => Ok(response.body),
            Status::KeyRefused => Err(CatalogError::KeyRefused),
            Status::Forbidden => Err(CatalogError::Forbidden),
            status => {
                tracing::warn!(
                    event = "providers.listing_failed",
                    kind = kind.as_str(),
                    status = response.status,
                    ?status
                );
                Err(CatalogError::Unavailable)
            }
        }
    }
}

#[async_trait]
impl ModelProviderCatalog for HttpProviderCatalog {
    fn name(&self) -> &'static str {
        NAME
    }

    async fn list_models(&self, kind: ProviderKind, key: &ProviderKey) -> Result<Vec<ProviderModel>, CatalogError> {
        let target = endpoint(kind);
        if let Some(key_url) = target.key_url {
            self.get(kind, key, key_url.to_owned()).await?;
        }
        let unreadable = |_| {
            tracing::warn!(event = "providers.listing_unreadable", kind = kind.as_str());
            CatalogError::Unavailable
        };
        match kind {
            ProviderKind::OpenAi => {
                let body = self.get(kind, key, target.models_url.to_owned()).await?;
                catalog::openai(&body).map_err(unreadable)
            }
            ProviderKind::OpenRouter => {
                let body = self.get(kind, key, target.models_url.to_owned()).await?;
                catalog::openrouter(&body).map_err(unreadable)
            }
            ProviderKind::Anthropic => {
                let mut models = Vec::new();
                let mut after: Option<String> = None;
                for _ in 0..catalog::MAX_PAGES {
                    let url = catalog::anthropic_page_url(target.models_url, after.as_deref());
                    let body = self.get(kind, key, url).await?;
                    let page = catalog::anthropic(&body).map_err(unreadable)?;
                    models.extend(page.models);
                    match page.next {
                        Some(next) => after = Some(next),
                        None => return Ok(models),
                    }
                }
                // More pages than any provider lists: what was read stands.
                Ok(models)
            }
        }
    }
}

#[cfg(test)]
mod tests;
