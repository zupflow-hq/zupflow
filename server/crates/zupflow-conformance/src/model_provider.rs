//! The `ModelProvider` suite: what every provider answers, whatever model it
//! serves. A turn without tools gets text, never a call; any answer passes
//! `validate_completion`; a conversation continues once every call is
//! answered; tokens are counted; a model it doesn't serve is `UnknownModel`;
//! and a turn that breaks the conversation rules is refused as
//! `InvalidTurn`, never sent on.
//!
//! The suite reaches the provider only through the trait, so a provider
//! adapter runs it against its real service (opt-in, as provider tests are).

use serde_json::json;
use zupflow_core::models::{
    Message, ModelProvider, ModelToolCall, ProviderError, Reply, ToolSpec, Turn, validate_completion,
};

fn tools() -> Vec<ToolSpec> {
    vec![
        ToolSpec {
            name: "echo".to_owned(),
            description: "Repeats the text it's given.".to_owned(),
            input_schema: json!({ "type": "object", "properties": { "text": { "type": "string" } } }),
        },
        ToolSpec {
            name: "search".to_owned(),
            description: "Finds documents by text.".to_owned(),
            input_schema: json!({ "type": "object", "properties": { "query": { "type": "string" } } }),
        },
    ]
}

fn user(text: &str) -> Message {
    Message::User { text: text.to_owned() }
}

fn turn<'a>(model: &'a str, messages: &'a [Message], tools: &'a [ToolSpec]) -> Turn<'a> {
    Turn {
        model,
        instructions: "Be brief.",
        messages,
        tools,
    }
}

/// `model`: an ID the provider serves.
pub async fn run(provider: &dyn ModelProvider, model: &str) {
    assert!(!provider.name().is_empty(), "a provider has a name");

    // No tools: text.
    let plain = [user("Say hello.")];
    let answer = provider.complete(&turn(model, &plain, &[])).await.unwrap();
    assert!(
        matches!(answer.reply, Reply::Text(_)),
        "without tools, never a call: {answer:?}"
    );
    assert!(answer.usage.input_tokens > 0, "tokens read are counted");
    assert!(answer.usage.output_tokens > 0, "tokens written are counted");

    // With tools: a valid answer, and, after the calls are answered, another.
    let tools = tools();
    let asked = [user(
        "call echo {\"text\": \"hello\"}\nRepeat the word hello with the echo tool.",
    )];
    let first_turn = turn(model, &asked, &tools);
    let first = provider.complete(&first_turn).await.unwrap();
    assert_eq!(
        validate_completion(&first_turn, &first),
        Ok(()),
        "a valid answer: {first:?}"
    );
    if let Reply::ToolCalls { text, calls } = &first.reply {
        let mut conversation = asked.to_vec();
        conversation.push(Message::Assistant {
            text: text.clone(),
            tool_calls: calls.clone(),
        });
        for call in calls {
            conversation.push(Message::ToolResult {
                call_id: call.id.clone(),
                content: "hello".to_owned(),
                is_error: false,
            });
        }
        let next_turn = turn(model, &conversation, &tools);
        let next = provider.complete(&next_turn).await.unwrap();
        assert_eq!(
            validate_completion(&next_turn, &next),
            Ok(()),
            "a valid answer: {next:?}"
        );
    }

    // A model it doesn't serve.
    assert_eq!(
        provider.complete(&turn("not-a-served-model-0", &plain, &tools)).await,
        Err(ProviderError::UnknownModel),
        "a model it doesn't serve"
    );

    // Turns that break the conversation rules are refused, whatever is in them.
    let call = |id: &str| Message::Assistant {
        text: None,
        tool_calls: vec![ModelToolCall {
            id: id.to_owned(),
            name: "echo".to_owned(),
            arguments: serde_json::Map::new(),
        }],
    };
    let result = |id: &str| Message::ToolResult {
        call_id: id.to_owned(),
        content: "hello".to_owned(),
        is_error: false,
    };
    let twice = [tools[0].clone(), tools[0].clone()];
    let invalid: [(&str, Vec<Message>, &[ToolSpec]); 6] = [
        ("an empty conversation", vec![], &tools),
        (
            "a conversation that doesn't start with the user",
            vec![call("a"), result("a")],
            &tools,
        ),
        ("a result for no call", vec![user("hi"), result("a")], &tools),
        (
            "a call left unanswered",
            vec![user("hi"), call("a"), user("again")],
            &tools,
        ),
        ("a conversation already answered", vec![user("hi"), call("a")], &tools),
        ("a tool given twice", vec![user("hi")], &twice),
    ];
    for (what, messages, tools) in invalid {
        assert!(
            matches!(
                provider.complete(&turn(model, &messages, tools)).await,
                Err(ProviderError::InvalidTurn(_))
            ),
            "refuses {what}"
        );
    }
}
