//! Read-only lazy indexes apply appended delta generations exactly like the
//! writable persistent index.

use std::{
    fs::{self, OpenOptions},
    io::Write,
    path::PathBuf,
};

use pari_core::{MinHash32, MinHash64};
use pari_store::{PersistentIndex32, PersistentIndex64, PersistentIndexOptions, ScoreFilter};
use pari_store_lazy::{LazyIndex, LazyIndex32, LazyIndex64};

fn test_path(name: &str) -> PathBuf {
    std::env::temp_dir().join(format!(
        "pari-lazy-deltas-{name}-{}.pari",
        std::process::id()
    ))
}

fn tokens(item: u64, variant: u64) -> impl Iterator<Item = u64> {
    let group = item / 4;
    (group * 1_000..group * 1_000 + 40).chain([1_000_000 + item * 10 + variant])
}

fn sketch(item: u64, variant: u64) -> MinHash32 {
    let mut sketch = MinHash32::new(128, 7).expect("sketch");
    for token in tokens(item, variant) {
        sketch.update(&token.to_le_bytes());
    }
    sketch
}

fn sketch64(item: u64, variant: u64) -> MinHash64 {
    let mut sketch = MinHash64::new(128, 7).expect("sketch");
    for token in tokens(item, variant) {
        sketch.update(&token.to_le_bytes());
    }
    sketch
}

/// Build a base of `items` keys and two small delta generations.
fn build32(name: &str, items: u64, signatures: bool) -> (PathBuf, PersistentIndex32) {
    let path = test_path(name);
    let _ = fs::remove_file(&path);
    let options = PersistentIndexOptions::new().store_signatures(signatures);
    let mut index =
        PersistentIndex32::create_with_options(&path, 0.5, 128, 7, options).expect("create");
    let sketches: Vec<_> = (0..items).map(|item| sketch(item, 0)).collect();
    index.insert_many((0..items).zip(&sketches)).expect("base");
    index.sync().expect("base commit");

    // Generation 1: new keys, a removed base key, a base key stored again.
    index
        .insert(items + 1, &sketch(items + 1, 0))
        .expect("insert");
    index
        .insert(items + 2, &sketch(items + 2, 0))
        .expect("insert");
    assert!(index.remove(5));
    assert!(index.remove(9));
    index.insert(9, &sketch(9, 3)).expect("reinsert");
    index.flush().expect("delta 1");
    // Generation 2: remove a delta key, move another delta key again.
    assert!(index.remove(items + 1));
    assert!(index.remove(items + 2));
    index
        .insert(items + 2, &sketch(items + 2, 4))
        .expect("reinsert");
    index.flush().expect("delta 2");
    assert_eq!(index.stats().expect("stats").delta_generations, 2);
    (path, index)
}

fn probes(items: u64) -> Vec<(u64, u64)> {
    (0..items + 4)
        .step_by(3)
        .flat_map(|item| [(item, 0), (item, 3), (item, 4)])
        .collect()
}

#[test]
fn lazy_queries_match_the_persistent_index_across_deltas() {
    let (path, index) = build32("parity", 1_200, true);
    let mut lazy = LazyIndex32::open(&path).expect("open lazy");
    let stats = lazy.stats();
    assert_eq!(stats.items, index.len());
    assert_eq!(stats.delta_generations, 2);
    let store = index.stats().expect("stats");
    assert_eq!(stats.buckets, store.committed_buckets);
    assert_eq!(stats.distribution, store.committed_distribution);
    for (item, variant) in probes(1_200) {
        let query = sketch(item, variant);
        assert_eq!(
            lazy.query(&query).expect("lazy query"),
            index.query(&query).expect("query"),
            "probe {item}/{variant}"
        );
        assert_eq!(
            lazy.query_scored(&query, ScoreFilter::new())
                .expect("lazy scored"),
            index
                .query_scored(&query, ScoreFilter::new())
                .expect("scored"),
            "scored probe {item}/{variant}"
        );
    }
    lazy.verify().expect("verify every generation");

    // The family-selecting reader sees the same generations.
    let LazyIndex::Affine32(mut selected) = LazyIndex::open(&path).expect("open") else {
        panic!("affine32 snapshot");
    };
    assert_eq!(
        selected.query(&sketch(9, 3)).expect("query"),
        index.query(&sketch(9, 3)).expect("query")
    );
    drop(index);
    let _ = fs::remove_file(&path);
}

#[test]
fn lazy_reader_ignores_an_interrupted_commit() {
    let (path, index) = build32("interrupted", 800, false);
    let expected = index.query(&sketch(9, 3)).expect("query");
    drop(index);
    let mut file = OpenOptions::new().append(true).open(&path).expect("open");
    file.write_all(b"PARIDLT\0").expect("partial header");
    drop(file);
    let mut lazy = LazyIndex32::open(&path).expect("open lazy");
    assert_eq!(lazy.query(&sketch(9, 3)).expect("query"), expected);
    assert!(matches!(
        lazy.query_scored(&sketch(9, 3), ScoreFilter::new()),
        Err(pari_store_lazy::LazyStoreError::SignaturesUnavailable)
    ));
    let _ = fs::remove_file(&path);
}

#[test]
fn affine64_lazy_reader_applies_deltas() {
    let path = test_path("affine64");
    let _ = fs::remove_file(&path);
    let mut index = PersistentIndex64::create(&path, 0.5, 128, 7).expect("create");
    let sketches: Vec<_> = (0..1_000).map(|item| sketch64(item, 0)).collect();
    index.insert_many((0..1_000).zip(&sketches)).expect("base");
    index.sync().expect("base commit");
    assert!(index.remove(2));
    index.insert(2, &sketch64(2, 7)).expect("reinsert");
    index.insert(5_000, &sketch64(5_000, 0)).expect("insert");
    index.flush().expect("delta");
    assert_eq!(index.stats().expect("stats").delta_generations, 1);
    let mut lazy = LazyIndex64::open(&path).expect("open lazy");
    for (item, variant) in [(2, 7), (2, 0), (5_000, 0), (3, 0)] {
        let query = sketch64(item, variant);
        assert_eq!(
            lazy.query(&query).expect("lazy"),
            index.query(&query).expect("query")
        );
    }
    drop(index);
    let _ = fs::remove_file(&path);
}
