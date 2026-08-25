use std::{
    error::Error,
    fmt,
    io::{Read, Seek},
};

use crc32fast::hash as crc32;

use crate::{FileLayout, LayoutError, SectionDescriptor, SectionKind};

const BUCKET_SEGMENT_MAGIC: [u8; 8] = *b"PARIBKT\0";
const BUCKET_SEGMENT_VERSION: u16 = 1;
const BUCKET_SEGMENT_HEADER_BYTES: usize = 40;
const BUCKET_SEGMENT_HEADER_BYTES_U16: u16 = 40;
const BUCKET_SEGMENT_HEADER_BYTES_U64: u64 = 40;
const BUCKET_DIRECTORY_ENTRY_BYTES: usize = 32;
const U64_BYTES: usize = 8;
const U64_BYTES_U64: u64 = 8;

/// Target payload size used when splitting large bucket collections into
/// independently checksummed sections.
pub const BUCKET_SEGMENT_TARGET_BYTES: usize = 32 * 1024 * 1024;

/// Stable `(band, hash)` identity of one LSH bucket.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct BucketKey {
    band: u32,
    hash: u64,
}

impl BucketKey {
    /// Construct a bucket identity.
    #[must_use]
    pub const fn new(band: u32, hash: u64) -> Self {
        Self { band, hash }
    }

    /// Return the zero-based LSH band number.
    #[must_use]
    pub const fn band(self) -> u32 {
        self.band
    }

    /// Return the stable hash of the values in this band.
    #[must_use]
    pub const fn hash(self) -> u64 {
        self.hash
    }
}

/// Borrowed bucket record used by the encoder.
#[derive(Debug, Clone, Copy)]
pub struct BucketRecord<'a> {
    key: BucketKey,
    members: &'a [u64],
}

impl<'a> BucketRecord<'a> {
    /// Construct one sorted bucket record.
    #[must_use]
    pub const fn new(key: BucketKey, members: &'a [u64]) -> Self {
        Self { key, members }
    }

    /// Return the bucket identity.
    #[must_use]
    pub const fn key(self) -> BucketKey {
        self.key
    }

    /// Borrow the member keys.
    #[must_use]
    pub const fn members(self) -> &'a [u64] {
        self.members
    }
}

/// Exact location of one bucket's member list inside a `Buckets` section.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BucketLocation {
    key: BucketKey,
    section: SectionDescriptor,
    member_offset: u64,
    member_count: u32,
    member_checksum: u32,
}

impl BucketLocation {
    /// Return the bucket identity.
    #[must_use]
    pub const fn key(self) -> BucketKey {
        self.key
    }

    /// Return the containing `Buckets` section.
    #[must_use]
    pub const fn section(self) -> SectionDescriptor {
        self.section
    }

    /// Return the member-list offset relative to the section payload.
    #[must_use]
    pub const fn member_offset(self) -> u64 {
        self.member_offset
    }

    /// Return the number of member keys.
    #[must_use]
    pub const fn member_count(self) -> u32 {
        self.member_count
    }

    /// Return the CRC32 of the encoded member list.
    #[must_use]
    pub const fn member_checksum(self) -> u32 {
        self.member_checksum
    }
}

/// Errors produced by the stable bucket-segment codec.
#[derive(Debug)]
pub enum BucketError {
    /// File I/O or outer-container range validation failed.
    Layout(LayoutError),
    /// Bucket bytes violate a structural invariant.
    Invalid { reason: &'static str },
    /// A checksummed bucket directory is corrupt.
    DirectoryChecksumMismatch { expected: u32, actual: u32 },
    /// A checksummed bucket member list is corrupt.
    MemberChecksumMismatch { expected: u32, actual: u32 },
    /// Checked length arithmetic or a platform conversion overflowed.
    LengthOverflow,
}

impl fmt::Display for BucketError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Layout(error) => error.fmt(formatter),
            Self::Invalid { reason } => write!(formatter, "invalid bucket segment: {reason}"),
            Self::DirectoryChecksumMismatch { expected, actual } => write!(
                formatter,
                "bucket directory checksum mismatch: stored {expected:#010x}, computed {actual:#010x}"
            ),
            Self::MemberChecksumMismatch { expected, actual } => write!(
                formatter,
                "bucket member checksum mismatch: stored {expected:#010x}, computed {actual:#010x}"
            ),
            Self::LengthOverflow => formatter.write_str("bucket segment length arithmetic overflowed"),
        }
    }
}

impl Error for BucketError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Layout(error) => Some(error),
            _ => None,
        }
    }
}

impl From<LayoutError> for BucketError {
    fn from(error: LayoutError) -> Self {
        Self::Layout(error)
    }
}

/// Return the encoded contribution of one bucket to a segment payload.
pub fn bucket_record_size(member_count: usize) -> Result<usize, BucketError> {
    BUCKET_DIRECTORY_ENTRY_BYTES
        .checked_add(
            member_count
                .checked_mul(U64_BYTES)
                .ok_or(BucketError::LengthOverflow)?,
        )
        .ok_or(BucketError::LengthOverflow)
}

/// Encode one sorted bucket segment using Pari's stable checksummed layout.
///
/// Records must be strictly ordered by `(band, hash)`. Empty segments are
/// valid and are used for empty indexes.
pub fn encode_bucket_segment(records: &[BucketRecord<'_>]) -> Result<Vec<u8>, BucketError> {
    validate_record_order(records)?;
    let directory_bytes = records
        .len()
        .checked_mul(BUCKET_DIRECTORY_ENTRY_BYTES)
        .ok_or(BucketError::LengthOverflow)?;
    let member_bytes = records.iter().try_fold(0_usize, |total, record| {
        total
            .checked_add(
                record
                    .members
                    .len()
                    .checked_mul(U64_BYTES)
                    .ok_or(BucketError::LengthOverflow)?,
            )
            .ok_or(BucketError::LengthOverflow)
    })?;
    let total = BUCKET_SEGMENT_HEADER_BYTES
        .checked_add(directory_bytes)
        .and_then(|value| value.checked_add(member_bytes))
        .ok_or(BucketError::LengthOverflow)?;

    let member_start = BUCKET_SEGMENT_HEADER_BYTES
        .checked_add(directory_bytes)
        .ok_or(BucketError::LengthOverflow)?;
    let mut directory = Vec::with_capacity(directory_bytes);
    let mut members_payload = Vec::with_capacity(member_bytes);
    let mut member_offset =
        u64::try_from(member_start).map_err(|_| BucketError::LengthOverflow)?;

    for record in records {
        let start = members_payload.len();
        for member in record.members {
            members_payload.extend_from_slice(&member.to_le_bytes());
        }
        let member_count =
            u32::try_from(record.members.len()).map_err(|_| BucketError::LengthOverflow)?;
        directory.extend_from_slice(&record.key.band.to_le_bytes());
        directory.extend_from_slice(&0_u32.to_le_bytes());
        directory.extend_from_slice(&record.key.hash.to_le_bytes());
        directory.extend_from_slice(&member_offset.to_le_bytes());
        directory.extend_from_slice(&member_count.to_le_bytes());
        directory.extend_from_slice(&crc32(&members_payload[start..]).to_le_bytes());
        member_offset = member_offset
            .checked_add(
                u64::from(member_count)
                    .checked_mul(U64_BYTES_U64)
                    .ok_or(BucketError::LengthOverflow)?,
            )
            .ok_or(BucketError::LengthOverflow)?;
    }

    let mut header = [0_u8; BUCKET_SEGMENT_HEADER_BYTES];
    header[..8].copy_from_slice(&BUCKET_SEGMENT_MAGIC);
    header[8..10].copy_from_slice(&BUCKET_SEGMENT_VERSION.to_le_bytes());
    header[10..12].copy_from_slice(&BUCKET_SEGMENT_HEADER_BYTES_U16.to_le_bytes());
    header[12..20].copy_from_slice(
        &u64::try_from(records.len())
            .map_err(|_| BucketError::LengthOverflow)?
            .to_le_bytes(),
    );
    header[20..28].copy_from_slice(
        &u64::try_from(directory_bytes)
            .map_err(|_| BucketError::LengthOverflow)?
            .to_le_bytes(),
    );
    header[28..32].copy_from_slice(&crc32(&directory).to_le_bytes());
    header[32..36].copy_from_slice(&crc32(&header[..32]).to_le_bytes());

    let mut output = Vec::with_capacity(total);
    output.extend_from_slice(&header);
    output.extend_from_slice(&directory);
    output.extend_from_slice(&members_payload);
    Ok(output)
}

/// Decode and validate one `Buckets` section without reading member payloads.
pub fn decode_bucket_segment<R: Read + Seek>(
    layout: &FileLayout,
    reader: &mut R,
    descriptor: SectionDescriptor,
    bands: usize,
) -> Result<Vec<BucketLocation>, BucketError> {
    if descriptor.kind() != SectionKind::Buckets || !descriptor.required() {
        return Err(BucketError::Invalid {
            reason: "bucket segment must be a required Buckets section",
        });
    }
    if descriptor.payload_length() < BUCKET_SEGMENT_HEADER_BYTES_U64 {
        return Err(BucketError::Invalid {
            reason: "bucket segment header is truncated",
        });
    }

    let header = layout.read_section_range(
        reader,
        descriptor,
        0,
        BUCKET_SEGMENT_HEADER_BYTES,
    )?;
    validate_header_fixed_fields(&header)?;
    let expected_header_checksum = read_u32_at(&header, 32)?;
    let actual_header_checksum = crc32(&header[..32]);
    if expected_header_checksum != actual_header_checksum {
        return Err(BucketError::DirectoryChecksumMismatch {
            expected: expected_header_checksum,
            actual: actual_header_checksum,
        });
    }

    let count = usize::try_from(read_u64_at(&header, 12)?)
        .map_err(|_| BucketError::LengthOverflow)?;
    let directory_bytes = usize::try_from(read_u64_at(&header, 20)?)
        .map_err(|_| BucketError::LengthOverflow)?;
    let expected_directory_bytes = count
        .checked_mul(BUCKET_DIRECTORY_ENTRY_BYTES)
        .ok_or(BucketError::LengthOverflow)?;
    if directory_bytes != expected_directory_bytes {
        return Err(BucketError::Invalid {
            reason: "bucket directory byte length does not match its record count",
        });
    }
    let member_start = BUCKET_SEGMENT_HEADER_BYTES
        .checked_add(directory_bytes)
        .ok_or(BucketError::LengthOverflow)?;
    let member_start_u64 =
        u64::try_from(member_start).map_err(|_| BucketError::LengthOverflow)?;
    if member_start_u64 > descriptor.payload_length() {
        return Err(BucketError::Invalid {
            reason: "bucket directory extends past the section payload",
        });
    }

    let directory = layout.read_section_range(
        reader,
        descriptor,
        BUCKET_SEGMENT_HEADER_BYTES_U64,
        directory_bytes,
    )?;
    let expected_directory_checksum = read_u32_at(&header, 28)?;
    let actual_directory_checksum = crc32(&directory);
    if expected_directory_checksum != actual_directory_checksum {
        return Err(BucketError::DirectoryChecksumMismatch {
            expected: expected_directory_checksum,
            actual: actual_directory_checksum,
        });
    }

    decode_directory_entries(descriptor, &directory, count, member_start_u64, bands)
}

/// Verify a bucket member range and return its decoded keys.
pub fn read_bucket_members<R: Read + Seek>(
    layout: &FileLayout,
    reader: &mut R,
    location: BucketLocation,
) -> Result<Vec<u64>, BucketError> {
    let byte_length = usize::try_from(location.member_count)
        .map_err(|_| BucketError::LengthOverflow)?
        .checked_mul(U64_BYTES)
        .ok_or(BucketError::LengthOverflow)?;
    let bytes = layout.read_section_range(
        reader,
        location.section,
        location.member_offset,
        byte_length,
    )?;
    let actual = crc32(&bytes);
    if actual != location.member_checksum {
        return Err(BucketError::MemberChecksumMismatch {
            expected: location.member_checksum,
            actual,
        });
    }
    bytes
        .chunks_exact(U64_BYTES)
        .map(read_u64)
        .collect()
}

/// Ensure locations from multiple bucket sections remain globally sorted and
/// contain no duplicate `(band, hash)` keys.
pub fn validate_global_bucket_order(locations: &[BucketLocation]) -> Result<(), BucketError> {
    for pair in locations.windows(2) {
        if pair[0].key >= pair[1].key {
            return Err(BucketError::Invalid {
                reason: "bucket directory contains duplicate or unsorted bucket keys",
            });
        }
    }
    Ok(())
}

fn validate_record_order(records: &[BucketRecord<'_>]) -> Result<(), BucketError> {
    for pair in records.windows(2) {
        if pair[0].key >= pair[1].key {
            return Err(BucketError::Invalid {
                reason: "bucket records must be strictly sorted and unique",
            });
        }
    }
    Ok(())
}

fn validate_header_fixed_fields(header: &[u8]) -> Result<(), BucketError> {
    if header.get(..8) != Some(BUCKET_SEGMENT_MAGIC.as_slice()) {
        return Err(BucketError::Invalid {
            reason: "invalid bucket segment magic",
        });
    }
    if read_u16_at(header, 8)? != BUCKET_SEGMENT_VERSION {
        return Err(BucketError::Invalid {
            reason: "unsupported bucket segment version",
        });
    }
    if read_u16_at(header, 10)? != BUCKET_SEGMENT_HEADER_BYTES_U16 {
        return Err(BucketError::Invalid {
            reason: "invalid bucket segment header size",
        });
    }
    if header.get(36..40) != Some(&[0, 0, 0, 0]) {
        return Err(BucketError::Invalid {
            reason: "bucket segment reserved bytes must be zero",
        });
    }
    Ok(())
}

fn decode_directory_entries(
    descriptor: SectionDescriptor,
    directory: &[u8],
    count: usize,
    member_start: u64,
    bands: usize,
) -> Result<Vec<BucketLocation>, BucketError> {
    let bands_u32 = u32::try_from(bands).map_err(|_| BucketError::LengthOverflow)?;
    let mut locations = Vec::with_capacity(count);
    let mut expected_member_offset = member_start;
    let mut previous_key = None;
    for entry in directory.chunks_exact(BUCKET_DIRECTORY_ENTRY_BYTES) {
        if read_u32_at(entry, 4)? != 0 {
            return Err(BucketError::Invalid {
                reason: "bucket directory reserved bytes must be zero",
            });
        }
        let key = BucketKey {
            band: read_u32_at(entry, 0)?,
            hash: read_u64_at(entry, 8)?,
        };
        if key.band >= bands_u32 {
            return Err(BucketError::Invalid {
                reason: "bucket directory band exceeds index metadata",
            });
        }
        if previous_key.is_some_and(|previous| previous >= key) {
            return Err(BucketError::Invalid {
                reason: "bucket segment keys are duplicated or unsorted",
            });
        }
        let member_offset = read_u64_at(entry, 16)?;
        if member_offset != expected_member_offset {
            return Err(BucketError::Invalid {
                reason: "bucket member ranges contain a gap, overlap, or invalid offset",
            });
        }
        let member_count = read_u32_at(entry, 24)?;
        let member_end = member_offset
            .checked_add(
                u64::from(member_count)
                    .checked_mul(U64_BYTES_U64)
                    .ok_or(BucketError::LengthOverflow)?,
            )
            .ok_or(BucketError::LengthOverflow)?;
        if member_end > descriptor.payload_length() {
            return Err(BucketError::Invalid {
                reason: "bucket member range extends past the section payload",
            });
        }
        let location = BucketLocation {
            key,
            section: descriptor,
            member_offset,
            member_count,
            member_checksum: read_u32_at(entry, 28)?,
        };
        expected_member_offset = member_end;
        previous_key = Some(key);
        locations.push(location);
    }
    if locations.len() != count {
        return Err(BucketError::Invalid {
            reason: "bucket directory record count is inconsistent",
        });
    }
    if expected_member_offset != descriptor.payload_length() {
        return Err(BucketError::Invalid {
            reason: "bucket segment has trailing member bytes",
        });
    }
    Ok(locations)
}

fn read_u16_at(bytes: &[u8], offset: usize) -> Result<u16, BucketError> {
    let end = offset.checked_add(2).ok_or(BucketError::LengthOverflow)?;
    let raw: [u8; 2] = bytes
        .get(offset..end)
        .ok_or(BucketError::Invalid {
            reason: "fixed-width u16 field is truncated",
        })?
        .try_into()
        .map_err(|_| BucketError::Invalid {
            reason: "fixed-width u16 field is truncated",
        })?;
    Ok(u16::from_le_bytes(raw))
}

fn read_u32_at(bytes: &[u8], offset: usize) -> Result<u32, BucketError> {
    let end = offset.checked_add(4).ok_or(BucketError::LengthOverflow)?;
    let raw: [u8; 4] = bytes
        .get(offset..end)
        .ok_or(BucketError::Invalid {
            reason: "fixed-width u32 field is truncated",
        })?
        .try_into()
        .map_err(|_| BucketError::Invalid {
            reason: "fixed-width u32 field is truncated",
        })?;
    Ok(u32::from_le_bytes(raw))
}

fn read_u64_at(bytes: &[u8], offset: usize) -> Result<u64, BucketError> {
    let end = offset
        .checked_add(U64_BYTES)
        .ok_or(BucketError::LengthOverflow)?;
    let raw = bytes.get(offset..end).ok_or(BucketError::Invalid {
        reason: "fixed-width u64 field is truncated",
    })?;
    read_u64(raw)
}

fn read_u64(bytes: &[u8]) -> Result<u64, BucketError> {
    let raw: [u8; U64_BYTES] = bytes.try_into().map_err(|_| BucketError::Invalid {
        reason: "fixed-width u64 field is truncated",
    })?;
    Ok(u64::from_le_bytes(raw))
}

#[cfg(test)]
mod tests {
    use std::io::Cursor;

    use super::{
        decode_bucket_segment, encode_bucket_segment, read_bucket_members,
        validate_global_bucket_order, BucketKey, BucketRecord,
    };
    use crate::{
        Algorithm, CodecId, FileLayout, IndexFile, IndexMetadata, Section, SectionKind,
        SignatureScheme,
    };

    fn metadata() -> IndexMetadata {
        IndexMetadata::new(
            Algorithm::MinHashLsh,
            SignatureScheme::PariAffine32V1,
            CodecId::U64,
            128,
            7,
            0.8,
            32,
            4,
            0,
        )
        .expect("valid metadata")
    }

    #[test]
    fn segment_round_trip_keeps_directory_lazy() {
        let first = [1_u64, 2, 3];
        let second = [7_u64, 9];
        let payload = encode_bucket_segment(&[
            BucketRecord::new(BucketKey::new(0, 11), &first),
            BucketRecord::new(BucketKey::new(1, 22), &second),
        ])
        .expect("encode");
        let bytes = IndexFile::new(
            metadata(),
            vec![Section::new(SectionKind::Buckets, true, payload).expect("section")],
        )
        .expect("file")
        .encode()
        .expect("container");
        let mut cursor = Cursor::new(bytes);
        let layout = FileLayout::read_from(&mut cursor).expect("layout");
        let section = layout.section(SectionKind::Buckets).expect("buckets");
        let locations = decode_bucket_segment(&layout, &mut cursor, section, 32).expect("decode");
        validate_global_bucket_order(&locations).expect("global order");
        assert_eq!(locations.len(), 2);
        assert_eq!(
            read_bucket_members(&layout, &mut cursor, locations[0]).expect("members"),
            first
        );
        assert_eq!(
            read_bucket_members(&layout, &mut cursor, locations[1]).expect("members"),
            second
        );
    }

    #[test]
    fn member_corruption_is_detected_on_read() {
        let members = [5_u64, 6];
        let payload = encode_bucket_segment(&[BucketRecord::new(
            BucketKey::new(0, 11),
            &members,
        )])
        .expect("encode");
        let mut bytes = IndexFile::new(
            metadata(),
            vec![Section::new(SectionKind::Buckets, true, payload).expect("section")],
        )
        .expect("file")
        .encode()
        .expect("container");
        let last = bytes.len() - 1;
        bytes[last] ^= 0xFF;
        let mut cursor = Cursor::new(bytes);
        let layout = FileLayout::read_from(&mut cursor).expect("layout");
        let section = layout.section(SectionKind::Buckets).expect("buckets");
        let locations = decode_bucket_segment(&layout, &mut cursor, section, 32).expect("decode");
        assert!(read_bucket_members(&layout, &mut cursor, locations[0]).is_err());
    }
}
