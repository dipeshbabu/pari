#![forbid(unsafe_code)]
//! PyO3 bindings for Pari's stable Rust APIs.

use std::{
    path::PathBuf,
    sync::{Arc, Mutex},
};

use pari_core::{MinHash32, MinHashError};
use pari_index::LshError;
use pari_store::{PersistentIndex32, StoreError, StoreStats};
use pyo3::{
    buffer::PyBuffer,
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

#[derive(Debug)]
enum BindingError {
    Closed,
    Poisoned,
    MinHash(MinHashError),
    Store(StoreError),
}

impl From<MinHashError> for BindingError {
    fn from(error: MinHashError) -> Self {
        Self::MinHash(error)
    }
}

impl From<StoreError> for BindingError {
    fn from(error: StoreError) -> Self {
        Self::Store(error)
    }
}

fn binding_error(error: BindingError) -> PyErr {
    match error {
        BindingError::Closed => ClosedIndexError::new_err("index is closed"),
        BindingError::Poisoned => StorageError::new_err("index state lock is poisoned"),
        BindingError::MinHash(MinHashError::InvalidPermutationCount { .. }) => {
            ConfigurationError::new_err(error_text(&error))
        }
        BindingError::MinHash(
            MinHashError::IncompatibleSeed { .. }
            | MinHashError::IncompatiblePermutationCount { .. },
        ) => CompatibilityError::new_err(error_text(&error)),
        BindingError::Store(StoreError::DuplicateKey { .. }) => {
            DuplicateKeyError::new_err(error_text(&error))
        }
        BindingError::Store(
            StoreError::IncompatibleSeed { .. }
            | StoreError::IncompatiblePermutationCount { .. },
        ) => CompatibilityError::new_err(error_text(&error)),
        BindingError::Store(StoreError::Index(
            LshError::InvalidThreshold { .. }
            | LshError::InvalidPermutationCount { .. }
            | LshError::AutomaticTuningTooLarge { .. }
            | LshError::InvalidParams { .. },
        )) => ConfigurationError::new_err(error_text(&error)),
        BindingError::Store(_) => StorageError::new_err(error_text(&error)),
    }
}

fn error_text(error: &BindingError) -> String {
    match error {
        BindingError::Closed => "index is closed".into(),
        BindingError::Poisoned => "index state lock is poisoned".into(),
        BindingError::MinHash(error) => error.to_string(),
        BindingError::Store(error) => error.to_string(),
    }
}

fn owned_bytes(py: Python<'_>, value: &Bound<'_, PyAny>) -> PyResult<Vec<u8>> {
    if let Ok(bytes) = value.cast::<PyBytes>() {
        return Ok(bytes.as_bytes().to_vec());
    }
    if let Ok(bytes) = value.extract::<Vec<u8>>() {
        return Ok(bytes);
    }
    PyBuffer::<u8>::get(value)?.to_vec(py)
}

fn collect_byte_values(py: Python<'_>, values: &Bound<'_, PyAny>) -> PyResult<Vec<Vec<u8>>> {
    values
        .try_iter()?
        .map(|item| owned_bytes(py, &item?))
        .collect()
}

/// Pari's 32-bit affine MinHash sketch.
#[pyclass(module = "pari._native", name = "MinHash")]
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
        result
            .map(|inner| Self { inner })
            .map_err(binding_error)
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

    fn jaccard(&self, other: PyRef<'_, Self>) -> PyResult<f64> {
        self.inner
            .jaccard(&other.inner)
            .map_err(|error| binding_error(error.into()))
    }

    fn merge(&mut self, other: PyRef<'_, Self>) -> PyResult<()> {
        self.inner
            .merge(&other.inner)
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

/// Stable snapshot of local-index statistics.
#[pyclass(frozen, module = "pari._native", name = "IndexStats")]
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

type SharedIndex = Arc<Mutex<Option<PersistentIndex32>>>;

/// High-level persistent MinHash LSH index.
#[pyclass(module = "pari._native", name = "Index")]
#[derive(Debug, Clone)]
struct PyIndex {
    inner: SharedIndex,
}

impl PyIndex {
    fn from_store(store: PersistentIndex32) -> Self {
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
    #[pyo3(signature = (path, *, threshold = 0.8, num_perm = 128, seed = 1))]
    fn create(
        py: Python<'_>,
        path: PathBuf,
        threshold: f64,
        num_perm: usize,
        seed: u64,
    ) -> PyResult<Self> {
        py.detach(move || {
            PersistentIndex32::create(path, threshold, num_perm, seed).map_err(BindingError::from)
        })
        .map(Self::from_store)
        .map_err(binding_error)
    }

    #[staticmethod]
    fn open(py: Python<'_>, path: PathBuf) -> PyResult<Self> {
        py.detach(move || PersistentIndex32::open(path).map_err(BindingError::from))
            .map(Self::from_store)
            .map_err(binding_error)
    }

    /// Insert one key and compatible MinHash sketch.
    fn add(&self, py: Python<'_>, key: u64, sketch: PyRef<'_, PyMinHash>) -> PyResult<()> {
        let sketch = sketch.inner.clone();
        self.run_write(py, move |store| {
            store.insert(key, &sketch).map_err(BindingError::from)
        })
    }

    /// Insert a batch atomically after Rust-side validation.
    fn add_many(
        &self,
        py: Python<'_>,
        items: Vec<(u64, Py<PyMinHash>)>,
    ) -> PyResult<()> {
        let items = items
            .into_iter()
            .map(|(key, sketch)| (key, sketch.borrow(py).inner.clone()))
            .collect::<Vec<_>>();
        self.run_write(py, move |store| {
            store
                .insert_many(items.iter().map(|(key, sketch)| (*key, sketch)))
                .map_err(BindingError::from)
        })
    }

    /// Return sorted approximate candidate keys for one sketch.
    fn search(&self, py: Python<'_>, sketch: PyRef<'_, PyMinHash>) -> PyResult<Vec<u64>> {
        let sketch = sketch.inner.clone();
        self.run_read(py, move |store| {
            store.query(&sketch).map_err(BindingError::from)
        })
    }

    /// Batch query while releasing the GIL for all Rust storage and LSH work.
    fn search_many(
        &self,
        py: Python<'_>,
        sketches: Vec<Py<PyMinHash>>,
    ) -> PyResult<Vec<Vec<u64>>> {
        let sketches = sketches
            .into_iter()
            .map(|sketch| sketch.borrow(py).inner.clone())
            .collect::<Vec<_>>();
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

#[pymodule]
fn _native(py: Python<'_>, module: &Bound<'_, PyModule>) -> PyResult<()> {
    module.add_class::<PyMinHash>()?;
    module.add_class::<PyIndex>()?;
    module.add_class::<PyIndexStats>()?;
    module.add("PariError", py.get_type::<PariError>())?;
    module.add("ConfigurationError", py.get_type::<ConfigurationError>())?;
    module.add("CompatibilityError", py.get_type::<CompatibilityError>())?;
    module.add("DuplicateKeyError", py.get_type::<DuplicateKeyError>())?;
    module.add("StorageError", py.get_type::<StorageError>())?;
    module.add("ClosedIndexError", py.get_type::<ClosedIndexError>())?;
    module.add("__version__", env!("CARGO_PKG_VERSION"))?;
    Ok(())
}
