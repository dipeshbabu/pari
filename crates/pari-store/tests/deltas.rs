//! Delta-generation commits: parity, compaction, interrupted and damaged appends.

use std::{
    collections::BTreeMap,
    fs::{self, OpenOptions},
    io::{Cursor, Write},
    path::{Path, PathBuf},
    sync::atomic::{AtomicU64, Ordering},
};

use pari_core::{MinHash32, MinHash64};
use pari_format::{FileLayout, FormatError, LayoutError};
use pari_index::{LshIndex32, LshIndex64, LshParams};
use pari_store::{PersistentIndex32, PersistentIndex64, PersistentIndexOptions, ScoreFilter};

static NEXT_TEST_PATH: AtomicU64 = AtomicU64::new(0);

const THRESHOLD: f64 = 0.5;
const NUM_PERM: usize = 128;
const SEED: u64 = 7;

fn test_path(name: &str) -> PathBuf {
    std::env::temp_dir().join(format!(
        "pari-store-deltas-{name}-{}-{}.pari",
        std::process::id(),
        NEXT_TEST_PATH.fetch_add(1, Ordering::Relaxed)
    ))
}

fn cleanup(path: &Path) {
    let _ = fs::remove_file(path);
}

/// Deterministic xorshift generator for reproducible workloads.
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }

    fn below(&mut self, bound: usize) -> usize {
        usize::try_from(self.next() % u64::try_from(bound).expect("bound")).expect("index")
    }
}

/// Items in groups of five near-duplicates; `variant` changes the noise so a
/// reinserted key gets a different signature.
fn tokens(item: u64, variant: u64) -> Vec<u64> {
    let group = item / 5;
    (group * 1_000..group * 1_000 + 40)
        .chain([1_000_000 + item * 10 + variant, 2_000_000 + variant])
        .collect()
}

fn sketch(item: u64, variant: u64) -> MinHash32 {
    let mut sketch = MinHash32::new(NUM_PERM, SEED).expect("sketch");
    for token in tokens(item, variant) {
        sketch.update(&token.to_le_bytes());
    }
    sketch
}

fn sketch64(item: u64, variant: u64) -> MinHash64 {
    let mut sketch = MinHash64::new(NUM_PERM, SEED).expect("sketch");
    for token in tokens(item, variant) {
        sketch.update(&token.to_le_bytes());
    }
    sketch
}

fn sorted(mut keys: Vec<u64>) -> Vec<u64> {
    keys.sort_unstable();
    keys
}

fn layout(path: &Path) -> FileLayout {
    FileLayout::read_with_generations(&mut fs::File::open(path).expect("open")).expect("layout")
}

/// A persistent index and an in-memory reference receiving the same changes.
struct Pair {
    path: PathBuf,
    index: PersistentIndex32,
    reference: LshIndex32,
    /// Live key -> signature variant.
    live: BTreeMap<u64, u64>,
}

impl Pair {
    fn new(name: &str, items: u64, options: PersistentIndexOptions) -> Self {
        let path = test_path(name);
        cleanup(&path);
        let mut index =
            PersistentIndex32::create_with_options(&path, THRESHOLD, NUM_PERM, SEED, options)
                .expect("create");
        let stats = index.stats().expect("stats");
        let mut reference = LshIndex32::with_params(
            THRESHOLD,
            NUM_PERM,
            SEED,
            LshParams::new(stats.bands, stats.rows),
        )
        .expect("reference");
        let sketches: Vec<_> = (0..items).map(|item| sketch(item, 0)).collect();
        index
            .insert_many((0..items).zip(&sketches))
            .expect("insert base");
        reference
            .insert_many((0..items).zip(&sketches))
            .expect("reference base");
        index.sync().expect("base commit");
        Self {
            path,
            index,
            reference,
            live: (0..items).map(|item| (item, 0)).collect(),
        }
    }

    fn insert(&mut self, key: u64, variant: u64) {
        let sketch = sketch(key, variant);
        self.index.insert(key, &sketch).expect("insert");
        self.reference
            .insert(key, &sketch)
            .expect("reference insert");
        self.live.insert(key, variant);
    }

    fn remove(&mut self, key: u64) {
        assert!(self.index.remove(key));
        assert!(self.reference.remove(key));
        self.live.remove(&key);
    }

    /// Reset the reference to exactly `live`.
    fn reset_to(&mut self, live: BTreeMap<u64, u64>) {
        let stats = self.index.stats().expect("stats");
        let mut reference = LshIndex32::with_params(
            THRESHOLD,
            NUM_PERM,
            SEED,
            LshParams::new(stats.bands, stats.rows),
        )
        .expect("reference");
        for (key, variant) in &live {
            reference
                .insert(*key, &sketch(*key, *variant))
                .expect("reference insert");
        }
        self.reference = reference;
        self.live = live;
    }

    fn random_live(&self, rng: &mut Rng) -> u64 {
        *self
            .live
            .keys()
            .nth(rng.below(self.live.len()))
            .expect("live key")
    }

    fn assert_matches(&self, index: &PersistentIndex32, context: &str) {
        assert_eq!(index.len(), self.live.len(), "{context}: len");
        for probe in (0..self.live.len() as u64 + 40).step_by(29) {
            for variant in [0, 1] {
                let query = sketch(probe, variant);
                assert_eq!(
                    sorted(index.query(&query).expect("query")),
                    sorted(self.reference.query(&query).expect("reference query")),
                    "{context}: probe {probe} variant {variant}"
                );
            }
        }
        for key in self.live.keys().step_by(97) {
            assert!(index.contains(*key), "{context}: contains {key}");
        }
    }
}

impl Drop for Pair {
    fn drop(&mut self) {
        cleanup(&self.path);
    }
}

#[test]
fn small_commits_append_deltas_and_match_a_reference_index() {
    // Large enough that 20 small deltas stay under a quarter of the base.
    let mut pair = Pair::new("parity", 5_000, PersistentIndexOptions::new());
    let base_length = layout(&pair.path).base_length();
    let mut rng = Rng(0x9E37_79B9_7F4A_7C15);
    let mut removed = Vec::new();
    let mut next_key = 5_000;
    for round in 1..=20_u64 {
        for _ in 0..30 {
            pair.insert(next_key, 0);
            next_key += 1;
        }
        for _ in 0..10 {
            let key = pair.random_live(&mut rng);
            pair.remove(key);
            removed.push(key);
        }
        for _ in 0..5 {
            let key = removed.swap_remove(rng.below(removed.len()));
            pair.insert(key, round);
        }
        // A change that cancels out leaves nothing to append.
        pair.insert(1_000_000 + round, 0);
        pair.remove(1_000_000 + round);
        pair.index.flush().expect("delta commit");

        let stats = pair.index.stats().expect("stats");
        assert!(!stats.dirty);
        assert_eq!(
            stats.delta_generations,
            usize::try_from(round).expect("round")
        );
        let file = layout(&pair.path);
        assert_eq!(
            file.base_length(),
            base_length,
            "the base is never rewritten"
        );
        assert_eq!(file.deltas().len(), stats.delta_generations);
        assert_eq!(stats.delta_bytes, file.file_length() - base_length);
        pair.assert_matches(&pair.index, &format!("round {round}"));
        if round % 5 == 0 {
            let reopened = PersistentIndex32::open(&pair.path).expect("reopen");
            pair.assert_matches(&reopened, &format!("reopened round {round}"));
            assert_eq!(
                reopened.stats().expect("stats").delta_generations,
                stats.delta_generations
            );
        }
    }
    assert!(matches!(
        FileLayout::read_from(&mut fs::File::open(&pair.path).expect("open")),
        Err(LayoutError::Format(FormatError::AppendedGenerations))
    ));
}

#[test]
fn compaction_folds_deltas_and_removed_keys() {
    let mut pair = Pair::new("compact", 2_000, PersistentIndexOptions::new());
    let mut rng = Rng(42);
    for round in 0..4_u64 {
        for offset in 0..20 {
            pair.insert(10_000 + round * 100 + offset, 0);
        }
        for _ in 0..15 {
            let key = pair.random_live(&mut rng);
            pair.remove(key);
        }
        pair.index.flush().expect("delta commit");
    }
    let before = pair.index.stats().expect("stats");
    assert_eq!(before.delta_generations, 4);
    assert!(before.suppressed_base_keys > 0);

    pair.index.compact().expect("compact");
    let after = pair.index.stats().expect("stats");
    assert_eq!(after.delta_generations, 0);
    assert_eq!(after.delta_bytes, 0);
    assert_eq!(after.suppressed_base_keys, 0);
    assert!(layout(&pair.path).deltas().is_empty());
    pair.assert_matches(&pair.index, "compacted");
    let reopened = PersistentIndex32::open(&pair.path).expect("reopen");
    pair.assert_matches(&reopened, "reopened after compaction");
    drop(reopened);

    // Compacting a compact, clean index changes nothing.
    let bytes = fs::read(&pair.path).expect("read");
    pair.index.compact().expect("no-op compact");
    assert_eq!(fs::read(&pair.path).expect("read"), bytes);
}

#[test]
fn commits_compact_automatically_when_deltas_grow() {
    // Many small deltas: the 33rd commit rewrites the snapshot.
    let mut pair = Pair::new("delta-count", 3_000, PersistentIndexOptions::new());
    for round in 1..=33_u64 {
        pair.insert(50_000 + round, 0);
        pair.index.flush().expect("commit");
        let generations = pair.index.stats().expect("stats").delta_generations;
        if round <= 32 {
            assert_eq!(generations, usize::try_from(round).expect("round"));
        } else {
            assert_eq!(generations, 0, "the 33rd commit compacts");
        }
    }
    pair.assert_matches(&pair.index, "after count compaction");

    // One large delta: more than a quarter of the base is written as a rewrite.
    let mut pair = Pair::new("delta-size", 1_000, PersistentIndexOptions::new());
    for key in 20_000..20_400 {
        pair.insert(key, 0);
    }
    pair.index.flush().expect("commit");
    assert_eq!(pair.index.stats().expect("stats").delta_generations, 0);
    pair.assert_matches(&pair.index, "after size compaction");

    // Removing many keys compacts to drop their memberships.
    let mut rng = Rng(7);
    for _ in 0..400 {
        let key = pair.random_live(&mut rng);
        pair.remove(key);
    }
    pair.index.flush().expect("commit");
    let stats = pair.index.stats().expect("stats");
    assert_eq!(stats.delta_generations, 0);
    assert_eq!(stats.suppressed_base_keys, 0);
    pair.assert_matches(&pair.index, "after removal compaction");
}

#[test]
fn removing_a_trailer_rolls_back_exactly_that_commit() {
    let mut pair = Pair::new("rollback", 2_000, PersistentIndexOptions::new());
    pair.insert(9_000, 0);
    pair.remove(5);
    pair.index.flush().expect("first delta");
    let committed_live = pair.live.clone();

    pair.insert(9_001, 0);
    pair.remove(9_000);
    pair.insert(5, 3);
    pair.index.flush().expect("second delta");
    drop(std::mem::replace(
        &mut pair.index,
        PersistentIndex32::open(&pair.path).expect("reopen"),
    ));

    // Cut the second commit's trailer: it was never committed.
    let length = fs::metadata(&pair.path).expect("metadata").len();
    OpenOptions::new()
        .write(true)
        .open(&pair.path)
        .expect("open")
        .set_len(length - 32)
        .expect("truncate");
    pair.reset_to(committed_live);
    let mut reopened = PersistentIndex32::open(&pair.path).expect("reopen without trailer");
    assert_eq!(reopened.stats().expect("stats").delta_generations, 1);
    pair.assert_matches(&reopened, "after losing the trailer");
    assert_eq!(
        layout(&pair.path).uncommitted_bytes(),
        length - 32 - layout(&pair.path).file_length()
    );

    // The next commit replaces the interrupted record.
    let late = sketch(9_100, 0);
    reopened.insert(9_100, &late).expect("insert");
    pair.reference.insert(9_100, &late).expect("reference");
    pair.live.insert(9_100, 0);
    reopened.flush().expect("commit over interrupted record");
    assert_eq!(layout(&pair.path).uncommitted_bytes(), 0);
    assert_eq!(layout(&pair.path).deltas().len(), 2);
    drop(reopened);
    pair.index = PersistentIndex32::open(&pair.path).expect("reopen");
    pair.assert_matches(&pair.index, "after replacing the interrupted record");
}

#[test]
fn garbage_after_the_last_commit_is_ignored() {
    let mut pair = Pair::new("garbage", 1_500, PersistentIndexOptions::new());
    pair.insert(7_000, 0);
    pair.index.flush().expect("delta");
    let mut file = OpenOptions::new()
        .append(true)
        .open(&pair.path)
        .expect("open");
    file.write_all(b"PARIDLT\0partial header").expect("garbage");
    file.write_all(&[0xAB; 300]).expect("garbage");
    drop(file);
    let mut reopened = PersistentIndex32::open(&pair.path).expect("reopen");
    pair.assert_matches(&reopened, "with garbage");
    reopened.insert(7_001, &sketch(7_001, 0)).expect("insert");
    pair.reference
        .insert(7_001, &sketch(7_001, 0))
        .expect("reference");
    pair.live.insert(7_001, 0);
    reopened.flush().expect("commit");
    let file = layout(&pair.path);
    assert_eq!(file.uncommitted_bytes(), 0);
    assert_eq!(file.deltas().len(), 2);
    pair.assert_matches(&reopened, "after replacing garbage");
}

#[test]
fn damaged_committed_deltas_are_rejected() {
    let mut pair = Pair::new("damaged", 1_500, PersistentIndexOptions::new());
    pair.insert(7_000, 0);
    pair.index.flush().expect("first delta");
    pair.insert(7_001, 0);
    pair.index.flush().expect("second delta");
    let file = layout(&pair.path);
    let bytes = fs::read(&pair.path).expect("read");
    let first = &file.deltas()[0];
    for offset in [
        first.offset() + 20,
        first.end() - 10,
        file.deltas()[1].offset() + 1,
    ] {
        let mut damaged = bytes.clone();
        damaged[usize::try_from(offset).expect("offset")] ^= 0x10;
        fs::write(&pair.path, &damaged).expect("write");
        assert!(
            PersistentIndex32::open(&pair.path).is_err(),
            "damage at {offset} was accepted"
        );
    }
    // Damaged delta payload bytes are caught when the generation is loaded.
    let keys = first.sections()[0];
    let mut damaged = bytes.clone();
    damaged[usize::try_from(keys.payload_offset()).expect("offset") + 3] ^= 0x10;
    fs::write(&pair.path, &damaged).expect("write");
    assert!(PersistentIndex32::open(&pair.path).is_err());
    fs::write(&pair.path, &bytes).expect("restore");
    assert!(PersistentIndex32::open(&pair.path).is_ok());
}

#[test]
fn stale_handles_refuse_to_commit_after_a_delta() {
    let mut pair = Pair::new("stale", 1_500, PersistentIndexOptions::new());
    let mut stale = PersistentIndex32::open(&pair.path).expect("second handle");
    pair.insert(8_000, 0);
    pair.index.flush().expect("delta");
    stale.insert(8_001, &sketch(8_001, 0)).expect("insert");
    let error = stale.flush().expect_err("stale commit");
    assert!(
        error.to_string().contains("committed by another handle"),
        "{error}"
    );
    drop(stale);
    let reopened = PersistentIndex32::open(&pair.path).expect("reopen");
    assert!(reopened.contains(8_000));
    assert!(!reopened.contains(8_001));
}

#[test]
fn stored_signatures_survive_deltas_reopen_and_compaction() {
    let options = PersistentIndexOptions::new().store_signatures(true);
    let mut pair = Pair::new("signatures", 2_000, options);
    pair.insert(30_000, 0);
    pair.remove(17);
    pair.insert(17, 5);
    pair.index.flush().expect("delta");
    pair.insert(30_001, 0);
    pair.remove(30_000);
    pair.index.flush().expect("delta");
    assert_eq!(pair.index.stats().expect("stats").delta_generations, 2);

    let check = |index: &PersistentIndex32, context: &str| {
        for (key, variant) in [(17, 5), (30_001, 0), (3, 0)] {
            assert_eq!(
                index.signature(key).expect("signature"),
                Some(sketch(key, variant)),
                "{context}: signature {key}"
            );
        }
        assert_eq!(index.signature(30_000).expect("signature"), None);
        let query = sketch(17, 5);
        let scored = index
            .query_scored(&query, ScoreFilter::new())
            .expect("scored");
        assert_eq!(
            sorted(scored.iter().map(|candidate| candidate.key).collect()),
            sorted(pair.reference.query(&query).expect("reference")),
            "{context}: scored keys"
        );
        for candidate in &scored {
            let candidate_sketch = sketch(candidate.key, pair.live[&candidate.key]);
            let expected = query.jaccard(&candidate_sketch).expect("jaccard");
            assert!(
                (candidate.similarity - expected).abs() < 1e-12,
                "{context}: similarity of {}",
                candidate.key
            );
        }
        assert_eq!(scored[0].key, 17);
    };
    check(&pair.index, "after deltas");
    let reopened = PersistentIndex32::open(&pair.path).expect("reopen");
    check(&reopened, "reopened");
    drop(reopened);
    pair.index.compact().expect("compact");
    check(&pair.index, "compacted");
    pair.assert_matches(&pair.index, "compacted");
}

#[test]
fn affine64_indexes_append_deltas() {
    let path = test_path("affine64");
    cleanup(&path);
    let mut index = PersistentIndex64::create(&path, THRESHOLD, NUM_PERM, SEED).expect("create");
    let stats = index.stats().expect("stats");
    let mut reference = LshIndex64::with_params(
        THRESHOLD,
        NUM_PERM,
        SEED,
        LshParams::new(stats.bands, stats.rows),
    )
    .expect("reference");
    let sketches: Vec<_> = (0..2_000).map(|item| sketch64(item, 0)).collect();
    index
        .insert_many((0..2_000).zip(&sketches))
        .expect("insert");
    reference
        .insert_many((0..2_000).zip(&sketches))
        .expect("reference");
    index.sync().expect("base");
    let late = sketch64(4_000, 0);
    index.insert(4_000, &late).expect("insert");
    reference.insert(4_000, &late).expect("reference");
    assert!(index.remove(3));
    assert!(reference.remove(3));
    index.flush().expect("delta");
    assert_eq!(index.stats().expect("stats").delta_generations, 1);
    let reopened = PersistentIndex64::open(&path).expect("reopen");
    for probe in [0, 3, 4_000, 1_234] {
        let query = sketch64(probe, 0);
        assert_eq!(
            sorted(reopened.query(&query).expect("query")),
            sorted(reference.query(&query).expect("reference"))
        );
    }
    drop((index, reopened));
    cleanup(&path);
}

#[test]
fn base_only_reader_check_uses_the_committed_file() {
    // A plain snapshot is still readable by base-only readers.
    let pair = Pair::new("plain", 200, PersistentIndexOptions::new());
    let bytes = fs::read(&pair.path).expect("read");
    let layout = FileLayout::read_from(&mut Cursor::new(&bytes)).expect("plain snapshot");
    assert!(layout.deltas().is_empty());
}
