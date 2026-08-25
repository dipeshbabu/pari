use std::io::{self, Read, Write};

use crc32fast::{hash as crc32, Hasher};

use crate::{BucketError, BucketKey, BUCKET_SEGMENT_HEADER_BYTES};

const BUCKET_SEGMENT_MAGIC: [u8; 8] = *b"PARIBKT\0";
const BUCKET_SEGMENT_VERSION: u16 = 1;
const BUCKET_SEGMENT_HEADER_BYTES_U16: u16 = 40;
const BUCKET_DIRECTORY_ENTRY_BYTES: usize = 32;
const U64_BYTES_U64: u64 = 8;
const COPY_BUFFER_BYTES: usize = 64 * 1024;

/// Metadata for one bucket whose encoded members are supplied by a sequential
/// reader to [`write_bucket_segment`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BucketWriteRecord {
    key: BucketKey,
    member_count: u32,
    member_checksum: u32,
}

impl BucketWriteRecord {
    /// Construct streaming metadata for one bucket.
    #[must_use]
    pub const fn new(key: BucketKey, member_count: u32, member_checksum: u32) -> Self {
        Self {
            key,
            member_count,
            member_checksum,
        }
    }

    /// Return the bucket identity.
    #[must_use]
    pub const fn key(self) -> BucketKey {
        self.key
    }

    /// Return the number of encoded `u64` members.
    #[must_use]
    pub const fn member_count(self) -> u32 {
        self.member_count
    }

    /// Return the expected CRC32 for the member bytes.
    #[must_use]
    pub const fn member_checksum(self) -> u32 {
        self.member_checksum
    }
}

/// Size and outer CRC32 of a streamed bucket segment.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StreamedBucketSegment {
    /// Total payload bytes written.
    pub bytes: u64,
    /// CRC32 of the complete payload, suitable for the outer section frame.
    pub checksum: u32,
}

/// Stream one canonical bucket segment without materializing member payloads.
///
/// `members` must contain exactly the concatenated little-endian `u64` member
/// lists described by `records`. Each list is checked against its per-bucket
/// CRC32 while being copied. Records must be strictly sorted and unique.
pub fn write_bucket_segment<W: Write, R: Read>(
    writer: &mut W,
    records: &[BucketWriteRecord],
    members: &mut R,
) -> Result<StreamedBucketSegment, BucketError> {
    validate_records(records)?;
    let directory_bytes = records
        .len()
        .checked_mul(BUCKET_DIRECTORY_ENTRY_BYTES)
        .ok_or(BucketError::LengthOverflow)?;
    let member_start = BUCKET_SEGMENT_HEADER_BYTES
        .checked_add(directory_bytes)
        .ok_or(BucketError::LengthOverflow)?;
    let mut member_offset =
        u64::try_from(member_start).map_err(|_| BucketError::LengthOverflow)?;
    let mut directory = Vec::with_capacity(directory_bytes);
    let mut total_member_bytes = 0_u64;

    for record in records {
        let member_bytes = u64::from(record.member_count)
            .checked_mul(U64_BYTES_U64)
            .ok_or(BucketError::LengthOverflow)?;
        directory.extend_from_slice(&record.key.band().to_le_bytes());
        directory.extend_from_slice(&0_u32.to_le_bytes());
        directory.extend_from_slice(&record.key.hash().to_le_bytes());
        directory.extend_from_slice(&member_offset.to_le_bytes());
        directory.extend_from_slice(&record.member_count.to_le_bytes());
        directory.extend_from_slice(&record.member_checksum.to_le_bytes());
        member_offset = member_offset
            .checked_add(member_bytes)
            .ok_or(BucketError::LengthOverflow)?;
        total_member_bytes = total_member_bytes
            .checked_add(member_bytes)
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

    let mut outer = Hasher::new();
    write_hashed(writer, &mut outer, &header)?;
    write_hashed(writer, &mut outer, &directory)?;

    let mut buffer = vec![0_u8; COPY_BUFFER_BYTES];
    for record in records {
        let mut remaining = u64::from(record.member_count)
            .checked_mul(U64_BYTES_U64)
            .ok_or(BucketError::LengthOverflow)?;
        let mut bucket = Hasher::new();
        while remaining > 0 {
            let count = usize::try_from(remaining.min(COPY_BUFFER_BYTES as u64))
                .map_err(|_| BucketError::LengthOverflow)?;
            members.read_exact(&mut buffer[..count]).map_err(io_error)?;
            writer.write_all(&buffer[..count]).map_err(io_error)?;
            bucket.update(&buffer[..count]);
            outer.update(&buffer[..count]);
            remaining -= u64::try_from(count).map_err(|_| BucketError::LengthOverflow)?;
        }
        let actual = bucket.finalize();
        if actual != record.member_checksum {
            return Err(BucketError::MemberChecksumMismatch {
                expected: record.member_checksum,
                actual,
            });
        }
    }

    let bytes = u64::try_from(member_start)
        .map_err(|_| BucketError::LengthOverflow)?
        .checked_add(total_member_bytes)
        .ok_or(BucketError::LengthOverflow)?;
    Ok(StreamedBucketSegment {
        bytes,
        checksum: outer.finalize(),
    })
}

fn validate_records(records: &[BucketWriteRecord]) -> Result<(), BucketError> {
    for pair in records.windows(2) {
        if pair[0].key >= pair[1].key {
            return Err(BucketError::Invalid {
                reason: "bucket records must be strictly sorted and unique",
            });
        }
    }
    Ok(())
}

fn write_hashed(
    writer: &mut impl Write,
    hasher: &mut Hasher,
    bytes: &[u8],
) -> Result<(), BucketError> {
    writer.write_all(bytes).map_err(io_error)?;
    hasher.update(bytes);
    Ok(())
}

fn io_error(error: io::Error) -> BucketError {
    BucketError::Invalid {
        reason: match error.kind() {
            io::ErrorKind::UnexpectedEof => "bucket member stream ended early",
            _ => "bucket segment I/O failed",
        },
    }
}

#[cfg(test)]
mod tests {
    use std::io::Cursor;

    use crc32fast::hash as crc32;

    use super::{write_bucket_segment, BucketWriteRecord};
    use crate::{encode_bucket_segment, BucketKey, BucketRecord};

    #[test]
    fn streaming_writer_matches_in_memory_encoder() {
        let first = [1_u64, 2, 3];
        let second = [7_u64, 9];
        let reference = encode_bucket_segment(&[
            BucketRecord::new(BucketKey::new(0, 11), &first),
            BucketRecord::new(BucketKey::new(1, 22), &second),
        ])
        .expect("reference");

        let mut member_bytes = Vec::new();
        for member in first.into_iter().chain(second) {
            member_bytes.extend_from_slice(&member.to_le_bytes());
        }
        let first_bytes = first.len() * 8;
        let records = [
            BucketWriteRecord::new(
                BucketKey::new(0, 11),
                u32::try_from(first.len()).expect("count"),
                crc32(&member_bytes[..first_bytes]),
            ),
            BucketWriteRecord::new(
                BucketKey::new(1, 22),
                u32::try_from(second.len()).expect("count"),
                crc32(&member_bytes[first_bytes..]),
            ),
        ];
        let mut output = Vec::new();
        let result = write_bucket_segment(
            &mut output,
            &records,
            &mut Cursor::new(member_bytes),
        )
        .expect("stream");
        assert_eq!(output, reference);
        assert_eq!(result.bytes, u64::try_from(reference.len()).expect("length"));
        assert_eq!(result.checksum, crc32(&reference));
    }
}
