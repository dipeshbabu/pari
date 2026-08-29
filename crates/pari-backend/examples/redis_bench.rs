use std::{
    env,
    error::Error,
    path::PathBuf,
    process::{self, Command},
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use pari_backend::{BackendIndex32, BackendStats, RedisBackend, StorageBackend};
use pari_bench::{
    write_report, BenchmarkConfig, BenchmarkReport, Environment, Metric, MetricDirection,
};
use pari_core::MinHash32;

fn main() -> Result<(), Box<dyn Error>> {
    let url = env::var("PARI_REDIS_URL")?;
    let items = env_usize("PARI_REDIS_BENCH_ITEMS", 2_000)?;
    let queries = env_usize("PARI_REDIS_BENCH_QUERIES", 200)?.min(items);
    let output = env::var_os("PARI_REDIS_BENCH_OUTPUT").map_or_else(
        || PathBuf::from("redis-backend-benchmark.json"),
        PathBuf::from,
    );
    if items == 0 || queries == 0 {
        return Err("benchmark item and query counts must be positive".into());
    }

    let namespace = benchmark_namespace()?;
    let mut backend = RedisBackend::connect(&url, &namespace)?;
    backend.cleanup()?;

    let num_perm = 128;
    let seed = 7;
    let mut index = BackendIndex32::create(backend, 0.8, num_perm, seed, None)?;
    let report_result = run_workload(&mut index, items, queries, num_perm, seed);
    let cleanup_result = index.cleanup();
    let report = report_result?;
    cleanup_result?;
    write_report(&output, &report)?;
    println!("wrote {}", output.display());
    Ok(())
}

struct Measurements {
    build_elapsed: Duration,
    query_elapsed: Duration,
    build_round_trips: u64,
    query_round_trips: u64,
    stats: BackendStats,
}

fn run_workload(
    index: &mut BackendIndex32<RedisBackend>,
    items: usize,
    queries: usize,
    num_perm: usize,
    seed: u64,
) -> Result<BenchmarkReport, Box<dyn Error>> {
    let signatures = (0..items)
        .map(|item| benchmark_signature(item, num_perm, seed))
        .collect::<Result<Vec<_>, _>>()?;

    let baseline_round_trips = index.stats()?.round_trips;
    let build_started = Instant::now();
    index.insert_many(
        signatures
            .iter()
            .enumerate()
            .map(|(key, sketch)| (u64::try_from(key).expect("benchmark key fits u64"), sketch)),
    )?;
    index.flush()?;
    let build_elapsed = build_started.elapsed();
    let build_stats = index.stats()?;
    let build_round_trips = stage_round_trips(baseline_round_trips, build_stats.round_trips);

    let query_started = Instant::now();
    let results = index.query_many(signatures.iter().take(queries))?;
    let query_elapsed = query_started.elapsed();
    let self_matches = results
        .iter()
        .enumerate()
        .filter(|(key, candidates)| u64::try_from(*key).is_ok_and(|key| candidates.contains(&key)))
        .count();
    if self_matches != queries {
        return Err(format!(
            "benchmark self-query recall failed: {self_matches}/{queries} queries returned their key"
        )
        .into());
    }

    let stats = index.stats()?;
    let query_round_trips = stage_round_trips(build_stats.round_trips, stats.round_trips);
    let measurements = Measurements {
        build_elapsed,
        query_elapsed,
        build_round_trips,
        query_round_trips,
        stats,
    };
    build_report(items, queries, num_perm, seed, &measurements)
}

fn build_report(
    items: usize,
    queries: usize,
    num_perm: usize,
    seed: u64,
    measurements: &Measurements,
) -> Result<BenchmarkReport, Box<dyn Error>> {
    let mut report = BenchmarkReport::new(
        "pari-redis",
        SystemTime::now().duration_since(UNIX_EPOCH)?.as_secs(),
        environment(),
        BenchmarkConfig {
            items,
            queries,
            set_size: 3,
            overlap: 3,
            threshold: 0.8,
            num_perm,
            seed,
            dataset: None,
        },
    );
    insert_timing_metrics(&mut report, items, queries, measurements)?;
    insert_round_trip_metrics(&mut report, items, queries, measurements)?;
    report.insert_metric(
        "backend.redis.bucket_memberships",
        Metric::new(
            u64_to_f64(measurements.stats.bucket_memberships)?,
            "memberships",
            MetricDirection::Neutral,
        ),
    );
    report.insert_metric(
        "backend.redis.self_recall",
        Metric::new(1.0, "ratio", MetricDirection::Higher),
    );
    Ok(report)
}

fn insert_timing_metrics(
    report: &mut BenchmarkReport,
    items: usize,
    queries: usize,
    measurements: &Measurements,
) -> Result<(), std::num::TryFromIntError> {
    report.insert_metric(
        "backend.redis.insert_items_per_second",
        Metric::new(
            rate(items, measurements.build_elapsed.as_secs_f64())?,
            "items/s",
            MetricDirection::Higher,
        ),
    );
    report.insert_metric(
        "backend.redis.build_elapsed_ms",
        Metric::new(
            measurements.build_elapsed.as_secs_f64() * 1_000.0,
            "ms",
            MetricDirection::Lower,
        ),
    );
    report.insert_metric(
        "backend.redis.batch_queries_per_second",
        Metric::new(
            rate(queries, measurements.query_elapsed.as_secs_f64())?,
            "queries/s",
            MetricDirection::Higher,
        ),
    );
    report.insert_metric(
        "backend.redis.batch_query_elapsed_ms",
        Metric::new(
            measurements.query_elapsed.as_secs_f64() * 1_000.0,
            "ms",
            MetricDirection::Lower,
        ),
    );
    report.insert_metric(
        "backend.redis.average_query_ms",
        Metric::new(
            measurements.query_elapsed.as_secs_f64() * 1_000.0 / usize_to_f64(queries)?,
            "ms/query",
            MetricDirection::Lower,
        ),
    );
    Ok(())
}

fn insert_round_trip_metrics(
    report: &mut BenchmarkReport,
    items: usize,
    queries: usize,
    measurements: &Measurements,
) -> Result<(), std::num::TryFromIntError> {
    report.insert_metric(
        "backend.redis.build_round_trips",
        Metric::new(
            u64_to_f64(measurements.build_round_trips)?,
            "round_trips",
            MetricDirection::Lower,
        ),
    );
    report.insert_metric(
        "backend.redis.query_round_trips",
        Metric::new(
            u64_to_f64(measurements.query_round_trips)?,
            "round_trips",
            MetricDirection::Lower,
        ),
    );
    report.insert_metric(
        "backend.redis.round_trips_per_item",
        Metric::new(
            u64_to_f64(measurements.build_round_trips)? / usize_to_f64(items)?,
            "round_trips/item",
            MetricDirection::Lower,
        ),
    );
    report.insert_metric(
        "backend.redis.round_trips_per_query",
        Metric::new(
            u64_to_f64(measurements.query_round_trips)? / usize_to_f64(queries)?,
            "round_trips/query",
            MetricDirection::Lower,
        ),
    );
    report.insert_metric(
        "backend.redis.total_round_trips",
        Metric::new(
            u64_to_f64(measurements.stats.round_trips)?,
            "round_trips",
            MetricDirection::Lower,
        ),
    );
    Ok(())
}

fn stage_round_trips(before: u64, after: u64) -> u64 {
    // Each stats snapshot performs one round trip of its own. Exclude that
    // observation call so stage counts describe only the measured operation.
    after.saturating_sub(before).saturating_sub(1)
}

fn environment() -> Environment {
    Environment {
        os: std::env::consts::OS.into(),
        arch: std::env::consts::ARCH.into(),
        logical_cpus: std::thread::available_parallelism().map_or(1, std::num::NonZeroUsize::get),
        rustc: command_output("rustc", &["--version"]).unwrap_or_else(|| "unknown".into()),
        git_sha: env::var("PARI_GIT_SHA")
            .ok()
            .filter(|value| !value.is_empty())
            .or_else(|| command_output("git", &["rev-parse", "HEAD"]))
            .unwrap_or_else(|| "unknown".into()),
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

fn env_usize(name: &str, default: usize) -> Result<usize, Box<dyn Error>> {
    match env::var(name) {
        Ok(value) => Ok(value.parse::<usize>()?),
        Err(env::VarError::NotPresent) => Ok(default),
        Err(error) => Err(error.into()),
    }
}

fn benchmark_namespace() -> Result<String, Box<dyn Error>> {
    let nanos = SystemTime::now().duration_since(UNIX_EPOCH)?.as_nanos();
    Ok(format!("bench-{}-{nanos}", process::id()))
}

fn benchmark_signature(
    item: usize,
    num_perm: usize,
    seed: u64,
) -> Result<MinHash32, pari_core::MinHashError> {
    let item = u64::try_from(item).expect("benchmark item fits u64");
    let cluster = item / 4;
    let mut sketch = MinHash32::new(num_perm, seed)?;
    sketch.update(&cluster.to_le_bytes());
    sketch.update(&(cluster.wrapping_mul(31)).to_le_bytes());
    sketch.update(&item.to_le_bytes());
    Ok(sketch)
}

fn rate(count: usize, seconds: f64) -> Result<f64, std::num::TryFromIntError> {
    if seconds <= 0.0 {
        return Ok(0.0);
    }
    let count = u32::try_from(count)?;
    Ok(f64::from(count) / seconds)
}

fn usize_to_f64(value: usize) -> Result<f64, std::num::TryFromIntError> {
    Ok(f64::from(u32::try_from(value)?))
}

fn u64_to_f64(value: u64) -> Result<f64, std::num::TryFromIntError> {
    Ok(f64::from(u32::try_from(value)?))
}

#[cfg(test)]
mod tests {
    use super::{rate, stage_round_trips};

    #[test]
    fn observation_round_trip_is_excluded_from_stage_count() {
        assert_eq!(stage_round_trips(4, 9), 4);
        assert_eq!(stage_round_trips(9, 9), 0);
    }

    #[test]
    fn rate_handles_elapsed_time() {
        assert_eq!(rate(10, 2.0).expect("rate"), 5.0);
        assert_eq!(rate(10, 0.0).expect("zero rate"), 0.0);
    }
}
