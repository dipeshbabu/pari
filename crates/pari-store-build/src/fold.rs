//! Fold a persistent index's appended commit generations into one snapshot.
//!
//! A `pari-store` file may hold delta generations after its base snapshot.
//! Each generation removes keys (its tombstones) and then inserts keys with
//! their band hashes and, when the index stores them, signatures. Every
//! generation stores its keys in ascending order, so the live state is a
//! k-way merge of sorted streams: memory holds one small read buffer per
//! stream, never the key set.
//!
//! The folded snapshot is laid out exactly as a `pari-store` compaction lays
//! it out: live keys ascending, `Keys` then `BandHashes` sections split at
//! [`METADATA_SECTION_TARGET_BYTES`], then signature segments split at
//! [`SIGNATURE_SEGMENT_TARGET_BYTES`]. It holds no bucket sections; the
//! builder derives those from the band hashes as usual.

use std::{
    cmp::Reverse,
    collections::BinaryHeap,
    fs::{File, OpenOptions},
    io::{BufReader, BufWriter, Read, Seek, SeekFrom, Write},
    path::Path,
};

use crc32fast::Hasher;
use pari_format::{
    decode_signature_segment, encode_signature_segment, signature_row_bytes,
    validate_signature_coverage, FileLayout, SectionDescriptor, SectionKind,
    SIGNATURE_SEGMENT_HEADER_BYTES, SIGNATURE_SEGMENT_TARGET_BYTES,
};

use crate::{
    encode_header, encode_section_header, key_metadata_pairs, read_record_count, temp_path,
    validate_source_lengths, verify_section_streaming, BuildError, MetadataChunk, TemporaryFiles,
    U64_BYTES, U64_BYTES_U64,
};

/// Target payload size of one `Keys` or `BandHashes` section, matching
/// `pari-store` snapshots so a folded build equals compact-then-build.
pub(crate) const METADATA_SECTION_TARGET_BYTES: usize = 64 * 1024 * 1024;
const BASE_BUFFER_BYTES: usize = 1 << 20;
const DELTA_BUFFER_BYTES: usize = 8 * 1024;
const SIGNATURE_ROW_COUNT_OFFSET: usize = 32;

/// Event kinds in the order a generation applies them.
const REMOVE: u8 = 0;
const INSERT: u8 = 1;

fn invalid(reason: &'static str) -> BuildError {
    BuildError::InvalidSource { reason }
}

/// Write the live state of `source`, a file with appended generations, to
/// `output` as a base-only snapshot without bucket sections.
pub(crate) fn write_folded_snapshot(
    source: &Path,
    layout: &FileLayout,
    output: &Path,
    nonce: u128,
    temporary: &mut TemporaryFiles,
) -> Result<(), BuildError> {
    let metadata = layout.metadata();
    let bands = usize::try_from(metadata.bands()).map_err(|_| BuildError::LengthOverflow)?;
    let hash_width = bands
        .checked_mul(U64_BYTES)
        .ok_or(BuildError::LengthOverflow)?;
    let signature_width = signature_row_bytes(metadata.signature_scheme(), metadata.num_perm())
        .map_err(|_| BuildError::LengthOverflow)?;

    let mut file = File::open(source)?;
    let mut generations = Vec::with_capacity(layout.deltas().len() + 1);
    let sections = std::iter::once(layout.sections()).chain(
        layout
            .deltas()
            .iter()
            .map(pari_format::DeltaLayout::sections),
    );
    for (index, sections) in sections.enumerate() {
        generations.push(Generation::open(
            layout,
            &mut file,
            sections,
            index == 0,
            bands,
            hash_width,
            signature_width,
        )?);
    }
    let signatures = generations[0].signatures.is_some();
    if generations
        .iter()
        .any(|generation| generation.signatures.is_some() != signatures)
    {
        return Err(invalid(
            "every generation must store signatures if the base does",
        ));
    }

    let keys_path = temporary.track(temp_path(output, nonce, "folded-keys"));
    let hashes_path = temporary.track(temp_path(output, nonce, "folded-hashes"));
    let signatures_path = temporary.track(temp_path(output, nonce, "folded-signatures"));
    let mut keys_out = BufWriter::new(create_new(&keys_path)?);
    let mut hashes_out = BufWriter::new(create_new(&hashes_path)?);
    let mut signatures_out = BufWriter::new(create_new(&signatures_path)?);
    let live = merge(&mut file, &mut generations, |generation, key| {
        keys_out.write_all(&key.to_le_bytes())?;
        hashes_out.write_all(&generation.hash_row)?;
        if signatures {
            signatures_out.write_all(&generation.signature_row)?;
        }
        Ok(())
    })?;
    for writer in [&mut keys_out, &mut hashes_out, &mut signatures_out] {
        writer.flush()?;
    }
    drop((keys_out, hashes_out, signatures_out));

    let layout_plan = SnapshotPlan::new(live, hash_width, signatures.then_some(signature_width))?;
    let mut output_file = BufWriter::new(create_new(output)?);
    output_file.write_all(&encode_header(metadata, layout_plan.section_count()?))?;
    let mut keys_in = BufReader::new(File::open(&keys_path)?);
    for rows in layout_plan.metadata_sections() {
        write_counted_section(
            &mut output_file,
            &mut keys_in,
            SectionKind::Keys,
            rows,
            U64_BYTES,
        )?;
    }
    let mut hashes_in = BufReader::new(File::open(&hashes_path)?);
    for rows in layout_plan.metadata_sections() {
        write_counted_section(
            &mut output_file,
            &mut hashes_in,
            SectionKind::BandHashes,
            rows,
            hash_width,
        )?;
    }
    if signatures {
        let mut signatures_in = BufReader::new(File::open(&signatures_path)?);
        for (first_row, rows) in layout_plan.signature_segments() {
            let mut payload = encode_signature_segment(
                metadata.signature_scheme(),
                metadata.num_perm(),
                first_row,
                std::iter::empty(),
            )
            .map_err(|_| invalid("signature segment could not be encoded"))?;
            debug_assert_eq!(payload.len(), SIGNATURE_SEGMENT_HEADER_BYTES);
            payload[SIGNATURE_ROW_COUNT_OFFSET..SIGNATURE_ROW_COUNT_OFFSET + U64_BYTES]
                .copy_from_slice(&rows.to_le_bytes());
            append_rows(&mut payload, &mut signatures_in, rows, signature_width)?;
            write_section(&mut output_file, SectionKind::Signatures, false, &payload)?;
        }
    }
    output_file.flush()?;
    output_file.get_ref().sync_all()?;
    Ok(())
}

fn create_new(path: &Path) -> Result<File, BuildError> {
    Ok(OpenOptions::new()
        .read(true)
        .write(true)
        .create_new(true)
        .open(path)?)
}

/// Section sizes of the folded snapshot, as `pari-store` would write them.
struct SnapshotPlan {
    live: u64,
    rows_per_metadata: u64,
    metadata_sections: u64,
    signatures: Option<(u64, u64)>,
}

impl SnapshotPlan {
    fn new(
        live: u64,
        hash_width: usize,
        signature_width: Option<usize>,
    ) -> Result<Self, BuildError> {
        let row_bytes = hash_width.max(U64_BYTES);
        let rows_per_metadata =
            u64::try_from(((METADATA_SECTION_TARGET_BYTES - U64_BYTES) / row_bytes).max(1))
                .map_err(|_| BuildError::LengthOverflow)?;
        let metadata_sections = live.div_ceil(rows_per_metadata).max(1);
        let signatures = match signature_width {
            None => None,
            Some(width) => {
                let per_segment = u64::try_from(
                    ((SIGNATURE_SEGMENT_TARGET_BYTES - SIGNATURE_SEGMENT_HEADER_BYTES)
                        / width.max(1))
                    .max(1),
                )
                .map_err(|_| BuildError::LengthOverflow)?;
                Some((per_segment, live.div_ceil(per_segment).max(1)))
            }
        };
        Ok(Self {
            live,
            rows_per_metadata,
            metadata_sections,
            signatures,
        })
    }

    fn section_count(&self) -> Result<u32, BuildError> {
        let signature_sections = self.signatures.map_or(0, |(_, sections)| sections);
        u32::try_from(
            self.metadata_sections
                .checked_mul(2)
                .and_then(|count| count.checked_add(signature_sections))
                .ok_or(BuildError::LengthOverflow)?,
        )
        .map_err(|_| BuildError::LengthOverflow)
    }

    /// Rows in each `Keys` (and matching `BandHashes`) section.
    fn metadata_sections(&self) -> impl Iterator<Item = u64> + '_ {
        (0..self.metadata_sections).map(|section| {
            self.live
                .saturating_sub(section * self.rows_per_metadata)
                .min(self.rows_per_metadata)
        })
    }

    /// `(first_row, rows)` of each signature segment.
    fn signature_segments(&self) -> impl Iterator<Item = (u64, u64)> + '_ {
        let (per_segment, sections) = self.signatures.unwrap_or((1, 0));
        (0..sections).map(move |segment| {
            let first = segment * per_segment;
            (first, self.live.saturating_sub(first).min(per_segment))
        })
    }
}

/// Write a row-count-prefixed `Keys` or `BandHashes` section.
fn write_counted_section(
    output: &mut impl Write,
    rows_in: &mut impl Read,
    kind: SectionKind,
    rows: u64,
    width: usize,
) -> Result<(), BuildError> {
    let mut payload = rows.to_le_bytes().to_vec();
    append_rows(&mut payload, rows_in, rows, width)?;
    write_section(output, kind, true, &payload)
}

fn append_rows(
    payload: &mut Vec<u8>,
    rows_in: &mut impl Read,
    rows: u64,
    width: usize,
) -> Result<(), BuildError> {
    let bytes = usize::try_from(rows)
        .map_err(|_| BuildError::LengthOverflow)?
        .checked_mul(width)
        .ok_or(BuildError::LengthOverflow)?;
    let start = payload.len();
    payload.resize(
        start.checked_add(bytes).ok_or(BuildError::LengthOverflow)?,
        0,
    );
    rows_in.read_exact(&mut payload[start..])?;
    Ok(())
}

fn write_section(
    output: &mut impl Write,
    kind: SectionKind,
    required: bool,
    payload: &[u8],
) -> Result<(), BuildError> {
    let mut checksum = Hasher::new();
    checksum.update(payload);
    let length = u64::try_from(payload.len()).map_err(|_| BuildError::LengthOverflow)?;
    output.write_all(&encode_section_header(
        kind,
        required,
        length,
        checksum.finalize(),
    ))?;
    output.write_all(payload)?;
    Ok(())
}

/// Merge every generation's sorted inserts and tombstones, calling `emit`
/// for each live key in ascending order with the generation that holds it.
fn merge(
    file: &mut File,
    generations: &mut [Generation],
    mut emit: impl FnMut(&Generation, u64) -> Result<(), BuildError>,
) -> Result<u64, BuildError> {
    let mut heap = BinaryHeap::new();
    for (index, generation) in generations.iter_mut().enumerate() {
        let index = u32::try_from(index).map_err(|_| BuildError::LengthOverflow)?;
        if let Some(key) = generation.next_insert(file)? {
            heap.push(Reverse((key, index, INSERT)));
        }
        if let Some(key) = generation.next_tombstone(file)? {
            heap.push(Reverse((key, index, REMOVE)));
        }
    }
    let mut live = 0_u64;
    while let Some(Reverse((key, _, _))) = heap.peek().copied() {
        // Replay every event for this key in generation order: within one
        // generation, its tombstone comes before its insert.
        let mut holder: Option<usize> = None;
        while let Some(Reverse((next, index, kind))) = heap.peek().copied() {
            if next != key {
                break;
            }
            heap.pop();
            let position = usize::try_from(index).map_err(|_| BuildError::LengthOverflow)?;
            let generation = &mut generations[position];
            if kind == REMOVE {
                if holder.take().is_none() {
                    return Err(invalid("a tombstone names a key that is not live"));
                }
                if let Some(next) = generation.next_tombstone(file)? {
                    heap.push(Reverse((next, index, REMOVE)));
                }
            } else {
                if holder.replace(position).is_some() {
                    return Err(invalid("a generation inserts a key that is already live"));
                }
                generation.read_payload(file)?;
                if let Some(next) = generation.next_insert(file)? {
                    heap.push(Reverse((next, index, INSERT)));
                }
            }
        }
        if let Some(position) = holder {
            emit(&generations[position], key)?;
            live = live.checked_add(1).ok_or(BuildError::LengthOverflow)?;
        }
    }
    Ok(live)
}

/// One generation's sorted streams and its current row.
struct Generation {
    keys: Rows,
    hashes: Rows,
    signatures: Option<Rows>,
    tombstones: Rows,
    last_key: Option<u64>,
    last_tombstone: Option<u64>,
    hash_row: Vec<u8>,
    signature_row: Vec<u8>,
}

impl Generation {
    fn open(
        layout: &FileLayout,
        file: &mut File,
        sections: &[SectionDescriptor],
        base: bool,
        bands: usize,
        hash_width: usize,
        signature_width: usize,
    ) -> Result<Self, BuildError> {
        let capacity = if base {
            BASE_BUFFER_BYTES
        } else {
            DELTA_BUFFER_BYTES
        };
        let mut chunks = Vec::new();
        let mut rows = 0_usize;
        for (keys, hashes) in key_metadata_pairs(sections)? {
            verify_section_streaming(file, keys)?;
            verify_section_streaming(file, hashes)?;
            let count = read_record_count(file, keys)?;
            if count != read_record_count(file, hashes)? {
                return Err(invalid("key and band-hash record counts differ"));
            }
            validate_source_lengths(keys, hashes, count, bands)?;
            rows = rows.checked_add(count).ok_or(BuildError::LengthOverflow)?;
            chunks.push(MetadataChunk {
                keys,
                hashes,
                count,
            });
        }
        let ranges =
            |pick: fn(&MetadataChunk) -> SectionDescriptor| -> Result<Vec<(u64, u64)>, BuildError> {
                chunks
                    .iter()
                    .map(|chunk| {
                        Ok((
                            pick(chunk).payload_offset() + U64_BYTES_U64,
                            u64::try_from(chunk.count).map_err(|_| BuildError::LengthOverflow)?,
                        ))
                    })
                    .collect()
            };
        let keys = Rows::new(ranges(|chunk| chunk.keys)?, U64_BYTES, capacity);
        let hashes = Rows::new(ranges(|chunk| chunk.hashes)?, hash_width, capacity);

        let mut segments = Vec::new();
        let mut tombstone_sections = Vec::new();
        for descriptor in sections {
            match descriptor.kind() {
                SectionKind::Signatures => {
                    if descriptor.required() {
                        return Err(invalid("signature sections must be optional"));
                    }
                    verify_section_streaming(file, *descriptor)?;
                    segments.push(
                        decode_signature_segment(layout, file, *descriptor)
                            .map_err(|_| invalid("invalid signature section"))?,
                    );
                }
                SectionKind::Tombstones => tombstone_sections.push(*descriptor),
                _ => {}
            }
        }
        let signatures = if segments.is_empty() {
            None
        } else {
            validate_signature_coverage(
                &segments,
                u64::try_from(rows).map_err(|_| BuildError::LengthOverflow)?,
            )
            .map_err(|_| invalid("invalid signature section"))?;
            let ranges = segments
                .iter()
                .map(|segment| {
                    (
                        segment.section().payload_offset()
                            + u64::try_from(SIGNATURE_SEGMENT_HEADER_BYTES).unwrap_or(u64::MAX),
                        segment.row_count(),
                    )
                })
                .collect();
            Some(Rows::new(ranges, signature_width, capacity))
        };

        let tombstones = tombstone_rows(file, &tombstone_sections, base, capacity)?;

        Ok(Self {
            keys,
            hashes,
            signatures,
            tombstones,
            last_key: None,
            last_tombstone: None,
            hash_row: vec![0; hash_width],
            signature_row: vec![0; signature_width],
        })
    }

    fn next_insert(&mut self, file: &mut File) -> Result<Option<u64>, BuildError> {
        let mut raw = [0_u8; U64_BYTES];
        if !self.keys.next(file, &mut raw)? {
            return Ok(None);
        }
        let key = u64::from_le_bytes(raw);
        if self.last_key.is_some_and(|last| last >= key) {
            return Err(invalid(
                "a generation's keys are not strictly ascending; compact the index first",
            ));
        }
        self.last_key = Some(key);
        Ok(Some(key))
    }

    fn next_tombstone(&mut self, file: &mut File) -> Result<Option<u64>, BuildError> {
        let mut raw = [0_u8; U64_BYTES];
        if !self.tombstones.next(file, &mut raw)? {
            return Ok(None);
        }
        let key = u64::from_le_bytes(raw);
        if self.last_tombstone.is_some_and(|last| last >= key) {
            return Err(invalid("tombstones are not strictly ascending"));
        }
        self.last_tombstone = Some(key);
        Ok(Some(key))
    }

    /// Read the band hashes and signature of the key just returned by
    /// [`Self::next_insert`].
    fn read_payload(&mut self, file: &mut File) -> Result<(), BuildError> {
        if !self.hashes.next(file, &mut self.hash_row)? {
            return Err(invalid("band-hash rows end before the keys"));
        }
        if let Some(signatures) = &mut self.signatures {
            if !signatures.next(file, &mut self.signature_row)? {
                return Err(invalid("signature rows end before the keys"));
            }
        }
        Ok(())
    }
}

/// The removals of one generation; the base holds none.
fn tombstone_rows(
    file: &mut File,
    tombstone_sections: &[SectionDescriptor],
    base: bool,
    capacity: usize,
) -> Result<Rows, BuildError> {
    if base {
        // The base holds no removals; optional tombstone sections from a
        // future writer carry nothing this builder applies.
        Ok(Rows::new(Vec::new(), U64_BYTES, U64_BYTES))
    } else {
        let [descriptor] = tombstone_sections else {
            return Err(invalid(
                "a delta generation needs exactly one tombstone section",
            ));
        };
        if !descriptor.required() {
            return Err(invalid("a delta tombstone section must be required"));
        }
        verify_section_streaming(file, *descriptor)?;
        let count = read_record_count(file, *descriptor)?;
        let expected = count
            .checked_mul(U64_BYTES)
            .and_then(|bytes| bytes.checked_add(U64_BYTES))
            .ok_or(BuildError::LengthOverflow)?;
        if descriptor.payload_length()
            != u64::try_from(expected).map_err(|_| BuildError::LengthOverflow)?
        {
            return Err(invalid("tombstone section length disagrees with its count"));
        }
        Ok(Rows::new(
            vec![(
                descriptor.payload_offset() + U64_BYTES_U64,
                u64::try_from(count).map_err(|_| BuildError::LengthOverflow)?,
            )],
            U64_BYTES,
            capacity,
        ))
    }
}

/// Fixed-width rows spread over byte ranges of a shared file, read through a
/// private buffer so that thousands of streams need one file handle.
struct Rows {
    ranges: Vec<(u64, u64)>,
    next_range: usize,
    remaining: u64,
    file_offset: u64,
    width: usize,
    buffer: Vec<u8>,
    start: usize,
    end: usize,
}

impl Rows {
    fn new(ranges: Vec<(u64, u64)>, width: usize, capacity: usize) -> Self {
        Self {
            ranges,
            next_range: 0,
            remaining: 0,
            file_offset: 0,
            width,
            buffer: vec![0; capacity.max(width)],
            start: 0,
            end: 0,
        }
    }

    /// Copy the next row into `row`, or return `false` after the last one.
    fn next(&mut self, file: &mut File, row: &mut [u8]) -> Result<bool, BuildError> {
        while self.remaining == 0 {
            let Some(&(offset, rows)) = self.ranges.get(self.next_range) else {
                return Ok(false);
            };
            self.next_range += 1;
            self.file_offset = offset;
            self.remaining = rows;
            self.start = 0;
            self.end = 0;
        }
        if self.end - self.start < self.width {
            let buffered = self.end - self.start;
            self.buffer.copy_within(self.start..self.end, 0);
            self.start = 0;
            self.end = buffered;
            let unread = u64::try_from(self.width)
                .map_err(|_| BuildError::LengthOverflow)?
                .checked_mul(self.remaining)
                .ok_or(BuildError::LengthOverflow)?
                - u64::try_from(buffered).map_err(|_| BuildError::LengthOverflow)?;
            let want = usize::try_from(
                unread.min(u64::try_from(self.buffer.len() - buffered).unwrap_or(u64::MAX)),
            )
            .map_err(|_| BuildError::LengthOverflow)?;
            file.seek(SeekFrom::Start(self.file_offset))?;
            file.read_exact(&mut self.buffer[buffered..buffered + want])?;
            self.file_offset += u64::try_from(want).map_err(|_| BuildError::LengthOverflow)?;
            self.end += want;
        }
        row.copy_from_slice(&self.buffer[self.start..self.start + self.width]);
        self.start += self.width;
        self.remaining -= 1;
        Ok(true)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_empty_fold_keeps_one_metadata_pair_and_one_signature_segment() {
        let plan = SnapshotPlan::new(0, 8 * 20, Some(512)).expect("plan");
        assert_eq!(plan.metadata_sections().collect::<Vec<_>>(), [0]);
        assert_eq!(plan.signature_segments().collect::<Vec<_>>(), [(0, 0)]);
        assert_eq!(plan.section_count().expect("count"), 3);
        let unsigned = SnapshotPlan::new(0, 8 * 20, None).expect("plan");
        assert_eq!(unsigned.signature_segments().count(), 0);
        assert_eq!(unsigned.section_count().expect("count"), 2);
    }

    #[test]
    fn sections_split_where_pari_store_splits_them() {
        let hash_width = 8 * 16;
        let per_section = ((METADATA_SECTION_TARGET_BYTES - 8) / hash_width) as u64;
        let per_segment =
            ((SIGNATURE_SEGMENT_TARGET_BYTES - SIGNATURE_SEGMENT_HEADER_BYTES) / 256) as u64;
        let live = per_section * 2 + 5;
        let plan = SnapshotPlan::new(live, hash_width, Some(256)).expect("plan");
        assert_eq!(
            plan.metadata_sections().collect::<Vec<_>>(),
            [per_section, per_section, 5]
        );
        let segments: Vec<_> = plan.signature_segments().collect();
        assert_eq!(segments.first(), Some(&(0, per_segment)));
        assert_eq!(segments.iter().map(|(_, rows)| rows).sum::<u64>(), live);
        assert!(segments
            .windows(2)
            .all(|pair| pair[0].0 + pair[0].1 == pair[1].0));
    }
}
