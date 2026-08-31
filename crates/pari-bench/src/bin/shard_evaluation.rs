use std::{
    collections::BTreeSet,
    env,
    error::Error,
    fs,
    path::PathBuf,
    time::{Instant, SystemTime, UNIX_EPOCH},
};

use pari_core::MinHash32;
use pari_index::LshIndex32;
use serde::Serialize;

const ITEMS: usize = 50_000;
const QUERIES: usize = 100;
const NUM_PERM: usize = 128;
const SEED: u64 = 7;
const THRESHOLD: f64 = 0.8;

#[derive(Serialize)]
struct Report {
    schema_version: u32,
    generated_unix_seconds: u64,
    items: usize,
    queries: usize,
    num_perm: usize,
    threshold: f64,
    baseline_query_ms: f64,
    results: Vec<ShardResult>,
}

#[derive(Serialize)]
struct ShardResult {
    shards: usize,
    build_ms: f64,
    query_ms: f64,
    query_overhead_ratio: f64,
    candidate_parity: bool,
    total_candidates: usize,
    max_shard_items: usize,
    total_bucket_memberships: u64,
}

fn main() -> Result<(), Box<dyn Error>> {
    let output = env::args_os()
        .nth(1)
        .map(PathBuf::from)
        .ok_or("usage: shard_evaluation OUTPUT.json")?;
    if output.exists() {
        return Err(format!("refusing to overwrite {}", output.display()).into());
    }

    let signatures = (0..ITEMS).map(signature).collect::<Result<Vec<_>, _>>()?;
    let query_positions = (0..QUERIES)
        .map(|query| query * ITEMS / QUERIES)
        .collect::<Vec<_>>();
    let queries = query_positions
        .iter()
        .map(|position| &signatures[*position])
        .collect::<Vec<_>>();

    let mut baseline = LshIndex32::new(THRESHOLD, NUM_PERM, SEED)?;
    baseline.insert_many(
        signatures
            .iter()
            .enumerate()
            .map(|(key, sketch)| (u64::try_from(key).expect("fixture key fits"), sketch)),
    )?;
    let started = Instant::now();
    let expected = baseline.query_many(queries.iter().copied())?;
    let baseline_query_ms = started.elapsed().as_secs_f64() * 1_000.0;
    drop(baseline);

    let mut results = Vec::new();
    for shard_count in [1, 2, 4, 8, 16] {
        let started = Instant::now();
        let mut shards = (0..shard_count)
            .map(|_| LshIndex32::new(THRESHOLD, NUM_PERM, SEED))
            .collect::<Result<Vec<_>, _>>()?;
        for (key, sketch) in signatures.iter().enumerate() {
            shards[key % shard_count].insert(u64::try_from(key)?, sketch)?;
        }
        let build_ms = started.elapsed().as_secs_f64() * 1_000.0;
        let max_shard_items = shards.iter().map(LshIndex32::len).max().unwrap_or(0);
        let total_bucket_memberships = shards
            .iter()
            .map(|shard| shard.stats().buckets.memberships)
            .sum();

        let started = Instant::now();
        let mut actual = Vec::with_capacity(queries.len());
        for query in &queries {
            let mut merged = BTreeSet::new();
            for shard in &shards {
                merged.extend(shard.query(query)?);
            }
            actual.push(merged.into_iter().collect::<Vec<_>>());
        }
        let query_ms = started.elapsed().as_secs_f64() * 1_000.0;
        results.push(ShardResult {
            shards: shard_count,
            build_ms,
            query_ms,
            query_overhead_ratio: query_ms / baseline_query_ms,
            candidate_parity: actual == expected,
            total_candidates: actual.iter().map(Vec::len).sum(),
            max_shard_items,
            total_bucket_memberships,
        });
    }

    let report = Report {
        schema_version: 1,
        generated_unix_seconds: SystemTime::now().duration_since(UNIX_EPOCH)?.as_secs(),
        items: ITEMS,
        queries: QUERIES,
        num_perm: NUM_PERM,
        threshold: THRESHOLD,
        baseline_query_ms,
        results,
    };
    fs::write(output, serde_json::to_vec_pretty(&report)?)?;
    Ok(())
}

fn signature(key: usize) -> Result<MinHash32, pari_core::MinHashError> {
    let key = u64::try_from(key).expect("fixture key fits");
    let values = (0..NUM_PERM)
        .map(|permutation| {
            let permutation = u64::try_from(permutation).expect("permutation fits");
            let mixed = mix64(key ^ permutation.wrapping_mul(0xD6E8_FEB8_6659_FD93));
            u32::from_le_bytes(
                mixed.to_le_bytes()[..4]
                    .try_into()
                    .expect("four-byte prefix"),
            )
        })
        .collect();
    MinHash32::from_signature(values, SEED)
}

fn mix64(mut value: u64) -> u64 {
    value ^= value >> 30;
    value = value.wrapping_mul(0xBF58_476D_1CE4_E5B9);
    value ^= value >> 27;
    value = value.wrapping_mul(0x94D0_49BB_1331_11EB);
    value ^ (value >> 31)
}
