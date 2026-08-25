#![forbid(unsafe_code)]
//! Crash-safe local persistence for Pari similarity indexes.
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

use std::{
    collections::{BTreeMap, HashMap, HashSet},
    error::Error,
    fmt,
    fs::{self, File, OpenOptions},
    io::{self, Write},
    path::{Path, PathBuf},
    sync::Mutex,
};

use pari_core::MinHash32;
use pari_format::{
    bucket_record_size, decode_bucket_segment, encode_bucket_segment, read_bucket_members,
    validate_global_bucket_order, Algorithm, BucketError, BucketKey, BucketLocation, BucketRecord,
    CodecId, FileLayout, FormatError, IndexFile, IndexMetadata, LayoutError, Section,
    SectionDescriptor, SectionKind, SignatureScheme, BUCKET_SEGMENT_HEADER_BYTES,
    BUCKET_SEGMENT_TARGET_BYTES,
};
use pari_index::{LshError, LshIndex32, LshParams};

const FNV_OFFSET_BASIS: u64 = 0xCBF2_9CE4_8422_2325;
const FNV_PRIME: u64 = 0x0000_0100_0000_01B3;
const COUNT_BYTES: usize = 8;
const U64_BYTES: usize = 8;

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
            Self::InvalidSnapshot { reason } => write!(formatter, "invalid local snapshot: {reason}"),
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

/// A persistent `MinHash32` LSH index with lazy committed-bucket reads.
#[derive(Debug)]
pub struct PersistentIndex32 {
    path: PathBuf,
    threshold: f64,
    num_perm: usize,
    seed: u64,
    params: LshParams,
    base: Option<LazyBase>,
    overlay_buckets: Vec<HashMap<u64, Vec<u64>>>,
    overlay_keys: HashSet<u64>,
    suppressed_base_keys: HashSet<u64>,
    key_hashes: BTreeMap<u64, Vec<u64>>,
    dirty: bool,
}

impl PersistentIndex32 {
    /// Create a new empty index and immediately commit its initial snapshot.
    pub fn create(
        path: impl AsRef<Path>,
        threshold: f64,
        num_perm: usize,
        seed: u64,
    ) -> Result<Self, StoreError> {
        let reference = LshIndex32::new(threshold, num_perm, seed)?;
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
        let path = path.as_ref().to_path_buf();
        if path.exists() {
            return Err(StoreError::Io(io::Error::new(
                io::ErrorKind::AlreadyExists,
                format!("index path {} already exists", path.display()),
            )));
        }
        LshIndex32::with_params(threshold, num_perm, seed, params)?;
        let mut store = Self::empty(path, threshold, num_perm, seed, params);
        store.sync()?;
        Ok(store)
    }

    /// Open and validate the last committed snapshot at `path`.
    ///
    /// Current snapshots load key metadata plus bucket locations only. Legacy
    /// phase-1 snapshots are accepted by rebuilding their buckets into the
    /// mutation overlay and are marked dirty so the next sync upgrades them.
    pub fn open(path: impl AsRef<Path>) -> Result<Self, StoreError> {
        let path = path.as_ref().to_path_buf();
        let mut file = File::open(&path)?;
        let layout = FileLayout::read_from(&mut file)?;
        validate_store_metadata(layout.metadata())?;
        let num_perm = usize::try_from(layout.metadata().num_perm())
            .map_err(|_| StoreError::LengthOverflow)?;
        let bands = usize::try_from(layout.metadata().bands())
            .map_err(|_| StoreError::LengthOverflow)?;
        let rows = usize::try_from(layout.metadata().rows())
            .map_err(|_| StoreError::LengthOverflow)?;
        let threshold = layout.metadata().threshold();
        let seed = layout.metadata().seed();
        let params = LshParams::new(bands, rows);
        LshIndex32::with_params(threshold, num_perm, seed, params)?;

        let key_hashes = read_key_metadata(&layout, &mut file, bands)?;
        let bucket_descriptors = collect_bucket_descriptors(&layout)?;
        if bucket_descriptors.is_empty() {
            return Self::open_legacy(path, threshold, num_perm, seed, params, key_hashes);
        }

        let mut buckets = Vec::new();
        for descriptor in bucket_descriptors {
            buckets.extend(decode_bucket_segment(
                &layout,
                &mut file,
                descriptor,
                bands,
            )?);
        }
        validate_global_bucket_order(&buckets)?;

        Ok(Self {
            path,
            threshold,
            num_perm,
            seed,
            params,
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
        })
    }

    /// Insert one external key and signature.
    pub fn insert(&mut self, key: u64, sketch: &MinHash32) -> Result<(), StoreError> {
        self.insert_many(std::iter::once((key, sketch)))
    }

    /// Insert a batch after validating the complete batch before mutation.
    pub fn insert_many<'a>(
        &mut self,
        items: impl IntoIterator<Item = (u64, &'a MinHash32)>,
    ) -> Result<(), StoreError> {
        let mut prepared = Vec::new();
        let mut batch_keys = HashSet::new();
        for (key, sketch) in items {
            if self.key_hashes.contains_key(&key) || !batch_keys.insert(key) {
                return Err(StoreError::DuplicateKey { key });
            }
            prepared.push((key, self.band_hashes(sketch)?));
        }

        for (key, hashes) in prepared {
            self.insert_overlay(key, hashes);
        }
        if !batch_keys.is_empty() {
            self.dirty = true;
        }
        Ok(())
    }

    /// Remove a key if present, returning whether the index changed.
    pub fn remove(&mut self, key: u64) -> bool {
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

    /// Query approximate candidates for one compatible signature.
    pub fn query(&self, sketch: &MinHash32) -> Result<Vec<u64>, StoreError> {
        let hashes = self.band_hashes(sketch)?;
        let mut candidates = HashSet::new();
        self.collect_candidates(&hashes, &mut candidates)?;
        let mut keys: Vec<_> = candidates.into_iter().collect();
        keys.sort_unstable();
        Ok(keys)
    }

    /// Query many signatures while reusing candidate scratch storage.
    pub fn query_many<'a>(
        &self,
        sketches: impl IntoIterator<Item = &'a MinHash32>,
    ) -> Result<Vec<Vec<u64>>, StoreError> {
        let mut output = Vec::new();
        let mut candidates = HashSet::new();
        for sketch in sketches {
            let hashes = self.band_hashes(sketch)?;
            candidates.clear();
            self.collect_candidates(&hashes, &mut candidates)?;
            let mut keys: Vec<_> = candidates.iter().copied().collect();
            keys.sort_unstable();
            output.push(keys);
        }
        Ok(output)
    }

    /// Return whether a live key exists.
    #[must_use]
    pub fn contains(&self, key: u64) -> bool {
        self.key_hashes.contains_key(&key)
    }

    /// Return the number of live keys.
    #[must_use]
    pub fn len(&self) -> usize {
        self.key_hashes.len()
    }

    /// Return whether no live keys are indexed.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.key_hashes.is_empty()
    }

    /// Return the configured similarity threshold.
    #[must_use]
    pub const fn threshold(&self) -> f64 {
        self.threshold
    }

    /// Return the configured signature length.
    #[must_use]
    pub const fn num_perm(&self) -> usize {
        self.num_perm
    }

    /// Return the required `MinHash` seed.
    #[must_use]
    pub const fn seed(&self) -> u64 {
        self.seed
    }

    /// Return the configured LSH banding parameters.
    #[must_use]
    pub const fn params(&self) -> LshParams {
        self.params
    }

    /// Commit dirty state to an atomic snapshot.
    ///
    /// The snapshot file itself is synced before rename. Use [`Self::sync`] when
    /// the caller also requires the containing directory entry to be synced.
    pub fn flush(&mut self) -> Result<(), StoreError> {
        self.commit(false)
    }

    /// Commit dirty state and sync the containing directory after rename.
    pub fn sync(&mut self) -> Result<(), StoreError> {
        self.commit(true)
    }

    /// Sync pending state and consume the index handle.
    pub fn close(mut self) -> Result<(), StoreError> {
        self.sync()
    }

    /// Return current in-memory and committed-file statistics.
    pub fn stats(&self) -> Result<StoreStats, StoreError> {
        let file_bytes = match fs::metadata(&self.path) {
            Ok(metadata) => metadata.len(),
            Err(error) if error.kind() == io::ErrorKind::NotFound => 0,
            Err(error) => return Err(StoreError::Io(error)),
        };
        let committed_buckets = self.base.as_ref().map_or(0, |base| base.buckets.len());
        let overlay_buckets = self.overlay_buckets.iter().map(HashMap::len).sum::<usize>();
        Ok(StoreStats {
            items: self.len(),
            file_bytes,
            dirty: self.dirty,
            bands: self.params.bands,
            rows: self.params.rows,
            committed_buckets,
            overlay_buckets,
            suppressed_base_keys: self.suppressed_base_keys.len(),
        })
    }

    fn empty(path: PathBuf, threshold: f64, num_perm: usize, seed: u64, params: LshParams) -> Self {
        Self {
            path,
            threshold,
            num_perm,
            seed,
            params,
            base: None,
            overlay_buckets: empty_bucket_tables(params.bands),
            overlay_keys: HashSet::new(),
            suppressed_base_keys: HashSet::new(),
            key_hashes: BTreeMap::new(),
            dirty: true,
        }
    }

    fn open_legacy(
        path: PathBuf,
        threshold: f64,
        num_perm: usize,
        seed: u64,
        params: LshParams,
        key_hashes: BTreeMap<u64, Vec<u64>>,
    ) -> Result<Self, StoreError> {
        let mut store = Self::empty(path, threshold, num_perm, seed, params);
        store.key_hashes = key_hashes;
        store.rebuild_overlay_all()?;
        store.dirty = true;
        Ok(store)
    }

    fn band_hashes(&self, sketch: &MinHash32) -> Result<Vec<u64>, StoreError> {
        if sketch.seed() != self.seed {
            return Err(StoreError::IncompatibleSeed {
                expected: self.seed,
                actual: sketch.seed(),
            });
        }
        if sketch.num_perm() != self.num_perm {
            return Err(StoreError::IncompatiblePermutationCount {
                expected: self.num_perm,
                actual: sketch.num_perm(),
            });
        }
        let used = self
            .params
            .used_permutations()
            .ok_or(StoreError::LengthOverflow)?;
        Ok(sketch.signature()[..used]
            .chunks_exact(self.params.rows)
            .map(hash_band)
            .collect())
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

        let snapshot = self.encode_snapshot()?;
        let temporary = temporary_path(&self.path)?;
        create_parent_if_needed(&self.path)?;
        write_synced_file(&temporary, &snapshot)?;

        // Close the committed base before replacing the file. This is required
        // on Windows and avoids retaining a handle to the old generation.
        self.base = None;
        if let Err(error) = fs::rename(&temporary, &self.path) {
            self.rebuild_overlay_all()?;
            return Err(StoreError::Io(error));
        }

        let parent_error = if sync_parent {
            sync_parent_directory(&self.path).err()
        } else {
            None
        };
        self.refresh_after_commit(parent_error)
    }

    fn refresh_after_commit(&mut self, parent_error: Option<StoreError>) -> Result<(), StoreError> {
        match Self::open(&self.path) {
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
            SignatureScheme::PariAffine32V1,
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

fn create_parent_if_needed(path: &Path) -> Result<(), StoreError> {
    if let Some(parent) = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
    {
        fs::create_dir_all(parent)?;
    }
    Ok(())
}

fn write_synced_file(path: &Path, bytes: &[u8]) -> Result<(), StoreError> {
    let mut file = OpenOptions::new()
        .create(true)
        .truncate(true)
        .write(true)
        .open(path)?;
    file.write_all(bytes)?;
    file.flush()?;
    file.sync_all()?;
    Ok(())
}

fn validate_store_metadata(metadata: &IndexMetadata) -> Result<(), StoreError> {
    if metadata.algorithm() != Algorithm::MinHashLsh {
        return Err(StoreError::InvalidSnapshot {
            reason: "snapshot algorithm is not MinHash LSH",
        });
    }
    if metadata.signature_scheme() != SignatureScheme::PariAffine32V1 {
        return Err(StoreError::InvalidSnapshot {
            reason: "snapshot signature scheme is not pari-affine32-v1",
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

fn temporary_path(path: &Path) -> Result<PathBuf, StoreError> {
    let file_name = path.file_name().ok_or(StoreError::InvalidPath)?;
    let mut temporary_name = file_name.to_os_string();
    temporary_name.push(".tmp");
    Ok(path.with_file_name(temporary_name))
}

fn sync_parent_directory(path: &Path) -> Result<(), StoreError> {
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
        io::{Seek, SeekFrom, Write},
        path::PathBuf,
    };

    use pari_core::MinHash32;
    use pari_format::{
        Algorithm, BucketError, CodecId, FileLayout, IndexFile, IndexMetadata, Section,
        SectionKind, SignatureScheme, BUCKET_SEGMENT_HEADER_BYTES,
    };
    use pari_index::{LshIndex32, LshParams};

    use super::{encode_band_hashes, encode_keys, temporary_path, PersistentIndex32, StoreError};

    fn sketch(values: impl IntoIterator<Item = u64>) -> MinHash32 {
        let mut sketch = MinHash32::new(128, 7).expect("valid test sketch");
        for value in values {
            sketch.update(&value.to_le_bytes());
        }
        sketch
    }

    fn test_path(name: &str) -> PathBuf {
        std::env::temp_dir().join(format!(
            "pari-store-{name}-{}-{}.pari",
            std::process::id(),
            std::thread::current().name().unwrap_or("test")
        ))
    }

    fn cleanup(path: &PathBuf) {
        let _ = fs::remove_file(path);
        if let Ok(temporary) = temporary_path(path) {
            let _ = fs::remove_file(temporary);
        }
    }

    fn phase1_snapshot(path: &PathBuf, rows: &[(u64, MinHash32)]) {
        let reference = LshIndex32::new(0.8, 128, 7).expect("reference");
        let params = reference.params();
        let helper = PersistentIndex32::empty(path.clone(), 0.8, 128, 7, params);
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
        assert!(reopened.base.is_some());
        assert!(reopened.overlay_buckets.iter().all(HashMap::is_empty));
        assert!(reopened.stats().expect("stats").committed_buckets > 0);
        assert_eq!(reopened.query(&first).expect("query reopened"), before);
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
        assert!(legacy.base.is_none());
        assert!(legacy.stats().expect("legacy stats").dirty);
        assert!(legacy.query(&first).expect("legacy query").contains(&10));
        legacy.sync().expect("upgrade sync");
        drop(legacy);

        let upgraded = PersistentIndex32::open(&path).expect("open upgraded");
        assert!(upgraded.base.is_some());
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

        let temporary = temporary_path(&path).expect("temporary path");
        fs::write(&temporary, b"partial and uncommitted").expect("write stale temp");

        let reopened = PersistentIndex32::open(&path).expect("committed target stays valid");
        assert!(reopened.contains(10));
        cleanup(&path);
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
            Err(StoreError::Bucket(BucketError::MemberChecksumMismatch {
                ..
            }))
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
        assert!(store.base.is_some());
        cleanup(&path);
    }

    #[test]
    fn bucket_segment_header_size_constant_matches_fixture_contract() {
        assert_eq!(BUCKET_SEGMENT_HEADER_BYTES, 40);
        let params = LshParams::new(32, 4);
        assert_eq!(params.used_permutations(), Some(128));
    }
}
