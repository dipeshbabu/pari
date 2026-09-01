#![forbid(unsafe_code)]
//! Crash-safe affine32 and affine64 local persistence for Pari similarity indexes.
//!
//! Committed snapshots keep keys and per-key LSH band hashes as stable metadata
//! while bucket membership uses the canonical checksummed segments from
//! `pari-format`. Reopening a current snapshot loads key metadata plus compact
//! bucket locations only. Candidate memberships remain on disk until queried.
//!
//! Inserts and removals since the last successful commit live in an in-memory
//! overlay. Removed committed generations are explicitly suppressed so
//! remove-then-reinsert semantics remain correct. `flush` and `sync` compact
//! committed state plus the overlay into a fresh atomic snapshot. Phase-1
//! snapshots without bucket segments remain readable and upgrade on next sync.
//! Width-specific public types reject snapshots from the other signature family.

use std::{
    collections::{BTreeMap, HashMap, HashSet},
    error::Error,
    fmt,
    fs::{self, File, OpenOptions},
    io::{self, Write},
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicU64, Ordering},
        Mutex,
    },
    time::Instant,
};

use pari_core::{MinHash32, MinHash64};
use pari_format::{
    bucket_record_size, decode_bucket_segment, encode_bucket_segment, read_bucket_members,
    validate_global_bucket_order, Algorithm, BucketError, BucketKey, BucketLocation, BucketRecord,
    CodecId, FileLayout, FormatError, IndexFile, IndexMetadata, LayoutError, Section,
    SectionDescriptor, SectionKind, SignatureScheme, BUCKET_SEGMENT_HEADER_BYTES,
    BUCKET_SEGMENT_TARGET_BYTES,
};
use pari_index::{
    explain_lsh, explain_lsh64, BucketDistribution, LshError, LshIndex32, LshIndex64, LshParams,
    LshPlan, LshPlanError, LshPlanOptions, QueryMetrics, StorageMode,
};
use same_file::Handle;

const FNV_OFFSET_BASIS: u64 = 0xCBF2_9CE4_8422_2325;
const FNV_PRIME: u64 = 0x0000_0100_0000_01B3;
const COUNT_BYTES: usize = 8;
const U64_BYTES: usize = 8;
const TEMPORARY_ALLOCATION_ATTEMPTS: u64 = 1_024;
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
    /// Size of the currently committed snapshot on disk.
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
    /// Number of committed key generations hidden until the next compaction.
    pub suppressed_base_keys: usize,
    /// Exact distribution of stored committed bucket memberships. Suppressed
    /// generations remain counted until compaction.
    pub committed_distribution: BucketDistribution,
    /// Exact distribution of uncommitted overlay bucket memberships.
    pub overlay_distribution: BucketDistribution,
    /// Process-local query metrics when observability is enabled.
    pub queries: Option<QueryMetrics>,
}

#[derive(Debug)]
struct LazyBase {
    file: Mutex<File>,
    layout: FileLayout,
    buckets: Vec<BucketLocation>,
}

impl LazyBase {
    fn collect_candidates(
        &self,
        hashes: &[u64],
        live_keys: &BTreeMap<u64, Vec<u64>>,
        suppressed: &HashSet<u64>,
        output: &mut HashSet<u64>,
    ) -> Result<(), StoreError> {
        let mut file = self.file.lock().map_err(|_| StoreError::PoisonedFileLock)?;
        for (band, hash) in hashes.iter().copied().enumerate() {
            let key = BucketKey::new(
                u32::try_from(band).map_err(|_| StoreError::LengthOverflow)?,
                hash,
            );
            let Ok(index) = self
                .buckets
                .binary_search_by_key(&key, |location| location.key())
            else {
                continue;
            };
            for member in read_bucket_members(&self.layout, &mut *file, self.buckets[index])? {
                if live_keys.contains_key(&member) && !suppressed.contains(&member) {
                    output.insert(member);
                }
            }
        }
        Ok(())
    }
}

#[derive(Debug)]
struct PersistentIndexCore {
    path: PathBuf,
    threshold: f64,
    num_perm: usize,
    seed: u64,
    params: LshParams,
    scheme: SignatureScheme,
    base: Option<LazyBase>,
    overlay_buckets: Vec<HashMap<u64, Vec<u64>>>,
    overlay_keys: HashSet<u64>,
    suppressed_base_keys: HashSet<u64>,
    key_hashes: BTreeMap<u64, Vec<u64>>,
    dirty: bool,
    query_metrics: Option<Mutex<QueryMetrics>>,
}

impl PersistentIndexCore {
    fn create_with_params(
        path: &Path,
        threshold: f64,
        num_perm: usize,
        seed: u64,
        params: LshParams,
        scheme: SignatureScheme,
    ) -> Result<Self, StoreError> {
        Self::create_with_params_and_hook(
            path,
            threshold,
            num_perm,
            seed,
            params,
            scheme,
            || Ok(()),
        )
    }

    fn create_with_params_and_hook(
        path: &Path,
        threshold: f64,
        num_perm: usize,
        seed: u64,
        params: LshParams,
        scheme: SignatureScheme,
        before_publish: impl FnOnce() -> Result<(), StoreError>,
    ) -> Result<Self, StoreError> {
        let path = path.to_path_buf();
        ensure_destination_absent(&path)?;
        validate_lsh_configuration(scheme, threshold, num_perm, seed, params)?;
        let mut store = Self::empty(path, threshold, num_perm, seed, params, scheme);
        store.commit_initial(before_publish)?;
        Ok(store)
    }

    fn open(path: &Path, scheme: SignatureScheme) -> Result<Self, StoreError> {
        let path = path.to_path_buf();
        let mut file = File::open(&path)?;
        let layout = FileLayout::read_from(&mut file)?;
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

        let key_hashes = read_key_metadata(&layout, &mut file, bands)?;
        let bucket_descriptors = collect_bucket_descriptors(&layout)?;
        if bucket_descriptors.is_empty() {
            return Self::open_legacy(path, threshold, num_perm, seed, params, scheme, key_hashes);
        }

        let mut buckets = Vec::new();
        for descriptor in bucket_descriptors {
            buckets.extend(decode_bucket_segment(
                &layout, &mut file, descriptor, bands,
            )?);
        }
        validate_global_bucket_order(&buckets)?;

        Ok(Self {
            path,
            threshold,
            num_perm,
            seed,
            params,
            scheme,
            base: Some(LazyBase {
                file: Mutex::new(file),
                layout,
                buckets,
            }),
            overlay_buckets: empty_bucket_tables(bands),
            overlay_keys: HashSet::new(),
            suppressed_base_keys: HashSet::new(),
            key_hashes,
            dirty: false,
            query_metrics: None,
        })
    }

    fn insert_hashes_many(&mut self, prepared: Vec<(u64, Vec<u64>)>) -> Result<(), StoreError> {
        let mut batch_keys = HashSet::new();
        for (key, _) in &prepared {
            if self.key_hashes.contains_key(key) || !batch_keys.insert(*key) {
                return Err(StoreError::DuplicateKey { key: *key });
            }
        }

        for (key, hashes) in prepared {
            self.insert_overlay(key, hashes);
        }
        if !batch_keys.is_empty() {
            self.dirty = true;
        }
        Ok(())
    }

    fn remove(&mut self, key: u64) -> bool {
        let Some(hashes) = self.key_hashes.remove(&key) else {
            return false;
        };
        let was_overlay = self.overlay_keys.remove(&key);
        remove_overlay_key(&mut self.overlay_buckets, key, &hashes);
        if self.base.is_some() && !was_overlay {
            self.suppressed_base_keys.insert(key);
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

    fn len(&self) -> usize {
        self.key_hashes.len()
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
        self.commit(false)
    }

    fn sync(&mut self) -> Result<(), StoreError> {
        self.commit(true)
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
        let committed_buckets = self.base.as_ref().map_or(0, |base| base.buckets.len());
        let overlay_buckets = self.overlay_buckets.iter().map(HashMap::len).sum::<usize>();
        let committed_distribution =
            self.base
                .as_ref()
                .map_or_else(BucketDistribution::default, |base| {
                    BucketDistribution::from_sizes(
                        base.buckets.iter().map(|bucket| {
                            usize::try_from(bucket.member_count()).unwrap_or(usize::MAX)
                        }),
                    )
                });
        let overlay_distribution = BucketDistribution::from_sizes(
            self.overlay_buckets
                .iter()
                .flat_map(|table| table.values().map(Vec::len)),
        );
        Ok(StoreStats {
            items: self.len(),
            file_bytes,
            dirty: self.dirty,
            bands: self.params.bands,
            rows: self.params.rows,
            committed_buckets,
            overlay_buckets,
            suppressed_base_keys: self.suppressed_base_keys.len(),
            committed_distribution,
            overlay_distribution,
            queries: self
                .query_metrics
                .as_ref()
                .and_then(|metrics| metrics.lock().ok().map(|metrics| *metrics)),
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
            base: None,
            overlay_buckets: empty_bucket_tables(params.bands),
            overlay_keys: HashSet::new(),
            suppressed_base_keys: HashSet::new(),
            key_hashes: BTreeMap::new(),
            dirty: true,
            query_metrics: None,
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

    fn open_legacy(
        path: PathBuf,
        threshold: f64,
        num_perm: usize,
        seed: u64,
        params: LshParams,
        scheme: SignatureScheme,
        key_hashes: BTreeMap<u64, Vec<u64>>,
    ) -> Result<Self, StoreError> {
        let mut store = Self::empty(path, threshold, num_perm, seed, params, scheme);
        store.key_hashes = key_hashes;
        store.rebuild_overlay_all()?;
        store.dirty = true;
        Ok(store)
    }

    fn insert_overlay(&mut self, key: u64, hashes: Vec<u64>) {
        debug_assert_eq!(hashes.len(), self.params.bands);
        add_overlay_key(&mut self.overlay_buckets, key, &hashes);
        self.overlay_keys.insert(key);
        self.key_hashes.insert(key, hashes);
    }

    fn collect_candidates(
        &self,
        hashes: &[u64],
        output: &mut HashSet<u64>,
    ) -> Result<(), StoreError> {
        if let Some(base) = &self.base {
            base.collect_candidates(hashes, &self.key_hashes, &self.suppressed_base_keys, output)?;
        }
        collect_overlay_candidates(&self.overlay_buckets, hashes, output);
        output.retain(|key| self.key_hashes.contains_key(key));
        Ok(())
    }

    fn rebuild_overlay_all(&mut self) -> Result<(), StoreError> {
        let mut overlay = empty_bucket_tables(self.params.bands);
        let mut overlay_keys = HashSet::with_capacity(self.key_hashes.len());
        for (key, hashes) in &self.key_hashes {
            if hashes.len() != self.params.bands {
                return Err(StoreError::InvalidSnapshot {
                    reason: "in-memory band-hash row has the wrong width",
                });
            }
            add_overlay_key(&mut overlay, *key, hashes);
            overlay_keys.insert(*key);
        }
        self.overlay_buckets = overlay;
        self.overlay_keys = overlay_keys;
        self.suppressed_base_keys.clear();
        self.base = None;
        Ok(())
    }

    fn commit(&mut self, sync_parent: bool) -> Result<(), StoreError> {
        if !self.dirty {
            if sync_parent {
                sync_parent_directory(&self.path)?;
            }
            return Ok(());
        }

        let temporary = self.stage_snapshot()?;

        // Close the committed base before replacing the file. This is required
        // on Windows and avoids retaining a handle to the old generation.
        self.base = None;
        if let Err(error) = temporary.publish_replace(&self.path) {
            self.rebuild_overlay_all()?;
            return Err(error);
        }

        let parent_error = if sync_parent {
            sync_parent_directory(&self.path).err().map(StoreError::Io)
        } else {
            None
        };
        self.refresh_after_commit(parent_error)
    }

    fn commit_initial(
        &mut self,
        before_publish: impl FnOnce() -> Result<(), StoreError>,
    ) -> Result<(), StoreError> {
        let temporary = self.stage_snapshot()?;
        before_publish()?;

        self.base = None;
        if let Err(error) = temporary.publish_no_replace(&self.path) {
            self.rebuild_overlay_all()?;
            return Err(error);
        }
        self.refresh_after_commit(None)
    }

    fn stage_snapshot(&self) -> Result<OwnedTemporaryFile, StoreError> {
        let snapshot = self.encode_snapshot()?;
        create_parent_if_needed(&self.path)?;
        let mut temporary = OwnedTemporaryFile::allocate(&self.path)?;
        if let Err(error) = temporary.write_synced(&snapshot) {
            return Err(temporary.cleanup_after(error));
        }
        Ok(temporary)
    }

    fn refresh_after_commit(&mut self, parent_error: Option<StoreError>) -> Result<(), StoreError> {
        match Self::open(&self.path, self.scheme) {
            Ok(mut refreshed) => {
                if parent_error.is_some() {
                    refreshed.dirty = true;
                }
                *self = refreshed;
            }
            Err(error) => {
                self.rebuild_overlay_all()?;
                self.dirty = true;
                return Err(error);
            }
        }
        if let Some(error) = parent_error {
            return Err(error);
        }
        Ok(())
    }

    fn encode_snapshot(&self) -> Result<Vec<u8>, StoreError> {
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
        let keys = encode_keys(self.key_hashes.keys().copied())?;
        let hashes = encode_band_hashes(self.key_hashes.values(), self.params.bands)?;
        let mut sections = vec![
            Section::new(SectionKind::Keys, true, keys)?,
            Section::new(SectionKind::BandHashes, true, hashes)?,
        ];
        append_bucket_sections(&mut sections, &self.key_hashes, self.params.bands)?;
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
    ($index:ident, $sketch:ty, $lsh:ty, $scheme:expr, $hash_band:ident) => {
        impl $index {
            /// Create a new empty index and immediately commit its initial snapshot.
            pub fn create(
                path: impl AsRef<Path>,
                threshold: f64,
                num_perm: usize,
                seed: u64,
            ) -> Result<Self, StoreError> {
                let reference = <$lsh>::new(threshold, num_perm, seed)?;
                Self::create_with_params(path, threshold, num_perm, seed, reference.params())
            }

            /// Create a new empty index with explicit LSH banding parameters.
            pub fn create_with_params(
                path: impl AsRef<Path>,
                threshold: f64,
                num_perm: usize,
                seed: u64,
                params: LshParams,
            ) -> Result<Self, StoreError> {
                Ok(Self {
                    inner: PersistentIndexCore::create_with_params(
                        path.as_ref(),
                        threshold,
                        num_perm,
                        seed,
                        params,
                        $scheme,
                    )?,
                })
            }

            /// Open and validate the last committed snapshot at `path`.
            ///
            /// Current snapshots load key metadata plus bucket locations only. Legacy
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
                let prepared = items
                    .into_iter()
                    .map(|(key, sketch)| Ok((key, self.band_hashes(sketch)?)))
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

            /// Return whether a live key exists.
            #[must_use]
            pub fn contains(&self, key: u64) -> bool {
                self.inner.key_hashes.contains_key(&key)
            }

            /// Return the number of live keys.
            #[must_use]
            pub fn len(&self) -> usize {
                self.inner.len()
            }

            /// Return whether no live keys are indexed.
            #[must_use]
            pub fn is_empty(&self) -> bool {
                self.inner.key_hashes.is_empty()
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

            /// Commit dirty state to an atomic snapshot.
            ///
            /// The snapshot file itself is synced before rename. Use [`Self::sync`] for
            /// the strongest post-rename durability step the current platform supports.
            pub fn flush(&mut self) -> Result<(), StoreError> {
                self.inner.flush()
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
    LshIndex32,
    SignatureScheme::PariAffine32V1,
    hash_band32
);
impl_persistent_index!(
    PersistentIndex64,
    MinHash64,
    LshIndex64,
    SignatureScheme::PariAffine64V1,
    hash_band64
);

fn create_parent_if_needed(path: &Path) -> Result<(), StoreError> {
    if let Some(parent) = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
    {
        fs::create_dir_all(parent)?;
    }
    Ok(())
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
    scheme: SignatureScheme,
    threshold: f64,
    num_perm: usize,
    seed: u64,
    params: LshParams,
) -> Result<(), StoreError> {
    match scheme {
        SignatureScheme::PariAffine32V1 => {
            LshIndex32::with_params(threshold, num_perm, seed, params)?;
        }
        SignatureScheme::PariAffine64V1 => {
            LshIndex64::with_params(threshold, num_perm, seed, params)?;
        }
    }
    Ok(())
}

fn read_key_metadata(
    layout: &FileLayout,
    file: &mut File,
    bands: usize,
) -> Result<BTreeMap<u64, Vec<u64>>, StoreError> {
    let keys_descriptor = required_unique_section(layout, SectionKind::Keys)?;
    let hashes_descriptor = required_unique_section(layout, SectionKind::BandHashes)?;
    let keys_payload = layout.read_section(file, keys_descriptor)?;
    let hashes_payload = layout.read_section(file, hashes_descriptor)?;
    let keys = decode_keys(&keys_payload)?;
    let rows_by_key = decode_band_hashes(&hashes_payload, bands)?;
    if keys.len() != rows_by_key.len() {
        return Err(StoreError::InvalidSnapshot {
            reason: "key and band-hash record counts differ",
        });
    }

    let mut key_hashes = BTreeMap::new();
    for (key, hashes) in keys.into_iter().zip(rows_by_key) {
        if key_hashes.insert(key, hashes).is_some() {
            return Err(StoreError::InvalidSnapshot {
                reason: "duplicate key in persisted keys section",
            });
        }
    }
    Ok(key_hashes)
}

fn collect_bucket_descriptors(layout: &FileLayout) -> Result<Vec<SectionDescriptor>, StoreError> {
    let mut descriptors = Vec::new();
    for descriptor in layout.sections() {
        match descriptor.kind() {
            SectionKind::Buckets => {
                if !descriptor.required() {
                    return Err(StoreError::InvalidSnapshot {
                        reason: "lazy bucket sections must be required",
                    });
                }
                descriptors.push(*descriptor);
            }
            SectionKind::Tombstones if descriptor.required() => {
                return Err(StoreError::InvalidSnapshot {
                    reason: "required tombstone sections are not supported",
                });
            }
            SectionKind::Keys | SectionKind::BandHashes | SectionKind::Tombstones => {}
        }
    }
    Ok(descriptors)
}

fn required_unique_section(
    layout: &FileLayout,
    kind: SectionKind,
) -> Result<SectionDescriptor, StoreError> {
    let mut matches = layout
        .sections()
        .iter()
        .copied()
        .filter(|descriptor| descriptor.kind() == kind);
    let descriptor = matches.next().ok_or(StoreError::InvalidSnapshot {
        reason: missing_section_reason(kind),
    })?;
    if matches.next().is_some() {
        return Err(StoreError::InvalidSnapshot {
            reason: duplicate_section_reason(kind),
        });
    }
    if !descriptor.required() {
        return Err(StoreError::InvalidSnapshot {
            reason: "required metadata section is marked optional",
        });
    }
    Ok(descriptor)
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

fn empty_bucket_tables(bands: usize) -> Vec<HashMap<u64, Vec<u64>>> {
    std::iter::repeat_with(HashMap::new).take(bands).collect()
}

fn add_overlay_key(tables: &mut [HashMap<u64, Vec<u64>>], key: u64, hashes: &[u64]) {
    for (table, hash) in tables.iter_mut().zip(hashes) {
        table.entry(*hash).or_default().push(key);
    }
}

fn remove_overlay_key(tables: &mut [HashMap<u64, Vec<u64>>], key: u64, hashes: &[u64]) {
    for (table, hash) in tables.iter_mut().zip(hashes) {
        let remove_bucket = if let Some(keys) = table.get_mut(hash) {
            keys.retain(|candidate| *candidate != key);
            keys.is_empty()
        } else {
            false
        };
        if remove_bucket {
            table.remove(hash);
        }
    }
}

fn collect_overlay_candidates(
    tables: &[HashMap<u64, Vec<u64>>],
    hashes: &[u64],
    output: &mut HashSet<u64>,
) {
    for (table, hash) in tables.iter().zip(hashes) {
        if let Some(keys) = table.get(hash) {
            output.extend(keys.iter().copied());
        }
    }
}

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

fn decode_band_hashes(payload: &[u8], bands: usize) -> Result<Vec<Vec<u64>>, StoreError> {
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
        collections::{BTreeMap, HashMap},
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
        encode_band_hashes, encode_keys, OwnedTemporaryFile, PersistentIndex32, PersistentIndex64,
        PersistentIndexCore, StoreError,
    };

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
        assert!(reopened.inner.base.is_some());
        assert!(reopened.inner.overlay_buckets.iter().all(HashMap::is_empty));
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
        assert!(legacy.inner.base.is_none());
        assert!(legacy.stats().expect("legacy stats").dirty);
        assert!(legacy.query(&first).expect("legacy query").contains(&10));
        legacy.sync().expect("upgrade sync");
        drop(legacy);

        let upgraded = PersistentIndex32::open(&path).expect("open upgraded");
        assert!(upgraded.inner.base.is_some());
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
        let location = store
            .inner
            .base
            .as_ref()
            .expect("lazy base")
            .buckets
            .first()
            .copied()
            .expect("bucket location");
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
        assert!(store.inner.base.is_some());
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
}
