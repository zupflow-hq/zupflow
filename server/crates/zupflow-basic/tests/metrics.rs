//! The in-process metrics pass the `Metrics` suite, keep latencies in their
//! histogram, and drop what would open a series past their bound.

use std::sync::Arc;
use std::time::Duration;

use zupflow_basic::in_process_metrics::{InProcessMetrics, LATENCY_BUCKETS_MS};
use zupflow_core::environment::Environment;
use zupflow_core::ids::{OrganizationId, WorkspaceId};
use zupflow_core::metrics::{Measurement, Metrics, ToolCallOutcome};

#[tokio::test]
async fn conforms() {
    let metrics = Arc::new(InProcessMetrics::default());
    let reader = metrics.clone();
    zupflow_conformance::metrics::run_readable(
        metrics,
        &move |organization_id, workspace_id, environment, name, label| {
            reader.count_in(organization_id, workspace_id, environment, name, label)
        },
    )
    .await;
}

#[tokio::test]
async fn keeps_latencies_in_buckets() {
    let metrics = InProcessMetrics::default();
    let (organization_id, workspace_id) = (OrganizationId::generate(), WorkspaceId::generate());
    for millis in [3, 10, 11, 700, 60_000] {
        metrics
            .record(
                organization_id,
                workspace_id,
                Environment::Sandbox,
                Measurement::ToolCall {
                    outcome: ToolCallOutcome::Completed,
                    latency: Duration::from_millis(millis),
                },
            )
            .await;
    }
    let figures = metrics
        .figures(
            organization_id,
            workspace_id,
            Environment::Sandbox,
            "tool_calls",
            Some("completed"),
        )
        .expect("recorded");
    assert_eq!(figures.count, 5);
    assert_eq!(figures.latency_ms_total, 3 + 10 + 11 + 700 + 60_000);
    let mut expected = [0u64; LATENCY_BUCKETS_MS.len() + 1];
    expected[0] = 2; // 3 and 10: up to 10 ms
    expected[1] = 1; // 11: up to 50 ms
    expected[5] = 1; // 700: up to 1 s
    expected[LATENCY_BUCKETS_MS.len()] = 1; // above 10 s
    assert_eq!(figures.latency_buckets, expected);
    assert_eq!(
        metrics.count(organization_id, workspace_id, "tool_calls", Some("unknown")),
        0
    );
    assert_eq!(
        metrics.figures(
            organization_id,
            workspace_id,
            Environment::Production,
            "tool_calls",
            Some("completed")
        ),
        None,
        "recorded in Sandbox only"
    );
}

#[tokio::test]
async fn drops_a_series_past_its_bound_rather_than_recording_it_elsewhere() {
    let metrics = InProcessMetrics::new(2);
    let organization_id = OrganizationId::generate();
    let (first, second, third) = (
        WorkspaceId::generate(),
        WorkspaceId::generate(),
        WorkspaceId::generate(),
    );
    for workspace_id in [first, second, third, first] {
        metrics
            .record(
                organization_id,
                workspace_id,
                Environment::Production,
                Measurement::RunStarted,
            )
            .await;
    }
    assert_eq!(
        metrics.count(organization_id, first, "runs_started", None),
        2,
        "a kept series still counts"
    );
    assert_eq!(metrics.count(organization_id, second, "runs_started", None), 1);
    assert_eq!(
        metrics.count(organization_id, third, "runs_started", None),
        0,
        "dropped"
    );
    assert_eq!(metrics.dropped(), 1);
    assert_eq!(metrics.snapshot().len(), 2);
}

#[tokio::test]
async fn keeps_each_environment_s_figures_apart() {
    let metrics = InProcessMetrics::default();
    let organization_id = OrganizationId::generate();
    let (sandbox, production) = (WorkspaceId::generate(), WorkspaceId::generate());
    metrics
        .record(
            organization_id,
            sandbox,
            Environment::Sandbox,
            Measurement::RunCompleted,
        )
        .await;
    for _ in 0..2 {
        metrics
            .record(
                organization_id,
                production,
                Environment::Production,
                Measurement::RunCompleted,
            )
            .await;
    }
    let read = |workspace_id, environment| {
        metrics
            .figures(organization_id, workspace_id, environment, "runs_completed", None)
            .map(|figures| figures.count)
    };
    assert_eq!(read(sandbox, Environment::Sandbox), Some(1));
    assert_eq!(read(production, Environment::Production), Some(2));
    assert_eq!(read(sandbox, Environment::Production), None);
    assert_eq!(read(production, Environment::Sandbox), None);
}
