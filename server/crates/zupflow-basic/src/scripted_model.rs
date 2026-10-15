//! [`ScriptedModel`]: the basic [`ModelProvider`], for tests and local runs.
//! It calls no service: it answers by a fixed script, the same way every
//! time, so a run's steps can be predicted.
//!
//! - Asked by the user, it looks for lines `call <tool> <arguments>` in what
//!   they wrote (the arguments a JSON object, optional) and calls each tool
//!   named that it was given, in order. A line naming a tool it wasn't given
//!   is answered in text instead. Without such lines it answers
//!   `You said: <what they wrote>`.
//! - A line `then` ends a round of calls: given the results of one round,
//!   it makes the next round's calls, as a model working step by step
//!   would.
//! - Given its last round's results, it answers in text, one line per
//!   call: `<tool>: <result>`, or `<tool> failed: <reason>`.
//!
//! It serves the model IDs it's made with and no others, and refuses a key
//! the scripted providers refuse (one containing `invalid`), whatever the
//! provider kind. Tokens are counted as words, so usage is deterministic too.

use async_trait::async_trait;
use serde_json::{Map, Value};
use zupflow_core::models::{
    Completion, Message, ModelProvider, ModelToolCall, ProviderAccess, ProviderError, Reply, TokenUsage, Turn,
    validate_turn,
};

use crate::scripted_providers::ScriptedProviders;

/// Recorded with failures.
pub const NAME: &str = "scripted";

/// The most of a tool result it repeats.
const RESULT_PREVIEW: usize = 200;

#[derive(Debug, Clone, Default)]
pub struct ScriptedModel {
    models: Vec<String>,
}

impl ScriptedModel {
    /// A model serving exactly these IDs.
    pub fn serving<I: IntoIterator<Item = S>, S: Into<String>>(models: I) -> Self {
        Self {
            models: models.into_iter().map(Into::into).collect(),
        }
    }
}

fn words(text: &str) -> u64 {
    text.split_whitespace().count() as u64
}

/// A round of calls: each tool's name and arguments.
type Round = Vec<(String, Option<Map<String, Value>>)>;

/// The rounds of a message's `call` lines, split by `then` lines.
fn rounds(text: &str) -> Vec<Round> {
    text.split('\n')
        .collect::<Vec<_>>()
        .split(|line| line.trim() == "then")
        .map(|lines| directives(&lines.join("\n")))
        .filter(|round| !round.is_empty())
        .collect()
}

/// The `call` lines of a message: each tool's name and arguments.
fn directives(text: &str) -> Round {
    text.lines()
        .filter_map(|line| {
            let rest = line.trim().strip_prefix("call ")?.trim();
            let (name, arguments) = match rest.split_once(char::is_whitespace) {
                Some((name, arguments)) => (name, arguments.trim()),
                None => (rest, ""),
            };
            let arguments = if arguments.is_empty() {
                Some(Map::new())
            } else {
                match serde_json::from_str::<Value>(arguments) {
                    Ok(Value::Object(arguments)) => Some(arguments),
                    _ => None,
                }
            };
            (!name.is_empty()).then(|| (name.to_owned(), arguments))
        })
        .collect()
}

fn preview(text: &str) -> String {
    let mut preview: String = text.chars().take(RESULT_PREVIEW).collect();
    if text.chars().count() > RESULT_PREVIEW {
        preview.push('…');
    }
    preview
}

fn answer_user(turn: &Turn<'_>, text: &str) -> Reply {
    match rounds(text).into_iter().next() {
        Some(asked) => call(turn, asked),
        None => Reply::Text(format!("You said: {}", text.trim())),
    }
}

/// The next round's calls when the user's last message asked for more
/// than were made since, else one line per call of the last round.
fn answer_results(turn: &Turn<'_>) -> Reply {
    let Some(asked) = turn
        .messages
        .iter()
        .rposition(|message| matches!(message, Message::User { .. }))
    else {
        return report(turn);
    };
    let Message::User { text } = &turn.messages[asked] else {
        return report(turn);
    };
    let made = turn.messages[asked..]
        .iter()
        .filter(|message| matches!(message, Message::Assistant { tool_calls, .. } if !tool_calls.is_empty()))
        .count();
    match rounds(text).into_iter().nth(made) {
        Some(next) => call(turn, next),
        None => report(turn),
    }
}

/// Calls the tools of `asked` it was given, numbered by the answers so far.
fn call(turn: &Turn<'_>, asked: Round) -> Reply {
    let round = turn
        .messages
        .iter()
        .filter(|message| matches!(message, Message::Assistant { .. }))
        .count();
    let mut calls = Vec::new();
    let mut refused = Vec::new();
    for (name, arguments) in asked {
        match (turn.tools.iter().any(|tool| tool.name == name), arguments) {
            (true, Some(arguments)) => calls.push(ModelToolCall {
                id: format!("scripted-{round}-{}", calls.len() + 1),
                name,
                arguments,
            }),
            (true, None) => refused.push(format!("The arguments for {name} aren't a JSON object.")),
            (false, _) => refused.push(format!("I can't use {name}: it isn't one of my tools.")),
        }
    }
    if calls.is_empty() {
        return Reply::Text(refused.join("\n"));
    }
    let names: Vec<&str> = calls.iter().map(|call| call.name.as_str()).collect();
    let mut said = format!("Calling {}.", names.join(", "));
    for line in refused {
        said.push('\n');
        said.push_str(&line);
    }
    Reply::ToolCalls {
        text: Some(said),
        calls,
    }
}

/// One line per call of the last assistant message, from its results.
fn report(turn: &Turn<'_>) -> Reply {
    let Some(asked) = turn.messages.iter().rev().find_map(|message| match message {
        Message::Assistant { tool_calls, .. } => Some(tool_calls),
        _ => None,
    }) else {
        return Reply::Text(String::new());
    };
    let lines: Vec<String> = asked
        .iter()
        .map(|call| {
            let result = turn.messages.iter().rev().find_map(|message| match message {
                Message::ToolResult {
                    call_id,
                    content,
                    is_error,
                } if *call_id == call.id => Some((content.as_str(), *is_error)),
                _ => None,
            });
            match result {
                Some((content, false)) => format!("{}: {}", call.name, preview(content)),
                Some((content, true)) => format!("{} failed: {}", call.name, preview(content)),
                None => format!("{}: no result", call.name),
            }
        })
        .collect();
    Reply::Text(lines.join("\n"))
}

fn usage(turn: &Turn<'_>, reply: &Reply) -> TokenUsage {
    let read: u64 = words(turn.instructions)
        + turn
            .messages
            .iter()
            .map(|message| match message {
                Message::User { text } => words(text),
                Message::Assistant { text, tool_calls } => text.as_deref().map_or(0, words) + tool_calls.len() as u64,
                Message::ToolResult { content, .. } => words(content),
            })
            .sum::<u64>()
        + turn.tools.iter().map(|tool| 1 + words(&tool.description)).sum::<u64>();
    let written = match reply {
        Reply::Text(text) => words(text),
        Reply::ToolCalls { text, calls } => text.as_deref().map_or(0, words) + calls.len() as u64,
    };
    TokenUsage::new(read.max(1), written.max(1), 0, 0)
}

#[async_trait]
impl ModelProvider for ScriptedModel {
    fn name(&self) -> &'static str {
        NAME
    }

    async fn complete(&self, access: &ProviderAccess<'_>, turn: &Turn<'_>) -> Result<Completion, ProviderError> {
        if !ScriptedProviders::accepts(access.key) {
            return Err(ProviderError::KeyRefused);
        }
        if !self.models.iter().any(|model| model == turn.model) {
            return Err(ProviderError::UnknownModel);
        }
        validate_turn(turn).map_err(ProviderError::InvalidTurn)?;
        let reply = match turn.messages.last() {
            Some(Message::User { text }) => answer_user(turn, text),
            _ => answer_results(turn),
        };
        let usage = usage(turn, &reply);
        Ok(Completion { reply, usage })
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use serde_json::json;
    use zupflow_core::model_providers::{ProviderKey, ProviderKind};
    use zupflow_core::models::ToolSpec;

    use super::*;

    fn key() -> ProviderKey {
        ProviderKey::new("sk-test-long-suite-0000-xx".to_owned())
    }

    fn tools() -> Vec<ToolSpec> {
        ["echo", "search"]
            .into_iter()
            .map(|name| ToolSpec {
                name: name.to_owned(),
                description: format!("The {name} tool."),
                input_schema: json!({ "type": "object" }),
            })
            .collect()
    }

    async fn complete(model: &ScriptedModel, messages: &[Message]) -> Completion {
        model
            .complete(
                &ProviderAccess {
                    kind: ProviderKind::OpenAi,
                    key: &key(),
                },
                &Turn {
                    model: "scripted-a",
                    instructions: "Be brief.",
                    messages,
                    tools: &tools(),
                    cache_key: None,
                },
            )
            .await
            .unwrap()
    }

    #[tokio::test]
    async fn passes_the_conformance_suite() {
        let model = ScriptedModel::serving(["scripted-a", "scripted-b"]);
        let refused = ProviderKey::new("sk-test-long-INVALID-0000".to_owned());
        for kind in ProviderKind::ALL {
            let access = ProviderAccess { kind, key: &key() };
            zupflow_conformance::model_provider::run(&model, &access, &refused, "scripted-a").await;
            zupflow_conformance::model_provider::run(&model, &access, &refused, "scripted-b").await;
        }
    }

    #[tokio::test]
    async fn calls_the_tools_it_is_told_to_and_reports_their_results() {
        let model = ScriptedModel::serving(["scripted-a"]);
        let asked = Message::User {
            text: "Please:\ncall echo {\"text\": \"hi\"}\ncall search\ncall delete_everything".to_owned(),
        };
        let first = complete(&model, std::slice::from_ref(&asked)).await;
        let Reply::ToolCalls { text, calls } = &first.reply else {
            panic!("{first:?}");
        };
        assert_eq!(
            calls
                .iter()
                .map(|call| (
                    call.id.as_str(),
                    call.name.as_str(),
                    Value::Object(call.arguments.clone())
                ))
                .collect::<Vec<_>>(),
            [
                ("scripted-0-1", "echo", json!({ "text": "hi" })),
                ("scripted-0-2", "search", json!({}))
            ]
        );
        assert!(text.as_deref().unwrap().contains("I can't use delete_everything"));

        let conversation = [
            asked,
            Message::Assistant {
                text: text.clone(),
                tool_calls: calls.clone(),
            },
            Message::ToolResult {
                call_id: "scripted-0-2".to_owned(),
                content: "denied by a person".to_owned(),
                is_error: true,
            },
            Message::ToolResult {
                call_id: "scripted-0-1".to_owned(),
                content: "hi".to_owned(),
                is_error: false,
            },
        ];
        let second = complete(&model, &conversation).await;
        assert_eq!(
            second.reply,
            Reply::Text("echo: hi\nsearch failed: denied by a person".to_owned())
        );
        assert_eq!(complete(&model, &conversation).await, second, "the same every time");
        assert!(second.usage.input_tokens > first.usage.input_tokens);
    }

    #[tokio::test]
    async fn makes_its_calls_round_by_round() {
        let model = ScriptedModel::serving(["scripted-a"]);
        let mut conversation = vec![Message::User {
            text: "call echo {\"text\": \"one\"}\nthen\ncall search\ncall echo".to_owned(),
        }];
        let mut rounds = Vec::new();
        loop {
            let answer = complete(&model, &conversation).await;
            let Reply::ToolCalls { text, calls } = answer.reply else {
                assert_eq!(answer.reply, Reply::Text("search: found\necho: found".to_owned()));
                break;
            };
            rounds.push(calls.iter().map(|call| call.id.clone()).collect::<Vec<_>>());
            conversation.push(Message::Assistant {
                text,
                tool_calls: calls.clone(),
            });
            conversation.extend(calls.iter().map(|call| Message::ToolResult {
                call_id: call.id.clone(),
                content: "found".to_owned(),
                is_error: false,
            }));
        }
        assert_eq!(rounds, [vec!["scripted-0-1"], vec!["scripted-1-1", "scripted-1-2"]]);
    }

    #[tokio::test]
    async fn answers_in_text_without_a_call() {
        let model = ScriptedModel::serving(["scripted-a"]);
        let answer = complete(
            &model,
            &[Message::User {
                text: "  Hello  ".to_owned(),
            }],
        )
        .await;
        assert_eq!(answer.reply, Reply::Text("You said: Hello".to_owned()));
        assert_eq!(answer.usage.output_tokens, 3);
    }
}
