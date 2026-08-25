#![allow(
    clippy::cast_possible_truncation,
    clippy::cast_precision_loss,
    clippy::cast_sign_loss,
    clippy::too_many_lines
)]

use std::{
    error::Error,
    fs,
    path::{Path, PathBuf},
    process::Command,
    thread,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use pari_core::MinHash32;
use pari_index::LshIndex32;
use pari_store::PersistentIndex32;
use pari_store_build::{build_external, BuildOptions};
use pari_store_lazy::LazyIndex32;

use crate::{
    report::{BenchmarkConfig, BenchmarkReport, Environment, Metric, MetricDirection},
    rss::{RssSample, RssSampler},
};

/// Run a deterministic end-to-end storage benchmark across Pari's in-memory,
/// mutable persistent, bounded builder, and read-only lazy paths.
pub fn run_storage_benchmark(
    config: BenchmarkConfig,
) -> Result<BenchmarkReport, Box<dyn Error>> {
    validate_config(&config)?;
    let corpus = match &config.dataset {
        Some(path) => load_set_dataset(Path::new(path), config.items)?,
        None => synthetic_corpus(config.items, config.set_size, config.seed),
    };
    if corpus.is_empty() {
        return Err("storage benchmark corpus is empty".into());
    }
    let queries = build_queries(&corpus, config.queries, config.overlap, config.seed);
    let signatures = corpus
        .iter()
        .map(|features| build_signature(features, config.num_perm, config.seed))
        .collect::<Result<Vec<_>, _>>()?;
    let query_signatures = queries
        .iter()
        .map(|features| build_signature(features, config.num_perm, config.seed))
        .collect::<Result<Vec<_>, _>>()?;

    let generated_unix_seconds = SystemTime::now().duration_since(UNIX_EPOCH)?.as_secs();
    let mut report = BenchmarkReport::new(
        "pari-storage",
        generated_unix_seconds,
        collect_environment(),
        config.clone(),
    );

    let mut reference = LshIndex32::new(config.threshold, config.num_perm, config.seed)?;
    reference.insert_many(signatures.iter().enumerate().map(|(key, signature)| {
        (
            u64::try_from(key).expect("validated item count fits u64"),
            signature,
        )
    }))?;
    let reference_results = reference.query_many(query_signatures.iter())?;
    drop(reference);

    let artifacts = TemporaryArtifacts::new()?;
    let persistent_path = artifacts.path("persistent.pari");
    let lazy_path = artifacts.path("lazy.pari");

    let persistent_build_rss = RssSampler::start();
    let persistent_build_started = Instant::now();
    let mut persistent = PersistentIndex32::create(
        &persistent_path,
        config.threshold,
        config.num_perm,
        config.seed,
    )?;
    persistent.insert_many(signatures.iter().enumerate().map(|(key, signature)| {
        (
            u64::try_from(key).expect("validated item count fits u64"),
            signature,
        )
    }))?;
    persistent.sync()?;
    let persistent_build_elapsed = persistent_build_started.elapsed();
    let persistent_build_rss = persistent_build_rss.finish();
    insert_throughput(
        &mut report,
        "storage.persistent.build_items_per_second",
        signatures.len(),
        persistent_build_elapsed,
    );
    insert_elapsed(
        &mut report,
        "storage.persistent.build_elapsed_ms",
        persistent_build_elapsed,
    );
    insert_storage_rss(
        &mut report,
        "storage.persistent.build",
        persistent_build_rss,
        signatures.len(),
    );
    let persistent_bytes = fs::metadata(&persistent_path)?.len();
    insert_file_metrics(
        &mut report,
        "storage.persistent",
        persistent_bytes,
        signatures.len(),
    );
    drop(persistent);

    let persistent_reopen_rss = RssSampler::start();
    let persistent_reopen_started = Instant::now();
    let persistent = PersistentIndex32::open(&persistent_path)?;
    let persistent_reopen_elapsed = persistent_reopen_started.elapsed();
    let persistent_reopen_rss = persistent_reopen_rss.finish();
    insert_elapsed(
        &mut report,
        "storage.persistent.reopen_ms",
        persistent_reopen_elapsed,
    );
    insert_storage_rss(
        &mut report,
        "storage.persistent.reopen",
        persistent_reopen_rss,
        signatures.len(),
    );
    let persistent_results = persistent.query_many(query_signatures.iter())?;
    ensure_candidate_parity(
        "PersistentIndex32",
        &reference_results,
        &persistent_results,
    )?;
    let persistent_stats = persistent.stats()?;
    report.insert_metric(
        "storage.persistent.committed_buckets",
        Metric::new(
            persistent_stats.committed_buckets as f64,
            "buckets",
            MetricDirection::Neutral,
        ),
    );
    drop(persistent);

    let builder_rss = RssSampler::start();
    let builder_started = Instant::now();
    let builder_stats = build_external(
        &persistent_path,
        &lazy_path,
        BuildOptions::default(),
    )?;
    let builder_elapsed = builder_started.elapsed();
    let builder_rss = builder_rss.finish();
    insert_throughput(
        &mut report,
        "storage.builder.build_items_per_second",
        signatures.len(),
        builder_elapsed,
    );
    insert_elapsed(
        &mut report,
        "storage.builder.build_elapsed_ms",
        builder_elapsed,
    );
    insert_storage_rss(
        &mut report,
        "storage.builder",
        builder_rss,
        signatures.len(),
    );
    report.insert_metric(
        "storage.builder.records",
        Metric::new(
            builder_stats.records as f64,
            "records",
            MetricDirection::Neutral,
        ),
    );
    report.insert_metric(
        "storage.builder.spill_runs",
        Metric::new(
            builder_stats.spill_runs as f64,
            "runs",
            MetricDirection::Neutral,
        ),
    );
    report.insert_metric(
        "storage.builder.peak_buffered_records",
        Metric::new(
            builder_stats.peak_buffered_records as f64,
            "records",
            MetricDirection::Lower,
        ),
    );
    report.insert_metric(
        "storage.builder.buckets",
        Metric::new(
            builder_stats.buckets as f64,
            "buckets",
            MetricDirection::Neutral,
        ),
    );
    report.insert_metric(
        "storage.builder.output_bytes",
        Metric::new(
            builder_stats.output_bytes as f64,
            "bytes",
            MetricDirection::Lower,
        ),
    );

    let lazy_reopen_rss = RssSampler::start();
    let lazy_reopen_started = Instant::now();
    let mut lazy = LazyIndex32::open(&lazy_path)?;
    let lazy_reopen_elapsed = lazy_reopen_started.elapsed();
    let lazy_reopen_rss = lazy_reopen_rss.finish();
    insert_elapsed(&mut report, "storage.lazy.reopen_ms", lazy_reopen_elapsed);
    insert_storage_rss(
        &mut report,
        "storage.lazy.reopen",
        lazy_reopen_rss,
        signatures.len(),
    );
    let lazy_stats = lazy.stats();
    insert_file_metrics(
        &mut report,
        "storage.lazy",
        lazy_stats.file_bytes,
        signatures.len(),
    );
    report.insert_metric(
        "storage.lazy.directory_buckets",
        Metric::new(
            lazy_stats.buckets as f64,
            "buckets",
            MetricDirection::Neutral,
        ),
    );

    let query_rss = RssSampler::start();
    let scalar_started = Instant::now();
    let mut scalar_latencies = Vec::with_capacity(query_signatures.len());
    let mut lazy_scalar_results = Vec::with_capacity(query_signatures.len());
    for signature in &query_signatures {
        let started = Instant::now();
        lazy_scalar_results.push(lazy.query(signature)?);
        scalar_latencies.push(started.elapsed());
    }
    let scalar_elapsed = scalar_started.elapsed();
    let query_rss = query_rss.finish();
    ensure_candidate_parity(
        "LazyIndex32 scalar",
        &reference_results,
        &lazy_scalar_results,
    )?;
    insert_throughput(
        &mut report,
        "storage.lazy.scalar_queries_per_second",
        query_signatures.len(),
        scalar_elapsed,
    );
    insert_latency_percentiles(&mut report, "storage.lazy.scalar", &scalar_latencies);
    insert_storage_rss(
        &mut report,
        "storage.lazy.query",
        query_rss,
        query_signatures.len(),
    );

    let batch_started = Instant::now();
    let lazy_batch_results = lazy.query_many(query_signatures.iter())?;
    let batch_elapsed = batch_started.elapsed();
    ensure_candidate_parity(
        "LazyIndex32 batch",
        &reference_results,
        &lazy_batch_results,
    )?;
    if lazy_batch_results != lazy_scalar_results {
        return Err("lazy scalar and batch candidate results diverged".into());
    }
    insert_throughput(
        &mut report,
        "storage.lazy.batch_queries_per_second",
        query_signatures.len(),
        batch_elapsed,
    );
    report.insert_metric(
        "storage.candidate_parity",
        Metric::new(1.0, "ratio", MetricDirection::Neutral),
    );

    if let (Some(delta), true) = (
        rss_delta(lazy_reopen_rss),
        lazy_stats.file_bytes > 0,
    ) {
        report.insert_metric(
            "storage.lazy.reopen_rss_to_file_ratio",
            Metric::new(
                delta as f64 / lazy_stats.file_bytes as f64,
                "ratio",
                MetricDirection::Lower,
            ),
        );
    }

    Ok(report)
}

fn ensure_candidate_parity(
    engine: &str,
    reference: &[Vec<u64>],
    actual: &[Vec<u64>],
) -> Result<(), Box<dyn Error>> {
    if reference.len() != actual.len() {
        return Err(format!(
            "{engine} returned {} query rows; expected {}",
            actual.len(),
            reference.len()
        )
        .into());
    }
    for (index, (expected, found)) in reference.iter().zip(actual).enumerate() {
        if expected != found {
            return Err(format!(
                "{engine} candidate mismatch at query {index}: expected {} candidates, got {}",
                expected.len(),
                found.len()
            )
            .into());
        }
    }
    Ok(())
}

fn validate_config(config: &BenchmarkConfig) -> Result<(), Box<dyn Error>> {
    if config.items == 0 {
        return Err("items must be positive".into());
    }
    if config.queries == 0 {
        return Err("queries must be positive".into());
    }
    if config.set_size == 0 && config.dataset.is_none() {
        return Err("set_size must be positive for synthetic workloads".into());
    }
    if config.overlap > config.set_size && config.dataset.is_none() {
        return Err("overlap cannot exceed synthetic set_size".into());
    }
    if !config.threshold.is_finite() || config.threshold <= 0.0 || config.threshold > 1.0 {
        return Err("threshold must be finite and in (0, 1]".into());
    }
    if config.num_perm == 0 || config.num_perm > 4_096 {
        return Err("num_perm must be in 1..=4096 for the benchmark harness".into());
    }
    Ok(())
}

fn build_signature(
    features: &[u64],
    num_perm: usize,
    seed: u64,
) -> Result<MinHash32, pari_core::MinHashError> {
    let mut signature = MinHash32::new(num_perm, seed)?;
    for feature in features {
        signature.update(&feature.to_le_bytes());
    }
    Ok(signature)
}

fn synthetic_corpus(items: usize, set_size: usize, seed: u64) -> Vec<Vec<u64>> {
    let stride = u64::try_from(set_size.saturating_add(1)).unwrap_or(u64::MAX);
    (0..items)
        .map(|item| {
            let item = u64::try_from(item).expect("validated item count fits u64");
            let base = item.saturating_mul(stride);
            (0..set_size)
                .map(|offset| {
                    let offset = u64::try_from(offset).expect("set offset fits u64");
                    mix64(base.wrapping_add(offset).wrapping_add(seed))
                })
                .collect::<Vec<_>>()
        })
        .map(|mut features| {
            features.sort_unstable();
            features.dedup();
            features
        })
        .collect()
}

fn build_queries(
    corpus: &[Vec<u64>],
    query_count: usize,
    overlap: usize,
    seed: u64,
) -> Vec<Vec<u64>> {
    (0..query_count)
        .map(|query_index| {
            let source = &corpus[query_index % corpus.len()];
            let retained = overlap.min(source.len());
            let mut query = source[..retained].to_vec();
            let replacements = source.len().saturating_sub(retained);
            for replacement in 0..replacements {
                let query_index = u64::try_from(query_index).expect("query index fits u64");
                let replacement = u64::try_from(replacement).expect("replacement index fits u64");
                query.push(mix64(
                    0xF000_0000_0000_0000_u64
                        ^ seed
                        ^ query_index.wrapping_mul(0x9E37_79B9)
                        ^ replacement,
                ));
            }
            query.sort_unstable();
            query.dedup();
            query
        })
        .collect()
}

fn load_set_dataset(path: &Path, limit: usize) -> Result<Vec<Vec<u64>>, Box<dyn Error>> {
    let contents = fs::read_to_string(path)?;
    let mut rows = Vec::with_capacity(limit.min(16_384));
    for (line_number, line) in contents.lines().enumerate() {
        if rows.len() >= limit {
            break;
        }
        let mut row = Vec::new();
        for token in line.split_whitespace() {
            let value = token.parse::<u64>().map_err(|error| {
                format!(
                    "failed to parse dataset line {} token {token:?}: {error}",
                    line_number + 1
                )
            })?;
            row.push(value);
        }
        if row.is_empty() {
            continue;
        }
        row.sort_unstable();
        row.dedup();
        rows.push(row);
    }
    Ok(rows)
}

fn insert_throughput(
    report: &mut BenchmarkReport,
    name: &str,
    count: usize,
    elapsed: Duration,
) {
    let seconds = elapsed.as_secs_f64();
    let value = if seconds > 0.0 {
        count as f64 / seconds
    } else {
        0.0
    };
    report.insert_metric(name, Metric::new(value, "items/s", MetricDirection::Higher));
}

fn insert_elapsed(report: &mut BenchmarkReport, name: &str, elapsed: Duration) {
    report.insert_metric(
        name,
        Metric::new(
            elapsed.as_secs_f64() * 1_000.0,
            "ms",
            MetricDirection::Lower,
        ),
    );
}

fn insert_latency_percentiles(
    report: &mut BenchmarkReport,
    prefix: &str,
    latencies: &[Duration],
) {
    let mut milliseconds: Vec<_> = latencies
        .iter()
        .map(|duration| duration.as_secs_f64() * 1_000.0)
        .collect();
    milliseconds.sort_by(f64::total_cmp);
    for (name, percentile) in [("p50_ms", 0.50), ("p95_ms", 0.95), ("p99_ms", 0.99)] {
        report.insert_metric(
            format!("{prefix}.{name}"),
            Metric::new(
                percentile_value(&milliseconds, percentile),
                "ms",
                MetricDirection::Lower,
            ),
        );
    }
}

fn percentile_value(sorted: &[f64], percentile: f64) -> f64 {
    if sorted.is_empty() {
        return 0.0;
    }
    let span = sorted.len().saturating_sub(1);
    let index = (percentile * span as f64).ceil() as usize;
    sorted[index.min(span)]
}

fn insert_file_metrics(report: &mut BenchmarkReport, prefix: &str, bytes: u64, items: usize) {
    report.insert_metric(
        format!("{prefix}.file_bytes"),
        Metric::new(bytes as f64, "bytes", MetricDirection::Lower),
    );
    report.insert_metric(
        format!("{prefix}.bytes_per_item"),
        Metric::new(
            bytes as f64 / items.max(1) as f64,
            "bytes/item",
            MetricDirection::Lower,
        ),
    );
}

fn insert_storage_rss(
    report: &mut BenchmarkReport,
    prefix: &str,
    sample: RssSample,
    items: usize,
) {
    if let Some(peak) = sample.peak_bytes {
        report.insert_metric(
            format!("{prefix}.peak_rss_bytes"),
            Metric::new(peak as f64, "bytes", MetricDirection::Lower),
        );
    }
    if let Some(delta) = rss_delta(sample) {
        report.insert_metric(
            format!("{prefix}.rss_delta_bytes"),
            Metric::new(delta as f64, "bytes", MetricDirection::Lower),
        );
        report.insert_metric(
            format!("{prefix}.rss_delta_bytes_per_item"),
            Metric::new(
                delta as f64 / items.max(1) as f64,
                "bytes/item",
                MetricDirection::Lower,
            ),
        );
    }
}

fn rss_delta(sample: RssSample) -> Option<u64> {
    let before = sample.before_bytes?;
    let peak = sample.peak_bytes?;
    Some(peak.saturating_sub(before))
}

fn collect_environment() -> Environment {
    let rustc = command_output("rustc", &["--version"]).unwrap_or_else(|| "unknown".into());
    let git_sha = std::env::var("PARI_GIT_SHA")
        .ok()
        .filter(|value| !value.is_empty())
        .or_else(|| command_output("git", &["rev-parse", "HEAD"]))
        .unwrap_or_else(|| "unknown".into());
    Environment {
        os: std::env::consts::OS.into(),
        arch: std::env::consts::ARCH.into(),
        logical_cpus: thread::available_parallelism().map_or(1, std::num::NonZeroUsize::get),
        rustc,
        git_sha,
    }
}

fn command_output(command: &str, arguments: &[&str]) -> Option<String> {
    let output = Command::new(command).args(arguments).output().ok()?;
    if !output.status.success() {
        return None;
    }
    String::from_utf8(output.stdout)
        .ok()
        .map(|value| value.trim().to_owned())
        .filter(|value| !value.is_empty())
}

fn mix64(mut value: u64) -> u64 {
    value ^= value >> 30;
    value = value.wrapping_mul(0xBF58_476D_1CE4_E5B9);
    value ^= value >> 27;
    value = value.wrapping_mul(0x94D0_49BB_1331_11EB);
    value ^ (value >> 31)
}

struct TemporaryArtifacts {
    prefix: PathBuf,
}

impl TemporaryArtifacts {
    fn new() -> Result<Self, Box<dyn Error>> {
        let nonce = SystemTime::now().duration_since(UNIX_EPOCH)?.as_nanos();
        Ok(Self {
            prefix: std::env::temp_dir().join(format!(
                "pari-storage-bench-{}-{nonce}",
                std::process::id()
            )),
        })
    }

    fn path(&self, suffix: &str) -> PathBuf {
        self.prefix.with_extension(suffix)
    }
}

impl Drop for TemporaryArtifacts {
    fn drop(&mut self) {
        for suffix in ["persistent.pari", "lazy.pari"] {
            let _ = fs::remove_file(self.path(suffix));
        }
        if let Ok(entries) = fs::read_dir(std::env::temp_dir()) {
            let prefix = self
                .prefix
                .file_name()
                .map(|name| name.to_string_lossy().into_owned())
                .unwrap_or_default();
            for entry in entries.flatten() {
                let name = entry.file_name();
                if name.to_string_lossy().starts_with(&prefix) {
                    let _ = fs::remove_file(entry.path());
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::run_storage_benchmark;
    use crate::BenchmarkConfig;

    #[test]
    fn storage_smoke_reports_parity_and_stable_metrics() {
        let report = run_storage_benchmark(BenchmarkConfig {
            items: 24,
            queries: 4,
            set_size: 20,
            overlap: 18,
            threshold: 0.8,
            num_perm: 32,
            seed: 7,
            dataset: None,
        })
        .expect("storage benchmark");
        assert_eq!(report.metrics["storage.candidate_parity"].value, 1.0);
        for metric in [
            "storage.persistent.reopen_ms",
            "storage.persistent.bytes_per_item",
            "storage.builder.peak_buffered_records",
            "storage.lazy.reopen_ms",
            "storage.lazy.scalar.p50_ms",
            "storage.lazy.scalar.p95_ms",
            "storage.lazy.scalar.p99_ms",
            "storage.lazy.bytes_per_item",
        ] {
            assert!(report.metrics.contains_key(metric), "missing {metric}");
        }
    }
}
