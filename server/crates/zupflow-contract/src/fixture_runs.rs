//! The fixture's runs, recorded step by step through the run store as the
//! run engine records them, so their timelines read like real ones.

use serde_json::{Map, Value, json};
use zupflow_core::ids::{AgentId, OrganizationId, RunId, SessionId, ToolCallId, WorkspaceId};
use zupflow_core::models::TokenUsage;
use zupflow_core::runs::{
    CallOutcome, CallState, CallTarget, Ending, NewRun, NewRunEvent, NewToolCall, NewTurn, RecordedDecision, RunError,
    RunEventType, RunStore, RunTrigger,
};
use zupflow_core::time::Timestamp;

fn reference(call_id: ToolCallId, target: &CallTarget, more: &[(&str, Value)]) -> Value {
    let mut data = zupflow_server::run_engine::reference(call_id, target);
    for (key, value) in more {
        data.insert((*key).to_owned(), value.clone());
    }
    Value::Object(data)
}

/// A run of `agent_id` that called `target`'s tool once: finished, or
/// waiting for approval of the call.
pub async fn seed(
    runs: &dyn RunStore,
    organization_id: OrganizationId,
    workspace_id: WorkspaceId,
    agent_id: AgentId,
    model: &str,
    target: CallTarget,
    waiting: bool,
) -> Result<(), RunError> {
    let run_id = RunId::generate();
    let job = zupflow_server::run_engine::job(run_id).ok_or(RunError::Unavailable)?;
    let input = format!("Use {} to find this week's open items.", target.tool);
    let now = Timestamp::now;
    runs.create_run(
        organization_id,
        workspace_id,
        run_id,
        &NewRun {
            agent_id,
            session_id: SessionId::generate(),
            trigger: RunTrigger::Api,
            input,
            model: model.to_owned(),
            instructions: "Be brief.".to_owned(),
        },
        &[NewRunEvent::new(RunEventType::RunQueued, json!({}))],
        &job,
        now(),
    )
    .await?;
    runs.start_run(
        organization_id,
        workspace_id,
        run_id,
        &[NewRunEvent::new(RunEventType::RunStarted, json!({ "model": model }))],
        now(),
    )
    .await?;
    let call_id = ToolCallId::generate();
    let arguments: Map<String, Value> = [("query".to_owned(), json!("open items"))].into_iter().collect();
    let said = format!("Calling {}.", target.tool);
    let decision = if waiting {
        RecordedDecision {
            verdict: "require_approval".to_owned(),
            policy: "configured".to_owned(),
            mode: Some("manual".to_owned()),
            rule: "grant_requires_approval".to_owned(),
            reason: Some(format!("The agent's grant requires approval for {}.", target.tool)),
        }
    } else {
        RecordedDecision {
            verdict: "allow".to_owned(),
            policy: "configured".to_owned(),
            mode: Some("manual".to_owned()),
            rule: "allowed".to_owned(),
            reason: None,
        }
    };
    let first = TokenUsage {
        input_tokens: 420,
        output_tokens: 18,
    };
    runs.record_turn(
        organization_id,
        workspace_id,
        run_id,
        0,
        &NewTurn {
            text: Some(said.clone()),
            calls: vec![NewToolCall {
                id: call_id,
                model_call_id: "fixture-1".to_owned(),
                name: target.tool.clone(),
                target: Some(target.clone()),
                arguments: arguments.clone(),
                state: if waiting {
                    CallState::AwaitingApproval
                } else {
                    CallState::Allowed
                },
                decision: Some(decision),
                outcome: None,
            }],
            usage: first,
            finish: None,
        },
        &[NewRunEvent::new(
            RunEventType::MessageAssistant,
            json!({ "text": said }),
        )],
        now(),
    )
    .await?;
    if waiting {
        runs.request_approval(
            organization_id,
            workspace_id,
            run_id,
            call_id,
            &[NewRunEvent::new(
                RunEventType::ApprovalRequested,
                reference(call_id, &target, &[("arguments", Value::Object(arguments))]),
            )],
            now(),
        )
        .await?;
        return Ok(());
    }
    runs.begin_call(
        organization_id,
        workspace_id,
        run_id,
        call_id,
        &[NewRunEvent::new(
            RunEventType::ToolCallStarted,
            reference(call_id, &target, &[("arguments", Value::Object(arguments))]),
        )],
        now(),
    )
    .await?;
    let result = json!({ "content": [{ "type": "text", "text": "3 open items" }] });
    runs.settle_call(
        organization_id,
        workspace_id,
        run_id,
        call_id,
        &[CallState::Started],
        CallState::Completed,
        &CallOutcome {
            content: result.to_string(),
            is_error: false,
        },
        &[NewRunEvent::new(
            RunEventType::ToolCallCompleted,
            reference(call_id, &target, &[("result", result), ("duration_ms", json!(640))]),
        )],
        now(),
    )
    .await?;
    let output = format!("{}: 3 open items", target.tool);
    let last = TokenUsage {
        input_tokens: 510,
        output_tokens: 12,
    };
    let usage = json!({
        "input_tokens": first.input_tokens + last.input_tokens,
        "output_tokens": first.output_tokens + last.output_tokens,
        "tool_calls": 1,
    });
    runs.record_turn(
        organization_id,
        workspace_id,
        run_id,
        1,
        &NewTurn {
            text: Some(output.clone()),
            calls: Vec::new(),
            usage: last,
            finish: Some(Ending::Completed { output: output.clone() }),
        },
        &[
            NewRunEvent::new(RunEventType::MessageAssistant, json!({ "text": output })),
            NewRunEvent::new(RunEventType::RunCompleted, json!({ "usage": usage })),
        ],
        now(),
    )
    .await?;
    Ok(())
}
