#![forbid(unsafe_code)]
//! Read-only paged bucket storage for Pari similarity indexes.
//!
//! This crate is the first lazy-storage slice of issue #18. It converts the
//! phase-1 snapshot representation into sorted on-disk bucket segments and
//! keeps only compact bucket descriptors in memory after reopen. Membership
//! vectors are read from disk only for buckets touched by a query.

use std::{
    collections::{BTreeMap, HashSet},
    error::Error,
    fmt,
    fs::{self, File, OpenOptions},
    io::{self, Write},
    path::{Path, PathBuf},
};

use crc32fast::hash as crc32;
use pari_core::MinHash32;
use pari_format::{
    Algorithm, CodecId, FileLayout, FormatError, IndexFile, IndexMetadata, LayoutError, Section,
    SectionDescriptor, SectionKind, SignatureScheme,
};
use pari_index::{LshError, LshIndex32, LshParams};

const BUCKET_MAGIC: [u8; 8] = *b"PARIBKT1";
const BUCKET_HEADER_BYTES: usize = 16;
const BUCKET_RECORD_BYTES: usize = 40;
const U64_BYTES: usize = 8;
const FNV_OFFSET_BASIS: u64 = 0xCBF2_9CE4_8422_2325;
const FNV_PRIME: u64 = 0x0000_0100_0000_01B3;

/// Errors returned by the lazy local store.
#[derive(Debug)]
pub enum LazyStoreError {
    /// Filesystem I/O failed.
    Io(io::Error),
    /// The lazy file-layout reader rejected the container.
    Layout(LayoutError),
    /// Encoding a versioned Pari container failed.
    Format(FormatError),
    /// LSH metadata is invalid or incompatible.
    Index(LshError),
    /// Storage-specific payload invariants were violated.
    InvalidSnapshot { reason: &'static str },
    /// Integer conversion or checked layout arithmetic overflowed.
    LengthOverflow,
    /// The destination already exists and is not overwritten implicitly.
    AlreadyExists(PathBuf),
    /// A supplied sketch uses a different MinHash seed.
    IncompatibleSeed { expected: u64, actual: u64 },
    /// A supplied sketch uses a different signature length.
    IncompatiblePermutationCount { expected: usize, actual: usize },
}

impl fmt::Display for LazyStoreError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io(error) => write!(formatter, "lazy Pari store I/O failed: {error}"),
            Self::Layout(error) => error.fmt(formatter),
            Self::Format(error) => error.fmt(formatter),
            Self::Index(error) => error.fmt(formatter),
            Self::InvalidSnapshot { reason } => {
                write!(formatter, "invalid lazy Pari snapshot: {reason}")
            }
            Self::LengthOverflow => formatter.write_str("lazy store layout arithmetic overflowed"),
            Self::AlreadyExists(path) => {
                write!(
                    formatter,
                    "lazy store destination already exists: {}",
                    path.display()
                )
            }
            Self::IncompatibleSeed { expected, actual } => write!(
                formatter,
                "incompatible MinHash seed: expected {expected}, got {actual}"
            ),
            Self::IncompatiblePermutationCount { expected, actual } => write!(
                formatter,
                "incompatible MinHash permutation count: expected {expected}, got {actual}"
            ),
        }
    }
}

impl Error for LazyStoreError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Io(error) => Some(error),
            Self::Layout(error) => Some(error),
            Self::Format(error) => Some(error),
            Self::Index(error) => Some(error),
            _ => None,
        }
    }
}

impl From<io::Error> for LazyStoreError {
    fn from(error: io::Error) -> Self {
        Self::Io(error)
    }
}

impl From<LayoutError> for LazyStoreError {
    fn from(error: LayoutError) -> Self {
        Self::Layout(error)
    }
}

impl From<FormatError> for LazyStoreError {
    fn from(error: FormatError) -> Self {
        Self::Format(error)
    }
}

impl From<LshError> for LazyStoreError {
    fn from(error: LshError) -> Self {
        Self::Index(error)
    }
}

/// Observable lazy-open state.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LazyStats {
    /// Number of live keys encoded by the source snapshot.
    pub items: usize,
    /// Number of distinct bucket descriptors retained in memory.
    pub buckets: usize,
    /// Number of LSH bands.
    pub bands: usize,
    /// Number of rows per band.
    pub rows: usize,
    /// Total persisted file bytes.
    pub file_bytes: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct BucketDescriptor {
    band: u32,
    hash: u64,
    member_offset: u64,
    member_count: u32,
    member_bytes: u64,
    checksum: u32,
}

impl BucketDescriptor {
    const fn sort_key(self) -> (u32, u64) {
        (self.band, self.hash)
    }
}

/// Read-only lazy LSH index backed by paged bucket-membership ranges.
#[derive(Debug)]
pub struct LazyIndex32 {
    file: File,
    layout: FileLayout,
    bucket_section: SectionDescriptor,
    directory: Vec<BucketDescriptor>,
    item_count: usize,
    num_perm: usize,
    seed: u64,
    params: LshParams,
}

impl LazyIndex32 {
    /// Open a converted lazy index without materializing bucket memberships.
    pub fn open(path: impl AsRef<Path>) -> Result<Self, LazyStoreError> {
        let mut file = File::open(path)?;
        let layout = FileLayout::read_from(&mut file)?;
        validate_metadata(layout.metadata())?;
        let bucket_section =
            layout
                .section(SectionKind::Buckets)
                .ok_or(LazyStoreError::InvalidSnapshot {
                    reason: "missing required buckets section",
                })?;
        let keys_section =
            layout
                .section(SectionKind::Keys)
                .ok_or(LazyStoreError::InvalidSnapshot {
                    reason: "missing required keys section",
                })?;

        let item_count = read_source_count(&layout, &mut file, keys_section)?;
        let directory = read_bucket_directory(&layout, &mut file, bucket_section)?;
        let metadata = layout.metadata();
        let num_perm =
            usize::try_from(metadata.num_perm()).map_err(|_| LazyStoreError::LengthOverflow)?;
        let bands =
            usize::try_from(metadata.bands()).map_err(|_| LazyStoreError::LengthOverflow)?;
        let rows = usize::try_from(metadata.rows()).map_err(|_| LazyStoreError::LengthOverflow)?;
        let seed = metadata.seed();
        let threshold = metadata.threshold();
        let params = LshParams::new(bands, rows);
        LshIndex32::with_params(threshold, num_perm, seed, params)?;

        validate_directory(&directory, bucket_section, bands)?;
        Ok(Self {
            file,
            layout,
            bucket_section,
            directory,
            item_count,
            num_perm,
            seed,
            params,
        })
    }

    /// Query approximate candidates while paging only matching bucket ranges.
    pub fn query(&mut self, sketch: &MinHash32) -> Result<Vec<u64>, LazyStoreError> {
        let hashes = self.band_hashes(sketch)?;
        let mut candidates = HashSet::new();
        for (band, hash) in hashes.into_iter().enumerate() {
            let band = u32::try_from(band).map_err(|_| LazyStoreError::LengthOverflow)?;
            if let Some(descriptor) = self.find_bucket(band, hash) {
                for key in self.read_bucket_membership(descriptor)? {
                    candidates.insert(key);
                }
            }
        }
        let mut output: Vec<_> = candidates.into_iter().collect();
        output.sort_unstable();
        Ok(output)
    }

    /// Query a batch while reusing the candidate scratch set.
    pub fn query_many<'a>(
        &mut self,
        sketches: impl IntoIterator<Item = &'a MinHash32>,
    ) -> Result<Vec<Vec<u64>>, LazyStoreError> {
        let mut results = Vec::new();
        let mut candidates = HashSet::new();
        for sketch in sketches {
            candidates.clear();
            let hashes = self.band_hashes(sketch)?;
            for (band, hash) in hashes.into_iter().enumerate() {
                let band = u32::try_from(band).map_err(|_| LazyStoreError::LengthOverflow)?;
                if let Some(descriptor) = self.find_bucket(band, hash) {
                    for key in self.read_bucket_membership(descriptor)? {
                        candidates.insert(key);
                    }
                }
            }
            let mut output: Vec<_> = candidates.iter().copied().collect();
            output.sort_unstable();
            results.push(output);
        }
        Ok(results)
    }

    /// Return compact in-memory directory and file statistics.
    #[must_use]
    pub fn stats(&self) -> LazyStats {
        LazyStats {
            items: self.item_count,
            buckets: self.directory.len(),
            bands: self.params.bands,
            rows: self.params.rows,
            file_bytes: self.layout.file_length(),
        }
    }

    /// Fully verify the bucket section checksum in addition to per-bucket checksums.
    pub fn verify(&mut self) -> Result<(), LazyStoreError> {
        self.layout
            .read_section(&mut self.file, self.bucket_section)?;
        Ok(())
    }

    fn find_bucket(&self, band: u32, hash: u64) -> Option<BucketDescriptor> {
        let key = (band, hash);
        self.directory
            .binary_search_by(|descriptor| descriptor.sort_key().cmp(&key))
            .ok()
            .map(|index| self.directory[index])
    }

    fn read_bucket_membership(
        &mut self,
        descriptor: BucketDescriptor,
    ) -> Result<Vec<u64>, LazyStoreError> {
        let length =
            usize::try_from(descriptor.member_bytes).map_err(|_| LazyStoreError::LengthOverflow)?;
        let bytes = self.layout.read_section_range(
            &mut self.file,
            self.bucket_section,
            descriptor.member_offset,
            length,
        )?;
        let actual = crc32(&bytes);
        if actual != descriptor.checksum {
            return Err(FormatError::SectionChecksumMismatch {
                expected: descriptor.checksum,
                actual,
            }
            .into());
        }
        let expected =
            usize::try_from(descriptor.member_count).map_err(|_| LazyStoreError::LengthOverflow)?;
        let chunks = bytes.chunks_exact(U64_BYTES);
        if !chunks.remainder().is_empty() {
            return Err(LazyStoreError::InvalidSnapshot {
                reason: "bucket membership bytes are not u64 aligned",
            });
        }
        let keys = chunks
            .map(read_u64_exact)
            .collect::<Result<Vec<_>, _>>()?;
        if keys.len() != expected {
            return Err(LazyStoreError::InvalidSnapshot {
                reason: "bucket membership length disagrees with its descriptor",
            });
        }
        Ok(keys)
    }

    fn band_hashes(&self, sketch: &MinHash32) -> Result<Vec<u64>, LazyStoreError> {
        if sketch.seed() != self.seed {
            return Err(LazyStoreError::IncompatibleSeed {
                expected: self.seed,
                actual: sketch.seed(),
            });
        }
        if sketch.num_perm() != self.num_perm {
            return Err(LazyStoreError::IncompatiblePermutationCount {
                expected: self.num_perm,
                actual: sketch.num_perm(),
            });
        }
        let used = self
            .params
            .used_permutations()
            .ok_or(LazyStoreError::LengthOverflow)?;
        Ok(sketch.signature()[..used]
            .chunks_exact(self.params.rows)
            .map(hash_band)
            .collect())
    }
}

/// Convert one committed phase-1 snapshot into a lazy bucket-segment snapshot.
///
/// This first implementation intentionally builds the bucket map in memory. The
/// next #18 slice will replace this conversion with external sorting/streaming
/// while preserving the on-disk read format established here.
pub fn build_from_snapshot(
    source: impl AsRef<Path>,
    destination: impl AsRef<Path>,
) -> Result<(), LazyStoreError> {
    let destination = destination.as_ref();
    if destination.exists() {
        return Err(LazyStoreError::AlreadyExists(destination.to_path_buf()));
    }

    let mut source_file = File::open(source)?;
    let source_layout = FileLayout::read_from(&mut source_file)?;
    validate_metadata(source_layout.metadata())?;
    let keys_descriptor =
        source_layout
            .section(SectionKind::Keys)
            .ok_or(LazyStoreError::InvalidSnapshot {
                reason: "phase-1 source is missing keys",
            })?;
    let hashes_descriptor =
        source_layout
            .section(SectionKind::BandHashes)
            .ok_or(LazyStoreError::InvalidSnapshot {
                reason: "phase-1 source is missing band hashes",
            })?;
    let keys_payload = source_layout.read_section(&mut source_file, keys_descriptor)?;
    let hashes_payload = source_layout.read_section(&mut source_file, hashes_descriptor)?;

    let keys = decode_phase1_keys(&keys_payload)?;
    let bands = usize::try_from(source_layout.metadata().bands())
        .map_err(|_| LazyStoreError::LengthOverflow)?;
    let rows = decode_phase1_band_hashes(&hashes_payload, bands)?;
    if keys.len() != rows.len() {
        return Err(LazyStoreError::InvalidSnapshot {
            reason: "phase-1 key and band-hash counts differ",
        });
    }

    let mut buckets: BTreeMap<(u32, u64), Vec<u64>> = BTreeMap::new();
    for (key, hashes) in keys.iter().copied().zip(&rows) {
        for (band, hash) in hashes.iter().copied().enumerate() {
            let band = u32::try_from(band).map_err(|_| LazyStoreError::LengthOverflow)?;
            buckets.entry((band, hash)).or_default().push(key);
        }
    }
    let bucket_payload = encode_buckets(&buckets)?;
    let metadata = clone_metadata(source_layout.metadata())?;
    let output = IndexFile::new(
        metadata,
        vec![
            Section::new(SectionKind::Keys, true, keys_payload)?,
            Section::new(SectionKind::Buckets, true, bucket_payload)?,
        ],
    )?
    .encode()?;
    atomic_create(destination, &output)
}

fn validate_metadata(metadata: &IndexMetadata) -> Result<(), LazyStoreError> {
    if metadata.algorithm() != Algorithm::MinHashLsh {
        return Err(LazyStoreError::InvalidSnapshot {
            reason: "algorithm is not MinHash LSH",
        });
    }
    if metadata.signature_scheme() != SignatureScheme::PariAffine32V1 {
        return Err(LazyStoreError::InvalidSnapshot {
            reason: "signature scheme is not pari-affine32-v1",
        });
    }
    if metadata.key_codec() != CodecId::U64 {
        return Err(LazyStoreError::InvalidSnapshot {
            reason: "lazy v1 store requires u64 keys",
        });
    }
    if metadata.feature_flags() != 0 {
        return Err(LazyStoreError::InvalidSnapshot {
            reason: "lazy v1 store does not support feature flags",
        });
    }
    Ok(())
}

fn clone_metadata(metadata: &IndexMetadata) -> Result<IndexMetadata, LazyStoreError> {
    IndexMetadata::new(
        metadata.algorithm(),
        metadata.signature_scheme(),
        metadata.key_codec(),
        metadata.num_perm(),
        metadata.seed(),
        metadata.threshold(),
        metadata.bands(),
        metadata.rows(),
        metadata.feature_flags(),
    )
    .map_err(LazyStoreError::Format)
}

fn read_source_count(
    layout: &FileLayout,
    file: &mut File,
    descriptor: SectionDescriptor,
) -> Result<usize, LazyStoreError> {
    let bytes = layout.read_section_range(file, descriptor, 0, U64_BYTES)?;
    let count = read_u64_exact(&bytes)?;
    usize::try_from(count).map_err(|_| LazyStoreError::LengthOverflow)
}

fn read_bucket_directory(
    layout: &FileLayout,
    file: &mut File,
    descriptor: SectionDescriptor,
) -> Result<Vec<BucketDescriptor>, LazyStoreError> {
    let header = layout.read_section_range(file, descriptor, 0, BUCKET_HEADER_BYTES)?;
    if header.get(..8) != Some(BUCKET_MAGIC.as_slice()) {
        return Err(LazyStoreError::InvalidSnapshot {
            reason: "invalid bucket section magic",
        });
    }
    let count = read_u64_exact(header.get(8..16).ok_or(LazyStoreError::InvalidSnapshot {
        reason: "truncated bucket header",
    })?)?;
    let count = usize::try_from(count).map_err(|_| LazyStoreError::LengthOverflow)?;
    let directory_bytes = count
        .checked_mul(BUCKET_RECORD_BYTES)
        .ok_or(LazyStoreError::LengthOverflow)?;
    let records = layout.read_section_range(
        file,
        descriptor,
        u64::try_from(BUCKET_HEADER_BYTES).map_err(|_| LazyStoreError::LengthOverflow)?,
        directory_bytes,
    )?;
    let chunks = records.chunks_exact(BUCKET_RECORD_BYTES);
    if !chunks.remainder().is_empty() {
        return Err(LazyStoreError::InvalidSnapshot {
            reason: "bucket directory is not record aligned",
        });
    }
    let output = chunks
        .map(decode_bucket_record)
        .collect::<Result<Vec<_>, _>>()?;
    if output.len() != count {
        return Err(LazyStoreError::InvalidSnapshot {
            reason: "bucket directory length disagrees with its count",
        });
    }
    Ok(output)
}

fn validate_directory(
    directory: &[BucketDescriptor],
    section: SectionDescriptor,
    bands: usize,
) -> Result<(), LazyStoreError> {
    let directory_bytes = directory
        .len()
        .checked_mul(BUCKET_RECORD_BYTES)
        .and_then(|value| value.checked_add(BUCKET_HEADER_BYTES))
        .ok_or(LazyStoreError::LengthOverflow)?;
    let minimum_membership_offset =
        u64::try_from(directory_bytes).map_err(|_| LazyStoreError::LengthOverflow)?;
    let bands = u32::try_from(bands).map_err(|_| LazyStoreError::LengthOverflow)?;
    let mut previous = None;
    for descriptor in directory {
        if descriptor.band >= bands {
            return Err(LazyStoreError::InvalidSnapshot {
                reason: "bucket descriptor references an out-of-range band",
            });
        }
        if let Some(previous) = previous {
            if previous >= descriptor.sort_key() {
                return Err(LazyStoreError::InvalidSnapshot {
                    reason: "bucket directory must be strictly sorted and unique",
                });
            }
        }
        previous = Some(descriptor.sort_key());
        if descriptor.member_offset < minimum_membership_offset {
            return Err(LazyStoreError::InvalidSnapshot {
                reason: "bucket membership overlaps the directory",
            });
        }
        let expected_bytes = u64::from(descriptor.member_count)
            .checked_mul(u64::try_from(U64_BYTES).map_err(|_| LazyStoreError::LengthOverflow)?)
            .ok_or(LazyStoreError::LengthOverflow)?;
        if descriptor.member_bytes != expected_bytes {
            return Err(LazyStoreError::InvalidSnapshot {
                reason: "bucket member byte length disagrees with member count",
            });
        }
        let end = descriptor
            .member_offset
            .checked_add(descriptor.member_bytes)
            .ok_or(LazyStoreError::LengthOverflow)?;
        if end > section.payload_length() {
            return Err(LazyStoreError::InvalidSnapshot {
                reason: "bucket membership range exceeds the section",
            });
        }
    }
    Ok(())
}

fn encode_buckets(buckets: &BTreeMap<(u32, u64), Vec<u64>>) -> Result<Vec<u8>, LazyStoreError> {
    let count = buckets.len();
    let directory_bytes = count
        .checked_mul(BUCKET_RECORD_BYTES)
        .ok_or(LazyStoreError::LengthOverflow)?;
    let membership_start = BUCKET_HEADER_BYTES
        .checked_add(directory_bytes)
        .ok_or(LazyStoreError::LengthOverflow)?;
    let membership_bytes = buckets.values().try_fold(0_usize, |total, keys| {
        let bytes = keys
            .len()
            .checked_mul(U64_BYTES)
            .ok_or(LazyStoreError::LengthOverflow)?;
        total
            .checked_add(bytes)
            .ok_or(LazyStoreError::LengthOverflow)
    })?;
    let total = membership_start
        .checked_add(membership_bytes)
        .ok_or(LazyStoreError::LengthOverflow)?;
    let mut output = Vec::with_capacity(total);
    output.extend_from_slice(&BUCKET_MAGIC);
    output.extend_from_slice(
        &u64::try_from(count)
            .map_err(|_| LazyStoreError::LengthOverflow)?
            .to_le_bytes(),
    );

    let mut memberships = Vec::with_capacity(membership_bytes);
    let mut offset = u64::try_from(membership_start).map_err(|_| LazyStoreError::LengthOverflow)?;
    for ((band, hash), keys) in buckets {
        let mut bytes = Vec::with_capacity(
            keys.len()
                .checked_mul(U64_BYTES)
                .ok_or(LazyStoreError::LengthOverflow)?,
        );
        for key in keys {
            bytes.extend_from_slice(&key.to_le_bytes());
        }
        let member_count = u32::try_from(keys.len()).map_err(|_| LazyStoreError::LengthOverflow)?;
        let member_bytes =
            u64::try_from(bytes.len()).map_err(|_| LazyStoreError::LengthOverflow)?;
        output.extend_from_slice(&band.to_le_bytes());
        output.extend_from_slice(&member_count.to_le_bytes());
        output.extend_from_slice(&hash.to_le_bytes());
        output.extend_from_slice(&offset.to_le_bytes());
        output.extend_from_slice(&member_bytes.to_le_bytes());
        output.extend_from_slice(&crc32(&bytes).to_le_bytes());
        output.extend_from_slice(&0_u32.to_le_bytes());
        memberships.extend_from_slice(&bytes);
        offset = offset
            .checked_add(member_bytes)
            .ok_or(LazyStoreError::LengthOverflow)?;
    }
    output.extend_from_slice(&memberships);
    Ok(output)
}

fn decode_bucket_record(record: &[u8]) -> Result<BucketDescriptor, LazyStoreError> {
    if record.len() != BUCKET_RECORD_BYTES {
        return Err(LazyStoreError::InvalidSnapshot {
            reason: "truncated bucket directory record",
        });
    }
    let reserved = read_u32_at(record, 36)?;
    if reserved != 0 {
        return Err(LazyStoreError::InvalidSnapshot {
            reason: "bucket directory reserved bytes must be zero",
        });
    }
    Ok(BucketDescriptor {
        band: read_u32_at(record, 0)?,
        member_count: read_u32_at(record, 4)?,
        hash: read_u64_at(record, 8)?,
        member_offset: read_u64_at(record, 16)?,
        member_bytes: read_u64_at(record, 24)?,
        checksum: read_u32_at(record, 32)?,
    })
}

fn decode_phase1_keys(payload: &[u8]) -> Result<Vec<u64>, LazyStoreError> {
    let count = phase1_count(payload)?;
    let expected = U64_BYTES
        .checked_add(
            count
                .checked_mul(U64_BYTES)
                .ok_or(LazyStoreError::LengthOverflow)?,
        )
        .ok_or(LazyStoreError::LengthOverflow)?;
    if payload.len() != expected {
        return Err(LazyStoreError::InvalidSnapshot {
            reason: "phase-1 keys payload length is invalid",
        });
    }
    payload[U64_BYTES..]
        .chunks_exact(U64_BYTES)
        .map(read_u64_exact)
        .collect()
}

fn decode_phase1_band_hashes(
    payload: &[u8],
    bands: usize,
) -> Result<Vec<Vec<u64>>, LazyStoreError> {
    let count = phase1_count(payload)?;
    let row_bytes = bands
        .checked_mul(U64_BYTES)
        .ok_or(LazyStoreError::LengthOverflow)?;
    let expected = U64_BYTES
        .checked_add(
            count
                .checked_mul(row_bytes)
                .ok_or(LazyStoreError::LengthOverflow)?,
        )
        .ok_or(LazyStoreError::LengthOverflow)?;
    if payload.len() != expected {
        return Err(LazyStoreError::InvalidSnapshot {
            reason: "phase-1 band-hash payload length is invalid",
        });
    }
    let mut rows = Vec::with_capacity(count);
    for row in payload[U64_BYTES..].chunks_exact(row_bytes) {
        rows.push(
            row.chunks_exact(U64_BYTES)
                .map(read_u64_exact)
                .collect::<Result<Vec<_>, _>>()?,
        );
    }
    Ok(rows)
}

fn phase1_count(payload: &[u8]) -> Result<usize, LazyStoreError> {
    let bytes = payload
        .get(..U64_BYTES)
        .ok_or(LazyStoreError::InvalidSnapshot {
            reason: "phase-1 section is missing its record count",
        })?;
    let count = read_u64_exact(bytes)?;
    usize::try_from(count).map_err(|_| LazyStoreError::LengthOverflow)
}

fn atomic_create(path: &Path, bytes: &[u8]) -> Result<(), LazyStoreError> {
    if let Some(parent) = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
    {
        fs::create_dir_all(parent)?;
    }
    let temporary = temporary_path(path)?;
    let result = (|| -> Result<(), LazyStoreError> {
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temporary)?;
        file.write_all(bytes)?;
        file.flush()?;
        file.sync_all()?;
        drop(file);
        fs::rename(&temporary, path)?;
        sync_parent(path)?;
        Ok(())
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temporary);
    }
    result
}

fn temporary_path(path: &Path) -> Result<PathBuf, LazyStoreError> {
    let name = path
        .file_name()
        .ok_or(LazyStoreError::InvalidSnapshot {
            reason: "destination path must identify a file",
        })?
        .to_string_lossy();
    Ok(path.with_file_name(format!(".{name}.pari-lazy-tmp")))
}

fn sync_parent(path: &Path) -> Result<(), LazyStoreError> {
    let parent = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    File::open(parent)?.sync_all()?;
    Ok(())
}

fn hash_band(values: &[u32]) -> u64 {
    let mut hash = FNV_OFFSET_BASIS;
    for value in values {
        hash ^= u64::from(*value);
        hash = hash.wrapping_mul(FNV_PRIME);
    }
    hash ^= u64::try_from(values.len()).expect("band length is bounded by num_perm");
    avalanche64(hash)
}

fn avalanche64(mut value: u64) -> u64 {
    value ^= value >> 33;
    value = value.wrapping_mul(0xFF51_AFD7_ED55_8CCD);
    value ^= value >> 33;
    value = value.wrapping_mul(0xC4CE_B9FE_1A85_EC53);
    value ^ (value >> 33)
}

fn read_u32_at(bytes: &[u8], offset: usize) -> Result<u32, LazyStoreError> {
    let end = offset
        .checked_add(4)
        .ok_or(LazyStoreError::LengthOverflow)?;
    let raw: [u8; 4] = bytes
        .get(offset..end)
        .ok_or(LazyStoreError::InvalidSnapshot {
            reason: "truncated u32 field",
        })?
        .try_into()
        .map_err(|_| LazyStoreError::InvalidSnapshot {
            reason: "truncated u32 field",
        })?;
    Ok(u32::from_le_bytes(raw))
}

fn read_u64_at(bytes: &[u8], offset: usize) -> Result<u64, LazyStoreError> {
    let end = offset
        .checked_add(U64_BYTES)
        .ok_or(LazyStoreError::LengthOverflow)?;
    read_u64_exact(
        bytes
            .get(offset..end)
            .ok_or(LazyStoreError::InvalidSnapshot {
                reason: "truncated u64 field",
            })?,
    )
}

fn read_u64_exact(bytes: &[u8]) -> Result<u64, LazyStoreError> {
    let raw: [u8; U64_BYTES] = bytes
        .try_into()
        .map_err(|_| LazyStoreError::InvalidSnapshot {
            reason: "fixed-width u64 field is truncated",
        })?;
    Ok(u64::from_le_bytes(raw))
}

#[cfg(test)]
mod tests {
    use std::{
        fs::{self, OpenOptions},
        io::{Seek, SeekFrom, Write},
        path::{Path, PathBuf},
    };

    use pari_core::MinHash32;
    use pari_format::{FileLayout, SectionKind};
    use pari_index::LshIndex32;
    use pari_store::PersistentIndex32;

    use super::{build_from_snapshot, LazyIndex32, LazyStoreError};

    fn sketch(values: impl IntoIterator<Item = u64>) -> MinHash32 {
        let mut sketch = MinHash32::new(128, 7).expect("valid sketch");
        for value in values {
            sketch.update(&value.to_le_bytes());
        }
        sketch
    }

    fn test_path(name: &str) -> PathBuf {
        std::env::temp_dir().join(format!(
            "pari-lazy-{name}-{}-{}.idx",
            std::process::id(),
            std::thread::current().name().unwrap_or("test")
        ))
    }

    fn cleanup(path: &Path) {
        let _ = fs::remove_file(path);
    }

    fn build_fixture(name: &str) -> (PathBuf, PathBuf, Vec<MinHash32>) {
        let source = test_path(&format!("{name}-source"));
        let lazy = test_path(&format!("{name}-lazy"));
        cleanup(&source);
        cleanup(&lazy);
        let sketches = vec![
            sketch(0..40),
            sketch(0..35),
            sketch(100..140),
            sketch(0..30),
        ];
        let mut store = PersistentIndex32::create(&source, 0.8, 128, 7).expect("create source");
        store
            .insert_many(
                sketches
                    .iter()
                    .enumerate()
                    .map(|(key, value)| (u64::try_from(key).expect("test key fits u64"), value)),
            )
            .expect("insert source");
        store.sync().expect("sync source");
        build_from_snapshot(&source, &lazy).expect("build lazy");
        (source, lazy, sketches)
    }

    #[test]
    fn lazy_candidates_match_phase1_and_memory_references() {
        let (source, lazy_path, sketches) = build_fixture("parity");
        let phase1 = PersistentIndex32::open(&source).expect("open phase1");
        let mut memory = LshIndex32::new(0.8, 128, 7).expect("memory index");
        memory
            .insert_many(
                sketches
                    .iter()
                    .enumerate()
                    .map(|(key, value)| (u64::try_from(key).expect("test key fits u64"), value)),
            )
            .expect("insert memory");
        let mut lazy = LazyIndex32::open(&lazy_path).expect("open lazy");

        for query in &sketches {
            assert_eq!(
                lazy.query(query).expect("lazy query"),
                phase1.query(query).expect("phase1 query")
            );
            assert_eq!(
                lazy.query(query).expect("lazy query"),
                memory.query(query).expect("memory query")
            );
        }
        assert_eq!(lazy.stats().items, sketches.len());
        assert!(lazy.stats().buckets > 0);
        lazy.verify().expect("verify bucket section");
        cleanup(&source);
        cleanup(&lazy_path);
    }

    #[test]
    fn batch_queries_match_scalar_queries() {
        let (source, lazy_path, sketches) = build_fixture("batch");
        let mut lazy = LazyIndex32::open(&lazy_path).expect("open lazy");
        let batch = lazy.query_many(&sketches).expect("batch query");
        let scalar = sketches
            .iter()
            .map(|query| lazy.query(query).expect("scalar query"))
            .collect::<Vec<_>>();
        assert_eq!(batch, scalar);
        cleanup(&source);
        cleanup(&lazy_path);
    }

    #[test]
    fn corrupt_bucket_directory_is_rejected() {
        let (source, lazy_path, _) = build_fixture("directory-corrupt");
        let mut file = OpenOptions::new()
            .read(true)
            .write(true)
            .open(&lazy_path)
            .expect("open file");
        let layout = FileLayout::read_from(&mut file).expect("layout");
        let buckets = layout
            .section(SectionKind::Buckets)
            .expect("bucket section");
        file.seek(SeekFrom::Start(buckets.payload_offset()))
            .expect("seek bucket magic");
        file.write_all(b"BROKEN!!").expect("corrupt bucket magic");
        file.sync_all().expect("sync corruption");
        drop(file);
        assert!(matches!(
            LazyIndex32::open(&lazy_path),
            Err(LazyStoreError::InvalidSnapshot { .. })
        ));
        cleanup(&source);
        cleanup(&lazy_path);
    }

    #[test]
    fn corrupt_membership_fails_per_bucket_checksum() {
        let (source, lazy_path, _) = build_fixture("membership-corrupt");
        let lazy = LazyIndex32::open(&lazy_path).expect("open lazy");
        let descriptor = lazy.directory[0];
        let absolute = lazy
            .bucket_section
            .payload_offset()
            .checked_add(descriptor.member_offset)
            .expect("offset fits");
        drop(lazy);

        let mut file = OpenOptions::new()
            .read(true)
            .write(true)
            .open(&lazy_path)
            .expect("open file");
        file.seek(SeekFrom::Start(absolute))
            .expect("seek membership");
        file.write_all(&[0xFF]).expect("corrupt membership");
        file.sync_all().expect("sync corruption");
        drop(file);

        let mut lazy = LazyIndex32::open(&lazy_path).expect("reopen lazy");
        assert!(matches!(
            lazy.read_bucket_membership(descriptor),
            Err(LazyStoreError::Format(_))
        ));
        cleanup(&source);
        cleanup(&lazy_path);
    }
}
