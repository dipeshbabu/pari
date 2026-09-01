#![forbid(unsafe_code)]
//! Pluggable storage backends for shared Pari similarity indexes.
//!
//! [`BackendIndex32`] owns `MinHash` compatibility checks, LSH band hashing,
//! batch orchestration, and deterministic candidate aggregation. Concrete
//! backends only persist validated index descriptors, user keys, per-key band
//! hashes, and bucket membership.

use std::{
    collections::{BTreeMap, BTreeSet, HashMap, HashSet},
    error::Error,
    fmt,
    time::{Duration, Instant},
};

use pari_core::MinHash32;
use pari_format::{BucketKey, CodecError};
#[cfg(feature = "redis")]
use pari_format::{KeyCodec, U64Codec};
use pari_index::{BucketDistribution, LshError, LshIndex32, LshParams, QueryMetrics};

#[cfg(feature = "redis")]
mod redis_backend;

#[cfg(feature = "conformance")]
pub mod conformance;

#[cfg(feature = "redis")]
pub use redis_backend::RedisBackend;

#[cfg(feature = "redis")]
const DESCRIPTOR_MAGIC: [u8; 8] = *b"PARIBK01";
#[cfg(feature = "redis")]
const DESCRIPTOR_BYTES: usize = 56;
#[cfg(feature = "redis")]
const NO_RETENTION: u64 = u64::MAX;
const MAX_RETENTION_SECONDS: u64 = (1_u64 << 53) - 1;
const FNV_OFFSET_BASIS: u64 = 0xCBF2_9CE4_8422_2325;
const FNV_PRIME: u64 = 0x0000_0100_0000_01B3;

/// One capability exposed by a storage backend.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BackendCapability {
    /// Multiple bucket reads can be completed as one backend operation.
    BatchRead,
    /// Multiple records can be committed as one backend operation.
    BatchWrite,
    /// Indexed keys can be deleted without rebuilding the complete index.
    Delete,
    /// The backend exposes an explicit completion barrier.
    Flush,
    /// Index-owned data can expire after a configured retention period.
    Ttl,
    /// The backend exposes an operational health probe.
    Health,
    /// The backend is shared outside the current process.
    Remote,
}

/// Compact set of backend capabilities.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BackendCapabilities {
    mask: u8,
}

impl BackendCapabilities {
    const BATCH_READ: u8 = 1 << 0;
    const BATCH_WRITE: u8 = 1 << 1;
    const DELETE: u8 = 1 << 2;
    const FLUSH: u8 = 1 << 3;
    const TTL: u8 = 1 << 4;
    const HEALTH: u8 = 1 << 5;
    const REMOTE: u8 = 1 << 6;

    const MEMORY: Self = Self {
        mask: Self::BATCH_READ | Self::BATCH_WRITE | Self::DELETE | Self::FLUSH | Self::HEALTH,
    };

    #[cfg(feature = "redis")]
    pub(crate) const REDIS: Self = Self {
        mask: Self::BATCH_READ
            | Self::BATCH_WRITE
            | Self::DELETE
            | Self::FLUSH
            | Self::TTL
            | Self::HEALTH
            | Self::REMOTE,
    };

    /// Create an empty capability set for a custom backend implementation.
    #[must_use]
    pub const fn empty() -> Self {
        Self { mask: 0 }
    }

    /// Return a capability set with `capability` enabled.
    #[must_use]
    pub const fn with(self, capability: BackendCapability) -> Self {
        Self {
            mask: self.mask | Self::flag(capability),
        }
    }

    /// Return whether the backend supports `capability`.
    #[must_use]
    pub const fn supports(self, capability: BackendCapability) -> bool {
        self.mask & Self::flag(capability) != 0
    }

    const fn flag(capability: BackendCapability) -> u8 {
        match capability {
            BackendCapability::BatchRead => Self::BATCH_READ,
            BackendCapability::BatchWrite => Self::BATCH_WRITE,
            BackendCapability::Delete => Self::DELETE,
            BackendCapability::Flush => Self::FLUSH,
            BackendCapability::Ttl => Self::TTL,
            BackendCapability::Health => Self::HEALTH,
            BackendCapability::Remote => Self::REMOTE,
        }
    }
}

/// Stable configuration shared by every backend for one index namespace.
#[derive(Debug, Clone, PartialEq)]
pub struct IndexDescriptor {
    threshold: f64,
    num_perm: usize,
    seed: u64,
    params: LshParams,
    retention: Option<Duration>,
}

impl IndexDescriptor {
    /// Construct and validate an index descriptor for a custom backend.
    ///
    /// Backends that persist their own metadata can use this constructor when
    /// rebuilding the descriptor returned by [`StorageBackend::load_descriptor`].
    pub fn new(
        threshold: f64,
        num_perm: usize,
        seed: u64,
        params: LshParams,
        retention: Option<Duration>,
    ) -> Result<Self, BackendError> {
        if retention.is_some_and(|value| {
            value.as_secs() == 0
                || value.subsec_nanos() != 0
                || value.as_secs() > MAX_RETENTION_SECONDS
        }) {
            return Err(BackendError::InvalidRetention);
        }
        LshIndex32::with_params(threshold, num_perm, seed, params).map_err(|error| {
            BackendError::InvalidDescriptor {
                reason: error.to_string(),
            }
        })?;
        Ok(Self {
            threshold,
            num_perm,
            seed,
            params,
            retention,
        })
    }

    /// Return the target Jaccard threshold used to tune the index.
    #[must_use]
    pub const fn threshold(&self) -> f64 {
        self.threshold
    }

    /// Return the required signature width.
    #[must_use]
    pub const fn num_perm(&self) -> usize {
        self.num_perm
    }

    /// Return the required `MinHash` seed.
    #[must_use]
    pub const fn seed(&self) -> u64 {
        self.seed
    }

    /// Return the LSH banding parameters.
    #[must_use]
    pub const fn params(&self) -> LshParams {
        self.params
    }

    /// Return the configured namespace retention period, if any.
    #[must_use]
    pub const fn retention(&self) -> Option<Duration> {
        self.retention
    }
}

/// One prepared backend record.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StoredItem {
    key: u64,
    band_hashes: Vec<u64>,
}

impl StoredItem {
    fn new(key: u64, band_hashes: Vec<u64>) -> Self {
        Self { key, band_hashes }
    }

    /// Return the external integer key.
    #[must_use]
    pub const fn key(&self) -> u64 {
        self.key
    }

    /// Borrow one stable hash per LSH band.
    #[must_use]
    pub fn band_hashes(&self) -> &[u64] {
        &self.band_hashes
    }
}

/// Observable storage statistics independent of a backend product.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BackendStats {
    /// Number of live indexed keys.
    pub items: u64,
    /// Number of live `(bucket, key)` memberships.
    pub bucket_memberships: u64,
    /// Network round trips performed by this backend handle.
    /// In-process backends report zero.
    pub round_trips: u64,
    /// Remaining namespace retention time when supported and active.
    pub ttl_seconds_remaining: Option<u64>,
    /// Exact bucket distribution when the backend can provide it without an
    /// additional scan or round trip.
    pub bucket_distribution: Option<BucketDistribution>,
    /// Process-local application query metrics when observation is enabled on
    /// [`BackendIndex32`].
    pub queries: Option<QueryMetrics>,
}

/// Errors produced by pluggable storage implementations.
#[derive(Debug)]
pub enum BackendError {
    /// The backend namespace already owns index data.
    AlreadyExists,
    /// No initialized index exists in the backend namespace.
    NotFound,
    /// A key already exists or appears more than once in one insertion batch.
    DuplicateKey { key: u64 },
    /// The backend cannot provide the requested capability.
    UnsupportedCapability { capability: BackendCapability },
    /// Retention must be an exactly representable supported whole-second value.
    InvalidRetention,
    /// A persisted index descriptor failed semantic validation.
    InvalidDescriptor { reason: String },
    /// A backend namespace failed validation.
    InvalidNamespace { reason: String },
    /// Backend data failed bounded decoding or structural validation.
    CorruptData { reason: String },
    /// Integer conversion or size arithmetic overflowed.
    LengthOverflow,
    /// A safe key codec rejected backend data.
    Codec(CodecError),
    /// A remote operation failed. URLs and credentials are never included.
    Transport {
        operation: &'static str,
        message: String,
    },
}

impl fmt::Display for BackendError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::AlreadyExists => {
                formatter.write_str("backend namespace already contains an index")
            }
            Self::NotFound => formatter.write_str("backend namespace does not contain an index"),
            Self::DuplicateKey { key } => {
                write!(formatter, "key {key} already exists in the index")
            }
            Self::UnsupportedCapability { capability } => {
                write!(formatter, "backend does not support {capability:?}")
            }
            Self::InvalidRetention => write!(
                formatter,
                "retention must be a whole number of seconds in 1..={MAX_RETENTION_SECONDS}"
            ),
            Self::InvalidDescriptor { reason } => write!(formatter, "invalid descriptor: {reason}"),
            Self::InvalidNamespace { reason } => write!(formatter, "invalid namespace: {reason}"),
            Self::CorruptData { reason } => write!(formatter, "invalid backend data: {reason}"),
            Self::LengthOverflow => formatter.write_str("backend length arithmetic overflowed"),
            Self::Codec(error) => error.fmt(formatter),
            Self::Transport { operation, message } => {
                write!(formatter, "backend {operation} failed: {message}")
            }
        }
    }
}

impl Error for BackendError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Codec(error) => Some(error),
            _ => None,
        }
    }
}

impl From<CodecError> for BackendError {
    fn from(error: CodecError) -> Self {
        Self::Codec(error)
    }
}

/// Errors returned by [`BackendIndex32`].
#[derive(Debug)]
pub enum BackendIndexError {
    /// LSH or `MinHash` compatibility validation failed.
    Index(LshError),
    /// The selected storage backend failed.
    Backend(BackendError),
}

impl fmt::Display for BackendIndexError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Index(error) => error.fmt(formatter),
            Self::Backend(error) => error.fmt(formatter),
        }
    }
}

impl Error for BackendIndexError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Index(error) => Some(error),
            Self::Backend(error) => Some(error),
        }
    }
}

impl From<LshError> for BackendIndexError {
    fn from(error: LshError) -> Self {
        Self::Index(error)
    }
}

impl From<BackendError> for BackendIndexError {
    fn from(error: BackendError) -> Self {
        Self::Backend(error)
    }
}

/// Typed storage contract used by shared LSH indexes.
///
/// Scalar application APIs live on [`BackendIndex32`]. Backends implement batch
/// primitives directly so remote implementations do not need one network round
/// trip per application record or query bucket.
pub trait StorageBackend {
    /// Return operations implemented natively by this backend.
    fn capabilities(&self) -> BackendCapabilities;

    /// Initialize an empty namespace with one validated index descriptor.
    fn initialize(&mut self, descriptor: &IndexDescriptor) -> Result<(), BackendError>;

    /// Load the descriptor required to open an existing namespace.
    fn load_descriptor(&mut self) -> Result<IndexDescriptor, BackendError>;

    /// Test key existence while preserving input order.
    fn contains_many(&mut self, keys: &[u64]) -> Result<Vec<bool>, BackendError>;

    /// Atomically insert one validated batch or fail without partial mutation.
    fn insert_many(&mut self, items: &[StoredItem]) -> Result<(), BackendError>;

    /// Read candidate members for each requested bucket in input order.
    fn query_buckets(&mut self, buckets: &[BucketKey]) -> Result<Vec<Vec<u64>>, BackendError>;

    /// Delete keys in one backend batch, returning the number actually removed.
    fn delete_many(&mut self, keys: &[u64]) -> Result<usize, BackendError>;

    /// Complete writes issued before this call.
    fn flush(&mut self) -> Result<(), BackendError>;

    /// Return backend health or an actionable error.
    fn health(&mut self) -> Result<(), BackendError>;

    /// Return current backend statistics.
    fn stats(&mut self) -> Result<BackendStats, BackendError>;

    /// Delete only data owned by this backend namespace.
    fn cleanup(&mut self) -> Result<(), BackendError>;
}

/// Batch-first LSH index backed by any [`StorageBackend`].
#[derive(Debug)]
pub struct BackendIndex32<B> {
    backend: B,
    descriptor: IndexDescriptor,
    query_metrics: Option<QueryMetrics>,
    known_items: Option<usize>,
}

impl<B: StorageBackend> BackendIndex32<B> {
    /// Create a new backend index with automatically tuned LSH parameters.
    pub fn create(
        backend: B,
        threshold: f64,
        num_perm: usize,
        seed: u64,
        retention: Option<Duration>,
    ) -> Result<Self, BackendIndexError> {
        let reference = LshIndex32::new(threshold, num_perm, seed)?;
        Self::create_with_params(
            backend,
            threshold,
            num_perm,
            seed,
            reference.params(),
            retention,
        )
    }

    /// Create a new backend index with explicit LSH parameters.
    pub fn create_with_params(
        mut backend: B,
        threshold: f64,
        num_perm: usize,
        seed: u64,
        params: LshParams,
        retention: Option<Duration>,
    ) -> Result<Self, BackendIndexError> {
        let descriptor = IndexDescriptor::new(threshold, num_perm, seed, params, retention)?;
        validate_retention_capability(&backend, &descriptor)?;
        backend.initialize(&descriptor)?;
        Ok(Self {
            backend,
            descriptor,
            query_metrics: None,
            known_items: Some(0),
        })
    }

    /// Open an existing index using its backend-owned descriptor.
    pub fn open(mut backend: B) -> Result<Self, BackendIndexError> {
        let descriptor = backend.load_descriptor()?;
        LshIndex32::with_params(
            descriptor.threshold,
            descriptor.num_perm,
            descriptor.seed,
            descriptor.params,
        )?;
        validate_retention_capability(&backend, &descriptor)?;
        Ok(Self {
            backend,
            descriptor,
            query_metrics: None,
            known_items: None,
        })
    }

    /// Insert one external key and compatible signature.
    pub fn insert(&mut self, key: u64, sketch: &MinHash32) -> Result<(), BackendIndexError> {
        self.insert_many(std::iter::once((key, sketch)))
    }

    /// Insert a complete batch through one backend batch operation.
    pub fn insert_many<'a>(
        &mut self,
        items: impl IntoIterator<Item = (u64, &'a MinHash32)>,
    ) -> Result<(), BackendIndexError> {
        let items: Vec<_> = items.into_iter().collect();
        let mut seen = HashSet::with_capacity(items.len());
        let mut stored = Vec::with_capacity(items.len());
        for (key, sketch) in items {
            if !seen.insert(key) {
                return Err(BackendError::DuplicateKey { key }.into());
            }
            stored.push(StoredItem::new(key, self.band_hashes(sketch)?));
        }
        self.backend.insert_many(&stored)?;
        if let Some(known_items) = &mut self.known_items {
            *known_items = known_items.saturating_add(stored.len());
        }
        Ok(())
    }

    /// Query approximate candidate keys for one signature.
    pub fn query(&mut self, sketch: &MinHash32) -> Result<Vec<u64>, BackendIndexError> {
        let mut results = self.query_many(std::iter::once(sketch))?;
        Ok(results.pop().unwrap_or_default())
    }

    /// Query many signatures with one backend batch read.
    pub fn query_many<'a>(
        &mut self,
        sketches: impl IntoIterator<Item = &'a MinHash32>,
    ) -> Result<Vec<Vec<u64>>, BackendIndexError> {
        let started = self.query_metrics.as_ref().map(|_| Instant::now());
        let mut unique_positions = BTreeMap::<BucketKey, usize>::new();
        let mut unique_buckets = Vec::new();
        let mut query_positions = Vec::new();

        for sketch in sketches {
            let hashes = self.band_hashes(sketch)?;
            let mut positions = Vec::with_capacity(hashes.len());
            for (band, hash) in hashes.into_iter().enumerate() {
                let band = u32::try_from(band).map_err(|_| BackendError::LengthOverflow)?;
                let bucket = BucketKey::new(band, hash);
                let position = if let Some(position) = unique_positions.get(&bucket) {
                    *position
                } else {
                    let position = unique_buckets.len();
                    unique_buckets.push(bucket);
                    unique_positions.insert(bucket, position);
                    position
                };
                positions.push(position);
            }
            query_positions.push(positions);
        }

        if query_positions.is_empty() {
            return Ok(Vec::new());
        }

        let bucket_members = self.backend.query_buckets(&unique_buckets)?;
        if bucket_members.len() != unique_buckets.len() {
            return Err(BackendError::CorruptData {
                reason: format!(
                    "backend returned {} bucket rows for {} requested buckets",
                    bucket_members.len(),
                    unique_buckets.len()
                ),
            }
            .into());
        }

        let mut candidates = HashSet::new();
        let mut output = Vec::with_capacity(query_positions.len());
        let mut candidate_count = 0_usize;
        for positions in query_positions {
            candidates.clear();
            for position in positions {
                candidates.extend(bucket_members[position].iter().copied());
            }
            let mut keys: Vec<_> = candidates.iter().copied().collect();
            keys.sort_unstable();
            candidate_count = candidate_count.saturating_add(keys.len());
            output.push(keys);
        }
        if let (Some(metrics), Some(started)) = (&mut self.query_metrics, started) {
            let possible = self
                .known_items
                .map_or(0, |items| items.saturating_mul(output.len()));
            metrics.record(output.len(), candidate_count, possible, started.elapsed());
        }
        Ok(output)
    }

    /// Remove one key, returning whether it existed.
    pub fn remove(&mut self, key: u64) -> Result<bool, BackendIndexError> {
        let removed = self.backend.delete_many(&[key])? == 1;
        if removed {
            if let Some(known_items) = &mut self.known_items {
                *known_items = known_items.saturating_sub(1);
            }
        }
        Ok(removed)
    }

    /// Remove a batch of keys, returning the number that existed.
    pub fn remove_many(
        &mut self,
        keys: impl IntoIterator<Item = u64>,
    ) -> Result<usize, BackendIndexError> {
        let keys: BTreeSet<_> = keys.into_iter().collect();
        let keys: Vec<_> = keys.into_iter().collect();
        let removed = self.backend.delete_many(&keys)?;
        if let Some(known_items) = &mut self.known_items {
            *known_items = known_items.saturating_sub(removed);
        }
        Ok(removed)
    }

    /// Return whether one key exists.
    pub fn contains(&mut self, key: u64) -> Result<bool, BackendIndexError> {
        let values = self.contains_many(&[key])?;
        values.first().copied().ok_or_else(|| {
            BackendError::CorruptData {
                reason: "backend returned no contains result".to_owned(),
            }
            .into()
        })
    }

    /// Test many keys in one backend operation.
    pub fn contains_many(&mut self, keys: &[u64]) -> Result<Vec<bool>, BackendIndexError> {
        let values = self.backend.contains_many(keys)?;
        if values.len() != keys.len() {
            return Err(BackendError::CorruptData {
                reason: format!(
                    "backend returned {} contains results for {} keys",
                    values.len(),
                    keys.len()
                ),
            }
            .into());
        }
        Ok(values)
    }

    /// Complete outstanding backend writes.
    pub fn flush(&mut self) -> Result<(), BackendIndexError> {
        self.backend.flush()?;
        Ok(())
    }

    /// Probe backend health.
    pub fn health(&mut self) -> Result<(), BackendIndexError> {
        self.backend.health()?;
        Ok(())
    }

    /// Return backend statistics.
    pub fn stats(&mut self) -> Result<BackendStats, BackendIndexError> {
        let mut stats = self.backend.stats()?;
        self.known_items = usize::try_from(stats.items).ok();
        stats.queries = self.query_metrics;
        Ok(stats)
    }

    /// Enable or disable process-local query observation.
    pub fn set_observability(&mut self, enabled: bool) {
        self.query_metrics = enabled.then(QueryMetrics::default);
    }

    /// Return the configured index descriptor.
    #[must_use]
    pub const fn descriptor(&self) -> &IndexDescriptor {
        &self.descriptor
    }

    /// Borrow the concrete backend for backend-specific inspection.
    #[must_use]
    pub const fn backend(&self) -> &B {
        &self.backend
    }

    /// Mutably borrow the concrete backend.
    #[must_use]
    pub fn backend_mut(&mut self) -> &mut B {
        &mut self.backend
    }

    /// Delete only this backend namespace's owned data.
    pub fn cleanup(mut self) -> Result<(), BackendIndexError> {
        self.backend.cleanup()?;
        Ok(())
    }

    /// Consume the index and return its concrete backend handle.
    pub fn into_backend(self) -> B {
        self.backend
    }

    fn band_hashes(&self, sketch: &MinHash32) -> Result<Vec<u64>, LshError> {
        if sketch.seed() != self.descriptor.seed {
            return Err(LshError::IncompatibleSeed {
                expected: self.descriptor.seed,
                actual: sketch.seed(),
            });
        }
        if sketch.num_perm() != self.descriptor.num_perm {
            return Err(LshError::IncompatiblePermutationCount {
                expected: self.descriptor.num_perm,
                actual: sketch.num_perm(),
            });
        }
        let used = self
            .descriptor
            .params
            .used_permutations()
            .expect("descriptor parameters were validated during create/open");
        Ok(sketch.signature()[..used]
            .chunks_exact(self.descriptor.params.rows)
            .map(hash_band)
            .collect())
    }
}

fn validate_retention_capability<B: StorageBackend>(
    backend: &B,
    descriptor: &IndexDescriptor,
) -> Result<(), BackendError> {
    if descriptor.retention.is_some() && !backend.capabilities().supports(BackendCapability::Ttl) {
        return Err(BackendError::UnsupportedCapability {
            capability: BackendCapability::Ttl,
        });
    }
    Ok(())
}

/// In-process reference implementation of [`StorageBackend`].
///
/// The memory backend intentionally has no TTL support. It is useful for
/// contract tests and callers that want the shared-backend API without a remote
/// service.
#[derive(Debug, Default)]
pub struct MemoryBackend {
    descriptor: Option<IndexDescriptor>,
    records: HashMap<u64, Vec<u64>>,
    buckets: BTreeMap<BucketKey, BTreeSet<u64>>,
}

impl MemoryBackend {
    /// Create an empty uninitialized memory backend.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }
}

impl StorageBackend for MemoryBackend {
    fn capabilities(&self) -> BackendCapabilities {
        BackendCapabilities::MEMORY
    }

    fn initialize(&mut self, descriptor: &IndexDescriptor) -> Result<(), BackendError> {
        if self.descriptor.is_some() || !self.records.is_empty() || !self.buckets.is_empty() {
            return Err(BackendError::AlreadyExists);
        }
        if descriptor.retention.is_some() {
            return Err(BackendError::UnsupportedCapability {
                capability: BackendCapability::Ttl,
            });
        }
        self.descriptor = Some(descriptor.clone());
        Ok(())
    }

    fn load_descriptor(&mut self) -> Result<IndexDescriptor, BackendError> {
        self.descriptor.clone().ok_or(BackendError::NotFound)
    }

    fn contains_many(&mut self, keys: &[u64]) -> Result<Vec<bool>, BackendError> {
        Ok(keys
            .iter()
            .map(|key| self.records.contains_key(key))
            .collect())
    }

    fn insert_many(&mut self, items: &[StoredItem]) -> Result<(), BackendError> {
        let descriptor = self.descriptor.as_ref().ok_or(BackendError::NotFound)?;
        let expected_bands = descriptor.params.bands;
        let mut seen = HashSet::with_capacity(items.len());
        for item in items {
            if item.band_hashes.len() != expected_bands {
                return Err(BackendError::CorruptData {
                    reason: format!(
                        "stored item has {} band hashes; expected {expected_bands}",
                        item.band_hashes.len()
                    ),
                });
            }
            if self.records.contains_key(&item.key) || !seen.insert(item.key) {
                return Err(BackendError::DuplicateKey { key: item.key });
            }
        }

        for item in items {
            self.records.insert(item.key, item.band_hashes.clone());
            for (band, hash) in item.band_hashes.iter().copied().enumerate() {
                let band = u32::try_from(band).map_err(|_| BackendError::LengthOverflow)?;
                self.buckets
                    .entry(BucketKey::new(band, hash))
                    .or_default()
                    .insert(item.key);
            }
        }
        Ok(())
    }

    fn query_buckets(&mut self, buckets: &[BucketKey]) -> Result<Vec<Vec<u64>>, BackendError> {
        Ok(buckets
            .iter()
            .map(|bucket| {
                self.buckets
                    .get(bucket)
                    .map_or_else(Vec::new, |members| members.iter().copied().collect())
            })
            .collect())
    }

    fn delete_many(&mut self, keys: &[u64]) -> Result<usize, BackendError> {
        if self.descriptor.is_none() {
            return Err(BackendError::NotFound);
        }
        let mut removed = 0_usize;
        for key in keys {
            let Some(hashes) = self.records.remove(key) else {
                continue;
            };
            removed += 1;
            for (band, hash) in hashes.into_iter().enumerate() {
                let band = u32::try_from(band).map_err(|_| BackendError::LengthOverflow)?;
                let bucket = BucketKey::new(band, hash);
                let remove_bucket = if let Some(members) = self.buckets.get_mut(&bucket) {
                    members.remove(key);
                    members.is_empty()
                } else {
                    false
                };
                if remove_bucket {
                    self.buckets.remove(&bucket);
                }
            }
        }
        Ok(removed)
    }

    fn flush(&mut self) -> Result<(), BackendError> {
        if self.descriptor.is_none() {
            return Err(BackendError::NotFound);
        }
        Ok(())
    }

    fn health(&mut self) -> Result<(), BackendError> {
        Ok(())
    }

    fn stats(&mut self) -> Result<BackendStats, BackendError> {
        if self.descriptor.is_none() {
            return Err(BackendError::NotFound);
        }
        let items = u64::try_from(self.records.len()).map_err(|_| BackendError::LengthOverflow)?;
        let memberships = self.buckets.values().try_fold(0_u64, |total, members| {
            let count = u64::try_from(members.len()).map_err(|_| BackendError::LengthOverflow)?;
            total.checked_add(count).ok_or(BackendError::LengthOverflow)
        })?;
        Ok(BackendStats {
            items,
            bucket_memberships: memberships,
            round_trips: 0,
            ttl_seconds_remaining: None,
            bucket_distribution: Some(BucketDistribution::from_sizes(
                self.buckets.values().map(BTreeSet::len),
            )),
            queries: None,
        })
    }

    fn cleanup(&mut self) -> Result<(), BackendError> {
        self.descriptor = None;
        self.records.clear();
        self.buckets.clear();
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

#[cfg(feature = "redis")]
pub(crate) fn encode_descriptor(descriptor: &IndexDescriptor) -> Result<Vec<u8>, BackendError> {
    let mut bytes = Vec::with_capacity(DESCRIPTOR_BYTES);
    bytes.extend_from_slice(&DESCRIPTOR_MAGIC);
    bytes.extend_from_slice(&descriptor.threshold.to_bits().to_le_bytes());
    bytes.extend_from_slice(&usize_to_u64(descriptor.num_perm)?.to_le_bytes());
    bytes.extend_from_slice(&descriptor.seed.to_le_bytes());
    bytes.extend_from_slice(&usize_to_u64(descriptor.params.bands)?.to_le_bytes());
    bytes.extend_from_slice(&usize_to_u64(descriptor.params.rows)?.to_le_bytes());
    let retention = descriptor
        .retention
        .map_or(NO_RETENTION, |value| value.as_secs());
    bytes.extend_from_slice(&retention.to_le_bytes());
    debug_assert_eq!(bytes.len(), DESCRIPTOR_BYTES);
    Ok(bytes)
}

#[cfg(feature = "redis")]
pub(crate) fn decode_descriptor(bytes: &[u8]) -> Result<IndexDescriptor, BackendError> {
    if bytes.len() != DESCRIPTOR_BYTES {
        return Err(BackendError::CorruptData {
            reason: format!(
                "descriptor is {} bytes; expected {DESCRIPTOR_BYTES}",
                bytes.len()
            ),
        });
    }
    if bytes[..8] != DESCRIPTOR_MAGIC {
        return Err(BackendError::CorruptData {
            reason: "descriptor magic or version is unsupported".to_owned(),
        });
    }
    let threshold = f64::from_bits(read_u64(bytes, 8)?);
    let num_perm = u64_to_usize(read_u64(bytes, 16)?)?;
    let seed = read_u64(bytes, 24)?;
    let bands = u64_to_usize(read_u64(bytes, 32)?)?;
    let rows = u64_to_usize(read_u64(bytes, 40)?)?;
    let retention = match read_u64(bytes, 48)? {
        NO_RETENTION => None,
        0 => return Err(BackendError::InvalidRetention),
        seconds => Some(Duration::from_secs(seconds)),
    };
    IndexDescriptor::new(
        threshold,
        num_perm,
        seed,
        LshParams::new(bands, rows),
        retention,
    )
}

#[cfg(feature = "redis")]
pub(crate) fn encode_user_key(key: u64) -> Result<Vec<u8>, BackendError> {
    Ok(U64Codec.encode(&key)?)
}

#[cfg(feature = "redis")]
pub(crate) fn decode_user_key(bytes: &[u8]) -> Result<u64, BackendError> {
    Ok(U64Codec.decode(bytes)?)
}

#[cfg(feature = "redis")]
fn read_u64(bytes: &[u8], offset: usize) -> Result<u64, BackendError> {
    let end = offset.checked_add(8).ok_or(BackendError::LengthOverflow)?;
    let raw: [u8; 8] = bytes
        .get(offset..end)
        .ok_or_else(|| BackendError::CorruptData {
            reason: "descriptor is truncated".to_owned(),
        })?
        .try_into()
        .map_err(|_| BackendError::CorruptData {
            reason: "descriptor integer width is invalid".to_owned(),
        })?;
    Ok(u64::from_le_bytes(raw))
}

#[cfg(feature = "redis")]
fn usize_to_u64(value: usize) -> Result<u64, BackendError> {
    u64::try_from(value).map_err(|_| BackendError::LengthOverflow)
}

#[cfg(feature = "redis")]
fn u64_to_usize(value: u64) -> Result<usize, BackendError> {
    usize::try_from(value).map_err(|_| BackendError::LengthOverflow)
}

#[cfg(all(test, feature = "redis"))]
mod tests {
    use std::time::Duration;

    use pari_index::LshParams;

    use super::{decode_descriptor, encode_descriptor, BackendError, IndexDescriptor};

    fn descriptor(retention: Duration) -> Result<IndexDescriptor, BackendError> {
        IndexDescriptor::new(0.8, 128, 7, LshParams::new(32, 4), Some(retention))
    }

    #[test]
    fn retention_requires_supported_whole_seconds() {
        for invalid in [
            Duration::ZERO,
            Duration::from_nanos(1),
            Duration::from_millis(999),
            Duration::new(1, 1),
            Duration::from_secs(super::MAX_RETENTION_SECONDS + 1),
            Duration::from_secs(u64::MAX),
        ] {
            assert!(matches!(
                descriptor(invalid),
                Err(BackendError::InvalidRetention)
            ));
        }
    }

    #[test]
    fn accepted_retention_round_trips_exactly() {
        for seconds in [1, 300, super::MAX_RETENTION_SECONDS] {
            let original = descriptor(Duration::from_secs(seconds)).expect("valid retention");
            let encoded = encode_descriptor(&original).expect("encode descriptor");
            let decoded = decode_descriptor(&encoded).expect("decode descriptor");
            assert_eq!(decoded, original);
        }
    }
}
