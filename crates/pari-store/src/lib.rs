#![forbid(unsafe_code)]
//! Crash-safe local persistence for Pari similarity indexes.
//!
//! The phase-1 backend intentionally favors a small, auditable snapshot format
//! over a complex storage engine. It persists external keys and precomputed LSH
//! band hashes, so reopening does not recompute `MinHash` signatures. Bucket
//! tables are rebuilt in memory on open; lazy on-disk bucket paging is tracked
//! separately by issue #18.
//!
//! A commit is written to a sibling temporary file, flushed and synced, then
//! atomically renamed over the committed snapshot. Temporary files are never
//! considered committed state. The API assumes one writer per index path.

use std::{
    collections::{BTreeMap, HashMap, HashSet},
    error::Error,
    fmt,
    fs::{self, File, OpenOptions},
    io::{self, Write},
    path::{Path, PathBuf},
};

use pari_core::MinHash32;
use pari_format::{
    Algorithm, CodecId, FormatError, IndexFile, IndexMetadata, Section, SectionKind,
    SignatureScheme,
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
    /// In-memory LSH configuration validation failed.
    Index(LshError),
    /// Snapshot sections violate the phase-1 storage contract.
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
}

impl fmt::Display for StoreError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io(error) => write!(formatter, "local index I/O failed: {error}"),
            Self::Format(error) => write!(formatter, "invalid Pari index snapshot: {error}"),
            Self::Index(error) => write!(formatter, "invalid LSH configuration: {error}"),
            Self::InvalidSnapshot { reason } => {
                write!(formatter, "invalid phase-1 local snapshot: {reason}")
            }
            Self::DuplicateKey { key } => write!(formatter, "key {key} already exists in the index"),
            Self::IncompatibleSeed { expected, actual } => write!(
                formatter,
                "incompatible MinHash seed: expected {expected}, got {actual}"
            ),
            Self::IncompatiblePermutationCount { expected, actual } => write!(
                formatter,
                "incompatible MinHash permutation count: expected {expected}, got {actual}"
            ),
            Self::LengthOverflow => formatter.write_str("persistent index length arithmetic overflowed"),
            Self::InvalidPath => formatter.write_str("persistent index path must identify a file"),
        }
    }
}

impl Error for StoreError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Io(error) => Some(error),
            Self::Format(error) => Some(error),
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
    /// Whether memory contains changes that have not been committed.
    pub dirty: bool,
    /// Number of LSH bands.
    pub bands: usize,
    /// Number of signature rows per band.
    pub rows: usize,
}

/// A phase-1 persistent `MinHash32` LSH index using atomic snapshots.
#[derive(Debug)]
pub struct PersistentIndex32 {
    path: PathBuf,
    threshold: f64,
    num_perm: usize,
    seed: u64,
    params: LshParams,
    buckets: Vec<HashMap<u64, Vec<u64>>>,
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
    pub fn open(path: impl AsRef<Path>) -> Result<Self, StoreError> {
        let path = path.as_ref().to_path_buf();
        let bytes = fs::read(&path)?;
        let file = IndexFile::decode(&bytes)?;
        Self::from_snapshot(path, file)
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
            let hashes = self.band_hashes(sketch)?;
            prepared.push((key, hashes));
        }

        for (key, hashes) in prepared {
            self.insert_precomputed(key, hashes);
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
        for (table, hash) in self.buckets.iter_mut().zip(hashes) {
            let remove_bucket = if let Some(keys) = table.get_mut(&hash) {
                keys.retain(|candidate| *candidate != key);
                keys.is_empty()
            } else {
                false
            };
            if remove_bucket {
                table.remove(&hash);
            }
        }
        self.dirty = true;
        true
    }

    /// Query approximate candidates for one compatible signature.
    pub fn query(&self, sketch: &MinHash32) -> Result<Vec<u64>, StoreError> {
        let hashes = self.band_hashes(sketch)?;
        let mut candidates = HashSet::new();
        self.collect_candidates(&hashes, &mut candidates);
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
            self.collect_candidates(&hashes, &mut candidates);
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
        Ok(StoreStats {
            items: self.len(),
            file_bytes,
            dirty: self.dirty,
            bands: self.params.bands,
            rows: self.params.rows,
        })
    }

    fn empty(path: PathBuf, threshold: f64, num_perm: usize, seed: u64, params: LshParams) -> Self {
        let buckets = std::iter::repeat_with(HashMap::new)
            .take(params.bands)
            .collect();
        Self {
            path,
            threshold,
            num_perm,
            seed,
            params,
            buckets,
            key_hashes: BTreeMap::new(),
            dirty: true,
        }
    }

    fn from_snapshot(path: PathBuf, file: IndexFile) -> Result<Self, StoreError> {
        let metadata = file.metadata();
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
                reason: "phase-1 local store requires u64 keys",
            });
        }
        if metadata.feature_flags() != 0 {
            return Err(StoreError::InvalidSnapshot {
                reason: "phase-1 local store does not support feature flags",
            });
        }

        let num_perm = usize::try_from(metadata.num_perm()).map_err(|_| StoreError::LengthOverflow)?;
        let bands = usize::try_from(metadata.bands()).map_err(|_| StoreError::LengthOverflow)?;
        let rows = usize::try_from(metadata.rows()).map_err(|_| StoreError::LengthOverflow)?;
        let params = LshParams::new(bands, rows);
        LshIndex32::with_params(metadata.threshold(), num_perm, metadata.seed(), params)?;

        let mut keys_payload = None;
        let mut hashes_payload = None;
        for section in file.sections() {
            match section.kind() {
                SectionKind::Keys => set_unique_section(&mut keys_payload, section.payload(), "keys")?,
                SectionKind::BandHashes => {
                    set_unique_section(&mut hashes_payload, section.payload(), "band hashes")?;
                }
                SectionKind::Buckets | SectionKind::Tombstones if section.required() => {
                    return Err(StoreError::InvalidSnapshot {
                        reason: "snapshot contains an unsupported required storage section",
                    });
                }
                SectionKind::Buckets | SectionKind::Tombstones => {}
            }
        }

        let keys = decode_keys(keys_payload.ok_or(StoreError::InvalidSnapshot {
            reason: "missing required keys section",
        })?)?;
        let rows_by_key = decode_band_hashes(
            hashes_payload.ok_or(StoreError::InvalidSnapshot {
                reason: "missing required band-hash section",
            })?,
            bands,
        )?;
        if keys.len() != rows_by_key.len() {
            return Err(StoreError::InvalidSnapshot {
                reason: "key and band-hash record counts differ",
            });
        }

        let mut store = Self::empty(path, metadata.threshold(), num_perm, metadata.seed(), params);
        store.dirty = false;
        for (key, hashes) in keys.into_iter().zip(rows_by_key) {
            if store.key_hashes.contains_key(&key) {
                return Err(StoreError::InvalidSnapshot {
                    reason: "duplicate key in persisted keys section",
                });
            }
            store.insert_precomputed(key, hashes);
        }
        store.dirty = false;
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

    fn insert_precomputed(&mut self, key: u64, hashes: Vec<u64>) {
        debug_assert_eq!(hashes.len(), self.params.bands);
        for (table, hash) in self.buckets.iter_mut().zip(&hashes) {
            table.entry(*hash).or_default().push(key);
        }
        self.key_hashes.insert(key, hashes);
    }

    fn collect_candidates(&self, hashes: &[u64], output: &mut HashSet<u64>) {
        for (table, hash) in self.buckets.iter().zip(hashes) {
            if let Some(keys) = table.get(hash) {
                output.extend(keys.iter().copied());
            }
        }
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
        if let Some(parent) = self.path.parent().filter(|parent| !parent.as_os_str().is_empty()) {
            fs::create_dir_all(parent)?;
        }

        let write_result = (|| -> Result<(), StoreError> {
            let mut file = OpenOptions::new()
                .create(true)
                .truncate(true)
                .write(true)
                .open(&temporary)?;
            file.write_all(&snapshot)?;
            file.flush()?;
            file.sync_all()?;
            fs::rename(&temporary, &self.path)?;
            if sync_parent {
                sync_parent_directory(&self.path)?;
            }
            Ok(())
        })();

        if write_result.is_ok() {
            self.dirty = false;
        }
        write_result
    }

    fn encode_snapshot(&self) -> Result<Vec<u8>, StoreError> {
        let num_perm = u32::try_from(self.num_perm).map_err(|_| StoreError::LengthOverflow)?;
        let bands = u32::try_from(self.params.bands).map_err(|_| StoreError::LengthOverflow)?;
        let rows = u32::try_from(self.params.rows).map_err(|_| StoreError::LengthOverflow)?;
        let metadata = IndexMetadata::new(
            Algorithm::MinHashLsh,
            SignatureScheme::PariAffine32V1,
            CodecId::U64,
            num_perm,
            self.seed,
            self.threshold,
            bands,
            rows,
            0,
        )?;

        let keys = encode_keys(self.key_hashes.keys().copied())?;
        let hashes = encode_band_hashes(self.key_hashes.values(), self.params.bands)?;
        let file = IndexFile::new(
            metadata,
            vec![
                Section::new(SectionKind::Keys, true, keys)?,
                Section::new(SectionKind::BandHashes, true, hashes)?,
            ],
        )?;
        Ok(file.encode()?)
    }
}

fn set_unique_section<'a>(
    slot: &mut Option<&'a [u8]>,
    payload: &'a [u8],
    name: &'static str,
) -> Result<(), StoreError> {
    if slot.replace(payload).is_some() {
        return Err(StoreError::InvalidSnapshot {
            reason: match name {
                "keys" => "duplicate keys section",
                _ => "duplicate band-hash section",
            },
        });
    }
    Ok(())
}

fn encode_keys(keys: impl ExactSizeIterator<Item = u64>) -> Result<Vec<u8>, StoreError> {
    let count = u64::try_from(keys.len()).map_err(|_| StoreError::LengthOverflow)?;
    let capacity = COUNT_BYTES
        .checked_add(keys.len().checked_mul(U64_BYTES).ok_or(StoreError::LengthOverflow)?)
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
        .checked_add(count.checked_mul(U64_BYTES).ok_or(StoreError::LengthOverflow)?)
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
    let values = rows.len().checked_mul(bands).ok_or(StoreError::LengthOverflow)?;
    let capacity = COUNT_BYTES
        .checked_add(values.checked_mul(U64_BYTES).ok_or(StoreError::LengthOverflow)?)
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
    let row_bytes = bands.checked_mul(U64_BYTES).ok_or(StoreError::LengthOverflow)?;
    let expected = COUNT_BYTES
        .checked_add(count.checked_mul(row_bytes).ok_or(StoreError::LengthOverflow)?)
        .ok_or(StoreError::LengthOverflow)?;
    if payload.len() != expected {
        return Err(StoreError::InvalidSnapshot {
            reason: "band-hash section length does not match metadata and record count",
        });
    }
    let mut rows = Vec::with_capacity(count);
    for row in payload[COUNT_BYTES..].chunks_exact(row_bytes) {
        let hashes = row.chunks_exact(U64_BYTES).map(read_u64).collect::<Result<_, _>>()?;
        rows.push(hashes);
    }
    Ok(rows)
}

fn read_count(payload: &[u8]) -> Result<usize, StoreError> {
    let raw = payload.get(..COUNT_BYTES).ok_or(StoreError::InvalidSnapshot {
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

#[cfg(test)]
mod tests {
    use std::{fs, path::PathBuf};

    use pari_core::MinHash32;
    use pari_index::LshIndex32;

    use super::{temporary_path, PersistentIndex32, StoreError};

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

    #[test]
    fn committed_snapshot_reopens_with_identical_queries_and_deletions() {
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
    fn corrupt_committed_snapshot_fails_explicitly() {
        let path = test_path("corrupt");
        cleanup(&path);
        let first = sketch(0..40);
        let mut store = PersistentIndex32::create(&path, 0.8, 128, 7).expect("create store");
        store.insert(10, &first).expect("insert");
        store.sync().expect("sync snapshot");
        drop(store);

        let mut bytes = fs::read(&path).expect("read snapshot");
        let last = bytes.last_mut().expect("snapshot is non-empty");
        *last ^= 0xFF;
        fs::write(&path, bytes).expect("write corruption");

        assert!(matches!(PersistentIndex32::open(&path), Err(StoreError::Format(_))));
        cleanup(&path);
    }

    #[test]
    fn candidates_match_in_memory_reference_index() {
        let path = test_path("parity");
        cleanup(&path);
        let sketches = [sketch(0..40), sketch(0..35), sketch(100..140), sketch(0..30)];
        let mut persistent =
            PersistentIndex32::create(&path, 0.8, 128, 7).expect("create persistent");
        let mut memory = LshIndex32::new(0.8, 128, 7).expect("create memory index");
        for (key, sketch) in sketches.iter().enumerate() {
            let key = u64::try_from(key).expect("small key");
            persistent.insert(key, sketch).expect("persistent insert");
            memory.insert(key, sketch).expect("memory insert");
        }
        for query in &sketches {
            assert_eq!(
                persistent.query(query).expect("persistent query"),
                memory.query(query).expect("memory query")
            );
        }
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
}
