#![forbid(unsafe_code)]
//! Crash-safe affine32 and affine64 local persistence for Pari similarity indexes.
//!
//! Committed snapshots keep keys and per-key LSH band hashes as stable metadata
//! while bucket membership uses the canonical checksummed segments from
//! `pari-format`. Reopening a current snapshot loads key metadata plus a compact
//! bucket directory (about 4.6 bytes per bucket). Directory entries and
//! candidate memberships remain on disk until queried.
//!
//! Inserts and removals since the last successful commit live in an in-memory
//! overlay. Removed committed keys are explicitly suppressed so
//! remove-then-reinsert semantics remain correct. `flush` and `sync` append the
//! overlay to the file as a checksummed delta generation while generations stay
//! small relative to the snapshot, and otherwise compact committed state plus
//! the overlay into a fresh atomic snapshot. Phase-1 snapshots without bucket
//! segments remain readable and upgrade on next sync.
//! Width-specific public types reject snapshots from the other signature family.

use std::{
    collections::{BTreeMap, BTreeSet, HashMap, HashSet},
    error::Error,
    fmt,
    fs::{self, File, OpenOptions},
    io::{self, BufWriter, Seek, SeekFrom, Write},
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicU64, Ordering},
        Mutex,
    },
    time::{Instant, SystemTime},
};

use fs4::FileExt;
use pari_core::{MinHash32, MinHash64};
#[cfg(test)]
use pari_format::{
    bucket_record_size, encode_bucket_segment, BucketRecord, IndexFile, Section,
    BUCKET_SEGMENT_HEADER_BYTES,
};
use pari_format::{
    decode_signature_segment, encode_signature_segment, read_signature_row,
    read_signature_segment_values, signature_row_bytes, validate_signature_coverage,
    SignatureError, SignatureSegment, SIGNATURE_SEGMENT_HEADER_BYTES,
    SIGNATURE_SEGMENT_TARGET_BYTES,
};
use pari_format::{
    read_bucket_members, Algorithm, BucketDirectory, BucketError, BucketKey, CodecId, FileLayout,
    FormatError, IndexMetadata, LayoutError, NextDelta, SectionDescriptor, SectionKind,
    SignatureScheme, BUCKET_SEGMENT_TARGET_BYTES,
};
use pari_format::{ContainerWriteError, ContainerWriter};
use pari_index::{
    explain_lsh, explain_lsh64, signature_similarity, BucketDistribution, LshError, LshParams,
    LshPlan, LshPlanError, LshPlanOptions, QueryMetrics, ScoreFilterError, StorageMode,
};
pub use pari_index::{ScoreFilter, ScoredCandidate};
use same_file::Handle;

mod snapshot_buckets;

use snapshot_buckets::{write_bucket_sections, SORT_RECORD_LIMIT};

const FNV_OFFSET_BASIS: u64 = 0xCBF2_9CE4_8422_2325;
const FNV_PRIME: u64 = 0x0000_0100_0000_01B3;
const COUNT_BYTES: usize = 8;
const U64_BYTES: usize = 8;
const TEMPORARY_ALLOCATION_ATTEMPTS: u64 = 1_024;
/// Target payload size for one `Keys` or `BandHashes` section. Larger key
/// catalogs are written as several row-aligned `Keys`/`BandHashes` pairs so no
/// section approaches the container's 256 MiB per-section limit.
const METADATA_SECTION_TARGET_BYTES: usize = 64 * 1024 * 1024;
static NEXT_TEMPORARY_FILE: AtomicU64 = AtomicU64::new(0);

/// Errors returned by the persistent local index.
#[derive(Debug)]
pub enum StoreError {
    /// Filesystem operation failed.
    Io(io::Error),
    /// The versioned Pari container is malformed or unsupported.
    Format(FormatError),
    /// The lazy file layout reader rejected an I/O or range operation.
    Layout(LayoutError),
    /// The canonical bucket codec rejected persisted bucket data.
    Bucket(BucketError),
    /// In-memory LSH configuration validation failed.
    Index(LshError),
    /// Snapshot sections violate the local storage contract.
    InvalidSnapshot { reason: &'static str },
    /// A key already exists in the index or appears twice in one insertion batch.
    DuplicateKey { key: u64 },
    /// A supplied sketch was created with a different seed.
    IncompatibleSeed { expected: u64, actual: u64 },
    /// A supplied sketch has a different signature length.
    IncompatiblePermutationCount { expected: usize, actual: usize },
    /// Integer conversion or layout arithmetic overflowed.
    LengthOverflow,
    /// The requested index path has no usable file name.
    InvalidPath,
    /// The committed-file mutex was poisoned by a panicking caller.
    PoisonedFileLock,
    /// Persisted signature sections are malformed.
    Signature(SignatureError),
    /// Scored queries need an index created with stored signatures.
    SignaturesUnavailable,
    /// A scored-query filter is out of range.
    InvalidScoreFilter { reason: &'static str },
}

impl fmt::Display for StoreError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io(error) => write!(formatter, "local index I/O failed: {error}"),
            Self::Format(error) => write!(formatter, "invalid Pari index snapshot: {error}"),
            Self::Layout(error) => write!(formatter, "invalid Pari lazy index layout: {error}"),
            Self::Bucket(error) => error.fmt(formatter),
            Self::Index(error) => write!(formatter, "invalid LSH configuration: {error}"),
            Self::InvalidSnapshot { reason } => {
                write!(formatter, "invalid local snapshot: {reason}")
            }
            Self::DuplicateKey { key } => {
                write!(formatter, "key {key} already exists in the index")
            }
            Self::IncompatibleSeed { expected, actual } => write!(
                formatter,
                "incompatible MinHash seed: expected {expected}, got {actual}"
            ),
            Self::IncompatiblePermutationCount { expected, actual } => write!(
                formatter,
                "incompatible MinHash permutation count: expected {expected}, got {actual}"
            ),
            Self::LengthOverflow => {
                formatter.write_str("persistent index length arithmetic overflowed")
            }
            Self::InvalidPath => formatter.write_str("persistent index path must identify a file"),
            Self::PoisonedFileLock => formatter.write_str("committed index file lock is poisoned"),
            Self::Signature(error) => write!(formatter, "invalid stored signatures: {error}"),
            Self::SignaturesUnavailable => formatter.write_str(
                "this index does not store signatures; create it with stored signatures to get similarity scores",
            ),
            Self::InvalidScoreFilter { reason } => {
                write!(formatter, "invalid similarity filter: {reason}")
            }
        }
    }
}

impl Error for StoreError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Io(error) => Some(error),
            Self::Format(error) => Some(error),
            Self::Layout(error) => Some(error),
            Self::Bucket(error) => Some(error),
            Self::Index(error) => Some(error),
            Self::Signature(error) => Some(error),
            _ => None,
        }
    }
}

impl From<io::Error> for StoreError {
    fn from(error: io::Error) -> Self {
        Self::Io(error)
    }
}

impl From<FormatError> for StoreError {
    fn from(error: FormatError) -> Self {
        Self::Format(error)
    }
}

impl From<LayoutError> for StoreError {
    fn from(error: LayoutError) -> Self {
        Self::Layout(error)
    }
}

impl From<BucketError> for StoreError {
    fn from(error: BucketError) -> Self {
        Self::Bucket(error)
    }
}

impl From<SignatureError> for StoreError {
    fn from(error: SignatureError) -> Self {
        Self::Signature(error)
    }
}

impl From<ScoreFilterError> for StoreError {
    fn from(error: ScoreFilterError) -> Self {
        Self::InvalidScoreFilter {
            reason: error.reason(),
        }
    }
}

impl From<LshError> for StoreError {
    fn from(error: LshError) -> Self {
        Self::Index(error)
    }
}

/// Lightweight observable state for a local persistent index.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StoreStats {
    /// Number of live external keys.
    pub items: usize,
    /// Size of the index file on disk, including appended delta generations.
    pub file_bytes: u64,
    /// Whether memory contains changes that have not been fully committed.
    pub dirty: bool,
    /// Number of LSH bands.
    pub bands: usize,
    /// Number of signature rows per band.
    pub rows: usize,
    /// Number of committed bucket records indexed for direct disk lookup.
    pub committed_buckets: usize,
    /// Number of overlay bucket records holding uncommitted insertions.
    pub overlay_buckets: usize,
    /// Number of committed key memberships hidden until the next compaction:
    /// keys removed since they were committed, and keys a later generation
    /// stores again.
    pub suppressed_base_keys: usize,
    /// Number of delta generations appended since the last full snapshot.
    pub delta_generations: usize,
    /// Bytes of committed delta generations after the full snapshot.
    pub delta_bytes: u64,
    /// Exact distribution of stored committed bucket memberships. Suppressed
    /// generations remain counted until compaction.
    pub committed_distribution: BucketDistribution,
    /// Exact distribution of uncommitted overlay bucket memberships.
    pub overlay_distribution: BucketDistribution,
    /// Process-local query metrics when observability is enabled.
    pub queries: Option<QueryMetrics>,
    /// Whether full signatures are stored for similarity-scored queries.
    pub stores_signatures: bool,
}

/// Options for creating a persistent index.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct PersistentIndexOptions {
    params: Option<LshParams>,
    store_signatures: bool,
}

impl PersistentIndexOptions {
    /// Start from automatic LSH tuning without stored signatures.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            params: None,
            store_signatures: false,
        }
    }

    /// Use explicit LSH banding parameters instead of automatic tuning.
    #[must_use]
    pub const fn params(mut self, params: LshParams) -> Self {
        self.params = Some(params);
        self
    }

    /// Store each key's full signature so queries can return similarity scores.
    ///
    /// Each item then uses `num_perm * 4` (affine32) or `num_perm * 8`
    /// (affine64) additional bytes on disk. Signatures are read lazily during
    /// queries and are materialized only while a commit encodes a snapshot.
    #[must_use]
    pub const fn store_signatures(mut self, enabled: bool) -> Self {
        self.store_signatures = enabled;
        self
    }
}

/// Stored-signature state for an index created with signatures enabled.
///
/// Committed signatures stay on disk in each generation; see [`Generation`].
#[derive(Debug, Default)]
struct SignatureState {
    /// Signatures inserted since the last commit, widened to `u64`.
    overlay: HashMap<u64, Vec<u64>>,
}

/// One committed generation: the base snapshot (index 0) or an appended delta.
#[derive(Debug)]
struct Generation {
    directory: BucketDirectory,
    /// Number of keys whose memberships this generation stores.
    keys: usize,
    /// `(key, row)` pairs sorted by key; rows index `signature_segments`.
    signature_rows: Vec<(u64, u64)>,
    /// Signature segments ordered by first row; empty without signatures.
    signature_segments: Vec<SignatureSegment>,
}

impl Generation {
    fn signature_location(&self, key: u64) -> Option<(SignatureSegment, u64)> {
        let row = self
            .signature_rows
            .binary_search_by_key(&key, |(candidate, _)| *candidate)
            .ok()
            .map(|index| self.signature_rows[index].1)?;
        let index = self
            .signature_segments
            .partition_point(|segment| segment.first_row() <= row);
        let segment = *self.signature_segments.get(index.checked_sub(1)?)?;
        segment.contains_row(row).then_some((segment, row))
    }
}

/// The committed file: its base snapshot and appended delta generations.
#[derive(Debug)]
struct Committed {
    file: Mutex<File>,
    layout: FileLayout,
    generations: Vec<Generation>,
}

/// What one delta record stores, in file order.
struct DeltaContents {
    keys: Vec<u64>,
    rows: Vec<Vec<u64>>,
    tombstones: Vec<u64>,
}

/// Appended delta generations at most, before a commit compacts the file.
const MAX_DELTA_GENERATIONS: usize = 32;
/// Appended delta generations at most before a bulk-ingest commit
/// ([`PersistentIndex32::flush_delta`]) compacts the file.
const MAX_BULK_DELTA_GENERATIONS: usize = 1_024;
/// Deltas may grow to `1 / DELTA_BYTES_DIVISOR` of the base snapshot, and
/// committed generations may hide up to `1 / DEAD_KEYS_DIVISOR` as many dead
/// key memberships as there are live keys, before a commit compacts the file.
const DELTA_BYTES_DIVISOR: u64 = 4;
const DEAD_KEYS_DIVISOR: usize = 4;
/// Fixed bytes of a delta record beyond its per-key and per-tombstone data.
const DELTA_OVERHEAD_BYTES: u64 = 4_096;

/// How a commit chooses between appending a delta and rewriting the file.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CommitPolicy {
    /// Append while deltas stay small relative to the snapshot.
    Automatic,
    /// Append regardless of delta size, for bulk ingest that compacts later.
    Bulk,
}

#[derive(Debug)]
struct PersistentIndexCore {
    path: PathBuf,
    threshold: f64,
    num_perm: usize,
    seed: u64,
    params: LshParams,
    scheme: SignatureScheme,
    committed: Option<Committed>,
    overlay_buckets: Vec<OverlayTable>,
    /// Band hashes of live keys that no committed generation stores yet.
    ///
    /// Committed keys' band hashes stay on disk: a snapshot rewrite reads them
    /// back from their generations, so memory holds them only for the overlay.
    overlay_hashes: BTreeMap<u64, Vec<u64>>,
    /// Every live key, committed or not.
    live_keys: HashSet<u64>,
    /// Base-snapshot keys whose base memberships are not live: removed, or
    /// reinserted into a later generation or the overlay.
    suppressed_base_keys: HashSet<u64>,
    /// Committed delta generation (index into `Committed::generations`) of
    /// each live key that a delta stores. Other committed keys live in the base.
    homes: HashMap<u64, u32>,
    /// Committed keys removed since the last commit, written as tombstones.
    pending_tombstones: BTreeSet<u64>,
    /// Total keys stored by committed delta generations, live or not.
    delta_keys: usize,
    /// The next commit rewrites the whole snapshot instead of appending.
    force_rewrite: bool,
    dirty: bool,
    query_metrics: Option<Mutex<QueryMetrics>>,
    signatures: Option<SignatureState>,
    /// The committed file generation this handle last read or published.
    generation: Option<FileGeneration>,
}

/// Live keys' signatures in key order, materialized for a commit.
type LiveSignatures = Vec<(u64, Vec<u64>)>;

impl PersistentIndexCore {
    fn create_with_params(
        path: &Path,
        threshold: f64,
        num_perm: usize,
        seed: u64,
        params: LshParams,
        scheme: SignatureScheme,
        store_signatures: bool,
    ) -> Result<Self, StoreError> {
        Self::create_with_params_and_hook(
            path,
            threshold,
            num_perm,
            seed,
            params,
            scheme,
            store_signatures,
            || Ok(()),
        )
    }

    #[allow(clippy::too_many_arguments)] // Private constructor plumbing.
    fn create_with_params_and_hook(
        path: &Path,
        threshold: f64,
        num_perm: usize,
        seed: u64,
        params: LshParams,
        scheme: SignatureScheme,
        store_signatures: bool,
        before_publish: impl FnOnce() -> Result<(), StoreError>,
    ) -> Result<Self, StoreError> {
        let path = path.to_path_buf();
        ensure_destination_absent(&path)?;
        validate_lsh_configuration(scheme, threshold, num_perm, seed, params)?;
        let mut store = Self::empty(path, threshold, num_perm, seed, params, scheme);
        if store_signatures {
            store.signatures = Some(SignatureState::default());
        }
        store.commit_initial(before_publish)?;
        Ok(store)
    }

    fn open(path: &Path, scheme: SignatureScheme) -> Result<Self, StoreError> {
        let path = path.to_path_buf();
        let mut file = File::open(&path)?;
        let generation = FileGeneration::of(&file).ok();
        let layout = FileLayout::read_with_generations(&mut file)?;
        validate_store_metadata(layout.metadata(), scheme)?;
        let num_perm = usize::try_from(layout.metadata().num_perm())
            .map_err(|_| StoreError::LengthOverflow)?;
        let bands =
            usize::try_from(layout.metadata().bands()).map_err(|_| StoreError::LengthOverflow)?;
        let rows =
            usize::try_from(layout.metadata().rows()).map_err(|_| StoreError::LengthOverflow)?;
        let threshold = layout.metadata().threshold();
        let seed = layout.metadata().seed();
        let params = LshParams::new(bands, rows);
        validate_lsh_configuration(scheme, threshold, num_perm, seed, params)?;

        let bucket_descriptors = collect_bucket_descriptors(layout.sections(), false)?;
        if bucket_descriptors.is_empty() {
            // A phase-1 snapshot has no bucket sections: every key moves to
            // the overlay with its band hashes, and the next commit upgrades it.
            let (file_keys, key_hashes) = read_key_metadata(&layout, &mut file, bands)?;
            let signature_segments =
                read_signature_segments(&layout, layout.sections(), &mut file, file_keys.len())?;
            if !layout.deltas().is_empty() {
                return Err(StoreError::InvalidSnapshot {
                    reason: "appended generations require a snapshot with bucket sections",
                });
            }
            let signatures = match signature_segments {
                Some(segments) => Some(SignatureState {
                    overlay: read_all_signatures(&layout, &mut file, &segments, &file_keys)?,
                }),
                None => None,
            };
            let mut store = Self::open_legacy(
                path, threshold, num_perm, seed, params, scheme, key_hashes, signatures,
            )?;
            store.generation = generation;
            return Ok(store);
        }
        let (file_keys, live_keys) =
            read_key_catalog(&layout, layout.sections(), &mut file, bands)?;
        let signature_segments =
            read_signature_segments(&layout, layout.sections(), &mut file, file_keys.len())?;
        let stores_signatures = signature_segments.is_some();
        let base = Generation {
            directory: BucketDirectory::read(&layout, &mut file, &bucket_descriptors, bands)?,
            keys: file_keys.len(),
            signature_rows: if stores_signatures {
                sorted_rows(&file_keys)
            } else {
                Vec::new()
            },
            signature_segments: signature_segments.unwrap_or_default(),
        };

        let mut store = Self {
            path,
            threshold,
            num_perm,
            seed,
            params,
            scheme,
            committed: Some(Committed {
                file: Mutex::new(file),
                layout,
                generations: vec![base],
            }),
            overlay_buckets: Vec::new(),
            overlay_hashes: BTreeMap::new(),
            live_keys,
            suppressed_base_keys: HashSet::new(),
            homes: HashMap::new(),
            pending_tombstones: BTreeSet::new(),
            delta_keys: 0,
            force_rewrite: false,
            dirty: false,
            query_metrics: None,
            signatures: stores_signatures.then(SignatureState::default),
            generation,
        };
        store.replay_deltas()?;
        Ok(store)
    }

    /// Load and apply every committed delta the layout lists, in order.
    fn replay_deltas(&mut self) -> Result<(), StoreError> {
        let Some(committed) = &mut self.committed else {
            return Ok(());
        };
        let file = committed
            .file
            .get_mut()
            .map_err(|_| StoreError::PoisonedFileLock)?;
        let stores_signatures = self.signatures.is_some();
        for index in committed.generations.len()..=committed.layout.deltas().len() {
            let sections = committed.layout.deltas()[index - 1].sections().to_vec();
            let (generation, contents) = read_delta_generation(
                &committed.layout,
                file,
                &sections,
                self.params.bands,
                stores_signatures,
            )?;
            let home = u32::try_from(index).map_err(|_| StoreError::LengthOverflow)?;
            for key in contents.tombstones {
                if !self.live_keys.remove(&key) {
                    return Err(StoreError::InvalidSnapshot {
                        reason: "delta removes a key that is not live",
                    });
                }
                if self.homes.remove(&key).is_none() {
                    self.suppressed_base_keys.insert(key);
                }
            }
            for key in contents.keys {
                if !self.live_keys.insert(key) {
                    return Err(StoreError::InvalidSnapshot {
                        reason: "delta inserts a key that is already live",
                    });
                }
                self.homes.insert(key, home);
            }
            self.delta_keys = self
                .delta_keys
                .checked_add(generation.keys)
                .ok_or(StoreError::LengthOverflow)?;
            committed.generations.push(generation);
        }
        Ok(())
    }

    /// Return whether `key`'s live memberships are stored by committed
    /// generation `index`.
    fn is_live_in(&self, index: usize, key: u64) -> bool {
        if index == 0 {
            self.live_keys.contains(&key) && !self.suppressed_base_keys.contains(&key)
        } else {
            self.homes
                .get(&key)
                .is_some_and(|home| usize::try_from(*home).is_ok_and(|home| home == index))
        }
    }

    /// Return the committed generation that stores `key`'s live memberships.
    fn home_of(&self, key: u64) -> Option<usize> {
        match self.homes.get(&key) {
            Some(home) => usize::try_from(*home).ok(),
            None => self.is_live_in(0, key).then_some(0),
        }
    }

    fn insert_hashes_many(&mut self, prepared: Vec<PreparedItem>) -> Result<(), StoreError> {
        let mut batch_keys = HashSet::new();
        for item in &prepared {
            if self.live_keys.contains(&item.key) || !batch_keys.insert(item.key) {
                return Err(StoreError::DuplicateKey { key: item.key });
            }
        }

        for item in prepared {
            if let Some(state) = &mut self.signatures {
                state.overlay.insert(item.key, item.signature);
            }
            self.insert_overlay(item.key, item.hashes);
        }
        if !batch_keys.is_empty() {
            self.dirty = true;
        }
        Ok(())
    }

    fn remove(&mut self, key: u64) -> bool {
        if !self.live_keys.remove(&key) {
            return false;
        }
        let overlay_hashes = self.overlay_hashes.remove(&key);
        let was_overlay = overlay_hashes.is_some();
        if let Some(hashes) = overlay_hashes {
            remove_overlay_key(&mut self.overlay_buckets, key, &hashes);
        }
        if let Some(state) = &mut self.signatures {
            state.overlay.remove(&key);
        }
        if self.committed.is_some() && !was_overlay {
            // A committed key: hide its committed memberships and record the
            // removal for the next commit.
            if self.homes.remove(&key).is_none() {
                self.suppressed_base_keys.insert(key);
            }
            self.pending_tombstones.insert(key);
        }
        self.dirty = true;
        true
    }

    fn query_hashes(
        &self,
        hashes: &[u64],
        started: Option<Instant>,
    ) -> Result<Vec<u64>, StoreError> {
        let mut candidates = HashSet::new();
        self.collect_candidates(hashes, &mut candidates)?;
        let mut keys: Vec<_> = candidates.into_iter().collect();
        keys.sort_unstable();
        self.record_query_metrics(1, keys.len(), self.len(), started);
        Ok(keys)
    }

    fn query_hashes_many(
        &self,
        hashes_by_query: impl IntoIterator<Item = Vec<u64>>,
        started: Option<Instant>,
    ) -> Result<Vec<Vec<u64>>, StoreError> {
        let mut output = Vec::new();
        let mut candidates = HashSet::new();
        let mut candidate_count = 0_usize;
        for hashes in hashes_by_query {
            candidates.clear();
            self.collect_candidates(&hashes, &mut candidates)?;
            let mut keys: Vec<_> = candidates.iter().copied().collect();
            keys.sort_unstable();
            candidate_count = candidate_count.saturating_add(keys.len());
            output.push(keys);
        }
        self.record_query_metrics(
            output.len(),
            candidate_count,
            output.len().saturating_mul(self.len()),
            started,
        );
        Ok(output)
    }

    fn query_scored(
        &self,
        hashes: &[u64],
        signature: &[u64],
        filter: ScoreFilter,
        started: Option<Instant>,
    ) -> Result<Vec<ScoredCandidate>, StoreError> {
        let scored = self.score_candidates(hashes, signature, filter)?;
        self.record_query_metrics(1, scored.len(), self.len(), started);
        Ok(scored)
    }

    fn query_scored_many(
        &self,
        queries: Vec<(Vec<u64>, Vec<u64>)>,
        filter: ScoreFilter,
        started: Option<Instant>,
    ) -> Result<Vec<Vec<ScoredCandidate>>, StoreError> {
        let mut output = Vec::with_capacity(queries.len());
        let mut candidate_count = 0_usize;
        for (hashes, signature) in queries {
            let scored = self.score_candidates(&hashes, &signature, filter)?;
            candidate_count = candidate_count.saturating_add(scored.len());
            output.push(scored);
        }
        self.record_query_metrics(
            output.len(),
            candidate_count,
            output.len().saturating_mul(self.len()),
            started,
        );
        Ok(output)
    }

    fn score_candidates(
        &self,
        hashes: &[u64],
        signature: &[u64],
        filter: ScoreFilter,
    ) -> Result<Vec<ScoredCandidate>, StoreError> {
        filter.validate()?;
        if self.signatures.is_none() {
            return Err(StoreError::SignaturesUnavailable);
        }
        let mut candidates = HashSet::new();
        self.collect_candidates(hashes, &mut candidates)?;
        let mut keys: Vec<_> = candidates.into_iter().collect();
        keys.sort_unstable();
        let signatures = self.stored_signatures(&keys)?;
        let scored = keys
            .into_iter()
            .zip(signatures)
            .map(|(key, values)| ScoredCandidate {
                key,
                similarity: signature_similarity(signature, &values),
            })
            .collect();
        Ok(filter.apply(scored))
    }

    /// Return the stored signatures of live `keys`, in order.
    fn stored_signatures(&self, keys: &[u64]) -> Result<Vec<Vec<u64>>, StoreError> {
        let state = self
            .signatures
            .as_ref()
            .ok_or(StoreError::SignaturesUnavailable)?;
        let mut output = Vec::with_capacity(keys.len());
        let mut file = None;
        for key in keys {
            if let Some(values) = state.overlay.get(key) {
                output.push(values.clone());
                continue;
            }
            let location = self.committed.as_ref().and_then(|committed| {
                let generation = committed.generations.get(self.home_of(*key)?)?;
                Some((committed, generation.signature_location(*key)?))
            });
            let Some((committed, (segment, row))) = location else {
                return Err(StoreError::InvalidSnapshot {
                    reason: "live key has no stored signature",
                });
            };
            if file.is_none() {
                file = Some(
                    committed
                        .file
                        .lock()
                        .map_err(|_| StoreError::PoisonedFileLock)?,
                );
            }
            let handle = file.as_mut().ok_or(StoreError::PoisonedFileLock)?;
            output.push(read_signature_row(
                &committed.layout,
                &mut **handle,
                segment,
                row,
            )?);
        }
        Ok(output)
    }

    /// Return one live key's stored signature, if signatures are enabled.
    fn stored_signature(&self, key: u64) -> Result<Option<Vec<u64>>, StoreError> {
        if self.signatures.is_none() || !self.live_keys.contains(&key) {
            return Ok(None);
        }
        Ok(self.stored_signatures(&[key])?.pop())
    }

    /// Materialize every live signature in key order for a snapshot commit.
    fn collect_live_signatures(&self) -> Result<Option<LiveSignatures>, StoreError> {
        let Some(state) = &self.signatures else {
            return Ok(None);
        };
        let mut live = Vec::with_capacity(self.live_keys.len());
        if let Some(committed) = &self.committed {
            let mut file = committed
                .file
                .lock()
                .map_err(|_| StoreError::PoisonedFileLock)?;
            for (index, generation) in committed.generations.iter().enumerate() {
                let mut row_keys = vec![0_u64; generation.signature_rows.len()];
                for (key, row) in &generation.signature_rows {
                    let row = usize::try_from(*row).map_err(|_| StoreError::LengthOverflow)?;
                    *row_keys.get_mut(row).ok_or(StoreError::InvalidSnapshot {
                        reason: "stored signature row is out of range",
                    })? = *key;
                }
                for segment in &generation.signature_segments {
                    let values =
                        read_signature_segment_values(&committed.layout, &mut *file, *segment)?;
                    let first = usize::try_from(segment.first_row())
                        .map_err(|_| StoreError::LengthOverflow)?;
                    for (offset, row) in values.chunks_exact(self.num_perm).enumerate() {
                        let key =
                            *row_keys
                                .get(first + offset)
                                .ok_or(StoreError::InvalidSnapshot {
                                    reason: "stored signature row is out of range",
                                })?;
                        if self.is_live_in(index, key) && !state.overlay.contains_key(&key) {
                            live.push((key, row.to_vec()));
                        }
                    }
                }
            }
        }
        live.extend(
            state
                .overlay
                .iter()
                .filter(|(key, _)| self.live_keys.contains(key))
                .map(|(key, values)| (*key, values.clone())),
        );
        live.sort_unstable_by_key(|(key, _)| *key);
        if live.len() != self.live_keys.len() {
            return Err(StoreError::InvalidSnapshot {
                reason: "stored signatures do not cover every live key",
            });
        }
        Ok(Some(live))
    }

    /// Keep signatures in memory after the committed base becomes unreadable.
    fn restore_signatures(&mut self, live: Option<LiveSignatures>) {
        if let (Some(state), Some(live)) = (&mut self.signatures, live) {
            state.overlay = live.into_iter().collect();
        }
    }

    fn len(&self) -> usize {
        self.live_keys.len()
    }

    /// Materialize every live key's band hashes in key order for a snapshot
    /// rewrite, reading committed keys' rows back from their generations.
    fn collect_live_hashes(&self) -> Result<BTreeMap<u64, Vec<u64>>, StoreError> {
        let mut live = BTreeMap::new();
        if let Some(committed) = &self.committed {
            let mut file = committed
                .file
                .lock()
                .map_err(|_| StoreError::PoisonedFileLock)?;
            for index in 0..committed.generations.len() {
                let sections = if index == 0 {
                    committed.layout.sections()
                } else {
                    committed
                        .layout
                        .deltas()
                        .get(index - 1)
                        .ok_or(StoreError::InvalidSnapshot {
                            reason: "committed generation is missing from the layout",
                        })?
                        .sections()
                };
                for_each_key_row(
                    &committed.layout,
                    sections,
                    &mut file,
                    self.params.bands,
                    |key, hashes| {
                        if self.is_live_in(index, key) && !self.overlay_hashes.contains_key(&key) {
                            live.insert(key, hashes);
                        }
                        Ok(())
                    },
                )?;
            }
        }
        live.extend(
            self.overlay_hashes
                .iter()
                .map(|(key, hashes)| (*key, hashes.clone())),
        );
        if live.len() != self.live_keys.len() {
            return Err(StoreError::InvalidSnapshot {
                reason: "stored band hashes do not cover every live key",
            });
        }
        Ok(live)
    }

    fn explain(&self) -> Result<LshPlan, LshPlanError> {
        let options = LshPlanOptions::new(
            u64::try_from(self.len()).unwrap_or(u64::MAX),
            self.threshold,
            self.num_perm,
        )
        .storage_mode(StorageMode::Persistent);
        match self.scheme {
            SignatureScheme::PariAffine32V1 => explain_lsh(options, self.params),
            SignatureScheme::PariAffine64V1 => explain_lsh64(options, self.params),
        }
    }

    fn set_observability(&mut self, enabled: bool) {
        self.query_metrics = enabled.then(|| Mutex::new(QueryMetrics::default()));
    }

    fn flush(&mut self) -> Result<(), StoreError> {
        self.commit(false, CommitPolicy::Automatic)
    }

    fn flush_delta(&mut self) -> Result<(), StoreError> {
        self.commit(false, CommitPolicy::Bulk)
    }

    fn sync(&mut self) -> Result<(), StoreError> {
        self.commit(true, CommitPolicy::Automatic)
    }

    fn close(mut self) -> Result<(), StoreError> {
        self.sync()
    }

    fn stats(&self) -> Result<StoreStats, StoreError> {
        let file_bytes = match fs::metadata(&self.path) {
            Ok(metadata) => metadata.len(),
            Err(error) if error.kind() == io::ErrorKind::NotFound => 0,
            Err(error) => return Err(StoreError::Io(error)),
        };
        let generations = self
            .committed
            .as_ref()
            .map_or(&[][..], |committed| committed.generations.as_slice());
        let committed_buckets = generations.iter().fold(0_usize, |total, generation| {
            total.saturating_add(usize::try_from(generation.directory.len()).unwrap_or(usize::MAX))
        });
        let overlay_buckets = self
            .overlay_buckets
            .iter()
            .map(OverlayTable::len)
            .sum::<usize>();
        let committed_distribution =
            BucketDistribution::from_size_counts(generations.iter().flat_map(|generation| {
                generation
                    .directory
                    .size_counts()
                    .map(|(size, count)| (u64::from(size), count))
            }));
        let (delta_generations, delta_bytes) =
            self.committed.as_ref().map_or((0, 0), |committed| {
                (
                    committed.generations.len().saturating_sub(1),
                    committed.layout.file_length() - committed.layout.base_length(),
                )
            });
        let overlay_distribution = BucketDistribution::from_sizes(
            self.overlay_buckets
                .iter()
                .flat_map(OverlayTable::bucket_sizes),
        );
        Ok(StoreStats {
            items: self.len(),
            file_bytes,
            dirty: self.dirty,
            bands: self.params.bands,
            rows: self.params.rows,
            committed_buckets,
            overlay_buckets,
            suppressed_base_keys: self.dead_committed_keys(),
            delta_generations,
            delta_bytes,
            committed_distribution,
            overlay_distribution,
            queries: self
                .query_metrics
                .as_ref()
                .and_then(|metrics| metrics.lock().ok().map(|metrics| *metrics)),
            stores_signatures: self.signatures.is_some(),
        })
    }

    fn empty(
        path: PathBuf,
        threshold: f64,
        num_perm: usize,
        seed: u64,
        params: LshParams,
        scheme: SignatureScheme,
    ) -> Self {
        Self {
            path,
            threshold,
            num_perm,
            seed,
            params,
            scheme,
            committed: None,
            overlay_buckets: Vec::new(),
            overlay_hashes: BTreeMap::new(),
            live_keys: HashSet::new(),
            suppressed_base_keys: HashSet::new(),
            homes: HashMap::new(),
            pending_tombstones: BTreeSet::new(),
            delta_keys: 0,
            force_rewrite: false,
            dirty: true,
            query_metrics: None,
            signatures: None,
            generation: None,
        }
    }

    fn record_query_metrics(
        &self,
        queries: usize,
        candidates: usize,
        possible_candidates: usize,
        started: Option<Instant>,
    ) {
        let (Some(metrics), Some(started)) = (&self.query_metrics, started) else {
            return;
        };
        if let Ok(mut metrics) = metrics.lock() {
            metrics.record(queries, candidates, possible_candidates, started.elapsed());
        }
    }

    #[allow(clippy::too_many_arguments)] // Private constructor plumbing.
    fn open_legacy(
        path: PathBuf,
        threshold: f64,
        num_perm: usize,
        seed: u64,
        params: LshParams,
        scheme: SignatureScheme,
        key_hashes: BTreeMap<u64, Vec<u64>>,
        signatures: Option<SignatureState>,
    ) -> Result<Self, StoreError> {
        let mut store = Self::empty(path, threshold, num_perm, seed, params, scheme);
        store.signatures = signatures;
        store.rebuild_overlay_all(key_hashes)?;
        store.dirty = true;
        Ok(store)
    }

    fn insert_overlay(&mut self, key: u64, hashes: Vec<u64>) {
        debug_assert_eq!(hashes.len(), self.params.bands);
        if self.overlay_buckets.is_empty() {
            self.overlay_buckets = empty_bucket_tables(self.params.bands);
        }
        add_overlay_key(&mut self.overlay_buckets, key, &hashes);
        self.live_keys.insert(key);
        self.overlay_hashes.insert(key, hashes);
    }

    fn collect_candidates(
        &self,
        hashes: &[u64],
        output: &mut HashSet<u64>,
    ) -> Result<(), StoreError> {
        if let Some(committed) = &self.committed {
            let mut file = committed
                .file
                .lock()
                .map_err(|_| StoreError::PoisonedFileLock)?;
            for (band, hash) in hashes.iter().copied().enumerate() {
                let key = BucketKey::new(
                    u32::try_from(band).map_err(|_| StoreError::LengthOverflow)?,
                    hash,
                );
                for (index, generation) in committed.generations.iter().enumerate() {
                    let Some(location) =
                        generation
                            .directory
                            .find(&committed.layout, &mut *file, key)?
                    else {
                        continue;
                    };
                    for member in read_bucket_members(&committed.layout, &mut *file, location)? {
                        if self.is_live_in(index, member) {
                            output.insert(member);
                        }
                    }
                }
            }
        }
        collect_overlay_candidates(&self.overlay_buckets, hashes, output);
        output.retain(|key| self.live_keys.contains(key));
        Ok(())
    }

    /// Move every live key into the overlay, given all live band hashes, and
    /// forget the committed file.
    fn rebuild_overlay_all(
        &mut self,
        key_hashes: BTreeMap<u64, Vec<u64>>,
    ) -> Result<(), StoreError> {
        let mut overlay = if key_hashes.is_empty() {
            Vec::new()
        } else {
            empty_bucket_tables(self.params.bands)
        };
        let mut live_keys = HashSet::with_capacity(key_hashes.len());
        for (key, hashes) in &key_hashes {
            if hashes.len() != self.params.bands {
                return Err(StoreError::InvalidSnapshot {
                    reason: "in-memory band-hash row has the wrong width",
                });
            }
            add_overlay_key(&mut overlay, *key, hashes);
            live_keys.insert(*key);
        }
        self.overlay_buckets = overlay;
        self.overlay_hashes = key_hashes;
        self.live_keys = live_keys;
        self.suppressed_base_keys.clear();
        self.homes.clear();
        self.pending_tombstones.clear();
        self.delta_keys = 0;
        self.committed = None;
        Ok(())
    }

    fn commit(&mut self, sync_parent: bool, policy: CommitPolicy) -> Result<(), StoreError> {
        if !self.dirty {
            if sync_parent {
                sync_parent_directory(&self.path)?;
            }
            return Ok(());
        }

        // Fail fast before waiting, then hold the commit lock so no other
        // handle can publish between the check and this handle's rename.
        self.ensure_unchanged_on_disk()?;
        let _lock = CommitLock::acquire(&self.path)?;
        self.ensure_unchanged_on_disk()?;

        if self.committed.is_some()
            && !self.force_rewrite
            && self.overlay_hashes.is_empty()
            && self.pending_tombstones.is_empty()
        {
            // Every change since the last commit cancelled out.
            self.dirty = false;
            if sync_parent {
                sync_parent_directory(&self.path)?;
            }
            return Ok(());
        }
        if self.should_append(policy) {
            self.append_delta()?;
            if sync_parent {
                sync_parent_directory(&self.path)?;
            }
            return Ok(());
        }

        // Band hashes and signatures are materialized once: they are encoded
        // into the snapshot and kept to recover in memory if publication fails
        // after the committed file has been closed.
        let key_hashes = self.collect_live_hashes()?;
        let signatures = self.collect_live_signatures()?;
        let temporary = self.stage_snapshot(&key_hashes, signatures.as_deref())?;

        // Close the committed file before replacing it. This is required on
        // Windows and avoids retaining a handle to the old generation.
        self.committed = None;
        if let Err(error) = temporary.publish_replace(&self.path) {
            self.rebuild_overlay_all(key_hashes)?;
            self.restore_signatures(signatures);
            return Err(error);
        }
        // The lock is still held, so the published file is this handle's own.
        self.generation = FileGeneration::at(&self.path).ok();

        let parent_error = if sync_parent {
            sync_parent_directory(&self.path).err().map(StoreError::Io)
        } else {
            None
        };
        self.refresh_after_commit(parent_error, key_hashes, signatures)
    }

    /// Rewrite the committed file as one snapshot without delta generations.
    fn compact(&mut self) -> Result<(), StoreError> {
        let appended = self.committed.as_ref().is_some_and(|committed| {
            committed.generations.len() > 1 || committed.layout.uncommitted_bytes() > 0
        });
        if appended || self.dead_committed_keys() > 0 || self.dirty {
            self.force_rewrite = true;
            self.dirty = true;
        }
        self.commit(true, CommitPolicy::Automatic)
    }

    /// Committed key memberships that queries skip: removed keys and keys a
    /// later generation stores again.
    fn dead_committed_keys(&self) -> usize {
        self.suppressed_base_keys
            .len()
            .saturating_add(self.delta_keys.saturating_sub(self.homes.len()))
    }

    /// Return whether this commit should append a delta generation instead of
    /// rewriting the snapshot.
    ///
    /// Deltas are appended while they stay small relative to the base and
    /// few dead memberships accumulate; otherwise the commit compacts. A bulk
    /// commit ignores delta size and allows many more generations.
    fn should_append(&self, policy: CommitPolicy) -> bool {
        let Some(committed) = &self.committed else {
            return false;
        };
        let max_generations = match policy {
            CommitPolicy::Automatic => MAX_DELTA_GENERATIONS,
            CommitPolicy::Bulk => MAX_BULK_DELTA_GENERATIONS,
        };
        if self.force_rewrite || committed.generations.len() > max_generations {
            return false;
        }
        let few_dead = self.dead_committed_keys() <= self.live_keys.len() / DEAD_KEYS_DIVISOR;
        if policy == CommitPolicy::Bulk {
            return few_dead;
        }
        let layout = &committed.layout;
        let appended = layout.file_length() - layout.base_length();
        let Some(total) = appended.checked_add(self.projected_delta_bytes()) else {
            return false;
        };
        total <= layout.base_length() / DELTA_BYTES_DIVISOR && few_dead
    }

    /// Upper estimate of the delta record the pending changes would write.
    fn projected_delta_bytes(&self) -> u64 {
        let bands = u64::try_from(self.params.bands).unwrap_or(u64::MAX);
        let signature = if self.signatures.is_some() {
            let width = match self.scheme {
                SignatureScheme::PariAffine32V1 => 4,
                SignatureScheme::PariAffine64V1 => 8,
            };
            u64::try_from(self.num_perm)
                .unwrap_or(u64::MAX)
                .saturating_mul(width)
        } else {
            0
        };
        // Key, band hashes, then one directory entry and member per band.
        let per_key = 8_u64
            .saturating_add(bands.saturating_mul(8 + 32 + 8))
            .saturating_add(signature);
        let keys = u64::try_from(self.overlay_hashes.len()).unwrap_or(u64::MAX);
        let tombstones = u64::try_from(self.pending_tombstones.len()).unwrap_or(u64::MAX);
        keys.saturating_mul(per_key)
            .saturating_add(tombstones.saturating_mul(8))
            .saturating_add(DELTA_OVERHEAD_BYTES)
    }

    /// Append the changes since the last commit as a new delta generation.
    ///
    /// Must run under the commit lock. If the write fails before the
    /// generation commits, the changes stay pending in memory. If the outcome
    /// is uncertain, the next commit rewrites the snapshot.
    fn append_delta(&mut self) -> Result<(), StoreError> {
        // Copied, not taken: the changes stay pending if the append fails.
        let delta_keys = self.overlay_hashes.clone();
        let tombstones: Vec<u64> = self.pending_tombstones.iter().copied().collect();
        let signatures = match &self.signatures {
            Some(state) => Some(
                delta_keys
                    .keys()
                    .map(|key| {
                        state
                            .overlay
                            .get(key)
                            .map(|values| (*key, values.clone()))
                            .ok_or(StoreError::InvalidSnapshot {
                                reason: "uncommitted key has no stored signature",
                            })
                    })
                    .collect::<Result<Vec<_>, _>>()?,
            ),
            None => None,
        };
        let committed = self.committed.as_mut().ok_or(StoreError::InvalidSnapshot {
            reason: "no committed snapshot to append to",
        })?;
        let written = write_delta_record(
            &self.path,
            committed.layout.next_delta(),
            &DeltaRecord {
                keys: &delta_keys,
                tombstones: &tombstones,
                signatures: signatures.as_deref(),
                bands: self.params.bands,
                scheme: self.scheme,
                num_perm: self.num_perm,
            },
        );

        // Still under the commit lock: learn what is committed now, whatever
        // the write reported.
        let found = {
            let mut file = committed
                .file
                .lock()
                .map_err(|_| StoreError::PoisonedFileLock)?;
            committed.layout.refresh_generations(&mut *file)
        };
        self.generation = FileGeneration::at(&self.path).ok();
        let found = match found {
            Ok(found) => found,
            Err(error) => {
                self.force_rewrite = true;
                return Err(written.err().unwrap_or_else(|| error.into()));
            }
        };
        if found == 0 {
            // Nothing was committed, so the changes stay pending.
            return Err(written.err().unwrap_or(StoreError::InvalidSnapshot {
                reason: "appended generation was not committed",
            }));
        }
        if found != 1 {
            self.force_rewrite = true;
            return Err(StoreError::InvalidSnapshot {
                reason: "another writer appended generations during this commit",
            });
        }
        if let Err(error) = self.adopt_delta(&delta_keys, &tombstones) {
            self.force_rewrite = true;
            return Err(error);
        }
        if let Err(error) = written {
            // The generation is visible, but its durability is unconfirmed.
            self.force_rewrite = true;
            self.dirty = true;
            return Err(error);
        }
        Ok(())
    }

    /// Make the generation this handle just appended part of its committed
    /// state, after checking it holds exactly the pending changes.
    fn adopt_delta(
        &mut self,
        delta_keys: &BTreeMap<u64, Vec<u64>>,
        tombstones: &[u64],
    ) -> Result<(), StoreError> {
        let stores_signatures = self.signatures.is_some();
        let committed = self.committed.as_mut().ok_or(StoreError::InvalidSnapshot {
            reason: "no committed snapshot to append to",
        })?;
        let index = committed.generations.len();
        let sections = committed
            .layout
            .deltas()
            .get(index - 1)
            .ok_or(StoreError::InvalidSnapshot {
                reason: "appended generation is missing from the layout",
            })?
            .sections()
            .to_vec();
        let file = committed
            .file
            .get_mut()
            .map_err(|_| StoreError::PoisonedFileLock)?;
        let (generation, contents) = read_delta_generation(
            &committed.layout,
            file,
            &sections,
            self.params.bands,
            stores_signatures,
        )?;
        let matches = contents.tombstones == tombstones
            && contents.keys.len() == delta_keys.len()
            && contents
                .keys
                .iter()
                .zip(&contents.rows)
                .zip(delta_keys)
                .all(|((key, row), (expected, hashes))| key == expected && row == hashes);
        if !matches {
            return Err(StoreError::InvalidSnapshot {
                reason: "appended generation differs from the committed changes",
            });
        }
        let home = u32::try_from(index).map_err(|_| StoreError::LengthOverflow)?;
        for key in contents.keys {
            self.homes.insert(key, home);
        }
        self.delta_keys = self
            .delta_keys
            .checked_add(generation.keys)
            .ok_or(StoreError::LengthOverflow)?;
        committed.generations.push(generation);
        self.overlay_buckets = Vec::new();
        self.overlay_hashes = BTreeMap::new();
        self.pending_tombstones.clear();
        if let Some(state) = &mut self.signatures {
            state.overlay = HashMap::new();
        }
        self.dirty = false;
        Ok(())
    }

    /// Refuse to publish over a snapshot another handle committed after this
    /// handle read its own, which would silently discard that commit.
    fn ensure_unchanged_on_disk(&self) -> Result<(), StoreError> {
        let Some(expected) = self.generation else {
            return Ok(());
        };
        let current = match FileGeneration::at(&self.path) {
            Ok(current) => current,
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                return Err(concurrent_commit_error(&self.path));
            }
            Err(error) => return Err(StoreError::Io(error)),
        };
        let mut unchanged = current == expected;
        if unchanged {
            if let Some(committed) = &self.committed {
                // Exact file identity where a committed handle is open.
                let file = committed
                    .file
                    .lock()
                    .map_err(|_| StoreError::PoisonedFileLock)?;
                unchanged = Handle::from_file(file.try_clone()?)? == Handle::from_path(&self.path)?;
            }
        }
        if unchanged {
            Ok(())
        } else {
            Err(concurrent_commit_error(&self.path))
        }
    }

    fn commit_initial(
        &mut self,
        before_publish: impl FnOnce() -> Result<(), StoreError>,
    ) -> Result<(), StoreError> {
        let key_hashes = self.collect_live_hashes()?;
        let signatures = self.collect_live_signatures()?;
        let temporary = self.stage_snapshot(&key_hashes, signatures.as_deref())?;
        before_publish()?;

        self.committed = None;
        if let Err(error) = temporary.publish_no_replace(&self.path) {
            self.rebuild_overlay_all(key_hashes)?;
            self.restore_signatures(signatures);
            return Err(error);
        }
        self.refresh_after_commit(None, key_hashes, signatures)
    }

    fn stage_snapshot(
        &self,
        key_hashes: &BTreeMap<u64, Vec<u64>>,
        signatures: Option<&[(u64, Vec<u64>)]>,
    ) -> Result<OwnedTemporaryFile, StoreError> {
        create_parent_if_needed(&self.path)?;
        let mut temporary = OwnedTemporaryFile::allocate(&self.path)?;
        if let Err(error) =
            temporary.write_streamed(|output| self.write_snapshot(output, key_hashes, signatures))
        {
            return Err(temporary.cleanup_after_store(error));
        }
        Ok(temporary)
    }

    fn refresh_after_commit(
        &mut self,
        parent_error: Option<StoreError>,
        key_hashes: BTreeMap<u64, Vec<u64>>,
        signatures: Option<LiveSignatures>,
    ) -> Result<(), StoreError> {
        // The file was published. Reopening can release the old overlay now:
        // the collected band hashes and signatures suffice to rebuild it on error.
        self.overlay_buckets = Vec::new();
        self.overlay_hashes = BTreeMap::new();
        self.live_keys = HashSet::new();
        self.suppressed_base_keys = HashSet::new();
        if let Some(state) = &mut self.signatures {
            *state = SignatureState::default();
        }
        match Self::open(&self.path, self.scheme) {
            Ok(mut refreshed) => {
                if parent_error.is_some() {
                    // Durability is unconfirmed: the next commit rewrites.
                    refreshed.dirty = true;
                    refreshed.force_rewrite = true;
                }
                // Observation belongs to this handle, not the file generation.
                refreshed.query_metrics = self.query_metrics.take();
                *self = refreshed;
            }
            Err(error) => {
                self.rebuild_overlay_all(key_hashes)?;
                self.restore_signatures(signatures);
                self.dirty = true;
                return Err(error);
            }
        }
        if let Some(error) = parent_error {
            return Err(error);
        }
        Ok(())
    }

    fn snapshot_metadata(&self) -> Result<IndexMetadata, StoreError> {
        Ok(IndexMetadata::new(
            Algorithm::MinHashLsh,
            self.scheme,
            CodecId::U64,
            u32::try_from(self.num_perm).map_err(|_| StoreError::LengthOverflow)?,
            self.seed,
            self.threshold,
            u32::try_from(self.params.bands).map_err(|_| StoreError::LengthOverflow)?,
            u32::try_from(self.params.rows).map_err(|_| StoreError::LengthOverflow)?,
            0,
        )?)
    }

    /// Write each metadata and bucket section directly to the staging sink.
    fn write_snapshot<W: Write + Seek>(
        &self,
        output: W,
        key_hashes: &BTreeMap<u64, Vec<u64>>,
        signatures: Option<&[(u64, Vec<u64>)]>,
    ) -> Result<(), StoreError> {
        self.write_snapshot_with(
            output,
            key_hashes,
            signatures,
            BUCKET_SEGMENT_TARGET_BYTES,
            SORT_RECORD_LIMIT,
        )
    }

    fn write_snapshot_with<W: Write + Seek>(
        &self,
        output: W,
        key_hashes: &BTreeMap<u64, Vec<u64>>,
        signatures: Option<&[(u64, Vec<u64>)]>,
        bucket_target_bytes: usize,
        sort_limit: usize,
    ) -> Result<(), StoreError> {
        let metadata = self.snapshot_metadata()?;
        ContainerWriter::write_seekable(output, &metadata, |writer| {
            let mut emit = |kind, required, payload: Vec<u8>| {
                writer.write_section(kind, required, &payload)?;
                Ok(())
            };
            emit_key_metadata_sections(
                key_hashes,
                self.params.bands,
                METADATA_SECTION_TARGET_BYTES,
                &mut emit,
            )?;
            if let Some(signatures) = signatures {
                emit_signature_sections(signatures, self.scheme, self.num_perm, emit)?;
            }
            write_bucket_sections(
                writer,
                key_hashes,
                self.params.bands,
                bucket_target_bytes,
                sort_limit,
            )
        })?;
        Ok(())
    }

    #[cfg(test)]
    fn encode_snapshot(
        &self,
        signatures: Option<&[(u64, Vec<u64>)]>,
    ) -> Result<Vec<u8>, StoreError> {
        let metadata = IndexMetadata::new(
            Algorithm::MinHashLsh,
            self.scheme,
            CodecId::U64,
            u32::try_from(self.num_perm).map_err(|_| StoreError::LengthOverflow)?,
            self.seed,
            self.threshold,
            u32::try_from(self.params.bands).map_err(|_| StoreError::LengthOverflow)?,
            u32::try_from(self.params.rows).map_err(|_| StoreError::LengthOverflow)?,
            0,
        )?;
        let key_hashes = self.collect_live_hashes()?;
        let mut sections = Vec::new();
        append_key_metadata_sections(
            &mut sections,
            &key_hashes,
            self.params.bands,
            METADATA_SECTION_TARGET_BYTES,
        )?;
        if let Some(signatures) = signatures {
            emit_signature_sections(
                signatures,
                self.scheme,
                self.num_perm,
                |kind, required, payload| {
                    sections.push(Section::new(kind, required, payload)?);
                    Ok(())
                },
            )?;
        }
        append_bucket_sections(&mut sections, &key_hashes, self.params.bands)?;
        Ok(IndexFile::new(metadata, sections)?.encode()?)
    }
}

/// A persistent `MinHash32` LSH index with lazy committed-bucket reads.
#[derive(Debug)]
pub struct PersistentIndex32 {
    inner: PersistentIndexCore,
}

/// A persistent `MinHash64` LSH index with lazy committed-bucket reads.
///
/// Files opened by this type must explicitly declare `pari-affine64-v1` and
/// cannot be opened through [`PersistentIndex32`].
#[derive(Debug)]
pub struct PersistentIndex64 {
    inner: PersistentIndexCore,
}

macro_rules! impl_persistent_index {
    ($index:ident, $sketch:ty, $scheme:expr, $hash_band:ident) => {
        impl $index {
            /// Create a new empty index and immediately commit its initial snapshot.
            pub fn create(
                path: impl AsRef<Path>,
                threshold: f64,
                num_perm: usize,
                seed: u64,
            ) -> Result<Self, StoreError> {
                let params = LshParams::tune(threshold, num_perm)?;
                Self::create_with_params(path, threshold, num_perm, seed, params)
            }

            /// Create a new empty index with explicit LSH banding parameters.
            pub fn create_with_params(
                path: impl AsRef<Path>,
                threshold: f64,
                num_perm: usize,
                seed: u64,
                params: LshParams,
            ) -> Result<Self, StoreError> {
                Self::create_with_options(
                    path,
                    threshold,
                    num_perm,
                    seed,
                    PersistentIndexOptions::new().params(params),
                )
            }

            /// Create a new empty index from explicit creation options.
            ///
            /// Use [`PersistentIndexOptions::store_signatures`] to enable
            /// similarity-scored queries such as [`Self::query_scored`].
            pub fn create_with_options(
                path: impl AsRef<Path>,
                threshold: f64,
                num_perm: usize,
                seed: u64,
                options: PersistentIndexOptions,
            ) -> Result<Self, StoreError> {
                let params = match options.params {
                    Some(params) => params,
                    None => LshParams::tune(threshold, num_perm)?,
                };
                Ok(Self {
                    inner: PersistentIndexCore::create_with_params(
                        path.as_ref(),
                        threshold,
                        num_perm,
                        seed,
                        params,
                        $scheme,
                        options.store_signatures,
                    )?,
                })
            }

            /// Open and validate the last committed snapshot at `path`.
            ///
            /// Current snapshots load key metadata plus a sparse bucket directory. Legacy
            /// phase-1 snapshots are accepted by rebuilding their buckets into the
            /// mutation overlay and are marked dirty so the next sync upgrades them.
            pub fn open(path: impl AsRef<Path>) -> Result<Self, StoreError> {
                Ok(Self {
                    inner: PersistentIndexCore::open(path.as_ref(), $scheme)?,
                })
            }

            /// Insert one external key and signature.
            pub fn insert(&mut self, key: u64, sketch: &$sketch) -> Result<(), StoreError> {
                self.insert_many(std::iter::once((key, sketch)))
            }

            /// Insert a batch after validating the complete batch before mutation.
            pub fn insert_many<'a>(
                &mut self,
                items: impl IntoIterator<Item = (u64, &'a $sketch)>,
            ) -> Result<(), StoreError> {
                let store_signatures = self.inner.signatures.is_some();
                let prepared = items
                    .into_iter()
                    .map(|(key, sketch)| {
                        Ok(PreparedItem {
                            key,
                            hashes: self.band_hashes(sketch)?,
                            signature: if store_signatures {
                                sketch
                                    .signature()
                                    .iter()
                                    .map(|value| u64::from(*value))
                                    .collect()
                            } else {
                                Vec::new()
                            },
                        })
                    })
                    .collect::<Result<Vec<_>, StoreError>>()?;
                self.inner.insert_hashes_many(prepared)
            }

            /// Remove a key if present, returning whether the index changed.
            pub fn remove(&mut self, key: u64) -> bool {
                self.inner.remove(key)
            }

            /// Query approximate candidates for one compatible signature.
            pub fn query(&self, sketch: &$sketch) -> Result<Vec<u64>, StoreError> {
                let started = self.inner.query_metrics.as_ref().map(|_| Instant::now());
                let hashes = self.band_hashes(sketch)?;
                self.inner.query_hashes(&hashes, started)
            }

            /// Query many signatures while reusing candidate scratch storage.
            pub fn query_many<'a>(
                &self,
                sketches: impl IntoIterator<Item = &'a $sketch>,
            ) -> Result<Vec<Vec<u64>>, StoreError> {
                let started = self.inner.query_metrics.as_ref().map(|_| Instant::now());
                let hashes = sketches
                    .into_iter()
                    .map(|sketch| self.band_hashes(sketch))
                    .collect::<Result<Vec<_>, StoreError>>()?;
                self.inner.query_hashes_many(hashes, started)
            }

            /// Query candidates with their estimated Jaccard similarity.
            ///
            /// Results are sorted by similarity, highest first, then by key.
            /// Requires an index created with stored signatures; otherwise
            /// returns [`StoreError::SignaturesUnavailable`].
            pub fn query_scored(
                &self,
                sketch: &$sketch,
                filter: ScoreFilter,
            ) -> Result<Vec<ScoredCandidate>, StoreError> {
                let started = self.inner.query_metrics.as_ref().map(|_| Instant::now());
                let hashes = self.band_hashes(sketch)?;
                let signature = widen(sketch.signature());
                self.inner
                    .query_scored(&hashes, &signature, filter, started)
            }

            /// Query many signatures with similarity scores and one filter.
            pub fn query_scored_many<'a>(
                &self,
                sketches: impl IntoIterator<Item = &'a $sketch>,
                filter: ScoreFilter,
            ) -> Result<Vec<Vec<ScoredCandidate>>, StoreError> {
                let started = self.inner.query_metrics.as_ref().map(|_| Instant::now());
                let queries = sketches
                    .into_iter()
                    .map(|sketch| Ok((self.band_hashes(sketch)?, widen(sketch.signature()))))
                    .collect::<Result<Vec<_>, StoreError>>()?;
                self.inner.query_scored_many(queries, filter, started)
            }

            /// Return whether this index stores signatures for scored queries.
            #[must_use]
            pub const fn stores_signatures(&self) -> bool {
                self.inner.signatures.is_some()
            }

            /// Return the stored signature of a live key as a sketch.
            ///
            /// Returns `Ok(None)` when the key is absent or the index does not
            /// store signatures.
            pub fn signature(&self, key: u64) -> Result<Option<$sketch>, StoreError> {
                let Some(values) = self.inner.stored_signature(key)? else {
                    return Ok(None);
                };
                let values = values
                    .into_iter()
                    .map(|value| value.try_into().map_err(|_| StoreError::LengthOverflow))
                    .collect::<Result<Vec<_>, StoreError>>()?;
                Ok(Some(
                    <$sketch>::from_signature(values, self.inner.seed).map_err(|_| {
                        StoreError::InvalidSnapshot {
                            reason: "stored signature length is invalid",
                        }
                    })?,
                ))
            }

            /// Return whether a live key exists.
            #[must_use]
            pub fn contains(&self, key: u64) -> bool {
                self.inner.live_keys.contains(&key)
            }

            /// Return the number of live keys.
            #[must_use]
            pub fn len(&self) -> usize {
                self.inner.len()
            }

            /// Return whether no live keys are indexed.
            #[must_use]
            pub fn is_empty(&self) -> bool {
                self.inner.live_keys.is_empty()
            }

            /// Return the configured similarity threshold.
            #[must_use]
            pub const fn threshold(&self) -> f64 {
                self.inner.threshold
            }

            /// Return the configured signature length.
            #[must_use]
            pub const fn num_perm(&self) -> usize {
                self.inner.num_perm
            }

            /// Return the required `MinHash` seed.
            #[must_use]
            pub const fn seed(&self) -> u64 {
                self.inner.seed
            }

            /// Return the configured LSH banding parameters.
            #[must_use]
            pub const fn params(&self) -> LshParams {
                self.inner.params
            }

            /// Explain this index's persisted configuration without scanning buckets.
            pub fn explain(&self) -> Result<LshPlan, LshPlanError> {
                self.inner.explain()
            }

            /// Enable or disable process-local query observation.
            pub fn set_observability(&mut self, enabled: bool) {
                self.inner.set_observability(enabled);
            }

            /// Commit dirty state.
            ///
            /// Small changes are appended to the file as a delta generation:
            /// the record is synced, then a commit trailer is written and
            /// synced. Larger changes, or deltas that have grown large relative
            /// to the snapshot, rewrite the file as one snapshot that is synced
            /// and then atomically renamed into place. Use [`Self::sync`] for the
            /// strongest post-commit durability step the current platform
            /// supports, and [`Self::compact`] to rewrite on demand.
            pub fn flush(&mut self) -> Result<(), StoreError> {
                self.inner.flush()
            }

            /// Commit dirty state as an appended delta generation, without the
            /// size-based compaction that [`Self::flush`] applies.
            ///
            /// This is for bulk ingest that commits often: each call appends
            /// only the changes since the last commit, so the total write cost
            /// stays proportional to the data instead of rewriting a growing
            /// snapshot. The file is still rewritten once it holds 1,024
            /// generations or removed keys hide more than a quarter as many
            /// memberships as there are live keys. Queries look up every
            /// generation, so call [`Self::compact`] when the ingest finishes.
            /// Durability matches [`Self::flush`].
            pub fn flush_delta(&mut self) -> Result<(), StoreError> {
                self.inner.flush_delta()
            }

            /// Commit dirty state and rewrite the file as one snapshot.
            ///
            /// This folds appended delta generations into the snapshot and
            /// drops the memberships of removed keys. It is a no-op on a clean
            /// index that has neither. Durability matches [`Self::sync`].
            pub fn compact(&mut self) -> Result<(), StoreError> {
                self.inner.compact()
            }

            /// Commit dirty state and apply the platform-supported post-rename
            /// durability step. Unix-like platforms sync the containing directory;
            /// Windows has no portable directory `fsync` equivalent in `std`.
            pub fn sync(&mut self) -> Result<(), StoreError> {
                self.inner.sync()
            }

            /// Sync pending state and consume the index handle.
            pub fn close(self) -> Result<(), StoreError> {
                self.inner.close()
            }

            /// Return current in-memory and committed-file statistics.
            pub fn stats(&self) -> Result<StoreStats, StoreError> {
                self.inner.stats()
            }

            fn band_hashes(&self, sketch: &$sketch) -> Result<Vec<u64>, StoreError> {
                if sketch.seed() != self.inner.seed {
                    return Err(StoreError::IncompatibleSeed {
                        expected: self.inner.seed,
                        actual: sketch.seed(),
                    });
                }
                if sketch.num_perm() != self.inner.num_perm {
                    return Err(StoreError::IncompatiblePermutationCount {
                        expected: self.inner.num_perm,
                        actual: sketch.num_perm(),
                    });
                }
                let used = self
                    .inner
                    .params
                    .used_permutations()
                    .ok_or(StoreError::LengthOverflow)?;
                Ok(sketch.signature()[..used]
                    .chunks_exact(self.inner.params.rows)
                    .map($hash_band)
                    .collect())
            }
        }
    };
}

impl_persistent_index!(
    PersistentIndex32,
    MinHash32,
    SignatureScheme::PariAffine32V1,
    hash_band32
);
impl_persistent_index!(
    PersistentIndex64,
    MinHash64,
    SignatureScheme::PariAffine64V1,
    hash_band64
);

fn widen<T: Copy + Into<u64>>(values: &[T]) -> Vec<u64> {
    values.iter().map(|value| (*value).into()).collect()
}

fn create_parent_if_needed(path: &Path) -> Result<(), StoreError> {
    if let Some(parent) = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
    {
        fs::create_dir_all(parent)?;
    }
    Ok(())
}

/// Size and timestamps identifying one committed snapshot file.
///
/// Every commit publishes a freshly written file, so a different generation
/// means another handle committed. The committed handle's file identity is
/// compared as well when one is open.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct FileGeneration {
    len: u64,
    modified: Option<SystemTime>,
    created: Option<SystemTime>,
}

impl FileGeneration {
    fn from_metadata(metadata: &fs::Metadata) -> Self {
        Self {
            len: metadata.len(),
            modified: metadata.modified().ok(),
            created: metadata.created().ok(),
        }
    }

    fn of(file: &File) -> io::Result<Self> {
        file.metadata()
            .map(|metadata| Self::from_metadata(&metadata))
    }

    fn at(path: &Path) -> io::Result<Self> {
        fs::metadata(path).map(|metadata| Self::from_metadata(&metadata))
    }
}

fn concurrent_commit_error(path: &Path) -> StoreError {
    StoreError::Io(io::Error::other(format!(
        "index {} was committed by another handle after this one opened it; \
             uncommitted changes are kept in memory, reopen the index to continue",
        path.display()
    )))
}

/// Path of the advisory lock file that serializes commits to `path`.
fn commit_lock_path(path: &Path) -> Result<PathBuf, StoreError> {
    let mut name = path
        .file_name()
        .ok_or(StoreError::InvalidPath)?
        .to_os_string();
    name.push(".lock");
    Ok(path.with_file_name(name))
}

/// An exclusive operating-system lock held while one commit publishes.
///
/// The lock file exists only while a commit holds or waits for it. The holder
/// removes it before unlocking, so a waiter may wake holding a lock on a file
/// that is no longer at the lock path; it then retries with the current file.
/// Only the holder of the file currently at the path holds the commit lock.
#[derive(Debug)]
struct CommitLock {
    path: PathBuf,
    file: File,
}

/// Retries while a removed lock file is still pending deletion on Windows.
const COMMIT_LOCK_OPEN_ATTEMPTS: u32 = 2_000;
const COMMIT_LOCK_RETRY_DELAY: std::time::Duration = std::time::Duration::from_millis(5);

impl CommitLock {
    fn acquire(path: &Path) -> Result<Self, StoreError> {
        let path = commit_lock_path(path)?;
        let mut failed_opens = 0_u32;
        loop {
            let file = match OpenOptions::new()
                .read(true)
                .write(true)
                .create(true)
                .truncate(false)
                .open(&path)
            {
                Ok(file) => file,
                // Windows refuses to open a file whose deletion is pending.
                Err(error)
                    if error.kind() == io::ErrorKind::PermissionDenied
                        && failed_opens < COMMIT_LOCK_OPEN_ATTEMPTS =>
                {
                    failed_opens += 1;
                    std::thread::sleep(COMMIT_LOCK_RETRY_DELAY);
                    continue;
                }
                Err(error) => return Err(StoreError::Io(error)),
            };
            FileExt::lock(&file)?;
            let current = Handle::from_path(&path);
            if matches!(current, Ok(current) if Handle::from_file(file.try_clone()?)? == current) {
                return Ok(Self { path, file });
            }
            // The previous holder removed this file; dropping it unlocks it.
        }
    }
}

impl Drop for CommitLock {
    fn drop(&mut self) {
        // Remove while still locked so no waiter can claim this file again.
        let _ = fs::remove_file(&self.path);
        let _ = FileExt::unlock(&self.file);
    }
}

fn ensure_destination_absent(path: &Path) -> Result<(), StoreError> {
    match fs::symlink_metadata(path) {
        Ok(_) => Err(StoreError::Io(io::Error::new(
            io::ErrorKind::AlreadyExists,
            format!("index path {} already exists", path.display()),
        ))),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(StoreError::Io(error)),
    }
}

#[derive(Debug)]
struct OwnedTemporaryFile {
    path: PathBuf,
    file: Option<File>,
    identity: Handle,
    owns_path: bool,
}

impl OwnedTemporaryFile {
    fn allocate(target: &Path) -> Result<Self, StoreError> {
        target.file_name().ok_or(StoreError::InvalidPath)?;
        let parent = target
            .parent()
            .filter(|parent| !parent.as_os_str().is_empty())
            .unwrap_or_else(|| Path::new("."));
        let process_id = std::process::id();

        for _ in 0..TEMPORARY_ALLOCATION_ATTEMPTS {
            let sequence = NEXT_TEMPORARY_FILE.fetch_add(1, Ordering::Relaxed);
            let path = parent.join(format!(".pari-tmp-{process_id}-{sequence:016x}"));
            match OpenOptions::new().write(true).create_new(true).open(&path) {
                Ok(file) => {
                    let identity = match file.try_clone().and_then(Handle::from_file) {
                        Ok(identity) => identity,
                        Err(error) => {
                            drop(file);
                            return Err(cleanup_failed_allocation(&path, error));
                        }
                    };
                    return Ok(Self {
                        path,
                        file: Some(file),
                        identity,
                        owns_path: true,
                    });
                }
                Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {}
                Err(error) => return Err(StoreError::Io(error)),
            }
        }

        Err(StoreError::Io(io::Error::new(
            io::ErrorKind::AlreadyExists,
            format!(
                "could not allocate a unique temporary file beside {}",
                target.display()
            ),
        )))
    }

    /// Stream content through a buffered writer, then flush and sync it.
    fn write_streamed<F>(&mut self, write: F) -> Result<(), StoreError>
    where
        F: FnOnce(&mut BufWriter<&mut File>) -> Result<(), StoreError>,
    {
        let file = self.file.as_mut().ok_or_else(|| {
            StoreError::Io(io::Error::other(
                "persistent-index temporary file is already closed",
            ))
        })?;
        let mut writer = BufWriter::with_capacity(1024 * 1024, &mut *file);
        write(&mut writer)?;
        writer.flush()?;
        drop(writer);
        file.sync_all()?;
        Ok(())
    }

    /// Remove the owned temporary path after a failed snapshot write.
    fn cleanup_after_store(mut self, error: StoreError) -> StoreError {
        match self.remove_owned_path() {
            Ok(()) => error,
            Err(cleanup_error) => StoreError::Io(io::Error::other(format!(
                "{error}; additionally failed to remove transaction-owned {}: {cleanup_error}",
                self.path.display()
            ))),
        }
    }

    #[cfg(test)]
    fn write_synced(&mut self, bytes: &[u8]) -> io::Result<()> {
        let file = self
            .file
            .as_mut()
            .ok_or_else(|| io::Error::other("persistent-index temporary file is already closed"))?;
        file.write_all(bytes)?;
        file.flush()?;
        file.sync_all()
    }

    fn publish_replace(mut self, target: &Path) -> Result<(), StoreError> {
        if let Err(error) = self.verify_owned_path() {
            return Err(self.cleanup_after(error));
        }
        self.file.take();
        match fs::rename(&self.path, target) {
            Ok(()) => {
                self.owns_path = false;
                Ok(())
            }
            Err(error) => Err(self.cleanup_after(error)),
        }
    }

    fn publish_no_replace(self, target: &Path) -> Result<(), StoreError> {
        self.publish_no_replace_with(target, |path| fs::remove_file(path), sync_parent_directory)
    }

    fn publish_no_replace_with<R, S>(
        mut self,
        target: &Path,
        remove_temporary: R,
        sync_directory: S,
    ) -> Result<(), StoreError>
    where
        R: FnOnce(&Path) -> io::Result<()>,
        S: FnOnce(&Path) -> io::Result<()>,
    {
        self.file.take().ok_or_else(|| {
            StoreError::Io(io::Error::other(
                "persistent-index temporary file is already closed",
            ))
        })?;
        if let Err(error) = self.verify_owned_path() {
            return Err(self.cleanup_after(error));
        }

        if let Err(error) = fs::hard_link(&self.path, target) {
            return Err(self.cleanup_after(error));
        }
        if let Err(error) = remove_temporary(&self.path) {
            return Err(recover_after_initial_publication(self, target, error));
        }
        self.owns_path = false;
        if let Err(error) = sync_directory(target) {
            return Err(recover_after_initial_publication(self, target, error));
        }
        Ok(())
    }

    fn remove_owned_path(&mut self) -> io::Result<()> {
        self.file.take();
        if !self.owns_path {
            return Ok(());
        }
        if let Err(error) = self.verify_owned_path() {
            if error.kind() == io::ErrorKind::NotFound {
                self.owns_path = false;
                return Ok(());
            }
            return Err(error);
        }
        match fs::remove_file(&self.path) {
            Ok(()) => {
                self.owns_path = false;
                Ok(())
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                self.owns_path = false;
                Ok(())
            }
            Err(error) => Err(error),
        }
    }

    fn verify_owned_path(&mut self) -> io::Result<()> {
        let metadata = fs::symlink_metadata(&self.path)?;
        if metadata.file_type().is_symlink() {
            self.owns_path = false;
            return Err(io::Error::new(
                io::ErrorKind::NotFound,
                "transaction temporary path was replaced by a symlink",
            ));
        }
        let current = Handle::from_path(&self.path)?;
        if current != self.identity {
            self.owns_path = false;
            return Err(io::Error::new(
                io::ErrorKind::NotFound,
                "transaction temporary path was replaced",
            ));
        }
        Ok(())
    }

    fn cleanup_after(mut self, operation_error: io::Error) -> StoreError {
        match self.remove_owned_path() {
            Ok(()) => StoreError::Io(operation_error),
            Err(cleanup_error) => StoreError::Io(io::Error::new(
                operation_error.kind(),
                format!(
                    "{operation_error}; additionally failed to remove transaction-owned {}: \
                     {cleanup_error}",
                    self.path.display()
                ),
            )),
        }
    }
}

fn recover_after_initial_publication(
    mut temporary: OwnedTemporaryFile,
    destination: &Path,
    original: io::Error,
) -> StoreError {
    let kind = original.kind();
    let rollback = rollback_owned_destination(destination, &temporary.identity)
        .err()
        .map(|error| error.to_string());
    let cleanup = temporary
        .remove_owned_path()
        .err()
        .map(|error| error.to_string());
    if rollback.is_none() && cleanup.is_none() {
        return StoreError::Io(original);
    }
    StoreError::Io(io::Error::new(
        kind,
        PublicationRecoveryError {
            original,
            rollback,
            cleanup,
        },
    ))
}

fn cleanup_failed_allocation(path: &Path, original: io::Error) -> StoreError {
    let kind = original.kind();
    match fs::remove_file(path) {
        Ok(()) => StoreError::Io(original),
        Err(error) if error.kind() == io::ErrorKind::NotFound => StoreError::Io(original),
        Err(cleanup) => StoreError::Io(io::Error::new(
            kind,
            format!(
                "{original}; additionally failed to remove transaction-owned {}: {cleanup}",
                path.display()
            ),
        )),
    }
}

fn rollback_owned_destination(destination: &Path, identity: &Handle) -> io::Result<()> {
    let metadata = match fs::symlink_metadata(destination) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error),
    };
    if metadata.file_type().is_symlink() {
        return Ok(());
    }
    let current = match Handle::from_path(destination) {
        Ok(current) => current,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error),
    };
    if &current != identity {
        return Ok(());
    }
    fs::remove_file(destination)?;
    sync_parent_directory(destination)
}

#[derive(Debug)]
struct PublicationRecoveryError {
    original: io::Error,
    rollback: Option<String>,
    cleanup: Option<String>,
}

impl fmt::Display for PublicationRecoveryError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{}", self.original)?;
        if let Some(rollback) = &self.rollback {
            write!(
                formatter,
                "; also failed to roll back destination: {rollback}"
            )?;
        }
        if let Some(cleanup) = &self.cleanup {
            write!(
                formatter,
                "; also failed to remove temporary file: {cleanup}"
            )?;
        }
        Ok(())
    }
}

impl Error for PublicationRecoveryError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        Some(&self.original)
    }
}

impl Drop for OwnedTemporaryFile {
    fn drop(&mut self) {
        let _ = self.remove_owned_path();
    }
}

fn validate_store_metadata(
    metadata: &IndexMetadata,
    expected_scheme: SignatureScheme,
) -> Result<(), StoreError> {
    if metadata.algorithm() != Algorithm::MinHashLsh {
        return Err(StoreError::InvalidSnapshot {
            reason: "snapshot algorithm is not MinHash LSH",
        });
    }
    if metadata.signature_scheme() != expected_scheme {
        return Err(StoreError::InvalidSnapshot {
            reason: "snapshot signature scheme does not match the persistent index type",
        });
    }
    if metadata.key_codec() != CodecId::U64 {
        return Err(StoreError::InvalidSnapshot {
            reason: "local store requires u64 keys",
        });
    }
    if metadata.feature_flags() != 0 {
        return Err(StoreError::InvalidSnapshot {
            reason: "local store does not support feature flags",
        });
    }
    Ok(())
}

fn validate_lsh_configuration(
    _scheme: SignatureScheme,
    threshold: f64,
    num_perm: usize,
    _seed: u64,
    params: LshParams,
) -> Result<(), StoreError> {
    params.validate(threshold, num_perm)?;
    Ok(())
}

/// Keys in file row order plus each key's band hashes.
type KeyMetadata = (Vec<u64>, BTreeMap<u64, Vec<u64>>);

fn read_key_metadata(
    layout: &FileLayout,
    file: &mut File,
    bands: usize,
) -> Result<KeyMetadata, StoreError> {
    let (keys, rows_by_key) = read_key_rows(layout, layout.sections(), file, bands)?;
    let mut key_hashes = BTreeMap::new();
    for (key, hashes) in keys.iter().copied().zip(rows_by_key) {
        if key_hashes.insert(key, hashes).is_some() {
            return Err(StoreError::InvalidSnapshot {
                reason: "duplicate key in persisted keys section",
            });
        }
    }
    Ok((keys, key_hashes))
}

/// Read the keys of `sections` in file order, plus the set of them, checking
/// every band-hash row without keeping it.
fn read_key_catalog(
    layout: &FileLayout,
    sections: &[SectionDescriptor],
    file: &mut File,
    bands: usize,
) -> Result<(Vec<u64>, HashSet<u64>), StoreError> {
    let mut keys = Vec::new();
    let mut catalog = HashSet::new();
    for (keys_descriptor, hashes_descriptor) in key_metadata_pairs(sections)? {
        let chunk_keys = decode_keys(&layout.read_section(file, keys_descriptor)?)?;
        // The section checksum covers the rows; only their shape needs checking.
        let rows = band_hash_rows(&layout.read_section(file, hashes_descriptor)?, bands)?;
        if chunk_keys.len() != rows {
            return Err(StoreError::InvalidSnapshot {
                reason: "key and band-hash record counts differ",
            });
        }
        catalog.reserve(chunk_keys.len());
        for key in &chunk_keys {
            if !catalog.insert(*key) {
                return Err(StoreError::InvalidSnapshot {
                    reason: "duplicate key in persisted keys section",
                });
            }
        }
        keys.extend(chunk_keys);
    }
    Ok((keys, catalog))
}

/// Visit each key of `sections` with its band-hash row, in file order, one
/// section pair at a time.
fn for_each_key_row(
    layout: &FileLayout,
    sections: &[SectionDescriptor],
    file: &mut File,
    bands: usize,
    mut visit: impl FnMut(u64, Vec<u64>) -> Result<(), StoreError>,
) -> Result<(), StoreError> {
    for (keys_descriptor, hashes_descriptor) in key_metadata_pairs(sections)? {
        let chunk_keys = decode_keys(&layout.read_section(file, keys_descriptor)?)?;
        let chunk_rows = decode_band_hashes(&layout.read_section(file, hashes_descriptor)?, bands)?;
        if chunk_keys.len() != chunk_rows.len() {
            return Err(StoreError::InvalidSnapshot {
                reason: "key and band-hash record counts differ",
            });
        }
        for (key, row) in chunk_keys.into_iter().zip(chunk_rows) {
            visit(key, row)?;
        }
    }
    Ok(())
}

/// Read the keys and band-hash rows of `sections`, in file order.
fn read_key_rows(
    layout: &FileLayout,
    sections: &[SectionDescriptor],
    file: &mut File,
    bands: usize,
) -> Result<(Vec<u64>, Vec<Vec<u64>>), StoreError> {
    let mut keys = Vec::new();
    let mut rows_by_key = Vec::new();
    for (keys_descriptor, hashes_descriptor) in key_metadata_pairs(sections)? {
        let chunk_keys = decode_keys(&layout.read_section(file, keys_descriptor)?)?;
        let chunk_rows = decode_band_hashes(&layout.read_section(file, hashes_descriptor)?, bands)?;
        if chunk_keys.len() != chunk_rows.len() {
            return Err(StoreError::InvalidSnapshot {
                reason: "key and band-hash record counts differ",
            });
        }
        keys.extend(chunk_keys);
        rows_by_key.extend(chunk_rows);
    }
    Ok((keys, rows_by_key))
}

/// Return the row-aligned `Keys`/`BandHashes` section pairs in file order.
///
/// Small catalogs use one pair. Larger ones are split into several pairs whose
/// `n`-th `Keys` section and `n`-th `BandHashes` section hold the same rows.
fn key_metadata_pairs(
    sections: &[SectionDescriptor],
) -> Result<Vec<(SectionDescriptor, SectionDescriptor)>, StoreError> {
    let of_kind = |kind| -> Result<Vec<SectionDescriptor>, StoreError> {
        let sections: Vec<_> = sections
            .iter()
            .copied()
            .filter(|descriptor| descriptor.kind() == kind)
            .collect();
        if sections.is_empty() {
            return Err(StoreError::InvalidSnapshot {
                reason: missing_section_reason(kind),
            });
        }
        if sections.iter().any(|descriptor| !descriptor.required()) {
            return Err(StoreError::InvalidSnapshot {
                reason: "required metadata section is marked optional",
            });
        }
        Ok(sections)
    };
    let keys = of_kind(SectionKind::Keys)?;
    let hashes = of_kind(SectionKind::BandHashes)?;
    if keys.len() != hashes.len() {
        return Err(StoreError::InvalidSnapshot {
            reason: "keys and band-hash sections are not paired",
        });
    }
    Ok(keys.into_iter().zip(hashes).collect())
}

/// Write keys and band hashes as one or more row-aligned section pairs.
///
/// Every `Keys` section is written before the `BandHashes` sections, so a
/// catalog that fits one pair keeps the original single-pair byte layout.
fn emit_key_metadata_sections(
    key_hashes: &BTreeMap<u64, Vec<u64>>,
    bands: usize,
    target_bytes: usize,
    mut emit: impl FnMut(SectionKind, bool, Vec<u8>) -> Result<(), StoreError>,
) -> Result<(), StoreError> {
    let row_bytes = bands
        .max(1)
        .checked_mul(U64_BYTES)
        .ok_or(StoreError::LengthOverflow)?;
    let rows_per_section = (target_bytes.saturating_sub(COUNT_BYTES) / row_bytes).max(1);
    let chunks = key_hashes.len().div_ceil(rows_per_section).max(1);
    let mut keys = key_hashes.keys().copied();
    for _ in 0..chunks {
        emit(
            SectionKind::Keys,
            true,
            encode_keys(keys.by_ref().take(rows_per_section))?,
        )?;
    }
    let mut hashes = key_hashes.values();
    for _ in 0..chunks {
        emit(
            SectionKind::BandHashes,
            true,
            encode_band_hashes(hashes.by_ref().take(rows_per_section), bands)?,
        )?;
    }
    Ok(())
}

#[cfg(test)]
fn append_key_metadata_sections(
    sections: &mut Vec<Section>,
    key_hashes: &BTreeMap<u64, Vec<u64>>,
    bands: usize,
    target_bytes: usize,
) -> Result<(), StoreError> {
    emit_key_metadata_sections(
        key_hashes,
        bands,
        target_bytes,
        |kind, required, payload| {
            sections.push(Section::new(kind, required, payload)?);
            Ok(())
        },
    )
}

/// Validate optional signature sections, returning `None` when absent.
///
/// A file written with signatures enabled always contains at least one
/// section, even when it has no keys, so presence alone records the setting.
fn read_signature_segments(
    layout: &FileLayout,
    sections: &[SectionDescriptor],
    file: &mut File,
    rows: usize,
) -> Result<Option<Vec<SignatureSegment>>, StoreError> {
    let mut segments = Vec::new();
    for descriptor in sections {
        if descriptor.kind() != SectionKind::Signatures {
            continue;
        }
        if descriptor.required() {
            return Err(StoreError::InvalidSnapshot {
                reason: "signature sections must be optional",
            });
        }
        segments.push(decode_signature_segment(layout, file, *descriptor)?);
    }
    if segments.is_empty() {
        return Ok(None);
    }
    let rows = u64::try_from(rows).map_err(|_| StoreError::LengthOverflow)?;
    validate_signature_coverage(&segments, rows)?;
    Ok(Some(segments))
}

fn read_all_signatures(
    layout: &FileLayout,
    file: &mut File,
    segments: &[SignatureSegment],
    file_keys: &[u64],
) -> Result<HashMap<u64, Vec<u64>>, StoreError> {
    let width =
        usize::try_from(layout.metadata().num_perm()).map_err(|_| StoreError::LengthOverflow)?;
    let mut keys = file_keys.iter();
    let mut output = HashMap::with_capacity(file_keys.len());
    for segment in segments {
        let values = read_signature_segment_values(layout, file, *segment)?;
        for row in values.chunks_exact(width) {
            let key = keys.next().ok_or(StoreError::InvalidSnapshot {
                reason: "signature rows exceed the key count",
            })?;
            output.insert(*key, row.to_vec());
        }
    }
    Ok(output)
}

fn sorted_rows(file_keys: &[u64]) -> Vec<(u64, u64)> {
    let mut rows: Vec<(u64, u64)> = file_keys.iter().copied().zip(0_u64..).collect();
    rows.sort_unstable_by_key(|(key, _)| *key);
    rows
}

fn emit_signature_sections(
    signatures: &[(u64, Vec<u64>)],
    scheme: SignatureScheme,
    num_perm: usize,
    mut emit: impl FnMut(SectionKind, bool, Vec<u8>) -> Result<(), StoreError>,
) -> Result<(), StoreError> {
    let num_perm = u32::try_from(num_perm).map_err(|_| StoreError::LengthOverflow)?;
    let row_bytes = signature_row_bytes(scheme, num_perm)?.max(1);
    let rows_per_segment =
        ((SIGNATURE_SEGMENT_TARGET_BYTES - SIGNATURE_SEGMENT_HEADER_BYTES) / row_bytes).max(1);
    if signatures.is_empty() {
        return emit(
            SectionKind::Signatures,
            false,
            encode_signature_segment(scheme, num_perm, 0, std::iter::empty())?,
        );
    }
    for (index, chunk) in signatures.chunks(rows_per_segment).enumerate() {
        let first_row = index
            .checked_mul(rows_per_segment)
            .and_then(|row| u64::try_from(row).ok())
            .ok_or(StoreError::LengthOverflow)?;
        let payload = encode_signature_segment(
            scheme,
            num_perm,
            first_row,
            chunk.iter().map(|(_, values)| values.as_slice()),
        )?;
        emit(SectionKind::Signatures, false, payload)?;
    }
    Ok(())
}

impl From<ContainerWriteError> for StoreError {
    fn from(error: ContainerWriteError) -> Self {
        match error {
            ContainerWriteError::Io(error) => Self::Io(error),
            ContainerWriteError::Format(error) => Self::Format(error),
            ContainerWriteError::SectionCountMismatch { .. } => Self::InvalidSnapshot {
                reason: "snapshot writer section count mismatch",
            },
        }
    }
}

/// Read one committed delta record: its generation and what it stores.
fn read_delta_generation(
    layout: &FileLayout,
    file: &mut File,
    sections: &[SectionDescriptor],
    bands: usize,
    stores_signatures: bool,
) -> Result<(Generation, DeltaContents), StoreError> {
    let (keys, rows) = read_key_rows(layout, sections, file, bands)?;
    let mut tombstone_sections = sections
        .iter()
        .filter(|descriptor| descriptor.kind() == SectionKind::Tombstones);
    let (Some(tombstone_section), None) = (tombstone_sections.next(), tombstone_sections.next())
    else {
        return Err(StoreError::InvalidSnapshot {
            reason: "delta must have exactly one tombstone section",
        });
    };
    let tombstones = decode_keys(&layout.read_section(file, *tombstone_section)?)?;
    if tombstones.windows(2).any(|pair| pair[0] >= pair[1]) {
        return Err(StoreError::InvalidSnapshot {
            reason: "delta tombstones are not strictly ascending",
        });
    }
    let signature_segments = read_signature_segments(layout, sections, file, keys.len())?;
    if signature_segments.is_some() != stores_signatures {
        return Err(StoreError::InvalidSnapshot {
            reason: "delta signature sections do not match the snapshot",
        });
    }
    let buckets = collect_bucket_descriptors(sections, true)?;
    let directory = BucketDirectory::read(layout, file, &buckets, bands)?;
    if keys.is_empty() != directory.is_empty() {
        return Err(StoreError::InvalidSnapshot {
            reason: "delta bucket memberships do not match its keys",
        });
    }
    let generation = Generation {
        directory,
        keys: keys.len(),
        signature_rows: if stores_signatures {
            sorted_rows(&keys)
        } else {
            Vec::new()
        },
        signature_segments: signature_segments.unwrap_or_default(),
    };
    Ok((
        generation,
        DeltaContents {
            keys,
            rows,
            tombstones,
        },
    ))
}

/// The pending changes a delta record stores.
struct DeltaRecord<'a> {
    keys: &'a BTreeMap<u64, Vec<u64>>,
    tombstones: &'a [u64],
    signatures: Option<&'a [(u64, Vec<u64>)]>,
    bands: usize,
    scheme: SignatureScheme,
    num_perm: usize,
}

/// Append and commit one delta record at `next`.
///
/// Bytes after the committed data belong to an interrupted commit and are
/// discarded first. The record is made durable before its commit trailer is
/// written, and the trailer is made durable before this returns.
fn write_delta_record(
    path: &Path,
    next: NextDelta,
    record: &DeltaRecord<'_>,
) -> Result<(), StoreError> {
    let mut file = OpenOptions::new().write(true).open(path)?;
    let length = file.metadata()?.len();
    if length < next.offset() {
        return Err(StoreError::InvalidSnapshot {
            reason: "index file is shorter than its committed data",
        });
    }
    if length > next.offset() {
        file.set_len(next.offset())?;
    }
    file.seek(SeekFrom::Start(next.offset()))?;
    let (writer, commit) =
        ContainerWriter::write_delta(BufWriter::with_capacity(1 << 20, file), next, |writer| {
            let mut emit = |kind, required, payload: Vec<u8>| {
                writer.write_section(kind, required, &payload)?;
                Ok(())
            };
            emit_key_metadata_sections(
                record.keys,
                record.bands,
                METADATA_SECTION_TARGET_BYTES,
                &mut emit,
            )?;
            emit(
                SectionKind::Tombstones,
                true,
                encode_keys(record.tombstones.iter().copied())?,
            )?;
            if let Some(signatures) = record.signatures {
                emit_signature_sections(signatures, record.scheme, record.num_perm, emit)?;
            }
            write_bucket_sections(
                writer,
                record.keys,
                record.bands,
                BUCKET_SEGMENT_TARGET_BYTES,
                SORT_RECORD_LIMIT,
            )
        })?;
    let mut file = writer
        .into_inner()
        .map_err(|error| StoreError::Io(error.into_error()))?;
    file.sync_data()?;
    file.write_all(&commit.trailer())?;
    file.sync_data()?;
    Ok(())
}

/// One validated insertion: key, band hashes, and the signature to store.
struct PreparedItem {
    key: u64,
    hashes: Vec<u64>,
    signature: Vec<u64>,
}

/// Return the bucket sections of a base snapshot, or of a delta record when
/// `delta` is set; only delta records carry (required) tombstone sections.
fn collect_bucket_descriptors(
    sections: &[SectionDescriptor],
    delta: bool,
) -> Result<Vec<SectionDescriptor>, StoreError> {
    let mut descriptors = Vec::new();
    for descriptor in sections {
        match descriptor.kind() {
            SectionKind::Buckets => {
                if !descriptor.required() {
                    return Err(StoreError::InvalidSnapshot {
                        reason: "lazy bucket sections must be required",
                    });
                }
                descriptors.push(*descriptor);
            }
            SectionKind::Tombstones if descriptor.required() != delta => {
                return Err(StoreError::InvalidSnapshot {
                    reason: if delta {
                        "delta tombstone sections must be required"
                    } else {
                        "required tombstone sections are not supported"
                    },
                });
            }
            SectionKind::Keys
            | SectionKind::BandHashes
            | SectionKind::Tombstones
            | SectionKind::Signatures => {}
        }
    }
    Ok(descriptors)
}

const fn missing_section_reason(kind: SectionKind) -> &'static str {
    match kind {
        SectionKind::Keys => "missing required keys section",
        SectionKind::BandHashes => "missing required band-hash section",
        SectionKind::Buckets => "missing required bucket section",
        SectionKind::Tombstones => "missing required tombstone section",
        SectionKind::Signatures => "missing required signature section",
    }
}

/// One band's uncommitted buckets.
///
/// Most LSH buckets hold a single key, so the first member of every bucket is
/// stored inline and only buckets with more members allocate a list. This
/// keeps a singleton bucket at one map entry instead of a map entry plus a
/// separately allocated vector.
#[derive(Debug, Default)]
struct OverlayTable {
    /// The first member of every bucket.
    first: HashMap<u64, u64>,
    /// The remaining members of buckets with more than one.
    rest: HashMap<u64, Vec<u64>>,
}

impl OverlayTable {
    fn insert(&mut self, hash: u64, key: u64) {
        match self.first.entry(hash) {
            std::collections::hash_map::Entry::Vacant(entry) => {
                entry.insert(key);
            }
            std::collections::hash_map::Entry::Occupied(_) => {
                self.rest.entry(hash).or_default().push(key);
            }
        }
    }

    fn remove(&mut self, hash: u64, key: u64) {
        let Some(first) = self.first.get_mut(&hash) else {
            return;
        };
        if *first == key {
            // Promote a remaining member, or drop the now-empty bucket.
            match self.rest.get_mut(&hash) {
                Some(rest) => {
                    if let Some(next) = rest.pop() {
                        *first = next;
                    }
                    if rest.is_empty() {
                        self.rest.remove(&hash);
                    }
                }
                None => {
                    self.first.remove(&hash);
                }
            }
        } else if let Some(rest) = self.rest.get_mut(&hash) {
            rest.retain(|candidate| *candidate != key);
            if rest.is_empty() {
                self.rest.remove(&hash);
            }
        }
    }

    fn extend_members(&self, hash: u64, output: &mut HashSet<u64>) {
        if let Some(first) = self.first.get(&hash) {
            output.insert(*first);
            if let Some(rest) = self.rest.get(&hash) {
                output.extend(rest.iter().copied());
            }
        }
    }

    fn len(&self) -> usize {
        self.first.len()
    }

    fn bucket_sizes(&self) -> impl Iterator<Item = usize> + '_ {
        self.first
            .keys()
            .map(|hash| 1 + self.rest.get(hash).map_or(0, Vec::len))
    }
}

fn empty_bucket_tables(bands: usize) -> Vec<OverlayTable> {
    std::iter::repeat_with(OverlayTable::default)
        .take(bands)
        .collect()
}

fn add_overlay_key(tables: &mut [OverlayTable], key: u64, hashes: &[u64]) {
    for (table, hash) in tables.iter_mut().zip(hashes) {
        table.insert(*hash, key);
    }
}

fn remove_overlay_key(tables: &mut [OverlayTable], key: u64, hashes: &[u64]) {
    for (table, hash) in tables.iter_mut().zip(hashes) {
        table.remove(*hash, key);
    }
}

fn collect_overlay_candidates(tables: &[OverlayTable], hashes: &[u64], output: &mut HashSet<u64>) {
    for (table, hash) in tables.iter().zip(hashes) {
        table.extend_members(*hash, output);
    }
}

#[cfg(test)]
fn append_bucket_sections(
    sections: &mut Vec<Section>,
    key_hashes: &BTreeMap<u64, Vec<u64>>,
    bands: usize,
) -> Result<(), StoreError> {
    let buckets = materialize_sorted_buckets(key_hashes, bands)?;
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
    for (key, members) in &buckets {
        let contribution = bucket_record_size(members.len())?;
        if !current.is_empty()
            && estimated
                .checked_add(contribution)
                .ok_or(StoreError::LengthOverflow)?
                > BUCKET_SEGMENT_TARGET_BYTES
        {
            push_bucket_section(sections, &current)?;
            current.clear();
            estimated = BUCKET_SEGMENT_HEADER_BYTES;
        }
        estimated = estimated
            .checked_add(contribution)
            .ok_or(StoreError::LengthOverflow)?;
        current.push((*key, members));
    }
    if !current.is_empty() {
        push_bucket_section(sections, &current)?;
    }
    Ok(())
}

#[cfg(test)]
fn push_bucket_section(
    sections: &mut Vec<Section>,
    records: &[(BucketKey, &[u64])],
) -> Result<(), StoreError> {
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

#[cfg(test)]
fn materialize_sorted_buckets(
    key_hashes: &BTreeMap<u64, Vec<u64>>,
    bands: usize,
) -> Result<BTreeMap<BucketKey, Vec<u64>>, StoreError> {
    let mut buckets = BTreeMap::new();
    for (key, hashes) in key_hashes {
        if hashes.len() != bands {
            return Err(StoreError::InvalidSnapshot {
                reason: "in-memory band-hash row has the wrong width",
            });
        }
        for (band, hash) in hashes.iter().copied().enumerate() {
            buckets
                .entry(BucketKey::new(
                    u32::try_from(band).map_err(|_| StoreError::LengthOverflow)?,
                    hash,
                ))
                .or_insert_with(Vec::new)
                .push(*key);
        }
    }
    Ok(buckets)
}

fn encode_keys(keys: impl ExactSizeIterator<Item = u64>) -> Result<Vec<u8>, StoreError> {
    let count = u64::try_from(keys.len()).map_err(|_| StoreError::LengthOverflow)?;
    let capacity = COUNT_BYTES
        .checked_add(
            keys.len()
                .checked_mul(U64_BYTES)
                .ok_or(StoreError::LengthOverflow)?,
        )
        .ok_or(StoreError::LengthOverflow)?;
    let mut output = Vec::with_capacity(capacity);
    output.extend_from_slice(&count.to_le_bytes());
    for key in keys {
        output.extend_from_slice(&key.to_le_bytes());
    }
    Ok(output)
}

fn decode_keys(payload: &[u8]) -> Result<Vec<u64>, StoreError> {
    let count = read_count(payload)?;
    let expected = COUNT_BYTES
        .checked_add(
            count
                .checked_mul(U64_BYTES)
                .ok_or(StoreError::LengthOverflow)?,
        )
        .ok_or(StoreError::LengthOverflow)?;
    if payload.len() != expected {
        return Err(StoreError::InvalidSnapshot {
            reason: "keys section length does not match its record count",
        });
    }
    payload[COUNT_BYTES..]
        .chunks_exact(U64_BYTES)
        .map(read_u64)
        .collect()
}

fn encode_band_hashes<'a>(
    rows: impl ExactSizeIterator<Item = &'a Vec<u64>>,
    bands: usize,
) -> Result<Vec<u8>, StoreError> {
    let count = u64::try_from(rows.len()).map_err(|_| StoreError::LengthOverflow)?;
    let values = rows
        .len()
        .checked_mul(bands)
        .ok_or(StoreError::LengthOverflow)?;
    let capacity = COUNT_BYTES
        .checked_add(
            values
                .checked_mul(U64_BYTES)
                .ok_or(StoreError::LengthOverflow)?,
        )
        .ok_or(StoreError::LengthOverflow)?;
    let mut output = Vec::with_capacity(capacity);
    output.extend_from_slice(&count.to_le_bytes());
    for hashes in rows {
        if hashes.len() != bands {
            return Err(StoreError::InvalidSnapshot {
                reason: "in-memory band-hash row has the wrong width",
            });
        }
        for hash in hashes {
            output.extend_from_slice(&hash.to_le_bytes());
        }
    }
    Ok(output)
}

/// Check a band-hash section's length against its record count and return the
/// number of rows, without decoding them.
fn band_hash_rows(payload: &[u8], bands: usize) -> Result<usize, StoreError> {
    let count = read_count(payload)?;
    let row_bytes = bands
        .checked_mul(U64_BYTES)
        .ok_or(StoreError::LengthOverflow)?;
    let expected = COUNT_BYTES
        .checked_add(
            count
                .checked_mul(row_bytes)
                .ok_or(StoreError::LengthOverflow)?,
        )
        .ok_or(StoreError::LengthOverflow)?;
    if payload.len() != expected {
        return Err(StoreError::InvalidSnapshot {
            reason: "band-hash section length does not match metadata and record count",
        });
    }
    Ok(count)
}

fn decode_band_hashes(payload: &[u8], bands: usize) -> Result<Vec<Vec<u64>>, StoreError> {
    let count = band_hash_rows(payload, bands)?;
    let row_bytes = bands * U64_BYTES;
    let mut rows = Vec::with_capacity(count);
    for row in payload[COUNT_BYTES..].chunks_exact(row_bytes) {
        let hashes = row
            .chunks_exact(U64_BYTES)
            .map(read_u64)
            .collect::<Result<_, _>>()?;
        rows.push(hashes);
    }
    Ok(rows)
}

fn read_count(payload: &[u8]) -> Result<usize, StoreError> {
    let raw = payload
        .get(..COUNT_BYTES)
        .ok_or(StoreError::InvalidSnapshot {
            reason: "section is missing its record count",
        })?;
    let count = read_u64(raw)?;
    usize::try_from(count).map_err(|_| StoreError::LengthOverflow)
}

fn read_u64(bytes: &[u8]) -> Result<u64, StoreError> {
    let raw: [u8; U64_BYTES] = bytes.try_into().map_err(|_| StoreError::InvalidSnapshot {
        reason: "fixed-width u64 field is truncated",
    })?;
    Ok(u64::from_le_bytes(raw))
}

#[cfg_attr(not(unix), allow(clippy::unnecessary_wraps))]
fn sync_parent_directory(path: &Path) -> io::Result<()> {
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

fn hash_band32(values: &[u32]) -> u64 {
    let mut hash = FNV_OFFSET_BASIS;
    for value in values {
        hash ^= u64::from(*value);
        hash = hash.wrapping_mul(FNV_PRIME);
    }
    hash ^= u64::try_from(values.len()).unwrap_or(u64::MAX);
    avalanche64(hash)
}

fn hash_band64(values: &[u64]) -> u64 {
    let mut hash = FNV_OFFSET_BASIS;
    for value in values {
        hash ^= *value;
        hash = hash.wrapping_mul(FNV_PRIME);
    }
    hash ^= u64::try_from(values.len()).unwrap_or(u64::MAX);
    avalanche64(hash)
}

fn avalanche64(mut value: u64) -> u64 {
    value ^= value >> 33;
    value = value.wrapping_mul(0xFF51_AFD7_ED55_8CCD);
    value ^= value >> 33;
    value = value.wrapping_mul(0xC4CE_B9FE_1A85_EC53);
    value ^ (value >> 33)
}

#[cfg(test)]
mod tests {
    use std::{
        collections::BTreeMap,
        fs::{self, OpenOptions},
        io::{self, Seek, SeekFrom, Write},
        path::{Path, PathBuf},
        sync::atomic::{AtomicU64, Ordering},
    };

    use pari_core::{MinHash32, MinHash64};
    use pari_format::{
        Algorithm, BucketError, CodecId, FileLayout, IndexFile, IndexMetadata, Section,
        SectionKind, SignatureScheme, BUCKET_SEGMENT_HEADER_BYTES,
    };
    use pari_index::{LshIndex32, LshIndex64, LshParams};

    use super::{
        append_key_metadata_sections, commit_lock_path, encode_band_hashes, encode_keys,
        CommitLock, OwnedTemporaryFile, PersistentIndex32, PersistentIndex64, PersistentIndexCore,
        StoreError,
    };

    /// Rewrite a committed snapshot with its key metadata split into pairs of
    /// at most `rows_per_section` rows, keeping every other section.
    fn split_key_metadata(path: &Path, output: &Path, rows_per_section: usize) -> usize {
        let file = IndexFile::decode(&fs::read(path).expect("snapshot")).expect("decode");
        let bands = usize::try_from(file.metadata().bands()).expect("bands");
        let keys = super::decode_keys(
            file.sections()
                .iter()
                .find(|section| section.kind() == SectionKind::Keys)
                .expect("keys")
                .payload(),
        )
        .expect("keys payload");
        let rows = super::decode_band_hashes(
            file.sections()
                .iter()
                .find(|section| section.kind() == SectionKind::BandHashes)
                .expect("hashes")
                .payload(),
            bands,
        )
        .expect("hash payload");
        let key_hashes: BTreeMap<u64, Vec<u64>> = keys.into_iter().zip(rows).collect();
        let mut sections = Vec::new();
        append_key_metadata_sections(
            &mut sections,
            &key_hashes,
            bands,
            8 + rows_per_section * bands * 8,
        )
        .expect("split metadata");
        let pairs = sections.len() / 2;
        sections.extend(
            file.sections()
                .iter()
                .filter(|section| {
                    !matches!(section.kind(), SectionKind::Keys | SectionKind::BandHashes)
                })
                .cloned(),
        );
        let encoded = IndexFile::new(file.metadata().clone(), sections)
            .expect("file")
            .encode()
            .expect("encode");
        fs::write(output, encoded).expect("write split snapshot");
        pairs
    }

    #[test]
    fn overlay_table_matches_a_reference_multimap() {
        use std::collections::{HashMap, HashSet};

        let mut table = super::OverlayTable::default();
        let mut reference: HashMap<u64, HashSet<u64>> = HashMap::new();
        // A small hash range forces shared buckets, promotions, and removals
        // of both inline and overflow members.
        let mut state = 0x9E37_79B9_7F4A_7C15_u64;
        let mut next = || {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            state
        };
        let mut live: Vec<(u64, u64)> = Vec::new();
        for key in 0..4_000_u64 {
            if !live.is_empty() && next() % 3 == 0 {
                let index = usize::try_from(next() % live.len() as u64).expect("index");
                let (hash, removed) = live.swap_remove(index);
                table.remove(hash, removed);
                let members = reference.get_mut(&hash).expect("bucket");
                members.remove(&removed);
                if members.is_empty() {
                    reference.remove(&hash);
                }
            }
            let hash = next() % 64;
            table.insert(hash, key);
            reference.entry(hash).or_default().insert(key);
            live.push((hash, key));
        }
        // Removing an absent key or hash changes nothing.
        table.remove(1_000_000, 1);
        table.remove(live[0].0, u64::MAX);

        assert_eq!(table.len(), reference.len());
        let mut sizes: Vec<_> = table.bucket_sizes().collect();
        let mut expected_sizes: Vec<_> = reference.values().map(HashSet::len).collect();
        sizes.sort_unstable();
        expected_sizes.sort_unstable();
        assert_eq!(sizes, expected_sizes);
        for hash in 0..64 {
            let mut members = HashSet::new();
            table.extend_members(hash, &mut members);
            assert_eq!(members, reference.get(&hash).cloned().unwrap_or_default());
        }
        assert!(table.rest.values().all(|rest| !rest.is_empty()));
        assert!(table.rest.keys().all(|hash| table.first.contains_key(hash)));
    }

    #[test]
    fn key_metadata_split_across_section_pairs_round_trips() {
        let path = test_path("split-source");
        let split = test_path("split-output");
        let items: Vec<_> = (0..50_u64)
            .map(|item| sketch(item * 7..item * 7 + 40))
            .collect();
        let mut index = PersistentIndex32::create(&path, 0.8, 128, 7).expect("create");
        index
            .insert_many(items.iter().zip(0_u64..).map(|(sketch, key)| (key, sketch)))
            .expect("insert");
        index.sync().expect("sync");
        let expected: Vec<_> = items
            .iter()
            .map(|item| index.query(item).expect("query"))
            .collect();
        drop(index);

        assert_eq!(split_key_metadata(&path, &split, 3), 17);
        let mut reopened = PersistentIndex32::open(&split).expect("open split snapshot");
        assert_eq!(reopened.len(), 50);
        for (item, candidates) in items.iter().zip(&expected) {
            assert_eq!(&reopened.query(item).expect("query"), candidates);
        }
        // A rewrite of the split snapshot stays readable and keeps every key.
        assert!(reopened.remove(0));
        reopened.sync().expect("rewrite");
        drop(reopened);
        let rewritten = PersistentIndex32::open(&split).expect("reopen rewrite");
        assert_eq!(rewritten.len(), 49);
        let without_removed: Vec<u64> = expected[1]
            .iter()
            .copied()
            .filter(|key| *key != 0)
            .collect();
        assert_eq!(rewritten.query(&items[1]).expect("query"), without_removed);
        cleanup(&path);
        cleanup(&split);
    }

    #[test]
    fn streamed_snapshot_bytes_match_in_memory_encoding() {
        for (name, signatures) in [("plain", false), ("signed", true)] {
            let path = test_path(&format!("stream-equal-{name}"));
            let options = super::PersistentIndexOptions::new().store_signatures(signatures);
            let mut index = PersistentIndex32::create_with_options(&path, 0.8, 128, 7, options)
                .expect("create");
            let items: Vec<_> = (0..300_u64)
                .map(|item| sketch((item / 3) * 50..(item / 3) * 50 + 40))
                .collect();
            index
                .insert_many(items.iter().zip(0_u64..).map(|(sketch, key)| (key, sketch)))
                .expect("insert");
            index.sync().expect("sync");
            assert!(index.remove(5));
            index.insert(9_000, &items[0]).expect("insert");
            let live = index.inner.collect_live_signatures().expect("signatures");
            let hashes = index.inner.collect_live_hashes().expect("hashes");
            let expected = index
                .inner
                .encode_snapshot(live.as_deref())
                .expect("encode");
            // Exercise one sort, several band batches, and hash partitions.
            // A limit below three also forces buckets larger than the sort buffer.
            for sort_limit in [super::SORT_RECORD_LIMIT, 512, 17, 2] {
                let mut streamed = std::io::Cursor::new(Vec::new());
                index
                    .inner
                    .write_snapshot_with(
                        &mut streamed,
                        &hashes,
                        live.as_deref(),
                        super::BUCKET_SEGMENT_TARGET_BYTES,
                        sort_limit,
                    )
                    .expect("stream");
                assert_eq!(
                    streamed.into_inner(),
                    expected,
                    "{name} snapshot bytes differ at sort limit {sort_limit}",
                );
            }
            cleanup(&path);
        }
    }

    #[test]
    fn streamed_snapshot_splits_buckets_into_many_segments() {
        let path = test_path("stream-segments");
        let split = test_path("stream-segments-out");
        let items: Vec<_> = (0..200_u64)
            .map(|item| sketch((item / 2) * 60..(item / 2) * 60 + 40))
            .collect();
        let mut index = PersistentIndex32::create(&path, 0.8, 128, 7).expect("create");
        index
            .insert_many(items.iter().zip(0_u64..).map(|(sketch, key)| (key, sketch)))
            .expect("insert");
        index.sync().expect("sync");
        let expected: Vec<_> = items
            .iter()
            .map(|item| index.query(item).expect("query"))
            .collect();
        let mut output = std::io::Cursor::new(Vec::new());
        let hashes = index.inner.collect_live_hashes().expect("hashes");
        index
            .inner
            .write_snapshot_with(&mut output, &hashes, None, 2_048, 17)
            .expect("stream small segments");
        drop(index);
        fs::write(&split, output.into_inner()).expect("write");
        let layout =
            FileLayout::read_from(&mut fs::File::open(&split).expect("open")).expect("layout");
        let segments = layout
            .sections()
            .iter()
            .filter(|section| section.kind() == SectionKind::Buckets)
            .count();
        assert!(
            segments > 10,
            "expected many bucket segments, got {segments}"
        );
        let reopened = PersistentIndex32::open(&split).expect("reopen");
        for (item, candidates) in items.iter().zip(&expected) {
            assert_eq!(&reopened.query(item).expect("query"), candidates);
        }
        cleanup(&path);
        cleanup(&split);
    }

    #[test]
    fn empty_index_streams_one_empty_bucket_section() {
        let path = test_path("stream-empty");
        let index = PersistentIndex32::create(&path, 0.8, 128, 7).expect("create");
        let expected = index.inner.encode_snapshot(None).expect("encode");
        let mut streamed = std::io::Cursor::new(Vec::new());
        index
            .inner
            .write_snapshot(&mut streamed, &BTreeMap::new(), None)
            .expect("stream");
        assert_eq!(streamed.into_inner(), expected);
        cleanup(&path);
    }

    #[test]
    fn default_target_keeps_large_catalogs_under_the_section_limit() {
        // 1M rows x 40 bands is 320 MB of band hashes, which a single section
        // cannot hold (the container limit is 256 MiB).
        let bands = 40;
        let key_hashes: BTreeMap<u64, Vec<u64>> = (0..1_000_000_u64)
            .map(|key| (key, vec![key; bands]))
            .collect();
        let mut sections = Vec::new();
        append_key_metadata_sections(
            &mut sections,
            &key_hashes,
            bands,
            super::METADATA_SECTION_TARGET_BYTES,
        )
        .expect("metadata");
        assert_eq!(sections.len(), 10);
        assert!(sections
            .iter()
            .all(|section| section.payload().len() <= super::METADATA_SECTION_TARGET_BYTES));
        let metadata = IndexMetadata::new(
            Algorithm::MinHashLsh,
            SignatureScheme::PariAffine32V1,
            CodecId::U64,
            128,
            7,
            0.8,
            u32::try_from(bands).expect("bands"),
            1,
            0,
        )
        .expect("metadata");
        IndexFile::new(metadata, sections)
            .expect("file")
            .encode()
            .expect("split metadata fits the container limits");
    }

    #[test]
    fn small_catalogs_keep_the_single_pair_layout() {
        let key_hashes: BTreeMap<u64, Vec<u64>> =
            (0..10_u64).map(|key| (key, vec![key; 4])).collect();
        let mut sections = Vec::new();
        append_key_metadata_sections(
            &mut sections,
            &key_hashes,
            4,
            super::METADATA_SECTION_TARGET_BYTES,
        )
        .expect("metadata");
        assert_eq!(
            sections.iter().map(Section::kind).collect::<Vec<_>>(),
            vec![SectionKind::Keys, SectionKind::BandHashes]
        );
        assert_eq!(
            sections[0].payload(),
            encode_keys(key_hashes.keys().copied()).expect("keys")
        );
        assert_eq!(
            sections[1].payload(),
            encode_band_hashes(key_hashes.values(), 4).expect("hashes")
        );
        let mut empty = Vec::new();
        append_key_metadata_sections(&mut empty, &BTreeMap::new(), 4, 64).expect("empty");
        assert_eq!(empty.len(), 2);
    }

    #[test]
    fn unpaired_or_misaligned_key_metadata_is_rejected() {
        let path = test_path("unpaired-source");
        let mut index = PersistentIndex32::create(&path, 0.8, 128, 7).expect("create");
        index.insert(1, &sketch(0..40)).expect("insert");
        index.insert(2, &sketch(40..80)).expect("insert");
        index.sync().expect("sync");
        drop(index);
        let file = IndexFile::decode(&fs::read(&path).expect("snapshot")).expect("decode");
        let bands = usize::try_from(file.metadata().bands()).expect("bands");
        let others: Vec<_> = file
            .sections()
            .iter()
            .filter(|section| {
                !matches!(section.kind(), SectionKind::Keys | SectionKind::BandHashes)
            })
            .cloned()
            .collect();
        let keys = |values: &[u64]| {
            Section::new(
                SectionKind::Keys,
                true,
                encode_keys(values.iter().copied()).expect("keys"),
            )
            .expect("section")
        };
        let hashes = |rows: usize| {
            let rows = vec![vec![0_u64; bands]; rows];
            Section::new(
                SectionKind::BandHashes,
                true,
                encode_band_hashes(rows.iter(), bands).expect("hashes"),
            )
            .expect("section")
        };
        let open_with = |mut sections: Vec<Section>, name: &str| {
            sections.extend(others.iter().cloned());
            let output = test_path(name);
            let encoded = IndexFile::new(file.metadata().clone(), sections)
                .expect("file")
                .encode()
                .expect("encode");
            fs::write(&output, encoded).expect("write");
            let result = PersistentIndex32::open(&output).map(|_| ());
            cleanup(&output);
            result
        };

        assert!(matches!(
            open_with(vec![keys(&[1]), keys(&[2]), hashes(2)], "unpaired"),
            Err(StoreError::InvalidSnapshot {
                reason: "keys and band-hash sections are not paired"
            })
        ));
        assert!(matches!(
            open_with(
                vec![keys(&[1, 2]), keys(&[]), hashes(1), hashes(1)],
                "misaligned"
            ),
            Err(StoreError::InvalidSnapshot {
                reason: "key and band-hash record counts differ"
            })
        ));
        cleanup(&path);
    }

    fn sketch(values: impl IntoIterator<Item = u64>) -> MinHash32 {
        let mut sketch = MinHash32::new(128, 7).expect("valid test sketch");
        for value in values {
            sketch.update(&value.to_le_bytes());
        }
        sketch
    }

    fn sketch64(values: impl IntoIterator<Item = u64>) -> MinHash64 {
        let mut sketch = MinHash64::new(128, 7).expect("valid 64-bit test sketch");
        for value in values {
            sketch.update(&value.to_le_bytes());
        }
        sketch
    }

    static NEXT_TEST_PATH: AtomicU64 = AtomicU64::new(0);

    fn test_path(name: &str) -> PathBuf {
        std::env::temp_dir().join(format!(
            "pari-store-{name}-{}-{}.pari",
            std::process::id(),
            NEXT_TEST_PATH.fetch_add(1, Ordering::Relaxed)
        ))
    }

    fn cleanup(path: &PathBuf) {
        let _ = fs::remove_file(path);
        let _ = fs::remove_file(legacy_temporary_path(path));
        if let Ok(lock) = commit_lock_path(path) {
            let _ = fs::remove_file(lock);
        }
    }

    fn test_directory(name: &str) -> PathBuf {
        let path = test_path(name);
        fs::create_dir(&path).expect("create isolated test directory");
        path
    }

    fn transaction_artifacts(directory: &Path) -> Vec<PathBuf> {
        fs::read_dir(directory)
            .expect("read test directory")
            .map(|entry| entry.expect("read directory entry"))
            .filter(|entry| {
                entry
                    .file_name()
                    .to_string_lossy()
                    .starts_with(".pari-tmp-")
            })
            .map(|entry| entry.path())
            .collect()
    }

    fn assert_already_exists(error: StoreError) {
        match error {
            StoreError::Io(error) => assert_eq!(error.kind(), io::ErrorKind::AlreadyExists),
            other => panic!("expected an AlreadyExists I/O error, got {other}"),
        }
    }

    fn legacy_temporary_path(path: &Path) -> PathBuf {
        let mut temporary = path.as_os_str().to_os_string();
        temporary.push(".tmp");
        PathBuf::from(temporary)
    }

    fn phase1_snapshot(path: &PathBuf, rows: &[(u64, MinHash32)]) {
        let reference = LshIndex32::new(0.8, 128, 7).expect("reference");
        let params = reference.params();
        let helper = PersistentIndex32 {
            inner: super::PersistentIndexCore::empty(
                path.clone(),
                0.8,
                128,
                7,
                params,
                SignatureScheme::PariAffine32V1,
            ),
        };
        let mut hashes = BTreeMap::new();
        for (key, sketch) in rows {
            hashes.insert(*key, helper.band_hashes(sketch).expect("band hashes"));
        }
        let metadata = IndexMetadata::new(
            Algorithm::MinHashLsh,
            SignatureScheme::PariAffine32V1,
            CodecId::U64,
            128,
            7,
            0.8,
            u32::try_from(params.bands).expect("bands"),
            u32::try_from(params.rows).expect("rows"),
            0,
        )
        .expect("metadata");
        let file = IndexFile::new(
            metadata,
            vec![
                Section::new(
                    SectionKind::Keys,
                    true,
                    encode_keys(hashes.keys().copied()).expect("keys"),
                )
                .expect("keys section"),
                Section::new(
                    SectionKind::BandHashes,
                    true,
                    encode_band_hashes(hashes.values(), params.bands).expect("hashes"),
                )
                .expect("hash section"),
            ],
        )
        .expect("phase1 file");
        fs::write(path, file.encode().expect("encode phase1")).expect("write phase1");
    }

    fn assert_query_parity(store: &PersistentIndex32, memory: &LshIndex32, queries: &[&MinHash32]) {
        for query in queries {
            assert_eq!(
                store.query(query).expect("persistent query"),
                memory.query(query).expect("memory query")
            );
        }
    }

    macro_rules! query_observation_survives_commits {
        ($test:ident, $index:ty, $sketch:ident) => {
            #[test]
            fn $test() {
                let directory = test_directory(stringify!($test));
                let path = directory.join("index.pari");
                let value = $sketch(0..40);
                let mut store = <$index>::create(&path, 0.8, 128, 7).expect("create");
                store.set_observability(true);
                store.insert(1, &value).expect("insert");
                store.query(&value).expect("query overlay");
                let before_flush = store.stats().expect("stats").queries;
                assert_eq!(before_flush.expect("enabled").queries, 1);

                store.flush().expect("flush dirty snapshot");
                assert_eq!(store.stats().expect("stats").queries, before_flush);
                store
                    .query_many([&value, &value])
                    .expect("query committed data");
                let before_sync = store.stats().expect("stats").queries;
                let observed = before_sync.expect("still enabled");
                assert_eq!(observed.operations, 2);
                assert_eq!(observed.queries, 3);
                assert_eq!(observed.candidates, 3);

                store.insert(2, &value).expect("make dirty");
                store.sync().expect("sync dirty snapshot");
                assert_eq!(store.stats().expect("stats").queries, before_sync);
                store.flush().expect("clean flush");
                store.sync().expect("clean sync");
                assert_eq!(store.stats().expect("stats").queries, before_sync);
                assert_eq!(
                    store.query(&value).expect("continued observation"),
                    vec![1, 2]
                );
                let continued = store.stats().expect("stats").queries.expect("enabled");
                assert_eq!(continued.queries, 4);
                assert_eq!(continued.candidates, 5);

                store.set_observability(true);
                assert_eq!(
                    store.stats().expect("stats").queries,
                    Some(Default::default())
                );
                store.set_observability(false);
                store.remove(2);
                store.sync().expect("sync while disabled");
                assert!(store.stats().expect("stats").queries.is_none());
                drop(store);
                let reopened = <$index>::open(&path).expect("explicit reopen");
                assert!(reopened.stats().expect("stats").queries.is_none());
                drop(reopened);
                fs::remove_dir_all(directory).expect("cleanup");
            }
        };
    }

    query_observation_survives_commits!(
        affine32_query_metrics_survive_commits,
        PersistentIndex32,
        sketch
    );
    query_observation_survives_commits!(
        affine64_query_metrics_survive_commits,
        PersistentIndex64,
        sketch64
    );

    #[test]
    fn query_metrics_survive_refresh_errors_and_retry() {
        let directory = test_directory("query-metrics-refresh-error");
        let path = directory.join("index.pari");
        let value = sketch(0..40);
        let mut store = PersistentIndex32::create_with_options(
            &path,
            0.8,
            128,
            7,
            super::PersistentIndexOptions::new().store_signatures(true),
        )
        .expect("create");
        store.insert(1, &value).expect("insert");
        store.sync().expect("commit");
        store.set_observability(true);
        store.query(&value).expect("observe query");
        let observed = store.stats().expect("stats").queries;

        let hashes = store.inner.collect_live_hashes().expect("live hashes");
        store
            .inner
            .refresh_after_commit(
                Some(StoreError::Io(io::Error::other("directory sync failed"))),
                hashes,
                None,
            )
            .expect_err("durability error must propagate");
        assert!(store.stats().expect("stats").dirty);
        assert_eq!(store.stats().expect("stats").queries, observed);
        store.sync().expect("retry durability");
        assert_eq!(store.stats().expect("stats").queries, observed);

        let pending = sketch(30..64);
        store.insert(2, &pending).expect("insert pending key");
        let signatures = store
            .inner
            .collect_live_signatures()
            .expect("live signatures");
        let hashes = store.inner.collect_live_hashes().expect("live hashes");
        store.inner.committed = None;
        store.inner.path = directory.join("missing.pari");
        store
            .inner
            .refresh_after_commit(None, hashes, signatures.clone())
            .expect_err("reopen failure");
        assert_eq!(store.stats().expect("stats").queries, observed);
        assert_eq!(
            store
                .inner
                .collect_live_signatures()
                .expect("recovered signatures"),
            signatures
        );
        store.inner.path = path;
        store.sync().expect("retry reopen");
        assert_eq!(store.stats().expect("stats").queries, observed);
        assert!(store.query(&value).expect("query after retry").contains(&1));
        assert_eq!(
            store
                .stats()
                .expect("stats")
                .queries
                .expect("enabled")
                .queries,
            2
        );
        drop(store);
        fs::remove_dir_all(directory).expect("cleanup");
    }

    #[test]
    fn committed_snapshot_reopens_lazily_with_identical_queries_and_deletions() {
        let path = test_path("reopen");
        cleanup(&path);
        let first = sketch(0..40);
        let second = sketch(0..35);
        let third = sketch(100..140);

        let mut store = PersistentIndex32::create(&path, 0.8, 128, 7).expect("create store");
        store
            .insert_many([(10, &first), (20, &second), (30, &third)])
            .expect("insert batch");
        let before = store.query(&first).expect("query before sync");
        store.sync().expect("sync snapshot");
        store.close().expect("close store");

        let mut reopened = PersistentIndex32::open(&path).expect("reopen snapshot");
        assert!(reopened.inner.committed.is_some());
        assert!(reopened
            .inner
            .overlay_buckets
            .iter()
            .all(|table| table.len() == 0));
        let stats = reopened.stats().expect("stats");
        assert!(stats.committed_buckets > 0);
        assert!(stats.committed_distribution.memberships > 0);
        let explanation = reopened.explain().expect("explain");
        assert_eq!(explanation.expected_items, 3);
        assert_eq!(explanation.parameter_source.as_str(), "existing");
        assert_eq!(explanation.requested_storage.as_str(), "persistent");
        assert_eq!(
            stats.committed_distribution.buckets,
            u64::try_from(stats.committed_buckets).expect("small bucket count")
        );
        assert!(stats.queries.is_none());
        reopened.set_observability(true);
        assert_eq!(reopened.query(&first).expect("query reopened"), before);
        let observed = reopened
            .stats()
            .expect("observed stats")
            .queries
            .expect("query metrics");
        assert_eq!(observed.operations, 1);
        assert_eq!(observed.queries, 1);
        assert!(observed.candidates > 0);
        assert!(observed.total_latency_ns > 0);
        assert!(reopened.remove(20));
        reopened.sync().expect("sync deletion");
        drop(reopened);

        let reopened = PersistentIndex32::open(&path).expect("reopen deletion");
        assert!(!reopened.contains(20));
        assert_eq!(reopened.len(), 2);
        cleanup(&path);
    }

    #[test]
    fn mutation_overlay_matches_reference_before_and_after_sync() {
        let path = test_path("overlay");
        cleanup(&path);
        let first = sketch(0..40);
        let second = sketch(0..35);
        let third = sketch(100..140);
        let fourth = sketch(0..30);

        let mut store = PersistentIndex32::create(&path, 0.8, 128, 7).expect("create");
        let mut memory = LshIndex32::new(0.8, 128, 7).expect("memory");
        for (key, value) in [(10, &first), (20, &second), (30, &third)] {
            store.insert(key, value).expect("insert store");
            memory.insert(key, value).expect("insert memory");
        }
        store.sync().expect("initial sync");
        store.insert(40, &fourth).expect("overlay insert");
        memory.insert(40, &fourth).expect("memory insert");
        assert!(store.remove(20));
        assert!(memory.remove(20));
        assert_query_parity(&store, &memory, &[&first, &second, &third, &fourth]);
        store.sync().expect("overlay sync");
        drop(store);
        let reopened = PersistentIndex32::open(&path).expect("reopen");
        assert_query_parity(&reopened, &memory, &[&first, &second, &third, &fourth]);
        cleanup(&path);
    }

    #[test]
    fn remove_then_reinsert_same_key_suppresses_old_committed_generation() {
        let path = test_path("reinsert-generation");
        cleanup(&path);
        let old_value = sketch(0..40);
        let new_value = sketch(10_000..10_040);

        let mut store = PersistentIndex32::create(&path, 0.8, 128, 7).expect("create");
        let mut memory = LshIndex32::new(0.8, 128, 7).expect("memory");
        store.insert(20, &old_value).expect("store old insert");
        memory.insert(20, &old_value).expect("memory old insert");
        store.sync().expect("commit old generation");

        assert!(store.remove(20));
        assert!(memory.remove(20));
        store.insert(20, &new_value).expect("store new insert");
        memory.insert(20, &new_value).expect("memory new insert");
        assert_eq!(store.stats().expect("stats").suppressed_base_keys, 1);
        assert_query_parity(&store, &memory, &[&old_value, &new_value]);

        store.sync().expect("compact generation");
        assert_eq!(store.stats().expect("stats").suppressed_base_keys, 0);
        assert_query_parity(&store, &memory, &[&old_value, &new_value]);
        cleanup(&path);
    }

    #[test]
    fn phase1_snapshot_opens_and_upgrades_on_sync() {
        let path = test_path("phase1-upgrade");
        cleanup(&path);
        let first = sketch(0..40);
        let second = sketch(0..35);
        phase1_snapshot(&path, &[(10, first.clone()), (20, second.clone())]);

        let mut legacy = PersistentIndex32::open(&path).expect("open phase1");
        assert!(legacy.inner.committed.is_none());
        assert!(legacy.stats().expect("legacy stats").dirty);
        assert!(legacy.query(&first).expect("legacy query").contains(&10));
        legacy.sync().expect("upgrade sync");
        drop(legacy);

        let upgraded = PersistentIndex32::open(&path).expect("open upgraded");
        assert!(upgraded.inner.committed.is_some());
        assert!(!upgraded.stats().expect("upgraded stats").dirty);
        let mut reference = LshIndex32::new(0.8, 128, 7).expect("reference");
        reference.insert(10, &first).expect("reference first");
        reference.insert(20, &second).expect("reference second");
        assert_eq!(
            upgraded.query(&first).expect("upgraded query"),
            reference.query(&first).expect("reference query")
        );
        cleanup(&path);
    }

    #[test]
    fn stale_or_partial_temporary_file_never_replaces_committed_state() {
        let path = test_path("partial-temp");
        cleanup(&path);
        let first = sketch(0..40);
        let mut store = PersistentIndex32::create(&path, 0.8, 128, 7).expect("create store");
        store.insert(10, &first).expect("insert");
        store.sync().expect("sync committed snapshot");
        drop(store);

        let temporary = legacy_temporary_path(&path);
        fs::write(&temporary, b"partial and uncommitted").expect("write stale temp");

        let reopened = PersistentIndex32::open(&path).expect("committed target stays valid");
        assert!(reopened.contains(10));
        cleanup(&path);
    }

    #[test]
    fn temporary_allocations_are_unique_and_owned() {
        let directory = test_directory("unique-temporaries");
        let target = directory.join("index.pari");
        let first = OwnedTemporaryFile::allocate(&target).expect("allocate first temporary");
        let second = OwnedTemporaryFile::allocate(&target).expect("allocate second temporary");
        let first_path = first.path.clone();
        let second_path = second.path.clone();

        assert_ne!(first_path, second_path);
        assert!(first_path.exists());
        assert!(second_path.exists());
        drop(first);
        drop(second);
        assert!(!first_path.exists());
        assert!(!second_path.exists());
        fs::remove_dir_all(directory).expect("remove test directory");
    }

    #[test]
    fn temporary_cleanup_preserves_a_concurrent_replacement() {
        let directory = test_directory("temporary-replacement");
        let target = directory.join("index.pari");
        let mut temporary =
            OwnedTemporaryFile::allocate(&target).expect("allocate transaction temporary");
        let temporary_path = temporary.path.clone();
        temporary.file.take();
        fs::remove_file(&temporary_path).expect("remove transaction-owned path");
        fs::write(&temporary_path, b"concurrent owner").expect("write replacement");

        drop(temporary);

        assert_eq!(
            fs::read(&temporary_path).expect("read replacement"),
            b"concurrent owner"
        );
        fs::remove_dir_all(directory).expect("remove test directory");
    }

    #[test]
    fn failed_publication_removes_owned_temporary() {
        let directory = test_directory("failed-temporary-publication");
        let target = directory.join("target-directory");
        fs::create_dir(&target).expect("create invalid file target");
        let mut temporary =
            OwnedTemporaryFile::allocate(&target).expect("allocate transaction file");
        let temporary_path = temporary.path.clone();
        temporary
            .write_synced(b"complete candidate")
            .expect("write candidate");

        temporary
            .publish_replace(&target)
            .expect_err("cannot replace a directory with a file");

        assert!(!temporary_path.exists());
        fs::remove_dir_all(directory).expect("remove test directory");
    }

    #[test]
    fn initial_publication_cleanup_rolls_back_only_owned_paths() {
        let directory = test_directory("initial-publication-cleanup");
        let rollback_target = directory.join("rollback.pari");
        let mut rollback_temporary =
            OwnedTemporaryFile::allocate(&rollback_target).expect("allocate rollback temporary");
        let rollback_temporary_path = rollback_temporary.path.clone();
        rollback_temporary
            .write_synced(b"complete initial snapshot")
            .expect("write rollback temporary");

        let error = rollback_temporary
            .publish_no_replace_with(
                &rollback_target,
                |_| Err(io::Error::other("forced temporary cleanup failure")),
                |_| Ok(()),
            )
            .expect_err("cleanup failure must fail publication");
        assert!(error
            .to_string()
            .contains("forced temporary cleanup failure"));
        assert!(!rollback_target.exists());
        assert!(!rollback_temporary_path.exists());

        let replacement_target = directory.join("replacement.pari");
        let mut replacement_temporary = OwnedTemporaryFile::allocate(&replacement_target)
            .expect("allocate replacement temporary");
        let replacement_temporary_path = replacement_temporary.path.clone();
        replacement_temporary
            .write_synced(b"complete initial snapshot")
            .expect("write replacement temporary");
        let competing = b"concurrent replacement";

        let error = replacement_temporary
            .publish_no_replace_with(
                &replacement_target,
                |path| fs::remove_file(path),
                |published| {
                    fs::remove_file(published)?;
                    fs::write(published, competing)?;
                    Err(io::Error::other("forced directory sync failure"))
                },
            )
            .expect_err("directory sync failure must fail publication");
        assert!(error.to_string().contains("forced directory sync failure"));
        assert_eq!(
            fs::read(&replacement_target).expect("read concurrent replacement"),
            competing
        );
        assert!(!replacement_temporary_path.exists());
        assert!(transaction_artifacts(&directory).is_empty());
        fs::remove_dir_all(directory).expect("remove test directory");
    }

    #[cfg(unix)]
    #[test]
    fn initial_recovery_preserves_a_concurrent_destination_symlink() {
        use std::os::unix::fs::symlink;

        let directory = test_directory("initial-publication-symlink-replacement");
        let destination = directory.join("destination.pari");
        let preserved_inode = directory.join("preserved-inode.pari");
        let mut temporary =
            OwnedTemporaryFile::allocate(&destination).expect("allocate transaction temporary");
        temporary
            .write_synced(b"complete initial snapshot")
            .expect("write transaction temporary");

        temporary
            .publish_no_replace_with(
                &destination,
                |path| fs::remove_file(path),
                |published| {
                    fs::hard_link(published, &preserved_inode)?;
                    fs::remove_file(published)?;
                    symlink(&preserved_inode, published)?;
                    Err(io::Error::other("forced directory sync failure"))
                },
            )
            .expect_err("directory sync failure must fail publication");

        assert!(fs::symlink_metadata(&destination)
            .expect("inspect replacement symlink")
            .file_type()
            .is_symlink());
        assert_eq!(
            fs::read(&destination).expect("read replacement symlink target"),
            b"complete initial snapshot"
        );
        assert!(transaction_artifacts(&directory).is_empty());
        fs::remove_dir_all(directory).expect("remove test directory");
    }

    #[test]
    fn create_preserves_preexisting_final_names_for_both_widths() {
        let directory = test_directory("preexisting-create-destinations");
        let path32 = directory.join("affine32.pari");
        let path64 = directory.join("affine64.pari");
        let existing32 = b"existing affine32 owner";
        let existing64 = b"existing affine64 owner";
        fs::write(&path32, existing32).expect("write affine32 destination");
        fs::write(&path64, existing64).expect("write affine64 destination");

        assert_already_exists(
            PersistentIndex32::create(&path32, 0.8, 128, 7)
                .expect_err("affine32 create must not replace"),
        );
        assert_already_exists(
            PersistentIndex64::create(&path64, 0.8, 128, 7)
                .expect_err("affine64 create must not replace"),
        );
        assert_eq!(fs::read(&path32).expect("read affine32 owner"), existing32);
        assert_eq!(fs::read(&path64).expect("read affine64 owner"), existing64);
        assert!(transaction_artifacts(&directory).is_empty());
        fs::remove_dir_all(directory).expect("remove test directory");
    }

    #[cfg(unix)]
    #[test]
    fn create_preserves_a_broken_destination_symlink() {
        use std::os::unix::fs::symlink;

        let directory = test_directory("broken-create-symlink");
        let destination = directory.join("index.pari");
        let missing = directory.join("missing-target");
        symlink(&missing, &destination).expect("create broken destination symlink");
        assert!(!destination.exists(), "test symlink must be broken");

        assert_already_exists(
            PersistentIndex32::create(&destination, 0.8, 128, 7)
                .expect_err("create must preserve a broken symlink"),
        );
        assert!(fs::symlink_metadata(&destination)
            .expect("inspect destination symlink")
            .file_type()
            .is_symlink());
        assert!(transaction_artifacts(&directory).is_empty());
        fs::remove_dir_all(directory).expect("remove test directory");
    }

    #[test]
    fn concurrent_destination_wins_initial_publication() {
        let directory = test_directory("concurrent-create-destination");
        let destination = directory.join("index.pari");
        let competing = b"concurrently created destination";
        let params = LshIndex32::new(0.8, 128, 7)
            .expect("valid parameters")
            .params();

        let error = PersistentIndexCore::create_with_params_and_hook(
            &destination,
            0.8,
            128,
            7,
            params,
            SignatureScheme::PariAffine32V1,
            false,
            || {
                fs::write(&destination, competing)?;
                Ok(())
            },
        )
        .expect_err("concurrent destination must win");

        assert_already_exists(error);
        assert_eq!(
            fs::read(&destination).expect("read concurrent destination"),
            competing
        );
        assert!(transaction_artifacts(&directory).is_empty());
        fs::remove_dir_all(directory).expect("remove test directory");
    }

    #[test]
    fn commits_never_modify_the_legacy_temporary_path() {
        let directory = test_directory("legacy-temporary-sentinel");
        let target = directory.join("index.pari");
        let legacy_temporary = legacy_temporary_path(&target);
        let sentinel = b"unrelated sentinel data";
        fs::write(&legacy_temporary, sentinel).expect("write sentinel");

        let mut store =
            PersistentIndex32::create(&target, 0.8, 128, 7).expect("create persistent index");
        store.insert(10, &sketch(0..40)).expect("insert item");
        store.sync().expect("commit generation");
        drop(store);

        assert_eq!(
            fs::read(&legacy_temporary).expect("read sentinel"),
            sentinel
        );
        assert!(transaction_artifacts(&directory).is_empty());
        fs::remove_dir_all(directory).expect("remove test directory");
    }

    #[cfg(unix)]
    #[test]
    fn commits_never_follow_the_legacy_temporary_symlink() {
        use std::os::unix::fs::symlink;

        let directory = test_directory("legacy-temporary-symlink");
        let target = directory.join("index.pari");
        let sentinel = directory.join("sentinel");
        let legacy_temporary = legacy_temporary_path(&target);
        fs::write(&sentinel, b"must remain intact").expect("write sentinel");
        symlink(&sentinel, &legacy_temporary).expect("create legacy temporary symlink");

        let mut store =
            PersistentIndex32::create(&target, 0.8, 128, 7).expect("create persistent index");
        store.insert(10, &sketch(0..40)).expect("insert item");
        store.sync().expect("commit generation");
        drop(store);

        assert_eq!(
            fs::read(&sentinel).expect("read sentinel"),
            b"must remain intact"
        );
        assert!(fs::symlink_metadata(&legacy_temporary)
            .expect("inspect legacy temporary")
            .file_type()
            .is_symlink());
        assert!(transaction_artifacts(&directory).is_empty());
        fs::remove_dir_all(directory).expect("remove test directory");
    }

    #[test]
    fn successful_affine_commits_leave_no_transaction_artifacts() {
        let directory = test_directory("successful-temporary-cleanup");
        let path32 = directory.join("affine32.pari");
        let path64 = directory.join("affine64.pari");

        let mut store32 =
            PersistentIndex32::create(&path32, 0.8, 128, 7).expect("create affine32 index");
        store32.insert(10, &sketch(0..40)).expect("insert affine32");
        store32.sync().expect("commit affine32");
        drop(store32);

        let mut store64 =
            PersistentIndex64::create(&path64, 0.8, 128, 7).expect("create affine64 index");
        store64
            .insert(20, &sketch64(10_000..10_040))
            .expect("insert affine64");
        store64.sync().expect("commit affine64");
        drop(store64);

        let reopened32 = PersistentIndex32::open(&path32).expect("reopen affine32 index");
        let reopened64 = PersistentIndex64::open(&path64).expect("reopen affine64 index");
        assert!(reopened32.contains(10));
        assert!(reopened64.contains(20));
        drop(reopened32);
        drop(reopened64);
        assert!(transaction_artifacts(&directory).is_empty());
        fs::remove_dir_all(directory).expect("remove test directory");
    }

    #[test]
    fn corrupt_committed_header_fails_explicitly() {
        let path = test_path("corrupt-header");
        cleanup(&path);
        let mut store = PersistentIndex32::create(&path, 0.8, 128, 7).expect("create store");
        store.insert(10, &sketch(0..40)).expect("insert");
        store.sync().expect("sync snapshot");
        drop(store);

        let mut bytes = fs::read(&path).expect("read snapshot");
        bytes[20] ^= 0xFF;
        fs::write(&path, bytes).expect("write corruption");
        assert!(PersistentIndex32::open(&path).is_err());
        cleanup(&path);
    }

    #[test]
    fn corrupt_bucket_directory_fails_during_open() {
        let path = test_path("corrupt-directory");
        cleanup(&path);
        let mut store = PersistentIndex32::create(&path, 0.8, 128, 7).expect("create");
        store.insert(10, &sketch(0..40)).expect("insert");
        store.sync().expect("sync");
        drop(store);

        let mut file = OpenOptions::new()
            .read(true)
            .write(true)
            .open(&path)
            .expect("open file");
        let layout = FileLayout::read_from(&mut file).expect("layout");
        let buckets = layout
            .sections()
            .iter()
            .copied()
            .find(|section| section.kind() == SectionKind::Buckets)
            .expect("bucket section");
        file.seek(SeekFrom::Start(buckets.payload_offset() + 28))
            .expect("seek checksum");
        file.write_all(&[0xFF]).expect("corrupt directory checksum");
        file.sync_all().expect("sync corruption");
        drop(file);

        assert!(matches!(
            PersistentIndex32::open(&path),
            Err(StoreError::Bucket(
                BucketError::DirectoryChecksumMismatch { .. }
            ))
        ));
        cleanup(&path);
    }

    #[test]
    fn corrupt_bucket_members_fail_when_bucket_is_queried() {
        let path = test_path("corrupt-members");
        cleanup(&path);
        let query = sketch(0..40);
        let mut store = PersistentIndex32::create(&path, 0.8, 128, 7).expect("create");
        store.insert(10, &query).expect("insert");
        store.sync().expect("sync");
        let location = {
            let base = store.inner.committed.as_ref().expect("committed file");
            let hash = store.inner.collect_live_hashes().expect("hashes")[&10][0];
            let mut file = base.file.lock().expect("committed file");
            base.generations[0]
                .directory
                .find(
                    &base.layout,
                    &mut *file,
                    pari_format::BucketKey::new(0, hash),
                )
                .expect("read directory block")
                .expect("bucket location")
        };
        drop(store);

        let absolute = location
            .section()
            .payload_offset()
            .checked_add(location.member_offset())
            .expect("absolute member offset");
        let mut file = OpenOptions::new()
            .read(true)
            .write(true)
            .open(&path)
            .expect("open file");
        file.seek(SeekFrom::Start(absolute)).expect("seek member");
        file.write_all(&[0xFF]).expect("corrupt member");
        file.sync_all().expect("sync corruption");
        drop(file);

        let reopened = PersistentIndex32::open(&path).expect("directory still opens");
        assert!(matches!(
            reopened.query(&query),
            Err(StoreError::Bucket(
                BucketError::MemberChecksumMismatch { .. }
            ))
        ));
        cleanup(&path);
    }

    #[test]
    fn candidates_match_in_memory_reference_index() {
        let path = test_path("parity");
        cleanup(&path);
        let sketches = [
            sketch(0..40),
            sketch(0..35),
            sketch(100..140),
            sketch(0..30),
        ];
        let mut persistent =
            PersistentIndex32::create(&path, 0.8, 128, 7).expect("create persistent");
        let mut memory = LshIndex32::new(0.8, 128, 7).expect("create memory index");
        for (key, sketch) in sketches.iter().enumerate() {
            let key = u64::try_from(key).expect("small key");
            persistent.insert(key, sketch).expect("persistent insert");
            memory.insert(key, sketch).expect("memory insert");
        }
        persistent.sync().expect("persist lazy layout");
        assert_query_parity(
            &persistent,
            &memory,
            &[&sketches[0], &sketches[1], &sketches[2], &sketches[3]],
        );
        cleanup(&path);
    }

    #[test]
    fn duplicate_batch_is_rejected_without_partial_mutation() {
        let path = test_path("duplicate");
        cleanup(&path);
        let first = sketch(0..40);
        let second = sketch(100..140);
        let mut store = PersistentIndex32::create(&path, 0.8, 128, 7).expect("create store");
        let error = store
            .insert_many([(10, &first), (10, &second)])
            .expect_err("duplicate must fail");
        assert!(matches!(error, StoreError::DuplicateKey { key: 10 }));
        assert!(store.is_empty());
        cleanup(&path);
    }

    #[test]
    fn empty_lazy_snapshot_has_a_valid_bucket_segment() {
        let path = test_path("empty-lazy");
        cleanup(&path);
        let store = PersistentIndex32::create(&path, 0.8, 128, 7).expect("create");
        let stats = store.stats().expect("stats");
        assert_eq!(stats.items, 0);
        assert_eq!(stats.committed_buckets, 0);
        assert!(store.inner.committed.is_some());
        cleanup(&path);
    }

    #[test]
    fn bucket_segment_header_size_constant_matches_fixture_contract() {
        assert_eq!(BUCKET_SEGMENT_HEADER_BYTES, 40);
        let params = LshParams::new(32, 4);
        assert_eq!(params.used_permutations(), Some(128));
    }

    #[test]
    fn affine64_batches_queries_removals_and_reopen_match_memory() {
        let path = test_path("affine64-parity");
        cleanup(&path);
        let sketches = [
            sketch64(0..40),
            sketch64(0..35),
            sketch64(100..140),
            sketch64(0..30),
        ];
        let mut persistent =
            PersistentIndex64::create(&path, 0.8, 128, 7).expect("create affine64 store");
        let mut memory = LshIndex64::new(0.8, 128, 7).expect("create affine64 memory index");
        persistent
            .insert_many(
                sketches
                    .iter()
                    .enumerate()
                    .map(|(key, value)| (u64::try_from(key).expect("small key"), value)),
            )
            .expect("insert affine64 batch");
        memory
            .insert_many(
                sketches
                    .iter()
                    .enumerate()
                    .map(|(key, value)| (u64::try_from(key).expect("small key"), value)),
            )
            .expect("insert affine64 memory batch");

        let expected = memory
            .query_many(sketches.iter())
            .expect("memory affine64 queries");
        assert_eq!(
            persistent
                .query_many(sketches.iter())
                .expect("persistent affine64 queries"),
            expected
        );
        assert_eq!(
            persistent
                .explain()
                .expect("explain")
                .sizes
                .signature_bytes_per_item,
            1_024
        );
        persistent.flush().expect("flush affine64 snapshot");
        assert!(persistent.remove(1));
        assert!(memory.remove(1));
        persistent.sync().expect("sync affine64 removal");
        drop(persistent);

        let reopened = PersistentIndex64::open(&path).expect("reopen affine64 snapshot");
        assert_eq!(reopened.len(), 3);
        assert!(!reopened.contains(1));
        for query in &sketches {
            assert_eq!(
                reopened.query(query).expect("reopened affine64 query"),
                memory.query(query).expect("memory affine64 query")
            );
        }
        reopened.close().expect("close affine64 snapshot");
        cleanup(&path);
    }

    #[test]
    fn affine64_batch_failures_are_atomic() {
        let path = test_path("affine64-atomic");
        cleanup(&path);
        let valid = sketch64(0..40);
        let duplicate = sketch64(100..140);
        let mut wrong_seed = MinHash64::new(128, 99).expect("wrong-seed sketch");
        wrong_seed.update(b"wrong seed");
        let mut store =
            PersistentIndex64::create(&path, 0.8, 128, 7).expect("create affine64 store");

        assert!(matches!(
            store.insert_many([(10, &valid), (10, &duplicate)]),
            Err(StoreError::DuplicateKey { key: 10 })
        ));
        assert!(store.is_empty());
        assert!(matches!(
            store.insert_many([(10, &valid), (20, &wrong_seed)]),
            Err(StoreError::IncompatibleSeed {
                expected: 7,
                actual: 99
            })
        ));
        assert!(store.is_empty());
        cleanup(&path);
    }

    #[test]
    fn persistent_types_reject_cross_width_snapshots() {
        let path32 = test_path("cross-width-32");
        let path64 = test_path("cross-width-64");
        cleanup(&path32);
        cleanup(&path64);
        PersistentIndex32::create(&path32, 0.8, 128, 7)
            .expect("create affine32")
            .close()
            .expect("close affine32");
        PersistentIndex64::create(&path64, 0.8, 128, 7)
            .expect("create affine64")
            .close()
            .expect("close affine64");

        assert!(matches!(
            PersistentIndex64::open(&path32),
            Err(StoreError::InvalidSnapshot { .. })
        ));
        assert!(matches!(
            PersistentIndex32::open(&path64),
            Err(StoreError::InvalidSnapshot { .. })
        ));
        cleanup(&path32);
        cleanup(&path64);
    }

    fn assert_concurrent_commit(error: StoreError) {
        match error {
            StoreError::Io(error) => {
                assert_eq!(error.kind(), io::ErrorKind::Other);
                assert!(
                    error.to_string().contains("committed by another handle"),
                    "unexpected message: {error}"
                );
            }
            other => panic!("expected a concurrent-commit error, got {other}"),
        }
    }

    #[test]
    fn commits_refuse_to_overwrite_another_handles_commit() {
        let path = test_path("concurrent-commit");
        cleanup(&path);
        PersistentIndex32::create(&path, 0.8, 128, 7)
            .expect("create")
            .close()
            .expect("close");
        let mut first = PersistentIndex32::open(&path).expect("open first");
        let mut second = PersistentIndex32::open(&path).expect("open second");
        let one = sketch(0..40);
        let two = sketch(100..140);

        first.insert(1, &one).expect("insert first");
        first.sync().expect("first commit wins");
        second.insert(2, &two).expect("insert second");
        assert_concurrent_commit(second.sync().expect_err("stale commit is refused"));
        // The refused handle keeps its changes and keeps refusing.
        assert!(second.contains(2));
        assert!(second.stats().expect("stats").dirty);
        assert_concurrent_commit(second.flush().expect_err("still stale"));
        drop(second);

        // The first handle may keep committing its own generations.
        first.remove(1);
        first.insert(3, &one).expect("insert again");
        first.sync().expect("second commit by the same handle");
        drop(first);

        let reopened = PersistentIndex32::open(&path).expect("reopen");
        assert!(reopened.contains(3));
        assert!(!reopened.contains(1));
        assert!(!reopened.contains(2));
        drop(reopened);
        cleanup(&path);
    }

    #[test]
    fn commits_after_reopening_see_the_latest_generation() {
        let path = test_path("sequential-commit");
        cleanup(&path);
        let mut writer = PersistentIndex64::create(&path, 0.8, 128, 7).expect("create");
        writer.insert(1, &sketch64(0..40)).expect("insert");
        writer.sync().expect("commit");
        let mut reader = PersistentIndex64::open(&path).expect("open after commit");
        writer.insert(2, &sketch64(50..90)).expect("insert");
        writer.close().expect("close commits");

        reader.insert(3, &sketch64(200..240)).expect("insert");
        assert_concurrent_commit(reader.close().expect_err("stale on close"));

        let mut fresh = PersistentIndex64::open(&path).expect("reopen");
        fresh.insert(3, &sketch64(200..240)).expect("insert");
        fresh.sync().expect("fresh handle commits");
        drop(fresh);
        let reopened = PersistentIndex64::open(&path).expect("reopen");
        assert_eq!(reopened.len(), 3);
        drop(reopened);
        cleanup(&path);
    }

    #[test]
    fn commits_wait_for_the_commit_lock_and_release_it() {
        use std::{sync::mpsc, thread, time::Duration};

        let path = test_path("commit-lock");
        cleanup(&path);
        let mut store = PersistentIndex32::create(&path, 0.8, 128, 7).expect("create");
        store.insert(1, &sketch(0..40)).expect("insert");

        let held = CommitLock::acquire(&path).expect("hold the commit lock");
        let (sender, receiver) = mpsc::channel();
        let worker = thread::spawn(move || {
            let result = store.sync();
            sender.send(result.is_ok()).expect("report");
            store
        });
        assert!(
            matches!(
                receiver.recv_timeout(Duration::from_millis(300)),
                Err(mpsc::RecvTimeoutError::Timeout)
            ),
            "commit must wait while another handle holds the lock"
        );
        drop(held);
        assert!(receiver
            .recv_timeout(Duration::from_secs(30))
            .expect("commit finishes after release"));
        let store = worker.join().expect("worker");
        assert!(store.contains(1));

        // The lock file is gone and the lock is immediately available again.
        let lock_path = commit_lock_path(&path).expect("lock path");
        assert!(!lock_path.exists(), "commits leave no lock file behind");
        drop(CommitLock::acquire(&path).expect("lock released after commit"));
        assert!(!lock_path.exists());
        drop(store);
        cleanup(&path);
    }

    #[test]
    fn commit_lock_is_mutually_exclusive_across_handles() {
        use std::{
            sync::{
                atomic::{AtomicBool, AtomicUsize},
                Arc,
            },
            thread,
        };

        let path = test_path("commit-lock-contention");
        cleanup(&path);
        let inside = Arc::new(AtomicBool::new(false));
        let entries = Arc::new(AtomicUsize::new(0));
        let workers: Vec<_> = (0..8)
            .map(|_| {
                let (path, inside, entries) = (path.clone(), inside.clone(), entries.clone());
                thread::spawn(move || {
                    for _ in 0..25 {
                        let lock = CommitLock::acquire(&path).expect("acquire");
                        assert!(
                            !inside.swap(true, Ordering::SeqCst),
                            "two handles held the commit lock at once"
                        );
                        entries.fetch_add(1, Ordering::SeqCst);
                        thread::yield_now();
                        inside.store(false, Ordering::SeqCst);
                        drop(lock);
                    }
                })
            })
            .collect();
        for worker in workers {
            worker.join().expect("worker");
        }
        assert_eq!(entries.load(Ordering::SeqCst), 200);
        assert!(!commit_lock_path(&path).expect("lock path").exists());
        cleanup(&path);
    }

    #[test]
    fn clean_handles_skip_the_commit_lock() {
        let path = test_path("clean-commit");
        cleanup(&path);
        PersistentIndex32::create(&path, 0.8, 128, 7)
            .expect("create")
            .close()
            .expect("close");
        let mut stale = PersistentIndex32::open(&path).expect("open");
        let mut writer = PersistentIndex32::open(&path).expect("open");
        writer.insert(1, &sketch(0..40)).expect("insert");
        writer.sync().expect("commit");
        // A handle with nothing to commit neither waits nor fails.
        let held = CommitLock::acquire(&path).expect("hold");
        stale.sync().expect("nothing to commit");
        drop(held);
        drop((stale, writer));
        cleanup(&path);
    }
}
