//! [`InProcessMetrics`]: the server's measurements, kept in memory. Each
//! series is one measurement's name and label in one workspace (with its
//! organization and environment): a count, and for a timed measurement the
//! total and a histogram of its latencies. The number of series is bounded;
//! a measurement that would open one past the bound is dropped and counted
//! as dropped, never recorded elsewhere. Nothing is exported: a deployment
//! that ships metrics to a collector brings its own `Metrics`.

use std::collections::HashMap;
use std::sync::{Mutex, MutexGuard};
use std::time::Duration;

use async_trait::async_trait;
use zupflow_core::environment::Environment;
use zupflow_core::ids::{OrganizationId, WorkspaceId};
use zupflow_core::metrics::{Measurement, Metrics};

/// The upper bounds of the latency histogram's buckets, in milliseconds;
/// one more bucket takes everything above the last.
pub const LATENCY_BUCKETS_MS: [u64; 9] = [10, 50, 100, 250, 500, 1_000, 2_500, 5_000, 10_000];

/// One series' figures.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Figures {
    pub count: u64,
    /// For a timed measurement: the sum of its latencies, in milliseconds.
    pub latency_ms_total: u64,
    /// For a timed measurement: how many fell in each of
    /// [`LATENCY_BUCKETS_MS`], then above the last.
    pub latency_buckets: [u64; LATENCY_BUCKETS_MS.len() + 1],
}

/// Where a series belongs and what it measures.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct Series {
    pub organization_id: OrganizationId,
    pub workspace_id: WorkspaceId,
    pub environment: Environment,
    pub name: &'static str,
    pub label: Option<String>,
}

struct Kept {
    series: HashMap<Series, Figures>,
    dropped: u64,
}

pub struct InProcessMetrics {
    kept: Mutex<Kept>,
    max_series: usize,
}

impl InProcessMetrics {
    /// Keeps at most `max_series` series (at least one).
    pub fn new(max_series: usize) -> Self {
        Self {
            kept: Mutex::new(Kept {
                series: HashMap::new(),
                dropped: 0,
            }),
            max_series: max_series.max(1),
        }
    }

    fn lock(&self) -> MutexGuard<'_, Kept> {
        // Counters only: after a panic elsewhere they're still usable.
        self.kept.lock().unwrap_or_else(|poisoned| {
            self.kept.clear_poison();
            poisoned.into_inner()
        })
    }

    /// The figures of one series: a measurement's name and label in one
    /// workspace and environment, if it was ever recorded.
    pub fn figures(
        &self,
        organization_id: OrganizationId,
        workspace_id: WorkspaceId,
        environment: Environment,
        name: &'static str,
        label: Option<&str>,
    ) -> Option<Figures> {
        self.lock()
            .series
            .get(&Series {
                organization_id,
                workspace_id,
                environment,
                name,
                label: label.map(str::to_owned),
            })
            .cloned()
    }

    /// How many times `name` (with `label`, or any label when `None`) was
    /// recorded in one workspace, in whichever environment: a workspace has
    /// one, fixed when it's created.
    pub fn count(
        &self,
        organization_id: OrganizationId,
        workspace_id: WorkspaceId,
        name: &str,
        label: Option<&str>,
    ) -> u64 {
        self.lock()
            .series
            .iter()
            .filter(|(series, _)| {
                series.organization_id == organization_id
                    && series.workspace_id == workspace_id
                    && series.name == name
                    && (label.is_none() || series.label.as_deref() == label)
            })
            .map(|(_, figures)| figures.count)
            .sum()
    }

    /// How many times `name` (with `label`, or any label when `None`) was
    /// recorded in one workspace in one environment.
    pub fn count_in(
        &self,
        organization_id: OrganizationId,
        workspace_id: WorkspaceId,
        environment: Environment,
        name: &str,
        label: Option<&str>,
    ) -> u64 {
        self.lock()
            .series
            .iter()
            .filter(|(series, _)| {
                series.organization_id == organization_id
                    && series.workspace_id == workspace_id
                    && series.environment == environment
                    && series.name == name
                    && (label.is_none() || series.label.as_deref() == label)
            })
            .map(|(_, figures)| figures.count)
            .sum()
    }

    /// Every series and its figures, in no particular order.
    pub fn snapshot(&self) -> Vec<(Series, Figures)> {
        self.lock()
            .series
            .iter()
            .map(|(series, figures)| (series.clone(), figures.clone()))
            .collect()
    }

    /// How many measurements were dropped because the series bound was reached.
    pub fn dropped(&self) -> u64 {
        self.lock().dropped
    }
}

impl Default for InProcessMetrics {
    /// 100,000 series: every measurement of thousands of workspaces.
    fn default() -> Self {
        Self::new(100_000)
    }
}

fn bucket(latency: Duration) -> usize {
    let millis = u64::try_from(latency.as_millis()).unwrap_or(u64::MAX);
    LATENCY_BUCKETS_MS
        .iter()
        .position(|bound| millis <= *bound)
        .unwrap_or(LATENCY_BUCKETS_MS.len())
}

#[async_trait]
impl Metrics for InProcessMetrics {
    async fn record(
        &self,
        organization_id: OrganizationId,
        workspace_id: WorkspaceId,
        environment: Environment,
        measurement: Measurement,
    ) {
        let series = Series {
            organization_id,
            workspace_id,
            environment,
            name: measurement.name(),
            label: measurement.label().map(str::to_owned),
        };
        let mut kept = self.lock();
        if !kept.series.contains_key(&series) && kept.series.len() >= self.max_series {
            kept.dropped = kept.dropped.saturating_add(1);
            return;
        }
        let figures = kept.series.entry(series).or_default();
        figures.count = figures.count.saturating_add(1);
        if let Some(latency) = measurement.latency() {
            let millis = u64::try_from(latency.as_millis()).unwrap_or(u64::MAX);
            figures.latency_ms_total = figures.latency_ms_total.saturating_add(millis);
            let at = bucket(latency);
            figures.latency_buckets[at] = figures.latency_buckets[at].saturating_add(1);
        }
    }
}
