#![forbid(unsafe_code)]
//! Reproducible benchmark and comparison utilities for Pari.
//!
//! This crate is not part of the runtime engine. It records workload,
//! environment, correctness, latency, throughput, and memory measurements in a
//! versioned JSON schema so performance-sensitive changes can be compared
//! without embedding benchmark policy into product crates.

mod report;
mod rss;
mod storage;
mod workload;

use std::{error::Error, fs, path::Path};

pub use report::{
    compare_reports, BenchmarkConfig, BenchmarkReport, ComparisonReport, Environment, Metric,
    MetricDelta, MetricDirection, REPORT_SCHEMA_VERSION,
};
pub use storage::run_storage_benchmark;
pub use workload::run_benchmark;

/// Read and validate a benchmark report from JSON.
pub fn read_report(path: &Path) -> Result<BenchmarkReport, Box<dyn Error>> {
    let contents = fs::read_to_string(path)?;
    let report: BenchmarkReport = serde_json::from_str(&contents)?;
    if report.schema_version != REPORT_SCHEMA_VERSION {
        return Err(format!(
            "unsupported benchmark report schema {}; expected {}",
            report.schema_version, REPORT_SCHEMA_VERSION
        )
        .into());
    }
    Ok(report)
}

/// Write a benchmark report as deterministic pretty-printed JSON.
pub fn write_report(path: &Path, report: &BenchmarkReport) -> Result<(), Box<dyn Error>> {
    let contents = serde_json::to_string_pretty(report)?;
    fs::write(path, format!("{contents}\n"))?;
    Ok(())
}

/// Write a comparison report as deterministic pretty-printed JSON.
pub fn write_comparison(path: &Path, comparison: &ComparisonReport) -> Result<(), Box<dyn Error>> {
    let contents = serde_json::to_string_pretty(comparison)?;
    fs::write(path, format!("{contents}\n"))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::{
        read_report, write_report, BenchmarkConfig, BenchmarkReport, Environment,
        REPORT_SCHEMA_VERSION,
    };

    #[test]
    fn report_json_round_trip_preserves_schema() {
        let report = BenchmarkReport::new(
            "pari",
            1,
            Environment {
                os: "test".into(),
                arch: "test".into(),
                logical_cpus: 1,
                rustc: "rustc test".into(),
                git_sha: "abc".into(),
            },
            BenchmarkConfig {
                items: 4,
                queries: 1,
                set_size: 4,
                overlap: 3,
                threshold: 0.8,
                num_perm: 16,
                seed: 1,
                dataset: None,
            },
        );
        let path = temporary_path("round-trip");
        write_report(&path, &report).expect("write report");
        let decoded = read_report(&path).expect("read report");
        let _ = std::fs::remove_file(path);
        assert_eq!(decoded, report);
        assert_eq!(decoded.schema_version, REPORT_SCHEMA_VERSION);
    }

    fn temporary_path(name: &str) -> PathBuf {
        std::env::temp_dir().join(format!("pari-bench-{name}-{}.json", std::process::id()))
    }
}
