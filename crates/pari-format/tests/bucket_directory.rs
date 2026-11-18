use std::{collections::BTreeMap, io::Cursor};

use pari_format::{
    decode_bucket_segment, encode_bucket_segment, read_bucket_members, Algorithm, BucketDirectory,
    BucketError, BucketKey, BucketRecord, CodecId, FileLayout, IndexFile, IndexMetadata, Section,
    SectionDescriptor, SectionKind, SignatureScheme,
};

const BANDS: usize = 2;

type Segment = Vec<(BucketKey, Vec<u64>)>;

fn container(segments: &[Segment]) -> Vec<u8> {
    let metadata = IndexMetadata::new(
        Algorithm::MinHashLsh,
        SignatureScheme::PariAffine32V1,
        CodecId::U64,
        8,
        7,
        0.8,
        2,
        4,
        0,
    )
    .expect("metadata");
    let sections = segments
        .iter()
        .map(|segment| {
            let records: Vec<_> = segment
                .iter()
                .map(|(key, members)| BucketRecord::new(*key, members))
                .collect();
            Section::new(
                SectionKind::Buckets,
                true,
                encode_bucket_segment(&records).expect("encode segment"),
            )
            .expect("section")
        })
        .collect();
    IndexFile::new(metadata, sections)
        .expect("file")
        .encode()
        .expect("encode")
}

/// Even hashes only, so odd hashes are guaranteed misses.
fn segment(band: u32, first_hash: u64, buckets: u64) -> Segment {
    (0..buckets)
        .map(|index| {
            let hash = first_hash + index * 2;
            let size = 1 + (hash % 5);
            (
                BucketKey::new(band, hash),
                (0..size).map(|member| hash * 10 + member).collect(),
            )
        })
        .collect()
}

fn bucket_sections(layout: &FileLayout) -> Vec<SectionDescriptor> {
    layout
        .sections()
        .iter()
        .copied()
        .filter(|descriptor| descriptor.kind() == SectionKind::Buckets)
        .collect()
}

fn open(bytes: &[u8]) -> (FileLayout, Cursor<&[u8]>, Vec<SectionDescriptor>) {
    let mut reader = Cursor::new(bytes);
    let layout = FileLayout::read_from(&mut reader).expect("layout");
    let sections = bucket_sections(&layout);
    (layout, reader, sections)
}

#[test]
fn finds_every_bucket_exactly_like_the_full_decoder() {
    // Section sizes straddle the 64-entry block boundary in different ways.
    let segments = vec![
        segment(0, 0, 300),
        segment(0, 10_000, 1),
        segment(0, 20_000, 128),
        segment(1, 0, 517),
    ];
    let bytes = container(&segments);
    let (layout, mut reader, sections) = open(&bytes);
    let directory =
        BucketDirectory::read(&layout, &mut reader, &sections, BANDS).expect("directory");

    let mut expected = Vec::new();
    for descriptor in &sections {
        expected.extend(
            decode_bucket_segment(&layout, &mut reader, *descriptor, BANDS).expect("decode"),
        );
    }
    assert_eq!(directory.len(), 946);
    assert_eq!(
        directory.len(),
        u64::try_from(expected.len()).expect("count")
    );
    assert!(!directory.is_empty());

    let members_by_key: BTreeMap<_, _> = segments.iter().flatten().cloned().collect();
    for location in &expected {
        let found = directory
            .find(&layout, &mut reader, location.key())
            .expect("lookup")
            .expect("present bucket");
        assert_eq!(found, *location);
        assert_eq!(
            read_bucket_members(&layout, &mut reader, found).expect("members"),
            members_by_key[&location.key()]
        );
    }

    for missing in [
        BucketKey::new(0, 1),
        BucketKey::new(0, 599),
        BucketKey::new(0, 9_999),
        BucketKey::new(0, 10_001),
        BucketKey::new(0, u64::MAX),
        BucketKey::new(1, 1_033),
        BucketKey::new(1, 1_034),
        BucketKey::new(2, 0),
    ] {
        assert_eq!(
            directory
                .find(&layout, &mut reader, missing)
                .expect("lookup"),
            None,
            "{missing:?}"
        );
    }

    let mut histogram = BTreeMap::<u32, u64>::new();
    for location in &expected {
        *histogram.entry(location.member_count()).or_default() += 1;
    }
    assert_eq!(
        directory.size_counts().collect::<Vec<_>>(),
        histogram.into_iter().collect::<Vec<_>>()
    );
}

#[test]
fn empty_segments_index_nothing() {
    let bytes = container(&[Vec::new()]);
    let (layout, mut reader, sections) = open(&bytes);
    let directory =
        BucketDirectory::read(&layout, &mut reader, &sections, BANDS).expect("directory");
    assert!(directory.is_empty());
    assert_eq!(directory.size_counts().count(), 0);
    assert_eq!(
        directory
            .find(&layout, &mut reader, BucketKey::new(0, 0))
            .expect("lookup"),
        None
    );
}

fn directory_entry_offset(sections: &[SectionDescriptor], section: usize, entry: u64) -> usize {
    usize::try_from(sections[section].payload_offset() + 40 + entry * 32).expect("offset")
}

#[test]
fn lookups_verify_the_block_they_read() {
    let segments = vec![segment(0, 0, 300)];
    let mut bytes = container(&segments);
    let (layout, directory, sections) = {
        let (layout, mut reader, sections) = open(&bytes);
        let directory =
            BucketDirectory::read(&layout, &mut reader, &sections, BANDS).expect("directory");
        (layout, directory, sections)
    };

    // Corrupt the member checksum of entry 130, inside the second block.
    let offset = directory_entry_offset(&sections, 0, 130) + 28;
    bytes[offset] ^= 0xFF;
    let mut reader = Cursor::new(bytes.as_slice());
    assert!(matches!(
        directory.find(&layout, &mut reader, BucketKey::new(0, 260)),
        Err(BucketError::DirectoryChecksumMismatch { .. })
    ));
    assert!(matches!(
        directory.find(&layout, &mut reader, BucketKey::new(0, 300)),
        Err(BucketError::DirectoryChecksumMismatch { .. })
    ));
    // Other blocks remain readable.
    assert!(directory
        .find(&layout, &mut reader, BucketKey::new(0, 0))
        .expect("first block")
        .is_some());
    assert!(directory
        .find(&layout, &mut reader, BucketKey::new(0, 598))
        .expect("third block")
        .is_some());
}

#[test]
fn opening_rejects_corrupt_directories() {
    let mut bytes = container(&[segment(0, 0, 10)]);
    let (_, _, sections) = open(&bytes);
    let offset = directory_entry_offset(&sections, 0, 3) + 28;
    bytes[offset] ^= 0x01;
    let (layout, mut reader, sections) = open(&bytes);
    assert!(matches!(
        BucketDirectory::read(&layout, &mut reader, &sections, BANDS),
        Err(BucketError::DirectoryChecksumMismatch { .. })
    ));
}

#[test]
fn opening_rejects_bands_beyond_metadata() {
    let bytes = container(&[segment(2, 0, 3)]);
    let (layout, mut reader, sections) = open(&bytes);
    assert!(matches!(
        BucketDirectory::read(&layout, &mut reader, &sections, BANDS),
        Err(BucketError::Invalid {
            reason: "bucket directory band exceeds index metadata"
        })
    ));
}

#[test]
fn opening_rejects_unsorted_keys_across_sections() {
    for overlapping in [segment(0, 598, 4), segment(0, 0, 1)] {
        let bytes = container(&[segment(0, 0, 300), overlapping]);
        let (layout, mut reader, sections) = open(&bytes);
        assert!(matches!(
            BucketDirectory::read(&layout, &mut reader, &sections, BANDS),
            Err(BucketError::Invalid {
                reason: "bucket directory contains duplicate or unsorted bucket keys"
            })
        ));
    }
}

#[test]
fn blocks_split_at_bands_and_resolve_prefix_collisions() {
    // One section holding three bands, with hashes that share their high 32
    // bits so the in-memory prefix filter cannot distinguish them.
    let mut records: Segment = Vec::new();
    for band in [0_u32, 2] {
        for index in 0..150_u64 {
            let hash = ((index / 3) << 32) | ((index % 3) * 4);
            records.push((
                BucketKey::new(band, hash),
                vec![u64::from(band) * 1_000 + index],
            ));
        }
    }
    let bytes = container(&[records.clone()]);
    let (layout, mut reader, sections) = open(&bytes);
    let directory = BucketDirectory::read(&layout, &mut reader, &sections, 3).expect("directory");
    assert_eq!(directory.len(), 300);
    for (key, members) in &records {
        let location = directory
            .find(&layout, &mut reader, *key)
            .expect("lookup")
            .expect("present");
        assert_eq!(location.key(), *key);
        assert_eq!(
            &read_bucket_members(&layout, &mut reader, location).expect("members"),
            members
        );
    }
    for missing in [
        // Same prefix as stored buckets, different low bits.
        BucketKey::new(0, (7 << 32) | 1),
        BucketKey::new(2, (49 << 32) | 9),
        // A band with no buckets, between and after populated bands.
        BucketKey::new(1, 0),
        BucketKey::new(1, 7 << 32),
        // Beyond the last prefix of a band.
        BucketKey::new(0, 50 << 32),
        BucketKey::new(2, u64::MAX),
    ] {
        assert_eq!(
            directory
                .find(&layout, &mut reader, missing)
                .expect("lookup"),
            None,
            "{missing:?}"
        );
    }
}
