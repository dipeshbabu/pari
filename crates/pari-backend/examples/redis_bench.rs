use std::{
    env,
    error::Error,
    process,
    time::{Instant, SystemTime, UNIX_EPOCH},
};

use pari_backend::{BackendIndex32, RedisBackend, StorageBackend};
use pari_core::MinHash32;

fn main() -> Result<(), Box<dyn Error>> {
    let url = env::var("PARI_REDIS_URL")?;
    let items = env_usize("PARI_REDIS_BENCH_ITEMS", 2_000)?;
    let queries = env_usize("PARI_REDIS_BENCH_QUERIES", 200)?.min(items);
    if items == 0 || queries == 0 {
        return Err("benchmark item and query counts must be positive".into());
    }

    let namespace = benchmark_namespace()?;
    let mut backend = RedisBackend::connect(&url, &namespace)?;
    backend.cleanup()?;

    let num_perm = 128;
    let seed = 7;
    let signatures = (0..items)
        .map(|item| benchmark_signature(item, num_perm, seed))
        .collect::<Result<Vec<_>, _>>()?;

    let mut index = BackendIndex32::create(backend, 0.8, num_perm, seed, None)?;
    let build_started = Instant::now();
    index.insert_many(
        signatures
            .iter()
            .enumerate()
            .map(|(key, sketch)| (u64::try_from(key).expect("benchmark key fits u64"), sketch)),
    )?;
    index.flush()?;
    let build_elapsed = build_started.elapsed();

    let query_started = Instant::now();
    let results = index.query_many(signatures.iter().take(queries))?;
    let query_elapsed = query_started.elapsed();
    if results.iter().any(Vec::is_empty) {
        return Err("benchmark query unexpectedly returned no self candidate".into());
    }

    let stats = index.stats()?;
    let insert_rate = rate(items, build_elapsed.as_secs_f64())?;
    let query_rate = rate(queries, query_elapsed.as_secs_f64())?;
    println!(
        "{{\"items\":{items},\"queries\":{queries},\"insert_items_per_second\":{insert_rate:.3},\"query_items_per_second\":{query_rate:.3},\"backend_round_trips\":{},\"bucket_memberships\":{}}}",
        stats.round_trips, stats.bucket_memberships
    );

    index.cleanup()?;
    Ok(())
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
