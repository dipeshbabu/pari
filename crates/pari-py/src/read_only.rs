use std::{path::PathBuf, sync::Mutex};

use pari_store_lazy::{LazyIndex, LazyIndex32, LazyIndex64, LazyStoreError};
use pyo3::prelude::*;

use crate::{
    binding_error, clone_minhash32, clone_minhash64, score_filter, scored_pairs, BindingError,
};

#[derive(Debug)]
enum Reader {
    Affine32(Mutex<Option<LazyIndex32>>),
    Affine64(Mutex<Option<LazyIndex64>>),
}

/// Query an existing committed Pari index using its stored signature family.
#[pyclass(
    frozen,
    module = "pari._native",
    name = "ReadOnlyIndex",
    skip_from_py_object
)]
#[derive(Debug)]
pub(crate) struct PyReadOnlyIndex {
    reader: Reader,
    #[pyo3(get)]
    signature_width: u8,
    #[pyo3(get)]
    num_perm: usize,
    #[pyo3(get)]
    seed: u64,
    #[pyo3(get)]
    stores_signatures: bool,
    items: usize,
}

fn with_reader<T: Send, U: Send>(
    py: Python<'_>,
    reader: &Mutex<Option<T>>,
    operation: impl FnOnce(&mut T) -> Result<U, LazyStoreError> + Send,
) -> PyResult<U> {
    py.detach(move || {
        let mut state = reader.lock().map_err(|_| BindingError::Poisoned)?;
        let index = state.as_mut().ok_or(BindingError::Closed)?;
        operation(index).map_err(BindingError::LazyStore)
    })
    .map_err(binding_error)
}

#[pymethods]
impl PyReadOnlyIndex {
    /// Open an index for queries without loading its per-key band hashes.
    #[staticmethod]
    fn open(py: Python<'_>, path: PathBuf) -> PyResult<Self> {
        let index = py
            .detach(move || LazyIndex::open(path))
            .map_err(|error| binding_error(BindingError::LazyStore(error)))?;
        macro_rules! wrap {
            ($index:ident, $variant:ident, $width:expr) => {
                Self {
                    signature_width: $width,
                    num_perm: $index.num_perm(),
                    seed: $index.seed(),
                    stores_signatures: $index.stores_signatures(),
                    items: $index.len(),
                    reader: Reader::$variant(Mutex::new(Some($index))),
                }
            };
        }
        Ok(match index {
            LazyIndex::Affine32(index) => wrap!(index, Affine32, 32),
            LazyIndex::Affine64(index) => wrap!(index, Affine64, 64),
        })
    }

    /// Return candidate keys in ascending order.
    fn search(&self, py: Python<'_>, sketch: &Bound<'_, PyAny>) -> PyResult<Vec<u64>> {
        match &self.reader {
            Reader::Affine32(reader) => {
                let sketch = clone_minhash32(sketch)?;
                with_reader(py, reader, move |index| index.query(&sketch))
            }
            Reader::Affine64(reader) => {
                let sketch = clone_minhash64(sketch)?;
                with_reader(py, reader, move |index| index.query(&sketch))
            }
        }
    }

    /// Return a candidate list per query, in input order.
    fn search_many(&self, py: Python<'_>, sketches: Vec<Py<PyAny>>) -> PyResult<Vec<Vec<u64>>> {
        match &self.reader {
            Reader::Affine32(reader) => {
                let sketches = sketches
                    .into_iter()
                    .map(|sketch| clone_minhash32(sketch.bind(py)))
                    .collect::<PyResult<Vec<_>>>()?;
                with_reader(py, reader, move |index| index.query_many(&sketches))
            }
            Reader::Affine64(reader) => {
                let sketches = sketches
                    .into_iter()
                    .map(|sketch| clone_minhash64(sketch.bind(py)))
                    .collect::<PyResult<Vec<_>>>()?;
                with_reader(py, reader, move |index| index.query_many(&sketches))
            }
        }
    }

    /// Return candidate keys with estimated Jaccard similarity, highest first.
    #[pyo3(signature = (sketch, *, min_similarity = None, top_k = None))]
    fn search_scored(
        &self,
        py: Python<'_>,
        sketch: &Bound<'_, PyAny>,
        min_similarity: Option<f64>,
        top_k: Option<usize>,
    ) -> PyResult<Vec<(u64, f64)>> {
        let filter = score_filter(min_similarity, top_k);
        match &self.reader {
            Reader::Affine32(reader) => {
                let sketch = clone_minhash32(sketch)?;
                with_reader(py, reader, move |index| {
                    index.query_scored(&sketch, filter).map(scored_pairs)
                })
            }
            Reader::Affine64(reader) => {
                let sketch = clone_minhash64(sketch)?;
                with_reader(py, reader, move |index| {
                    index.query_scored(&sketch, filter).map(scored_pairs)
                })
            }
        }
    }

    /// Return ranked candidates for each query, in input order.
    #[pyo3(signature = (sketches, *, min_similarity = None, top_k = None))]
    fn search_scored_many(
        &self,
        py: Python<'_>,
        sketches: Vec<Py<PyAny>>,
        min_similarity: Option<f64>,
        top_k: Option<usize>,
    ) -> PyResult<Vec<Vec<(u64, f64)>>> {
        let filter = score_filter(min_similarity, top_k);
        let results = match &self.reader {
            Reader::Affine32(reader) => {
                let sketches = sketches
                    .into_iter()
                    .map(|sketch| clone_minhash32(sketch.bind(py)))
                    .collect::<PyResult<Vec<_>>>()?;
                with_reader(py, reader, move |index| {
                    index.query_scored_many(&sketches, filter)
                })?
            }
            Reader::Affine64(reader) => {
                let sketches = sketches
                    .into_iter()
                    .map(|sketch| clone_minhash64(sketch.bind(py)))
                    .collect::<PyResult<Vec<_>>>()?;
                with_reader(py, reader, move |index| {
                    index.query_scored_many(&sketches, filter)
                })?
            }
        };
        Ok(results.into_iter().map(scored_pairs).collect())
    }

    /// Release the file handle. Closing twice is safe.
    fn close(&self, py: Python<'_>) -> PyResult<()> {
        fn close<T>(reader: &Mutex<Option<T>>) -> Result<(), BindingError> {
            reader.lock().map_err(|_| BindingError::Poisoned)?.take();
            Ok(())
        }
        py.detach(|| match &self.reader {
            Reader::Affine32(reader) => close(reader),
            Reader::Affine64(reader) => close(reader),
        })
        .map_err(binding_error)
    }

    #[getter]
    fn closed(&self, py: Python<'_>) -> PyResult<bool> {
        fn closed<T>(reader: &Mutex<Option<T>>) -> Result<bool, BindingError> {
            Ok(reader.lock().map_err(|_| BindingError::Poisoned)?.is_none())
        }
        py.detach(|| match &self.reader {
            Reader::Affine32(reader) => closed(reader),
            Reader::Affine64(reader) => closed(reader),
        })
        .map_err(binding_error)
    }

    fn __len__(&self) -> usize {
        self.items
    }

    fn __enter__<'py>(slf: PyRef<'py, Self>, py: Python<'py>) -> PyResult<PyRef<'py, Self>> {
        if slf.closed(py)? {
            return Err(binding_error(BindingError::Closed));
        }
        Ok(slf)
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
}
