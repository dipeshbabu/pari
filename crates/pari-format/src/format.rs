use std::{error::Error, fmt};

use crc32fast::hash as crc32;

use crate::CodecId;

const MAGIC: [u8; 8] = *b"PARIIDX\0";
const FORMAT_VERSION: u16 = 1;
const HEADER_SIZE: usize = 72;
const HEADER_SIZE_U16: u16 = 72;
const SECTION_HEADER_SIZE: usize = 16;
const SECTION_FLAG_REQUIRED: u16 = 1;
const KNOWN_SECTION_FLAGS: u16 = SECTION_FLAG_REQUIRED;
const MAX_SECTION_COUNT: usize = 1_024;
const MAX_SECTION_BYTES: u64 = 256 * 1024 * 1024;
const SUPPORTED_FEATURE_FLAGS: u64 = 0;

/// Algorithm identifier stored in a Pari index header.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u16)]
pub enum Algorithm {
    /// `MinHash` locality-sensitive hashing.
    MinHashLsh = 1,
}

impl Algorithm {
    const fn from_raw(value: u16) -> Option<Self> {
        match value {
            1 => Some(Self::MinHashLsh),
            _ => None,
        }
    }

    const fn as_raw(self) -> u16 {
        self as u16
    }
}

/// Signature scheme identifier stored independently from the index algorithm.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u16)]
pub enum SignatureScheme {
    /// Pari's `pari-affine32-v1` scheme.
    PariAffine32V1 = 1,
    /// Pari's `pari-affine64-v1` scheme.
    PariAffine64V1 = 2,
}

impl SignatureScheme {
    const fn from_raw(value: u16) -> Option<Self> {
        match value {
            1 => Some(Self::PariAffine32V1),
            2 => Some(Self::PariAffine64V1),
            _ => None,
        }
    }

    const fn as_raw(self) -> u16 {
        self as u16
    }

    /// Return the width of each signature value in bits.
    #[must_use]
    pub const fn width_bits(self) -> u16 {
        match self {
            Self::PariAffine32V1 => 32,
            Self::PariAffine64V1 => 64,
        }
    }
}

/// Known top-level section types in a Pari index container.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u16)]
pub enum SectionKind {
    /// Encoded external keys.
    Keys = 1,
    /// Per-item band hashes used for updates and removals.
    BandHashes = 2,
    /// LSH bucket membership.
    Buckets = 3,
    /// Deletion/tombstone state for append-oriented stores.
    Tombstones = 4,
}

impl SectionKind {
    const fn from_raw(value: u16) -> Option<Self> {
        match value {
            1 => Some(Self::Keys),
            2 => Some(Self::BandHashes),
            3 => Some(Self::Buckets),
            4 => Some(Self::Tombstones),
            _ => None,
        }
    }

    const fn as_raw(self) -> u16 {
        self as u16
    }
}

/// Validated metadata that identifies how an index must be interpreted.
#[derive(Debug, Clone, PartialEq)]
pub struct IndexMetadata {
    algorithm: Algorithm,
    signature_scheme: SignatureScheme,
    key_codec: CodecId,
    num_perm: u32,
    seed: u64,
    threshold: f64,
    bands: u32,
    rows: u32,
    feature_flags: u64,
}

impl IndexMetadata {
    /// Construct and validate version-1 index metadata.
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        algorithm: Algorithm,
        signature_scheme: SignatureScheme,
        key_codec: CodecId,
        num_perm: u32,
        seed: u64,
        threshold: f64,
        bands: u32,
        rows: u32,
        feature_flags: u64,
    ) -> Result<Self, FormatError> {
        validate_metadata(num_perm, threshold, bands, rows, feature_flags)?;
        Ok(Self {
            algorithm,
            signature_scheme,
            key_codec,
            num_perm,
            seed,
            threshold,
            bands,
            rows,
            feature_flags,
        })
    }

    /// Return the stored algorithm.
    #[must_use]
    pub const fn algorithm(&self) -> Algorithm {
        self.algorithm
    }

    /// Return the stored signature scheme.
    #[must_use]
    pub const fn signature_scheme(&self) -> SignatureScheme {
        self.signature_scheme
    }

    /// Return the stored key codec.
    #[must_use]
    pub const fn key_codec(&self) -> CodecId {
        self.key_codec
    }

    /// Return the number of signature permutations.
    #[must_use]
    pub const fn num_perm(&self) -> u32 {
        self.num_perm
    }

    /// Return the signature permutation seed.
    #[must_use]
    pub const fn seed(&self) -> u64 {
        self.seed
    }

    /// Return the target similarity threshold.
    #[must_use]
    pub const fn threshold(&self) -> f64 {
        self.threshold
    }

    /// Return the number of LSH bands.
    #[must_use]
    pub const fn bands(&self) -> u32 {
        self.bands
    }

    /// Return the number of rows in each LSH band.
    #[must_use]
    pub const fn rows(&self) -> u32 {
        self.rows
    }

    /// Return required feature bits for this index.
    #[must_use]
    pub const fn feature_flags(&self) -> u64 {
        self.feature_flags
    }
}

/// A known framed section in an index container.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Section {
    kind: SectionKind,
    required: bool,
    payload: Vec<u8>,
}

impl Section {
    /// Create a bounded section payload.
    pub fn new(kind: SectionKind, required: bool, payload: Vec<u8>) -> Result<Self, FormatError> {
        let length = u64::try_from(payload.len()).map_err(|_| FormatError::LengthOverflow)?;
        if length > MAX_SECTION_BYTES {
            return Err(FormatError::SectionTooLarge { length });
        }
        Ok(Self {
            kind,
            required,
            payload,
        })
    }

    /// Return the section kind.
    #[must_use]
    pub const fn kind(&self) -> SectionKind {
        self.kind
    }

    /// Return whether readers must understand this section.
    #[must_use]
    pub const fn required(&self) -> bool {
        self.required
    }

    /// Borrow the section payload.
    #[must_use]
    pub fn payload(&self) -> &[u8] {
        &self.payload
    }
}

/// A small in-memory representation of the versioned container.
///
/// Persistent backends can use the same header and section framing while
/// streaming section payloads instead of materializing this type.
#[derive(Debug, Clone, PartialEq)]
pub struct IndexFile {
    metadata: IndexMetadata,
    sections: Vec<Section>,
}

impl IndexFile {
    /// Build an index container from validated metadata and known sections.
    pub fn new(metadata: IndexMetadata, sections: Vec<Section>) -> Result<Self, FormatError> {
        if sections.len() > MAX_SECTION_COUNT {
            return Err(FormatError::TooManySections {
                count: sections.len(),
                max: MAX_SECTION_COUNT,
            });
        }
        Ok(Self { metadata, sections })
    }

    /// Return the index metadata.
    #[must_use]
    pub const fn metadata(&self) -> &IndexMetadata {
        &self.metadata
    }

    /// Return known decoded sections. Unknown optional sections are skipped.
    #[must_use]
    pub fn sections(&self) -> &[Section] {
        &self.sections
    }

    /// Encode this container to the stable version-1 byte format.
    pub fn encode(&self) -> Result<Vec<u8>, FormatError> {
        let section_count =
            u32::try_from(self.sections.len()).map_err(|_| FormatError::TooManySections {
                count: self.sections.len(),
                max: MAX_SECTION_COUNT,
            })?;
        let mut total = HEADER_SIZE;
        for section in &self.sections {
            total = total
                .checked_add(SECTION_HEADER_SIZE)
                .and_then(|value| value.checked_add(section.payload.len()))
                .ok_or(FormatError::LengthOverflow)?;
        }

        let mut output = Vec::with_capacity(total);
        output.extend_from_slice(&encode_header(&self.metadata, section_count));
        for section in &self.sections {
            encode_section(section, &mut output)?;
        }
        Ok(output)
    }

    /// Decode a complete in-memory container with bounds and checksum checks.
    pub fn decode(bytes: &[u8]) -> Result<Self, FormatError> {
        let (metadata, section_count) = decode_header(bytes)?;
        let section_count =
            usize::try_from(section_count).map_err(|_| FormatError::LengthOverflow)?;
        if section_count > MAX_SECTION_COUNT {
            return Err(FormatError::TooManySections {
                count: section_count,
                max: MAX_SECTION_COUNT,
            });
        }

        let mut cursor = HEADER_SIZE;
        let mut sections = Vec::with_capacity(section_count.min(16));
        for _ in 0..section_count {
            let header_end = cursor
                .checked_add(SECTION_HEADER_SIZE)
                .ok_or(FormatError::LengthOverflow)?;
            let header = bytes
                .get(cursor..header_end)
                .ok_or(FormatError::Truncated {
                    context: "section header",
                })?;
            let kind_raw = read_u16(header, 0)?;
            let flags = read_u16(header, 2)?;
            let length = read_u64(header, 4)?;
            let expected_checksum = read_u32(header, 12)?;
            if flags & !KNOWN_SECTION_FLAGS != 0 {
                return Err(FormatError::InvalidSectionFlags { flags });
            }
            if length > MAX_SECTION_BYTES {
                return Err(FormatError::SectionTooLarge { length });
            }
            let payload_length =
                usize::try_from(length).map_err(|_| FormatError::LengthOverflow)?;
            let payload_end = header_end
                .checked_add(payload_length)
                .ok_or(FormatError::LengthOverflow)?;
            let payload = bytes
                .get(header_end..payload_end)
                .ok_or(FormatError::Truncated {
                    context: "section payload",
                })?;
            let actual_checksum = crc32(payload);
            if actual_checksum != expected_checksum {
                return Err(FormatError::SectionChecksumMismatch {
                    expected: expected_checksum,
                    actual: actual_checksum,
                });
            }

            let required = flags & SECTION_FLAG_REQUIRED != 0;
            if let Some(kind) = SectionKind::from_raw(kind_raw) {
                sections.push(Section::new(kind, required, payload.to_vec())?);
            } else if required {
                return Err(FormatError::UnknownRequiredSection { kind: kind_raw });
            }
            cursor = payload_end;
        }

        if cursor != bytes.len() {
            return Err(FormatError::TrailingBytes {
                remaining: bytes.len() - cursor,
            });
        }
        Self::new(metadata, sections)
    }
}

/// Errors produced while encoding or decoding the Pari index container.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FormatError {
    /// The input ends before a complete field or frame is available.
    Truncated { context: &'static str },
    /// The file does not begin with the Pari index magic bytes.
    InvalidMagic,
    /// The reader does not support this format version.
    UnsupportedVersion { found: u16 },
    /// Version 1 has a fixed header size.
    InvalidHeaderSize { found: u16 },
    /// Reserved version-1 header bytes are nonzero.
    InvalidReservedBytes,
    /// Header integrity validation failed.
    HeaderChecksumMismatch { expected: u32, actual: u32 },
    /// The algorithm identifier is unknown.
    UnsupportedAlgorithm { value: u16 },
    /// The signature scheme identifier is unknown.
    UnsupportedSignatureScheme { value: u16 },
    /// The persisted signature width disagrees with its scheme.
    SignatureWidthMismatch { expected: u16, actual: u16 },
    /// The key codec identifier is unknown.
    UnsupportedCodec { value: u16 },
    /// Metadata violates a format invariant.
    InvalidMetadata { reason: &'static str },
    /// Required feature bits are not supported by this reader.
    UnsupportedFeatures { flags: u64 },
    /// The section table is unreasonably large for version 1.
    TooManySections { count: usize, max: usize },
    /// A section exceeds the bounded in-memory section size.
    SectionTooLarge { length: u64 },
    /// Section flags contain unknown bits.
    InvalidSectionFlags { flags: u16 },
    /// An unknown section was explicitly marked required.
    UnknownRequiredSection { kind: u16 },
    /// Section payload integrity validation failed.
    SectionChecksumMismatch { expected: u32, actual: u32 },
    /// Checked arithmetic or platform conversion overflowed.
    LengthOverflow,
    /// Bytes remain after the declared section table.
    TrailingBytes { remaining: usize },
}

impl fmt::Display for FormatError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Truncated { context } => {
                write!(formatter, "truncated Pari index while reading {context}")
            }
            Self::InvalidMagic => formatter.write_str("invalid Pari index magic"),
            Self::UnsupportedVersion { found } => {
                write!(formatter, "unsupported Pari index format version {found}")
            }
            Self::InvalidHeaderSize { found } => {
                write!(formatter, "invalid version-1 header size {found}")
            }
            Self::InvalidReservedBytes => {
                formatter.write_str("reserved version-1 header bytes must be zero")
            }
            Self::HeaderChecksumMismatch { expected, actual } => write!(
                formatter,
                "header checksum mismatch: stored {expected:#010x}, computed {actual:#010x}"
            ),
            Self::UnsupportedAlgorithm { value } => {
                write!(formatter, "unsupported algorithm identifier {value}")
            }
            Self::UnsupportedSignatureScheme { value } => {
                write!(formatter, "unsupported signature scheme identifier {value}")
            }
            Self::SignatureWidthMismatch { expected, actual } => write!(
                formatter,
                "signature width mismatch: scheme requires {expected}, header stores {actual}"
            ),
            Self::UnsupportedCodec { value } => {
                write!(formatter, "unsupported key codec identifier {value}")
            }
            Self::InvalidMetadata { reason } => {
                write!(formatter, "invalid index metadata: {reason}")
            }
            Self::UnsupportedFeatures { flags } => {
                write!(formatter, "unsupported required feature flags {flags:#018x}")
            }
            Self::TooManySections { count, max } => {
                write!(formatter, "index declares {count} sections; maximum is {max}")
            }
            Self::SectionTooLarge { length } => write!(
                formatter,
                "section payload is {length} bytes; version-1 in-memory maximum is {MAX_SECTION_BYTES}"
            ),
            Self::InvalidSectionFlags { flags } => {
                write!(formatter, "unknown section flag bits {flags:#06x}")
            }
            Self::UnknownRequiredSection { kind } => {
                write!(formatter, "unknown required section kind {kind}")
            }
            Self::SectionChecksumMismatch { expected, actual } => write!(
                formatter,
                "section checksum mismatch: stored {expected:#010x}, computed {actual:#010x}"
            ),
            Self::LengthOverflow => formatter.write_str("index length arithmetic overflowed"),
            Self::TrailingBytes { remaining } => {
                write!(formatter, "{remaining} trailing bytes after declared sections")
            }
        }
    }
}

impl Error for FormatError {}

fn validate_metadata(
    num_perm: u32,
    threshold: f64,
    bands: u32,
    rows: u32,
    feature_flags: u64,
) -> Result<(), FormatError> {
    if num_perm == 0 {
        return Err(FormatError::InvalidMetadata {
            reason: "num_perm must be positive",
        });
    }
    if !threshold.is_finite() || threshold <= 0.0 || threshold > 1.0 {
        return Err(FormatError::InvalidMetadata {
            reason: "threshold must be finite and in (0, 1]",
        });
    }
    let Some(used) = bands.checked_mul(rows) else {
        return Err(FormatError::InvalidMetadata {
            reason: "bands * rows overflows",
        });
    };
    if bands == 0 || rows == 0 || used > num_perm {
        return Err(FormatError::InvalidMetadata {
            reason: "bands and rows must be positive and use no more than num_perm values",
        });
    }
    if feature_flags & !SUPPORTED_FEATURE_FLAGS != 0 {
        return Err(FormatError::UnsupportedFeatures {
            flags: feature_flags & !SUPPORTED_FEATURE_FLAGS,
        });
    }
    Ok(())
}

fn encode_header(metadata: &IndexMetadata, section_count: u32) -> [u8; HEADER_SIZE] {
    let mut header = [0_u8; HEADER_SIZE];
    header[0..8].copy_from_slice(&MAGIC);
    header[8..10].copy_from_slice(&FORMAT_VERSION.to_le_bytes());
    header[10..12].copy_from_slice(&HEADER_SIZE_U16.to_le_bytes());
    header[12..14].copy_from_slice(&metadata.algorithm.as_raw().to_le_bytes());
    header[14..16].copy_from_slice(&metadata.signature_scheme.as_raw().to_le_bytes());
    header[16..18].copy_from_slice(&metadata.signature_scheme.width_bits().to_le_bytes());
    header[18..20].copy_from_slice(&metadata.key_codec.as_raw().to_le_bytes());
    header[20..24].copy_from_slice(&metadata.num_perm.to_le_bytes());
    header[24..28].copy_from_slice(&metadata.bands.to_le_bytes());
    header[28..32].copy_from_slice(&metadata.rows.to_le_bytes());
    header[32..36].copy_from_slice(&section_count.to_le_bytes());
    header[40..48].copy_from_slice(&metadata.seed.to_le_bytes());
    header[48..56].copy_from_slice(&metadata.threshold.to_le_bytes());
    header[56..64].copy_from_slice(&metadata.feature_flags.to_le_bytes());
    let checksum = crc32(&header[..64]);
    header[64..68].copy_from_slice(&checksum.to_le_bytes());
    header
}

fn decode_header(bytes: &[u8]) -> Result<(IndexMetadata, u32), FormatError> {
    let header = bytes.get(..HEADER_SIZE).ok_or(FormatError::Truncated {
        context: "file header",
    })?;
    if header.get(0..8) != Some(MAGIC.as_slice()) {
        return Err(FormatError::InvalidMagic);
    }
    let version = read_u16(header, 8)?;
    if version != FORMAT_VERSION {
        return Err(FormatError::UnsupportedVersion { found: version });
    }
    let header_size = read_u16(header, 10)?;
    if usize::from(header_size) != HEADER_SIZE {
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
    let algorithm =
        Algorithm::from_raw(algorithm_raw).ok_or(FormatError::UnsupportedAlgorithm {
            value: algorithm_raw,
        })?;
    let scheme_raw = read_u16(header, 14)?;
    let signature_scheme = SignatureScheme::from_raw(scheme_raw)
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

fn encode_section(section: &Section, output: &mut Vec<u8>) -> Result<(), FormatError> {
    let length = u64::try_from(section.payload.len()).map_err(|_| FormatError::LengthOverflow)?;
    if length > MAX_SECTION_BYTES {
        return Err(FormatError::SectionTooLarge { length });
    }
    output.extend_from_slice(&section.kind.as_raw().to_le_bytes());
    let flags = if section.required {
        SECTION_FLAG_REQUIRED
    } else {
        0
    };
    output.extend_from_slice(&flags.to_le_bytes());
    output.extend_from_slice(&length.to_le_bytes());
    output.extend_from_slice(&crc32(&section.payload).to_le_bytes());
    output.extend_from_slice(&section.payload);
    Ok(())
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
    use super::{
        Algorithm, FormatError, IndexFile, IndexMetadata, Section, SectionKind, SignatureScheme,
        HEADER_SIZE,
    };
    use crate::CodecId;

    const GOLDEN_V1: &[u8] = include_bytes!("../testdata/index_v1.bin");

    fn golden_file() -> IndexFile {
        let metadata = IndexMetadata::new(
            Algorithm::MinHashLsh,
            SignatureScheme::PariAffine32V1,
            CodecId::U64,
            128,
            42,
            0.8,
            32,
            4,
            0,
        )
        .expect("valid metadata");
        let section = Section::new(SectionKind::Keys, true, 7_u64.to_le_bytes().to_vec())
            .expect("valid section");
        IndexFile::new(metadata, vec![section]).expect("valid file")
    }

    #[test]
    fn golden_fixture_is_stable_and_round_trips() {
        let file = golden_file();
        assert_eq!(file.encode().expect("encode"), GOLDEN_V1);
        assert_eq!(IndexFile::decode(GOLDEN_V1).expect("decode"), file);
    }

    #[test]
    fn every_truncated_golden_prefix_is_rejected() {
        for end in 0..GOLDEN_V1.len() {
            assert!(
                IndexFile::decode(&GOLDEN_V1[..end]).is_err(),
                "accepted prefix {end}"
            );
        }
    }

    #[test]
    fn header_and_payload_corruption_are_detected() {
        let mut header_corrupt = GOLDEN_V1.to_vec();
        header_corrupt[20] ^= 1;
        assert!(matches!(
            IndexFile::decode(&header_corrupt),
            Err(FormatError::HeaderChecksumMismatch { .. })
        ));

        let mut payload_corrupt = GOLDEN_V1.to_vec();
        let last = payload_corrupt.len() - 1;
        payload_corrupt[last] ^= 1;
        assert!(matches!(
            IndexFile::decode(&payload_corrupt),
            Err(FormatError::SectionChecksumMismatch { .. })
        ));
    }

    #[test]
    fn unknown_optional_sections_are_skipped() {
        let mut bytes = GOLDEN_V1.to_vec();
        bytes[HEADER_SIZE..HEADER_SIZE + 2].copy_from_slice(&999_u16.to_le_bytes());
        bytes[HEADER_SIZE + 2..HEADER_SIZE + 4].copy_from_slice(&0_u16.to_le_bytes());
        let decoded =
            IndexFile::decode(&bytes).expect("optional unknown section should be skipped");
        assert!(decoded.sections().is_empty());
    }

    #[test]
    fn unknown_required_sections_are_rejected() {
        let mut bytes = GOLDEN_V1.to_vec();
        bytes[HEADER_SIZE..HEADER_SIZE + 2].copy_from_slice(&999_u16.to_le_bytes());
        assert_eq!(
            IndexFile::decode(&bytes),
            Err(FormatError::UnknownRequiredSection { kind: 999 })
        );
    }

    #[test]
    fn malicious_section_length_is_rejected_before_slicing() {
        let mut bytes = GOLDEN_V1.to_vec();
        bytes[HEADER_SIZE + 4..HEADER_SIZE + 12].copy_from_slice(&u64::MAX.to_le_bytes());
        assert_eq!(
            IndexFile::decode(&bytes),
            Err(FormatError::SectionTooLarge { length: u64::MAX })
        );
    }

    #[test]
    fn reserved_and_trailing_bytes_are_rejected() {
        let mut reserved = GOLDEN_V1.to_vec();
        reserved[68] = 1;
        assert_eq!(
            IndexFile::decode(&reserved),
            Err(FormatError::InvalidReservedBytes)
        );

        let mut trailing = GOLDEN_V1.to_vec();
        trailing.push(0);
        assert_eq!(
            IndexFile::decode(&trailing),
            Err(FormatError::TrailingBytes { remaining: 1 })
        );
    }

    #[test]
    fn invalid_metadata_is_rejected_at_construction() {
        assert!(matches!(
            IndexMetadata::new(
                Algorithm::MinHashLsh,
                SignatureScheme::PariAffine32V1,
                CodecId::U64,
                128,
                1,
                0.8,
                33,
                4,
                0,
            ),
            Err(FormatError::InvalidMetadata { .. })
        ));
    }
}
