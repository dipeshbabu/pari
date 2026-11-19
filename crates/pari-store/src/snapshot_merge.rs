//! Snapshot rewrite by merging committed generations that are already sorted.
//!
//! Every committed generation stores its keys in ascending order and its
//! buckets in `(band, hash)` order with ascending members. A rewrite can
//! therefore merge those streams, dropping memberships that are no longer
//! live, instead of materializing every live key's band hashes and sorting
//! every membership again. Buckets are merged in parallel over key ranges.
//! Memory stays at a few read windows per generation plus the ranges in
//! flight, and the output is byte-for-byte the snapshot the sorting writer
//! produces for the same live state.

use std::{
    cmp::Reverse,
    collections::{BTreeMap, BinaryHeap, HashSet},
    fs::File,
    hash::{BuildHasherDefault, Hasher},
    io::{self, Read, Seek, SeekFrom, Write},
};

use pari_format::{
    bucket_record_size, encode_bucket_segment_parts, BucketDirectory, BucketKey, BucketWriteRecord,
    ContainerWriter, FileLayout, FormatError, SectionDescriptor, SectionKind,
    BUCKET_SEGMENT_HEADER_BYTES, MAX_SECTION_BYTES,
};
use rayon::prelude::*;

use crate::{key_metadata_pairs, read_count, read_u64, StoreError, COUNT_BYTES, U64_BYTES};

/// Read window per section stream, in bytes.
const MERGE_WINDOW_BYTES: usize = 1024 * 1024;
/// Total read-window budget across all generations, in bytes.
const MERGE_WINDOW_BUDGET: usize = 64 * 1024 * 1024;

/// Hasher for `u64` keys: a `splitmix64` finalizer, much cheaper than the
/// default `SipHash` for the per-membership liveness checks of a merge.
#[derive(Default)]
pub(crate) struct KeyHasher(u64);

impl Hasher for KeyHasher {
    fn write(&mut self, bytes: &[u8]) {
        for byte in bytes {
            self.write_u64(self.0.rotate_left(8) ^ u64::from(*byte));
        }
    }

    fn write_u64(&mut self, value: u64) {
        let mut mixed = value.wrapping_add(0x9E37_79B9_7F4A_7C15);
        mixed = (mixed ^ (mixed >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        mixed = (mixed ^ (mixed >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        self.0 = mixed ^ (mixed >> 31);
    }

    fn finish(&self) -> u64 {
        self.0
    }
}

/// A set of keys hashed with [`KeyHasher`].
pub(crate) type KeySet = HashSet<u64, BuildHasherDefault<KeyHasher>>;

/// One committed generation as a merge input.
pub(crate) struct MergeGeneration<'a> {
    /// The generation's sections, from the file layout.
    pub(crate) sections: &'a [SectionDescriptor],
    /// The generation's validated bucket directory index.
    pub(crate) directory: &'a BucketDirectory,
    /// Keys the generation stores that are no longer live there, or `None`
    /// when every key it stores is live.
    pub(crate) dead: Option<KeySet>,
}

impl MergeGeneration<'_> {
    fn is_live(&self, key: u64) -> bool {
        !self.dead.as_ref().is_some_and(|dead| dead.contains(&key))
    }
}

/// Committed state and pending changes to merge into one snapshot.
pub(crate) struct MergeSource<'a> {
    pub(crate) layout: &'a FileLayout,
    pub(crate) generations: Vec<MergeGeneration<'a>>,
    /// Band hashes of live keys no committed generation holds.
    pub(crate) overlay: &'a BTreeMap<u64, Vec<u64>>,
    pub(crate) bands: usize,
    /// Number of live keys, committed and pending.
    pub(crate) live_keys: usize,
}

impl MergeSource<'_> {
    fn window(&self) -> usize {
        // Two streams per generation are open at once.
        let streams = self.generations.len().saturating_mul(2).max(1);
        (MERGE_WINDOW_BUDGET / streams).clamp(64 * 1024, MERGE_WINDOW_BYTES)
    }
}

/// Write the `Keys` sections and then the `BandHashes` sections of the merged
/// snapshot, split exactly as the sorting writer splits them.
///
/// Rows are copied as raw little-endian bytes from the committed sections
/// into the output payloads, without decoding them.
pub(crate) fn write_merged_key_metadata<W: Write>(
    writer: &mut ContainerWriter<W>,
    file: &mut File,
    source: &MergeSource<'_>,
    target_bytes: usize,
) -> Result<(), StoreError> {
    let row_bytes = source
        .bands
        .max(1)
        .checked_mul(U64_BYTES)
        .ok_or(StoreError::LengthOverflow)?;
    let rows_per_section = (target_bytes.saturating_sub(COUNT_BYTES) / row_bytes).max(1);
    let chunks = source.live_keys.div_ceil(rows_per_section).max(1);
    let section_rows = |chunk: usize| {
        source
            .live_keys
            .saturating_sub(chunk * rows_per_section)
            .min(rows_per_section)
    };

    for with_rows in [false, true] {
        let width = if with_rows {
            source.bands * U64_BYTES
        } else {
            U64_BYTES
        };
        let mut keys = LiveKeyRows::new(source, file, with_rows)?;
        for chunk in 0..chunks {
            let rows = section_rows(chunk);
            let mut payload = Vec::with_capacity(COUNT_BYTES + rows * width);
            payload.extend_from_slice(
                &u64::try_from(rows)
                    .map_err(|_| StoreError::LengthOverflow)?
                    .to_le_bytes(),
            );
            for _ in 0..rows {
                if !keys.next_into(file, &mut payload)? {
                    return Err(StoreError::InvalidSnapshot {
                        reason: "merged keys do not match the live key count",
                    });
                }
            }
            let kind = if with_rows {
                SectionKind::BandHashes
            } else {
                SectionKind::Keys
            };
            writer.write_section(kind, true, &payload)?;
        }
        if keys.next_into(file, &mut Vec::new())? {
            return Err(StoreError::InvalidSnapshot {
                reason: "merged keys do not match the live key count",
            });
        }
    }
    Ok(())
}

/// Write the merged `Buckets` sections.
///
/// The bucket key space is split at directory-block boundaries into many
/// ranges. Ranges are merged on the Rayon pool, a window of a few per thread
/// at a time, each with its own positioned reader, and written in key order,
/// so the output matches a sequential merge while memory stays bounded by the
/// window.
pub(crate) fn write_merged_buckets<W: Write>(
    writer: &mut ContainerWriter<W>,
    file: &File,
    source: &MergeSource<'_>,
    target_bytes: usize,
) -> Result<(), StoreError> {
    // Pending memberships, sorted like the sorting writer sorts them.
    let mut overlay = Vec::new();
    for (key, hashes) in source.overlay {
        if hashes.len() != source.bands {
            return Err(StoreError::InvalidSnapshot {
                reason: "in-memory band-hash row has the wrong width",
            });
        }
        for (band, hash) in hashes.iter().enumerate() {
            let band = u32::try_from(band).map_err(|_| StoreError::LengthOverflow)?;
            overlay.push((BucketKey::new(band, *hash), *key));
        }
    }
    overlay.sort_unstable();

    let threads = rayon::current_num_threads().max(1);
    let splits = source
        .generations
        .iter()
        .max_by_key(|generation| generation.directory.len())
        .map(|generation| generation.directory.split_keys(threads * RANGES_PER_THREAD))
        .unwrap_or_default();
    let mut ranges = Vec::with_capacity(splits.len() + 1);
    let mut start = None;
    for split in splits {
        ranges.push((start, Some(split)));
        start = Some(split);
    }
    ranges.push((start, None));

    let mut output = PreparedSegments::new(writer, target_bytes);
    for window in ranges.chunks(threads * RANGES_IN_FLIGHT_PER_THREAD) {
        let merged: Vec<Result<MergedRange, StoreError>> = window
            .par_iter()
            .map(|&(start, end)| merge_range(file, source, &overlay, start, end))
            .collect();
        for range in merged {
            let range = range?;
            let mut at = 0;
            for record in range.records {
                let end = at
                    + usize::try_from(record.member_count())
                        .map_err(|_| StoreError::LengthOverflow)?
                        * U64_BYTES;
                output.push(record, &range.members[at..end])?;
                at = end;
            }
        }
    }
    output.flush()
}

/// Groups prepared buckets into `Buckets` sections exactly as the sorting
/// writer's segment builder does, so the two produce identical bytes.
struct PreparedSegments<'a, W: Write> {
    writer: &'a mut ContainerWriter<W>,
    target_bytes: usize,
    size: usize,
    records: Vec<BucketWriteRecord>,
    members: Vec<u8>,
}

impl<'a, W: Write> PreparedSegments<'a, W> {
    fn new(writer: &'a mut ContainerWriter<W>, target_bytes: usize) -> Self {
        Self {
            writer,
            target_bytes,
            size: BUCKET_SEGMENT_HEADER_BYTES,
            records: Vec::new(),
            members: Vec::new(),
        }
    }

    fn push(&mut self, record: BucketWriteRecord, members: &[u8]) -> Result<(), StoreError> {
        let count =
            usize::try_from(record.member_count()).map_err(|_| StoreError::LengthOverflow)?;
        let contribution = bucket_record_size(count)?;
        let single_size = BUCKET_SEGMENT_HEADER_BYTES
            .checked_add(contribution)
            .ok_or(StoreError::LengthOverflow)?;
        let length = u64::try_from(single_size).map_err(|_| StoreError::LengthOverflow)?;
        if length > MAX_SECTION_BYTES {
            return Err(FormatError::SectionTooLarge { length }.into());
        }
        if !self.records.is_empty() && self.size + contribution > self.target_bytes {
            self.flush()?;
        }
        self.records.push(record);
        self.members.extend_from_slice(members);
        self.size += contribution;
        Ok(())
    }

    fn flush(&mut self) -> Result<(), StoreError> {
        let payload = encode_bucket_segment_parts(&self.records, &self.members)?;
        self.writer
            .write_section(SectionKind::Buckets, true, &payload)?;
        self.records.clear();
        self.members.clear();
        self.size = BUCKET_SEGMENT_HEADER_BYTES;
        Ok(())
    }
}

/// Key ranges per worker thread: enough to balance uneven ranges.
const RANGES_PER_THREAD: usize = 32;
/// Ranges merged and buffered at once per worker thread.
const RANGES_IN_FLIGHT_PER_THREAD: usize = 2;

/// The live buckets of one key range, in key order, with their member lists
/// already encoded and checksummed.
struct MergedRange {
    records: Vec<BucketWriteRecord>,
    members: Vec<u8>,
}

/// Merge the buckets of every generation and the overlay in `[start, end)`.
fn merge_range(
    file: &File,
    source: &MergeSource<'_>,
    overlay: &[(BucketKey, u64)],
    start: Option<BucketKey>,
    end: Option<BucketKey>,
) -> Result<MergedRange, StoreError> {
    let mut reader = PositionedFile::new(file);
    let mut cursors: Vec<_> = source
        .generations
        .iter()
        .map(|generation| generation.directory.range_cursor(start, end))
        .collect();
    let first = start.map_or(0, |start| overlay.partition_point(|(key, _)| *key < start));
    let last = end.map_or(overlay.len(), |end| {
        overlay.partition_point(|(key, _)| *key < end)
    });
    let mut overlay = overlay[first..last].iter().peekable();

    let mut heads: Vec<Vec<u64>> = vec![Vec::new(); cursors.len()];
    let mut heap = BinaryHeap::with_capacity(cursors.len());
    for (index, cursor) in cursors.iter_mut().enumerate() {
        if let Some(key) = cursor.next_bucket(source.layout, &mut reader, &mut heads[index])? {
            heap.push(Reverse((key, index)));
        }
    }

    let mut output = MergedRange {
        records: Vec::new(),
        members: Vec::new(),
    };
    let mut members = Vec::new();
    loop {
        let committed = heap.peek().map(|Reverse((key, _))| *key);
        let pending = overlay.peek().map(|(key, _)| *key);
        let bucket = match (committed, pending) {
            (None, None) => break,
            (Some(key), None) | (None, Some(key)) => key,
            (Some(left), Some(right)) => left.min(right),
        };
        members.clear();
        while let Some(&Reverse((key, index))) = heap.peek() {
            if key != bucket {
                break;
            }
            heap.pop();
            let generation = &source.generations[index];
            if generation.dead.is_none() {
                members.extend_from_slice(&heads[index]);
            } else {
                members.extend(
                    heads[index]
                        .iter()
                        .copied()
                        .filter(|member| generation.is_live(*member)),
                );
            }
            if let Some(next) =
                cursors[index].next_bucket(source.layout, &mut reader, &mut heads[index])?
            {
                heap.push(Reverse((next, index)));
            }
        }
        while let Some(&&(key, member)) = overlay.peek() {
            if key != bucket {
                break;
            }
            members.push(member);
            overlay.next();
        }
        if members.is_empty() {
            continue;
        }
        members.sort_unstable();
        if members.windows(2).any(|pair| pair[0] == pair[1]) {
            return Err(StoreError::InvalidSnapshot {
                reason: "a live key is stored by more than one generation",
            });
        }
        let start = output.members.len();
        for member in &members {
            output.members.extend_from_slice(&member.to_le_bytes());
        }
        let checksum = crc32fast::hash(&output.members[start..]);
        let count = u32::try_from(members.len()).map_err(|_| StoreError::LengthOverflow)?;
        output
            .records
            .push(BucketWriteRecord::new(bucket, count, checksum));
    }
    Ok(output)
}

/// A reader over a shared file that keeps its own position and reads with
/// positioned I/O, so several threads can read one file handle at once.
struct PositionedFile<'a> {
    file: &'a File,
    position: u64,
}

impl<'a> PositionedFile<'a> {
    const fn new(file: &'a File) -> Self {
        Self { file, position: 0 }
    }
}

impl Read for PositionedFile<'_> {
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        #[cfg(unix)]
        let read = std::os::unix::fs::FileExt::read_at(self.file, buffer, self.position)?;
        #[cfg(windows)]
        let read = std::os::windows::fs::FileExt::seek_read(self.file, buffer, self.position)?;
        self.position += u64::try_from(read).map_err(io::Error::other)?;
        Ok(read)
    }
}

impl Seek for PositionedFile<'_> {
    fn seek(&mut self, target: SeekFrom) -> io::Result<u64> {
        let position = match target {
            SeekFrom::Start(offset) => Some(offset),
            SeekFrom::Current(delta) => self.position.checked_add_signed(delta),
            SeekFrom::End(delta) => self.file.metadata()?.len().checked_add_signed(delta),
        };
        self.position = position.ok_or_else(|| {
            io::Error::new(io::ErrorKind::InvalidInput, "seek to a negative position")
        })?;
        Ok(self.position)
    }
}

/// Live keys in ascending order, merged from every generation and the overlay.
///
/// [`Self::next_into`] appends the next key, or with `with_rows` its band-hash
/// row instead, as raw little-endian bytes.
struct LiveKeyRows<'a> {
    source: &'a MergeSource<'a>,
    streams: Vec<KeyRowStream>,
    /// Current key and raw row bytes of each generation's stream.
    heads: Vec<(u64, Vec<u8>)>,
    heap: BinaryHeap<Reverse<(u64, usize)>>,
    overlay: std::iter::Peekable<std::collections::btree_map::Iter<'a, u64, Vec<u64>>>,
    with_rows: bool,
}

impl<'a> LiveKeyRows<'a> {
    fn new(
        source: &'a MergeSource<'a>,
        file: &mut File,
        with_rows: bool,
    ) -> Result<Self, StoreError> {
        let window = source.window();
        let mut streams = Vec::with_capacity(source.generations.len());
        for generation in &source.generations {
            streams.push(KeyRowStream::new(
                key_metadata_pairs(generation.sections)?,
                source.bands,
                window,
                with_rows,
            ));
        }
        let mut rows = Self {
            source,
            heads: vec![(0, Vec::new()); streams.len()],
            streams,
            heap: BinaryHeap::new(),
            overlay: source.overlay.iter().peekable(),
            with_rows,
        };
        for index in 0..rows.streams.len() {
            rows.advance(index, file)?;
        }
        Ok(rows)
    }

    /// Load the next live key of generation `index` into its head.
    fn advance(&mut self, index: usize, file: &mut File) -> Result<(), StoreError> {
        let generation = &self.source.generations[index];
        let (key, row) = &mut self.heads[index];
        while let Some(next) = self.streams[index].next(self.source.layout, file, row)? {
            if generation.is_live(next) {
                *key = next;
                self.heap.push(Reverse((next, index)));
                return Ok(());
            }
        }
        Ok(())
    }

    /// Append the next live key (or its row) to `output`; return `false`
    /// after the last key.
    fn next_into(&mut self, file: &mut File, output: &mut Vec<u8>) -> Result<bool, StoreError> {
        let committed = self.heap.peek().map(|Reverse((key, _))| *key);
        let pending = self.overlay.peek().map(|(key, _)| **key);
        let take_committed = match (committed, pending) {
            (None, None) => return Ok(false),
            (Some(left), Some(right)) if left == right => {
                return Err(StoreError::InvalidSnapshot {
                    reason: "a pending key is also live in a committed generation",
                });
            }
            (Some(left), Some(right)) => left < right,
            (Some(_), None) => true,
            (None, Some(_)) => false,
        };
        if !take_committed {
            let (key, row) = self.overlay.next().ok_or(StoreError::InvalidSnapshot {
                reason: "merged overlay ended early",
            })?;
            if self.with_rows {
                if row.len() != self.source.bands {
                    return Err(StoreError::InvalidSnapshot {
                        reason: "in-memory band-hash row has the wrong width",
                    });
                }
                for hash in row {
                    output.extend_from_slice(&hash.to_le_bytes());
                }
            } else {
                output.extend_from_slice(&key.to_le_bytes());
            }
            return Ok(true);
        }
        let Reverse((key, index)) = self.heap.pop().ok_or(StoreError::InvalidSnapshot {
            reason: "merged keys ended early",
        })?;
        if self.with_rows {
            output.extend_from_slice(&self.heads[index].1);
        } else {
            output.extend_from_slice(&key.to_le_bytes());
        }
        self.advance(index, file)?;
        if let Some(&Reverse((next, _))) = self.heap.peek() {
            if next == key {
                return Err(StoreError::InvalidSnapshot {
                    reason: "a live key is stored by more than one generation",
                });
            }
        }
        Ok(true)
    }
}

/// Keys, and optionally raw band-hash rows, of one generation in file order.
struct KeyRowStream {
    pairs: std::vec::IntoIter<(SectionDescriptor, SectionDescriptor)>,
    bands: usize,
    window: usize,
    with_rows: bool,
    keys: Option<SectionStream>,
    rows: Option<SectionStream>,
    remaining: usize,
    previous: Option<u64>,
}

impl KeyRowStream {
    fn new(
        pairs: Vec<(SectionDescriptor, SectionDescriptor)>,
        bands: usize,
        window: usize,
        with_rows: bool,
    ) -> Self {
        Self {
            pairs: pairs.into_iter(),
            bands,
            window,
            with_rows,
            keys: None,
            rows: None,
            remaining: 0,
            previous: None,
        }
    }

    /// Return the next key and, with rows, replace `row` with its raw bytes.
    fn next(
        &mut self,
        layout: &FileLayout,
        file: &mut File,
        row: &mut Vec<u8>,
    ) -> Result<Option<u64>, StoreError> {
        while self.remaining == 0 {
            if let Some(keys) = self.keys.take() {
                keys.finish()?;
            }
            if let Some(rows) = self.rows.take() {
                rows.finish()?;
            }
            let Some((keys_descriptor, rows_descriptor)) = self.pairs.next() else {
                return Ok(None);
            };
            let mut keys = SectionStream::new(keys_descriptor, self.window);
            let count = read_count(keys.read(layout, file, COUNT_BYTES)?)?;
            let expected = count
                .checked_mul(U64_BYTES)
                .and_then(|bytes| bytes.checked_add(COUNT_BYTES))
                .ok_or(StoreError::LengthOverflow)?;
            if u64::try_from(expected).ok() != Some(keys_descriptor.payload_length()) {
                return Err(StoreError::InvalidSnapshot {
                    reason: "keys section length does not match its record count",
                });
            }
            let row_bytes = self
                .bands
                .checked_mul(U64_BYTES)
                .ok_or(StoreError::LengthOverflow)?;
            let expected_rows = count
                .checked_mul(row_bytes)
                .and_then(|bytes| bytes.checked_add(COUNT_BYTES))
                .ok_or(StoreError::LengthOverflow)?;
            if u64::try_from(expected_rows).ok() != Some(rows_descriptor.payload_length()) {
                return Err(StoreError::InvalidSnapshot {
                    reason: "band-hash section length does not match metadata and record count",
                });
            }
            if self.with_rows {
                let mut rows = SectionStream::new(rows_descriptor, self.window);
                if read_count(rows.read(layout, file, COUNT_BYTES)?)? != count {
                    return Err(StoreError::InvalidSnapshot {
                        reason: "key and band-hash record counts differ",
                    });
                }
                self.rows = Some(rows);
            }
            self.keys = Some(keys);
            self.remaining = count;
        }
        self.remaining -= 1;
        let keys = self.keys.as_mut().ok_or(StoreError::InvalidSnapshot {
            reason: "key stream lost its section",
        })?;
        let key = read_u64(keys.read(layout, file, U64_BYTES)?)?;
        if self.previous.is_some_and(|previous| previous >= key) {
            return Err(StoreError::InvalidSnapshot {
                reason: "generation keys are not in ascending order",
            });
        }
        self.previous = Some(key);
        if let Some(rows) = self.rows.as_mut() {
            row.clear();
            row.extend_from_slice(rows.read(layout, file, self.bands * U64_BYTES)?);
        }
        Ok(Some(key))
    }
}

/// Sequential reader over one section through a bounded window, verifying
/// the section checksum once every byte has been read.
struct SectionStream {
    descriptor: SectionDescriptor,
    window: usize,
    hasher: crc32fast::Hasher,
    /// Section offset of `buffer[0]`.
    buffer_start: u64,
    buffer: Vec<u8>,
    position: usize,
}

impl SectionStream {
    fn new(descriptor: SectionDescriptor, window: usize) -> Self {
        Self {
            descriptor,
            window,
            hasher: crc32fast::Hasher::new(),
            buffer_start: 0,
            buffer: Vec::new(),
            position: 0,
        }
    }

    /// Return the next `length` bytes of the section.
    fn read(
        &mut self,
        layout: &FileLayout,
        file: &mut File,
        length: usize,
    ) -> Result<&[u8], StoreError> {
        if self.buffer.len() - self.position < length {
            let consumed =
                u64::try_from(self.buffer.len()).map_err(|_| StoreError::LengthOverflow)?;
            let mut carry = self.buffer.split_off(self.position);
            let start = self
                .buffer_start
                .checked_add(consumed)
                .ok_or(StoreError::LengthOverflow)?;
            let remaining = self.descriptor.payload_length().saturating_sub(start);
            let wanted = (length - carry.len()).max(self.window);
            let read = usize::try_from(remaining.min(u64::try_from(wanted).unwrap_or(u64::MAX)))
                .map_err(|_| StoreError::LengthOverflow)?;
            let fresh = layout.read_section_range(file, self.descriptor, start, read)?;
            self.hasher.update(&fresh);
            let carried = u64::try_from(carry.len()).map_err(|_| StoreError::LengthOverflow)?;
            carry.extend_from_slice(&fresh);
            self.buffer = carry;
            self.buffer_start = start - carried;
            self.position = 0;
            if self.buffer.len() < length {
                return Err(StoreError::InvalidSnapshot {
                    reason: "section ended before its records",
                });
            }
        }
        let start = self.position;
        self.position += length;
        Ok(&self.buffer[start..self.position])
    }

    /// Check that every byte was read and the section checksum matches.
    fn finish(self) -> Result<(), StoreError> {
        let consumed = self
            .buffer_start
            .checked_add(u64::try_from(self.position).map_err(|_| StoreError::LengthOverflow)?)
            .ok_or(StoreError::LengthOverflow)?;
        if consumed != self.descriptor.payload_length() {
            return Err(StoreError::InvalidSnapshot {
                reason: "section has trailing bytes after its records",
            });
        }
        let actual = self.hasher.finalize();
        if actual != self.descriptor.checksum() {
            return Err(StoreError::Format(
                pari_format::FormatError::SectionChecksumMismatch {
                    expected: self.descriptor.checksum(),
                    actual,
                },
            ));
        }
        Ok(())
    }
}
