use pari_core::{MinHash32, MinHash64};
use pari_index::{LshError, LshIndex32, LshIndex64, LshParams, ScoreFilter};

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

#[test]
fn in_memory_scores_match_minhash_estimates() {
    let query = sketch(0..100);
    let close = sketch(0..95);
    let medium = sketch((0..80).chain(500..520));
    let far = sketch(1_000..1_100);
    let mut index = LshIndex32::with_params(0.5, 128, 7, LshParams::new(32, 4))
        .expect("index")
        .with_signature_storage();
    assert!(index.stores_signatures());
    index
        .insert_many([(3, &medium), (1, &close), (9, &far)])
        .expect("insert");

    let scored = index
        .query_scored(&query, ScoreFilter::new())
        .expect("scored");
    let mut keys: Vec<_> = scored.iter().map(|candidate| candidate.key).collect();
    keys.sort_unstable();
    assert_eq!(keys, index.query(&query).expect("candidates"));
    assert_eq!(scored[0].key, 1);
    for candidate in &scored {
        let item = match candidate.key {
            1 => &close,
            3 => &medium,
            _ => &far,
        };
        assert!(
            (candidate.similarity - query.jaccard(item).expect("jaccard")).abs() < f64::EPSILON
        );
    }
    assert_eq!(
        index
            .query_scored(&query, ScoreFilter::new().top_k(1))
            .expect("top"),
        scored[..1].to_vec()
    );
    let batch = index
        .query_scored_many([&query, &far], ScoreFilter::new().top_k(1))
        .expect("batch");
    assert_eq!(batch[0], scored[..1].to_vec());
    assert_eq!(batch[1][0].key, 9);

    assert!(index.remove(1));
    assert!(index
        .query_scored(&query, ScoreFilter::new())
        .expect("after remove")
        .iter()
        .all(|candidate| candidate.key != 1));
}

#[test]
fn in_memory_scoring_requires_storage_and_valid_filters() {
    let item = sketch(0..50);
    let mut plain = LshIndex32::new(0.8, 128, 7).expect("index");
    plain.insert(1, &item).expect("insert");
    assert!(!plain.stores_signatures());
    assert!(matches!(
        plain.query_scored(&item, ScoreFilter::new()),
        Err(LshError::SignaturesUnavailable)
    ));
    // Items inserted before storage was enabled have no signature.
    let late = plain.with_signature_storage();
    assert!(matches!(
        late.query_scored(&item, ScoreFilter::new()),
        Err(LshError::SignaturesUnavailable)
    ));
    let mut stored = LshIndex32::new(0.8, 128, 7)
        .expect("index")
        .with_signature_storage();
    stored.insert(1, &item).expect("insert");
    for filter in [
        ScoreFilter::new().min_similarity(1.5),
        ScoreFilter::new().top_k(0),
    ] {
        assert!(matches!(
            stored.query_scored(&item, filter),
            Err(LshError::InvalidScoreFilter(_))
        ));
    }
}

#[test]
fn affine64_in_memory_scoring_uses_full_width() {
    let query = sketch64(0..100);
    let close = sketch64(0..95);
    let mut index = LshIndex64::with_params(0.5, 128, 7, LshParams::new(32, 4))
        .expect("index")
        .with_signature_storage();
    index.insert(4, &close).expect("insert");
    let scored = index
        .query_scored(&query, ScoreFilter::new())
        .expect("scored");
    assert_eq!(scored.len(), 1);
    assert!((scored[0].similarity - query.jaccard(&close).expect("jaccard")).abs() < f64::EPSILON);
}
