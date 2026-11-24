//! Verified reuse of a source snapshot's bucket sections.
//!
//! A compacted `pari-store` snapshot already holds the bucket sections that the
//! external sort would write. When [`crate::BuildOptions::reuse_source_buckets`]
//! is set, the builder checks that each source section is exactly what the
//! full rebuild would write, then copies it instead of re-sorting every
//! `(band, hash, key)` record. Every check uses bounded memory:
//!
//! * Layout: headers, directories and checksums are valid, bucket keys rise
//!   strictly across all sections, members rise strictly within each bucket,
//!   and the segments break where the rebuild's greedy grouping would.
//! * Contents: a keyed hash of every `(band, hash, key)` entry, summed over the
//!   source band hashes and again over the bucket members, must agree, along
//!   with the entry count. The hash keys are random for each build, so stale or
//!   missing members are detected with overwhelming probability. Stored files
//!   are not adversarial inputs, so this is an integrity check, not a
//!   cryptographic one.
//!
//! If any check fails, the builder falls back to the full rebuild, so the
//! output is the same either way.

use std::{
    collections::hash_map::RandomState,
    fs::File,
    hash::{BuildHasher, Hasher as _},
    io::{self, BufReader, Read, Seek, SeekFrom},
    path::Path,
};

use crc32fast::Hasher;
use pari_format::{
    bucket_record_size, FileLayout, SectionDescriptor, SectionKind, BUCKET_SEGMENT_HEADER_BYTES,
    BUCKET_SEGMENT_TARGET_BYTES,
};

use crate::{BuildError, MetadataChunk, U64_BYTES, U64_BYTES_U64};

const SEGMENT_MAGIC: [u8; 8] = *b"PARIBKT\0";
const SEGMENT_VERSION: u16 = 1;
const DIRECTORY_ENTRY_BYTES: usize = 32;
const READ_BUFFER_BYTES: usize = 1 << 20;
const MEMBER_CHUNK_BYTES: usize = 64 * 1024;

/// Bucket sections that passed every check and can be copied unchanged.
#[derive(Debug)]
pub(crate) struct ReusableBuckets {
    pub(crate) sections: Vec<SectionDescriptor>,
    pub(crate) buckets: u64,
}

/// Why a check stopped: the source cannot be reused, or reading it failed.
enum Stop {
    Mismatch,
    Io(io::Error),
}

impl From<io::Error> for Stop {
    fn from(error: io::Error) -> Self {
        Self::Io(error)
    }
}

type Checked<T> = Result<T, Stop>;

fn require(condition: bool) -> Checked<()> {
    if condition {
        Ok(())
    } else {
        Err(Stop::Mismatch)
    }
}

fn to_usize(value: u64) -> Checked<usize> {
    usize::try_from(value).map_err(|_| Stop::Mismatch)
}

/// Return the source's bucket sections if the full rebuild would write
/// exactly the same bytes, or `None` to rebuild them.
///
/// `chunks` must already be checksum-verified, and their keys strictly
/// increasing, so each key appears once.
pub(crate) fn reusable_source_buckets(
    source: &Path,
    layout: &FileLayout,
    chunks: &[MetadataChunk],
    bands: usize,
    records: u64,
) -> Result<Option<ReusableBuckets>, BuildError> {
    match check(source, layout, chunks, bands, records) {
        Ok(found) => Ok(Some(found)),
        Err(Stop::Mismatch) => Ok(None),
        Err(Stop::Io(error)) => Err(error.into()),
    }
}

fn check(
    source: &Path,
    layout: &FileLayout,
    chunks: &[MetadataChunk],
    bands: usize,
    records: u64,
) -> Checked<ReusableBuckets> {
    let sections: Vec<SectionDescriptor> = layout
        .sections()
        .iter()
        .copied()
        .filter(|section| section.kind() == SectionKind::Buckets)
        .collect();
    require(!sections.is_empty())?;
    let hasher = EntryHasher::new();

    let file = File::open(source)?;
    let mut walker = SectionWalker {
        bands: u32::try_from(bands).map_err(|_| Stop::Mismatch)?,
        hasher: &hasher,
        previous: None,
        fingerprint: Fingerprint::default(),
        buffer: vec![0; MEMBER_CHUNK_BYTES],
    };
    let mut shapes = Vec::with_capacity(sections.len());
    for section in &sections {
        shapes.push(walker.section(&file, *section)?);
    }
    require(segments_are_canonical(&shapes)?)?;
    require(walker.fingerprint.count == records)?;

    let expected = band_hash_fingerprint(source, chunks, bands, &hasher)?;
    require(expected == walker.fingerprint)?;

    let buckets = shapes.iter().map(|shape| shape.buckets).sum();
    Ok(ReusableBuckets { sections, buckets })
}

/// Order-independent summary of a multiset of `(band, hash, key)` entries.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
struct Fingerprint {
    count: u64,
    sums: [u64; 2],
}

/// A hash of `(band, hash, key)` keyed with random seeds for each build.
struct EntryHasher {
    seeds: [u64; 6],
}

impl EntryHasher {
    fn new() -> Self {
        let state = RandomState::new();
        let mut seeds = [0; 6];
        for (index, seed) in (0_u64..).zip(seeds.iter_mut()) {
            let mut hasher = state.build_hasher();
            hasher.write_u64(index);
            // An odd multiplier keeps the final fold from discarding bits.
            *seed = hasher.finish() | 1;
        }
        Self { seeds }
    }

    fn add(&self, fingerprint: &mut Fingerprint, band: u32, hash: u64, key: u64) {
        let seeds = &self.seeds;
        let band = u64::from(band);
        fingerprint.count += 1;
        fingerprint.sums[0] = fingerprint.sums[0]
            .wrapping_add(fold(fold(hash ^ seeds[0], key ^ seeds[1]) ^ band, seeds[2]));
        fingerprint.sums[1] = fingerprint.sums[1]
            .wrapping_add(fold(fold(key ^ seeds[3], hash ^ seeds[4]) ^ band, seeds[5]));
    }
}

/// Multiply into 128 bits and fold the halves together.
fn fold(left: u64, right: u64) -> u64 {
    let product = u128::from(left).wrapping_mul(u128::from(right));
    let [low, high] = split_u128(product);
    low ^ high
}

fn split_u128(value: u128) -> [u64; 2] {
    let bytes = value.to_le_bytes();
    let mut low = [0; 8];
    let mut high = [0; 8];
    low.copy_from_slice(&bytes[..8]);
    high.copy_from_slice(&bytes[8..]);
    [u64::from_le_bytes(low), u64::from_le_bytes(high)]
}

/// Fingerprint every `(band, hash, key)` entry the source's band hashes imply.
fn band_hash_fingerprint(
    source: &Path,
    chunks: &[MetadataChunk],
    bands: usize,
    hasher: &EntryHasher,
) -> Checked<Fingerprint> {
    let mut fingerprint = Fingerprint::default();
    let row_bytes = bands.checked_mul(U64_BYTES).ok_or(Stop::Mismatch)?;
    let mut row = vec![0_u8; row_bytes];
    let mut key_raw = [0_u8; U64_BYTES];
    let mut key_reader = BufReader::with_capacity(READ_BUFFER_BYTES, File::open(source)?);
    let mut row_reader = BufReader::with_capacity(READ_BUFFER_BYTES, File::open(source)?);
    for chunk in chunks {
        key_reader.seek(SeekFrom::Start(chunk.keys.payload_offset() + U64_BYTES_U64))?;
        row_reader.seek(SeekFrom::Start(
            chunk.hashes.payload_offset() + U64_BYTES_U64,
        ))?;
        for _ in 0..chunk.count {
            key_reader.read_exact(&mut key_raw)?;
            row_reader.read_exact(&mut row)?;
            let key = u64::from_le_bytes(key_raw);
            for (band, raw) in (0_u32..).zip(row.chunks_exact(U64_BYTES)) {
                hasher.add(&mut fingerprint, band, read_u64(raw), key);
            }
        }
    }
    Ok(fingerprint)
}

/// The sizes the greedy segment grouping depends on.
#[derive(Debug, Clone, Copy)]
struct SegmentShape {
    buckets: u64,
    /// Header plus every bucket's directory entry and members.
    estimated_bytes: usize,
    /// The first bucket's directory entry and members.
    first_bucket_bytes: usize,
}

/// Whether the sections break exactly where the rebuild's grouping would.
///
/// The rebuild adds buckets to a segment until the next one would take it past
/// [`BUCKET_SEGMENT_TARGET_BYTES`], always taking at least one. An empty index
/// is a single empty segment.
fn segments_are_canonical(shapes: &[SegmentShape]) -> Checked<bool> {
    if shapes.iter().all(|shape| shape.buckets == 0) {
        return Ok(shapes.len() == 1);
    }
    for (index, shape) in shapes.iter().enumerate() {
        if shape.buckets == 0 {
            return Ok(false);
        }
        if shape.buckets > 1 && shape.estimated_bytes > BUCKET_SEGMENT_TARGET_BYTES {
            return Ok(false);
        }
        if let Some(next) = shapes.get(index + 1) {
            let with_next = shape
                .estimated_bytes
                .checked_add(next.first_bucket_bytes)
                .ok_or(Stop::Mismatch)?;
            if with_next <= BUCKET_SEGMENT_TARGET_BYTES {
                return Ok(false);
            }
        }
    }
    Ok(true)
}

struct SectionWalker<'a> {
    bands: u32,
    hasher: &'a EntryHasher,
    previous: Option<(u32, u64)>,
    fingerprint: Fingerprint,
    buffer: Vec<u8>,
}

impl SectionWalker<'_> {
    /// Verify one bucket section byte for byte and fingerprint its members.
    fn section(&mut self, file: &File, descriptor: SectionDescriptor) -> Checked<SegmentShape> {
        require(descriptor.required())?;
        let payload_length = to_usize(descriptor.payload_length())?;
        require(payload_length >= BUCKET_SEGMENT_HEADER_BYTES)?;
        let mut reader = BufReader::with_capacity(READ_BUFFER_BYTES, file);
        reader.seek(SeekFrom::Start(descriptor.payload_offset()))?;
        let mut outer = Hasher::new();

        let mut header = [0_u8; BUCKET_SEGMENT_HEADER_BYTES];
        reader.read_exact(&mut header)?;
        outer.update(&header);
        require(header[..8] == SEGMENT_MAGIC)?;
        require(read_u16(&header[8..10]) == SEGMENT_VERSION)?;
        require(usize::from(read_u16(&header[10..12])) == BUCKET_SEGMENT_HEADER_BYTES)?;
        require(header[36..40] == [0; 4])?;
        require(read_u32(&header[32..36]) == crc32fast::hash(&header[..32]))?;
        let count = to_usize(read_u64(&header[12..20]))?;
        let directory_bytes = to_usize(read_u64(&header[20..28]))?;
        require(count.checked_mul(DIRECTORY_ENTRY_BYTES) == Some(directory_bytes))?;
        // A canonical segment's directory never exceeds the segment target, so
        // this bounds memory even for a hostile source.
        require(directory_bytes <= BUCKET_SEGMENT_TARGET_BYTES)?;
        let member_start = BUCKET_SEGMENT_HEADER_BYTES + directory_bytes;
        require(member_start <= payload_length)?;

        let mut directory = vec![0_u8; directory_bytes];
        reader.read_exact(&mut directory)?;
        outer.update(&directory);
        require(read_u32(&header[28..32]) == crc32fast::hash(&directory))?;

        let mut shape = SegmentShape {
            buckets: u64::try_from(count).map_err(|_| Stop::Mismatch)?,
            estimated_bytes: BUCKET_SEGMENT_HEADER_BYTES,
            first_bucket_bytes: 0,
        };
        let mut expected_offset = member_start;
        for entry in directory.chunks_exact(DIRECTORY_ENTRY_BYTES) {
            let band = read_u32(&entry[0..4]);
            require(read_u32(&entry[4..8]) == 0)?;
            let hash = read_u64(&entry[8..16]);
            require(band < self.bands)?;
            require(self.previous < Some((band, hash)))?;
            self.previous = Some((band, hash));
            require(to_usize(read_u64(&entry[16..24]))? == expected_offset)?;
            let members = to_usize(u64::from(read_u32(&entry[24..28])))?;
            // The rebuild never writes an empty bucket.
            require(members > 0)?;
            let bucket_bytes = bucket_record_size(members).map_err(|_| Stop::Mismatch)?;
            if shape.first_bucket_bytes == 0 {
                shape.first_bucket_bytes = bucket_bytes;
            }
            shape.estimated_bytes = shape
                .estimated_bytes
                .checked_add(bucket_bytes)
                .ok_or(Stop::Mismatch)?;
            let member_bytes = members * U64_BYTES;
            expected_offset = expected_offset
                .checked_add(member_bytes)
                .ok_or(Stop::Mismatch)?;
            require(expected_offset <= payload_length)?;
            let checksum = self.members(&mut reader, &mut outer, band, hash, member_bytes)?;
            require(checksum == read_u32(&entry[28..32]))?;
        }
        require(expected_offset == payload_length)?;
        require(outer.finalize() == descriptor.checksum())?;
        Ok(shape)
    }

    /// Read one bucket's members, checking they rise strictly, and return
    /// their CRC32.
    fn members(
        &mut self,
        reader: &mut impl Read,
        outer: &mut Hasher,
        band: u32,
        hash: u64,
        mut remaining: usize,
    ) -> Checked<u32> {
        let mut checksum = Hasher::new();
        let mut previous = None;
        while remaining > 0 {
            let length = remaining.min(self.buffer.len());
            let bytes = &mut self.buffer[..length];
            reader.read_exact(bytes)?;
            checksum.update(bytes);
            outer.update(bytes);
            for raw in bytes.chunks_exact(U64_BYTES) {
                let key = read_u64(raw);
                require(previous < Some(key))?;
                previous = Some(key);
                self.hasher.add(&mut self.fingerprint, band, hash, key);
            }
            remaining -= length;
        }
        Ok(checksum.finalize())
    }
}

fn read_u16(bytes: &[u8]) -> u16 {
    let mut raw = [0; 2];
    raw.copy_from_slice(bytes);
    u16::from_le_bytes(raw)
}

fn read_u32(bytes: &[u8]) -> u32 {
    let mut raw = [0; 4];
    raw.copy_from_slice(bytes);
    u32::from_le_bytes(raw)
}

fn read_u64(bytes: &[u8]) -> u64 {
    let mut raw = [0; 8];
    raw.copy_from_slice(bytes);
    u64::from_le_bytes(raw)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn shape(buckets: u64, estimated_bytes: usize, first_bucket_bytes: usize) -> SegmentShape {
        SegmentShape {
            buckets,
            estimated_bytes,
            first_bucket_bytes,
        }
    }

    #[test]
    fn segment_breaks_must_match_the_greedy_grouping() {
        let target = BUCKET_SEGMENT_TARGET_BYTES;
        let check = |shapes: &[SegmentShape]| matches!(segments_are_canonical(shapes), Ok(true));
        // An empty index is exactly one empty segment.
        assert!(check(&[shape(0, 40, 0)]));
        assert!(!check(&[shape(0, 40, 0), shape(0, 40, 0)]));
        // A full segment followed by one whose first bucket did not fit.
        assert!(check(&[shape(9, target - 10, 40), shape(1, 80, 40)]));
        // The next segment's first bucket would have fit: not canonical.
        assert!(!check(&[shape(9, target - 100, 40), shape(1, 80, 40)]));
        // An oversized segment is canonical only with a single bucket.
        assert!(check(&[shape(1, target * 2, target * 2)]));
        assert!(!check(&[shape(2, target + 1, 40)]));
        // Empty segments never appear between buckets.
        assert!(!check(&[shape(1, 80, 40), shape(0, 40, 0)]));
    }

    #[test]
    fn the_fingerprint_ignores_order_but_not_contents() {
        let hasher = EntryHasher::new();
        let mut forward = Fingerprint::default();
        let mut backward = Fingerprint::default();
        let entries = [(0, 11, 1), (0, 11, 2), (1, 12, 1), (1, 13, 2)];
        for (band, hash, key) in entries {
            hasher.add(&mut forward, band, hash, key);
        }
        for (band, hash, key) in entries.iter().rev().copied() {
            hasher.add(&mut backward, band, hash, key);
        }
        assert_eq!(forward, backward);
        let mut moved = Fingerprint::default();
        for (band, hash, key) in [(0, 11, 1), (0, 11, 2), (1, 13, 1), (1, 12, 2)] {
            hasher.add(&mut moved, band, hash, key);
        }
        assert_eq!(moved.count, forward.count);
        assert_ne!(moved, forward);
    }
}
