#![forbid(unsafe_code)]
//! Read-only paged bucket storage for Pari similarity indexes.
//!
//! Lazy indexes use the canonical bucket-segment codec from `pari-format`.
//! Reopening reads validated metadata and compact bucket locations only;
//! membership vectors stay on disk until a query touches the corresponding
//! bucket. The same persisted files retain `BandHashes`, so mutable store
//! implementations can reopen and compact them without format conversion.

use std::{
    collections::{BTreeMap, HashSet},
    error::Error,
    ffi::OsString,
    fmt,
    fs::{self, File, OpenOptions},
    io::{self, Write},
    path::{Path, PathBuf},
    process,
    sync::atomic::{AtomicU64, Ordering},
    time::Instant,
};

use pari_core::MinHash32;
use pari_format::{
    bucket_record_size, decode_bucket_segment, encode_bucket_segment, read_bucket_members,
    validate_global_bucket_order, Algorithm, BucketError, BucketKey, BucketLocation, BucketRecord,
    CodecId, FileLayout, FormatError, IndexFile, IndexMetadata, LayoutError, Section,
    SectionDescriptor, SectionKind, SignatureScheme, BUCKET_SEGMENT_HEADER_BYTES,
    BUCKET_SEGMENT_TARGET_BYTES,
};
use pari_index::{
    explain_lsh, BucketDistribution, LshError, LshIndex32, LshParams, LshPlan, LshPlanError,
    LshPlanOptions, QueryMetrics, StorageMode,
};

const U64_BYTES: usize = 8;
const FNV_OFFSET_BASIS: u64 = 0xCBF2_9CE4_8422_2325;
const FNV_PRIME: u64 = 0x0000_0100_0000_01B3;
const TEMPORARY_CREATE_ATTEMPTS: usize = 128;

static NEXT_TEMPORARY_ID: AtomicU64 = AtomicU64::new(0);

/// Errors returned by the lazy local store.
#[derive(Debug)]
pub enum LazyStoreError {
    /// Filesystem I/O failed.
    Io(io::Error),
    /// The lazy file-layout reader rejected the outer container.
    Layout(LayoutError),
    /// Encoding a versioned Pari container failed.
    Format(FormatError),
    /// The canonical bucket-segment codec rejected persisted bucket data.
    Bucket(BucketError),
    /// LSH metadata is invalid or incompatible.
    Index(LshError),
    /// Storage-specific payload invariants were violated.
    InvalidSnapshot { reason: &'static str },
    /// Integer conversion or checked layout arithmetic overflowed.
    LengthOverflow,
    /// The destination already exists and is not overwritten implicitly.
    AlreadyExists(PathBuf),
    /// A supplied sketch uses a different `MinHash` seed.
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
            Self::Bucket(error) => error.fmt(formatter),
            Self::Index(error) => error.fmt(formatter),
            Self::InvalidSnapshot { reason } => {
                write!(formatter, "invalid lazy Pari snapshot: {reason}")
            }
            Self::LengthOverflow => formatter.write_str("lazy store layout arithmetic overflowed"),
            Self::AlreadyExists(path) => write!(
                formatter,
                "lazy store destination already exists: {}",
                path.display()
            ),
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
            Self::Bucket(error) => Some(error),
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

impl From<BucketError> for LazyStoreError {
    fn from(error: BucketError) -> Self {
        Self::Bucket(error)
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
    /// Exact stored bucket membership distribution.
    pub distribution: BucketDistribution,
    /// Process-local query metrics when observability is enabled.
    pub queries: Option<QueryMetrics>,
}

/// Read-only lazy LSH index backed by checksummed bucket segments.
#[derive(Debug)]
pub struct LazyIndex32 {
    file: File,
    layout: FileLayout,
    bucket_sections: Vec<SectionDescriptor>,
    directory: Vec<BucketLocation>,
    item_count: usize,
    num_perm: usize,
    seed: u64,
    params: LshParams,
    query_metrics: Option<QueryMetrics>,
}

impl LazyIndex32 {
    /// Open a lazy index without materializing bucket memberships.
    pub fn open(path: impl AsRef<Path>) -> Result<Self, LazyStoreError> {
        let mut file = File::open(path)?;
        let layout = FileLayout::read_from(&mut file)?;
        validate_metadata(layout.metadata())?;
        let keys_section = required_unique_section(&layout, SectionKind::Keys)?;
        let bucket_sections = collect_bucket_sections(&layout)?;
        if bucket_sections.is_empty() {
            return Err(LazyStoreError::InvalidSnapshot {
                reason: "missing required bucket sections",
            });
        }

        let item_count = read_source_count(&layout, &mut file, keys_section)?;
        let num_perm = usize::try_from(layout.metadata().num_perm())
            .map_err(|_| LazyStoreError::LengthOverflow)?;
        let bands = usize::try_from(layout.metadata().bands())
            .map_err(|_| LazyStoreError::LengthOverflow)?;
        let rows = usize::try_from(layout.metadata().rows())
            .map_err(|_| LazyStoreError::LengthOverflow)?;
        let seed = layout.metadata().seed();
        let threshold = layout.metadata().threshold();
        let params = LshParams::new(bands, rows);
        LshIndex32::with_params(threshold, num_perm, seed, params)?;

        let mut directory = Vec::new();
        for descriptor in &bucket_sections {
            directory.extend(decode_bucket_segment(
                &layout,
                &mut file,
                *descriptor,
                bands,
            )?);
        }
        validate_global_bucket_order(&directory)?;

        Ok(Self {
            file,
            layout,
            bucket_sections,
            directory,
            item_count,
            num_perm,
            seed,
            params,
            query_metrics: None,
        })
    }

    /// Query approximate candidates while paging only matching member ranges.
    pub fn query(&mut self, sketch: &MinHash32) -> Result<Vec<u64>, LazyStoreError> {
        let started = self.query_metrics.as_ref().map(|_| Instant::now());
        let hashes = self.band_hashes(sketch)?;
        let mut candidates = HashSet::new();
        self.collect_candidates(&hashes, &mut candidates)?;
        let mut output: Vec<_> = candidates.into_iter().collect();
        output.sort_unstable();
        if let (Some(metrics), Some(started)) = (&mut self.query_metrics, started) {
            metrics.record(1, output.len(), self.item_count, started.elapsed());
        }
        Ok(output)
    }

    /// Query a batch while reusing candidate scratch storage.
    pub fn query_many<'a>(
        &mut self,
        sketches: impl IntoIterator<Item = &'a MinHash32>,
    ) -> Result<Vec<Vec<u64>>, LazyStoreError> {
        let started = self.query_metrics.as_ref().map(|_| Instant::now());
        let mut results = Vec::new();
        let mut candidates = HashSet::new();
        let mut candidate_count = 0_usize;
        for sketch in sketches {
            candidates.clear();
            let hashes = self.band_hashes(sketch)?;
            self.collect_candidates(&hashes, &mut candidates)?;
            let mut output: Vec<_> = candidates.iter().copied().collect();
            output.sort_unstable();
            candidate_count = candidate_count.saturating_add(output.len());
            results.push(output);
        }
        if let (Some(metrics), Some(started)) = (&mut self.query_metrics, started) {
            metrics.record(
                results.len(),
                candidate_count,
                results.len().saturating_mul(self.item_count),
                started.elapsed(),
            );
        }
        Ok(results)
    }

    /// Enable or disable process-local query observation.
    pub fn set_observability(&mut self, enabled: bool) {
        self.query_metrics = enabled.then(QueryMetrics::default);
    }

    /// Explain this index's persisted configuration without scanning buckets.
    pub fn explain(&self) -> Result<LshPlan, LshPlanError> {
        explain_lsh(
            LshPlanOptions::new(
                u64::try_from(self.item_count).unwrap_or(u64::MAX),
                self.layout.metadata().threshold(),
                self.num_perm,
            )
            .storage_mode(StorageMode::Lazy),
            self.params,
        )
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
            distribution: BucketDistribution::from_sizes(
                self.directory
                    .iter()
                    .map(|bucket| usize::try_from(bucket.member_count()).unwrap_or(usize::MAX)),
            ),
            queries: self.query_metrics,
        }
    }

    /// Verify the outer checksums of every bucket section.
    ///
    /// Directory structure is validated during open and each member range is
    /// independently verified when queried.
    pub fn verify(&mut self) -> Result<(), LazyStoreError> {
        for descriptor in &self.bucket_sections {
            self.layout.read_section(&mut self.file, *descriptor)?;
        }
        Ok(())
    }

    fn collect_candidates(
        &mut self,
        hashes: &[u64],
        output: &mut HashSet<u64>,
    ) -> Result<(), LazyStoreError> {
        for (band, hash) in hashes.iter().copied().enumerate() {
            let key = BucketKey::new(
                u32::try_from(band).map_err(|_| LazyStoreError::LengthOverflow)?,
                hash,
            );
            if let Ok(index) = self
                .directory
                .binary_search_by_key(&key, |location| location.key())
            {
                for member in
                    read_bucket_members(&self.layout, &mut self.file, self.directory[index])?
                {
                    output.insert(member);
                }
            }
        }
        Ok(())
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

/// Convert one committed phase-1 snapshot into the canonical lazy format.
///
/// This reference builder materializes the bucket map in memory. Large builds
/// should use `pari-store-build`, which produces byte-compatible segments using
/// bounded spill buffers and external merge.
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
    let keys_descriptor = required_unique_section(&source_layout, SectionKind::Keys)?;
    let hashes_descriptor = required_unique_section(&source_layout, SectionKind::BandHashes)?;
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

    let mut buckets: BTreeMap<BucketKey, Vec<u64>> = BTreeMap::new();
    for (key, hashes) in keys.iter().copied().zip(&rows) {
        for (band, hash) in hashes.iter().copied().enumerate() {
            buckets
                .entry(BucketKey::new(
                    u32::try_from(band).map_err(|_| LazyStoreError::LengthOverflow)?,
                    hash,
                ))
                .or_default()
                .push(key);
        }
    }

    let metadata = clone_metadata(source_layout.metadata())?;
    let mut sections = vec![
        Section::new(SectionKind::Keys, true, keys_payload)?,
        Section::new(SectionKind::BandHashes, true, hashes_payload)?,
    ];
    append_bucket_sections(&mut sections, &buckets)?;
    let output = IndexFile::new(metadata, sections)?.encode()?;
    atomic_create(destination, &output)
}

fn append_bucket_sections(
    sections: &mut Vec<Section>,
    buckets: &BTreeMap<BucketKey, Vec<u64>>,
) -> Result<(), LazyStoreError> {
    if buckets.is_empty() {
        sections.push(Section::new(
            SectionKind::Buckets,
            true,
            encode_bucket_segment(&[])?,
        )?);
        return Ok(());
    }

    let mut current: Vec<(BucketKey, &[u64])> = Vec::new();
    let mut estimated = BUCKET_SEGMENT_HEADER_BYTES;
    for (key, members) in buckets {
        let contribution = bucket_record_size(members.len())?;
        if !current.is_empty()
            && estimated
                .checked_add(contribution)
                .ok_or(LazyStoreError::LengthOverflow)?
                > BUCKET_SEGMENT_TARGET_BYTES
        {
            push_bucket_section(sections, &current)?;
            current.clear();
            estimated = BUCKET_SEGMENT_HEADER_BYTES;
        }
        estimated = estimated
            .checked_add(contribution)
            .ok_or(LazyStoreError::LengthOverflow)?;
        current.push((*key, members));
    }
    if !current.is_empty() {
        push_bucket_section(sections, &current)?;
    }
    Ok(())
}

fn push_bucket_section(
    sections: &mut Vec<Section>,
    records: &[(BucketKey, &[u64])],
) -> Result<(), LazyStoreError> {
    let records: Vec<_> = records
        .iter()
        .map(|(key, members)| BucketRecord::new(*key, members))
        .collect();
    sections.push(Section::new(
        SectionKind::Buckets,
        true,
        encode_bucket_segment(&records)?,
    )?);
    Ok(())
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

fn required_unique_section(
    layout: &FileLayout,
    kind: SectionKind,
) -> Result<SectionDescriptor, LazyStoreError> {
    let mut matches = layout
        .sections()
        .iter()
        .copied()
        .filter(|descriptor| descriptor.kind() == kind);
    let descriptor = matches.next().ok_or(LazyStoreError::InvalidSnapshot {
        reason: missing_section_reason(kind),
    })?;
    if matches.next().is_some() {
        return Err(LazyStoreError::InvalidSnapshot {
            reason: duplicate_section_reason(kind),
        });
    }
    if !descriptor.required() {
        return Err(LazyStoreError::InvalidSnapshot {
            reason: "required metadata section is marked optional",
        });
    }
    Ok(descriptor)
}

fn collect_bucket_sections(layout: &FileLayout) -> Result<Vec<SectionDescriptor>, LazyStoreError> {
    let mut sections = Vec::new();
    for descriptor in layout.sections() {
        match descriptor.kind() {
            SectionKind::Buckets => {
                if !descriptor.required() {
                    return Err(LazyStoreError::InvalidSnapshot {
                        reason: "bucket sections must be required",
                    });
                }
                sections.push(*descriptor);
            }
            SectionKind::Tombstones if descriptor.required() => {
                return Err(LazyStoreError::InvalidSnapshot {
                    reason: "required tombstone sections are not supported",
                });
            }
            SectionKind::Keys | SectionKind::BandHashes | SectionKind::Tombstones => {}
        }
    }
    Ok(sections)
}

const fn missing_section_reason(kind: SectionKind) -> &'static str {
    match kind {
        SectionKind::Keys => "missing required keys section",
        SectionKind::BandHashes => "missing required band-hash section",
        SectionKind::Buckets => "missing required bucket section",
        SectionKind::Tombstones => "missing required tombstone section",
    }
}

const fn duplicate_section_reason(kind: SectionKind) -> &'static str {
    match kind {
        SectionKind::Keys => "duplicate keys section",
        SectionKind::BandHashes => "duplicate band-hash section",
        SectionKind::Buckets => "duplicate singleton bucket section",
        SectionKind::Tombstones => "duplicate tombstone section",
    }
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
    usize::try_from(read_u64_exact(bytes)?).map_err(|_| LazyStoreError::LengthOverflow)
}

fn atomic_create(path: &Path, bytes: &[u8]) -> Result<(), LazyStoreError> {
    atomic_create_with(path, bytes, |_| Ok(()))
}

fn atomic_create_with(
    path: &Path,
    bytes: &[u8],
    before_publish: impl FnOnce(&Path) -> Result<(), LazyStoreError>,
) -> Result<(), LazyStoreError> {
    if let Some(parent) = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
    {
        fs::create_dir_all(parent)?;
    }
    let (temporary, mut file) = create_temporary(path)?;
    let result = (|| -> Result<(), LazyStoreError> {
        file.write_all(bytes)?;
        file.flush()?;
        file.sync_all()?;
        drop(file);
        before_publish(&temporary)?;
        match fs::hard_link(&temporary, path) {
            Ok(()) => {}
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {
                return Err(LazyStoreError::AlreadyExists(path.to_path_buf()));
            }
            Err(error) => return Err(LazyStoreError::Io(error)),
        }
        fs::remove_file(&temporary)?;
        sync_parent(path)?;
        Ok(())
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temporary);
    }
    result
}

fn create_temporary(path: &Path) -> Result<(PathBuf, File), LazyStoreError> {
    for _ in 0..TEMPORARY_CREATE_ATTEMPTS {
        let temporary = temporary_path(path, NEXT_TEMPORARY_ID.fetch_add(1, Ordering::Relaxed))?;
        match OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temporary)
        {
            Ok(file) => return Ok((temporary, file)),
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {}
            Err(error) => return Err(LazyStoreError::Io(error)),
        }
    }
    Err(LazyStoreError::Io(io::Error::new(
        io::ErrorKind::AlreadyExists,
        "could not allocate a unique lazy-store temporary file",
    )))
}

fn temporary_path(path: &Path, id: u64) -> Result<PathBuf, LazyStoreError> {
    let file_name = path.file_name().ok_or(LazyStoreError::InvalidSnapshot {
        reason: "destination path must identify a file",
    })?;
    let mut temporary_name = OsString::from(".");
    temporary_name.push(file_name);
    temporary_name.push(format!(".pari-lazy-{}-{id}.tmp", process::id()));
    Ok(path.with_file_name(temporary_name))
}

#[cfg_attr(not(unix), allow(clippy::unnecessary_wraps))]
fn sync_parent(path: &Path) -> Result<(), LazyStoreError> {
    #[cfg(windows)]
    {
        let _ = path;
        Ok(())
    }
    #[cfg(not(windows))]
    {
        let parent = path
            .parent()
            .filter(|parent| !parent.as_os_str().is_empty())
            .unwrap_or_else(|| Path::new("."));
        File::open(parent)?.sync_all()?;
        Ok(())
    }
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

fn read_u64_exact(bytes: &[u8]) -> Result<u64, LazyStoreError> {
    let raw: [u8; U64_BYTES] = bytes
        .try_into()
        .map_err(|_| LazyStoreError::InvalidSnapshot {
            reason: "truncated fixed-width u64",
        })?;
    Ok(u64::from_le_bytes(raw))
}

#[cfg(test)]
mod tests {
    use std::{
        cell::RefCell,
        fs,
        fs::OpenOptions,
        io::{Seek, SeekFrom, Write},
        path::{Path, PathBuf},
        sync::atomic::{AtomicU64, Ordering},
    };

    use pari_core::MinHash32;
    use pari_format::{FileLayout, SectionKind};
    use pari_index::LshIndex32;
    use pari_store::PersistentIndex32;

    use super::{atomic_create_with, build_from_snapshot, LazyIndex32, LazyStoreError};

    static NEXT_TEST_PATH: AtomicU64 = AtomicU64::new(0);

    fn test_path(name: &str) -> PathBuf {
        std::env::temp_dir().join(format!(
            "pari-lazy-{name}-{}-{}.idx",
            std::process::id(),
            NEXT_TEST_PATH.fetch_add(1, Ordering::Relaxed)
        ))
    }

    fn cleanup(path: &PathBuf) {
        let _ = fs::remove_file(path);
    }

    fn assert_already_exists(error: LazyStoreError, expected: &Path) {
        match error {
            LazyStoreError::AlreadyExists(path) => assert_eq!(path, expected),
            other => panic!("expected AlreadyExists, got {other:?}"),
        }
    }

    fn sketch(base: u64) -> MinHash32 {
        let mut sketch = MinHash32::new(128, 7).expect("valid sketch");
        for value in base..base + 40 {
            sketch.update(&value.to_le_bytes());
        }
        sketch
    }

    fn build_fixture(name: &str) -> (PathBuf, PathBuf, Vec<MinHash32>) {
        let source = test_path(&format!("{name}-source"));
        let lazy = test_path(&format!("{name}-lazy"));
        cleanup(&source);
        cleanup(&lazy);
        let sketches = vec![sketch(0), sketch(5), sketch(1_000), sketch(2_000)];
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
    fn lazy_queries_match_phase1_and_memory_reference() {
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
        let initial = lazy.stats();
        assert!(initial.distribution.memberships > 0);
        assert!(initial.queries.is_none());
        let explanation = lazy.explain().expect("explain");
        assert_eq!(explanation.expected_items, 4);
        assert_eq!(explanation.parameter_source.as_str(), "existing");
        assert_eq!(explanation.requested_storage.as_str(), "lazy");
        lazy.set_observability(true);
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
        let observed = lazy.stats().queries.expect("query metrics");
        assert_eq!(observed.operations, 8);
        assert_eq!(observed.queries, 8);
        assert!(observed.candidates > 0);
        lazy.verify().expect("verify");
        cleanup(&source);
        cleanup(&lazy_path);
    }

    #[test]
    fn converted_snapshot_preserves_band_hash_metadata() {
        let (source, lazy_path, _) = build_fixture("metadata");
        let mut file = fs::File::open(&lazy_path).expect("open lazy file");
        let layout = FileLayout::read_from(&mut file).expect("layout");
        assert!(layout.section(SectionKind::Keys).is_some());
        assert!(layout.section(SectionKind::BandHashes).is_some());
        assert!(layout
            .sections()
            .iter()
            .any(|section| section.kind() == SectionKind::Buckets));
        cleanup(&source);
        cleanup(&lazy_path);
    }

    #[test]
    fn pre_existing_destination_is_preserved_with_typed_error() {
        let (source, lazy_path, _) = build_fixture("pre-existing");
        let existing = b"bytes owned by an earlier writer";
        fs::write(&lazy_path, existing).expect("replace fixture with sentinel");

        let error =
            build_from_snapshot(&source, &lazy_path).expect_err("destination must conflict");

        assert_already_exists(error, &lazy_path);
        assert_eq!(fs::read(&lazy_path).expect("read sentinel"), existing);
        cleanup(&source);
        cleanup(&lazy_path);
    }

    #[test]
    fn concurrent_destination_claim_is_preserved_and_owned_temporary_is_removed() {
        let destination = test_path("publication-race");
        let unrelated = test_path("publication-race-unrelated-temp");
        cleanup(&destination);
        cleanup(&unrelated);
        fs::write(&unrelated, b"unrelated transaction").expect("write unrelated temporary");
        let temporary = RefCell::new(None);
        let concurrent = b"bytes owned by the concurrent writer";

        let error = atomic_create_with(&destination, b"new lazy index", |owned_temporary| {
            temporary.replace(Some(owned_temporary.to_path_buf()));
            let mut file = OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&destination)?;
            file.write_all(concurrent)?;
            file.sync_all()?;
            Ok(())
        })
        .expect_err("concurrent destination must conflict");

        assert_already_exists(error, &destination);
        assert_eq!(
            fs::read(&destination).expect("read concurrent bytes"),
            concurrent
        );
        assert!(!temporary
            .into_inner()
            .expect("capture owned temporary")
            .exists());
        assert_eq!(
            fs::read(&unrelated).expect("read unrelated temporary"),
            b"unrelated transaction"
        );
        cleanup(&destination);
        cleanup(&unrelated);
    }

    #[test]
    fn successful_publication_is_byte_exact_and_removes_temporary() {
        let destination = test_path("publication-success");
        cleanup(&destination);
        let temporary = RefCell::new(None);
        let expected = b"exact canonical lazy-index bytes";

        atomic_create_with(&destination, expected, |owned_temporary| {
            temporary.replace(Some(owned_temporary.to_path_buf()));
            Ok(())
        })
        .expect("publish destination");

        assert_eq!(fs::read(&destination).expect("read destination"), expected);
        assert!(!temporary
            .into_inner()
            .expect("capture owned temporary")
            .exists());
        cleanup(&destination);
    }

    #[test]
    fn corrupt_bucket_payload_fails_verification() {
        let (source, lazy_path, _) = build_fixture("corrupt");
        let mut file = OpenOptions::new()
            .read(true)
            .write(true)
            .open(&lazy_path)
            .expect("open file");
        let layout = FileLayout::read_from(&mut file).expect("layout");
        let bucket = layout
            .sections()
            .iter()
            .copied()
            .find(|section| section.kind() == SectionKind::Buckets)
            .expect("bucket section");
        let position = bucket
            .payload_offset()
            .checked_add(bucket.payload_length().saturating_sub(1))
            .expect("position");
        file.seek(SeekFrom::Start(position)).expect("seek");
        file.write_all(&[0xA5]).expect("corrupt");
        file.sync_all().expect("sync");
        drop(file);

        let mut lazy = LazyIndex32::open(&lazy_path).expect("directory still opens");
        assert!(lazy.verify().is_err());
        cleanup(&source);
        cleanup(&lazy_path);
    }
}
