"""Optional bounded-batch adapters for common analytics dataset libraries."""

from __future__ import annotations

import importlib
from collections.abc import Callable, Iterable, Iterator, Mapping, Sequence
from pathlib import Path
from typing import Any, TypeVar

from .dedupe import DedupeIndex, ProgressCallback

_T = TypeVar("_T")
Row = Mapping[str, Any]
RecordProjector = Callable[[Row], _T]
FeatureProjector = Callable[[Row], Iterable[bytes | bytearray | memoryview]]


class IntegrationDependencyError(ImportError):
    """Raised when an explicitly requested integration extra is unavailable."""


def _require(module: str, extra: str) -> Any:
    try:
        return importlib.import_module(module)
    except ModuleNotFoundError as error:
        if error.name != module.split(".", 1)[0]:
            raise
        raise IntegrationDependencyError(
            f"{module} is required for this adapter; install 'pari-similarity[{extra}]'"
        ) from error


def _positive_batch_size(batch_size: int) -> int:
    if batch_size <= 0:
        raise ValueError(f"batch_size must be positive, got {batch_size}")
    return batch_size


def iter_pyarrow_rows(
    source: object,
    *,
    columns: Sequence[str] | None = None,
    batch_size: int = 65_536,
) -> Iterator[dict[str, Any]]:
    """Yield Python row dictionaries from Arrow or Parquet one batch at a time."""

    batch_size = _positive_batch_size(batch_size)
    pa = _require("pyarrow", "pyarrow")
    arrow_dataset = _require("pyarrow.dataset", "pyarrow")
    selected = list(columns) if columns is not None else None

    if isinstance(source, (str, Path)):
        parquet = _require("pyarrow.parquet", "pyarrow")
        batches = parquet.ParquetFile(source).iter_batches(
            batch_size=batch_size, columns=selected
        )
    elif isinstance(source, pa.Table):
        table = source.select(selected) if selected is not None else source
        batches = table.to_batches(max_chunksize=batch_size)
    elif isinstance(source, pa.RecordBatch):
        batches = (source,)
    elif isinstance(source, pa.RecordBatchReader):
        batches = iter(source)
    elif isinstance(source, arrow_dataset.Scanner):
        batches = source.to_batches()
    elif isinstance(source, arrow_dataset.Dataset):
        batches = source.to_batches(columns=selected, batch_size=batch_size)
        selected = None
    else:
        raise TypeError(
            "source must be a Parquet path, pyarrow Table/RecordBatch/Reader, "
            "Dataset, or Scanner"
        )

    for batch in batches:
        if selected is not None:
            batch = batch.select(selected)
        for start in range(0, batch.num_rows, batch_size):
            yield from batch.slice(start, batch_size).to_pylist()


def iter_polars_rows(
    source: object,
    *,
    columns: Sequence[str] | None = None,
    batch_size: int = 65_536,
) -> Iterator[dict[str, Any]]:
    """Yield rows from a Polars frame or lazy Parquet scan in bounded chunks."""

    batch_size = _positive_batch_size(batch_size)
    pl = _require("polars", "polars")
    selected = list(columns) if columns is not None else None
    if isinstance(source, (str, Path)):
        source = pl.scan_parquet(source)

    if isinstance(source, pl.LazyFrame):
        lazy = source.select(selected) if selected is not None else source
        if not hasattr(lazy, "collect_batches"):
            raise RuntimeError(
                "this Polars version does not provide LazyFrame.collect_batches(); "
                "install a current 'pari-similarity[polars]' extra"
            )
        frames = lazy.collect_batches(
            chunk_size=batch_size, maintain_order=True, engine="streaming"
        )
    elif isinstance(source, pl.DataFrame):
        frame = source.select(selected) if selected is not None else source
        frames = frame.iter_slices(n_rows=batch_size)
    else:
        raise TypeError("source must be a Parquet path, polars DataFrame, or LazyFrame")

    for frame in frames:
        yield from frame.iter_rows(named=True)


def iter_huggingface_rows(
    source: object,
    *,
    columns: Sequence[str] | None = None,
    batch_size: int = 1_000,
) -> Iterator[dict[str, Any]]:
    """Yield rows from a Hugging Face Dataset/IterableDataset batch iterator."""

    batch_size = _positive_batch_size(batch_size)
    _require("datasets", "huggingface")
    dataset = source
    if columns is not None:
        if not hasattr(dataset, "select_columns"):
            raise TypeError("source does not support Hugging Face column projection")
        dataset = dataset.select_columns(list(columns))
    if not hasattr(dataset, "iter"):
        raise TypeError("source must be a Hugging Face Dataset or IterableDataset")

    for batch in dataset.iter(batch_size=batch_size, drop_last_batch=False):
        if not isinstance(batch, Mapping):
            raise TypeError("Hugging Face batch iterator returned a non-mapping value")
        names = list(batch)
        if not names:
            continue
        lengths = {len(batch[name]) for name in names}
        if len(lengths) != 1:
            raise ValueError("Hugging Face batch columns have different lengths")
        length = lengths.pop()
        for position in range(length):
            yield {name: batch[name][position] for name in names}


def _add_rows(
    index: DedupeIndex[_T],
    rows: Iterable[Row],
    *,
    record: RecordProjector[_T],
    features: FeatureProjector,
    progress: ProgressCallback | None = None,
) -> int:
    return index.add_many_features(
        ((record(row), features(row)) for row in rows), progress=progress
    )


def add_pyarrow(
    index: DedupeIndex[_T],
    source: object,
    *,
    record: RecordProjector[_T],
    features: FeatureProjector,
    columns: Sequence[str] | None = None,
    batch_size: int = 65_536,
    progress: ProgressCallback | None = None,
) -> int:
    """Project Arrow/Parquet rows directly into one `DedupeIndex`."""

    return _add_rows(
        index,
        iter_pyarrow_rows(source, columns=columns, batch_size=batch_size),
        record=record,
        features=features,
        progress=progress,
    )


def add_polars(
    index: DedupeIndex[_T],
    source: object,
    *,
    record: RecordProjector[_T],
    features: FeatureProjector,
    columns: Sequence[str] | None = None,
    batch_size: int = 65_536,
    progress: ProgressCallback | None = None,
) -> int:
    """Project Polars rows directly into one `DedupeIndex`."""

    return _add_rows(
        index,
        iter_polars_rows(source, columns=columns, batch_size=batch_size),
        record=record,
        features=features,
        progress=progress,
    )


def add_huggingface(
    index: DedupeIndex[_T],
    source: object,
    *,
    record: RecordProjector[_T],
    features: FeatureProjector,
    columns: Sequence[str] | None = None,
    batch_size: int = 1_000,
    progress: ProgressCallback | None = None,
) -> int:
    """Project Hugging Face rows directly into one `DedupeIndex`."""

    return _add_rows(
        index,
        iter_huggingface_rows(source, columns=columns, batch_size=batch_size),
        record=record,
        features=features,
        progress=progress,
    )


__all__ = [
    "IntegrationDependencyError",
    "add_huggingface",
    "add_polars",
    "add_pyarrow",
    "iter_huggingface_rows",
    "iter_polars_rows",
    "iter_pyarrow_rows",
]
