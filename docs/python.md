# Python API

Pari ships a small typed Python API backed by the same Rust implementation used by the native crates. The wheel uses Python's stable ABI with a minimum of CPython 3.10.

## Install from the repository

```bash
python -m pip install .
```

For local extension development:

```bash
python -m pip install "maturin>=1.14,<2"
maturin develop
```

The distribution is named `pari-similarity`; the import is simply `pari`.

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

`MinHash.update` and `MinHash.from_values` accept byte-like inputs. Python `bytes` use the direct borrowed path for scalar updates. `bytearray`, `memoryview`, and other contiguous unsigned-byte buffers are accepted through the Python buffer protocol. Batch values are copied into Rust-owned storage before the GIL is released so Python memory is never accessed without interpreter ownership.

`update_many` performs the CPU-heavy hashing and permutation loop through `Python::detach`, so the Python interpreter is not held while Rust performs the batch computation.

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

`Index.search` returns **approximate candidate keys** that share one or more configured LSH bands. It is not an exact Jaccard filter. Applications that require an exact threshold should retain or reconstruct the source signatures and verify returned candidates with `MinHash.jaccard`.

## Reopen an index

```python
from pari import Index

with Index.open("documents.pari") as index:
    print(len(index))
    print(index.stats().file_bytes)
```

The context manager calls `sync()` and closes the Python handle on exit. `close()` is idempotent. Operations on a closed handle raise `ClosedIndexError`.

See [persistence.md](persistence.md) for the local backend's writer, reader, durability, crash, and backup semantics.

## Mutations

```python
index.add(400, signature)
removed = index.remove(200)
index.flush()  # atomic committed generation
index.sync()   # also sync the parent directory
```

Mutations are visible through the same Python handle immediately. `flush`, `sync`, and `close` delegate to the production `PersistentIndex32` implementation rather than duplicating persistence logic in the binding.

All persistent index operations that may perform Rust compute or filesystem work run outside the Python GIL. The Python binding keeps the Rust index behind a synchronized handle so scalar and batch calls share the same safety contract.

## Exceptions

All Pari-specific Python exceptions derive from `PariError`:

- `ConfigurationError`: invalid threshold, permutation count, or index configuration.
- `CompatibilityError`: a sketch has the wrong seed or permutation count for another sketch or index.
- `DuplicateKeyError`: an insert would reuse an existing key.
- `StorageError`: filesystem, format, checksum, or persistence failures.
- `ClosedIndexError`: an operation requires a handle that has already been closed.

These exception classes are stable API surface; callers do not need to parse Rust error strings.

## Typing

The wheel includes `pari/__init__.pyi` and `pari/py.typed`. Editors and static type checkers therefore see the public signatures without importing PyO3 internals.

The type surface accepts `str` and `os.PathLike[str]` paths, `bytes | bytearray | memoryview` values, integer keys, `MinHash` sketches, and typed `IndexStats` results.

## Supported Python versions

The first wheel line supports CPython 3.10 through 3.14 on Linux, macOS, and Windows. The native extension is built with `abi3-py310`, so one platform wheel can target the stable ABI rather than requiring a different extension ABI for every CPython minor release.
