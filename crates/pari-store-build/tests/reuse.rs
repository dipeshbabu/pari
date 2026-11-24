//! The opt-in bucket reuse path must write exactly what the full rebuild
//! writes, and fall back to the rebuild whenever the source differs.

use std::{
    fs,
    io::Cursor,
    path::{Path, PathBuf},
    sync::atomic::{AtomicU64, Ordering},
};

use pari_core::MinHash32;
use pari_format::{
    decode_bucket_segment, encode_bucket_segment, read_bucket_members, BucketKey, BucketRecord,
    FileLayout, IndexFile, Section, SectionKind,
};
use pari_store::PersistentIndex32;
use pari_store_build::{build_external, BuildOptions, BuildStats};

static NEXT: AtomicU64 = AtomicU64::new(0);

struct Fixture(PathBuf);

impl Fixture {
    fn new() -> Self {
        let path = std::env::temp_dir().join(format!(
            "pari-reuse-{}-{}",
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

/// A closed persistent snapshot whose items share some buckets.
fn persistent_source(path: &Path, items: u64) {
    let mut index = PersistentIndex32::create(path, 0.5, 64, 3).expect("create");
    let sketches: Vec<_> = (0..items)
        .map(|item| {
            let mut sketch = MinHash32::new(64, 3).expect("sketch");
            for value in (item / 3) * 100..(item / 3) * 100 + 30 + item % 3 {
                sketch.update(&value.to_le_bytes());
            }
            sketch
        })
        .collect();
    index
        .insert_many(
            sketches
                .iter()
                .zip(0_u64..)
                .map(|(sketch, key)| (key, sketch)),
        )
        .expect("insert");
    index.close().expect("close");
}

/// Build with and without reuse, check both outputs match, and return the
/// reuse build's stats.
fn build_both(fixture: &Fixture, source: &Path, name: &str) -> BuildStats {
    let rebuilt = fixture.path(&format!("{name}-rebuilt"));
    let reused = fixture.path(&format!("{name}-reused"));
    let full = build_external(
        source,
        &rebuilt,
        BuildOptions {
            max_buffer_records: 97,
            reuse_source_buckets: false,
        },
    )
    .expect("full rebuild");
    assert!(!full.reused_source_buckets);
    let stats = build_external(
        source,
        &reused,
        BuildOptions {
            reuse_source_buckets: true,
            ..BuildOptions::default()
        },
    )
    .expect("reuse build");
    assert_eq!(
        fs::read(&rebuilt).expect("rebuilt bytes"),
        fs::read(&reused).expect("reused bytes"),
        "reuse must write exactly what the rebuild writes"
    );
    assert_eq!(stats.records, full.records);
    assert_eq!(stats.buckets, full.buckets);
    assert_eq!(stats.output_bytes, full.output_bytes);
    stats
}

/// Rewrite the source's bucket sections from its decoded buckets.
fn rewrite_buckets(
    source: &Path,
    output: &Path,
    edit: impl FnOnce(&mut Vec<(BucketKey, Vec<u64>)>) -> Vec<usize>,
) {
    let bytes = fs::read(source).expect("source bytes");
    let file = IndexFile::decode(&bytes).expect("container");
    let mut reader = Cursor::new(&bytes);
    let layout = FileLayout::read_from(&mut reader).expect("layout");
    let bands = usize::try_from(layout.metadata().bands()).expect("bands");
    let mut buckets = Vec::new();
    for descriptor in layout
        .sections()
        .iter()
        .filter(|section| section.kind() == SectionKind::Buckets)
    {
        for location in
            decode_bucket_segment(&layout, &mut reader, *descriptor, bands).expect("directory")
        {
            let members = read_bucket_members(&layout, &mut reader, location).expect("members");
            buckets.push((location.key(), members));
        }
    }
    // Returns the number of buckets in each output section.
    let splits = edit(&mut buckets);
    let mut sections: Vec<_> = file
        .sections()
        .iter()
        .filter(|section| section.kind() != SectionKind::Buckets)
        .cloned()
        .collect();
    let mut rest = buckets.as_slice();
    for count in splits {
        let (group, tail) = rest.split_at(count);
        rest = tail;
        let records: Vec<_> = group
            .iter()
            .map(|(key, members)| BucketRecord::new(*key, members))
            .collect();
        sections.push(
            Section::new(
                SectionKind::Buckets,
                true,
                encode_bucket_segment(&records).expect("encode"),
            )
            .expect("section"),
        );
    }
    assert!(rest.is_empty());
    fs::write(
        output,
        IndexFile::new(file.metadata().clone(), sections)
            .expect("container")
            .encode()
            .expect("encode"),
    )
    .expect("write source");
}

#[test]
fn compacted_snapshot_buckets_are_reused_byte_for_byte() {
    let fixture = Fixture::new();
    let source = fixture.path("source");
    persistent_source(&source, 300);
    let stats = build_both(&fixture, &source, "compacted");
    assert!(stats.reused_source_buckets);
    assert_eq!(stats.spill_runs, 0);
}

#[test]
fn empty_snapshot_buckets_are_reused() {
    let fixture = Fixture::new();
    let source = fixture.path("source");
    persistent_source(&source, 0);
    let stats = build_both(&fixture, &source, "empty");
    assert!(stats.reused_source_buckets);
    assert_eq!(stats.buckets, 0);
}

#[test]
fn a_stale_member_falls_back_to_the_rebuild() {
    let fixture = Fixture::new();
    let source = fixture.path("source");
    persistent_source(&source, 60);
    let stale = fixture.path("stale");
    rewrite_buckets(&source, &stale, |buckets| {
        // Key 7 exists, but not in this bucket.
        let (_, members) = buckets
            .iter_mut()
            .find(|(_, members)| !members.contains(&7))
            .expect("a bucket without key 7");
        members.push(7);
        members.sort_unstable();
        vec![buckets.len()]
    });
    assert!(!build_both(&fixture, &stale, "stale").reused_source_buckets);
}

#[test]
fn a_moved_member_falls_back_to_the_rebuild() {
    let fixture = Fixture::new();
    let source = fixture.path("source");
    persistent_source(&source, 60);
    let moved = fixture.path("moved");
    rewrite_buckets(&source, &moved, |buckets| {
        // Same entry count and keys, but one member in the wrong bucket.
        let from = buckets
            .iter()
            .position(|(_, members)| members.len() > 1)
            .expect("a shared bucket");
        let key = buckets[from].1.pop().expect("member");
        let band = buckets[from].0.band();
        let to = buckets
            .iter()
            .enumerate()
            .position(|(index, (bucket, members))| {
                index != from && bucket.band() == band && !members.contains(&key)
            })
            .expect("another bucket in the band");
        buckets[to].1.push(key);
        buckets[to].1.sort_unstable();
        vec![buckets.len()]
    });
    assert!(!build_both(&fixture, &moved, "moved").reused_source_buckets);
}

#[test]
fn non_canonical_segment_breaks_fall_back_to_the_rebuild() {
    let fixture = Fixture::new();
    let source = fixture.path("source");
    persistent_source(&source, 60);
    let split = fixture.path("split");
    rewrite_buckets(&source, &split, |buckets| {
        let half = buckets.len() / 2;
        vec![half, buckets.len() - half]
    });
    assert!(!build_both(&fixture, &split, "split").reused_source_buckets);
}

#[test]
fn a_source_without_buckets_falls_back_to_the_rebuild() {
    let fixture = Fixture::new();
    let source = fixture.path("source");
    persistent_source(&source, 30);
    let bare = fixture.path("bare");
    rewrite_buckets(&source, &bare, |buckets| {
        buckets.clear();
        Vec::new()
    });
    assert!(!build_both(&fixture, &bare, "bare").reused_source_buckets);
}

#[test]
fn default_options_reuse_a_compacted_snapshot() {
    let fixture = Fixture::new();
    let source = fixture.path("source");
    persistent_source(&source, 120);
    let stats = build_external(&source, fixture.path("default"), BuildOptions::default())
        .expect("default build");
    assert!(stats.reused_source_buckets);
}
