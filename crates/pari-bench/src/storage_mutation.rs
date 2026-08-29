use std::{
    error::Error,
    fs,
    path::{Path, PathBuf},
    time::{Instant, SystemTime, UNIX_EPOCH},
};

use pari_core::MinHash32;
use pari_index::LshIndex32;
use pari_store::PersistentIndex32;

use crate::{BenchmarkConfig, BenchmarkReport, Metric, MetricDirection};

const MAX_MUTATION_ITEMS: usize = 256;

pub(crate) fn append_storage_mutation_metrics(
    config: &BenchmarkConfig,
    report: &mut BenchmarkReport,
) -> Result<(), Box<dyn Error>> {
    let item_count = config.items.min(MAX_MUTATION_ITEMS);
    if item_count == 0 {
        return Err("storage mutation benchmark requires at least one item".into());
    }
    let feature_count = config.set_size.max(1);
    let signatures = (0..item_count)
        .map(|item| build_signature(item, feature_count, config.num_perm, config.seed))
        .collect::<Result<Vec<_>, _>>()?;
    let indexed = signatures
        .iter()
        .enumerate()
        .map(|(key, signature)| Ok((u64::try_from(key)?, signature)))
        .collect::<Result<Vec<_>, std::num::TryFromIntError>>()?;

    let artifact = TemporaryIndex::new()?;
    let mut persistent = PersistentIndex32::create(
        artifact.path(),
        config.threshold,
        config.num_perm,
        config.seed,
    )?;
    let mut reference = LshIndex32::new(config.threshold, config.num_perm, config.seed)?;
    persistent.insert_many(indexed.iter().copied())?;
    reference.insert_many(indexed.iter().copied())?;
    persistent.sync()?;

    let mutation_started = Instant::now();
    for (key, signature) in indexed.iter().copied() {
        if !persistent.remove(key) {
            return Err(format!("persistent mutation benchmark could not remove key {key}").into());
        }
        if !reference.remove(key) {
            return Err(format!("reference mutation benchmark could not remove key {key}").into());
        }
        persistent.insert(key, signature)?;
        reference.insert(key, signature)?;
    }
    let mutation_elapsed = mutation_started.elapsed();
    let operations = u32::try_from(item_count * 2)?;
    let mutation_seconds = mutation_elapsed.as_secs_f64();
    let operations_per_second = if mutation_seconds > 0.0 {
        f64::from(operations) / mutation_seconds
    } else {
        0.0
    };
    report.insert_metric(
        "storage.persistent.mutation_operations_per_second",
        Metric::new(operations_per_second, "ops/s", MetricDirection::Higher),
    );

    verify_query_parity(&persistent, &reference, &signatures, config.queries)?;

    let sync_started = Instant::now();
    persistent.sync()?;
    let sync_elapsed = sync_started.elapsed();
    report.insert_metric(
        "storage.persistent.sync_ms",
        Metric::new(
            sync_elapsed.as_secs_f64() * 1_000.0,
            "ms",
            MetricDirection::Lower,
        ),
    );
    drop(persistent);

    let reopened = PersistentIndex32::open(artifact.path())?;
    verify_query_parity(&reopened, &reference, &signatures, config.queries)?;
    report.insert_metric(
        "storage.persistent.mutation_parity",
        Metric::new(1.0, "ratio", MetricDirection::Neutral),
    );
    Ok(())
}

fn verify_query_parity(
    persistent: &PersistentIndex32,
    reference: &LshIndex32,
    signatures: &[MinHash32],
    requested_queries: usize,
) -> Result<(), Box<dyn Error>> {
    let query_count = requested_queries.min(signatures.len()).max(1);
    for (index, signature) in signatures.iter().take(query_count).enumerate() {
        let expected = reference.query(signature)?;
        let actual = persistent.query(signature)?;
        if actual != expected {
            return Err(format!(
                "persistent mutation candidate mismatch at query {index}: expected {} candidates, got {}",
                expected.len(),
                actual.len()
            )
            .into());
        }
    }
    Ok(())
}

fn build_signature(
    item: usize,
    feature_count: usize,
    num_perm: usize,
    seed: u64,
) -> Result<MinHash32, Box<dyn Error>> {
    let item = u64::try_from(item)?;
    let mut sketch = MinHash32::new(num_perm, seed)?;
    let base = item.wrapping_mul(1_000_003);
    for offset in 0..feature_count {
        let offset = u64::try_from(offset)?;
        sketch.update(&base.wrapping_add(offset).to_le_bytes());
    }
    Ok(sketch)
}

struct TemporaryIndex {
    path: PathBuf,
}

impl TemporaryIndex {
    fn new() -> Result<Self, Box<dyn Error>> {
        let nonce = SystemTime::now().duration_since(UNIX_EPOCH)?.as_nanos();
        Ok(Self {
            path: std::env::temp_dir().join(format!(
                "pari-storage-mutation-{}-{nonce}.pari",
                std::process::id()
            )),
        })
    }

    fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for TemporaryIndex {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.path);
        let mut temporary = self.path.as_os_str().to_os_string();
        temporary.push(".tmp");
        let _ = fs::remove_file(PathBuf::from(temporary));
    }
}

#[cfg(test)]
mod tests {
    use super::append_storage_mutation_metrics;
    use crate::{BenchmarkConfig, BenchmarkReport, Environment};

    #[test]
    fn mutation_metrics_include_sync_and_parity() {
        let config = BenchmarkConfig {
            items: 16,
            queries: 4,
            set_size: 12,
            overlap: 10,
            threshold: 0.8,
            num_perm: 32,
            seed: 7,
            threads: None,
            dataset: None,
        };
        let mut report = BenchmarkReport::new(
            "storage-mutation-test",
            1,
            Environment {
                os: "test".into(),
                arch: "test".into(),
                logical_cpus: 1,
                rustc: "test".into(),
                git_sha: "test".into(),
            },
            config.clone(),
        );
        append_storage_mutation_metrics(&config, &mut report).expect("mutation benchmark");
        assert!(report
            .metrics
            .contains_key("storage.persistent.mutation_operations_per_second"));
        assert!(report.metrics.contains_key("storage.persistent.sync_ms"));
        assert!(
            (report.metrics["storage.persistent.mutation_parity"].value - 1.0).abs() < f64::EPSILON
        );
    }
}
