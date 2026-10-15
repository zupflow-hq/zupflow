//! A channel session's conversation across its runs.
//!
//! A run in a session continues the session's earlier runs instead of
//! rebuilding them, so everything before its new messages is a prefix the
//! provider caches. What an earlier run leaves in the session is its
//! [`transcript`]: its input, its steered turns and its turns with their
//! calls' outcomes, exactly as they were sent, from what was committed and
//! never changes after the run ends. A run's prompt is the transcripts of
//! the session's runs from its history start, then its own conversation.
//!
//! History is kept within a budget of tokens ([`trimmed_start`]): when the
//! next run would go over it, the start moves forward by whole runs, at
//! least a quarter of the budget at once, so the cache misses once per
//! trim rather than on every run. The start is stored on the run, so every
//! run until the next trim sends the same prefix.

use crate::models::{Message, ModelToolCall};
use crate::runs::state::{CallRecord, CommittedTurn, SteeredTurn};

/// The share of the model's context window a session's history may take.
pub const HISTORY_SHARE_PERCENT: u64 = 60;

/// The context window assumed for a model whose provider doesn't list one.
pub const DEFAULT_CONTEXT_WINDOW: u64 = 128_000;

/// A session's history budget in tokens, for a model with this context
/// window.
pub fn history_budget(context_window: Option<u64>) -> u64 {
    let window = context_window
        .filter(|window| *window > 0)
        .unwrap_or(DEFAULT_CONTEXT_WINDOW);
    (window.saturating_mul(HISTORY_SHARE_PERCENT) / 100).max(1)
}

/// A committed turn's assistant message exactly as its run sent it (its
/// text untouched, so every adapter renders it as it did), except that an
/// answer with neither text nor calls is left out: providers refuse an
/// empty one, and a run never sent its own last answer back.
fn assistant(text: &Option<String>, tool_calls: Vec<ModelToolCall>) -> Option<Message> {
    let blank = text.as_deref().is_none_or(|text| text.trim().is_empty());
    (!blank || !tool_calls.is_empty()).then(|| Message::Assistant {
        text: text.clone(),
        tool_calls,
    })
}

/// What an ended run leaves in its session's conversation, from what it
/// committed: its input, then each turn (the steered turns before the turn
/// they name) with its calls' outcomes.
///
/// It depends only on what never changes once the run has ended: a last
/// turn with calls is left out (one of its calls may be settled only
/// later, when its sender's lease lapses), and so are the turns after a
/// call without an outcome. Steered turns after the last turn kept follow
/// it, in order.
pub fn transcript(input: &str, turns: &[CommittedTurn], calls: &[CallRecord], steers: &[SteeredTurn]) -> Vec<Message> {
    let mut messages = vec![Message::User { text: input.to_owned() }];
    let last = turns.len().checked_sub(1);
    let mut kept = 0usize;
    'turns: for (index, turn) in turns.iter().enumerate() {
        let mut own: Vec<&CallRecord> = calls.iter().filter(|call| call.turn == turn.turn).collect();
        own.sort_by_key(|call| call.position);
        if Some(index) == last && !own.is_empty() {
            break;
        }
        let mut results = Vec::with_capacity(own.len());
        for call in &own {
            let Some(outcome) = &call.outcome else {
                break 'turns;
            };
            results.push(Message::ToolResult {
                call_id: call.model_call_id.clone(),
                content: outcome.content.clone(),
                is_error: outcome.is_error,
            });
        }
        for steer in steers.iter().filter(|steer| steer.turn == turn.turn) {
            messages.push(Message::User {
                text: steer.text.clone(),
            });
        }
        let tool_calls = own
            .iter()
            .map(|call| ModelToolCall {
                id: call.model_call_id.clone(),
                name: call.model_tool_name.clone(),
                arguments: call.arguments.clone(),
            })
            .collect();
        if let Some(answer) = assistant(&turn.text, tool_calls) {
            messages.push(answer);
        }
        messages.extend(results);
        kept = index + 1;
    }
    let kept = u32::try_from(kept).unwrap_or(u32::MAX);
    for steer in steers.iter().filter(|steer| steer.turn >= kept) {
        messages.push(Message::User {
            text: steer.text.clone(),
        });
    }
    messages
}

/// Roughly how many tokens messages take: one per four bytes of what they
/// say, as providers count closely enough for a budget.
pub fn weight(messages: &[Message]) -> u64 {
    let bytes: usize = messages
        .iter()
        .map(|message| match message {
            Message::User { text } => text.len(),
            Message::Assistant { text, tool_calls } => {
                text.as_deref().map_or(0, str::len)
                    + tool_calls
                        .iter()
                        .map(|call| {
                            call.id.len()
                                + call.name.len()
                                + serde_json::Value::Object(call.arguments.clone()).to_string().len()
                        })
                        .sum::<usize>()
            }
            Message::ToolResult { call_id, content, .. } => call_id.len() + content.len(),
        })
        .sum();
    u64::try_from(bytes).unwrap_or(u64::MAX).div_ceil(4)
}

/// Where a new run's history starts, as an index into `weights` (the
/// session's runs from the current start, oldest first): the current start
/// (`0`) while they and the new input fit in `budget`; else moved forward
/// by whole runs, at least a quarter of the budget at a time, until they
/// fit. `weights.len()` when none are kept.
pub fn trimmed_start(weights: &[u64], incoming: u64, budget: u64) -> usize {
    let quarter = (budget / 4).max(1);
    let mut total = weights.iter().fold(incoming, |sum, weight| sum.saturating_add(*weight));
    let mut start = 0;
    while total > budget && start < weights.len() {
        let mut cut = 0u64;
        while cut < quarter && start < weights.len() {
            cut = cut.saturating_add(weights[start]);
            start += 1;
        }
        total = total.saturating_sub(cut);
    }
    start
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use serde_json::Map;

    use super::*;
    use crate::ids::ToolCallId;
    use crate::runs::{CallOutcome, CallState};

    fn turn(turn: u32, text: Option<&str>) -> CommittedTurn {
        CommittedTurn {
            turn,
            text: text.map(str::to_owned),
        }
    }

    fn call(turn: u32, position: u32, outcome: Option<&str>) -> CallRecord {
        CallRecord {
            id: ToolCallId::generate(),
            turn,
            position,
            model_call_id: format!("c-{turn}-{position}"),
            model_tool_name: "lookup".to_owned(),
            target: None,
            arguments: Map::new(),
            state: if outcome.is_some() {
                CallState::Completed
            } else {
                CallState::Started
            },
            decision: None,
            call_digest: None,
            approval: None,
            outcome: outcome.map(|content| CallOutcome {
                content: content.to_owned(),
                is_error: false,
            }),
            failure: None,
            attempts: Vec::new(),
        }
    }

    fn user(text: &str) -> Message {
        Message::User { text: text.to_owned() }
    }

    #[test]
    fn keeps_a_completed_runs_turns_with_their_steered_turns() {
        let steers = [SteeredTurn {
            turn: 1,
            text: "New in the thread while you worked:\n[Ana] also".to_owned(),
        }];
        let messages = transcript(
            "[Ana] hello",
            &[turn(0, Some("Looking.")), turn(1, Some("Done."))],
            &[call(0, 0, Some("{}"))],
            &steers,
        );
        assert_eq!(messages.len(), 5);
        assert_eq!(messages[0], user("[Ana] hello"));
        assert!(matches!(&messages[1], Message::Assistant { tool_calls, .. } if tool_calls.len() == 1));
        assert!(matches!(&messages[2], Message::ToolResult { content, .. } if content == "{}"));
        assert_eq!(messages[3], user(&steers[0].text));
        assert!(matches!(&messages[4], Message::Assistant { text: Some(text), .. } if text == "Done."));
    }

    #[test]
    fn leaves_out_a_last_turn_with_calls_whatever_their_outcome() {
        let unsettled = transcript(
            "hi",
            &[turn(0, Some("a")), turn(1, None)],
            &[call(0, 0, Some("x")), call(1, 0, None)],
            &[],
        );
        let settled = transcript(
            "hi",
            &[turn(0, Some("a")), turn(1, None)],
            &[call(0, 0, Some("x")), call(1, 0, Some("late"))],
            &[],
        );
        assert_eq!(
            unsettled, settled,
            "an outcome settled after the run ends changes nothing"
        );
        assert_eq!(unsettled.len(), 3);
    }

    #[test]
    fn leaves_out_an_empty_answer_and_keeps_late_steers() {
        let messages = transcript(
            "hi",
            &[turn(0, Some("  "))],
            &[],
            &[SteeredTurn {
                turn: 1,
                text: "more".to_owned(),
            }],
        );
        assert_eq!(messages, vec![user("hi"), user("more")]);
    }

    #[test]
    fn trims_by_whole_runs_a_quarter_of_the_budget_at_least() {
        assert_eq!(trimmed_start(&[10, 10, 10], 10, 100), 0, "fits");
        // 100 over 80: cut at least 20 (a quarter), so two runs of 10 go.
        assert_eq!(trimmed_start(&[10, 10, 30, 30], 20, 80), 2);
        // Nothing fits but the input: every run goes.
        assert_eq!(trimmed_start(&[50, 50], 90, 80), 2);
        assert_eq!(history_budget(Some(1_000)), 600);
        assert_eq!(history_budget(None), 76_800);
    }
}
