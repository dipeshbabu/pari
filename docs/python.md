# Python API

Pari ships a small typed Python API backed by the same Rust implementation used by the native crates. The wheel uses Python's stable ABI with a minimum of CPython 3.10.

## Plan and explain LSH settings

```python
from pari import Index, plan_lsh

plan = plan_lsh(
    1_000_000,
    threshold=0.8,
    num_perm=128,
    memory_budget_bytes=2 * 1024**3,
    storage="auto",
)
print(plan.bands, plan.rows, plan.recommended_storage)

with Index.open("documents.pari") as index:
    current = index.explain()
    print(current.parameter_source, current.candidate_probability_at_threshold)
```

`LshPlan` is produced by the canonical Rust model used by the CLI. It exposes signature cost, modeled index and resident bytes, budget fit, candidate probabilities, and an explicit storage recommendation reason. Estimates are not measured guarantees. See [LSH planning and explanation](planning.md) for model assumptions and validation evidence.

`DedupeIndex.candidate_pairs()` returns the native index's unique LSH bucket-collision pairs as record pairs. Use it when a downstream verifier or labeled evaluation needs real candidate edges rather than connected-component closure. See the [entity matching workload](entity-matching.md) for a complete example.

## Install the published package

```bash
python -m pip install "pari-similarity==0.3.1"
```

The PyPI distribution is named `pari-similarity`; the import namespace is `pari`.

## Install from source for contributors

From a repository checkout:

```bash
python -m pip install .
```

For local extension development:

```bash
python -m pip install "maturin>=1.14,<2"
maturin develop
```

## Stability

The 0.1 top-level Python export set is defined by `pari.__all__` and pinned by installed-wheel tests. Patch releases in 0.1.x must not intentionally break exports classified as supported or make a previously valid supported typed call invalid. Planner exports remain explicitly experimental. See [compatibility.md](compatibility.md) for the full v0.x policy, deprecation rules, signature compatibility, and persisted-format guarantees.

## Search a read-only snapshot

Use `ReadOnlyIndex` when a process only needs to query a committed `.pari`
file. It keeps the key catalog and bucket directory in memory and reads
matching bucket members from disk. The reader selects the signature width from
the file and exposes the seed and permutation count needed to build a query.

```python
from pari import MinHash, MinHash64, ReadOnlyIndex, shingles

with ReadOnlyIndex.open("products.pari") as index:
    sketch_type = MinHash if index.signature_width == 32 else MinHash64
    query = sketch_type.from_values(
        shingles("aluminum travel mug"), num_perm=index.num_perm, seed=index.seed
    )
    candidates = index.search(query)
    if index.stores_signatures:
        ranked = index.search_scored(query, top_k=5, min_similarity=0.8)
```

`search_many` and `search_scored_many` return one result list per sketch in
input order. Scored queries require a file created with
`store_signatures=True`. A query with a different width, seed, or permutation
count raises `CompatibilityError`. Open a new reader to see a later commit.
For an older snapshot that lacks bucket sections, open and sync it with
`Index` or `Index64` once to upgrade it.

## Deduplicate records

> **Availability:** `DedupeIndex` and `deduplicate` are included in Pari 0.3.0 and newer releases.

`deduplicate` is the concise API for users who do not need to manage signatures or an index directly. Supply a feature callback that returns byte-like shingles for one record:

```python
from pari import deduplicate

documents = [
    {"id": 1, "text": "Rust makes systems programming practical"},
    {"id": 2, "text": "Rust makes systems programming practical"},
    {"id": 3, "text": "Python is useful for data workflows"},
]

result = deduplicate(
    documents,
    feature=lambda row: (word.casefold().encode() for word in row["text"].split()),
    threshold=0.8,
    num_perm=128,
    seed=7,
)

print(result.groups[0].member_indices)  # (0, 1)
print([row["id"] for row in result.kept])  # [1, 3]
print([row["id"] for row in result.dropped])  # [2]
```

The default representative is the first record in ingestion order. A selector can keep a preferred member instead:

```python
result = deduplicate(
    documents,
    feature=features,
    representative=lambda members: max(members, key=lambda row: row["quality"]),
)
```

The selector must return one of the exact member objects it receives. Returning an unrelated object raises `InvalidRepresentativeError`.

LSH groups are approximate connected components. Use `exact` when candidate pairs need application-level verification:

```python
result = deduplicate(
    records,
    feature=features,
    exact=lambda left, right: normalized_distance(left, right) <= 2,
)
```

The verifier runs before native components are joined; it does not post-filter already connected groups or alter LSH internals. Exceptions raised by feature, verifier, or representative callbacks propagate to the caller.

For a similarity floor without writing a verifier, pass `min_similarity`:

```python
result = deduplicate(records, feature=features, threshold=0.8, min_similarity=0.9)
```

Candidate pairs whose estimated Jaccard similarity is below `min_similarity` are rejected natively before components are joined and before any `exact` verifier runs, so the callback sees fewer pairs. The engine keeps each record's signature (`num_perm × 4` bytes) only when `min_similarity` is set. `DedupeIndex.similarity(left, right)` returns the estimate for two ingestion indexes. `candidate_groups()` and `candidate_pairs()` remain unverified LSH views. Values outside `[0, 1]` raise `ConfigurationError`.

### Incremental and persistent ingestion

`DedupeIndex` exposes the same operation incrementally. `add_many` consumes any iterable in bounded batches, and `add` has the same ingestion-order semantics:

```python
from pari import DedupeIndex

with DedupeIndex(features, batch_size=2048) as index:
    index.add_many(stream_of_records())
    result = index.result()
```

Feature extraction runs in Python. Each batch's byte buffers are copied into Rust-owned memory before CPU-heavy MinHash construction, insertion, and grouping run outside the GIL. The native grouping path unions LSH buckets directly and does not materialize all candidate pairs.

Streaming integrations that should discard large source payloads immediately can retain a lightweight record reference and provide precomputed features separately:

```python
with DedupeIndex(None, batch_size=2048) as index:
    index.add_many_features((record_ref, shingles) for record_ref, shingles in rows)
    result = index.result()
```

`candidate_groups()` exposes the unverified LSH components for measurement while `groups()` and `result()` apply the configured exact verifier. Batch signature consumers can use `MinHash.from_batch(feature_rows, num_perm=..., seed=...)`; all Python buffers are copied before the batch computation detaches from the GIL.

The default `memory` backend is fastest for one-shot jobs. To mirror the same native batches into a crash-safe local index, supply a new path:

```python
with DedupeIndex(features, backend="local", path="records.pari") as index:
    index.add_many(records)
    result = index.result()
```

The local file contains signatures and bucket membership, not the original Python records. Reopen it with the lower-level `Index` API for candidate queries; reconstructing high-level results still requires the source records.

### Feature callback patterns

For plain text, `shingles` returns overlapping word or character n-grams as bytes, ready to use as a feature:

```python
from pari import deduplicate, shingles

result = deduplicate(documents, feature=lambda text: shingles(text, 3))
typo_tolerant = deduplicate(names, feature=lambda name: shingles(name, 3, unit="char"))
```

`shingles(text, size=3, *, unit="word", normalize=True)` NFKC-normalizes and case-folds the text, splits words on Unicode word characters (ignoring punctuation), and returns each run of `size` consecutive words, without duplicates and in first-seen order. `unit="char"` uses runs of `size` characters with whitespace collapsed, which tolerates typos and suits text without spaces. Text shorter than one shingle yields a single shingle and empty text yields none. Larger sizes are stricter: 1–2 for short titles, 3–5 for paragraphs and documents. `normalize=False` keeps the original text and splits words on whitespace.

Other domains still choose their own tokenization:

```python
# Text: custom word tokens.
text_features = lambda text: (word.encode() for word in text.casefold().split())

# Code: normalized non-empty source lines.
code_features = lambda source: (
    line.strip().encode() for line in source.splitlines() if line.strip()
)

# Records: explicit field/value tokens.
def record_features(row):
    yield f"email:{row['email'].casefold()}".encode()
    yield f"postal:{row['postal_code']}".encode()
```

This keeps similarity semantics explicit while the signature, index, and grouping implementation remains shared across workloads.

## Build signatures

```python
from pari import MinHash

first = MinHash.from_values(
    [b"new york", b"similarity search", b"rust"],
    num_perm=128,
    seed=7,
)

second = MinHash(num_perm=128, seed=7)
second.update_many([b"new york", b"similarity search", b"python"])

print(first.jaccard(second))
```

`MinHash.update` and `MinHash.from_values` accept byte-like inputs. Python `bytes` use the direct borrowed path for scalar updates. `bytearray`, `memoryview`, and other buffer exporters are hashed as raw bytes, equivalent to `memoryview(value).tobytes()` in C order, preserving element width and the supplied byte order. This applies to strided views and to `MinHash64` as well. Batch values are copied into Rust-owned storage before the GIL is released so Python memory is never accessed without interpreter ownership.

Earlier bindings incorrectly treated some multi-byte buffers as integer sequences when every element fit in `u8`. Rebuild sketches and indexes created from those buffers; byte-oriented inputs were unaffected.

`update_many` performs the CPU-heavy hashing and permutation loop through `Python::detach`, so the Python interpreter is not held while Rust performs the batch computation.

`MinHash.from_batch(..., threads=None)` automatically uses bounded CPU parallelism for batches of at least 256 rows. Pass `threads=1` for scalar execution or a larger positive integer to set a maximum. `DedupeIndex` and `deduplicate` accept the same `threads` option. See [CPU parallelism](parallelism.md) for crossover results and memory behavior.

`MinHash.from_signature` reconstructs a sketch only when the caller has already established `pari-affine32-v1` compatibility. `MinHash.permutations` exposes the stable multiplier and offset arrays used by the optional, conservatively checked [Datasketch 2.x adapter](datasketch-v2.md). Ordinary Datasketch equal-seed signatures are not compatible and must be rebuilt from source features.

### Opt in to full-width affine64 signatures

`MinHash64` is a separate type for applications that deliberately choose
`pari-affine64-v1`. It mirrors scalar updates, bounded ordered batches,
similarity, merge, clear, signature reconstruction, and permutation access:

```python
from pari import MinHash64

rows = [[b"new york", b"rust"], [b"new york", b"python"]]
first, second = MinHash64.from_batch(rows, num_perm=128, seed=7)
restored = MinHash64.from_signature(first.signature, seed=7)

assert restored.signature == first.signature
print(first.jaccard(second))
```

Python integers retain every `u64` signature and permutation bit. `MinHash64`
does not reinterpret `MinHash` values, and cross-width comparison or merge
raises `CompatibilityError` before either sketch changes.

## Create and query a persistent index

```python
from pari import Index, MinHash

alpha = MinHash.from_values([b"a", b"b", b"c"], num_perm=128, seed=7)
beta = MinHash.from_values([b"a", b"b", b"d"], num_perm=128, seed=7)
gamma = MinHash.from_values([b"x", b"y", b"z"], num_perm=128, seed=7)

with Index.create("documents.pari", threshold=0.8, num_perm=128, seed=7) as index:
    index.add(100, alpha)
    index.add_many([(200, beta), (300, gamma)])

    print(index.search(alpha))
    print(index.search_many([alpha, gamma]))
    print(index.stats())
```

The common API selects LSH bands and rows automatically from the threshold and signature length. Advanced Rust APIs can still use explicit parameters, but Python users do not need to understand storage layout or banding to create an index.

`Index.search` returns **approximate candidate keys** that share one or more configured LSH bands. It is not a similarity filter.

To rank or threshold candidates, create the index with `store_signatures=True` and use `search_scored`:

```python
with Index.create("documents.pari", threshold=0.8, store_signatures=True) as index:
    index.add_many(items)
    for key, similarity in index.search_scored(query, min_similarity=0.8, top_k=10):
        print(key, round(similarity, 3))
```

`search_scored` returns `(key, similarity)` pairs for LSH candidates, most similar first with ties broken by key. Similarity is the fraction of equal signature values, the same estimate as `MinHash.jaccard`. `min_similarity` must be in `[0, 1]` and `top_k` must be positive; invalid values raise `ConfigurationError`, as does calling `search_scored` on an index created without signatures. `search_scored_many` applies one filter to a batch, `signature(key)` returns a stored sketch or `None`, and `stores_signatures` reports the setting. Stored signatures add `num_perm × 4` bytes per item on disk (`× 8` for `Index64`) and are read lazily during queries.

## Reopen an index

```python
from pari import Index

with Index.open("documents.pari") as index:
    print(len(index))
    print(index.stats().file_bytes)
```

The context manager calls `sync()` and closes the Python handle on exit. `close()` is idempotent after success. If saving fails, the storage exception propagates and the handle retains its pending changes so you can resolve the filesystem error and retry `sync()` or `close()`. This applies to `Index`, `Index64`, and local `DedupeIndex`, including context-manager exit. Operations on a closed handle raise `ClosedIndexError`.

See [persistence.md](persistence.md) for the local backend's writer, reader, durability, crash, and backup semantics.

## Mutations

```python
index.add(400, signature)
removed = index.remove(200)
index.flush()  # atomic committed generation
index.sync()   # also sync the parent directory
```

Mutations are visible through the same Python handle immediately. `flush`, `sync`, and `close` delegate to the production `PersistentIndex32` implementation rather than duplicating persistence logic in the binding.

### Persist affine64 signatures

Use the distinct `Index64` type with `MinHash64` signatures. Its lifecycle and
mutation API matches `Index`, but it creates and opens only
`pari-affine64-v1` snapshots:

```python
from pari import Index64, MinHash64

signature = MinHash64.from_values([b"a", b"b"], num_perm=128, seed=7)
with Index64.create("documents-64.pari", num_perm=128, seed=7) as index:
    index.add(100, signature)
    print(index.search(signature))

with Index64.open("documents-64.pari") as index:
    print(index.stats().file_bytes)
```

`Index64` rejects `MinHash` values, `Index` rejects `MinHash64` values, and
each index type rejects persisted files of the other width with
`CompatibilityError`. No conversion, narrowing, or relabeling occurs. The
optional Datasketch adapter supports full-width affine64 signatures, and the CLI
supports affine64 ingestion through its explicit signature-family option.

All persistent index operations that may perform Rust compute or filesystem work run outside the Python GIL. The Python binding keeps the Rust index behind a synchronized handle so scalar and batch calls share the same safety contract.

Create or open an index with `observability=True` to collect process-local query counts, candidate rate, and wall-clock latency. `IndexStats` also exposes exact committed bucket percentiles and overlay membership counts. Observation can be reset or disabled with `set_observability()`; it is never persisted into the index file.

High-level deduplication accepts a batch progress callback. Callbacks receive `ProgressEvent` after each completed native batch. Returning `False` raises `ProgressCancelledError` with the committed item count, while callback exceptions propagate unchanged. See [observability](observability.md) for examples and metric semantics.

## Exceptions

All Pari-specific Python exceptions derive from `PariError`:

- `ConfigurationError`: invalid threshold, permutation count, or index configuration.
- `CompatibilityError`: a sketch has the wrong seed or permutation count for another sketch or index.
- `DuplicateKeyError`: an insert would reuse an existing key.
- `StorageError`: filesystem, format, checksum, or persistence failures.
- `ClosedIndexError`: an operation requires a handle that has already been closed.
- `ProgressCancelledError`: a progress callback returned `False` after a completed batch; its `completed` field reports committed items.

These exception classes are stable API surface; callers do not need to parse Rust error strings.

## Typing

The wheel includes `pari/__init__.pyi` and `pari/py.typed`. Editors and static type checkers therefore see the public signatures without importing PyO3 internals.

The type surface accepts `str` and `os.PathLike[str]` paths, `bytes | bytearray | memoryview` values, integer keys, width-matched `MinHash` or `MinHash64` sketches, and typed `IndexStats` results.

## Supported Python versions

Published wheels support CPython 3.10 through 3.14 on Linux, macOS, and Windows. The native extension is built with `abi3-py310`, so one platform wheel can target the stable ABI rather than requiring a different extension ABI for every CPython minor release.
