# Optional dataset integrations

`pari.integrations` connects PyArrow/Parquet, Polars, and Hugging Face Datasets to `DedupeIndex` without adding those libraries to Pari's core installation. The module itself imports no optional package. An adapter loads its dependency only when called and raises `IntegrationDependencyError` with the matching install extra when it is missing.

```bash
python -m pip install "pari-similarity[pyarrow]"
python -m pip install "pari-similarity[polars]"
python -m pip install "pari-similarity[huggingface]"

# Install all three adapters.
python -m pip install "pari-similarity[integrations]"
```

Plain `import pari` and `import pari.integrations` stay lightweight without these extras.

## Conversion boundary

Every direct-ingestion helper requires two callbacks:

- `record(row)` chooses the lightweight application object retained by `DedupeIndex` and returned in pairs or groups;
- `features(row)` converts the current row to byte-like MinHash features.

That boundary is explicit because retaining complete Arrow/Polars/Hugging Face rows can defeat bounded source batching. Project an ID or small reference when the original table is large.

```python
from pari import DedupeIndex
from pari.integrations import add_pyarrow

index = DedupeIndex[str](None, threshold=0.8, num_perm=128, batch_size=2048)
try:
    add_pyarrow(
        index,
        "records.parquet",
        columns=["id", "text"],
        batch_size=65_536,
        record=lambda row: row["id"],
        features=lambda row: (
            token.encode("utf-8") for token in row["text"].casefold().split()
        ),
    )
    pairs = index.candidate_pairs()
finally:
    index.close()
```

The adapter's `batch_size` controls source conversion. `DedupeIndex.batch_size` independently controls native signature and insertion batches.

## PyArrow and Parquet

`iter_pyarrow_rows` accepts:

- a Parquet path;
- `pyarrow.Table`, `RecordBatch`, or `RecordBatchReader`;
- an Arrow dataset object with `to_batches()`.

Parquet paths use [`ParquetFile.iter_batches`](https://arrow.apache.org/docs/python/generated/pyarrow.parquet.ParquetFile.html#pyarrow.parquet.ParquetFile.iter_batches), which caps records per yielded batch. Arrow buffers remain columnar until one bounded record batch crosses the Python boundary through `to_pylist()`. At that point values and row dictionaries are Python copies.

```python
from pari.integrations import iter_pyarrow_rows

for row in iter_pyarrow_rows(
    "records.parquet", columns=["id", "text"], batch_size=32_768
):
    ...
```

`add_pyarrow` passes the iterator directly to `DedupeIndex.add_many_features`.

## Polars

`iter_polars_rows` accepts `DataFrame`, `LazyFrame`, or a Parquet path. DataFrames use [`iter_slices`](https://docs.pola.rs/api/python/stable/reference/dataframe/api/polars.DataFrame.iter_slices.html); the frame is already materialized, but Python row conversion stays chunked. LazyFrames and Parquet scans use [`collect_batches`](https://docs.pola.rs/api/python/stable/reference/lazyframe/api/polars.LazyFrame.collect_batches.html) with order preservation and the streaming engine.

Polars currently marks `collect_batches` unstable and slower than native sinks. Pari checks for the method and fails with an upgrade message instead of silently calling `collect()` and materializing the full result.

```python
import polars as pl
from pari.integrations import add_polars

lazy = pl.scan_parquet("records.parquet").filter(pl.col("active"))
add_polars(
    index,
    lazy,
    columns=["id", "text"],
    record=lambda row: row["id"],
    features=lambda row: [row["text"].encode("utf-8")],
)
```

## Hugging Face Datasets

`iter_huggingface_rows` accepts `Dataset` and `IterableDataset`. It applies `select_columns` before using the library's documented [`iter(batch_size=...)`](https://huggingface.co/docs/datasets/package_reference/main_classes) batch iterator. This preserves streaming behavior for `IterableDataset`; it does not convert the full dataset to a Python list.

```python
from datasets import load_dataset
from pari.integrations import add_huggingface

rows = load_dataset("json", data_files="records.jsonl", split="train", streaming=True)
add_huggingface(
    index,
    rows,
    columns=["id", "text"],
    batch_size=1_000,
    record=lambda row: row["id"],
    features=lambda row: (token.encode() for token in row["text"].split()),
)
```

## Nulls, columns, and errors

Adapters preserve library-converted values, including `None`. Pari does not guess whether a null should be skipped, replaced, or treated as an empty feature set; the callbacks own that policy. Missing columns and unsupported nested conversions propagate the source library's error. Batch columns with inconsistent lengths fail explicitly.

Column projection happens before row conversion wherever the source supports it. Row order is retained. A non-positive batch size fails before loading any optional dependency.

## Tests and stability

The normal Python matrix verifies that the module imports without extras and that missing-dependency errors remain actionable. A separate CI job installs all three extras and runs real Table, Parquet, DataFrame, LazyFrame, Dataset, and IterableDataset tests with batch sizes smaller than the fixtures.

These adapters are experimental in the v0.x compatibility policy because upstream batch APIs, especially Polars streaming collection, may change. Core MinHash, index, and persistence semantics are unaffected.
