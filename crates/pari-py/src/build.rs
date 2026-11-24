use std::path::PathBuf;

use pari_store_build::{build_external, BuildOptions};
use pyo3::prelude::*;

use crate::{binding_error, BindingError};

/// What [`build_read_only_index`] wrote.
#[pyclass(
    frozen,
    module = "pari._native",
    name = "BuildStats",
    skip_from_py_object
)]
#[derive(Debug, Clone)]
pub(crate) struct PyBuildStats {
    /// `(band, hash, key)` records in the output, one per item and band.
    #[pyo3(get)]
    records: u64,
    /// Distinct buckets in the output.
    #[pyo3(get)]
    buckets: u64,
    /// Size of the output file.
    #[pyo3(get)]
    output_bytes: u64,
    /// Sorted runs spilled to disk; zero when the source's buckets were reused.
    #[pyo3(get)]
    spill_runs: usize,
    /// Whether the source's bucket sections were copied instead of rebuilt.
    #[pyo3(get)]
    reused_source_buckets: bool,
}

#[pymethods]
impl PyBuildStats {
    fn __repr__(&self) -> String {
        format!(
            "BuildStats(records={}, buckets={}, output_bytes={}, spill_runs={}, \
             reused_source_buckets={})",
            self.records,
            self.buckets,
            self.output_bytes,
            self.spill_runs,
            if self.reused_source_buckets {
                "True"
            } else {
                "False"
            }
        )
    }
}

/// Write a compact, query-ready copy of a committed index in bounded memory.
#[pyfunction(name = "build_read_only_index")]
#[pyo3(signature = (source, destination, *, reuse_buckets = true, max_buffer_records = 262_144))]
pub(crate) fn py_build_read_only_index(
    py: Python<'_>,
    source: PathBuf,
    destination: PathBuf,
    reuse_buckets: bool,
    max_buffer_records: usize,
) -> PyResult<PyBuildStats> {
    let options = BuildOptions {
        max_buffer_records,
        reuse_source_buckets: reuse_buckets,
    };
    let stats = py
        .detach(move || build_external(source, destination, options))
        .map_err(|error| binding_error(BindingError::Build(error)))?;
    Ok(PyBuildStats {
        records: stats.records,
        buckets: stats.buckets,
        output_bytes: stats.output_bytes,
        spill_runs: stats.spill_runs,
        reused_source_buckets: stats.reused_source_buckets,
    })
}
