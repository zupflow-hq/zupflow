#![allow(clippy::unwrap_used)]

use std::sync::Arc;
use std::time::Duration;

use serde_json::{Value, json};
use zupflow_basic::scripted_providers::catalog;
use zupflow_core::model_providers::{CatalogError, ModelProviderCatalog, ProviderKey, ProviderKind};
use zupflow_core::models::{
    Message, ModelProvider, ModelToolCall, ProviderAccess, ProviderError, Reply, ToolSpec, Turn,
};
use zupflow_core::outbound::Method;

use super::*;
use crate::fake::{FakeProviders, Fault};

const KEY: &str = "sk-test-0123456789abcdefWXYZ";
const REFUSED: &str = "sk-test-INVALID-0123456789";

fn key() -> ProviderKey {
    ProviderKey::new(KEY.to_owned())
}

/// A model each kind serves that calls tools.
fn model(kind: ProviderKind) -> &'static str {
    match kind {
        ProviderKind::OpenAi => "gpt-5-mini",
        ProviderKind::Anthropic => "claude-sonnet-4-5",
        ProviderKind::OpenRouter => "openai/gpt-5-mini",
    }
}

fn fake() -> (Arc<FakeProviders>, HttpModelProvider, HttpProviderCatalog) {
    let fake = Arc::new(FakeProviders::new());
    (
        fake.clone(),
        HttpModelProvider::new(fake.clone()),
        HttpProviderCatalog::new(fake),
    )
}

fn tools() -> Vec<ToolSpec> {
    vec![ToolSpec {
        name: "work_create_task".to_owned(),
        description: "Creates a task.".to_owned(),
        input_schema: json!({ "type": "object", "properties": { "title": { "type": "string" } } }),
    }]
}

fn call(id: &str) -> ModelToolCall {
    ModelToolCall {
        id: id.to_owned(),
        name: "work_create_task".to_owned(),
        arguments: json!({ "title": "Ship" }).as_object().unwrap().clone(),
    }
}

/// A conversation one turn in: asked, one call made, its result back.
fn answered() -> Vec<Message> {
    vec![
        Message::User {
            text: "call work_create_task {\"title\": \"Ship\"}".to_owned(),
        },
        Message::Assistant {
            text: Some("On it.".to_owned()),
            tool_calls: vec![call("call_a"), call("call_b")],
        },
        Message::ToolResult {
            call_id: "call_a".to_owned(),
            content: "Created.".to_owned(),
            is_error: false,
        },
        Message::ToolResult {
            call_id: "call_b".to_owned(),
            content: "A person denied this call.".to_owned(),
            is_error: true,
        },
    ]
}

async fn ask(
    provider: &HttpModelProvider,
    kind: ProviderKind,
    messages: &[Message],
) -> Result<Completion, ProviderError> {
    let tools = tools();
    provider
        .complete(
            &ProviderAccess { kind, key: &key() },
            &Turn {
                model: model(kind),
                instructions: "Be brief.",
                messages,
                tools: &tools,
            },
        )
        .await
}

#[tokio::test]
async fn passes_the_catalog_suite_for_every_kind() {
    let (_, _, catalog) = fake();
    zupflow_conformance::model_provider_catalog::run(&catalog, &key(), &ProviderKey::new(REFUSED.to_owned())).await;
}

#[tokio::test]
async fn passes_the_model_provider_suite_for_every_kind() {
    let (_, provider, _) = fake();
    let refused = ProviderKey::new(REFUSED.to_owned());
    for kind in ProviderKind::ALL {
        let key = key();
        zupflow_conformance::model_provider::run(&provider, &ProviderAccess { kind, key: &key }, &refused, model(kind))
            .await;
    }
}

#[tokio::test]
async fn lists_each_providers_models_as_it_describes_them() {
    let (fake, _, catalog) = fake();
    // Anthropic and OpenRouter say everything the catalog holds, except
    // that a model writing less than a turn's cap (Claude Haiku 3.5, listed
    // at 8,192) and, through OpenRouter, Gemini aren't offered for tools.
    for kind in [ProviderKind::Anthropic, ProviderKind::OpenRouter] {
        assert_eq!(
            catalog.list_models(kind, &key()).await.unwrap(),
            catalog_of(kind),
            "{kind:?}"
        );
    }
    let pages = fake
        .ledger()
        .iter()
        .filter(|received| received.url.starts_with("https://api.anthropic.com/v1/models"))
        .count();
    assert_eq!(pages, 3, "Anthropic's seven models, three a page");

    // OpenAI names IDs only: names come from them, tools from the heuristic.
    let listed = catalog.list_models(ProviderKind::OpenAi, &key()).await.unwrap();
    let mut expected = catalog_of(ProviderKind::OpenAi);
    expected.sort_by(|a, b| a.provider_model_id.cmp(&b.provider_model_id));
    assert_eq!(
        listed
            .iter()
            .map(|model| (model.provider_model_id.as_str(), model.supports_tools))
            .collect::<Vec<_>>(),
        expected
            .iter()
            .map(|model| (model.provider_model_id.as_str(), model.supports_tools))
            .collect::<Vec<_>>()
    );
    let mini = listed
        .iter()
        .find(|model| model.provider_model_id == "gpt-4.1-mini")
        .unwrap();
    assert_eq!((mini.name.as_str(), mini.author.as_str()), ("GPT-4.1 mini", "openai"));
    assert!(
        listed
            .iter()
            .all(|model| model.pricing.is_none() && model.context_window.is_none())
    );

    // Every listing ran under the listing limits, with the key as the credential only.
    for received in fake.ledger() {
        assert_eq!(received.limits, LISTING_LIMITS);
        assert!(!received.key_elsewhere);
        assert_eq!(received.method, Method::Get);
    }
}

/// The scripted catalog, as the adapters list it from the fake's wire.
fn catalog_of(kind: ProviderKind) -> Vec<zupflow_core::model_providers::ProviderModel> {
    let mut models = catalog(kind);
    for model in &mut models {
        let id = model.provider_model_id.as_str();
        if crate::fake::max_output(id) < MAX_OUTPUT_TOKENS
            || (kind == ProviderKind::OpenRouter && catalog::openrouter_needs_reasoning_back(id))
        {
            model.supports_tools = false;
        }
    }
    models
}

#[tokio::test]
async fn checks_an_openrouter_key_before_trusting_its_open_list() {
    let (fake, _, catalog) = fake();
    assert_eq!(
        catalog
            .list_models(ProviderKind::OpenRouter, &ProviderKey::new(REFUSED.to_owned()))
            .await,
        Err(CatalogError::KeyRefused)
    );
    let urls: Vec<String> = fake.ledger().into_iter().map(|received| received.url).collect();
    assert_eq!(urls, ["https://openrouter.ai/api/v1/key"], "refused before listing");
}

#[tokio::test]
async fn sends_chat_completions_as_openai_reads_them() {
    let (fake, provider, _) = fake();
    let answer = ask(&provider, ProviderKind::OpenAi, &answered()).await.unwrap();
    assert_eq!(
        answer.reply,
        Reply::Text("work_create_task: Created.\nwork_create_task failed: A person denied this call.".to_owned())
    );
    assert!(answer.usage.input_tokens > 0 && answer.usage.output_tokens > 0);
    let sent = fake.turns().pop().unwrap();
    assert_eq!(sent.url, "https://api.openai.com/v1/chat/completions");
    assert_eq!(sent.limits, MODEL_CALL_LIMITS);
    assert_eq!(
        sent.credential,
        Some(("Authorization".to_owned(), format!("Bearer {KEY}")))
    );
    assert!(!sent.key_elsewhere);
    let body = sent.body;
    assert_eq!(body["messages"][0], json!({ "role": "system", "content": "Be brief." }));
    assert_eq!(
        body["messages"][2]["tool_calls"][0]["function"]["arguments"],
        json!("{\"title\":\"Ship\"}")
    );
    assert_eq!(
        body["messages"][4],
        json!({ "role": "tool", "tool_call_id": "call_b", "content": "Error: A person denied this call." })
    );
    assert_eq!(body["tools"][0]["function"]["name"], "work_create_task");
    assert_eq!(body["tool_choice"], "auto");
    assert_eq!(body.get("provider"), None, "OpenAI has no provider routing");
}

#[tokio::test]
async fn names_one_model_through_openrouter_and_caps_every_turn() {
    let (fake, provider, _) = fake();
    let asked = [Message::User {
        text: "call work_create_task {\"title\": \"Ship\"}".to_owned(),
    }];
    let answer = ask(&provider, ProviderKind::OpenRouter, &asked).await.unwrap();
    let Reply::ToolCalls { calls, .. } = answer.reply else {
        panic!("a call");
    };
    assert_eq!(calls[0].name, "work_create_task");
    assert_eq!(calls[0].arguments, *json!({ "title": "Ship" }).as_object().unwrap());
    let sent = fake.turns().pop().unwrap();
    assert_eq!(sent.url, "https://openrouter.ai/api/v1/chat/completions");
    assert_eq!(
        sent.body["model"], "openai/gpt-5-mini",
        "the route's own model ID, as is"
    );
    // One model, never a list or a router; any host of it may serve it.
    for field in ["models", "route", "provider"] {
        assert_eq!(sent.body.get(field), None, "{field}");
    }
    assert_eq!(sent.body["max_tokens"], json!(MAX_OUTPUT_TOKENS));
    assert_eq!(sent.body.get("max_completion_tokens"), None);

    // OpenAI takes its cap as `max_completion_tokens`.
    ask(&provider, ProviderKind::OpenAi, &asked).await.unwrap();
    let sent = fake.turns().pop().unwrap();
    assert_eq!(sent.body["max_completion_tokens"], json!(MAX_OUTPUT_TOKENS));
    assert_eq!(sent.body.get("max_tokens"), None);
}

#[tokio::test]
async fn sends_messages_as_anthropic_reads_them() {
    let (fake, provider, _) = fake();
    let mut conversation = answered();
    conversation.push(Message::User {
        text: "Thanks".to_owned(),
    });
    let answer = ask(&provider, ProviderKind::Anthropic, &conversation).await.unwrap();
    assert_eq!(answer.reply, Reply::Text("You said: Thanks".to_owned()));
    let sent = fake.turns().pop().unwrap();
    assert_eq!(sent.url, "https://api.anthropic.com/v1/messages");
    assert_eq!(sent.credential, Some(("x-api-key".to_owned(), KEY.to_owned())));
    assert!(!sent.key_elsewhere);
    assert!(
        sent.headers
            .iter()
            .any(|(name, value)| name == "anthropic-version" && value == ANTHROPIC_VERSION)
    );
    let body = sent.body;
    assert_eq!(body["system"], "Be brief.");
    assert_eq!(body["max_tokens"], json!(crate::MAX_OUTPUT_TOKENS));
    let roles: Vec<&str> = body["messages"]
        .as_array()
        .unwrap()
        .iter()
        .map(|message| message["role"].as_str().unwrap())
        .collect();
    assert_eq!(
        roles,
        ["user", "assistant", "user"],
        "both results and the text after them in one user message"
    );
    assert_eq!(
        body["messages"][2]["content"][2],
        json!({ "type": "text", "text": "Thanks" })
    );
    assert_eq!(
        body["messages"][1]["content"][0],
        json!({ "type": "text", "text": "On it." })
    );
    assert_eq!(
        body["messages"][1]["content"][1],
        json!({ "type": "tool_use", "id": "call_a", "name": "work_create_task", "input": { "title": "Ship" } })
    );
    assert_eq!(
        body["messages"][2]["content"][1],
        json!({ "type": "tool_result", "tool_use_id": "call_b", "content": "A person denied this call.", "is_error": true })
    );
    assert_eq!(body["tools"][0]["input_schema"]["type"], "object");
}

#[tokio::test]
async fn classifies_failures_by_status_alone() {
    let (fake, provider, _) = fake();
    let asked = [Message::User { text: "hi".to_owned() }];
    let cases = [
        (Fault::Status(401), Err(ProviderError::KeyRefused)),
        (Fault::Status(402), Err(ProviderError::OutOfCredits)),
        (Fault::Status(403), Err(ProviderError::Forbidden)),
        (Fault::Status(404), Err(ProviderError::UnknownModel)),
        (Fault::Status(408), Err(ProviderError::Unavailable)),
        (Fault::Status(429), Err(ProviderError::Unavailable)),
        (
            Fault::RetryAfter { status: 429, secs: 30 },
            Err(ProviderError::RetryAfter(Duration::from_secs(30))),
        ),
        (
            Fault::RetryAfter {
                status: 503,
                secs: 3600,
            },
            Err(ProviderError::RetryAfter(MAX_RETRY_AFTER)),
        ),
        (Fault::Status(500), Err(ProviderError::Unavailable)),
        (Fault::Status(503), Err(ProviderError::Unavailable)),
        (Fault::Status(529), Err(ProviderError::Unavailable)),
        (Fault::TimedOut, Err(ProviderError::TimedOut)),
        (Fault::Unreachable, Err(ProviderError::Unavailable)),
        (
            Fault::Status(400),
            Err(ProviderError::Failed("the provider refused the turn")),
        ),
        (
            Fault::Status(422),
            Err(ProviderError::Failed("the provider refused the turn")),
        ),
    ];
    for kind in ProviderKind::ALL {
        for (fault, expected) in &cases {
            fake.fail_next(&[*fault]);
            let got = ask(&provider, kind, &asked).await.map(|_| ());
            assert_eq!(&got, expected, "{kind:?} {fault:?}");
        }
        assert!(ask(&provider, kind, &asked).await.is_ok(), "then answers again");
    }
    fake.remove_model(ProviderKind::Anthropic, "claude-sonnet-4-5");
    assert_eq!(
        ask(&provider, ProviderKind::Anthropic, &asked).await,
        Err(ProviderError::UnknownModel)
    );
    // OpenRouter answers `400` for a model it doesn't have: read as such.
    fake.remove_model(ProviderKind::OpenRouter, "openai/gpt-5-mini");
    assert_eq!(
        ask(&provider, ProviderKind::OpenRouter, &asked).await,
        Err(ProviderError::UnknownModel)
    );
    fake.revoke(KEY);
    assert_eq!(
        ask(&provider, ProviderKind::OpenAi, &asked).await,
        Err(ProviderError::KeyRefused)
    );
}

#[tokio::test]
async fn never_lets_a_key_the_provider_repeats_through() {
    let (fake, provider, catalog) = fake();
    fake.echo_key();
    for kind in ProviderKind::ALL {
        let asked = [Message::User {
            text: "call work_create_task {\"title\": \"Ship\"}".to_owned(),
        }];
        let first = ask(&provider, kind, &asked).await.unwrap();
        let shown = format!("{first:?}");
        assert!(!shown.contains(KEY), "{kind:?}: {shown}");
        let Reply::ToolCalls { text, calls } = first.reply else {
            panic!("a call");
        };
        let mut conversation = asked.to_vec();
        conversation.push(Message::Assistant {
            text,
            tool_calls: calls.clone(),
        });
        conversation.push(Message::ToolResult {
            call_id: calls[0].id.clone(),
            content: "Created.".to_owned(),
            is_error: false,
        });
        let second = ask(&provider, kind, &conversation).await.unwrap();
        let shown = format!("{second:?}");
        assert!(!shown.contains(KEY), "{kind:?}: {shown}");
        assert!(shown.contains("[redacted]"), "it did repeat it: {shown}");
        let models = catalog.list_models(kind, &key()).await.unwrap();
        assert!(!format!("{models:?}").contains(KEY));
        fake.fail_next(&[Fault::Status(500)]);
        assert_eq!(ask(&provider, kind, &asked).await, Err(ProviderError::Unavailable));
    }
    assert!(fake.ledger().iter().all(|received| !received.key_elsewhere));
}

#[test]
fn reads_what_isnt_an_answer_as_a_failure() {
    let chat = |value: Value| chat::completion(value.to_string().as_bytes());
    let choice = |finish: &str| json!({ "choices": [{ "message": { "role": "assistant", "content": "Half an ans" }, "finish_reason": finish }] });
    assert!(matches!(chat(choice("length")), Err(ProviderError::Failed(_))));
    assert!(matches!(chat(choice("content_filter")), Err(ProviderError::Failed(_))));
    assert_eq!(chat(choice("error")), Err(ProviderError::Unavailable));
    assert_eq!(
        chat(json!({ "error": { "message": "upstream", "code": 502 } })),
        Err(ProviderError::Unavailable),
        "OpenRouter's 200 with an error"
    );
    assert!(matches!(
        chat(json!({ "choices": [{ "message": { "content": "" }, "finish_reason": "stop" }] })),
        Err(ProviderError::Failed(_))
    ));
    assert!(matches!(
        chat(
            json!({ "choices": [{ "message": { "tool_calls": [{ "id": "c", "type": "function", "function": { "name": "t", "arguments": "{not json" } }] }, "finish_reason": "tool_calls" }] })
        ),
        Err(ProviderError::Failed(_))
    ));
    let no_arguments = chat(json!({
        "choices": [{ "message": { "content": null, "tool_calls": [{ "id": "c", "type": "function", "function": { "name": "t", "arguments": "" } }] }, "finish_reason": "tool_calls" }],
        "usage": { "prompt_tokens": 7, "completion_tokens": 2 },
    }))
    .unwrap();
    assert_eq!(no_arguments.usage.input_tokens, 7);
    assert!(matches!(no_arguments.reply, Reply::ToolCalls { text: None, .. }));

    let messages = |value: Value| messages::completion(value.to_string().as_bytes());
    let stopped = |reason: &str| json!({ "content": [{ "type": "text", "text": "Hi" }], "stop_reason": reason });
    assert!(matches!(messages(stopped("max_tokens")), Err(ProviderError::Failed(_))));
    assert!(matches!(messages(stopped("refusal")), Err(ProviderError::Failed(_))));
    assert!(matches!(messages(stopped("pause_turn")), Err(ProviderError::Failed(_))));
    let thought = messages(json!({
        "content": [
            { "type": "thinking", "thinking": "", "signature": "abc" },
            { "type": "text", "text": "Let me look." },
            { "type": "tool_use", "id": "toolu_1", "name": "t", "input": {} },
        ],
        "stop_reason": "tool_use",
        "usage": { "input_tokens": 10, "cache_creation_input_tokens": 5, "cache_read_input_tokens": 20, "output_tokens": 3 },
    }))
    .unwrap();
    assert_eq!(thought.usage.input_tokens, 35, "cached tokens were read too");
    let Reply::ToolCalls { text, calls } = thought.reply else {
        panic!("a call");
    };
    assert_eq!((text.as_deref(), calls.len()), (Some("Let me look."), 1));
}

#[test]
fn judges_openai_models_by_their_ids() {
    for id in [
        "gpt-5",
        "gpt-5-mini",
        "gpt-4.1-nano",
        "gpt-4o-2024-08-06",
        "o3",
        "o4-mini",
        "gpt-4o",
        "ft:gpt-4.1-mini-2025-04-14:example-org::abc123",
    ] {
        assert!(catalog::openai_calls_tools(id), "{id}");
    }
    for id in [
        "text-embedding-3-small",
        "gpt-image-1",
        "dall-e-3",
        "whisper-1",
        "tts-1",
        "gpt-4o-mini-tts",
        "gpt-4o-transcribe",
        "gpt-4o-realtime-preview",
        "gpt-4o-audio-preview",
        "gpt-4o-search-preview",
        "omni-moderation-latest",
        "gpt-3.5-turbo-instruct",
        "o1-mini",
        "o3-pro",
        "gpt-5-codex",
        "o3-deep-research",
        "davinci-002",
        "chatgpt-4o-latest",
        // Writing fewer tokens than a turn's cap.
        "gpt-3.5-turbo",
        "gpt-4",
        "gpt-4-turbo",
        "gpt-4-0613",
        "gpt-4o-2024-05-13",
        "ft:gpt-3.5-turbo-0125:example-org::xyz789",
        "ft:davinci-002:example-org::xyz789",
    ] {
        assert!(!catalog::openai_calls_tools(id), "{id}");
    }
    assert_eq!(catalog::openai_name("gpt-5-mini"), "GPT-5 mini");
    assert_eq!(catalog::openai_name("gpt-4o"), "GPT-4o");
    assert_eq!(catalog::openai_name("o4-mini"), "o4-mini");
}

#[test]
fn reads_openrouter_prices_per_million() {
    let listed = catalog::openrouter(
        json!({ "data": [
            { "id": "openai/gpt-5", "name": "OpenAI: GPT-5", "context_length": 400000,
              "pricing": { "prompt": "0.00000125", "completion": "0.00001" }, "supported_parameters": ["tools"] },
            { "id": "openrouter/auto", "name": "Auto Router", "context_length": 2000000,
              "pricing": { "prompt": "-1", "completion": "-1" }, "supported_parameters": [] },
            { "id": "openai/gpt-5", "name": "again" },
            { "name": "no ID" },
        ] })
        .to_string()
        .as_bytes(),
    )
    .unwrap();
    assert_eq!(listed.len(), 2);
    assert_eq!(listed[0].name, "GPT-5");
    assert_eq!(listed[0].author, "openai");
    let pricing = listed[0].pricing.unwrap();
    assert_eq!(
        (pricing.input_per_million_usd, pricing.output_per_million_usd),
        (1.25, 10.0)
    );
    assert!(listed[0].supports_tools);
    assert_eq!(listed[1].pricing, None, "a negative price is none");
    assert!(!listed[1].supports_tools);
    assert_eq!(
        (listed[1].name.as_str(), listed[1].author.as_str()),
        ("Auto Router", "openrouter")
    );
}

#[test]
fn offers_for_tools_only_what_can_take_a_turns_cap() {
    let listed = catalog::openrouter(
        json!({ "data": [
            { "id": "anthropic/claude-3.5-haiku", "name": "Anthropic: Claude 3.5 Haiku",
              "top_provider": { "max_completion_tokens": 8192 }, "supported_parameters": ["tools"] },
            { "id": "anthropic/claude-sonnet-4.5", "name": "Anthropic: Claude Sonnet 4.5",
              "top_provider": { "max_completion_tokens": 64000 }, "supported_parameters": ["tools"] },
            { "id": "openai/gpt-5", "name": "OpenAI: GPT-5",
              "top_provider": { "max_completion_tokens": null }, "supported_parameters": ["tools"] },
            { "id": "google/gemini-2.5-pro", "name": "Google: Gemini 2.5 Pro",
              "top_provider": { "max_completion_tokens": 65536 }, "supported_parameters": ["tools"] },
        ] })
        .to_string()
        .as_bytes(),
    )
    .unwrap();
    assert_eq!(
        listed.iter().map(|model| model.supports_tools).collect::<Vec<_>>(),
        [false, true, true, false]
    );
    // Anthropic: by the listed `max_tokens`, else by the model's ID.
    assert!(!catalog::anthropic_writes_enough("claude-sonnet-4-5", Some(8_192)));
    assert!(catalog::anthropic_writes_enough(
        "claude-3-5-haiku-latest",
        Some(64_000)
    ));
    for id in [
        "claude-sonnet-4-5",
        "claude-opus-4-1",
        "claude-3-7-sonnet-latest",
        "claude-haiku-4-5",
    ] {
        assert!(catalog::anthropic_writes_enough(id, None), "{id}");
    }
    for id in [
        "claude-3-5-haiku-latest",
        "claude-3-5-sonnet-20241022",
        "claude-3-opus-20240229",
        "claude-3-haiku-20240307",
        "claude-2.1",
    ] {
        assert!(!catalog::anthropic_writes_enough(id, None), "{id}");
    }
}

#[test]
fn reads_a_providers_wait_and_openrouters_unknown_model() {
    let headers = |pairs: &[(&str, &str)]| -> Vec<(String, String)> {
        pairs
            .iter()
            .map(|(name, value)| ((*name).to_owned(), (*value).to_owned()))
            .collect()
    };
    assert_eq!(
        retry_after(&headers(&[("Retry-After", "7")])),
        Some(Duration::from_secs(7))
    );
    assert_eq!(
        retry_after(&headers(&[("retry-after-ms", "1500"), ("retry-after", "9")])),
        Some(Duration::from_millis(1500))
    );
    assert_eq!(
        retry_after(&headers(&[("retry-after", "86400")])),
        Some(MAX_RETRY_AFTER)
    );
    assert_eq!(
        retry_after(&headers(&[("retry-after", "Wed, 21 Oct 2026 07:28:00 GMT")])),
        None
    );
    assert_eq!(retry_after(&headers(&[("retry-after", "-3")])), None);
    assert_eq!(retry_after(&[]), None);

    let body = |value: Value| value.to_string().into_bytes();
    assert!(chat::names_an_unknown_model(&body(json!({
        "error": { "code": 400, "message": "acme/nope is not a valid model ID" }
    }))));
    for other in [
        json!({ "error": { "code": 400, "message": "messages: required" } }),
        json!({ "error": "is not a valid model ID" }),
        json!({ "message": "is not a valid model ID" }),
    ] {
        assert!(!chat::names_an_unknown_model(&body(other.clone())), "{other}");
    }
    assert!(!chat::names_an_unknown_model(b"not json"));
}

#[test]
fn classifies_statuses() {
    assert_eq!(classify(200), Status::Answered);
    assert_eq!(classify(401), Status::KeyRefused);
    assert_eq!(classify(402), Status::OutOfCredits);
    assert_eq!(classify(403), Status::Forbidden);
    assert_eq!(classify(404), Status::NotFound);
    for retry in [408, 409, 425, 429, 500, 502, 503, 504, 529] {
        assert_eq!(classify(retry), Status::Retry, "{retry}");
    }
    for refused in [301, 400, 413, 422] {
        assert_eq!(classify(refused), Status::Refused, "{refused}");
    }
}
