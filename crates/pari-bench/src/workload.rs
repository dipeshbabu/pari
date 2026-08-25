#![allow(
    clippy::cast_possible_truncation,
    clippy::cast_precision_loss,
    clippy::cast_sign_loss,
    clippy::missing_panics_doc,
    clippy::needless_pass_by_value,
    clippy::too_many_lines
)]

use std::{
    collections::HashSet,
    error::Error,
    fs,
    path::Path,
    process::Command,
    thread,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use pari_core::MinHash32;
use pari_index::{group_pairs, LshIndex32};

use crate::{
    report::{BenchmarkConfig, BenchmarkReport, Environment, Metric, MetricDirection},
    rss::{RssSample, RssSampler},
};

/// Run the configured benchmark workload and return a machine-readable report.
pub fn run_benchmark(config: BenchmarkConfig) -> Result<BenchmarkReport, Box<dyn Error>> {
    validate_config(&config)?;
    let corpus = match &config.dataset {
        Some(path) => load_set_dataset(Path::new(path), config.items)?,
        None => synthetic_corpus(config.items, config.set_size, config.seed),
    };
    if corpus.is_empty() {
        return Err("benchmark corpus is empty".into());
    }
    let queries = build_queries(&corpus, config.queries, config.overlap, config.seed);

    let generated_unix_seconds = SystemTime::now().duration_since(UNIX_EPOCH)?.as_secs();
    let environment = collect_environment();
    let mut report =
        BenchmarkReport::new("pari", generated_unix_seconds, environment, config.clone());

    let signature_sampler = RssSampler::start();
    let signature_started = Instant::now();
    let signatures: Vec<_> = corpus
        .iter()
        .map(|features| build_signature(features, config.num_perm, config.seed))
        .collect::<Result<_, _>>()?;
    let signature_elapsed = signature_started.elapsed();
    let signature_rss = signature_sampler.finish();
    insert_throughput(
        &mut report,
        "signature.items_per_second",
        signatures.len(),
        signature_elapsed,
    );
    insert_elapsed(&mut report, "signature.elapsed_ms", signature_elapsed);
    insert_rss_metrics(&mut report, "signature", signature_rss, signatures.len());

    let query_signature_started = Instant::now();
    let query_signatures: Vec<_> = queries
        .iter()
        .map(|features| build_signature(features, config.num_perm, config.seed))
        .collect::<Result<_, _>>()?;
    let query_signature_elapsed = query_signature_started.elapsed();
    insert_throughput(
        &mut report,
        "query_signature.items_per_second",
        query_signatures.len(),
        query_signature_elapsed,
    );

    let mut index = LshIndex32::new(config.threshold, config.num_perm, config.seed)?;
    let build_sampler = RssSampler::start();
    let build_started = Instant::now();
    index.insert_many(signatures.iter().enumerate().map(|(key, signature)| {
        (
            u64::try_from(key).expect("validated item count fits u64"),
            signature,
        )
    }))?;
    let build_elapsed = build_started.elapsed();
    let build_rss = build_sampler.finish();
    insert_throughput(
        &mut report,
        "index.build_items_per_second",
        signatures.len(),
        build_elapsed,
    );
    insert_elapsed(&mut report, "index.build_elapsed_ms", build_elapsed);
    insert_rss_metrics(&mut report, "index_build", build_rss, signatures.len());
    report.insert_metric(
        "index.live_items",
        Metric::new(index.len() as f64, "items", MetricDirection::Neutral),
    );

    let mut scalar_latencies = Vec::with_capacity(query_signatures.len());
    let mut scalar_results = Vec::with_capacity(query_signatures.len());
    let scalar_started = Instant::now();
    for signature in &query_signatures {
        let query_started = Instant::now();
        let candidates = index.query(signature)?;
        scalar_latencies.push(query_started.elapsed());
        scalar_results.push(candidates);
    }
    let scalar_elapsed = scalar_started.elapsed();
    insert_throughput(
        &mut report,
        "query.scalar_queries_per_second",
        query_signatures.len(),
        scalar_elapsed,
    );
    insert_latency_percentiles(&mut report, &scalar_latencies);

    let batch_started = Instant::now();
    let batch_results = index.query_many(query_signatures.iter())?;
    let batch_elapsed = batch_started.elapsed();
    insert_throughput(
        &mut report,
        "query.batch_queries_per_second",
        query_signatures.len(),
        batch_elapsed,
    );
    if batch_results != scalar_results {
        return Err("scalar and batch query results diverged".into());
    }

    let correctness_started = Instant::now();
    let correctness = candidate_correctness(&corpus, &queries, &scalar_results, config.threshold);
    let correctness_elapsed = correctness_started.elapsed();
    report.insert_metric(
        "candidate.recall",
        Metric::new(correctness.recall, "ratio", MetricDirection::Higher),
    );
    report.insert_metric(
        "candidate.precision",
        Metric::new(correctness.precision, "ratio", MetricDirection::Higher),
    );
    report.insert_metric(
        "candidate.average_candidates",
        Metric::new(
            correctness.average_candidates,
            "items",
            MetricDirection::Lower,
        ),
    );
    report.insert_metric(
        "candidate.exact_matches",
        Metric::new(
            correctness.exact_matches as f64,
            "pairs",
            MetricDirection::Neutral,
        ),
    );
    insert_elapsed(
        &mut report,
        "candidate.ground_truth_elapsed_ms",
        correctness_elapsed,
    );

    let mutation_count = signatures.len().div_ceil(100).clamp(1, 1_000);
    let mutation_started = Instant::now();
    for key in 0..mutation_count {
        let key = u64::try_from(key).expect("mutation key fits u64");
        if !index.remove(key) {
            return Err(format!("failed to remove benchmark key {key}").into());
        }
    }
    index.insert_many(signatures.iter().take(mutation_count).enumerate().map(
        |(key, signature)| {
            (
                u64::try_from(key).expect("mutation key fits u64"),
                signature,
            )
        },
    ))?;
    let mutation_elapsed = mutation_started.elapsed();
    insert_throughput(
        &mut report,
        "index.mutation_operations_per_second",
        mutation_count.saturating_mul(2),
        mutation_elapsed,
    );

    let grouping_started = Instant::now();
    let groups = index.duplicate_groups();
    let grouping_elapsed = grouping_started.elapsed();
    insert_throughput(
        &mut report,
        "grouping.index_items_per_second",
        index.len(),
        grouping_elapsed,
    );
    report.insert_metric(
        "grouping.index_group_count",
        Metric::new(groups.len() as f64, "groups", MetricDirection::Neutral),
    );

    let edge_count = corpus.len().saturating_mul(4).max(1);
    let pair_grouping_started = Instant::now();
    let pair_groups = group_pairs(
        (0..edge_count).map(|edge| {
            let left = u64::try_from(edge).expect("benchmark edge fits u64");
            (left, left + 1)
        }),
        2,
    );
    let pair_grouping_elapsed = pair_grouping_started.elapsed();
    insert_throughput(
        &mut report,
        "grouping.stream_edges_per_second",
        edge_count,
        pair_grouping_elapsed,
    );
    report.insert_metric(
        "grouping.stream_group_count",
        Metric::new(pair_groups.len() as f64, "groups", MetricDirection::Neutral),
    );

    Ok(report)
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

#[derive(Debug, Clone, Copy)]
struct CandidateCorrectness {
    recall: f64,
    precision: f64,
    average_candidates: f64,
    exact_matches: usize,
}

fn candidate_correctness(
    corpus: &[Vec<u64>],
    queries: &[Vec<u64>],
    results: &[Vec<u64>],
    threshold: f64,
) -> CandidateCorrectness {
    let mut exact_matches = 0_usize;
    let mut found_exact = 0_usize;
    let mut total_candidates = 0_usize;
    let mut exact_candidates = 0_usize;

    for (query, candidates) in queries.iter().zip(results) {
        let candidate_keys: HashSet<u64> = candidates.iter().copied().collect();
        total_candidates += candidates.len();
        for (key, item) in corpus.iter().enumerate() {
            if exact_jaccard(query, item) + f64::EPSILON < threshold {
                continue;
            }
            exact_matches += 1;
            let key = u64::try_from(key).expect("corpus key fits u64");
            if candidate_keys.contains(&key) {
                found_exact += 1;
            }
        }
        for key in candidates {
            let Ok(key) = usize::try_from(*key) else {
                continue;
            };
            if let Some(item) = corpus.get(key) {
                if exact_jaccard(query, item) + f64::EPSILON >= threshold {
                    exact_candidates += 1;
                }
            }
        }
    }

    CandidateCorrectness {
        recall: ratio(found_exact, exact_matches),
        precision: ratio(exact_candidates, total_candidates),
        average_candidates: ratio(total_candidates, queries.len()),
        exact_matches,
    }
}

fn exact_jaccard(left: &[u64], right: &[u64]) -> f64 {
    let mut left_index = 0;
    let mut right_index = 0;
    let mut intersection = 0_usize;
    while left_index < left.len() && right_index < right.len() {
        match left[left_index].cmp(&right[right_index]) {
            std::cmp::Ordering::Less => left_index += 1,
            std::cmp::Ordering::Greater => right_index += 1,
            std::cmp::Ordering::Equal => {
                intersection += 1;
                left_index += 1;
                right_index += 1;
            }
        }
    }
    let union = left.len() + right.len() - intersection;
    ratio(intersection, union)
}

fn insert_latency_percentiles(report: &mut BenchmarkReport, latencies: &[Duration]) {
    let mut milliseconds: Vec<_> = latencies
        .iter()
        .map(|duration| duration.as_secs_f64() * 1_000.0)
        .collect();
    milliseconds.sort_by(f64::total_cmp);
    for (name, percentile) in [("p50", 0.50), ("p95", 0.95), ("p99", 0.99)] {
        report.insert_metric(
            format!("query.scalar_{name}_ms"),
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

fn insert_throughput(report: &mut BenchmarkReport, name: &str, count: usize, elapsed: Duration) {
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

fn insert_rss_metrics(report: &mut BenchmarkReport, prefix: &str, rss: RssSample, items: usize) {
    if let Some(peak) = rss.peak_bytes {
        report.insert_metric(
            format!("memory.{prefix}_peak_rss_bytes"),
            Metric::new(peak as f64, "bytes", MetricDirection::Lower),
        );
    }
    if let (Some(before), Some(after)) = (rss.before_bytes, rss.after_bytes) {
        let delta = after.saturating_sub(before);
        report.insert_metric(
            format!("memory.{prefix}_rss_delta_bytes"),
            Metric::new(delta as f64, "bytes", MetricDirection::Lower),
        );
        if items > 0 {
            report.insert_metric(
                format!("memory.{prefix}_rss_delta_bytes_per_item"),
                Metric::new(
                    delta as f64 / items as f64,
                    "bytes/item",
                    MetricDirection::Lower,
                ),
            );
        }
    }
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

fn ratio(numerator: usize, denominator: usize) -> f64 {
    if denominator == 0 {
        return if numerator == 0 { 1.0 } else { 0.0 };
    }
    numerator as f64 / denominator as f64
}

fn mix64(mut value: u64) -> u64 {
    value ^= value >> 30;
    value = value.wrapping_mul(0xBF58_476D_1CE4_E5B9);
    value ^= value >> 27;
    value = value.wrapping_mul(0x94D0_49BB_1331_11EB);
    value ^ (value >> 31)
}

#[cfg(test)]
#[allow(clippy::float_cmp)]
mod tests {
    use super::{
        build_queries, exact_jaccard, load_set_dataset, percentile_value, synthetic_corpus,
    };

    #[test]
    fn synthetic_query_has_expected_overlap() {
        let corpus = synthetic_corpus(4, 100, 7);
        let queries = build_queries(&corpus, 1, 90, 7);
        let similarity = exact_jaccard(&corpus[0], &queries[0]);
        assert!((similarity - (90.0 / 110.0)).abs() < 1e-12);
    }

    #[test]
    fn percentile_uses_nearest_upper_rank() {
        let values = [1.0, 2.0, 3.0, 4.0, 5.0];
        assert_eq!(percentile_value(&values, 0.50), 3.0);
        assert_eq!(percentile_value(&values, 0.95), 5.0);
    }

    #[test]
    fn dataset_parser_sorts_deduplicates_and_limits() {
        let path = std::env::temp_dir().join(format!(
            "pari-bench-dataset-{}-{}.txt",
            std::process::id(),
            std::thread::current().name().unwrap_or("test")
        ));
        std::fs::write(&path, "3 1 1 2\n\n9 8\n7 6\n").expect("write fixture");
        let rows = load_set_dataset(&path, 2).expect("parse dataset");
        let _ = std::fs::remove_file(path);
        assert_eq!(rows, vec![vec![1, 2, 3], vec![8, 9]]);
    }
}
