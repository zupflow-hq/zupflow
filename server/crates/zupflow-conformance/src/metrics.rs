//! The `Metrics` suite: recording takes every measurement the core defines,
//! in every scope and environment, never panics on extreme values, and never
//! makes its caller wait, alone or with many recording at once.
//!
//! An adapter that can read back what it recorded (the in-process one) runs
//! [`run_readable`] too, which also holds it to scope: a measurement counts
//! in its own organization and workspace and nowhere else. An exporter that
//! can't read back runs [`run`] alone.

use std::sync::Arc;
use std::time::{Duration, Instant};

use zupflow_core::environment::Environment;
use zupflow_core::ids::{OrganizationId, WorkspaceId};
use zupflow_core::metrics::{Measurement, Metrics, ToolCallOutcome};

/// How long one `record` may take: it never waits on I/O.
const PROMPT: Duration = Duration::from_millis(250);

/// Reads back how many times a measurement's name, with a label or with
/// any label (`None`), was recorded in one workspace of one organization,
/// in one environment.
pub type Read<'a> = &'a (dyn Fn(OrganizationId, WorkspaceId, Environment, &str, Option<&str>) -> u64 + Sync);

/// Every kind of measurement, each label of a tool call's outcome included.
fn every_measurement() -> Vec<Measurement> {
    let mut every = vec![
        Measurement::RunStarted,
        Measurement::RunWaiting,
        Measurement::RunCompleted,
        Measurement::RunFailed {
            code: "model_failed".to_owned(),
        },
        Measurement::RunCancelled,
        Measurement::ToolCallRefused {
            code: "installation_removed".to_owned(),
        },
        Measurement::ToolCallSettledUnknown,
        Measurement::ModelAttemptAbandoned,
    ];
    for outcome in ToolCallOutcome::ALL {
        every.push(Measurement::ToolCall {
            outcome,
            latency: Duration::from_millis(42),
        });
    }
    every
}

pub async fn run(metrics: Arc<dyn Metrics>) {
    records_every_measurement_promptly(metrics.as_ref()).await;
    takes_extreme_values(metrics.as_ref()).await;
    records_from_many_tasks_at_once(metrics).await;
}

/// [`run`], and the scope rules through `read`.
pub async fn run_readable(metrics: Arc<dyn Metrics>, read: Read<'_>) {
    run(metrics.clone()).await;
    counts_in_its_own_scope_only(metrics.as_ref(), read).await;
    counts_each_label_apart(metrics.as_ref(), read).await;
    counts_each_environment_apart(metrics.as_ref(), read).await;
}

async fn timed(
    metrics: &dyn Metrics,
    organization_id: OrganizationId,
    workspace_id: WorkspaceId,
    environment: Environment,
    measurement: Measurement,
) {
    let what = format!("{measurement:?}");
    let started = Instant::now();
    metrics
        .record(organization_id, workspace_id, environment, measurement)
        .await;
    assert!(
        started.elapsed() < PROMPT,
        "recording {what} kept its caller waiting {:?}",
        started.elapsed()
    );
}

async fn records_every_measurement_promptly(metrics: &dyn Metrics) {
    for environment in [Environment::Sandbox, Environment::Production] {
        for _ in 0..2 {
            let organization_id = OrganizationId::generate();
            for _ in 0..2 {
                let workspace_id = WorkspaceId::generate();
                for measurement in every_measurement() {
                    timed(metrics, organization_id, workspace_id, environment, measurement).await;
                }
            }
        }
    }
}

async fn takes_extreme_values(metrics: &dyn Metrics) {
    let (organization_id, workspace_id) = (OrganizationId::generate(), WorkspaceId::generate());
    for latency in [Duration::ZERO, Duration::from_nanos(1), Duration::MAX] {
        timed(
            metrics,
            organization_id,
            workspace_id,
            Environment::Sandbox,
            Measurement::ToolCall {
                outcome: ToolCallOutcome::Completed,
                latency,
            },
        )
        .await;
    }
    // A failure's code is short and stable, but an adapter mustn't trust that.
    for code in [String::new(), "x".repeat(4096)] {
        timed(
            metrics,
            organization_id,
            workspace_id,
            Environment::Production,
            Measurement::RunFailed { code },
        )
        .await;
    }
}

async fn records_from_many_tasks_at_once(metrics: Arc<dyn Metrics>) {
    let organization_id = OrganizationId::generate();
    let started = Instant::now();
    let mut tasks = tokio::task::JoinSet::new();
    for _ in 0..16 {
        let metrics = metrics.clone();
        tasks.spawn(async move {
            let workspace_id = WorkspaceId::generate();
            for _ in 0..100 {
                metrics
                    .record(
                        organization_id,
                        workspace_id,
                        Environment::Sandbox,
                        Measurement::RunStarted,
                    )
                    .await;
            }
        });
    }
    while let Some(joined) = tasks.join_next().await {
        joined.expect("recording doesn't panic");
    }
    assert!(
        started.elapsed() < Duration::from_secs(10),
        "1,600 measurements from 16 tasks took {:?}",
        started.elapsed()
    );
}

async fn counts_in_its_own_scope_only(metrics: &dyn Metrics, read: Read<'_>) {
    let (org_a, org_b) = (OrganizationId::generate(), OrganizationId::generate());
    let (ws_a1, ws_a2, ws_b1) = (
        WorkspaceId::generate(),
        WorkspaceId::generate(),
        WorkspaceId::generate(),
    );
    for _ in 0..3 {
        metrics
            .record(org_a, ws_a1, Environment::Sandbox, Measurement::RunCompleted)
            .await;
    }
    assert_eq!(
        read(org_a, ws_a1, Environment::Sandbox, "runs_completed", None),
        3,
        "counted where recorded"
    );
    assert_eq!(
        read(org_a, ws_a2, Environment::Sandbox, "runs_completed", None),
        0,
        "never in a sibling workspace"
    );
    assert_eq!(
        read(org_b, ws_b1, Environment::Sandbox, "runs_completed", None),
        0,
        "never in another organization"
    );
    assert_eq!(
        read(org_b, ws_a1, Environment::Sandbox, "runs_completed", None),
        0,
        "never under another organization's ID for the same workspace ID"
    );
    metrics
        .record(org_b, ws_b1, Environment::Production, Measurement::RunCompleted)
        .await;
    assert_eq!(
        read(org_a, ws_a1, Environment::Sandbox, "runs_completed", None),
        3,
        "another's measurement changes nothing here"
    );
    assert_eq!(read(org_b, ws_b1, Environment::Production, "runs_completed", None), 1);
    assert_eq!(
        read(org_b, ws_b1, Environment::Sandbox, "runs_completed", None),
        0,
        "recorded in Production"
    );
}

async fn counts_each_label_apart(metrics: &dyn Metrics, read: Read<'_>) {
    let (organization_id, workspace_id) = (OrganizationId::generate(), WorkspaceId::generate());
    for code in ["model_failed", "model_failed", "run_unavailable"] {
        metrics
            .record(
                organization_id,
                workspace_id,
                Environment::Sandbox,
                Measurement::RunFailed { code: code.to_owned() },
            )
            .await;
    }
    assert_eq!(
        read(
            organization_id,
            workspace_id,
            Environment::Sandbox,
            "runs_failed",
            Some("model_failed")
        ),
        2
    );
    assert_eq!(
        read(
            organization_id,
            workspace_id,
            Environment::Sandbox,
            "runs_failed",
            Some("run_unavailable")
        ),
        1
    );
    assert_eq!(
        read(organization_id, workspace_id, Environment::Sandbox, "runs_failed", None),
        3
    );
    assert_eq!(
        read(
            organization_id,
            workspace_id,
            Environment::Sandbox,
            "runs_completed",
            None
        ),
        0
    );
}

/// An adapter keeps the environment it's told: Sandbox and Production
/// figures never merge, even under one workspace's ID.
async fn counts_each_environment_apart(metrics: &dyn Metrics, read: Read<'_>) {
    let (organization_id, workspace_id) = (OrganizationId::generate(), WorkspaceId::generate());
    for environment in [Environment::Sandbox, Environment::Sandbox, Environment::Production] {
        metrics
            .record(organization_id, workspace_id, environment, Measurement::RunStarted)
            .await;
    }
    assert_eq!(
        read(
            organization_id,
            workspace_id,
            Environment::Sandbox,
            "runs_started",
            None
        ),
        2
    );
    assert_eq!(
        read(
            organization_id,
            workspace_id,
            Environment::Production,
            "runs_started",
            None
        ),
        1
    );
}
