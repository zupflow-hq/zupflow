//! Metrics: the counts and timings an operator watches the run engine by
//! (the operations standard): runs started, waiting, completed, failed and
//! cancelled; tool calls by how their send came back, with its latency;
//! calls refused at send or settled unknown; and model attempts abandoned,
//! whose tokens may have been spent (decision 0046 keeps them internal: in
//! the store, the logs and here).
//!
//! **Best effort.** Recording never fails its caller and never makes it wait
//! on I/O: an adapter that exports somewhere buffers and sends on its own
//! time, and drops what it can't keep. Nothing depends on a measurement for
//! correctness; the run store is the record.
//!
//! **Scoped.** Every measurement is recorded for the run's organization and
//! workspace, with the workspace's environment. An adapter chooses how much
//! of that it keeps as labels (an exporter may keep only the environment, to
//! bound its series), but never moves a measurement into another scope.
//!
//! **No content.** A measurement carries fixed names, stable codes (a
//! failure's `code`), outcome kinds and durations: never a prompt, arguments,
//! a tool's result, a message or a credential.

use std::time::Duration;

use async_trait::async_trait;

use crate::environment::Environment;
use crate::ids::{OrganizationId, WorkspaceId};

/// How a sent call came back to its sender, as the outbound path reported
/// it: the label of [`Measurement::ToolCall`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum ToolCallOutcome {
    /// The tool answered.
    Completed,
    /// The tool answered with an error of its own.
    ToolError,
    /// It couldn't be sent.
    NotSent,
    /// The app's server refused it without acting.
    Rejected,
    /// The answer repeated the installation's credential, so it was withheld.
    Withheld,
    /// Sent, and whether it acted is unknown.
    Unknown,
    /// Refused for the credential even after one refresh.
    Unauthorized,
}

impl ToolCallOutcome {
    pub const ALL: [Self; 7] = [
        Self::Completed,
        Self::ToolError,
        Self::NotSent,
        Self::Rejected,
        Self::Withheld,
        Self::Unknown,
        Self::Unauthorized,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Completed => "completed",
            Self::ToolError => "tool_error",
            Self::NotSent => "not_sent",
            Self::Rejected => "rejected",
            Self::Withheld => "withheld",
            Self::Unknown => "unknown",
            Self::Unauthorized => "unauthorized",
        }
    }

    /// Anything but [`ToolCallOutcome::Completed`]: a failure to watch.
    pub fn is_failure(self) -> bool {
        self != Self::Completed
    }
}

/// One thing that happened, to be counted (or timed).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Measurement {
    /// A worker took a run for the first time: `run.started`.
    RunStarted,
    /// A run parked, waiting for a person's decision: `approval.requested`.
    RunWaiting,
    RunCompleted,
    /// With the run's error code.
    RunFailed {
        code: String,
    },
    RunCancelled,
    /// A call's send came back to its sender, after `latency`.
    ToolCall {
        outcome: ToolCallOutcome,
        latency: Duration,
    },
    /// A call refused, never sent, with the failure's code: when its turn
    /// was committed (the floor or the policy denied it, or its name wasn't
    /// offered), when it would have been sent, or while it waited for
    /// approval on an installation that went.
    ToolCallRefused {
        code: String,
    },
    /// A call whose send lease lapsed with no outcome, settled `unknown`.
    ToolCallSettledUnknown,
    /// A prepared model attempt whose answer will never be committed.
    ModelAttemptAbandoned,
}

impl Measurement {
    /// The measurement's stable name.
    pub fn name(&self) -> &'static str {
        match self {
            Self::RunStarted => "runs_started",
            Self::RunWaiting => "runs_waiting",
            Self::RunCompleted => "runs_completed",
            Self::RunFailed { .. } => "runs_failed",
            Self::RunCancelled => "runs_cancelled",
            Self::ToolCall { .. } => "tool_calls",
            Self::ToolCallRefused { .. } => "tool_calls_refused",
            Self::ToolCallSettledUnknown => "tool_calls_settled_unknown",
            Self::ModelAttemptAbandoned => "model_attempts_abandoned",
        }
    }

    /// The one label it carries, if any: a code or an outcome kind.
    pub fn label(&self) -> Option<&str> {
        match self {
            Self::RunFailed { code } | Self::ToolCallRefused { code } => Some(code),
            Self::ToolCall { outcome, .. } => Some(outcome.as_str()),
            _ => None,
        }
    }

    /// How long it took, for a timed measurement.
    pub fn latency(&self) -> Option<Duration> {
        match self {
            Self::ToolCall { latency, .. } => Some(*latency),
            _ => None,
        }
    }
}

/// Records measurements. Implementations pass the suite in
/// `zupflow-conformance`.
#[async_trait]
pub trait Metrics: Send + Sync {
    /// Records one measurement in a workspace of an organization, with the
    /// workspace's environment. Never fails and never waits on I/O.
    async fn record(
        &self,
        organization_id: OrganizationId,
        workspace_id: WorkspaceId,
        environment: Environment,
        measurement: Measurement,
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_and_labels_are_stable_and_carry_no_content() {
        let every = [
            Measurement::RunStarted,
            Measurement::RunWaiting,
            Measurement::RunCompleted,
            Measurement::RunFailed {
                code: "model_failed".to_owned(),
            },
            Measurement::RunCancelled,
            Measurement::ToolCall {
                outcome: ToolCallOutcome::Unknown,
                latency: Duration::from_millis(5),
            },
            Measurement::ToolCallRefused {
                code: "installation_removed".to_owned(),
            },
            Measurement::ToolCallSettledUnknown,
            Measurement::ModelAttemptAbandoned,
        ];
        let names: Vec<&str> = every.iter().map(Measurement::name).collect();
        let mut unique = names.clone();
        unique.sort_unstable();
        unique.dedup();
        assert_eq!(unique.len(), names.len(), "one name per kind: {names:?}");
        for measurement in &every {
            let name = measurement.name();
            assert!(
                name.bytes().all(|byte| byte.is_ascii_lowercase() || byte == b'_'),
                "{name}"
            );
        }
        assert_eq!(every[3].label(), Some("model_failed"));
        assert_eq!(every[5].label(), Some("unknown"));
        assert_eq!(every[5].latency(), Some(Duration::from_millis(5)));
        assert_eq!(every[0].label(), None);
    }

    #[test]
    fn only_a_completed_send_is_not_a_failure() {
        let failures: Vec<ToolCallOutcome> = ToolCallOutcome::ALL
            .into_iter()
            .filter(|outcome| outcome.is_failure())
            .collect();
        assert_eq!(failures.len(), ToolCallOutcome::ALL.len() - 1);
        assert!(!ToolCallOutcome::Completed.is_failure());
    }
}
