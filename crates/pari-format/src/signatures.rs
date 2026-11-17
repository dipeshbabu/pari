//! Optional per-key `MinHash` signature segments.
//!
//! A `Signatures` section stores the full signature for a contiguous range of
//! rows, where row `n` belongs to the `n`-th key of the snapshot's `Keys`
//! section. Signatures let readers return estimated Jaccard similarity for LSH
//! candidates without an external signature store. The sections are optional:
//! readers that predate them skip them after verifying their checksum.

use std::{
    error::Error,
    fmt,
    io::{Read, Seek},
};

use crate::{FileLayout, LayoutError, SectionDescriptor, SectionKind, SignatureScheme};

const SIGNATURE_SEGMENT_MAGIC: [u8; 8] = *b"PARISIG\0";
const SIGNATURE_SEGMENT_VERSION: u16 = 1;
/// Fixed byte width of a version-1 signature-segment header.
pub const SIGNATURE_SEGMENT_HEADER_BYTES: usize = 40;
const SIGNATURE_SEGMENT_HEADER_BYTES_U16: u16 = 40;
const SIGNATURE_SEGMENT_HEADER_BYTES_U64: u64 = 40;

/// Target payload size used when splitting large signature tables into
/// independently checksummed sections.
pub const SIGNATURE_SEGMENT_TARGET_BYTES: usize = 32 * 1024 * 1024;

/// Errors produced by the signature-segment codec.
#[derive(Debug)]
pub enum SignatureError {
    /// File I/O or outer-container range validation failed.
    Layout(LayoutError),
    /// Signature bytes violate a structural invariant.
    Invalid { reason: &'static str },
    /// A signature value does not fit the scheme's value width.
    ValueOutOfRange { value: u64, width_bits: u16 },
    /// Checked length arithmetic or a platform conversion overflowed.
    LengthOverflow,
}

impl fmt::Display for SignatureError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Layout(error) => error.fmt(formatter),
            Self::Invalid { reason } => write!(formatter, "invalid signature segment: {reason}"),
            Self::ValueOutOfRange { value, width_bits } => write!(
                formatter,
                "signature value {value} does not fit in {width_bits} bits"
            ),
            Self::LengthOverflow => formatter.write_str("signature segment length overflow"),
        }
    }
}

impl Error for SignatureError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Layout(error) => Some(error),
            _ => None,
        }
    }
}

impl From<LayoutError> for SignatureError {
    fn from(error: LayoutError) -> Self {
        Self::Layout(error)
    }
}

/// Validated location of one `Signatures` section and the rows it covers.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SignatureSegment {
    section: SectionDescriptor,
    first_row: u64,
    row_count: u64,
    num_perm: u32,
    value_bytes: u16,
}

impl SignatureSegment {
    /// Return the containing `Signatures` section.
    #[must_use]
    pub const fn section(self) -> SectionDescriptor {
        self.section
    }

    /// Return the zero-based index of the first key row in this segment.
    #[must_use]
    pub const fn first_row(self) -> u64 {
        self.first_row
    }

    /// Return the number of key rows in this segment.
    #[must_use]
    pub const fn row_count(self) -> u64 {
        self.row_count
    }

    /// Return the number of signature values per row.
    #[must_use]
    pub const fn num_perm(self) -> u32 {
        self.num_perm
    }

    /// Return the encoded width of one signature value in bytes.
    #[must_use]
    pub const fn value_bytes(self) -> u16 {
        self.value_bytes
    }

    /// Return whether `row` falls inside this segment.
    #[must_use]
    pub const fn contains_row(self, row: u64) -> bool {
        row >= self.first_row && row - self.first_row < self.row_count
    }
}

/// Return the encoded byte width of one signature row.
pub fn signature_row_bytes(
    scheme: SignatureScheme,
    num_perm: u32,
) -> Result<usize, SignatureError> {
    let value_bytes = usize::from(scheme_value_bytes(scheme));
    usize::try_from(num_perm)
        .map_err(|_| SignatureError::LengthOverflow)?
        .checked_mul(value_bytes)
        .ok_or(SignatureError::LengthOverflow)
}

/// Encode rows `first_row..first_row + rows.len()` as one segment payload.
///
/// Every row must contain exactly `num_perm` values that fit the scheme's
/// value width. Affine32 values are stored as little-endian `u32` and affine64
/// values as little-endian `u64`.
pub fn encode_signature_segment<'a>(
    scheme: SignatureScheme,
    num_perm: u32,
    first_row: u64,
    rows: impl ExactSizeIterator<Item = &'a [u64]>,
) -> Result<Vec<u8>, SignatureError> {
    let value_bytes = scheme_value_bytes(scheme);
    let row_bytes = signature_row_bytes(scheme, num_perm)?;
    let row_count = rows.len();
    let capacity = row_count
        .checked_mul(row_bytes)
        .and_then(|bytes| bytes.checked_add(SIGNATURE_SEGMENT_HEADER_BYTES))
        .ok_or(SignatureError::LengthOverflow)?;
    let row_count_u64 = u64::try_from(row_count).map_err(|_| SignatureError::LengthOverflow)?;
    first_row
        .checked_add(row_count_u64)
        .ok_or(SignatureError::LengthOverflow)?;
    let expected_values = usize::try_from(num_perm).map_err(|_| SignatureError::LengthOverflow)?;

    let mut output = Vec::with_capacity(capacity);
    output.extend_from_slice(&SIGNATURE_SEGMENT_MAGIC);
    output.extend_from_slice(&SIGNATURE_SEGMENT_VERSION.to_le_bytes());
    output.extend_from_slice(&SIGNATURE_SEGMENT_HEADER_BYTES_U16.to_le_bytes());
    output.extend_from_slice(&value_bytes.to_le_bytes());
    output.extend_from_slice(&0_u16.to_le_bytes());
    output.extend_from_slice(&num_perm.to_le_bytes());
    output.extend_from_slice(&0_u32.to_le_bytes());
    output.extend_from_slice(&first_row.to_le_bytes());
    output.extend_from_slice(&row_count_u64.to_le_bytes());
    debug_assert_eq!(output.len(), SIGNATURE_SEGMENT_HEADER_BYTES);

    for row in rows {
        if row.len() != expected_values {
            return Err(SignatureError::Invalid {
                reason: "signature row length does not match num_perm",
            });
        }
        for value in row {
            match scheme {
                SignatureScheme::PariAffine32V1 => {
                    let narrow =
                        u32::try_from(*value).map_err(|_| SignatureError::ValueOutOfRange {
                            value: *value,
                            width_bits: scheme.width_bits(),
                        })?;
                    output.extend_from_slice(&narrow.to_le_bytes());
                }
                SignatureScheme::PariAffine64V1 => output.extend_from_slice(&value.to_le_bytes()),
            }
        }
    }
    Ok(output)
}

/// Read and validate one `Signatures` section header against the file metadata.
pub fn decode_signature_segment<R: Read + Seek>(
    layout: &FileLayout,
    reader: &mut R,
    descriptor: SectionDescriptor,
) -> Result<SignatureSegment, SignatureError> {
    if descriptor.kind() != SectionKind::Signatures {
        return Err(SignatureError::Invalid {
            reason: "section is not a signature section",
        });
    }
    if descriptor.payload_length() < SIGNATURE_SEGMENT_HEADER_BYTES_U64 {
        return Err(SignatureError::Invalid {
            reason: "signature segment is shorter than its header",
        });
    }
    let header =
        layout.read_section_range(reader, descriptor, 0, SIGNATURE_SEGMENT_HEADER_BYTES)?;
    if header.get(..8) != Some(SIGNATURE_SEGMENT_MAGIC.as_slice()) {
        return Err(SignatureError::Invalid {
            reason: "signature segment magic is invalid",
        });
    }
    if read_u16(&header, 8)? != SIGNATURE_SEGMENT_VERSION {
        return Err(SignatureError::Invalid {
            reason: "unsupported signature segment version",
        });
    }
    if read_u16(&header, 10)? != SIGNATURE_SEGMENT_HEADER_BYTES_U16 {
        return Err(SignatureError::Invalid {
            reason: "unexpected signature segment header size",
        });
    }
    let value_bytes = read_u16(&header, 12)?;
    if read_u16(&header, 14)? != 0 || read_u32(&header, 20)? != 0 {
        return Err(SignatureError::Invalid {
            reason: "signature segment reserved fields must be zero",
        });
    }
    let scheme = layout.metadata().signature_scheme();
    if value_bytes != scheme_value_bytes(scheme) {
        return Err(SignatureError::Invalid {
            reason: "signature value width does not match the signature scheme",
        });
    }
    let num_perm = read_u32(&header, 16)?;
    if num_perm != layout.metadata().num_perm() {
        return Err(SignatureError::Invalid {
            reason: "signature permutation count does not match file metadata",
        });
    }
    let first_row = read_u64(&header, 24)?;
    let row_count = read_u64(&header, 32)?;
    first_row
        .checked_add(row_count)
        .ok_or(SignatureError::LengthOverflow)?;
    let row_bytes = u64::try_from(signature_row_bytes(scheme, num_perm)?)
        .map_err(|_| SignatureError::LengthOverflow)?;
    let expected = row_count
        .checked_mul(row_bytes)
        .and_then(|bytes| bytes.checked_add(SIGNATURE_SEGMENT_HEADER_BYTES_U64))
        .ok_or(SignatureError::LengthOverflow)?;
    if expected != descriptor.payload_length() {
        return Err(SignatureError::Invalid {
            reason: "signature segment length does not match its row count",
        });
    }
    Ok(SignatureSegment {
        section: descriptor,
        first_row,
        row_count,
        num_perm,
        value_bytes,
    })
}

/// Require segments to cover rows `0..rows` exactly once, in order.
pub fn validate_signature_coverage(
    segments: &[SignatureSegment],
    rows: u64,
) -> Result<(), SignatureError> {
    let mut next = 0_u64;
    for segment in segments {
        if segment.first_row != next {
            return Err(SignatureError::Invalid {
                reason: "signature segments are not contiguous and ordered",
            });
        }
        next = next
            .checked_add(segment.row_count)
            .ok_or(SignatureError::LengthOverflow)?;
    }
    if next != rows {
        return Err(SignatureError::Invalid {
            reason: "signature segments do not cover every key exactly once",
        });
    }
    Ok(())
}

/// Read one signature row by its absolute row index.
pub fn read_signature_row<R: Read + Seek>(
    layout: &FileLayout,
    reader: &mut R,
    segment: SignatureSegment,
    row: u64,
) -> Result<Vec<u64>, SignatureError> {
    if !segment.contains_row(row) {
        return Err(SignatureError::Invalid {
            reason: "signature row is outside the segment",
        });
    }
    let row_bytes = row_bytes_u64(segment)?;
    let offset = (row - segment.first_row)
        .checked_mul(row_bytes)
        .and_then(|bytes| bytes.checked_add(SIGNATURE_SEGMENT_HEADER_BYTES_U64))
        .ok_or(SignatureError::LengthOverflow)?;
    let length = usize::try_from(row_bytes).map_err(|_| SignatureError::LengthOverflow)?;
    let bytes = layout.read_section_range(reader, segment.section, offset, length)?;
    decode_values(&bytes, segment.value_bytes)
}

/// Read every row of a segment after verifying the section checksum.
///
/// Values are returned row-major: row `n` occupies
/// `values[n * num_perm..(n + 1) * num_perm]`.
pub fn read_signature_segment_values<R: Read + Seek>(
    layout: &FileLayout,
    reader: &mut R,
    segment: SignatureSegment,
) -> Result<Vec<u64>, SignatureError> {
    let payload = layout.read_section(reader, segment.section)?;
    let body = payload
        .get(SIGNATURE_SEGMENT_HEADER_BYTES..)
        .ok_or(SignatureError::Invalid {
            reason: "signature segment is shorter than its header",
        })?;
    decode_values(body, segment.value_bytes)
}

fn row_bytes_u64(segment: SignatureSegment) -> Result<u64, SignatureError> {
    u64::from(segment.num_perm)
        .checked_mul(u64::from(segment.value_bytes))
        .ok_or(SignatureError::LengthOverflow)
}

fn decode_values(bytes: &[u8], value_bytes: u16) -> Result<Vec<u64>, SignatureError> {
    match value_bytes {
        4 => Ok(bytes
            .chunks_exact(4)
            .map(|chunk| u64::from(u32::from_le_bytes([chunk[0], chunk[1], chunk[2], chunk[3]])))
            .collect()),
        8 => Ok(bytes
            .chunks_exact(8)
            .map(|chunk| {
                u64::from_le_bytes([
                    chunk[0], chunk[1], chunk[2], chunk[3], chunk[4], chunk[5], chunk[6], chunk[7],
                ])
            })
            .collect()),
        _ => Err(SignatureError::Invalid {
            reason: "unsupported signature value width",
        }),
    }
}

const fn scheme_value_bytes(scheme: SignatureScheme) -> u16 {
    scheme.width_bits() / 8
}

fn read_u16(bytes: &[u8], offset: usize) -> Result<u16, SignatureError> {
    let raw = bytes
        .get(offset..offset + 2)
        .ok_or(SignatureError::Invalid {
            reason: "truncated signature segment header",
        })?;
    Ok(u16::from_le_bytes([raw[0], raw[1]]))
}

fn read_u32(bytes: &[u8], offset: usize) -> Result<u32, SignatureError> {
    let raw = bytes
        .get(offset..offset + 4)
        .ok_or(SignatureError::Invalid {
            reason: "truncated signature segment header",
        })?;
    Ok(u32::from_le_bytes([raw[0], raw[1], raw[2], raw[3]]))
}

fn read_u64(bytes: &[u8], offset: usize) -> Result<u64, SignatureError> {
    let raw = bytes
        .get(offset..offset + 8)
        .ok_or(SignatureError::Invalid {
            reason: "truncated signature segment header",
        })?;
    Ok(u64::from_le_bytes([
        raw[0], raw[1], raw[2], raw[3], raw[4], raw[5], raw[6], raw[7],
    ]))
}

#[cfg(test)]
mod tests {
    use std::io::Cursor;

    use super::{
        decode_signature_segment, encode_signature_segment, read_signature_row,
        read_signature_segment_values, validate_signature_coverage, SignatureError,
    };
    use crate::{
        Algorithm, CodecId, FileLayout, IndexFile, IndexMetadata, Section, SectionKind,
        SignatureScheme,
    };

    fn file_with(scheme: SignatureScheme, num_perm: u32, segments: Vec<Vec<u8>>) -> Vec<u8> {
        let metadata = IndexMetadata::new(
            Algorithm::MinHashLsh,
            scheme,
            CodecId::U64,
            num_perm,
            7,
            0.8,
            1,
            1,
            0,
        )
        .expect("metadata");
        let sections = segments
            .into_iter()
            .map(|payload| Section::new(SectionKind::Signatures, false, payload).expect("section"))
            .collect();
        IndexFile::new(metadata, sections)
            .expect("file")
            .encode()
            .expect("encode")
    }

    #[test]
    fn affine32_rows_round_trip_by_segment_and_row() {
        let rows = [vec![1_u64, 2, 3], vec![4, 5, u64::from(u32::MAX)]];
        let first = encode_signature_segment(
            SignatureScheme::PariAffine32V1,
            3,
            0,
            rows.iter().map(Vec::as_slice),
        )
        .expect("encode");
        let second = encode_signature_segment(
            SignatureScheme::PariAffine32V1,
            3,
            2,
            [[9_u64, 8, 7].as_slice()].into_iter(),
        )
        .expect("encode");
        let bytes = file_with(SignatureScheme::PariAffine32V1, 3, vec![first, second]);
        let mut cursor = Cursor::new(bytes);
        let layout = FileLayout::read_from(&mut cursor).expect("layout");
        let segments = layout
            .sections()
            .iter()
            .map(|descriptor| {
                decode_signature_segment(&layout, &mut cursor, *descriptor).expect("segment")
            })
            .collect::<Vec<_>>();
        validate_signature_coverage(&segments, 3).expect("coverage");
        assert_eq!(
            read_signature_row(&layout, &mut cursor, segments[0], 1).expect("row"),
            vec![4, 5, u64::from(u32::MAX)]
        );
        assert_eq!(
            read_signature_row(&layout, &mut cursor, segments[1], 2).expect("row"),
            vec![9, 8, 7]
        );
        assert_eq!(
            read_signature_segment_values(&layout, &mut cursor, segments[0]).expect("values"),
            vec![1, 2, 3, 4, 5, u64::from(u32::MAX)]
        );
        assert!(read_signature_row(&layout, &mut cursor, segments[1], 0).is_err());
    }

    #[test]
    fn affine64_keeps_full_width_values() {
        let row = [u64::MAX, 1 << 40];
        let payload = encode_signature_segment(
            SignatureScheme::PariAffine64V1,
            2,
            0,
            [row.as_slice()].into_iter(),
        )
        .expect("encode");
        let bytes = file_with(SignatureScheme::PariAffine64V1, 2, vec![payload]);
        let mut cursor = Cursor::new(bytes);
        let layout = FileLayout::read_from(&mut cursor).expect("layout");
        let segment =
            decode_signature_segment(&layout, &mut cursor, layout.sections()[0]).expect("segment");
        assert_eq!(segment.value_bytes(), 8);
        assert_eq!(
            read_signature_row(&layout, &mut cursor, segment, 0).expect("row"),
            row.to_vec()
        );
    }

    #[test]
    fn encoder_rejects_wrong_widths_and_overflowing_values() {
        assert!(matches!(
            encode_signature_segment(
                SignatureScheme::PariAffine32V1,
                2,
                0,
                [[1_u64].as_slice()].into_iter()
            ),
            Err(SignatureError::Invalid { .. })
        ));
        assert!(matches!(
            encode_signature_segment(
                SignatureScheme::PariAffine32V1,
                1,
                0,
                [[u64::from(u32::MAX) + 1].as_slice()].into_iter()
            ),
            Err(SignatureError::ValueOutOfRange { .. })
        ));
    }

    #[test]
    fn decoder_rejects_metadata_mismatches_and_gaps() {
        // Segment written for 2 permutations inside a 3-permutation file.
        let payload = encode_signature_segment(
            SignatureScheme::PariAffine32V1,
            2,
            0,
            [[1_u64, 2].as_slice()].into_iter(),
        )
        .expect("encode");
        let bytes = file_with(SignatureScheme::PariAffine32V1, 3, vec![payload]);
        let mut cursor = Cursor::new(bytes);
        let layout = FileLayout::read_from(&mut cursor).expect("layout");
        assert!(decode_signature_segment(&layout, &mut cursor, layout.sections()[0]).is_err());

        let gap = encode_signature_segment(
            SignatureScheme::PariAffine32V1,
            1,
            1,
            [[1_u64].as_slice()].into_iter(),
        )
        .expect("encode");
        let bytes = file_with(SignatureScheme::PariAffine32V1, 1, vec![gap]);
        let mut cursor = Cursor::new(bytes);
        let layout = FileLayout::read_from(&mut cursor).expect("layout");
        let segment =
            decode_signature_segment(&layout, &mut cursor, layout.sections()[0]).expect("segment");
        assert!(validate_signature_coverage(&[segment], 2).is_err());
        assert!(validate_signature_coverage(&[], 0).is_ok());
    }

    #[test]
    fn decoder_rejects_corrupt_headers() {
        let mut payload = encode_signature_segment(
            SignatureScheme::PariAffine32V1,
            1,
            0,
            [[1_u64].as_slice()].into_iter(),
        )
        .expect("encode");
        payload[0] = b'X';
        let bytes = file_with(SignatureScheme::PariAffine32V1, 1, vec![payload]);
        let mut cursor = Cursor::new(bytes);
        let layout = FileLayout::read_from(&mut cursor).expect("layout");
        assert!(decode_signature_segment(&layout, &mut cursor, layout.sections()[0]).is_err());
    }
}
