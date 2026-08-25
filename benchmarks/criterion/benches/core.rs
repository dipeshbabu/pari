use criterion::{black_box, criterion_group, criterion_main, BatchSize, Criterion};
use pari_core::{sha1_hash32, MinHash32};
use pari_index::{group_pairs, LshIndex32, LshParams};

fn bench_hashing(criterion: &mut Criterion) {
    let payload = b"pari-benchmark-feature";
    criterion.bench_function("sha1_hash32_22_bytes", |bencher| {
        bencher.iter(|| black_box(sha1_hash32(black_box(payload))))
    });
}

fn bench_minhash_update(criterion: &mut Criterion) {
    criterion.bench_function("minhash32_update_128", |bencher| {
        bencher.iter_batched(
            || MinHash32::new(128, 7).expect("valid benchmark sketch"),
            |mut sketch| {
                sketch.update(black_box(b"feature"));
                black_box(sketch);
            },
            BatchSize::SmallInput,
        );
    });
}

fn bench_lsh_query(criterion: &mut Criterion) {
    let mut index =
        LshIndex32::with_params(0.8, 128, 7, LshParams::new(32, 4)).expect("valid index");
    let mut signatures = Vec::new();
    for key in 0_u64..1_000 {
        let mut sketch = MinHash32::new(128, 7).expect("valid sketch");
        for feature in 0_u64..100 {
            sketch.update(&key.wrapping_mul(1_000).wrapping_add(feature).to_le_bytes());
        }
        signatures.push(sketch);
    }
    index
        .insert_many(
            signatures
                .iter()
                .enumerate()
                .map(|(key, sketch)| (u64::try_from(key).expect("key fits u64"), sketch)),
        )
        .expect("valid benchmark index");
    let query = &signatures[500];

    criterion.bench_function("lsh32_query_1000_items", |bencher| {
        bencher.iter(|| black_box(index.query(black_box(query)).expect("valid query")))
    });
}

fn bench_grouping(criterion: &mut Criterion) {
    criterion.bench_function("group_pairs_100k_chain_edges", |bencher| {
        bencher.iter(|| black_box(group_pairs((0_u64..100_000).map(|key| (key, key + 1)), 2)))
    });
}

criterion_group!(
    benches,
    bench_hashing,
    bench_minhash_update,
    bench_lsh_query,
    bench_grouping
);
criterion_main!(benches);
