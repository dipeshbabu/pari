use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

/// Current machine-readable benchmark report schema.
pub const REPORT_SCHEMA_VERSION: u32 = 1;

/// Whether larger or smaller values are preferable for a metric.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MetricDirection {
    /// Larger values are better, such as throughput or recall.
    Higher,
    /// Smaller values are better, such as latency or memory.
    Lower,
    /// The metric is informational and should not be scored as an improvement.
    Neutral,
}

/// One scalar benchmark measurement.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Metric {
    /// Numeric value in the declared unit.
    pub value: f64,
    /// Stable unit string such as `items/s`, `ms`, `bytes`, or `ratio`.
    pub unit: String,
    /// Optimization direction for comparisons.
    pub direction: MetricDirection,
}

impl Metric {
    /// Construct one metric.
    #[must_use]
    pub fn new(value: f64, unit: impl Into<String>, direction: MetricDirection) -> Self {
        Self {
            value,
            unit: unit.into(),
            direction,
        }
    }
}

/// Environment metadata needed to interpret a benchmark result.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Environment {
    /// Operating system reported by the Rust target.
    pub os: String,
    /// CPU architecture reported by the Rust target.
    pub arch: String,
    /// Available parallelism reported to this process.
    pub logical_cpus: usize,
    /// `rustc --version` when available.
    pub rustc: String,
    /// Git commit SHA when discoverable.
    pub git_sha: String,
}

/// Workload parameters recorded with each run.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct BenchmarkConfig {
    /// Number of corpus items.
    pub items: usize,
    /// Number of queries.
    pub queries: usize,
    /// Number of features in each synthetic set.
    pub set_size: usize,
    /// Number of source features retained in each synthetic query.
    pub overlap: usize,
    /// LSH target threshold.
    pub threshold: f64,
    /// `MinHash` signature length.
    pub num_perm: usize,
    /// Deterministic workload and signature seed.
    pub seed: u64,
    /// Optional real-dataset path used instead of the synthetic corpus.
    pub dataset: Option<String>,
}

/// Versioned benchmark result written by Pari and compatible baseline runners.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct BenchmarkReport {
    /// Report schema version.
    pub schema_version: u32,
    /// Engine name, such as `pari` or `datasketch`.
    pub engine: String,
    /// UNIX timestamp in seconds.
    pub generated_unix_seconds: u64,
    /// Runtime and source revision metadata.
    pub environment: Environment,
    /// Workload configuration.
    pub config: BenchmarkConfig,
    /// Flat stable metric namespace used for cross-run comparisons.
    pub metrics: BTreeMap<String, Metric>,
}

impl BenchmarkReport {
    /// Construct an empty report using the current schema.
    #[must_use]
    pub fn new(
        engine: impl Into<String>,
        generated_unix_seconds: u64,
        environment: Environment,
        config: BenchmarkConfig,
    ) -> Self {
        Self {
            schema_version: REPORT_SCHEMA_VERSION,
            engine: engine.into(),
            generated_unix_seconds,
            environment,
            config,
            metrics: BTreeMap::new(),
        }
    }

    /// Insert or replace one named metric.
    pub fn insert_metric(&mut self, name: impl Into<String>, metric: Metric) {
        self.metrics.insert(name.into(), metric);
    }
}

/// Comparison of one metric shared by two reports.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MetricDelta {
    /// Baseline value.
    pub baseline: f64,
    /// Current value.
    pub current: f64,
    /// Raw percentage change `(current - baseline) / abs(baseline)`.
    pub change_percent: Option<f64>,
    /// Positive means better according to the metric direction.
    pub improvement_percent: Option<f64>,
    /// Unit copied from both compatible metrics.
    pub unit: String,
    /// Optimization direction.
    pub direction: MetricDirection,
}

/// Machine-readable comparison between two compatible benchmark reports.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ComparisonReport {
    /// Comparison schema version, aligned with the benchmark report schema.
    pub schema_version: u32,
    /// Baseline engine label.
    pub baseline_engine: String,
    /// Current engine label.
    pub current_engine: String,
    /// Shared metrics with matching units and directions.
    pub metrics: BTreeMap<String, MetricDelta>,
}

/// Compare all shared metrics with matching units and optimization directions.
#[must_use]
pub fn compare_reports(baseline: &BenchmarkReport, current: &BenchmarkReport) -> ComparisonReport {
    let mut metrics = BTreeMap::new();
    for (name, baseline_metric) in &baseline.metrics {
        let Some(current_metric) = current.metrics.get(name) else {
            continue;
        };
        if baseline_metric.unit != current_metric.unit
            || baseline_metric.direction != current_metric.direction
        {
            continue;
        }

        let change_percent = percent_change(baseline_metric.value, current_metric.value);
        let improvement_percent = match baseline_metric.direction {
            MetricDirection::Higher => change_percent,
            MetricDirection::Lower => change_percent.map(|value| -value),
            MetricDirection::Neutral => None,
        };
        metrics.insert(
            name.clone(),
            MetricDelta {
                baseline: baseline_metric.value,
                current: current_metric.value,
                change_percent,
                improvement_percent,
                unit: baseline_metric.unit.clone(),
                direction: baseline_metric.direction,
            },
        );
    }

    ComparisonReport {
        schema_version: REPORT_SCHEMA_VERSION,
        baseline_engine: baseline.engine.clone(),
        current_engine: current.engine.clone(),
        metrics,
    }
}

fn percent_change(baseline: f64, current: f64) -> Option<f64> {
    if !baseline.is_finite() || !current.is_finite() || baseline == 0.0 {
        return None;
    }
    Some((current - baseline) / baseline.abs() * 100.0)
}

#[cfg(test)]
mod tests {
    use super::{
        compare_reports, BenchmarkConfig, BenchmarkReport, Environment, Metric, MetricDirection,
    };

    fn report(engine: &str) -> BenchmarkReport {
        BenchmarkReport::new(
            engine,
            1,
            Environment {
                os: "test".into(),
                arch: "test".into(),
                logical_cpus: 1,
                rustc: "rustc test".into(),
                git_sha: "abc".into(),
            },
            BenchmarkConfig {
                items: 10,
                queries: 2,
                set_size: 4,
                overlap: 3,
                threshold: 0.8,
                num_perm: 16,
                seed: 1,
                dataset: None,
            },
        )
    }

    #[test]
    fn comparison_respects_metric_direction() {
        let mut baseline = report("baseline");
        baseline.insert_metric(
            "throughput",
            Metric::new(100.0, "items/s", MetricDirection::Higher),
        );
        baseline.insert_metric("latency", Metric::new(10.0, "ms", MetricDirection::Lower));

        let mut current = report("current");
        current.insert_metric(
            "throughput",
            Metric::new(125.0, "items/s", MetricDirection::Higher),
        );
        current.insert_metric("latency", Metric::new(8.0, "ms", MetricDirection::Lower));

        let comparison = compare_reports(&baseline, &current);
        assert_eq!(
            comparison.metrics["throughput"].improvement_percent,
            Some(25.0)
        );
        assert_eq!(
            comparison.metrics["latency"].improvement_percent,
            Some(20.0)
        );
    }

    #[test]
    fn comparison_skips_incompatible_units() {
        let mut baseline = report("baseline");
        baseline.insert_metric("latency", Metric::new(10.0, "ms", MetricDirection::Lower));
        let mut current = report("current");
        current.insert_metric(
            "latency",
            Metric::new(10.0, "seconds", MetricDirection::Lower),
        );
        assert!(compare_reports(&baseline, &current).metrics.is_empty());
    }
}
