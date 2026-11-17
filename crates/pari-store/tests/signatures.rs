use std::{
    fs,
    path::PathBuf,
    sync::atomic::{AtomicU64, Ordering},
};

use pari_core::{MinHash32, MinHash64};
use pari_index::LshParams;
use pari_store::{
    PersistentIndex32, PersistentIndex64, PersistentIndexOptions, ScoreFilter, StoreError,
};

static NEXT_TEST_PATH: AtomicU64 = AtomicU64::new(0);

fn test_path(name: &str) -> PathBuf {
    std::env::temp_dir().join(format!(
        "pari-store-signatures-{name}-{}-{}.pari",
        std::process::id(),
        NEXT_TEST_PATH.fetch_add(1, Ordering::Relaxed)
    ))
}

fn sketch(values: impl IntoIterator<Item = u64>) -> MinHash32 {
    let mut sketch = MinHash32::new(128, 7).expect("sketch");
    for value in values {
        sketch.update(&value.to_le_bytes());
    }
    sketch
}

fn sketch64(values: impl IntoIterator<Item = u64>) -> MinHash64 {
    let mut sketch = MinHash64::new(128, 7).expect("sketch");
    for value in values {
        sketch.update(&value.to_le_bytes());
    }
    sketch
}

fn with_signatures() -> PersistentIndexOptions {
    PersistentIndexOptions::new()
        .params(LshParams::new(32, 4))
        .store_signatures(true)
}

fn keys(scored: &[pari_store::ScoredCandidate]) -> Vec<u64> {
    scored.iter().map(|candidate| candidate.key).collect()
}

#[test]
fn scored_queries_match_minhash_estimates_before_and_after_commit() {
    let path = test_path("estimates");
    let query = sketch(0..100);
    let close = sketch(0..95);
    let medium = sketch((0..80).chain(200..220));
    let far = sketch(1_000..1_100);
    let mut index = PersistentIndex32::create_with_options(&path, 0.5, 128, 7, with_signatures())
        .expect("create");
    assert!(index.stores_signatures());
    index
        .insert_many([(3, &medium), (1, &close), (9, &far)])
        .expect("insert");

    let check = |index: &PersistentIndex32| {
        let scored = index
            .query_scored(&query, ScoreFilter::new())
            .expect("scored");
        let candidates = index.query(&query).expect("candidates");
        let mut sorted = keys(&scored);
        sorted.sort_unstable();
        assert_eq!(
            sorted, candidates,
            "scores cover exactly the LSH candidates"
        );
        for candidate in &scored {
            let item = match candidate.key {
                1 => &close,
                3 => &medium,
                _ => &far,
            };
            let expected = query.jaccard(item).expect("jaccard");
            assert!((candidate.similarity - expected).abs() < f64::EPSILON);
        }
        assert!(scored
            .windows(2)
            .all(|pair| pair[0].similarity >= pair[1].similarity));
        assert_eq!(scored.first().map(|candidate| candidate.key), Some(1));
    };

    check(&index);
    index.sync().expect("sync");
    check(&index);
    drop(index);
    let reopened = PersistentIndex32::open(&path).expect("reopen");
    assert!(reopened.stores_signatures());
    assert!(reopened.stats().expect("stats").stores_signatures);
    check(&reopened);
    let _ = fs::remove_file(path);
}

#[test]
fn filters_limit_and_threshold_results() {
    let path = test_path("filters");
    let query = sketch(0..100);
    let mut index = PersistentIndex32::create_with_options(&path, 0.5, 128, 7, with_signatures())
        .expect("create");
    let items: Vec<_> = (0..5_u64).map(|step| sketch(0..(100 - step * 5))).collect();
    index
        .insert_many(
            items
                .iter()
                .enumerate()
                .map(|(key, sketch)| (key as u64, sketch)),
        )
        .expect("insert");
    index.sync().expect("sync");

    let all = index
        .query_scored(&query, ScoreFilter::new())
        .expect("scored");
    assert_eq!(all.len(), 5);
    let top = index
        .query_scored(&query, ScoreFilter::new().top_k(2))
        .expect("top");
    assert_eq!(top, all[..2].to_vec());
    let threshold = all[2].similarity;
    let filtered = index
        .query_scored(&query, ScoreFilter::new().min_similarity(threshold))
        .expect("filtered");
    assert!(filtered
        .iter()
        .all(|candidate| candidate.similarity >= threshold));
    assert!(filtered.len() >= 3);
    let batch = index
        .query_scored_many([&query, &items[4]], ScoreFilter::new().top_k(1))
        .expect("batch");
    assert_eq!(batch[0], all[..1].to_vec());
    assert_eq!(batch[1][0].key, 4);
    assert!((batch[1][0].similarity - 1.0).abs() < f64::EPSILON);

    for invalid in [
        ScoreFilter::new().min_similarity(f64::NAN),
        ScoreFilter::new().min_similarity(1.5),
        ScoreFilter::new().min_similarity(-0.1),
        ScoreFilter::new().top_k(0),
    ] {
        assert!(matches!(
            index.query_scored(&query, invalid),
            Err(StoreError::InvalidScoreFilter { .. })
        ));
    }
    let _ = fs::remove_file(path);
}

#[test]
fn remove_and_reinsert_replace_committed_signatures() {
    let path = test_path("reinsert");
    let first = sketch(0..100);
    let second = sketch(500..600);
    let mut index = PersistentIndex32::create_with_options(&path, 0.5, 128, 7, with_signatures())
        .expect("create");
    index.insert(5, &first).expect("insert");
    index.insert(6, &first).expect("insert");
    index.sync().expect("sync");

    assert!(index.remove(5));
    assert_eq!(index.signature(5).expect("absent"), None);
    index.insert(5, &second).expect("reinsert");
    assert_eq!(index.signature(5).expect("signature"), Some(second.clone()));
    assert_eq!(index.signature(6).expect("signature"), Some(first.clone()));
    let scored = index
        .query_scored(&second, ScoreFilter::new())
        .expect("scored");
    assert_eq!(keys(&scored), vec![5]);
    assert!((scored[0].similarity - 1.0).abs() < f64::EPSILON);

    index.sync().expect("sync");
    let reopened = PersistentIndex32::open(&path).expect("reopen");
    assert_eq!(reopened.signature(5).expect("signature"), Some(second));
    assert_eq!(reopened.signature(6).expect("signature"), Some(first));
    assert_eq!(reopened.signature(99).expect("missing key"), None);
    let _ = fs::remove_file(path);
}

#[test]
fn indexes_without_signatures_reject_scored_queries() {
    let path = test_path("disabled");
    let mut index = PersistentIndex32::create(&path, 0.8, 128, 7).expect("create");
    index.insert(1, &sketch(0..10)).expect("insert");
    assert!(!index.stores_signatures());
    assert!(!index.stats().expect("stats").stores_signatures);
    assert_eq!(index.signature(1).expect("disabled"), None);
    assert!(matches!(
        index.query_scored(&sketch(0..10), ScoreFilter::new()),
        Err(StoreError::SignaturesUnavailable)
    ));
    index.sync().expect("sync");
    let reopened = PersistentIndex32::open(&path).expect("reopen");
    assert!(!reopened.stores_signatures());
    let _ = fs::remove_file(path);
}

#[test]
fn empty_signature_indexes_keep_the_setting_across_reopen() {
    let path = test_path("empty");
    let index = PersistentIndex32::create_with_options(&path, 0.8, 128, 7, with_signatures())
        .expect("create");
    drop(index);
    let mut reopened = PersistentIndex32::open(&path).expect("reopen");
    assert!(reopened.stores_signatures());
    reopened.insert(1, &sketch(0..10)).expect("insert");
    reopened.sync().expect("sync");
    assert!(PersistentIndex32::open(&path)
        .expect("reopen")
        .stores_signatures());
    let _ = fs::remove_file(path);
}

#[test]
fn affine64_signatures_keep_full_width() {
    let path = test_path("affine64");
    let query = sketch64(0..100);
    let close = sketch64(0..95);
    let mut index = PersistentIndex64::create_with_options(&path, 0.5, 128, 7, with_signatures())
        .expect("create");
    index.insert(2, &close).expect("insert");
    index.sync().expect("sync");
    let reopened = PersistentIndex64::open(&path).expect("reopen");
    assert_eq!(
        reopened.signature(2).expect("signature"),
        Some(close.clone())
    );
    let scored = reopened
        .query_scored(&query, ScoreFilter::new())
        .expect("scored");
    assert_eq!(keys(&scored), vec![2]);
    let expected = query.jaccard(&close).expect("jaccard");
    assert!((scored[0].similarity - expected).abs() < f64::EPSILON);
    let _ = fs::remove_file(path);
}

#[test]
fn large_tables_split_into_multiple_segments() {
    // 4,096 affine32 values are 16 KiB per row, so 2,100 rows exceed one
    // 32 MiB segment and exercise row lookup across segment boundaries.
    let path = test_path("segments");
    let options = PersistentIndexOptions::new()
        .params(LshParams::new(1, 1))
        .store_signatures(true);
    let mut index =
        PersistentIndex32::create_with_options(&path, 0.5, 4_096, 7, options).expect("create");
    let sketches: Vec<_> = (0..2_100_u32)
        .map(|row| {
            let values = (0..4_096_u32)
                .map(|column| row.wrapping_mul(31).wrapping_add(column))
                .collect();
            MinHash32::from_signature(values, 7).expect("signature")
        })
        .collect();
    index
        .insert_many(
            sketches
                .iter()
                .enumerate()
                .map(|(key, sketch)| (key as u64, sketch)),
        )
        .expect("insert");
    index.sync().expect("sync");
    drop(index);
    let reopened = PersistentIndex32::open(&path).expect("reopen");
    for key in [0_usize, 2_000, 2_047, 2_048, 2_099] {
        assert_eq!(
            reopened.signature(key as u64).expect("signature").as_ref(),
            Some(&sketches[key])
        );
    }
    let _ = fs::remove_file(path);
}
