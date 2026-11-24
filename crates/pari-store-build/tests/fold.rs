//! Building from an index with appended commit generations must give exactly
//! the bytes that compacting it first and then building gives.

use std::{
    collections::BTreeSet,
    fs,
    path::{Path, PathBuf},
    sync::atomic::{AtomicU64, Ordering},
};

use pari_core::{MinHash32, MinHash64};
use pari_format::FileLayout;
use pari_store::{PersistentIndex32, PersistentIndex64, PersistentIndexOptions};
use pari_store_build::{build_external, BuildOptions};
use pari_store_lazy::LazyIndex32;

static NEXT: AtomicU64 = AtomicU64::new(0);

struct Fixture(PathBuf);

impl Fixture {
    fn new() -> Self {
        let path = std::env::temp_dir().join(format!(
            "pari-fold-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir(&path).expect("owned fixture directory");
        Self(path)
    }

    fn path(&self, name: &str) -> PathBuf {
        self.0.join(name)
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

/// A small deterministic generator for reproducible histories.
struct Random(u64);

impl Random {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }

    fn below(&mut self, bound: u64) -> u64 {
        self.next() % bound
    }
}

/// Features for one version of a key; nearby keys share most of them.
fn features(key: u64, version: u64) -> Vec<Vec<u8>> {
    let group = key / 4;
    (0..24)
        .map(|token| format!("g{group}-t{token}").into_bytes())
        .chain([format!("k{key}-v{version}").into_bytes()])
        .collect()
}

/// One committed step of a history: keys inserted with their version, and
/// keys removed.
struct Step {
    inserts: Vec<(u64, u64)>,
    removes: Vec<u64>,
}

/// A random history over keys `0..key_space` where every later step appends
/// a delta generation. Keys are removed and re-inserted with new versions.
fn history(seed: u64, steps: usize, key_space: u64) -> Vec<Step> {
    let mut random = Random(seed);
    let mut live = BTreeSet::new();
    let mut history = Vec::new();
    for step in 0..steps {
        let mut inserts = Vec::new();
        let mut removes = Vec::new();
        for _ in 0..random.below(12) {
            let candidates: Vec<u64> = live.iter().copied().collect();
            if candidates.is_empty() {
                break;
            }
            let key =
                candidates[usize::try_from(random.below(candidates.len() as u64)).expect("index")];
            if !removes.contains(&key) {
                removes.push(key);
            }
        }
        for _ in 0..(if step == 0 { 120 } else { random.below(30) }) {
            let key = random.below(key_space);
            let removed_now = removes.contains(&key);
            if (!live.contains(&key) || removed_now)
                && !inserts.iter().any(|(existing, _)| *existing == key)
            {
                inserts.push((key, step as u64));
            }
        }
        for key in &removes {
            live.remove(key);
        }
        for (key, _) in &inserts {
            live.insert(*key);
        }
        history.push(Step { inserts, removes });
    }
    history
}

macro_rules! write_history {
    ($index:ty, $sketch:ty, $path:expr, $history:expr, $signatures:expr) => {{
        let options = PersistentIndexOptions::new().store_signatures($signatures);
        let mut index = <$index>::create_with_options($path, 0.5, 64, 7, options).expect("create");
        for step in $history {
            for key in &step.removes {
                assert!(index.remove(*key));
            }
            let sketches: Vec<_> = step
                .inserts
                .iter()
                .map(|(key, version)| {
                    let mut sketch = <$sketch>::new(64, 7).expect("sketch");
                    for feature in features(*key, *version) {
                        sketch.update(&feature);
                    }
                    (*key, sketch)
                })
                .collect();
            index
                .insert_many(sketches.iter().map(|(key, sketch)| (*key, sketch)))
                .expect("insert");
            index.flush_delta().expect("append a generation");
        }
        drop(index);
    }};
}

fn generations(path: &Path) -> usize {
    let mut file = fs::File::open(path).expect("open");
    FileLayout::read_with_generations(&mut file)
        .expect("layout")
        .deltas()
        .len()
}

/// Build from the uncompacted file and from a compacted copy; both outputs
/// and the compacted snapshot itself must be the same bytes.
fn assert_fold_matches_compaction(fixture: &Fixture, source: &Path, compact: impl FnOnce(&Path)) {
    let appended = generations(source);
    assert!(appended > 0, "the history must append generations");

    let folded_output = fixture.path("folded.pari");
    let stats = build_external(
        source,
        &folded_output,
        BuildOptions {
            max_buffer_records: 211,
            ..BuildOptions::default()
        },
    )
    .expect("build from appended generations");
    assert_eq!(stats.folded_generations, appended);
    assert!(!stats.reused_source_buckets);

    let compacted = fixture.path("compacted.pari");
    fs::copy(source, &compacted).expect("copy");
    compact(&compacted);
    assert_eq!(generations(&compacted), 0);
    let compacted_output = fixture.path("compacted-built.pari");
    let compacted_stats =
        build_external(&compacted, &compacted_output, BuildOptions::default()).expect("build");
    assert_eq!(compacted_stats.folded_generations, 0);

    let folded = fs::read(&folded_output).expect("folded bytes");
    assert_eq!(
        folded,
        fs::read(&compacted_output).expect("compacted build bytes"),
        "folding must equal compact-then-build"
    );
    assert_eq!(
        folded,
        fs::read(&compacted).expect("compacted bytes"),
        "the build must equal the compacted snapshot"
    );
    // The source is left untouched.
    assert_eq!(generations(source), appended);
}

#[test]
fn folded_histories_equal_compact_then_build() {
    for (seed, signatures) in [(1, false), (2, true), (3, false), (4, true)] {
        let fixture = Fixture::new();
        let source = fixture.path("source.pari");
        write_history!(
            PersistentIndex32,
            MinHash32,
            &source,
            &history(seed, 9, 400),
            signatures
        );
        assert_fold_matches_compaction(&fixture, &source, |path| {
            let mut index = PersistentIndex32::open(path).expect("open");
            index.compact().expect("compact");
        });
    }
}

#[test]
fn folded_affine64_history_equals_compact_then_build() {
    let fixture = Fixture::new();
    let source = fixture.path("source.pari");
    write_history!(
        PersistentIndex64,
        MinHash64,
        &source,
        &history(5, 7, 300),
        true
    );
    assert_fold_matches_compaction(&fixture, &source, |path| {
        let mut index = PersistentIndex64::open(path).expect("open");
        index.compact().expect("compact");
    });
}

#[test]
fn folded_builds_answer_queries_like_the_source() {
    let fixture = Fixture::new();
    let source = fixture.path("source.pari");
    let steps = history(7, 8, 300);
    write_history!(PersistentIndex32, MinHash32, &source, &steps, false);
    let output = fixture.path("built.pari");
    build_external(&source, &output, BuildOptions::default()).expect("build");

    let original = PersistentIndex32::open(&source).expect("source");
    let mut lazy = LazyIndex32::open(&output).expect("lazy");
    assert_eq!(lazy.len(), original.len());
    for key in 0..300 {
        for version in [0, 3, 7] {
            let mut sketch = MinHash32::new(64, 7).expect("sketch");
            for feature in features(key, version) {
                sketch.update(&feature);
            }
            assert_eq!(
                lazy.query(&sketch).expect("lazy query"),
                original.query(&sketch).expect("source query"),
                "key {key} version {version}"
            );
        }
    }
}
