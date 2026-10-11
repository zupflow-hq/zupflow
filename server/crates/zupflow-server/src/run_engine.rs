//! The run engine (milestone 4): a worker claims a run's job with a lease
//! and takes it step by step, each step recorded in the run store before the
//! next is taken, so a run survives a restart and no step is taken twice.
//!
//! A run is a conversation with the agent's model, kept as it started (its
//! model and instructions). Each turn, the model is offered the tools the
//! agent's grants name that the floor and the [`ApprovalPolicy`] wouldn't
//! refuse (a refused tool is never shown; a changed one that nobody
//! reviewed is offered with its pinned definition, and waits for approval).
//! Each call it asks for is decided again, the floor first and then the
//! policy, and the decision is recorded with the call: allowed calls are
//! sent in order through the installation's MCP server (`OutboundHttp`, the
//! credential injected last); a call that needs approval pauses the run
//! until the customer's backend decides; a refusal or a denial goes back to
//! the model as the call's result, with its reason.
//!
//! Delivery is at least once. Every step is a conditional write that
//! answers `Moved` when another worker, a cancel or a decision got there
//! first; the worker then reads the run again. A call is recorded as sent
//! before it's sent, and its outcome after: a call found sent without an
//! outcome (the worker stopped mid-call) is settled as unknown and never
//! sent again. Asking the model again is safe: nothing it is sent acts.
//!
//! Every event appended is published on the [`EVENTS_TOPIC`] of the
//! workspace and of its environment, as a notification: listeners read the
//! timeline from the store.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::time::Duration;

use serde_json::{Map, Value, json};
use zupflow_core::environment::Environment;
use zupflow_core::ids::{AgentId, OrganizationId, RunId, ToolCallId, WorkspaceId};
use zupflow_core::installations::{Installation, InstalledApp};
use zupflow_core::jobs::{Job, JobId, JobOptions, LeaseToken};
use zupflow_core::models::{
    MAX_MODEL_TOOL_NAME, Message, ModelToolCall, ProviderError, Reply, TokenUsage, ToolSpec, Turn, validate_completion,
};
use zupflow_core::policy::{CallApp, Decision, GrantedAs, OrganizationLevel, ToolCall, Verdict, floor};
use zupflow_core::runs::{
    CallOutcome, CallState, CallTarget, Ending, Failure, NewRunEvent, NewToolCall, NewTurn, QueuedJob,
    RecordedDecision, RunError, RunEvent, RunEventType, RunProgress, RunStatus, RunToolCall, RunUsage,
};
use zupflow_core::scope::{Name, Scope};
use zupflow_core::time::Timestamp;
use zupflow_core::tools::{RegisteredTool, ToolError};
use zupflow_mcp::CallError;

use crate::auth::AppState;
use crate::mcp_access::{self, Unreachable};

/// The job kind.
pub const RUN_AGENT: &str = "run_agent";

/// The event bus topic every appended run event is announced on.
pub const EVENTS_TOPIC: &str = "run_events";

/// How long a claim holds a run; renewed before every step, and longer than
/// any one step (a model turn, a tool call).
const LEASE: Duration = Duration::from_secs(5 * 60);

const IDLE: Duration = Duration::from_secs(1);

/// How many times a run's job is tried before the run fails.
pub const ATTEMPTS: u32 = 5;

/// The most turns a run takes before it fails.
pub const MAX_TURNS: usize = 10;

/// The most calls one turn may ask for; the rest are refused.
pub const MAX_CALLS_PER_TURN: usize = 20;

/// The most steps one claim takes before it gives the run back to the queue.
const MAX_STEPS: usize = 200;

/// What the model reads back for a call a person denied.
pub const DENIED: &str = "A person denied this call, so it wasn't made. Continue without it.";

const UNKNOWN: &str = "The call was sent, but no answer came back, so whether it acted is unknown. It wasn't repeated.";

fn kind() -> Option<Name> {
    Name::new(RUN_AGENT).ok()
}

/// The job that takes a run forward, to queue with a change that needs one.
pub fn job(run_id: RunId) -> Option<QueuedJob> {
    Some(QueuedJob {
        kind: kind()?,
        payload: json!({ "run_id": run_id.to_string() }).to_string().into_bytes(),
        options: JobOptions {
            delay: Duration::ZERO,
            max_attempts: ATTEMPTS,
        },
    })
}

/// Announces appended events on the workspace's and its environment's
/// topic. Best effort: the events are in the store either way.
pub async fn publish(
    state: &AppState,
    organization_id: OrganizationId,
    workspace_id: WorkspaceId,
    environment: Environment,
    events: &[RunEvent],
) {
    let Ok(topic) = Name::new(EVENTS_TOPIC) else {
        return;
    };
    for event in events {
        let payload = json!({
            "workspace_id": workspace_id,
            "run_id": event.run_id,
            "sequence": event.sequence,
            "type": event.kind.as_str(),
        })
        .to_string()
        .into_bytes();
        for scope in [
            Scope::Workspace(organization_id, workspace_id),
            Scope::Environment(organization_id, environment),
        ] {
            if let Err(error) = state.events.publish(&scope, &topic, payload.clone()).await {
                tracing::debug!(event = "runs.publish_failed", %organization_id, %workspace_id, %error);
            }
        }
    }
}

/// A call's reference, as its events show it.
pub fn reference(call_id: ToolCallId, target: &CallTarget) -> Map<String, Value> {
    let mut data = Map::new();
    data.insert("call_id".to_owned(), json!(call_id));
    data.insert("installation_id".to_owned(), json!(target.installation_id));
    data.insert("installation_label".to_owned(), json!(target.installation_label));
    data.insert("app_name".to_owned(), json!(target.app_name));
    data.insert("tool".to_owned(), json!(target.tool));
    data
}

fn with(mut data: Map<String, Value>, more: &[(&str, Value)]) -> Value {
    for (key, value) in more {
        data.insert((*key).to_owned(), value.clone());
    }
    Value::Object(data)
}

fn failed_event(call_id: ToolCallId, target: &CallTarget, error: &Failure, duration_ms: i64) -> NewRunEvent {
    NewRunEvent::new(
        RunEventType::ToolCallFailed,
        with(
            reference(call_id, target),
            &[("error", json!(error)), ("duration_ms", json!(duration_ms.max(0)))],
        ),
    )
}

fn recorded(decision: &Decision) -> RecordedDecision {
    let (verdict, reason) = match &decision.verdict {
        Verdict::Allow => ("allow", None),
        Verdict::RequireApproval { reason } => ("require_approval", Some(reason.clone())),
        Verdict::Deny { reason } => ("deny", Some(reason.clone())),
    };
    RecordedDecision {
        verdict: verdict.to_owned(),
        policy: decision.policy.to_owned(),
        mode: decision.mode.map(|mode| mode.as_str().to_owned()),
        rule: decision.rule.as_str().to_owned(),
        reason,
    }
}

fn reconnect(label: &str) -> Failure {
    Failure::new(
        "installation_reconnect_required",
        format!("{label} needs to be reconnected before its tools can run."),
    )
}

/// What came of one claim.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Outcome {
    /// Finished, paused, or gone: nothing more for this job.
    Done,
    /// Worth another attempt later, with why.
    Retry(&'static str),
    /// The lease was lost: another worker has the run.
    Lost,
}

/// What one step did.
enum Step {
    /// Read the run again and take the next step.
    Next,
    Stop(Outcome),
}

fn retry(error: RunError) -> Step {
    match error {
        RunError::Unavailable => Step::Stop(Outcome::Retry("store_unavailable")),
        // Moved on: read it again.
        _ => Step::Next,
    }
}

/// A tool offered to the model this turn.
struct Offer {
    /// The name the model calls it by.
    name: String,
    target: CallTarget,
    call: ToolCall,
    spec: ToolSpec,
}

/// A tool name a model can call: the tool's own, with anything else
/// replaced, cut to length, and made unique in the turn.
fn model_name(tool: &str, taken: &mut HashSet<String>) -> String {
    let base: String = tool
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '_' || c == '-' {
                c
            } else {
                '_'
            }
        })
        .take(MAX_MODEL_TOOL_NAME)
        .collect();
    let base = if base.is_empty() { "tool".to_owned() } else { base };
    let mut name = base.clone();
    let mut n = 2;
    while taken.contains(&name) {
        let suffix = format!("_{n}");
        let keep = MAX_MODEL_TOOL_NAME.saturating_sub(suffix.len());
        name = format!("{}{suffix}", base.chars().take(keep).collect::<String>());
        n += 1;
    }
    taken.insert(name.clone());
    name
}

/// What the model is told about a tool: its pinned definition.
fn spec(name: String, tool: &RegisteredTool) -> ToolSpec {
    let definition: Map<String, Value> = serde_json::from_str(&tool.pinned.definition).unwrap_or_default();
    let description = definition
        .get("description")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_owned();
    let input_schema = definition
        .get("inputSchema")
        .filter(|schema| schema.is_object())
        .cloned()
        .unwrap_or_else(|| json!({ "type": "object" }));
    ToolSpec {
        name,
        description,
        input_schema,
    }
}

/// The conversation so far, rebuilt from what the store recorded.
fn conversation(progress: &RunProgress) -> Vec<Message> {
    let mut messages = vec![Message::User {
        text: progress.run.input.clone(),
    }];
    let calls: HashMap<ToolCallId, &RunToolCall> = progress.calls.iter().map(|call| (call.id, call)).collect();
    for turn in &progress.turns {
        let asked: Vec<&RunToolCall> = turn.calls.iter().filter_map(|id| calls.get(id).copied()).collect();
        messages.push(Message::Assistant {
            text: turn.text.clone(),
            tool_calls: asked
                .iter()
                .map(|call| ModelToolCall {
                    id: call.model_call_id.clone(),
                    name: call.name.clone(),
                    arguments: call.arguments.clone(),
                })
                .collect(),
        });
        for call in asked {
            if let Some(outcome) = &call.outcome {
                messages.push(Message::ToolResult {
                    call_id: call.model_call_id.clone(),
                    content: outcome.content.clone(),
                    is_error: outcome.is_error,
                });
            }
        }
    }
    messages
}

/// One claim of one run, in its organization and workspace.
struct Engine<'a> {
    state: &'a AppState,
    organization_id: OrganizationId,
    workspace_id: WorkspaceId,
    environment: Environment,
    run_id: RunId,
    claim: Option<(&'a Scope, JobId, &'a LeaseToken)>,
}

impl Engine<'_> {
    async fn publish(&self, events: &[RunEvent]) {
        publish(
            self.state,
            self.organization_id,
            self.workspace_id,
            self.environment,
            events,
        )
        .await;
    }

    /// Renews the lease; `false` once it's lost.
    async fn renew(&self) -> bool {
        let Some((scope, id, lease)) = self.claim else {
            return true;
        };
        self.state.jobs.extend(scope, id, lease, LEASE).await.is_ok()
    }

    async fn advance(&self) -> Outcome {
        for _ in 0..MAX_STEPS {
            if !self.renew().await {
                return Outcome::Lost;
            }
            let progress = match self
                .state
                .runs
                .find_progress(self.organization_id, self.workspace_id, self.run_id)
                .await
            {
                Ok(Some(progress)) => progress,
                Ok(None) => return Outcome::Done,
                Err(_) => return Outcome::Retry("store_unavailable"),
            };
            let step = match progress.run.status {
                RunStatus::Queued => self.start(&progress).await,
                RunStatus::Running => self.step(&progress).await,
                // Paused for a decision, or finished.
                _ => return Outcome::Done,
            };
            if let Step::Stop(outcome) = step {
                return outcome;
            }
        }
        Outcome::Retry("too_many_steps")
    }

    async fn start(&self, progress: &RunProgress) -> Step {
        let started = NewRunEvent::new(RunEventType::RunStarted, json!({ "model": progress.model }));
        match self
            .state
            .runs
            .start_run(
                self.organization_id,
                self.workspace_id,
                self.run_id,
                &[started],
                Timestamp::now(),
            )
            .await
        {
            Ok(events) => {
                self.publish(&events).await;
                Step::Next
            }
            Err(error) => retry(error),
        }
    }

    async fn step(&self, progress: &RunProgress) -> Step {
        // A call recorded as sent with no outcome: the worker that sent it
        // stopped. It may have acted, so it's never sent again.
        if let Some(call) = progress.calls.iter().find(|call| call.state == CallState::Started) {
            return self.settle_unknown(call).await;
        }
        let pending = progress.calls.iter().find(|call| {
            matches!(
                call.state,
                CallState::Allowed | CallState::Approved | CallState::AwaitingApproval
            )
        });
        match pending {
            Some(call) if call.state == CallState::AwaitingApproval => self.pause(call).await,
            Some(call) => self.send(progress, call).await,
            None => self.turn(progress).await,
        }
    }

    async fn settle(
        &self,
        call: &RunToolCall,
        from: &[CallState],
        to: CallState,
        outcome: CallOutcome,
        events: Vec<NewRunEvent>,
    ) -> Step {
        match self
            .state
            .runs
            .settle_call(
                self.organization_id,
                self.workspace_id,
                self.run_id,
                call.id,
                from,
                to,
                &outcome,
                &events,
                Timestamp::now(),
            )
            .await
        {
            Ok(events) => {
                self.publish(&events).await;
                Step::Next
            }
            Err(error) => retry(error),
        }
    }

    async fn settle_unknown(&self, call: &RunToolCall) -> Step {
        let duration = call
            .started_at
            .map_or(0, |started| Timestamp::now().millis() - started.millis());
        let events = call
            .target
            .as_ref()
            .map(|target| {
                failed_event(
                    call.id,
                    target,
                    &Failure::new(
                        "outcome_unknown",
                        "The call was sent, but its outcome is unknown. It wasn't repeated.",
                    ),
                    duration,
                )
            })
            .into_iter()
            .collect();
        tracing::warn!(event = "runs.call_outcome_unknown", organization_id = %self.organization_id, workspace_id = %self.workspace_id, run_id = %self.run_id, call_id = %call.id);
        self.settle(
            call,
            &[CallState::Started],
            CallState::Unknown,
            CallOutcome {
                content: UNKNOWN.to_owned(),
                is_error: true,
            },
            events,
        )
        .await
    }

    async fn pause(&self, call: &RunToolCall) -> Step {
        let Some(target) = &call.target else {
            return self.refuse(call, "This call can't be made.".to_owned()).await;
        };
        let requested = NewRunEvent::new(
            RunEventType::ApprovalRequested,
            with(
                reference(call.id, target),
                &[("arguments", Value::Object(call.arguments.clone()))],
            ),
        );
        match self
            .state
            .runs
            .request_approval(
                self.organization_id,
                self.workspace_id,
                self.run_id,
                call.id,
                &[requested],
                Timestamp::now(),
            )
            .await
        {
            Ok(events) => {
                self.publish(&events).await;
                // Waiting: the decision queues the run again.
                Step::Stop(Outcome::Done)
            }
            Err(error) => retry(error),
        }
    }

    /// Settles an allowed or approved call as refused, never sent.
    async fn refuse(&self, call: &RunToolCall, reason: String) -> Step {
        let events = call
            .target
            .as_ref()
            .map(|target| failed_event(call.id, target, &Failure::new("tool_refused", reason.clone()), 0))
            .into_iter()
            .collect();
        self.settle(
            call,
            &[CallState::Allowed, CallState::Approved],
            CallState::Refused,
            CallOutcome {
                content: reason,
                is_error: true,
            },
            events,
        )
        .await
    }

    /// Fails the run, as long as it still has the turns `progress` read.
    async fn fail(&self, progress: &RunProgress, failure: Failure) -> Step {
        let Ok(turns) = u32::try_from(progress.turns.len()) else {
            return Step::Stop(Outcome::Done);
        };
        let event = NewRunEvent::new(RunEventType::RunFailed, json!({ "error": failure }));
        match self
            .state
            .runs
            .finish_run(
                self.organization_id,
                self.workspace_id,
                self.run_id,
                turns,
                &Ending::Failed(failure),
                &[event],
                Timestamp::now(),
            )
            .await
        {
            Ok(events) => {
                self.publish(&events).await;
                Step::Stop(Outcome::Done)
            }
            Err(error) => retry(error),
        }
    }

    /// Records a new decision on an allowed or approved call before it's
    /// sent: it now waits for approval, or is refused.
    async fn redecide(
        &self,
        call: &RunToolCall,
        target: &CallTarget,
        to: CallState,
        decision: &Decision,
        outcome: Option<CallOutcome>,
    ) -> Step {
        let events: Vec<NewRunEvent> = outcome
            .as_ref()
            .map(|outcome| {
                failed_event(
                    call.id,
                    target,
                    &Failure::new("tool_refused", outcome.content.clone()),
                    0,
                )
            })
            .into_iter()
            .collect();
        match self
            .state
            .runs
            .decide_call(
                self.organization_id,
                self.workspace_id,
                self.run_id,
                call.id,
                &[CallState::Allowed, CallState::Approved],
                to,
                &recorded(decision),
                outcome.as_ref(),
                &events,
                Timestamp::now(),
            )
            .await
        {
            Ok(events) => {
                self.publish(&events).await;
                Step::Next
            }
            Err(error) => retry(error),
        }
    }

    /// Sends an allowed or approved call, once.
    async fn send(&self, progress: &RunProgress, call: &RunToolCall) -> Step {
        let Some(target) = &call.target else {
            return self.refuse(call, "This call can't be made.".to_owned()).await;
        };
        // The floor holds at the moment of sending: a grant revoked or a
        // tool gone since the call was decided refuses it now.
        let installation = match self
            .state
            .installations
            .find_installation(self.organization_id, self.workspace_id, target.installation_id)
            .await
        {
            Ok(Some(installation)) => installation,
            Ok(None) => {
                return self
                    .refuse(call, format!("{} is no longer installed.", target.installation_label))
                    .await;
            }
            Err(_) => return Step::Stop(Outcome::Retry("store_unavailable")),
        };
        let checked = match self.tool_call(progress.run.agent_id, &installation, &target.tool).await {
            Ok(checked) => checked,
            Err(why) => return Step::Stop(Outcome::Retry(why)),
        };
        // Decided again on what holds now, the floor first and then the
        // policy: a call allowed then may need approval now, and one
        // allowed or approved then may be refused now. A person's approval
        // stands for what needed it.
        let decision = self.decide(&checked).await;
        match &decision.verdict {
            Verdict::Deny { reason } => {
                let reason = reason.clone();
                return self
                    .redecide(
                        call,
                        target,
                        CallState::Refused,
                        &decision,
                        Some(CallOutcome {
                            content: reason,
                            is_error: true,
                        }),
                    )
                    .await;
            }
            Verdict::RequireApproval { .. } if call.state == CallState::Allowed => {
                return self
                    .redecide(call, target, CallState::AwaitingApproval, &decision, None)
                    .await;
            }
            _ => {}
        }
        let reach = match mcp_access::reach(self.state, self.organization_id, self.workspace_id, &installation).await {
            Ok(reach) => reach,
            Err(Unreachable::Retry(why)) => return Step::Stop(Outcome::Retry(why)),
            Err(unreachable) if unreachable.needs_reconnect() => {
                let failure = reconnect(&target.installation_label);
                let settled = self
                    .settle(
                        call,
                        &[CallState::Allowed, CallState::Approved],
                        CallState::Refused,
                        CallOutcome {
                            content: failure.message.clone(),
                            is_error: true,
                        },
                        vec![failed_event(call.id, target, &failure, 0)],
                    )
                    .await;
                if let Step::Stop(outcome) = settled {
                    return Step::Stop(outcome);
                }
                return self.fail(progress, failure).await;
            }
            Err(unreachable) => {
                tracing::warn!(event = "runs.installation_unreachable", organization_id = %self.organization_id, workspace_id = %self.workspace_id, installation_id = %target.installation_id, why = unreachable.why());
                return self
                    .refuse(
                        call,
                        format!(
                            "{} can't be used as it's set up: check its app's configuration, or reconnect it.",
                            target.installation_label
                        ),
                    )
                    .await;
            }
        };
        let started = NewRunEvent::new(
            RunEventType::ToolCallStarted,
            with(
                reference(call.id, target),
                &[("arguments", Value::Object(call.arguments.clone()))],
            ),
        );
        match self
            .state
            .runs
            .begin_call(
                self.organization_id,
                self.workspace_id,
                self.run_id,
                call.id,
                &[started],
                Timestamp::now(),
            )
            .await
        {
            Ok(events) => self.publish(&events).await,
            Err(error) => return retry(error),
        }
        let began = Timestamp::now();
        let answer = mcp_access::call_tool(
            self.state,
            self.organization_id,
            self.workspace_id,
            installation.id,
            reach,
            &target.tool,
            &call.arguments,
        )
        .await;
        let duration = Timestamp::now().millis() - began.millis();
        let from = [CallState::Started];
        let failed = |code: &str, message: &str| {
            let failure = Failure::new(code, message);
            (
                CallOutcome {
                    content: message.to_owned(),
                    is_error: true,
                },
                vec![failed_event(call.id, target, &failure, duration)],
            )
        };
        let (to, (outcome, events)) = match answer {
            Ok(result) if !result.is_error => {
                let result = Value::Object(result.result);
                let completed = NewRunEvent::new(
                    RunEventType::ToolCallCompleted,
                    with(
                        reference(call.id, target),
                        &[("result", result.clone()), ("duration_ms", json!(duration.max(0)))],
                    ),
                );
                (
                    CallState::Completed,
                    (
                        CallOutcome {
                            content: result.to_string(),
                            is_error: false,
                        },
                        vec![completed],
                    ),
                )
            }
            Ok(result) => {
                // The tool's own report is for the model; the timeline says what happened.
                let (_, events) = failed("tool_error", "The tool reported an error.");
                (
                    CallState::Failed,
                    (
                        CallOutcome {
                            content: Value::Object(result.result).to_string(),
                            is_error: true,
                        },
                        events,
                    ),
                )
            }
            Err(CallError::NotSent(_)) => (
                CallState::Failed,
                failed(
                    "tool_unreachable",
                    "The app's server couldn't be reached, so the call wasn't made.",
                ),
            ),
            Err(CallError::Rejected(_)) => (
                CallState::Failed,
                failed("tool_rejected", "The app's server refused the call without acting."),
            ),
            Err(CallError::Withheld) => (
                CallState::Failed,
                failed(
                    "result_withheld",
                    "The app's answer repeated the installation's credential, so it was withheld.",
                ),
            ),
            Err(CallError::Unknown(_)) => (
                CallState::Unknown,
                (
                    CallOutcome {
                        content: UNKNOWN.to_owned(),
                        is_error: true,
                    },
                    vec![failed_event(
                        call.id,
                        target,
                        &Failure::new(
                            "outcome_unknown",
                            "The call was sent, but its outcome is unknown. It wasn't repeated.",
                        ),
                        duration,
                    )],
                ),
            ),
            Err(CallError::Unauthorized) => {
                let failure = reconnect(&target.installation_label);
                let settled = self
                    .settle(
                        call,
                        &from,
                        CallState::Failed,
                        CallOutcome {
                            content: failure.message.clone(),
                            is_error: true,
                        },
                        vec![failed_event(call.id, target, &failure, duration)],
                    )
                    .await;
                if let Step::Stop(outcome) = settled {
                    return Step::Stop(outcome);
                }
                return self.fail(progress, failure).await;
            }
        };
        self.settle(call, &from, to, outcome, events).await
    }

    /// The policy's view of one call to `tool` through `installation`, read
    /// now: the grant, the registry and the organization's level.
    async fn tool_call(
        &self,
        agent_id: AgentId,
        installation: &Installation,
        tool: &str,
    ) -> Result<ToolCall, &'static str> {
        let grants = match self
            .state
            .agents
            .list_grants(self.organization_id, self.workspace_id, agent_id)
            .await
        {
            Ok(grants) => grants,
            Err(zupflow_core::agents::AgentError::Unavailable) => return Err("store_unavailable"),
            Err(_) => Vec::new(),
        };
        let grant = grants
            .iter()
            .find(|grant| grant.installation_id == installation.id && grant.tools.iter().any(|name| name == tool));
        let registered = self.registry(installation).await?;
        let levels = self.levels(installation, &[tool.to_owned()]).await;
        Ok(self.policy_call(
            agent_id,
            installation,
            tool,
            grant.map(|grant| grant.approval_required.iter().any(|name| name == tool)),
            registered.get(tool),
            &levels,
        ))
    }

    fn policy_call(
        &self,
        agent_id: AgentId,
        installation: &Installation,
        tool: &str,
        approval_required: Option<bool>,
        registered: Option<&RegisteredTool>,
        levels: &Levels,
    ) -> ToolCall {
        let (app, level) = match installation.app {
            InstalledApp::Workspace(app_id) => (CallApp::Workspace(app_id), OrganizationLevel::NotApplicable),
            InstalledApp::Catalog(app_id) => (
                CallApp::Catalog(app_id),
                match levels {
                    Levels::Unreadable => OrganizationLevel::Unreadable,
                    Levels::Read(levels) => levels.get(tool).copied().unwrap_or(OrganizationLevel::Unset),
                },
            ),
        };
        ToolCall {
            organization_id: self.organization_id,
            workspace_id: self.workspace_id,
            agent_id,
            installation_id: installation.id,
            app,
            tool: tool.to_owned(),
            status: registered.map(RegisteredTool::status),
            grant: match approval_required {
                None => GrantedAs::NotGranted,
                Some(approval_required) => GrantedAs::Granted { approval_required },
            },
            level,
        }
    }

    /// Every tool in the installation's registry, by name.
    async fn registry(&self, installation: &Installation) -> Result<BTreeMap<String, RegisteredTool>, &'static str> {
        let mut tools = BTreeMap::new();
        let mut after: Option<String> = None;
        loop {
            let page = match self
                .state
                .tools
                .list_tools(
                    self.organization_id,
                    self.workspace_id,
                    installation.id,
                    after.as_deref(),
                    zupflow_core::tools::MAX_PAGE,
                )
                .await
            {
                Ok(page) => page,
                Err(ToolError::NotFound) => return Ok(tools),
                Err(_) => return Err("store_unavailable"),
            };
            for tool in page.items {
                tools.insert(tool.name.clone(), tool);
            }
            match page.next_after {
                Some(next) => after = Some(next),
                None => return Ok(tools),
            }
        }
    }

    /// The organization's levels for a catalog app's tools.
    async fn levels(&self, installation: &Installation, tools: &[String]) -> Levels {
        let InstalledApp::Catalog(app_id) = installation.app else {
            return Levels::Read(HashMap::new());
        };
        match self
            .state
            .policies
            .find_levels(self.organization_id, app_id, tools)
            .await
        {
            Ok(levels) => Levels::Read(
                levels
                    .into_iter()
                    .map(|level| {
                        (
                            level.tool,
                            OrganizationLevel::Set {
                                level: level.level,
                                confirmed: level.confirmed,
                            },
                        )
                    })
                    .collect(),
            ),
            // Fails closed: the floor requires approval.
            Err(_) => Levels::Unreadable,
        }
    }

    /// The app's name, as the timeline shows it.
    async fn app_name(&self, installation: &Installation) -> Result<Option<String>, &'static str> {
        let found = match installation.app {
            InstalledApp::Catalog(app_id) => {
                self.state
                    .apps
                    .find_organization_app(self.organization_id, app_id)
                    .await
            }
            InstalledApp::Workspace(app_id) => {
                self.state
                    .apps
                    .find_workspace_app(self.organization_id, self.workspace_id, app_id)
                    .await
            }
        };
        found
            .map(|app| app.map(|app| app.name))
            .map_err(|_| "store_unavailable")
    }

    /// The tools to offer this turn: the grants' tools that neither the
    /// floor nor the policy refuses.
    async fn offers(&self, agent_id: AgentId) -> Result<Vec<Offer>, &'static str> {
        let grants = match self
            .state
            .agents
            .list_grants(self.organization_id, self.workspace_id, agent_id)
            .await
        {
            Ok(grants) => grants,
            Err(zupflow_core::agents::AgentError::Unavailable) => return Err("store_unavailable"),
            // The agent is gone: its run goes on without tools.
            Err(_) => Vec::new(),
        };
        let mut offers = Vec::new();
        let mut taken = HashSet::new();
        for grant in grants {
            let installation = match self
                .state
                .installations
                .find_installation(self.organization_id, self.workspace_id, grant.installation_id)
                .await
            {
                Ok(Some(installation)) => installation,
                Ok(None) => continue,
                Err(_) => return Err("store_unavailable"),
            };
            let Some(app_name) = self.app_name(&installation).await? else {
                continue;
            };
            let registered = self.registry(&installation).await?;
            let levels = self.levels(&installation, &grant.tools).await;
            for tool in &grant.tools {
                let call = self.policy_call(
                    agent_id,
                    &installation,
                    tool,
                    Some(grant.approval_required.contains(tool)),
                    registered.get(tool),
                    &levels,
                );
                let policy = self.state.approvals.name();
                if floor(&call, policy).is_some_and(|decision| matches!(decision.verdict, Verdict::Deny { .. })) {
                    continue;
                }
                if matches!(self.state.approvals.decide(&call).await.verdict, Verdict::Deny { .. }) {
                    continue;
                }
                let Some(registered) = registered.get(tool) else {
                    continue;
                };
                let name = model_name(tool, &mut taken);
                offers.push(Offer {
                    spec: spec(name.clone(), registered),
                    name,
                    target: CallTarget {
                        installation_id: installation.id,
                        installation_label: installation.label.clone(),
                        app_name: app_name.clone(),
                        tool: tool.clone(),
                    },
                    call,
                });
            }
        }
        Ok(offers)
    }

    /// Floor, then policy.
    async fn decide(&self, call: &ToolCall) -> Decision {
        match floor(call, self.state.approvals.name()) {
            Some(decision) => decision,
            None => self.state.approvals.decide(call).await,
        }
    }

    /// Asks the model for the next turn and records it with its calls decided.
    async fn turn(&self, progress: &RunProgress) -> Step {
        if progress.turns.len() >= MAX_TURNS {
            return self
                .fail(
                    progress,
                    Failure::new(
                        "too_many_turns",
                        format!(
                            "The agent took {MAX_TURNS} turns without finishing. Start a run with a narrower request."
                        ),
                    ),
                )
                .await;
        }
        let Ok(index) = u32::try_from(progress.turns.len()) else {
            return Step::Stop(Outcome::Done);
        };
        let offers = match self.offers(progress.run.agent_id).await {
            Ok(offers) => offers,
            Err(why) => return Step::Stop(Outcome::Retry(why)),
        };
        let specs: Vec<ToolSpec> = offers.iter().map(|offer| offer.spec.clone()).collect();
        let messages = conversation(progress);
        let turn = Turn {
            model: &progress.model,
            instructions: &progress.instructions,
            messages: &messages,
            tools: &specs,
        };
        let completion = match self.state.provider.complete(&turn).await {
            Ok(completion) => completion,
            Err(ProviderError::Unavailable) => return Step::Stop(Outcome::Retry("model_unavailable")),
            Err(ProviderError::NotConfigured) => {
                return self
                    .fail(
                        progress,
                        Failure::new(
                            "model_unavailable",
                            "No model provider is configured for this deployment, so the run can't be answered.",
                        ),
                    )
                    .await;
            }
            Err(ProviderError::UnknownModel) => {
                return self
                    .fail(
                        progress,
                        Failure::new(
                            "model_unavailable",
                            format!(
                                "The model {} isn't available. Choose another model for the agent and start a new run.",
                                progress.model
                            ),
                        ),
                    )
                    .await;
            }
            Err(error) => {
                tracing::error!(event = "runs.model_failed", provider = self.state.provider.name(), organization_id = %self.organization_id, workspace_id = %self.workspace_id, run_id = %self.run_id, %error);
                return self
                    .fail(
                        progress,
                        Failure::new(
                            "model_failed",
                            "The model provider couldn't answer this run. Start it again.",
                        ),
                    )
                    .await;
            }
        };
        if let Err(why) = validate_completion(&turn, &completion) {
            tracing::error!(event = "runs.model_answer_invalid", provider = self.state.provider.name(), run_id = %self.run_id, why);
            return self
                .fail(
                    progress,
                    Failure::new(
                        "model_failed",
                        "The model provider couldn't answer this run. Start it again.",
                    ),
                )
                .await;
        }
        match completion.reply {
            Reply::Text(text) => self.complete(progress, index, text, completion.usage).await,
            Reply::ToolCalls { text, calls } => self.record(index, text, calls, &offers, completion.usage).await,
        }
    }

    /// Records the final answer and ends the run with it, in one change.
    async fn complete(&self, progress: &RunProgress, index: u32, text: String, usage: TokenUsage) -> Step {
        let total = RunUsage {
            input_tokens: progress.run.usage.input_tokens + usage.input_tokens,
            output_tokens: progress.run.usage.output_tokens + usage.output_tokens,
            tool_calls: progress.run.usage.tool_calls,
        };
        let mut events: Vec<NewRunEvent> = (!text.trim().is_empty())
            .then(|| NewRunEvent::new(RunEventType::MessageAssistant, json!({ "text": text })))
            .into_iter()
            .collect();
        events.push(NewRunEvent::new(RunEventType::RunCompleted, json!({ "usage": total })));
        let turn = NewTurn {
            text: Some(text.clone()),
            calls: Vec::new(),
            usage,
            finish: Some(Ending::Completed { output: text }),
        };
        match self
            .state
            .runs
            .record_turn(
                self.organization_id,
                self.workspace_id,
                self.run_id,
                index,
                &turn,
                &events,
                Timestamp::now(),
            )
            .await
        {
            Ok(events) => {
                self.publish(&events).await;
                Step::Stop(Outcome::Done)
            }
            Err(error) => retry(error),
        }
    }

    async fn record(
        &self,
        index: u32,
        text: Option<String>,
        asked: Vec<ModelToolCall>,
        offers: &[Offer],
        usage: TokenUsage,
    ) -> Step {
        let mut events = Vec::new();
        if let Some(text) = text.as_ref().filter(|text| !text.trim().is_empty()) {
            events.push(NewRunEvent::new(
                RunEventType::MessageAssistant,
                json!({ "text": text }),
            ));
        }
        let mut calls = Vec::with_capacity(asked.len());
        for (position, call) in asked.into_iter().enumerate() {
            let id = ToolCallId::generate();
            let offer = offers.iter().find(|offer| offer.name == call.name);
            let refused = |reason: String| CallOutcome {
                content: reason,
                is_error: true,
            };
            let Some(offer) = offer else {
                calls.push(NewToolCall {
                    id,
                    model_call_id: call.id,
                    outcome: Some(refused(format!(
                        "There's no tool named {} available to you.",
                        call.name
                    ))),
                    name: call.name,
                    target: None,
                    arguments: call.arguments,
                    state: CallState::Refused,
                    decision: None,
                });
                continue;
            };
            let (state, decision, outcome) = if position >= MAX_CALLS_PER_TURN {
                let reason = format!("Only {MAX_CALLS_PER_TURN} calls can be made in one turn.");
                events.push(failed_event(
                    id,
                    &offer.target,
                    &Failure::new("tool_refused", reason.clone()),
                    0,
                ));
                (CallState::Refused, None, Some(refused(reason)))
            } else {
                let decision = self.decide(&offer.call).await;
                match &decision.verdict {
                    Verdict::Allow => (CallState::Allowed, Some(recorded(&decision)), None),
                    Verdict::RequireApproval { .. } => (CallState::AwaitingApproval, Some(recorded(&decision)), None),
                    Verdict::Deny { reason } => {
                        events.push(failed_event(
                            id,
                            &offer.target,
                            &Failure::new("tool_refused", reason.clone()),
                            0,
                        ));
                        (
                            CallState::Refused,
                            Some(recorded(&decision)),
                            Some(refused(reason.clone())),
                        )
                    }
                }
            };
            calls.push(NewToolCall {
                id,
                model_call_id: call.id,
                name: call.name,
                target: Some(offer.target.clone()),
                arguments: call.arguments,
                state,
                decision,
                outcome,
            });
        }
        let turn = NewTurn {
            text,
            calls,
            usage,
            finish: None,
        };
        match self
            .state
            .runs
            .record_turn(
                self.organization_id,
                self.workspace_id,
                self.run_id,
                index,
                &turn,
                &events,
                Timestamp::now(),
            )
            .await
        {
            Ok(events) => {
                self.publish(&events).await;
                Step::Next
            }
            Err(error) => retry(error),
        }
    }
}

enum Levels {
    Read(HashMap<String, OrganizationLevel>),
    Unreadable,
}

/// Fails a run whose job ran out of attempts, or that no job takes
/// forward any more, unless it moved on meanwhile.
async fn give_up(engine: &Engine<'_>, why: &'static str) {
    let failure = if why == "model_unavailable" {
        Failure::new("model_unavailable", "The model provider did not respond in time.")
    } else {
        Failure::new("run_unavailable", "The run couldn't continue. Start it again.")
    };
    let progress = match engine
        .state
        .runs
        .find_progress(engine.organization_id, engine.workspace_id, engine.run_id)
        .await
    {
        Ok(Some(progress)) => progress,
        _ => return,
    };
    if matches!(progress.run.status, RunStatus::Queued | RunStatus::Running) {
        let _ = engine.fail(&progress, failure).await;
    }
}

/// How often the worker looks for runs no job takes forward.
const SWEEP: Duration = Duration::from_secs(60);

/// Fails runs whose job died (its last attempt's lease ran out, say), so
/// none stays queued or running forever. How many it found.
pub async fn sweep(state: &AppState) -> usize {
    // Cross-tenant by design, as the claim: each run comes with its scope.
    let abandoned = match state.runs.abandoned_runs(100).await {
        Ok(abandoned) => abandoned,
        Err(error) => {
            tracing::warn!(event = "runs.sweep_failed", %error);
            return 0;
        }
    };
    for (organization_id, workspace_id, run_id) in &abandoned {
        let environment = match state.workspaces.find_workspace(*organization_id, *workspace_id).await {
            Ok(Some(workspace)) => workspace.environment,
            _ => continue,
        };
        tracing::warn!(event = "runs.abandoned", %organization_id, %workspace_id, %run_id);
        let engine = Engine {
            state,
            organization_id: *organization_id,
            workspace_id: *workspace_id,
            environment,
            run_id: *run_id,
            claim: None,
        };
        give_up(&engine, "abandoned").await;
    }
    abandoned.len()
}

/// Claims and works one run's job, if one is due. Whether one was.
pub async fn work_once(state: &AppState) -> bool {
    let Some(kind) = kind() else { return false };
    // Cross-tenant by design: a worker takes the next job of any scope.
    let job = match state.jobs.claim_next(std::slice::from_ref(&kind), LEASE).await {
        Ok(Some(job)) => job,
        Ok(None) => return false,
        Err(error) => {
            tracing::warn!(event = "runs.claim_failed", %error);
            return false;
        }
    };
    handle(state, job).await;
    true
}

async fn handle(state: &AppState, job: Job) {
    let Job {
        id,
        scope,
        payload,
        attempt,
        lease,
        ..
    } = job;
    let run_id = serde_json::from_slice::<Value>(&payload)
        .ok()
        .and_then(|payload| payload.get("run_id")?.as_str().map(str::to_owned))
        .and_then(|id| RunId::parse(&id).ok());
    let (Scope::Workspace(organization_id, workspace_id), Some(run_id)) = (scope, run_id) else {
        tracing::error!(event = "runs.malformed_job", kind = RUN_AGENT);
        if let Ok(reason) = Name::new("malformed") {
            let _ = state.jobs.bury(&scope, id, &lease, &reason).await;
        }
        return;
    };
    let environment = match state.workspaces.find_workspace(organization_id, workspace_id).await {
        Ok(Some(workspace)) => workspace.environment,
        // Gone, with its runs.
        Ok(None) => {
            let _ = state.jobs.complete(&scope, id, &lease).await;
            return;
        }
        Err(_) => {
            if let Ok(reason) = Name::new("store_unavailable") {
                let _ = state
                    .jobs
                    .retry(&scope, id, &lease, Duration::from_secs(5), &reason)
                    .await;
            }
            return;
        }
    };
    let engine = Engine {
        state,
        organization_id,
        workspace_id,
        environment,
        run_id,
        claim: Some((&scope, id, &lease)),
    };
    let finished = match engine.advance().await {
        Outcome::Done => state.jobs.complete(&scope, id, &lease).await,
        Outcome::Lost => return,
        Outcome::Retry(why) if attempt >= ATTEMPTS => {
            give_up(&engine, why).await;
            state.jobs.complete(&scope, id, &lease).await
        }
        Outcome::Retry(why) => match Name::new(why) {
            Ok(reason) => {
                let after = Duration::from_secs(5 * u64::from(attempt.max(1)));
                state.jobs.retry(&scope, id, &lease, after, &reason).await.map(|_| ())
            }
            Err(_) => state.jobs.complete(&scope, id, &lease).await,
        },
    };
    if let Err(error) = finished {
        tracing::warn!(event = "runs.finish_failed", %organization_id, %workspace_id, %run_id, %error);
    }
}

/// Works runs until the process stops.
pub async fn run_worker(state: AppState) {
    let mut swept = tokio::time::Instant::now();
    loop {
        if swept.elapsed() >= SWEEP {
            sweep(&state).await;
            swept = tokio::time::Instant::now();
        }
        if !work_once(&state).await {
            tokio::time::sleep(IDLE).await;
        }
    }
}
