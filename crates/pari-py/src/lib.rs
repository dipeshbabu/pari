#![forbid(unsafe_code)]
//! `PyO3` bindings for Pari's stable Rust APIs.

use std::{
    num::NonZeroUsize,
    path::PathBuf,
    sync::{Arc, Mutex, TryLockError},
};

use pari_core::{BatchThreads, MinHash32, MinHash64, MinHashError};
use pari_index::{
    plan_lsh, DuplicateGroup, LshError, LshIndex32, LshPlan, LshPlanError, LshPlanOptions,
    StorageMode,
};
use pari_store::{PersistentIndex32, PersistentIndex64, StoreError, StoreStats};
use pyo3::{
    create_exception,
    exceptions::PyException,
    prelude::*,
    types::{PyAny, PyBytes, PyModule},
};

create_exception!(_native, PariError, PyException);
create_exception!(_native, ConfigurationError, PariError);
create_exception!(_native, CompatibilityError, PariError);
create_exception!(_native, DuplicateKeyError, PariError);
create_exception!(_native, StorageError, PariError);
create_exception!(_native, ClosedIndexError, PariError);

#[derive(Debug, Clone, Copy)]
enum BindingErrorKind {
    Closed,
    Configuration,
    Compatibility,
    DuplicateKey,
    Storage,
}

#[derive(Debug)]
enum BindingError {
    Busy,
    Closed,
    Poisoned,
    Index(LshError),
    Plan(LshPlanError),
    MinHash(MinHashError),
    Store(StoreError),
}

impl BindingError {
    fn kind(&self) -> BindingErrorKind {
        match self {
            Self::Closed => BindingErrorKind::Closed,
            Self::MinHash(MinHashError::InvalidPermutationCount { .. }) => {
                BindingErrorKind::Configuration
            }
            Self::MinHash(
                MinHashError::IncompatibleSeed { .. }
                | MinHashError::IncompatiblePermutationCount { .. },
            )
            | Self::Index(
                LshError::IncompatibleSeed { .. } | LshError::IncompatiblePermutationCount { .. },
            )
            | Self::Store(
                StoreError::IncompatibleSeed { .. }
                | StoreError::IncompatiblePermutationCount { .. }
                | StoreError::InvalidSnapshot {
                    reason: "snapshot signature scheme does not match the persistent index type",
                },
            ) => BindingErrorKind::Compatibility,
            Self::Index(LshError::DuplicateKey { .. })
            | Self::Store(StoreError::DuplicateKey { .. }) => BindingErrorKind::DuplicateKey,
            Self::Index(
                LshError::InvalidThreshold { .. }
                | LshError::InvalidPermutationCount { .. }
                | LshError::AutomaticTuningTooLarge { .. }
                | LshError::InvalidParams { .. },
            )
            | Self::Store(StoreError::Index(
                LshError::InvalidThreshold { .. }
                | LshError::InvalidPermutationCount { .. }
                | LshError::AutomaticTuningTooLarge { .. }
                | LshError::InvalidParams { .. },
            ))
            | Self::Plan(_) => BindingErrorKind::Configuration,
            Self::Busy
            | Self::Poisoned
            | Self::MinHash(MinHashError::BatchPoolUnavailable { .. })
            | Self::Index(_)
            | Self::Store(_) => BindingErrorKind::Storage,
        }
    }

    fn into_message(self) -> String {
        match self {
            Self::Busy => "index is busy running a callback".into(),
            Self::Closed => "index is closed".into(),
            Self::Poisoned => "index state lock is poisoned".into(),
            Self::Index(error) => error.to_string(),
            Self::Plan(error) => error.to_string(),
            Self::MinHash(error) => error.to_string(),
            Self::Store(error) => error.to_string(),
        }
    }
}

impl<T> From<TryLockError<T>> for BindingError {
    fn from(error: TryLockError<T>) -> Self {
        match error {
            TryLockError::Poisoned(_) => Self::Poisoned,
            TryLockError::WouldBlock => Self::Busy,
        }
    }
}

impl From<MinHashError> for BindingError {
    fn from(error: MinHashError) -> Self {
        Self::MinHash(error)
    }
}

impl From<LshError> for BindingError {
    fn from(error: LshError) -> Self {
        Self::Index(error)
    }
}

impl From<LshPlanError> for BindingError {
    fn from(error: LshPlanError) -> Self {
        Self::Plan(error)
    }
}

impl From<StoreError> for BindingError {
    fn from(error: StoreError) -> Self {
        Self::Store(error)
    }
}

fn binding_error(error: BindingError) -> PyErr {
    let kind = error.kind();
    let message = error.into_message();
    match kind {
        BindingErrorKind::Closed => ClosedIndexError::new_err(message),
        BindingErrorKind::Configuration => ConfigurationError::new_err(message),
        BindingErrorKind::Compatibility => CompatibilityError::new_err(message),
        BindingErrorKind::DuplicateKey => DuplicateKeyError::new_err(message),
        BindingErrorKind::Storage => StorageError::new_err(message),
    }
}

fn owned_bytes(py: Python<'_>, value: &Bound<'_, PyAny>) -> PyResult<Vec<u8>> {
    if let Ok(bytes) = value.cast::<PyBytes>() {
        return Ok(bytes.as_bytes().to_vec());
    }
    if let Ok(bytes) = value.extract::<Vec<u8>>() {
        return Ok(bytes);
    }

    // `PyO3`'s low-level buffer module is not exposed by abi3-py310. A Python
    // memoryview still validates the generic buffer protocol through the stable
    // ABI, and `tobytes` gives Rust-owned memory before detached work starts.
    let builtins = py.import("builtins")?;
    let view = builtins.getattr("memoryview")?.call1((value,))?;
    let bytes = view.call_method0("tobytes")?;
    Ok(bytes.cast::<PyBytes>()?.as_bytes().to_vec())
}

fn collect_byte_values(py: Python<'_>, values: &Bound<'_, PyAny>) -> PyResult<Vec<Vec<u8>>> {
    values
        .try_iter()?
        .map(|item| owned_bytes(py, &item?))
        .collect()
}

fn collect_feature_rows(py: Python<'_>, rows: &Bound<'_, PyAny>) -> PyResult<Vec<Vec<Vec<u8>>>> {
    rows.try_iter()?
        .map(|row| collect_byte_values(py, &row?))
        .collect()
}

fn indexed_signature_error(py: Python<'_>, error: &PyErr, index: usize) -> PyErr {
    let message = error.value(py).str().map_or_else(
        |_| error.to_string(),
        |message| message.to_string_lossy().into_owned(),
    );
    PyErr::from_type(error.get_type(py), format!("signature[{index}]: {message}"))
}

fn collect_signature64(py: Python<'_>, signature: &Bound<'_, PyAny>) -> PyResult<Vec<u64>> {
    signature
        .try_iter()?
        .enumerate()
        .map(|(index, value)| {
            value
                .map_err(|error| indexed_signature_error(py, &error, index))?
                .extract::<u64>()
                .map_err(|error| indexed_signature_error(py, &error, index))
        })
        .collect()
}

fn build_sketches(
    rows: &[Vec<Vec<u8>>],
    num_perm: usize,
    seed: u64,
    threads: BatchThreads,
) -> Result<Vec<MinHash32>, BindingError> {
    MinHash32::from_batch(rows, num_perm, seed, threads).map_err(BindingError::from)
}

fn build_sketches64(
    rows: &[Vec<Vec<u8>>],
    num_perm: usize,
    seed: u64,
    threads: BatchThreads,
) -> Result<Vec<MinHash64>, BindingError> {
    MinHash64::from_batch(rows, num_perm, seed, threads).map_err(BindingError::from)
}

fn batch_threads(threads: Option<usize>) -> PyResult<BatchThreads> {
    match threads {
        None => Ok(BatchThreads::Auto),
        Some(1) => Ok(BatchThreads::Sequential),
        Some(threads) => NonZeroUsize::new(threads)
            .map(BatchThreads::max)
            .ok_or_else(|| ConfigurationError::new_err("threads must be a positive integer")),
    }
}

fn owned_groups(groups: Vec<DuplicateGroup>) -> Vec<(u64, Vec<u64>)> {
    groups
        .into_iter()
        .map(|group| (group.representative(), group.members().to_vec()))
        .collect()
}

/// Pari's 32-bit affine `MinHash` sketch.
#[pyclass(module = "pari._native", name = "MinHash", skip_from_py_object)]
#[derive(Debug, Clone)]
struct PyMinHash {
    inner: MinHash32,
}

#[pymethods]
impl PyMinHash {
    #[new]
    #[pyo3(signature = (num_perm = 128, seed = 1))]
    fn new(num_perm: usize, seed: u64) -> PyResult<Self> {
        MinHash32::new(num_perm, seed)
            .map(|inner| Self { inner })
            .map_err(|error| binding_error(error.into()))
    }

    /// Construct a sketch from an iterable of byte-like values.
    #[staticmethod]
    #[pyo3(signature = (values, *, num_perm = 128, seed = 1))]
    fn from_values(
        py: Python<'_>,
        values: &Bound<'_, PyAny>,
        num_perm: usize,
        seed: u64,
    ) -> PyResult<Self> {
        let values = collect_byte_values(py, values)?;
        let result = py.detach(move || {
            let mut sketch = MinHash32::new(num_perm, seed).map_err(BindingError::from)?;
            sketch.update_many(values);
            Ok::<_, BindingError>(sketch)
        });
        result.map(|inner| Self { inner }).map_err(binding_error)
    }

    /// Construct a batch of sketches after copying all Python feature buffers.
    #[staticmethod]
    #[pyo3(signature = (rows, *, num_perm = 128, seed = 1, threads = None))]
    fn from_batch(
        py: Python<'_>,
        rows: &Bound<'_, PyAny>,
        num_perm: usize,
        seed: u64,
        threads: Option<usize>,
    ) -> PyResult<Vec<Py<Self>>> {
        let rows = collect_feature_rows(py, rows)?;
        let threads = batch_threads(threads)?;
        let sketches = py
            .detach(move || build_sketches(&rows, num_perm, seed, threads))
            .map_err(binding_error)?;
        sketches
            .into_iter()
            .map(|inner| Py::new(py, Self { inner }))
            .collect()
    }

    /// Reconstruct a Pari affine32 sketch from a compatible signature.
    #[staticmethod]
    #[pyo3(signature = (signature, *, seed = 1))]
    fn from_signature(signature: Vec<u32>, seed: u64) -> PyResult<Self> {
        MinHash32::from_signature(signature, seed)
            .map(|inner| Self { inner })
            .map_err(|error| binding_error(error.into()))
    }

    /// Update from one bytes or byte-buffer value.
    fn update(&mut self, py: Python<'_>, value: &Bound<'_, PyAny>) -> PyResult<()> {
        if let Ok(bytes) = value.cast::<PyBytes>() {
            self.inner.update(bytes.as_bytes());
            return Ok(());
        }
        let value = owned_bytes(py, value)?;
        let mut sketch = self.inner.clone();
        self.inner = py.detach(move || {
            sketch.update(&value);
            sketch
        });
        Ok(())
    }

    /// Batch update while releasing the Python GIL for hashing and permutations.
    fn update_many(&mut self, py: Python<'_>, values: &Bound<'_, PyAny>) -> PyResult<()> {
        let values = collect_byte_values(py, values)?;
        let mut sketch = self.inner.clone();
        self.inner = py.detach(move || {
            sketch.update_many(values);
            sketch
        });
        Ok(())
    }

    fn jaccard(&self, other: &Bound<'_, PyAny>) -> PyResult<f64> {
        let other = clone_minhash32(other)?;
        self.inner
            .jaccard(&other)
            .map_err(|error| binding_error(error.into()))
    }

    fn merge(&mut self, other: &Bound<'_, PyAny>) -> PyResult<()> {
        let other = clone_minhash32(other)?;
        self.inner
            .merge(&other)
            .map_err(|error| binding_error(error.into()))
    }

    fn clear(&mut self) {
        self.inner.clear();
    }

    #[getter]
    fn seed(&self) -> u64 {
        self.inner.seed()
    }

    #[getter]
    fn num_perm(&self) -> usize {
        self.inner.num_perm()
    }

    #[getter]
    fn scheme(&self) -> &'static str {
        self.inner.scheme()
    }

    #[getter]
    fn signature(&self) -> Vec<u32> {
        self.inner.signature().to_vec()
    }

    #[getter]
    fn permutations(&self) -> (Vec<u32>, Vec<u32>) {
        let (multipliers, offsets) = self.inner.permutations();
        (multipliers.to_vec(), offsets.to_vec())
    }

    #[getter]
    fn is_empty(&self) -> bool {
        self.inner.is_empty()
    }

    fn __len__(&self) -> usize {
        self.inner.num_perm()
    }

    fn __repr__(&self) -> String {
        format!(
            "MinHash(num_perm={}, seed={}, scheme='{}')",
            self.inner.num_perm(),
            self.inner.seed(),
            self.inner.scheme()
        )
    }
}

/// Pari's full-width 64-bit affine `MinHash` sketch.
#[pyclass(module = "pari._native", name = "MinHash64", skip_from_py_object)]
#[derive(Debug, Clone)]
struct PyMinHash64 {
    inner: MinHash64,
}

#[pymethods]
impl PyMinHash64 {
    #[new]
    #[pyo3(signature = (num_perm = 128, seed = 1))]
    fn new(num_perm: usize, seed: u64) -> PyResult<Self> {
        MinHash64::new(num_perm, seed)
            .map(|inner| Self { inner })
            .map_err(|error| binding_error(error.into()))
    }

    /// Construct a sketch from an iterable of byte-like values.
    #[staticmethod]
    #[pyo3(signature = (values, *, num_perm = 128, seed = 1))]
    fn from_values(
        py: Python<'_>,
        values: &Bound<'_, PyAny>,
        num_perm: usize,
        seed: u64,
    ) -> PyResult<Self> {
        let values = collect_byte_values(py, values)?;
        let result = py.detach(move || {
            let mut sketch = MinHash64::new(num_perm, seed).map_err(BindingError::from)?;
            sketch.update_many(values);
            Ok::<_, BindingError>(sketch)
        });
        result.map(|inner| Self { inner }).map_err(binding_error)
    }

    /// Construct a bounded ordered batch after copying Python feature buffers.
    #[staticmethod]
    #[pyo3(signature = (rows, *, num_perm = 128, seed = 1, threads = None))]
    fn from_batch(
        py: Python<'_>,
        rows: &Bound<'_, PyAny>,
        num_perm: usize,
        seed: u64,
        threads: Option<usize>,
    ) -> PyResult<Vec<Py<Self>>> {
        let rows = collect_feature_rows(py, rows)?;
        let threads = batch_threads(threads)?;
        let sketches = py
            .detach(move || build_sketches64(&rows, num_perm, seed, threads))
            .map_err(binding_error)?;
        sketches
            .into_iter()
            .map(|inner| Py::new(py, Self { inner }))
            .collect()
    }

    /// Reconstruct a Pari affine64 sketch without narrowing upper signature bits.
    #[staticmethod]
    #[pyo3(signature = (signature, *, seed = 1))]
    fn from_signature(py: Python<'_>, signature: &Bound<'_, PyAny>, seed: u64) -> PyResult<Self> {
        let signature = collect_signature64(py, signature)?;
        MinHash64::from_signature(signature, seed)
            .map(|inner| Self { inner })
            .map_err(|error| binding_error(error.into()))
    }

    fn update(&mut self, py: Python<'_>, value: &Bound<'_, PyAny>) -> PyResult<()> {
        if let Ok(bytes) = value.cast::<PyBytes>() {
            self.inner.update(bytes.as_bytes());
            return Ok(());
        }
        let value = owned_bytes(py, value)?;
        let mut sketch = self.inner.clone();
        self.inner = py.detach(move || {
            sketch.update(&value);
            sketch
        });
        Ok(())
    }

    fn update_many(&mut self, py: Python<'_>, values: &Bound<'_, PyAny>) -> PyResult<()> {
        let values = collect_byte_values(py, values)?;
        let mut sketch = self.inner.clone();
        self.inner = py.detach(move || {
            sketch.update_many(values);
            sketch
        });
        Ok(())
    }

    fn jaccard(&self, other: &Bound<'_, PyAny>) -> PyResult<f64> {
        let other = clone_minhash64(other)?;
        self.inner
            .jaccard(&other)
            .map_err(|error| binding_error(error.into()))
    }

    fn merge(&mut self, other: &Bound<'_, PyAny>) -> PyResult<()> {
        let other = clone_minhash64(other)?;
        self.inner
            .merge(&other)
            .map_err(|error| binding_error(error.into()))
    }

    fn clear(&mut self) {
        self.inner.clear();
    }

    #[getter]
    fn seed(&self) -> u64 {
        self.inner.seed()
    }

    #[getter]
    fn num_perm(&self) -> usize {
        self.inner.num_perm()
    }

    #[getter]
    fn scheme(&self) -> &'static str {
        self.inner.scheme()
    }

    #[getter]
    fn signature(&self) -> Vec<u64> {
        self.inner.signature().to_vec()
    }

    #[getter]
    fn permutations(&self) -> (Vec<u64>, Vec<u64>) {
        let (multipliers, offsets) = self.inner.permutations();
        (multipliers.to_vec(), offsets.to_vec())
    }

    #[getter]
    fn is_empty(&self) -> bool {
        self.inner.is_empty()
    }

    fn __len__(&self) -> usize {
        self.inner.num_perm()
    }

    fn __repr__(&self) -> String {
        format!(
            "MinHash64(num_perm={}, seed={}, scheme='{}')",
            self.inner.num_perm(),
            self.inner.seed(),
            self.inner.scheme()
        )
    }
}

fn cross_width_error(expected: &str, actual: &str) -> PyErr {
    CompatibilityError::new_err(format!(
        "incompatible MinHash width: {expected} requires {expected} sketches, got {actual}"
    ))
}

fn clone_minhash32(value: &Bound<'_, PyAny>) -> PyResult<MinHash32> {
    if let Ok(sketch) = value.extract::<PyRef<'_, PyMinHash>>() {
        return Ok(sketch.inner.clone());
    }
    if value.extract::<PyRef<'_, PyMinHash64>>().is_ok() {
        return Err(cross_width_error("affine32", "affine64"));
    }
    Ok(value
        .extract::<PyRef<'_, PyMinHash>>()
        .map(|sketch| sketch.inner.clone())?)
}

fn clone_minhash64(value: &Bound<'_, PyAny>) -> PyResult<MinHash64> {
    if let Ok(sketch) = value.extract::<PyRef<'_, PyMinHash64>>() {
        return Ok(sketch.inner.clone());
    }
    if value.extract::<PyRef<'_, PyMinHash>>().is_ok() {
        return Err(cross_width_error("affine64", "affine32"));
    }
    Ok(value
        .extract::<PyRef<'_, PyMinHash64>>()
        .map(|sketch| sketch.inner.clone())?)
}

/// Stable snapshot of local-index statistics.
#[pyclass(
    frozen,
    module = "pari._native",
    name = "IndexStats",
    skip_from_py_object
)]
#[derive(Debug, Clone)]
struct PyIndexStats {
    #[pyo3(get)]
    items: usize,
    #[pyo3(get)]
    file_bytes: u64,
    #[pyo3(get)]
    dirty: bool,
    #[pyo3(get)]
    bands: usize,
    #[pyo3(get)]
    rows: usize,
    #[pyo3(get)]
    committed_buckets: usize,
    #[pyo3(get)]
    overlay_buckets: usize,
    #[pyo3(get)]
    suppressed_base_keys: usize,
    #[pyo3(get)]
    committed_memberships: u64,
    #[pyo3(get)]
    committed_bucket_minimum: u64,
    #[pyo3(get)]
    committed_bucket_p50: u64,
    #[pyo3(get)]
    committed_bucket_p95: u64,
    #[pyo3(get)]
    committed_bucket_p99: u64,
    #[pyo3(get)]
    committed_bucket_maximum: u64,
    #[pyo3(get)]
    committed_bucket_average: f64,
    #[pyo3(get)]
    overlay_memberships: u64,
    #[pyo3(get)]
    query_operations: Option<u64>,
    #[pyo3(get)]
    query_count: Option<u64>,
    #[pyo3(get)]
    candidate_count: Option<u64>,
    #[pyo3(get)]
    candidate_rate: Option<f64>,
    #[pyo3(get)]
    average_query_ms: Option<f64>,
    #[pyo3(get)]
    max_query_ms: Option<f64>,
}

impl From<StoreStats> for PyIndexStats {
    fn from(stats: StoreStats) -> Self {
        Self {
            items: stats.items,
            file_bytes: stats.file_bytes,
            dirty: stats.dirty,
            bands: stats.bands,
            rows: stats.rows,
            committed_buckets: stats.committed_buckets,
            overlay_buckets: stats.overlay_buckets,
            suppressed_base_keys: stats.suppressed_base_keys,
            committed_memberships: stats.committed_distribution.memberships,
            committed_bucket_minimum: stats.committed_distribution.minimum,
            committed_bucket_p50: stats.committed_distribution.p50,
            committed_bucket_p95: stats.committed_distribution.p95,
            committed_bucket_p99: stats.committed_distribution.p99,
            committed_bucket_maximum: stats.committed_distribution.maximum,
            committed_bucket_average: stats.committed_distribution.average_members(),
            overlay_memberships: stats.overlay_distribution.memberships,
            query_operations: stats.queries.map(|metrics| metrics.operations),
            query_count: stats.queries.map(|metrics| metrics.queries),
            candidate_count: stats.queries.map(|metrics| metrics.candidates),
            candidate_rate: stats.queries.map(|metrics| metrics.candidate_rate()),
            average_query_ms: stats.queries.map(|metrics| metrics.average_operation_ms()),
            max_query_ms: stats.queries.map(|metrics| {
                std::time::Duration::from_nanos(metrics.max_latency_ns).as_secs_f64() * 1_000.0
            }),
        }
    }
}

#[pymethods]
impl PyIndexStats {
    fn __repr__(&self) -> String {
        format!(
            "IndexStats(items={}, file_bytes={}, dirty={}, bands={}, rows={})",
            self.items, self.file_bytes, self.dirty, self.bands, self.rows
        )
    }
}

/// Model-labeled LSH plan or explanation from Pari's canonical Rust planner.
#[pyclass(frozen, module = "pari._native", name = "LshPlan", skip_from_py_object)]
#[derive(Debug, Clone)]
struct PyLshPlan {
    inner: LshPlan,
}

impl From<LshPlan> for PyLshPlan {
    fn from(inner: LshPlan) -> Self {
        Self { inner }
    }
}

#[pymethods]
impl PyLshPlan {
    #[getter]
    fn model(&self) -> &'static str {
        self.inner.model
    }

    #[getter]
    #[allow(clippy::unused_self)]
    fn estimate_semantics(&self) -> &'static str {
        "analytical/model-based, not a measured guarantee"
    }

    #[getter]
    fn expected_items(&self) -> u64 {
        self.inner.expected_items
    }

    #[getter]
    fn threshold(&self) -> f64 {
        self.inner.threshold
    }

    #[getter]
    fn num_perm(&self) -> usize {
        self.inner.num_perm
    }

    #[getter]
    fn bands(&self) -> usize {
        self.inner.params.bands
    }

    #[getter]
    fn rows(&self) -> usize {
        self.inner.params.rows
    }

    #[getter]
    fn parameter_source(&self) -> &'static str {
        self.inner.parameter_source.as_str()
    }

    #[getter]
    fn used_permutations(&self) -> usize {
        self.inner.used_permutations
    }

    #[getter]
    fn unused_permutations(&self) -> usize {
        self.inner.unused_permutations
    }

    #[getter]
    fn candidate_probability_at_threshold(&self) -> f64 {
        self.inner.candidate_probability_at_threshold
    }

    #[getter]
    fn similarity_at_50_percent_candidates(&self) -> f64 {
        self.inner.similarity_at_50_percent_candidates
    }

    #[getter]
    fn false_positive_area(&self) -> f64 {
        self.inner.false_positive_area
    }

    #[getter]
    fn false_negative_area(&self) -> f64 {
        self.inner.false_negative_area
    }

    #[getter]
    fn bucket_memberships_per_item(&self) -> u64 {
        self.inner.bucket_memberships_per_item
    }

    #[getter]
    fn signature_bytes_per_item(&self) -> u64 {
        self.inner.sizes.signature_bytes_per_item
    }

    #[getter]
    fn signature_bytes(&self) -> u64 {
        self.inner.sizes.signature_bytes
    }

    #[getter]
    fn index_metadata_bytes_per_item(&self) -> u64 {
        self.inner.sizes.index_metadata_bytes_per_item
    }

    #[getter]
    fn index_metadata_bytes(&self) -> u64 {
        self.inner.sizes.index_metadata_bytes
    }

    #[getter]
    fn in_memory_index_bytes_per_item(&self) -> u64 {
        self.inner.sizes.in_memory_index_bytes_per_item
    }

    #[getter]
    fn in_memory_index_bytes(&self) -> u64 {
        self.inner.sizes.in_memory_index_bytes
    }

    #[getter]
    fn persistent_index_bytes_per_item(&self) -> u64 {
        self.inner.sizes.persistent_index_bytes_per_item
    }

    #[getter]
    fn persistent_index_bytes(&self) -> u64 {
        self.inner.sizes.persistent_index_bytes
    }

    #[getter]
    fn lazy_resident_bytes_per_item(&self) -> u64 {
        self.inner.sizes.lazy_resident_bytes_per_item
    }

    #[getter]
    fn lazy_resident_bytes(&self) -> u64 {
        self.inner.sizes.lazy_resident_bytes
    }

    #[getter]
    fn in_memory_with_headroom_bytes(&self) -> u64 {
        self.inner.sizes.in_memory_with_headroom_bytes
    }

    #[getter]
    fn lazy_with_headroom_bytes(&self) -> u64 {
        self.inner.sizes.lazy_with_headroom_bytes
    }

    #[getter]
    fn memory_budget_bytes(&self) -> Option<u64> {
        self.inner.memory_budget_bytes
    }

    #[getter]
    fn in_memory_fits_budget(&self) -> Option<bool> {
        self.inner.in_memory_fits_budget
    }

    #[getter]
    fn persistent_fits_budget(&self) -> Option<bool> {
        self.inner.persistent_fits_budget
    }

    #[getter]
    fn requested_storage(&self) -> &'static str {
        self.inner.requested_storage.as_str()
    }

    #[getter]
    fn recommended_storage(&self) -> &'static str {
        self.inner.recommended_storage.as_str()
    }

    #[getter]
    fn recommendation_reason(&self) -> &'static str {
        self.inner.recommendation_reason.as_str()
    }

    #[getter]
    fn recommendation(&self) -> &'static str {
        self.inner.recommendation_guidance()
    }

    fn candidate_probability(&self, similarity: f64) -> PyResult<f64> {
        self.inner.candidate_probability(similarity).ok_or_else(|| {
            ConfigurationError::new_err(format!(
                "similarity must be finite and in [0, 1], got {similarity}"
            ))
        })
    }

    fn __repr__(&self) -> String {
        format!(
            "LshPlan(items={}, threshold={}, bands={}, rows={}, recommended_storage='{}')",
            self.inner.expected_items,
            self.inner.threshold,
            self.inner.params.bands,
            self.inner.params.rows,
            self.inner.recommended_storage
        )
    }
}

type SharedIndex = Arc<Mutex<Option<PersistentIndex32>>>;

/// High-level persistent `MinHash` LSH index.
#[pyclass(module = "pari._native", name = "Index", skip_from_py_object)]
#[derive(Debug, Clone)]
struct PyIndex {
    inner: SharedIndex,
}

impl PyIndex {
    fn from_store(mut store: PersistentIndex32, observability: bool) -> Self {
        store.set_observability(observability);
        Self {
            inner: Arc::new(Mutex::new(Some(store))),
        }
    }

    fn run_read<T, F>(&self, py: Python<'_>, operation: F) -> PyResult<T>
    where
        T: Send + 'static,
        F: FnOnce(&PersistentIndex32) -> Result<T, BindingError> + Send + 'static,
    {
        let inner = Arc::clone(&self.inner);
        py.detach(move || {
            let guard = inner.lock().map_err(|_| BindingError::Poisoned)?;
            let store = guard.as_ref().ok_or(BindingError::Closed)?;
            operation(store)
        })
        .map_err(binding_error)
    }

    fn run_write<T, F>(&self, py: Python<'_>, operation: F) -> PyResult<T>
    where
        T: Send + 'static,
        F: FnOnce(&mut PersistentIndex32) -> Result<T, BindingError> + Send + 'static,
    {
        let inner = Arc::clone(&self.inner);
        py.detach(move || {
            let mut guard = inner.lock().map_err(|_| BindingError::Poisoned)?;
            let store = guard.as_mut().ok_or(BindingError::Closed)?;
            operation(store)
        })
        .map_err(binding_error)
    }
}

#[pymethods]
impl PyIndex {
    #[staticmethod]
    #[pyo3(signature = (path, *, threshold = 0.8, num_perm = 128, seed = 1, observability = false))]
    fn create(
        py: Python<'_>,
        path: PathBuf,
        threshold: f64,
        num_perm: usize,
        seed: u64,
        observability: bool,
    ) -> PyResult<Self> {
        py.detach(move || {
            PersistentIndex32::create(path, threshold, num_perm, seed).map_err(BindingError::from)
        })
        .map(|store| Self::from_store(store, observability))
        .map_err(binding_error)
    }

    #[staticmethod]
    #[pyo3(signature = (path, *, observability = false))]
    fn open(py: Python<'_>, path: PathBuf, observability: bool) -> PyResult<Self> {
        py.detach(move || PersistentIndex32::open(path).map_err(BindingError::from))
            .map(|store| Self::from_store(store, observability))
            .map_err(binding_error)
    }

    /// Insert one key and compatible `MinHash` sketch.
    fn add(&self, py: Python<'_>, key: u64, sketch: &Bound<'_, PyAny>) -> PyResult<()> {
        let sketch = clone_minhash32(sketch)?;
        self.run_write(py, move |store| {
            store.insert(key, &sketch).map_err(BindingError::from)
        })
    }

    /// Insert a batch atomically after Rust-side validation.
    fn add_many(&self, py: Python<'_>, items: Vec<(u64, Py<PyAny>)>) -> PyResult<()> {
        let items = items
            .into_iter()
            .map(|(key, sketch)| clone_minhash32(sketch.bind(py)).map(|sketch| (key, sketch)))
            .collect::<PyResult<Vec<_>>>()?;
        self.run_write(py, move |store| {
            store
                .insert_many(items.iter().map(|(key, sketch)| (*key, sketch)))
                .map_err(BindingError::from)
        })
    }

    /// Return sorted approximate candidate keys for one sketch.
    fn search(&self, py: Python<'_>, sketch: &Bound<'_, PyAny>) -> PyResult<Vec<u64>> {
        let sketch = clone_minhash32(sketch)?;
        self.run_read(py, move |store| {
            store.query(&sketch).map_err(BindingError::from)
        })
    }

    /// Batch query while releasing the GIL for all Rust storage and LSH work.
    fn search_many(&self, py: Python<'_>, sketches: Vec<Py<PyAny>>) -> PyResult<Vec<Vec<u64>>> {
        let sketches = sketches
            .into_iter()
            .map(|sketch| clone_minhash32(sketch.bind(py)))
            .collect::<PyResult<Vec<_>>>()?;
        self.run_read(py, move |store| {
            store
                .query_many(sketches.iter())
                .map_err(BindingError::from)
        })
    }

    /// Remove a key, returning whether it existed.
    fn remove(&self, py: Python<'_>, key: u64) -> PyResult<bool> {
        self.run_write(py, move |store| Ok(store.remove(key)))
    }

    fn contains(&self, py: Python<'_>, key: u64) -> PyResult<bool> {
        self.run_read(py, move |store| Ok(store.contains(key)))
    }

    fn stats(&self, py: Python<'_>) -> PyResult<PyIndexStats> {
        self.run_read(py, |store| {
            store
                .stats()
                .map(PyIndexStats::from)
                .map_err(BindingError::from)
        })
    }

    /// Explain stored LSH parameters without reading bucket memberships.
    fn explain(&self, py: Python<'_>) -> PyResult<PyLshPlan> {
        self.run_read(py, |store| {
            store
                .explain()
                .map(PyLshPlan::from)
                .map_err(BindingError::from)
        })
    }

    #[pyo3(signature = (enabled = true))]
    fn set_observability(&self, py: Python<'_>, enabled: bool) -> PyResult<()> {
        self.run_write(py, move |store| {
            store.set_observability(enabled);
            Ok(())
        })
    }

    fn flush(&self, py: Python<'_>) -> PyResult<()> {
        self.run_write(py, |store| store.flush().map_err(BindingError::from))
    }

    fn sync(&self, py: Python<'_>) -> PyResult<()> {
        self.run_write(py, |store| store.sync().map_err(BindingError::from))
    }

    /// Sync pending changes and make the Python handle unusable.
    fn close(&self, py: Python<'_>) -> PyResult<()> {
        let inner = Arc::clone(&self.inner);
        py.detach(move || {
            let mut guard = inner.lock().map_err(|_| BindingError::Poisoned)?;
            let Some(mut store) = guard.take() else {
                return Ok(());
            };
            store.sync().map_err(BindingError::from)
        })
        .map_err(binding_error)
    }

    #[getter]
    fn closed(&self) -> PyResult<bool> {
        self.inner
            .lock()
            .map(|guard| guard.is_none())
            .map_err(|_| binding_error(BindingError::Poisoned))
    }

    fn __len__(&self, py: Python<'_>) -> PyResult<usize> {
        self.run_read(py, |store| Ok(store.len()))
    }

    fn __contains__(&self, py: Python<'_>, key: u64) -> PyResult<bool> {
        self.contains(py, key)
    }

    fn __enter__(slf: PyRef<'_, Self>) -> PyRef<'_, Self> {
        slf
    }

    fn __exit__(
        &self,
        py: Python<'_>,
        _exc_type: &Bound<'_, PyAny>,
        _exc_value: &Bound<'_, PyAny>,
        _traceback: &Bound<'_, PyAny>,
    ) -> PyResult<bool> {
        self.close(py)?;
        Ok(false)
    }

    fn __repr__(&self) -> PyResult<String> {
        let closed = self.closed()?;
        Ok(format!("Index(closed={closed})"))
    }
}

type SharedIndex64 = Arc<Mutex<Option<PersistentIndex64>>>;

/// High-level persistent affine64 `MinHash64` LSH index.
#[pyclass(module = "pari._native", name = "Index64", skip_from_py_object)]
#[derive(Debug, Clone)]
struct PyIndex64 {
    inner: SharedIndex64,
}

impl PyIndex64 {
    fn from_store(mut store: PersistentIndex64, observability: bool) -> Self {
        store.set_observability(observability);
        Self {
            inner: Arc::new(Mutex::new(Some(store))),
        }
    }

    fn run_read<T, F>(&self, py: Python<'_>, operation: F) -> PyResult<T>
    where
        T: Send + 'static,
        F: FnOnce(&PersistentIndex64) -> Result<T, BindingError> + Send + 'static,
    {
        let inner = Arc::clone(&self.inner);
        py.detach(move || {
            let guard = inner.lock().map_err(|_| BindingError::Poisoned)?;
            let store = guard.as_ref().ok_or(BindingError::Closed)?;
            operation(store)
        })
        .map_err(binding_error)
    }

    fn run_write<T, F>(&self, py: Python<'_>, operation: F) -> PyResult<T>
    where
        T: Send + 'static,
        F: FnOnce(&mut PersistentIndex64) -> Result<T, BindingError> + Send + 'static,
    {
        let inner = Arc::clone(&self.inner);
        py.detach(move || {
            let mut guard = inner.lock().map_err(|_| BindingError::Poisoned)?;
            let store = guard.as_mut().ok_or(BindingError::Closed)?;
            operation(store)
        })
        .map_err(binding_error)
    }
}

#[pymethods]
impl PyIndex64 {
    #[staticmethod]
    #[pyo3(signature = (path, *, threshold = 0.8, num_perm = 128, seed = 1, observability = false))]
    fn create(
        py: Python<'_>,
        path: PathBuf,
        threshold: f64,
        num_perm: usize,
        seed: u64,
        observability: bool,
    ) -> PyResult<Self> {
        py.detach(move || {
            PersistentIndex64::create(path, threshold, num_perm, seed).map_err(BindingError::from)
        })
        .map(|store| Self::from_store(store, observability))
        .map_err(binding_error)
    }

    #[staticmethod]
    #[pyo3(signature = (path, *, observability = false))]
    fn open(py: Python<'_>, path: PathBuf, observability: bool) -> PyResult<Self> {
        py.detach(move || PersistentIndex64::open(path).map_err(BindingError::from))
            .map(|store| Self::from_store(store, observability))
            .map_err(binding_error)
    }

    /// Insert one key and compatible `MinHash64` sketch.
    fn add(&self, py: Python<'_>, key: u64, sketch: &Bound<'_, PyAny>) -> PyResult<()> {
        let sketch = clone_minhash64(sketch)?;
        self.run_write(py, move |store| {
            store.insert(key, &sketch).map_err(BindingError::from)
        })
    }

    /// Insert a batch atomically after Rust-side validation.
    fn add_many(&self, py: Python<'_>, items: Vec<(u64, Py<PyAny>)>) -> PyResult<()> {
        let items = items
            .into_iter()
            .map(|(key, sketch)| clone_minhash64(sketch.bind(py)).map(|sketch| (key, sketch)))
            .collect::<PyResult<Vec<_>>>()?;
        self.run_write(py, move |store| {
            store
                .insert_many(items.iter().map(|(key, sketch)| (*key, sketch)))
                .map_err(BindingError::from)
        })
    }

    /// Return sorted approximate candidate keys for one affine64 sketch.
    fn search(&self, py: Python<'_>, sketch: &Bound<'_, PyAny>) -> PyResult<Vec<u64>> {
        let sketch = clone_minhash64(sketch)?;
        self.run_read(py, move |store| {
            store.query(&sketch).map_err(BindingError::from)
        })
    }

    /// Batch query while releasing the GIL for all Rust storage and LSH work.
    fn search_many(&self, py: Python<'_>, sketches: Vec<Py<PyAny>>) -> PyResult<Vec<Vec<u64>>> {
        let sketches = sketches
            .into_iter()
            .map(|sketch| clone_minhash64(sketch.bind(py)))
            .collect::<PyResult<Vec<_>>>()?;
        self.run_read(py, move |store| {
            store
                .query_many(sketches.iter())
                .map_err(BindingError::from)
        })
    }

    fn remove(&self, py: Python<'_>, key: u64) -> PyResult<bool> {
        self.run_write(py, move |store| Ok(store.remove(key)))
    }

    fn contains(&self, py: Python<'_>, key: u64) -> PyResult<bool> {
        self.run_read(py, move |store| Ok(store.contains(key)))
    }

    fn stats(&self, py: Python<'_>) -> PyResult<PyIndexStats> {
        self.run_read(py, |store| {
            store
                .stats()
                .map(PyIndexStats::from)
                .map_err(BindingError::from)
        })
    }

    fn explain(&self, py: Python<'_>) -> PyResult<PyLshPlan> {
        self.run_read(py, |store| {
            store
                .explain()
                .map(PyLshPlan::from)
                .map_err(BindingError::from)
        })
    }

    #[pyo3(signature = (enabled = true))]
    fn set_observability(&self, py: Python<'_>, enabled: bool) -> PyResult<()> {
        self.run_write(py, move |store| {
            store.set_observability(enabled);
            Ok(())
        })
    }

    fn flush(&self, py: Python<'_>) -> PyResult<()> {
        self.run_write(py, |store| store.flush().map_err(BindingError::from))
    }

    fn sync(&self, py: Python<'_>) -> PyResult<()> {
        self.run_write(py, |store| store.sync().map_err(BindingError::from))
    }

    fn close(&self, py: Python<'_>) -> PyResult<()> {
        let inner = Arc::clone(&self.inner);
        py.detach(move || {
            let mut guard = inner.lock().map_err(|_| BindingError::Poisoned)?;
            let Some(mut store) = guard.take() else {
                return Ok(());
            };
            store.sync().map_err(BindingError::from)
        })
        .map_err(binding_error)
    }

    #[getter]
    fn closed(&self) -> PyResult<bool> {
        self.inner
            .lock()
            .map(|guard| guard.is_none())
            .map_err(|_| binding_error(BindingError::Poisoned))
    }

    fn __len__(&self, py: Python<'_>) -> PyResult<usize> {
        self.run_read(py, |store| Ok(store.len()))
    }

    fn __contains__(&self, py: Python<'_>, key: u64) -> PyResult<bool> {
        self.contains(py, key)
    }

    fn __enter__(slf: PyRef<'_, Self>) -> PyRef<'_, Self> {
        slf
    }

    fn __exit__(
        &self,
        py: Python<'_>,
        _exc_type: &Bound<'_, PyAny>,
        _exc_value: &Bound<'_, PyAny>,
        _traceback: &Bound<'_, PyAny>,
    ) -> PyResult<bool> {
        self.close(py)?;
        Ok(false)
    }

    fn __repr__(&self) -> PyResult<String> {
        let closed = self.closed()?;
        Ok(format!("Index64(closed={closed})"))
    }
}

#[derive(Debug)]
struct DedupeState {
    index: LshIndex32,
    store: Option<PersistentIndex32>,
}

type SharedDedupe = Arc<Mutex<Option<DedupeState>>>;

/// Native batch and grouping engine behind the Python usability layer.
#[pyclass(module = "pari._native", name = "_DedupeEngine", skip_from_py_object)]
#[derive(Debug, Clone)]
struct PyDedupeEngine {
    inner: SharedDedupe,
    num_perm: usize,
    seed: u64,
    threads: BatchThreads,
}

impl PyDedupeEngine {
    fn run_read<T, F>(&self, py: Python<'_>, operation: F) -> PyResult<T>
    where
        T: Send + 'static,
        F: FnOnce(&DedupeState) -> Result<T, BindingError> + Send + 'static,
    {
        let inner = Arc::clone(&self.inner);
        py.detach(move || {
            let guard = inner.try_lock().map_err(BindingError::from)?;
            let state = guard.as_ref().ok_or(BindingError::Closed)?;
            operation(state)
        })
        .map_err(binding_error)
    }

    fn run_write<T, F>(&self, py: Python<'_>, operation: F) -> PyResult<T>
    where
        T: Send + 'static,
        F: FnOnce(&mut DedupeState) -> Result<T, BindingError> + Send + 'static,
    {
        let inner = Arc::clone(&self.inner);
        py.detach(move || {
            let mut guard = inner.try_lock().map_err(BindingError::from)?;
            let state = guard.as_mut().ok_or(BindingError::Closed)?;
            operation(state)
        })
        .map_err(binding_error)
    }
}

#[pymethods]
impl PyDedupeEngine {
    #[new]
    #[pyo3(signature = (*, threshold = 0.8, num_perm = 128, seed = 1, threads = None, path = None))]
    fn new(
        py: Python<'_>,
        threshold: f64,
        num_perm: usize,
        seed: u64,
        threads: Option<usize>,
        path: Option<PathBuf>,
    ) -> PyResult<Self> {
        let threads = batch_threads(threads)?;
        let state = py
            .detach(move || {
                let index =
                    LshIndex32::new(threshold, num_perm, seed).map_err(BindingError::from)?;
                let store = path
                    .map(|path| {
                        PersistentIndex32::create(path, threshold, num_perm, seed)
                            .map_err(BindingError::from)
                    })
                    .transpose()?;
                Ok::<_, BindingError>(DedupeState { index, store })
            })
            .map_err(binding_error)?;

        Ok(Self {
            inner: Arc::new(Mutex::new(Some(state))),
            num_perm,
            seed,
            threads,
        })
    }

    /// Build and insert one bounded batch after copying Python buffers.
    fn add_many(
        &self,
        py: Python<'_>,
        keys: Vec<u64>,
        feature_rows: &Bound<'_, PyAny>,
    ) -> PyResult<()> {
        let rows = collect_feature_rows(py, feature_rows)?;
        if keys.len() != rows.len() {
            return Err(ConfigurationError::new_err(format!(
                "keys and feature rows must have equal lengths, got {} and {}",
                keys.len(),
                rows.len()
            )));
        }

        let num_perm = self.num_perm;
        let seed = self.seed;
        let threads = self.threads;
        let sketches = py
            .detach(move || build_sketches(&rows, num_perm, seed, threads))
            .map_err(binding_error)?;

        self.run_write(py, move |state| {
            state
                .index
                .insert_many(
                    keys.iter()
                        .zip(&sketches)
                        .map(|(key, sketch)| (*key, sketch)),
                )
                .map_err(BindingError::from)?;

            if let Some(store) = &mut state.store {
                if let Err(error) = store.insert_many(
                    keys.iter()
                        .zip(&sketches)
                        .map(|(key, sketch)| (*key, sketch)),
                ) {
                    for key in &keys {
                        state.index.remove(*key);
                    }
                    return Err(BindingError::from(error));
                }
            }
            Ok(())
        })
    }

    /// Run direct native grouping, optionally invoking a Python pair verifier.
    #[pyo3(signature = (*, verifier = None))]
    fn groups(
        &self,
        py: Python<'_>,
        verifier: Option<Py<PyAny>>,
    ) -> PyResult<Vec<(u64, Vec<u64>)>> {
        let Some(verifier) = verifier else {
            return self.run_read(py, |state| Ok(owned_groups(state.index.duplicate_groups())));
        };

        let inner = Arc::clone(&self.inner);
        py.detach(move || {
            let guard = inner
                .try_lock()
                .map_err(BindingError::from)
                .map_err(binding_error)?;
            let state = guard
                .as_ref()
                .ok_or_else(|| binding_error(BindingError::Closed))?;
            let mut callback_error = None;
            let groups = state.index.duplicate_groups_with(2, |left, right| {
                if callback_error.is_some() {
                    return false;
                }
                match Python::attach(|py| verifier.bind(py).call1((left, right))?.extract::<bool>())
                {
                    Ok(accepted) => accepted,
                    Err(error) => {
                        callback_error = Some(error);
                        false
                    }
                }
            });
            if let Some(error) = callback_error {
                return Err(error);
            }
            Ok(owned_groups(groups))
        })
    }

    /// Return unique normalized candidate pairs from LSH bucket collisions.
    fn pairs(&self, py: Python<'_>) -> PyResult<Vec<(u64, u64)>> {
        self.run_read(py, |state| Ok(state.index.candidate_pairs().collect()))
    }

    fn sync(&self, py: Python<'_>) -> PyResult<()> {
        self.run_write(py, |state| {
            if let Some(store) = &mut state.store {
                store.sync().map_err(BindingError::from)?;
            }
            Ok(())
        })
    }

    fn close(&self, py: Python<'_>) -> PyResult<()> {
        let inner = Arc::clone(&self.inner);
        py.detach(move || {
            let mut guard = inner.try_lock().map_err(BindingError::from)?;
            let Some(mut state) = guard.take() else {
                return Ok(());
            };
            if let Some(store) = &mut state.store {
                store.sync().map_err(BindingError::from)?;
            }
            Ok(())
        })
        .map_err(binding_error)
    }

    #[getter]
    fn closed(&self) -> PyResult<bool> {
        self.inner
            .try_lock()
            .map(|guard| guard.is_none())
            .map_err(BindingError::from)
            .map_err(binding_error)
    }

    fn __len__(&self, py: Python<'_>) -> PyResult<usize> {
        self.run_read(py, |state| Ok(state.index.len()))
    }

    fn __repr__(&self) -> PyResult<String> {
        Ok(format!("_DedupeEngine(closed={})", self.closed()?))
    }
}

#[pyfunction(name = "plan_lsh")]
#[pyo3(signature = (expected_items, *, threshold = 0.8, num_perm = 128, memory_budget_bytes = None, storage = "auto"))]
fn py_plan_lsh(
    expected_items: u64,
    threshold: f64,
    num_perm: usize,
    memory_budget_bytes: Option<u64>,
    storage: &str,
) -> PyResult<PyLshPlan> {
    let storage = storage
        .parse::<StorageMode>()
        .map_err(BindingError::from)
        .map_err(binding_error)?;
    let mut options =
        LshPlanOptions::new(expected_items, threshold, num_perm).storage_mode(storage);
    if let Some(bytes) = memory_budget_bytes {
        options = options.memory_budget_bytes(bytes);
    }
    plan_lsh(options)
        .map(PyLshPlan::from)
        .map_err(BindingError::from)
        .map_err(binding_error)
}

#[pymodule]
fn _native(py: Python<'_>, module: &Bound<'_, PyModule>) -> PyResult<()> {
    module.add_class::<PyMinHash>()?;
    module.add_class::<PyMinHash64>()?;
    module.add_class::<PyIndex>()?;
    module.add_class::<PyIndex64>()?;
    module.add_class::<PyIndexStats>()?;
    module.add_class::<PyLshPlan>()?;
    module.add_class::<PyDedupeEngine>()?;
    module.add_function(wrap_pyfunction!(py_plan_lsh, module)?)?;
    module.add("PariError", py.get_type::<PariError>())?;
    module.add("ConfigurationError", py.get_type::<ConfigurationError>())?;
    module.add("CompatibilityError", py.get_type::<CompatibilityError>())?;
    module.add("DuplicateKeyError", py.get_type::<DuplicateKeyError>())?;
    module.add("StorageError", py.get_type::<StorageError>())?;
    module.add("ClosedIndexError", py.get_type::<ClosedIndexError>())?;
    module.add("__version__", env!("CARGO_PKG_VERSION"))?;
    Ok(())
}
