use std::{
    error::Error,
    fmt,
    io::{self, Read, Seek, SeekFrom},
};

use crc32fast::hash as crc32;

use crate::{Algorithm, CodecId, FormatError, IndexMetadata, SectionKind, SignatureScheme};

const MAGIC: [u8; 8] = *b"PARIIDX\0";
const FORMAT_VERSION: u16 = 1;
const HEADER_SIZE: usize = 72;
const HEADER_SIZE_U16: u16 = 72;
const HEADER_SIZE_U64: u64 = 72;
const SECTION_HEADER_SIZE: usize = 16;
const SECTION_HEADER_SIZE_U64: u64 = 16;
const SECTION_FLAG_REQUIRED: u16 = 1;
const KNOWN_SECTION_FLAGS: u16 = SECTION_FLAG_REQUIRED;
const MAX_SECTION_COUNT: usize = 1_024;
const MAX_SECTION_BYTES: u64 = 256 * 1024 * 1024;
const SUPPORTED_FEATURE_FLAGS: u64 = 0;

/// File I/O or structural errors produced by the lazy container reader.
#[derive(Debug)]
pub enum LayoutError {
    /// The underlying reader failed.
    Io(io::Error),
    /// The bytes violate the versioned Pari container format.
    Format(FormatError),
    /// A requested byte range falls outside its section payload.
    RangeOutOfBounds {
        /// Offset relative to the beginning of the section payload.
        offset: u64,
        /// Requested number of bytes.
        length: usize,
        /// Total section payload length.
        section_length: u64,
    },
}

impl fmt::Display for LayoutError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io(error) => write!(formatter, "Pari index I/O failed: {error}"),
            Self::Format(error) => error.fmt(formatter),
            Self::RangeOutOfBounds {
                offset,
                length,
                section_length,
            } => write!(
                formatter,
                "section range offset {offset} with length {length} exceeds payload length {section_length}"
            ),
        }
    }
}

impl Error for LayoutError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Io(error) => Some(error),
            Self::Format(error) => Some(error),
            Self::RangeOutOfBounds { .. } => None,
        }
    }
}

impl From<io::Error> for LayoutError {
    fn from(error: io::Error) -> Self {
        Self::Io(error)
    }
}

impl From<FormatError> for LayoutError {
    fn from(error: FormatError) -> Self {
        Self::Format(error)
    }
}

/// Location and integrity metadata for one understood section in a Pari file.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SectionDescriptor {
    kind: SectionKind,
    required: bool,
    payload_offset: u64,
    payload_length: u64,
    checksum: u32,
}

impl SectionDescriptor {
    /// Return the known section kind.
    #[must_use]
    pub const fn kind(self) -> SectionKind {
        self.kind
    }

    /// Return whether a reader must understand this section.
    #[must_use]
    pub const fn required(self) -> bool {
        self.required
    }

    /// Return the absolute file offset at which the payload begins.
    #[must_use]
    pub const fn payload_offset(self) -> u64 {
        self.payload_offset
    }

    /// Return the payload length in bytes.
    #[must_use]
    pub const fn payload_length(self) -> u64 {
        self.payload_length
    }

    /// Return the CRC32 stored in the section frame.
    #[must_use]
    pub const fn checksum(self) -> u32 {
        self.checksum
    }
}

/// Validated file metadata and section locations without materialized payloads.
#[derive(Debug, Clone, PartialEq)]
pub struct FileLayout {
    metadata: IndexMetadata,
    sections: Vec<SectionDescriptor>,
    file_length: u64,
}

impl FileLayout {
    /// Scan a seekable Pari container without reading section payloads.
    ///
    /// Memory use is proportional to the number of understood sections rather
    /// than the file size. Unknown optional sections are validated and skipped;
    /// unknown required sections are rejected.
    pub fn read_from<R: Read + Seek>(reader: &mut R) -> Result<Self, LayoutError> {
        let file_length = reader.seek(SeekFrom::End(0))?;
        reader.seek(SeekFrom::Start(0))?;

        let mut header = [0_u8; HEADER_SIZE];
        read_exact_format(reader, &mut header, "file header")?;
        let (metadata, section_count) = decode_header(&header)?;
        let section_count =
            usize::try_from(section_count).map_err(|_| FormatError::LengthOverflow)?;
        if section_count > MAX_SECTION_COUNT {
            return Err(FormatError::TooManySections {
                count: section_count,
                max: MAX_SECTION_COUNT,
            }
            .into());
        }

        let mut cursor = HEADER_SIZE_U64;
        let mut sections = Vec::with_capacity(section_count.min(16));
        for _ in 0..section_count {
            let header_end = cursor
                .checked_add(SECTION_HEADER_SIZE_U64)
                .ok_or(FormatError::LengthOverflow)?;
            if header_end > file_length {
                return Err(FormatError::Truncated {
                    context: "section header",
                }
                .into());
            }

            reader.seek(SeekFrom::Start(cursor))?;
            let mut section_header = [0_u8; SECTION_HEADER_SIZE];
            read_exact_format(reader, &mut section_header, "section header")?;
            let kind_raw = read_u16(&section_header, 0)?;
            let flags = read_u16(&section_header, 2)?;
            let payload_length = read_u64(&section_header, 4)?;
            let checksum = read_u32(&section_header, 12)?;
            if flags & !KNOWN_SECTION_FLAGS != 0 {
                return Err(FormatError::InvalidSectionFlags { flags }.into());
            }
            if payload_length > MAX_SECTION_BYTES {
                return Err(FormatError::SectionTooLarge {
                    length: payload_length,
                }
                .into());
            }

            let payload_offset = header_end;
            let payload_end = payload_offset
                .checked_add(payload_length)
                .ok_or(FormatError::LengthOverflow)?;
            if payload_end > file_length {
                return Err(FormatError::Truncated {
                    context: "section payload",
                }
                .into());
            }

            let required = flags & SECTION_FLAG_REQUIRED != 0;
            if let Some(kind) = section_kind_from_raw(kind_raw) {
                sections.push(SectionDescriptor {
                    kind,
                    required,
                    payload_offset,
                    payload_length,
                    checksum,
                });
            } else if required {
                return Err(FormatError::UnknownRequiredSection { kind: kind_raw }.into());
            }
            cursor = payload_end;
        }

        if cursor != file_length {
            let remaining = file_length
                .checked_sub(cursor)
                .ok_or(FormatError::LengthOverflow)?;
            let remaining = usize::try_from(remaining).map_err(|_| FormatError::LengthOverflow)?;
            return Err(FormatError::TrailingBytes { remaining }.into());
        }

        Ok(Self {
            metadata,
            sections,
            file_length,
        })
    }

    /// Return validated index metadata from the file header.
    #[must_use]
    pub const fn metadata(&self) -> &IndexMetadata {
        &self.metadata
    }

    /// Return descriptors for understood sections in file order.
    #[must_use]
    pub fn sections(&self) -> &[SectionDescriptor] {
        &self.sections
    }

    /// Return the validated total file length.
    #[must_use]
    pub const fn file_length(&self) -> u64 {
        self.file_length
    }

    /// Return the first understood section of `kind`, if present.
    #[must_use]
    pub fn section(&self, kind: SectionKind) -> Option<SectionDescriptor> {
        self.sections
            .iter()
            .copied()
            .find(|descriptor| descriptor.kind == kind)
    }

    /// Materialize one complete section and verify its stored checksum.
    pub fn read_section<R: Read + Seek>(
        &self,
        reader: &mut R,
        descriptor: SectionDescriptor,
    ) -> Result<Vec<u8>, LayoutError> {
        let length =
            usize::try_from(descriptor.payload_length).map_err(|_| FormatError::LengthOverflow)?;
        let payload = self.read_section_range(reader, descriptor, 0, length)?;
        let actual = crc32(&payload);
        if actual != descriptor.checksum {
            return Err(FormatError::SectionChecksumMismatch {
                expected: descriptor.checksum,
                actual,
            }
            .into());
        }
        Ok(payload)
    }

    /// Read a bounded byte range from a section without materializing it all.
    ///
    /// Range reads validate section and file bounds but cannot verify the
    /// section-level CRC32 because only a subset of the payload is read.
    pub fn read_section_range<R: Read + Seek>(
        &self,
        reader: &mut R,
        descriptor: SectionDescriptor,
        offset: u64,
        length: usize,
    ) -> Result<Vec<u8>, LayoutError> {
        let length_u64 = u64::try_from(length).map_err(|_| FormatError::LengthOverflow)?;
        let range_end = offset
            .checked_add(length_u64)
            .ok_or(FormatError::LengthOverflow)?;
        if range_end > descriptor.payload_length {
            return Err(LayoutError::RangeOutOfBounds {
                offset,
                length,
                section_length: descriptor.payload_length,
            });
        }

        let absolute = descriptor
            .payload_offset
            .checked_add(offset)
            .ok_or(FormatError::LengthOverflow)?;
        let absolute_end = absolute
            .checked_add(length_u64)
            .ok_or(FormatError::LengthOverflow)?;
        if absolute_end > self.file_length {
            return Err(FormatError::Truncated {
                context: "section range",
            }
            .into());
        }

        reader.seek(SeekFrom::Start(absolute))?;
        let mut output = vec![0_u8; length];
        read_exact_format(reader, &mut output, "section range")?;
        Ok(output)
    }
}

fn read_exact_format<R: Read>(
    reader: &mut R,
    output: &mut [u8],
    context: &'static str,
) -> Result<(), LayoutError> {
    match reader.read_exact(output) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == io::ErrorKind::UnexpectedEof => {
            Err(FormatError::Truncated { context }.into())
        }
        Err(error) => Err(LayoutError::Io(error)),
    }
}

fn decode_header(header: &[u8; HEADER_SIZE]) -> Result<(IndexMetadata, u32), FormatError> {
    if header.get(0..8) != Some(MAGIC.as_slice()) {
        return Err(FormatError::InvalidMagic);
    }
    let version = read_u16(header, 8)?;
    if version != FORMAT_VERSION {
        return Err(FormatError::UnsupportedVersion { found: version });
    }
    let header_size = read_u16(header, 10)?;
    if header_size != HEADER_SIZE_U16 {
        return Err(FormatError::InvalidHeaderSize { found: header_size });
    }
    if header.get(36..40) != Some(&[0, 0, 0, 0]) || header.get(68..72) != Some(&[0, 0, 0, 0]) {
        return Err(FormatError::InvalidReservedBytes);
    }

    let expected_checksum = read_u32(header, 64)?;
    let actual_checksum = crc32(header.get(..64).ok_or(FormatError::Truncated {
        context: "header checksum range",
    })?);
    if expected_checksum != actual_checksum {
        return Err(FormatError::HeaderChecksumMismatch {
            expected: expected_checksum,
            actual: actual_checksum,
        });
    }

    let algorithm_raw = read_u16(header, 12)?;
    let algorithm = algorithm_from_raw(algorithm_raw).ok_or(FormatError::UnsupportedAlgorithm {
        value: algorithm_raw,
    })?;
    let scheme_raw = read_u16(header, 14)?;
    let signature_scheme = signature_scheme_from_raw(scheme_raw)
        .ok_or(FormatError::UnsupportedSignatureScheme { value: scheme_raw })?;
    let signature_width = read_u16(header, 16)?;
    if signature_width != signature_scheme.width_bits() {
        return Err(FormatError::SignatureWidthMismatch {
            expected: signature_scheme.width_bits(),
            actual: signature_width,
        });
    }
    let codec_raw = read_u16(header, 18)?;
    let key_codec =
        CodecId::from_raw(codec_raw).ok_or(FormatError::UnsupportedCodec { value: codec_raw })?;
    let num_perm = read_u32(header, 20)?;
    let bands = read_u32(header, 24)?;
    let rows = read_u32(header, 28)?;
    let section_count = read_u32(header, 32)?;
    let seed = read_u64(header, 40)?;
    let threshold = f64::from_le_bytes(read_array::<8>(header, 48)?);
    let feature_flags = read_u64(header, 56)?;
    if feature_flags & !SUPPORTED_FEATURE_FLAGS != 0 {
        return Err(FormatError::UnsupportedFeatures {
            flags: feature_flags & !SUPPORTED_FEATURE_FLAGS,
        });
    }

    let metadata = IndexMetadata::new(
        algorithm,
        signature_scheme,
        key_codec,
        num_perm,
        seed,
        threshold,
        bands,
        rows,
        feature_flags,
    )?;
    Ok((metadata, section_count))
}

const fn algorithm_from_raw(value: u16) -> Option<Algorithm> {
    match value {
        1 => Some(Algorithm::MinHashLsh),
        _ => None,
    }
}

const fn signature_scheme_from_raw(value: u16) -> Option<SignatureScheme> {
    match value {
        1 => Some(SignatureScheme::PariAffine32V1),
        2 => Some(SignatureScheme::PariAffine64V1),
        _ => None,
    }
}

const fn section_kind_from_raw(value: u16) -> Option<SectionKind> {
    match value {
        1 => Some(SectionKind::Keys),
        2 => Some(SectionKind::BandHashes),
        3 => Some(SectionKind::Buckets),
        4 => Some(SectionKind::Tombstones),
        _ => None,
    }
}

fn read_u16(bytes: &[u8], offset: usize) -> Result<u16, FormatError> {
    Ok(u16::from_le_bytes(read_array::<2>(bytes, offset)?))
}

fn read_u32(bytes: &[u8], offset: usize) -> Result<u32, FormatError> {
    Ok(u32::from_le_bytes(read_array::<4>(bytes, offset)?))
}

fn read_u64(bytes: &[u8], offset: usize) -> Result<u64, FormatError> {
    Ok(u64::from_le_bytes(read_array::<8>(bytes, offset)?))
}

fn read_array<const N: usize>(bytes: &[u8], offset: usize) -> Result<[u8; N], FormatError> {
    let end = offset.checked_add(N).ok_or(FormatError::LengthOverflow)?;
    bytes
        .get(offset..end)
        .ok_or(FormatError::Truncated { context: "field" })?
        .try_into()
        .map_err(|_| FormatError::Truncated { context: "field" })
}

#[cfg(test)]
mod tests {
    use std::io::Cursor;

    use super::{FileLayout, LayoutError, HEADER_SIZE};
    use crate::{FormatError, SectionKind};

    const GOLDEN_V1: &[u8] = include_bytes!("../testdata/index_v1.bin");

    #[test]
    fn golden_layout_reads_metadata_and_payload_on_demand() {
        let mut cursor = Cursor::new(GOLDEN_V1);
        let layout = FileLayout::read_from(&mut cursor).expect("scan golden layout");
        assert_eq!(
            layout.file_length(),
            u64::try_from(GOLDEN_V1.len()).expect("fixture length fits u64")
        );
        assert_eq!(layout.metadata().num_perm(), 128);
        assert_eq!(layout.sections().len(), 1);
        let keys = layout.section(SectionKind::Keys).expect("keys descriptor");
        assert_eq!(keys.payload_length(), 8);
        assert_eq!(
            layout.read_section(&mut cursor, keys).expect("read keys"),
            7_u64.to_le_bytes()
        );
    }

    #[test]
    fn range_reads_are_bounded() {
        let mut cursor = Cursor::new(GOLDEN_V1);
        let layout = FileLayout::read_from(&mut cursor).expect("scan golden layout");
        let keys = layout.section(SectionKind::Keys).expect("keys descriptor");
        assert_eq!(
            layout
                .read_section_range(&mut cursor, keys, 2, 3)
                .expect("bounded range"),
            7_u64.to_le_bytes()[2..5]
        );
        assert!(matches!(
            layout.read_section_range(&mut cursor, keys, 7, 2),
            Err(LayoutError::RangeOutOfBounds { .. })
        ));
    }

    #[test]
    fn every_truncated_golden_prefix_is_rejected() {
        for end in 0..GOLDEN_V1.len() {
            let mut cursor = Cursor::new(&GOLDEN_V1[..end]);
            assert!(
                FileLayout::read_from(&mut cursor).is_err(),
                "accepted prefix {end}"
            );
        }
    }

    #[test]
    fn payload_corruption_is_deferred_until_complete_section_read() {
        let mut bytes = GOLDEN_V1.to_vec();
        let last = bytes.len() - 1;
        bytes[last] ^= 1;
        let mut cursor = Cursor::new(bytes);
        let layout = FileLayout::read_from(&mut cursor).expect("scan without payload read");
        let keys = layout.section(SectionKind::Keys).expect("keys descriptor");
        assert!(matches!(
            layout.read_section(&mut cursor, keys),
            Err(LayoutError::Format(
                FormatError::SectionChecksumMismatch { .. }
            ))
        ));
    }

    #[test]
    fn unknown_optional_is_skipped_and_unknown_required_is_rejected() {
        let mut optional = GOLDEN_V1.to_vec();
        optional[HEADER_SIZE..HEADER_SIZE + 2].copy_from_slice(&999_u16.to_le_bytes());
        optional[HEADER_SIZE + 2..HEADER_SIZE + 4].copy_from_slice(&0_u16.to_le_bytes());
        let mut cursor = Cursor::new(optional);
        let layout = FileLayout::read_from(&mut cursor).expect("unknown optional section");
        assert!(layout.sections().is_empty());

        let mut required = GOLDEN_V1.to_vec();
        required[HEADER_SIZE..HEADER_SIZE + 2].copy_from_slice(&999_u16.to_le_bytes());
        let mut cursor = Cursor::new(required);
        assert!(matches!(
            FileLayout::read_from(&mut cursor),
            Err(LayoutError::Format(FormatError::UnknownRequiredSection {
                kind: 999
            }))
        ));
    }

    #[test]
    fn malicious_section_length_is_rejected_before_payload_read() {
        let mut bytes = GOLDEN_V1.to_vec();
        bytes[HEADER_SIZE + 4..HEADER_SIZE + 12].copy_from_slice(&u64::MAX.to_le_bytes());
        let mut cursor = Cursor::new(bytes);
        assert!(matches!(
            FileLayout::read_from(&mut cursor),
            Err(LayoutError::Format(FormatError::SectionTooLarge {
                length: u64::MAX
            }))
        ));
    }
}
