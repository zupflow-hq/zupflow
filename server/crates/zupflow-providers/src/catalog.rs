//! Each provider's model list, as [`ProviderModel`]s. A model is listed as
//! calling tools only if it can through the adapters as they are: it calls
//! tools, and it may write [`MAX_OUTPUT_TOKENS`], the cap every turn is sent
//! with (a provider refuses a cap past the model's own).
//!
//! - **OpenAI** (`GET /v1/models`) names only IDs. Names are made from them
//!   (`gpt-5-mini` is "GPT-5 mini"), the author is `openai`, and neither
//!   context sizes nor prices are given. Whether a model can call tools
//!   through chat completions isn't listed either, so it is judged from its
//!   ID ([`openai_calls_tools`]): the `gpt-` and `o<n>` families, less the
//!   models that serve something else (embeddings, images, audio, speech,
//!   moderation, search, completions) or only another API (`-pro`, codex,
//!   deep research, o1's previews), and less those that write fewer tokens
//!   than the cap (`gpt-3.5`, `gpt-4` and its `gpt-4-` versions, the first
//!   `gpt-4o` snapshot). A fine-tuned model (`ft:<base>:…`) is judged by its
//!   base. Sorted by ID, since the list's own order means nothing.
//! - **Anthropic** (`GET /v1/models`, paged with `after_id`) gives each
//!   model's display name and context window (`max_input_tokens`). Every
//!   Claude model calls tools; one whose `max_tokens` is listed below the
//!   cap, or, where it isn't listed, a Claude 3 model before 3.7 (or older),
//!   is listed as not calling them. Its own order, newest first.
//! - **OpenRouter** (`GET /api/v1/models`) gives names (`Author: Name`, the
//!   author dropped), context length, prices per token as decimal strings
//!   (shown per million; a negative or unreadable price is no price) and
//!   `supported_parameters`, where `tools` says it calls tools, less a
//!   model whose `top_provider.max_completion_tokens` is below the cap, and
//!   less Google's Gemini models: through OpenRouter they expect their
//!   `reasoning_details` sent back with each tool call, which a run's
//!   conversation doesn't keep. The author is the ID's first segment. Its
//!   own order.

use serde_json::Value;
use zupflow_core::model_providers::{ModelPricing, ProviderModel};

use crate::MAX_OUTPUT_TOKENS;

/// The most pages of Anthropic's list read.
pub const MAX_PAGES: usize = 20;

/// How many models one page of Anthropic's list asks for: its most.
pub const ANTHROPIC_PAGE: u32 = 1000;

/// A list that can't be read as the provider's.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Unreadable;

fn data(body: &[u8]) -> Result<(Value, Vec<Value>), Unreadable> {
    let list: Value = serde_json::from_slice(body).map_err(|_| Unreadable)?;
    let data = list.get("data").and_then(Value::as_array).ok_or(Unreadable)?.clone();
    Ok((list, data))
}

fn id_of(entry: &Value) -> Option<String> {
    entry
        .get("id")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|id| !id.is_empty())
        .map(str::to_owned)
}

/// Whether an OpenAI model ID is one that calls tools through chat completions.
pub fn openai_calls_tools(id: &str) -> bool {
    let id = id.to_ascii_lowercase();
    // `ft:gpt-4.1-mini-2025-04-14:org::abc123` is judged as its base.
    let id = match id.strip_prefix("ft:") {
        Some(rest) => rest.split(':').next().unwrap_or_default().to_owned(),
        None => id,
    };
    // Models that write fewer tokens than every turn is capped at.
    let writes_less = id.starts_with("gpt-3") || id == "gpt-4" || id.starts_with("gpt-4-") || id == "gpt-4o-2024-05-13";
    if writes_less {
        return false;
    }
    let reasoning = id
        .strip_prefix('o')
        .is_some_and(|rest| rest.starts_with(|c: char| c.is_ascii_digit()));
    let family = id.starts_with("gpt-") || id.starts_with("chatgpt-") || reasoning;
    const ELSEWHERE: [&str; 15] = [
        "embedding",
        "image",
        "audio",
        "realtime",
        "transcribe",
        "tts",
        "search",
        "moderation",
        "instruct",
        "codex",
        "deep-research",
        "-pro",
        "o1-mini",
        "o1-preview",
        "chatgpt-4o",
    ];
    family && !ELSEWHERE.iter().any(|word| id.contains(word))
}

/// A name for an OpenAI model ID: `gpt` as `GPT`, joined to its version
/// (`GPT-4.1`), and the rest as words (`GPT-4.1 mini`).
pub fn openai_name(id: &str) -> String {
    let mut parts = id.split('-');
    let Some(first) = parts.next() else {
        return id.to_owned();
    };
    if !first.eq_ignore_ascii_case("gpt") {
        return id.to_owned();
    }
    let rest: Vec<&str> = parts.collect();
    match rest.split_first() {
        Some((version, words)) if version.starts_with(|c: char| c.is_ascii_digit()) => {
            let mut name = format!("GPT-{version}");
            for word in words {
                name.push(' ');
                name.push_str(word);
            }
            name
        }
        _ => id.to_owned(),
    }
}

pub fn openai(body: &[u8]) -> Result<Vec<ProviderModel>, Unreadable> {
    let (_, data) = data(body)?;
    let mut models: Vec<ProviderModel> = data
        .iter()
        .filter_map(id_of)
        .map(|id| ProviderModel {
            name: openai_name(&id),
            author: "openai".to_owned(),
            context_window: None,
            supports_tools: openai_calls_tools(&id),
            pricing: None,
            provider_model_id: id,
        })
        .collect();
    models.sort_by(|a, b| a.provider_model_id.cmp(&b.provider_model_id));
    models.dedup_by(|a, b| a.provider_model_id == b.provider_model_id);
    Ok(models)
}

/// One page of Anthropic's list, and the ID to read the next one after.
pub struct Page {
    pub models: Vec<ProviderModel>,
    pub next: Option<String>,
}

/// The URL of the page after `after` (the first page without it).
pub fn anthropic_page_url(models_url: &str, after: Option<&str>) -> String {
    match after {
        // An ID is letters, digits, `-`, `.` and `_`; anything else is escaped.
        Some(after) => {
            let escaped: String = after
                .bytes()
                .map(|byte| match byte {
                    b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' => (byte as char).to_string(),
                    _ => format!("%{byte:02X}"),
                })
                .collect();
            format!("{models_url}?limit={ANTHROPIC_PAGE}&after_id={escaped}")
        }
        None => format!("{models_url}?limit={ANTHROPIC_PAGE}"),
    }
}

/// Whether a Claude model may write [`MAX_OUTPUT_TOKENS`]: as its listed
/// `max_tokens` says, or else by its ID (the Claude 3 models before 3.7,
/// and older ones, write at most 8,192).
pub fn anthropic_writes_enough(id: &str, max_tokens: Option<u64>) -> bool {
    match max_tokens {
        Some(max_tokens) => max_tokens >= MAX_OUTPUT_TOKENS,
        None => {
            let id = id.to_ascii_lowercase();
            let claude_3 = id.starts_with("claude-3-") && !id.starts_with("claude-3-7");
            !(claude_3 || id.starts_with("claude-2") || id.starts_with("claude-instant"))
        }
    }
}

pub fn anthropic(body: &[u8]) -> Result<Page, Unreadable> {
    let (list, data) = data(body)?;
    let models: Vec<ProviderModel> = data
        .iter()
        .filter_map(|entry| {
            let id = id_of(entry)?;
            let name = entry
                .get("display_name")
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|name| !name.is_empty())
                .map_or_else(|| id.clone(), str::to_owned);
            Some(ProviderModel {
                name,
                author: "anthropic".to_owned(),
                context_window: entry.get("max_input_tokens").and_then(Value::as_u64),
                supports_tools: anthropic_writes_enough(&id, entry.get("max_tokens").and_then(Value::as_u64)),
                pricing: None,
                provider_model_id: id,
            })
        })
        .collect();
    let next = if list.get("has_more").and_then(Value::as_bool) == Some(true) {
        // A page that says there's more but names no last ID is the last read.
        list.get("last_id")
            .and_then(Value::as_str)
            .filter(|last| !last.is_empty())
            .map(str::to_owned)
    } else {
        None
    };
    Ok(Page { models, next })
}

/// A price per token, as a decimal string, per million tokens.
fn per_million(value: Option<&Value>) -> Option<f64> {
    let per_token = match value? {
        Value::String(text) => text.trim().parse::<f64>().ok()?,
        Value::Number(number) => number.as_f64()?,
        _ => return None,
    };
    let per_million = per_token * 1_000_000.0;
    // Shown to the cent's hundredth of a millionth, without float noise.
    (per_million.is_finite() && per_million >= 0.0).then(|| (per_million * 1e6).round() / 1e6)
}

/// Whether a model, through OpenRouter, expects its `reasoning_details`
/// sent back with its tool calls: Google's Gemini models.
pub fn openrouter_needs_reasoning_back(id: &str) -> bool {
    id.to_ascii_lowercase().starts_with("google/gemini-")
}

pub fn openrouter(body: &[u8]) -> Result<Vec<ProviderModel>, Unreadable> {
    let (_, data) = data(body)?;
    let mut models: Vec<ProviderModel> = Vec::with_capacity(data.len());
    for entry in &data {
        let Some(id) = id_of(entry) else {
            continue;
        };
        if models.iter().any(|model| model.provider_model_id == id) {
            continue;
        }
        let listed = entry.get("name").and_then(Value::as_str).unwrap_or_default().trim();
        let name = match listed.split_once(": ") {
            Some((_, name)) if !name.trim().is_empty() => name.trim(),
            _ => listed,
        };
        let name = if name.is_empty() { id.clone() } else { name.to_owned() };
        let author = id.split_once('/').map_or("openrouter", |(author, _)| author).to_owned();
        let pricing = entry.get("pricing");
        let pricing = match (
            per_million(pricing.and_then(|pricing| pricing.get("prompt"))),
            per_million(pricing.and_then(|pricing| pricing.get("completion"))),
        ) {
            (Some(input), Some(output)) => Some(ModelPricing {
                input_per_million_usd: input,
                output_per_million_usd: output,
            }),
            _ => None,
        };
        let calls_tools = entry
            .get("supported_parameters")
            .and_then(Value::as_array)
            .is_some_and(|parameters| parameters.iter().any(|parameter| parameter == "tools"));
        let writes_enough = entry
            .pointer("/top_provider/max_completion_tokens")
            .and_then(Value::as_u64)
            .is_none_or(|max| max >= MAX_OUTPUT_TOKENS);
        let supports_tools = calls_tools && writes_enough && !openrouter_needs_reasoning_back(&id);
        models.push(ProviderModel {
            name,
            author,
            context_window: entry.get("context_length").and_then(Value::as_u64),
            supports_tools,
            pricing,
            provider_model_id: id,
        });
    }
    Ok(models)
}
