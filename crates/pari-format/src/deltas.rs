//! Commit generations appended after a version-1 container.
//!
//! A *delta record* holds the changes of one commit as ordinary framed
//! sections, preceded by a 48-byte header and followed by a 32-byte commit
//! trailer. A record counts as committed only once its trailer is present and
//! valid. Writers make the header and sections durable before writing the
//! trailer, so a damaged record that has a valid trailer is corruption, while a
//! record without one is an interrupted commit that readers ignore.

use std::io::{Read, Seek, SeekFrom};

use crc32fast::{hash as crc32, Hasher};

use crate::{
    format::{MAX_SECTION_BYTES, MAX_SECTION_COUNT},
    layout::{read_exact_format, section_kind_from_raw, verify_skipped_payload},
    FormatError, LayoutError, SectionDescriptor,
};

pub(crate) const DELTA_MAGIC: [u8; 8] = *b"PARIDLT\0";
const COMMIT_MAGIC: [u8; 8] = *b"PARICMT\0";
const DELTA_VERSION: u16 = 1;
const DELTA_HEADER_SIZE_U16: u16 = 48;
const SECTION_FRAME_BYTES: usize = 16;
const SECTION_FRAME_BYTES_U64: u64 = 16;
const SECTION_FLAG_REQUIRED: u16 = 1;

/// Size of the header that starts every appended delta record.
pub const DELTA_HEADER_BYTES: usize = 48;
/// Size of the trailer that commits an appended delta record.
pub const COMMIT_TRAILER_BYTES: usize = 32;
const DELTA_HEADER_BYTES_U64: u64 = 48;
const COMMIT_TRAILER_BYTES_U64: u64 = 32;

/// Location and section descriptors of one committed delta record.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeltaLayout {
    generation: u64,
    offset: u64,
    end: u64,
    digest: u32,
    sections: Vec<SectionDescriptor>,
}

impl DeltaLayout {
    /// Return the generation number; the first delta after the base is 1.
    #[must_use]
    pub const fn generation(&self) -> u64 {
        self.generation
    }

    /// Return the absolute offset of the record header.
    #[must_use]
    pub const fn offset(&self) -> u64 {
        self.offset
    }

    /// Return the absolute offset just past the commit trailer.
    #[must_use]
    pub const fn end(&self) -> u64 {
        self.end
    }

    /// Return descriptors for the understood sections, in record order.
    #[must_use]
    pub fn sections(&self) -> &[SectionDescriptor] {
        &self.sections
    }

    pub(crate) const fn digest(&self) -> u32 {
        self.digest
    }
}

/// Where the next delta record of a file must be written.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct NextDelta {
    pub(crate) generation: u64,
    pub(crate) offset: u64,
    pub(crate) previous_digest: u32,
}

impl NextDelta {
    /// Return the generation number the next record must carry.
    #[must_use]
    pub const fn generation(self) -> u64 {
        self.generation
    }

    /// Return the absolute offset at which the next record starts.
    ///
    /// Bytes at or beyond this offset belong to an interrupted commit and are
    /// replaced by the next record.
    #[must_use]
    pub const fn offset(self) -> u64 {
        self.offset
    }
}

/// A written but not yet committed delta record.
///
/// Make the record durable, then append [`Self::trailer`] immediately after it
/// and make that durable to commit the generation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DeltaCommit {
    pub(crate) generation: u64,
    pub(crate) offset: u64,
    pub(crate) end: u64,
    pub(crate) digest: u32,
}

impl DeltaCommit {
    /// Return the generation number of the written record.
    #[must_use]
    pub const fn generation(self) -> u64 {
        self.generation
    }

    /// Return the absolute offset at which the trailer must be written.
    #[must_use]
    pub const fn trailer_offset(self) -> u64 {
        self.end
    }

    /// Return the trailer bytes that commit this record.
    #[must_use]
    pub fn trailer(self) -> [u8; COMMIT_TRAILER_BYTES] {
        let mut trailer = [0_u8; COMMIT_TRAILER_BYTES];
        trailer[0..8].copy_from_slice(&COMMIT_MAGIC);
        trailer[8..16].copy_from_slice(&self.generation.to_le_bytes());
        trailer[16..24].copy_from_slice(&self.offset.to_le_bytes());
        trailer[24..28].copy_from_slice(&self.digest.to_le_bytes());
        let checksum = crc32(&trailer[..28]);
        trailer[28..32].copy_from_slice(&checksum.to_le_bytes());
        trailer
    }
}

pub(crate) fn encode_delta_header(
    section_count: u32,
    generation: u64,
    body_length: u64,
    previous_digest: u32,
) -> [u8; DELTA_HEADER_BYTES] {
    let mut header = [0_u8; DELTA_HEADER_BYTES];
    header[0..8].copy_from_slice(&DELTA_MAGIC);
    header[8..10].copy_from_slice(&DELTA_VERSION.to_le_bytes());
    header[10..12].copy_from_slice(&DELTA_HEADER_SIZE_U16.to_le_bytes());
    header[12..16].copy_from_slice(&section_count.to_le_bytes());
    header[16..24].copy_from_slice(&generation.to_le_bytes());
    header[24..32].copy_from_slice(&body_length.to_le_bytes());
    header[32..36].copy_from_slice(&previous_digest.to_le_bytes());
    let checksum = crc32(&header[..44]);
    header[44..48].copy_from_slice(&checksum.to_le_bytes());
    header
}

/// Return the digest that commits a record: CRC32 of its header, without the
/// header checksum, followed by every section frame.
///
/// The header checksum is left out because a CRC32 run over bytes that end in
/// their own CRC32 yields the same value for every input.
pub(crate) fn record_digest(header: &[u8; DELTA_HEADER_BYTES], frames: &[u8]) -> u32 {
    let mut hasher = Hasher::new();
    hasher.update(&header[..44]);
    hasher.update(frames);
    hasher.finalize()
}

struct Trailer {
    generation: u64,
    offset: u64,
    digest: u32,
}

fn decode_trailer(bytes: &[u8; COMMIT_TRAILER_BYTES]) -> Option<Trailer> {
    if bytes[0..8] != COMMIT_MAGIC {
        return None;
    }
    if crc32(&bytes[..28]) != u32::from_le_bytes(bytes[28..32].try_into().ok()?) {
        return None;
    }
    Some(Trailer {
        generation: u64::from_le_bytes(bytes[8..16].try_into().ok()?),
        offset: u64::from_le_bytes(bytes[16..24].try_into().ok()?),
        digest: u32::from_le_bytes(bytes[24..28].try_into().ok()?),
    })
}

fn invalid(generation: u64, reason: &'static str) -> LayoutError {
    FormatError::InvalidGeneration { generation, reason }.into()
}

fn read_at<R: Read + Seek>(
    reader: &mut R,
    offset: u64,
    output: &mut [u8],
    context: &'static str,
) -> Result<(), LayoutError> {
    reader.seek(SeekFrom::Start(offset))?;
    read_exact_format(reader, output, context)
}

/// Read the committed delta records from `start` up to `file_length`.
///
/// Returns the records and the offset where committed data ends. Bytes after
/// that offset are an interrupted commit, unless a valid trailer for a later
/// generation ends the file: then a committed record is damaged and the scan
/// fails.
pub(crate) fn scan_deltas<R: Read + Seek>(
    reader: &mut R,
    start: u64,
    file_length: u64,
    mut next: NextDelta,
) -> Result<(Vec<DeltaLayout>, u64), LayoutError> {
    let mut records = Vec::new();
    let mut cursor = start;
    while cursor < file_length {
        let Some(record) = read_record(reader, cursor, file_length, next)? else {
            check_uncommitted_tail(reader, cursor, file_length, next.generation)?;
            break;
        };
        cursor = record.end;
        next = NextDelta {
            generation: next
                .generation
                .checked_add(1)
                .ok_or(FormatError::LengthOverflow)?,
            offset: record.end,
            previous_digest: record.digest,
        };
        records.push(record);
    }
    Ok((records, cursor))
}

/// Fail if the file ends with a valid trailer for an unread generation.
fn check_uncommitted_tail<R: Read + Seek>(
    reader: &mut R,
    cursor: u64,
    file_length: u64,
    generation: u64,
) -> Result<(), LayoutError> {
    if file_length - cursor < COMMIT_TRAILER_BYTES_U64 {
        return Ok(());
    }
    let mut bytes = [0_u8; COMMIT_TRAILER_BYTES];
    read_at(
        reader,
        file_length - COMMIT_TRAILER_BYTES_U64,
        &mut bytes,
        "commit trailer",
    )?;
    match decode_trailer(&bytes) {
        Some(trailer) if trailer.generation >= generation => {
            Err(invalid(generation, "a committed delta record is damaged"))
        }
        _ => Ok(()),
    }
}

/// Read one record, or `None` if no committed record starts at `offset`.
fn read_record<R: Read + Seek>(
    reader: &mut R,
    offset: u64,
    file_length: u64,
    next: NextDelta,
) -> Result<Option<DeltaLayout>, LayoutError> {
    let generation = next.generation;
    if file_length - offset < DELTA_HEADER_BYTES_U64 {
        return Ok(None);
    }
    let mut header = [0_u8; DELTA_HEADER_BYTES];
    read_at(reader, offset, &mut header, "delta header")?;
    if header[0..8] != DELTA_MAGIC
        || crc32(&header[..44])
            != u32::from_le_bytes([header[44], header[45], header[46], header[47]])
    {
        return Ok(None);
    }
    let field_u16 = |at: usize| u16::from_le_bytes([header[at], header[at + 1]]);
    let field_u32 = |at: usize| {
        u32::from_le_bytes([header[at], header[at + 1], header[at + 2], header[at + 3]])
    };
    let field_u64 = |at: usize| {
        let mut raw = [0_u8; 8];
        raw.copy_from_slice(&header[at..at + 8]);
        u64::from_le_bytes(raw)
    };
    if field_u16(8) != DELTA_VERSION {
        return Err(invalid(generation, "unsupported delta record version"));
    }
    if field_u16(10) != DELTA_HEADER_SIZE_U16 || field_u32(36) != 0 || field_u32(40) != 0 {
        return Err(invalid(generation, "invalid delta header fields"));
    }
    if field_u64(16) != generation {
        return Err(invalid(generation, "delta records are out of sequence"));
    }
    if field_u32(32) != next.previous_digest {
        return Err(invalid(
            generation,
            "delta record does not follow the preceding generation",
        ));
    }
    let section_count = usize::try_from(field_u32(12)).map_err(|_| FormatError::LengthOverflow)?;
    if section_count > MAX_SECTION_COUNT {
        return Err(invalid(
            generation,
            "delta record declares too many sections",
        ));
    }
    let body_start = offset + DELTA_HEADER_BYTES_U64;
    let body_end = body_start
        .checked_add(field_u64(24))
        .ok_or(FormatError::LengthOverflow)?;
    let Some(end) = body_end
        .checked_add(COMMIT_TRAILER_BYTES_U64)
        .filter(|end| *end <= file_length)
    else {
        return Ok(None);
    };
    let mut trailer = [0_u8; COMMIT_TRAILER_BYTES];
    read_at(reader, body_end, &mut trailer, "commit trailer")?;
    let Some(trailer) = decode_trailer(&trailer) else {
        return Ok(None);
    };
    if trailer.generation != generation || trailer.offset != offset {
        return Err(invalid(
            generation,
            "commit trailer does not match its delta record",
        ));
    }

    // The trailer is valid, so the record was made durable before it: any
    // inconsistency from here on is corruption.
    let (sections, frames) = read_frames(reader, generation, body_start, body_end, section_count)?;
    let digest = record_digest(&header, &frames);
    if digest != trailer.digest {
        return Err(invalid(
            generation,
            "delta record does not match its commit trailer",
        ));
    }
    Ok(Some(DeltaLayout {
        generation,
        offset,
        end,
        digest,
        sections,
    }))
}

/// Read and validate the section frames of a committed record.
fn read_frames<R: Read + Seek>(
    reader: &mut R,
    generation: u64,
    body_start: u64,
    body_end: u64,
    section_count: usize,
) -> Result<(Vec<SectionDescriptor>, Vec<u8>), LayoutError> {
    let mut frames = Vec::with_capacity(section_count * SECTION_FRAME_BYTES);
    let mut sections = Vec::with_capacity(section_count);
    let mut cursor = body_start;
    for _ in 0..section_count {
        let payload_offset = cursor
            .checked_add(SECTION_FRAME_BYTES_U64)
            .filter(|end| *end <= body_end)
            .ok_or_else(|| invalid(generation, "section frame exceeds the delta record"))?;
        let mut frame = [0_u8; SECTION_FRAME_BYTES];
        read_at(reader, cursor, &mut frame, "delta section frame")?;
        let kind_raw = u16::from_le_bytes([frame[0], frame[1]]);
        let flags = u16::from_le_bytes([frame[2], frame[3]]);
        let mut length = [0_u8; 8];
        length.copy_from_slice(&frame[4..12]);
        let payload_length = u64::from_le_bytes(length);
        let checksum = u32::from_le_bytes([frame[12], frame[13], frame[14], frame[15]]);
        if flags & !SECTION_FLAG_REQUIRED != 0 {
            return Err(FormatError::InvalidSectionFlags { flags }.into());
        }
        if payload_length > MAX_SECTION_BYTES {
            return Err(FormatError::SectionTooLarge {
                length: payload_length,
            }
            .into());
        }
        let payload_end = payload_offset
            .checked_add(payload_length)
            .filter(|end| *end <= body_end)
            .ok_or_else(|| invalid(generation, "section payload exceeds the delta record"))?;
        let required = flags & SECTION_FLAG_REQUIRED != 0;
        if !required {
            reader.seek(SeekFrom::Start(payload_offset))?;
            verify_skipped_payload(reader, payload_length, checksum)?;
        }
        match section_kind_from_raw(kind_raw) {
            Some(kind) => sections.push(SectionDescriptor::new(
                kind,
                required,
                payload_offset,
                payload_length,
                checksum,
            )),
            None if required => {
                return Err(FormatError::UnknownRequiredSection { kind: kind_raw }.into());
            }
            None => {}
        }
        frames.extend_from_slice(&frame);
        cursor = payload_end;
    }
    if cursor != body_end {
        return Err(invalid(
            generation,
            "delta sections do not fill the declared record length",
        ));
    }
    Ok((sections, frames))
}
