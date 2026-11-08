use std::{
    fs,
    io::Cursor,
    path::{Path, PathBuf},
    sync::atomic::{AtomicU64, Ordering},
};

use pari_format::{
    decode_bucket_segment, encode_bucket_segment, read_bucket_members, Algorithm, BucketKey,
    BucketRecord, CodecId, FileLayout, IndexFile, IndexMetadata, Section, SectionKind,
    SignatureScheme,
};
use pari_store_build::{build_external, BuildOptions};
use pari_store_lazy::{build_from_snapshot, LazyIndex32, LazyIndex64, LazyStoreError};

static NEXT: AtomicU64 = AtomicU64::new(0);

struct Fixture(PathBuf);
impl Fixture {
    fn new() -> Self {
        let path = std::env::temp_dir().join(format!(
            "pari-validation-{}-{}",
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

fn snapshot(
    scheme: SignatureScheme,
    keys: &[u64],
    shared: bool,
    buckets: bool,
    tombstones: Option<bool>,
) -> Vec<u8> {
    let metadata = IndexMetadata::new(
        Algorithm::MinHashLsh,
        scheme,
        CodecId::U64,
        8,
        7,
        0.8,
        2,
        4,
        0,
    )
    .expect("metadata");
    let mut key_bytes = u64::try_from(keys.len())
        .expect("count")
        .to_le_bytes()
        .to_vec();
    let mut hashes = key_bytes.clone();
    for (position, key) in keys.iter().enumerate() {
        key_bytes.extend_from_slice(&key.to_le_bytes());
        let base = if shared {
            11
        } else {
            11 + u64::try_from(position).expect("position") * 2
        };
        hashes.extend_from_slice(&base.to_le_bytes());
        hashes.extend_from_slice(&(base + 1).to_le_bytes());
    }
    let mut sections = vec![
        Section::new(SectionKind::Keys, true, key_bytes).expect("keys"),
        Section::new(SectionKind::BandHashes, true, hashes).expect("hashes"),
    ];
    if buckets {
        let mut members = keys.to_vec();
        members.sort_unstable();
        members.dedup();
        let records = if members.is_empty() {
            vec![]
        } else {
            vec![
                BucketRecord::new(BucketKey::new(0, 11), &members),
                BucketRecord::new(BucketKey::new(1, 12), &members),
            ]
        };
        sections.push(
            Section::new(
                SectionKind::Buckets,
                true,
                encode_bucket_segment(&records).expect("buckets"),
            )
            .expect("section"),
        );
    }
    if let Some(required) = tombstones {
        sections
            .push(Section::new(SectionKind::Tombstones, required, vec![0; 8]).expect("extension"));
    }
    IndexFile::new(metadata, sections)
        .expect("container")
        .encode()
        .expect("encode")
}

fn lazy_count(path: &Path, scheme: SignatureScheme) -> Result<usize, LazyStoreError> {
    match scheme {
        SignatureScheme::PariAffine32V1 => LazyIndex32::open(path).map(|index| index.stats().items),
        SignatureScheme::PariAffine64V1 => LazyIndex64::open(path).map(|index| index.stats().items),
    }
}

#[test]
fn lazy_open_rejects_key_checksum_count_and_uniqueness_errors() {
    for scheme in [
        SignatureScheme::PariAffine32V1,
        SignatureScheme::PariAffine64V1,
    ] {
        for failure in ["checksum", "count", "overflow", "duplicate"] {
            let fixture = Fixture::new();
            let path = fixture.path("source");
            let mut bytes = snapshot(
                scheme,
                if failure == "duplicate" {
                    &[1, 9, 1]
                } else {
                    &[1, 9]
                },
                true,
                true,
                None,
            );
            if failure != "duplicate" {
                let mut cursor = Cursor::new(&bytes);
                let layout = FileLayout::read_from(&mut cursor).expect("layout");
                let descriptor = layout.section(SectionKind::Keys).expect("keys");
                let offset = usize::try_from(descriptor.payload_offset()).expect("offset");
                if failure == "checksum" {
                    bytes[offset + 8] ^= 1;
                } else {
                    let file = IndexFile::decode(&bytes).expect("file");
                    let mut sections = file.sections().to_vec();
                    let mut payload = sections[0].payload().to_vec();
                    payload[..8].copy_from_slice(
                        &(if failure == "overflow" { u64::MAX } else { 3 }).to_le_bytes(),
                    );
                    sections[0] = Section::new(SectionKind::Keys, true, payload).expect("keys");
                    bytes = IndexFile::new(file.metadata().clone(), sections)
                        .expect("file")
                        .encode()
                        .expect("encode");
                }
            }
            fs::write(&path, bytes).expect("write");
            assert!(
                lazy_count(&path, scheme).is_err(),
                "accepted {scheme:?} {failure}"
            );
        }
    }
}

#[test]
fn converters_reject_duplicate_keys_and_required_unknown_semantics() {
    for scheme in [
        SignatureScheme::PariAffine32V1,
        SignatureScheme::PariAffine64V1,
    ] {
        for failure in ["adjacent", "nonadjacent", "tombstone"] {
            let tombstone = failure == "tombstone";
            let fixture = Fixture::new();
            let source = fixture.path("source");
            let keys = if tombstone {
                vec![1, 9]
            } else if failure == "nonadjacent" {
                vec![9, 1, 9]
            } else {
                vec![9, 9]
            };
            fs::write(
                &source,
                snapshot(scheme, &keys, false, false, tombstone.then_some(true)),
            )
            .expect("source");
            for external in [false, true] {
                let output = fixture.path(if external { "external" } else { "reference" });
                let rejected = if external {
                    build_external(
                        &source,
                        &output,
                        BuildOptions {
                            max_buffer_records: 1,
                        },
                    )
                    .is_err()
                } else {
                    build_from_snapshot(&source, &output).is_err()
                };
                assert!(
                    rejected,
                    "accepted {scheme:?}, tombstone={tombstone}, external={external}"
                );
                assert!(!output.exists());
                assert_eq!(fs::read_dir(&fixture.0).expect("directory").count(), 1);
            }
        }
    }
}

#[test]
fn unordered_unique_legacy_keys_remain_supported_with_canonical_members() {
    for scheme in [
        SignatureScheme::PariAffine32V1,
        SignatureScheme::PariAffine64V1,
    ] {
        let fixture = Fixture::new();
        let source = fixture.path("source");
        let keys = [u64::MAX, 9, 0];
        fs::write(&source, snapshot(scheme, &keys, true, false, Some(false))).expect("source");
        let reference = fixture.path("reference");
        build_from_snapshot(&source, &reference).expect("valid legacy source");
        for size in [1, 3, usize::MAX] {
            let output = fixture.path(&format!("external-{size}"));
            build_external(
                &source,
                &output,
                BuildOptions {
                    max_buffer_records: size,
                },
            )
            .expect("external");
            assert_eq!(
                fs::read(&output).expect("external"),
                fs::read(&reference).expect("reference")
            );
            assert_eq!(lazy_count(&output, scheme).expect("lazy open"), 3);
            let mut reader = fs::File::open(&output).expect("open");
            let layout = FileLayout::read_from(&mut reader).expect("layout");
            let descriptor = layout.section(SectionKind::Buckets).expect("buckets");
            let locations =
                decode_bucket_segment(&layout, &mut reader, descriptor, 2).expect("directory");
            for location in locations {
                assert_eq!(
                    read_bucket_members(&layout, &mut reader, location).expect("members"),
                    [0, 9, u64::MAX]
                );
            }
        }
    }
}

#[test]
fn maximum_buffer_limit_does_not_preallocate_beyond_small_input() {
    for keys in [vec![], vec![0], vec![0, u64::MAX]] {
        let fixture = Fixture::new();
        let source = fixture.path("source");
        let output = fixture.path("output");
        fs::write(
            &source,
            snapshot(SignatureScheme::PariAffine32V1, &keys, true, false, None),
        )
        .expect("source");
        let result = std::panic::catch_unwind(|| {
            build_external(
                &source,
                &output,
                BuildOptions {
                    max_buffer_records: usize::MAX,
                },
            )
        });
        assert!(
            result.is_ok(),
            "large buffer limit panicked for a small source"
        );
        let stats = result.expect("no panic").expect("valid build");
        assert_eq!(
            stats.records,
            u64::try_from(keys.len() * 2).expect("records")
        );
        assert!(stats.peak_buffered_records <= keys.len() * 3);
    }
}

#[test]
fn external_builder_creates_a_missing_destination_parent() {
    let fixture = Fixture::new();
    let source = fixture.path("source");
    fs::write(
        &source,
        snapshot(SignatureScheme::PariAffine32V1, &[1, 9], true, false, None),
    )
    .expect("source");
    let output = fixture.path("new/nested/output");
    build_external(
        &source,
        &output,
        BuildOptions {
            max_buffer_records: 1,
        },
    )
    .expect("create parent");
    assert_eq!(
        lazy_count(&output, SignatureScheme::PariAffine32V1).expect("open"),
        2
    );
}

#[test]
fn key_validation_crosses_checksum_buffer_boundaries() {
    for scheme in [
        SignatureScheme::PariAffine32V1,
        SignatureScheme::PariAffine64V1,
    ] {
        let fixture = Fixture::new();
        let path = fixture.path("source");
        let mut keys = (0_u64..8193).collect::<Vec<_>>();
        fs::write(&path, snapshot(scheme, &keys, true, true, None)).expect("valid source");
        assert_eq!(
            lazy_count(&path, scheme).expect("multi-buffer catalog"),
            8193
        );
        keys[8192] = 8191;
        fs::write(&path, snapshot(scheme, &keys, true, true, None))
            .expect("duplicate across boundary");
        assert!(lazy_count(&path, scheme).is_err());
    }
}

#[test]
fn failed_build_preserves_existing_names_and_blocking_parent_files() {
    let fixture = Fixture::new();
    let source = fixture.path("source");
    fs::write(
        &source,
        snapshot(SignatureScheme::PariAffine32V1, &[9, 9], false, false, None),
    )
    .expect("source");
    let output = fixture.path("output");
    fs::write(&output, b"preserve").expect("existing");
    assert!(build_external(&source, &output, BuildOptions::default()).is_err());
    assert_eq!(fs::read(&output).expect("existing"), b"preserve");
    assert!(build_external(&source, output.join("child"), BuildOptions::default()).is_err());
    assert_eq!(fs::read(&output).expect("parent file"), b"preserve");
}

#[test]
fn empty_advanced_parameter_indexes_round_trip_without_runtime_band_tables() {
    use pari_index::LshParams;
    use pari_store::{PersistentIndex32, PersistentIndex64};
    for scheme in [
        SignatureScheme::PariAffine32V1,
        SignatureScheme::PariAffine64V1,
    ] {
        let fixture = Fixture::new();
        let source = fixture.path("source");
        let params = LshParams::new(16_384, 4);
        match scheme {
            SignatureScheme::PariAffine32V1 => {
                PersistentIndex32::create_with_params(&source, 0.8, 65_536, 7, params)
                    .expect("create")
                    .close()
                    .expect("close");
            }
            SignatureScheme::PariAffine64V1 => {
                PersistentIndex64::create_with_params(&source, 0.8, 65_536, 7, params)
                    .expect("create")
                    .close()
                    .expect("close");
            }
        }
        assert!(fs::metadata(&source).expect("small empty file").len() < 1024);
        assert_eq!(lazy_count(&source, scheme).expect("open"), 0);
        let output = fixture.path("converted");
        build_external(
            &source,
            &output,
            BuildOptions {
                max_buffer_records: usize::MAX,
            },
        )
        .expect("empty build");
        assert_eq!(lazy_count(&output, scheme).expect("converted"), 0);
    }
}

fn create_shared_source(
    source: &Path,
    scheme: SignatureScheme,
    sketch32: &pari_core::MinHash32,
    sketch64: &pari_core::MinHash64,
) {
    use pari_store::{PersistentIndex32, PersistentIndex64};
    match scheme {
        SignatureScheme::PariAffine32V1 => {
            let mut index = PersistentIndex32::create(source, 0.8, 8, 7).expect("create");
            index
                .insert_many([(0, sketch32), (u64::MAX, sketch32)])
                .expect("insert");
            index.close().expect("close");
        }
        SignatureScheme::PariAffine64V1 => {
            let mut index = PersistentIndex64::create(source, 0.8, 8, 7).expect("create");
            index
                .insert_many([(0, sketch64), (u64::MAX, sketch64)])
                .expect("insert");
            index.close().expect("close");
        }
    }
}

#[test]
fn lazy_queries_filter_stale_members_against_the_live_key_catalog() {
    use pari_core::{MinHash32, MinHash64};
    use pari_store::{PersistentIndex32, PersistentIndex64};
    for scheme in [
        SignatureScheme::PariAffine32V1,
        SignatureScheme::PariAffine64V1,
    ] {
        let fixture = Fixture::new();
        let source = fixture.path("source");
        let mut sketch32 = MinHash32::new(8, 7).expect("sketch");
        sketch32.update(b"known");
        let mut sketch64 = MinHash64::new(8, 7).expect("sketch");
        sketch64.update(b"known");
        create_shared_source(&source, scheme, &sketch32, &sketch64);
        let bytes = fs::read(&source).expect("source");
        let file = IndexFile::decode(&bytes).expect("container");
        let mut reader = Cursor::new(&bytes);
        let layout = FileLayout::read_from(&mut reader).expect("layout");
        let mut sections = file
            .sections()
            .iter()
            .filter(|s| s.kind() != SectionKind::Buckets)
            .cloned()
            .collect::<Vec<_>>();
        for descriptor in layout
            .sections()
            .iter()
            .filter(|s| s.kind() == SectionKind::Buckets)
        {
            let locations = decode_bucket_segment(
                &layout,
                &mut reader,
                *descriptor,
                usize::try_from(layout.metadata().bands()).expect("bands"),
            )
            .expect("directory");
            let rows = locations
                .into_iter()
                .map(|location| {
                    let mut members =
                        read_bucket_members(&layout, &mut reader, location).expect("members");
                    members.push(5);
                    members.sort_unstable();
                    (location.key(), members)
                })
                .collect::<Vec<_>>();
            let records = rows
                .iter()
                .map(|(key, members)| BucketRecord::new(*key, members))
                .collect::<Vec<_>>();
            sections.push(
                Section::new(
                    SectionKind::Buckets,
                    true,
                    encode_bucket_segment(&records).expect("encode"),
                )
                .expect("section"),
            );
        }
        fs::write(
            &source,
            IndexFile::new(file.metadata().clone(), sections)
                .expect("container")
                .encode()
                .expect("encode"),
        )
        .expect("source");
        match scheme {
            SignatureScheme::PariAffine32V1 => {
                let persistent = PersistentIndex32::open(&source).expect("persistent");
                let expected = persistent.query(&sketch32).expect("query");
                assert_eq!(expected, [0, u64::MAX]);
                let mut lazy = LazyIndex32::open(&source).expect("lazy");
                lazy.set_observability(true);
                assert_eq!(lazy.query(&sketch32).expect("query"), expected);
                assert_eq!(
                    lazy.query_many([&sketch32, &sketch32]).expect("batch"),
                    vec![expected.clone(), expected]
                );
                assert!(lazy.stats().queries.expect("metrics").candidate_rate() <= 1.0);
            }
            SignatureScheme::PariAffine64V1 => {
                let persistent = PersistentIndex64::open(&source).expect("persistent");
                let expected = persistent.query(&sketch64).expect("query");
                assert_eq!(expected, [0, u64::MAX]);
                let mut lazy = LazyIndex64::open(&source).expect("lazy");
                lazy.set_observability(true);
                assert_eq!(lazy.query(&sketch64).expect("query"), expected);
                assert_eq!(
                    lazy.query_many([&sketch64, &sketch64]).expect("batch"),
                    vec![expected.clone(), expected]
                );
                assert!(lazy.stats().queries.expect("metrics").candidate_rate() <= 1.0);
            }
        }
    }
}
