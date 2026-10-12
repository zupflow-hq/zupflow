//! What operators see of runs (the operations standard): one structured log
//! line for every transition a run commits, one for every send that comes
//! back, and the metrics each implies, through [`Metrics`].
//!
//! Every line carries the stable event name, the run's organization,
//! workspace, environment and ID, and what the transition was about: its
//! operation and revisions, the run's status after it, the call, turn or
//! attempt it concerned, a failure's or refusal's code, an outcome's kind,
//! a send's latency. Never what a run carries: its input, the model's text,
//! a call's arguments or result, a denial's reason, a message or a
//! credential (the security standard's "Never log").
//!
//! Levels: `info` for a run's lifecycle and its calls (admitted, parked,
//! decided, sent, recorded, refused, cancelled, finished); `warn` for what
//! is degraded but handled (a call settled unknown, a late outcome kept as
//! evidence, a model that didn't answer, a run the sweep failed, a model
//! attempt abandoned); `debug` for a worker's routine steps (claims, model
//! attempts prepared, turns committed, step-budget yields).
//!
//! [`Metrics`]: zupflow_core::metrics::Metrics

use std::time::Duration;

use tracing::field::display;
use zupflow_core::environment::Environment;
use zupflow_core::ids::{OrganizationId, RunId, ToolCallId, WorkspaceId};
use zupflow_core::metrics::{Measurement, ToolCallOutcome};
use zupflow_core::runs::RunEventType;
use zupflow_core::runs::reducer::SendRequest;
use zupflow_core::runs::store::Receipt;
use zupflow_core::runs::transition::{
    Authorization, Operation, RunEnding, SendOutcome, TransitionRequest, YieldReason,
};

use crate::auth::AppState;
use crate::run_engine;

/// After a transition commits: its events published, its log line written
/// and its measurements recorded.
///
/// A receipt can't say whether it was committed now or answered again to a
/// retry of the same request, so this counts every receipt it's given. None
/// is given twice today: the store answers a receipt again only to a request
/// with the same transition ID and digest, and no caller sends one twice. A
/// worker that commits reads the run again at the next revision, so its next
/// request has another ID; one whose `apply` failed, even after an unclear
/// commit, gives the job back and plans afresh on its next claim; the API's
/// cancels and decisions and the sweep's and refusal job's requests each
/// carry a fresh request ID, and an admission a fresh run ID. A caller that
/// starts retrying a request must tell this apart first.
pub async fn committed(
    state: &AppState,
    organization_id: OrganizationId,
    workspace_id: WorkspaceId,
    environment: Environment,
    request: &TransitionRequest,
    receipt: &Receipt,
) {
    run_engine::publish(state, organization_id, workspace_id, environment, &receipt.events).await;
    log_transition(organization_id, workspace_id, environment, request, receipt);
    for measurement in measurements(request, receipt) {
        state
            .metrics
            .record(organization_id, workspace_id, environment, measurement)
            .await;
    }
}

/// After a send came back to its sender, whatever becomes of its outcome:
/// a log line and the call's measurement, with its latency, in the run's
/// organization and workspace as the driver holds them, never as read off
/// the plan.
// The run's scope comes first and whole, as everywhere: organization,
// workspace, environment and run.
#[allow(clippy::too_many_arguments)]
pub async fn sent(
    state: &AppState,
    organization_id: OrganizationId,
    workspace_id: WorkspaceId,
    environment: Environment,
    run_id: RunId,
    send: &SendRequest,
    outcome: &SendOutcome,
    latency: Duration,
) {
    let kind = outcome_kind(outcome);
    let latency_ms = u64::try_from(latency.as_millis()).unwrap_or(u64::MAX);
    macro_rules! line {
        ($macro:ident) => {
            tracing::$macro!(
                event = "runs.call_answered",
                %organization_id,
                %workspace_id,
                %environment,
                %run_id,
                call_id = %send.call_id,
                attempt = send.attempt,
                installation_id = %send.installation_id,
                outcome = kind.as_str(),
                latency_ms,
            )
        };
    }
    if kind.is_failure() {
        line!(warn);
    } else {
        line!(info);
    }
    state
        .metrics
        .record(
            organization_id,
            workspace_id,
            environment,
            Measurement::ToolCall { outcome: kind, latency },
        )
        .await;
}

/// How a send came back, as a metric's label names it.
pub fn outcome_kind(outcome: &SendOutcome) -> ToolCallOutcome {
    match outcome {
        SendOutcome::Completed { .. } => ToolCallOutcome::Completed,
        SendOutcome::ToolError { .. } => ToolCallOutcome::ToolError,
        SendOutcome::NotSent => ToolCallOutcome::NotSent,
        SendOutcome::Rejected => ToolCallOutcome::Rejected,
        SendOutcome::Withheld => ToolCallOutcome::Withheld,
        SendOutcome::Unknown => ToolCallOutcome::Unknown,
        SendOutcome::Unauthorized => ToolCallOutcome::Unauthorized,
    }
}

/// What a committed transition counts for: read from its events (a run
/// started, parked or ended) and its operation (a call refused or settled
/// unknown, a model attempt abandoned).
pub fn measurements(request: &TransitionRequest, receipt: &Receipt) -> Vec<Measurement> {
    let mut measured = Vec::new();
    for event in &receipt.events {
        match event.kind {
            RunEventType::RunStarted => measured.push(Measurement::RunStarted),
            RunEventType::ApprovalRequested => measured.push(Measurement::RunWaiting),
            RunEventType::RunCompleted => measured.push(Measurement::RunCompleted),
            RunEventType::RunFailed => measured.push(Measurement::RunFailed {
                code: receipt
                    .run
                    .error
                    .as_ref()
                    .map_or_else(|| "unknown".to_owned(), |error| error.code.clone()),
            }),
            RunEventType::RunCancelled => measured.push(Measurement::RunCancelled),
            _ => {}
        }
    }
    let abandons = match request.operation() {
        Operation::PrepareModel(prepare) => prepare.abandons.is_some(),
        Operation::Cancel(cancel) => cancel.abandons.is_some(),
        Operation::Finish(finish) => finish.abandons.is_some(),
        Operation::FailAbandoned(failed) => failed.abandons.is_some(),
        _ => false,
    };
    if abandons {
        measured.push(Measurement::ModelAttemptAbandoned);
    }
    match request.operation() {
        Operation::CommitTurn(commit) => {
            for call in &commit.calls {
                if let Some(failure) = &call.failure {
                    measured.push(Measurement::ToolCallRefused {
                        code: failure.code.clone(),
                    });
                }
            }
        }
        Operation::AuthorizeCall(authorize) => {
            if let Authorization::Refuse { failure, .. } = &authorize.authorization {
                measured.push(Measurement::ToolCallRefused {
                    code: failure.code.clone(),
                });
            }
        }
        Operation::RefuseWaiting(refused) => measured.push(Measurement::ToolCallRefused {
            code: refused.failure.code.clone(),
        }),
        Operation::SettleUnknown(_) => measured.push(Measurement::ToolCallSettledUnknown),
        _ => {}
    }
    measured
}

#[derive(Clone, Copy)]
enum Level {
    Debug,
    Info,
    Warn,
}

/// What one transition is about, beyond the run: only IDs, numbers, codes
/// and kinds.
#[derive(Default)]
struct About {
    call_id: Option<ToolCallId>,
    turn: Option<u32>,
    attempt: Option<u32>,
    /// A failure's or a refusal's code.
    code: Option<String>,
    /// An outcome's kind, a decision, a call's settled state or why a
    /// worker yielded.
    detail: Option<&'static str>,
}

fn about(operation: &Operation) -> (Level, About) {
    match operation {
        Operation::Admit(_) => (Level::Info, About::default()),
        Operation::Claim(_) => (Level::Debug, About::default()),
        Operation::PrepareModel(prepare) => (
            if prepare.abandons.is_some() {
                Level::Warn
            } else {
                Level::Debug
            },
            About {
                turn: Some(prepare.turn),
                attempt: Some(prepare.attempt),
                detail: prepare.abandons.map(|_| "abandoned_an_attempt"),
                ..About::default()
            },
        ),
        Operation::CommitTurn(commit) => (
            if commit.ending.is_some() {
                Level::Info
            } else {
                Level::Debug
            },
            About {
                turn: Some(commit.turn),
                attempt: Some(commit.attempt),
                ..About::default()
            },
        ),
        Operation::Park(park) => (
            Level::Info,
            About {
                call_id: Some(park.call_id),
                ..About::default()
            },
        ),
        Operation::ResolveApproval(resolve) => (
            Level::Info,
            About {
                call_id: Some(resolve.call_id),
                detail: Some(resolve.decision.as_str()),
                ..About::default()
            },
        ),
        Operation::RefuseWaiting(refused) => (
            Level::Info,
            About {
                call_id: Some(refused.call_id),
                code: Some(refused.failure.code.clone()),
                ..About::default()
            },
        ),
        Operation::AuthorizeCall(authorize) => match &authorize.authorization {
            Authorization::Send { attempt, .. } => (
                Level::Info,
                About {
                    call_id: Some(authorize.call_id),
                    attempt: Some(*attempt),
                    detail: Some("send"),
                    ..About::default()
                },
            ),
            Authorization::Refuse { failure, .. } => (
                Level::Info,
                About {
                    call_id: Some(authorize.call_id),
                    code: Some(failure.code.clone()),
                    detail: Some("refuse"),
                    ..About::default()
                },
            ),
        },
        Operation::RecordOutcome(record) => match &record.settle {
            Some(settlement) => (
                Level::Info,
                About {
                    call_id: Some(record.call_id),
                    attempt: Some(record.attempt),
                    code: settlement.failure.as_ref().map(|failure| failure.code.clone()),
                    detail: Some(settlement.state.as_str()),
                    ..About::default()
                },
            ),
            // The call was settled unknown first: kept as late evidence.
            None => (
                Level::Warn,
                About {
                    call_id: Some(record.call_id),
                    attempt: Some(record.attempt),
                    detail: Some("late_evidence"),
                    ..About::default()
                },
            ),
        },
        Operation::SettleUnknown(settle) => (
            Level::Warn,
            About {
                call_id: Some(settle.call_id),
                attempt: Some(settle.attempt),
                code: Some(settle.failure.code.clone()),
                ..About::default()
            },
        ),
        Operation::Cancel(cancel) => (
            Level::Info,
            About {
                detail: Some(cancel.source.as_str()),
                ..About::default()
            },
        ),
        Operation::Yield(yielded) => match yielded.why {
            YieldReason::StepBudget => (
                Level::Debug,
                About {
                    detail: Some("step_budget"),
                    ..About::default()
                },
            ),
            YieldReason::SendInFlight { call_id, attempt } => (
                Level::Debug,
                About {
                    call_id: Some(call_id),
                    attempt: Some(attempt),
                    detail: Some("send_in_flight"),
                    ..About::default()
                },
            ),
            YieldReason::ModelUnavailable { turn, attempt } => (
                Level::Warn,
                About {
                    turn: Some(turn),
                    attempt: Some(attempt),
                    detail: Some("model_unavailable"),
                    ..About::default()
                },
            ),
        },
        Operation::Finish(finish) => (
            Level::Info,
            About {
                code: match &finish.ending {
                    RunEnding::Completed { .. } => None,
                    RunEnding::Failed { error } => Some(error.code.clone()),
                },
                ..About::default()
            },
        ),
        Operation::FailAbandoned(failed) => (
            Level::Warn,
            About {
                code: Some(failed.error.code.clone()),
                ..About::default()
            },
        ),
    }
}

fn log_transition(
    organization_id: OrganizationId,
    workspace_id: WorkspaceId,
    environment: Environment,
    request: &TransitionRequest,
    receipt: &Receipt,
) {
    let (level, about) = about(request.operation());
    let run_id = request.run_id();
    let operation = request.operation().name();
    let status = receipt.status.as_str();
    let call_id = about.call_id.map(display);
    let code = about.code.as_deref();
    macro_rules! line {
        ($macro:ident) => {
            tracing::$macro!(
                event = "runs.transition",
                %organization_id,
                %workspace_id,
                %environment,
                %run_id,
                operation,
                from_revision = receipt.from_revision,
                to_revision = receipt.to_revision,
                status,
                call_id,
                turn = about.turn,
                attempt = about.attempt,
                code,
                detail = about.detail,
            )
        };
    }
    match level {
        Level::Debug => line!(debug),
        Level::Info => line!(info),
        Level::Warn => line!(warn),
    }
}
