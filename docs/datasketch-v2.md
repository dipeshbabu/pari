# Datasketch 2.x interoperability and migration

Pari can exchange MinHash state with Datasketch 2.x only when the complete signature semantics match. An equal seed and permutation count are not enough: Datasketch and Pari deliberately use different seed-to-permutation generators.

Install the optional dependency only when migrating:

```bash
python -m pip install ".[datasketch]"
```

For a future published release containing this adapter, the equivalent command is `python -m pip install "pari-similarity[datasketch]"`.

`import pari` does not import or require Datasketch, NumPy, or SciPy. Adapter functions live in `pari.datasketch` and load the optional dependency on first use.

## Compatibility matrix

| Surface | Datasketch 2.x | Pari | Status |
| --- | --- | --- | --- |
| Default input hash, affine32 | SHA-1 low 32 bits | SHA-1 low 32 bits | Exact |
| affine32 pre-mix and permutation arithmetic | MurmurHash3 `fmix32`, then wrapping `a*h+b` | Same | Exact |
| Default input hash, affine64 | SHA-1 low 64 bits | SHA-1 low 64 bits | Exact |
| affine64 pre-mix and permutation arithmetic | MurmurHash3 `fmix64`, then wrapping `a*h+b` | Same | Exact |
| Default seed mapping | NumPy `RandomState` | Stable SplitMix64 mapping | **Incompatible**, even for equal seeds |
| affine32 with explicit Pari multiplier/offset arrays | Supported | `pari-affine32-v1` | Value-for-value compatible and convertible |
| affine64 with explicit Pari multiplier/offset arrays | Supported | `pari-affine64-v1` | Value-for-value compatible at the Rust/core level |
| Custom input hash function | User-defined | SHA-1 width-specific default | State may match accidentally but future updates are incompatible; adapter rejects |
| Datasketch `legacy` scheme | 61-bit-prime legacy permutations | Not implemented | Unsupported; rebuild from source values |
| Datasketch default affine32 signature state | Valid Datasketch state | Different permutation family | Not convertible without original values |
| Python affine64 indexing | Datasketch supports signatures | Pari Python `Index` currently accepts affine32 | Signature arithmetic is proven; Python index ingestion is not supported |
| LSH tables and serialized indexes | Datasketch-specific storage/banding | Versioned `.pari` format | Not interchangeable; rebuild the Pari index |
| Pickles / LeanMinHash bytes | Datasketch formats | Explicit Pari signatures and `.pari` files | No direct deserialization or executable-object migration |

The adapter rejects every unsupported row rather than inferring compatibility from a seed, dtype, or value range.

## Preferred migration: rebuild from feature values

When source features are available, rebuild signatures in bounded Pari batches. This is the only safe path for ordinary Datasketch sketches created with its default permutation generator:

```python
from pari import Index, MinHash

feature_rows = [
    [b"new york", b"rust", b"search"],
    [b"new york", b"python", b"search"],
]
signatures = MinHash.from_batch(feature_rows, num_perm=128, seed=7)

with Index.create("documents.pari", threshold=0.8, num_perm=128, seed=7) as index:
    index.add_many(list(enumerate(signatures)))
```

This preserves feature construction and Jaccard semantics while adopting Pari's stable `pari-affine32-v1` permutation family and persistent format.

## Exact state migration for compatible sketches

`to_datasketch` creates a Datasketch affine32 object with Pari's exact multiplier and offset arrays. The returned object remains update-compatible in both libraries:

```python
from pari import MinHash
from pari.datasketch import to_datasketch

pari_sketch = MinHash.from_values([b"a", b"b"], num_perm=128, seed=7)
datasketch_sketch = to_datasketch(pari_sketch)
```

If a Datasketch sketch was created with those explicit arrays, import it without rehashing original features:

```python
from pari.datasketch import from_datasketch

pari_sketch = from_datasketch(datasketch_sketch)
```

The import checks:

1. Datasketch major version 2;
2. `scheme == "affine32"`;
3. the Datasketch SHA-1 32-bit input hash function;
4. valid u64 seed and u32 signature values;
5. exact multiplier and offset equality with Pari's stable seed mapping.

The reconstructed Pari sketch remains safe to update, merge, index, query, and persist because the permutation family is proven identical. `is_compatible` performs the same conservative check and returns a boolean.

Default Datasketch construction is intentionally rejected:

```python
from datasketch import MinHash as DatasketchMinHash
from pari.datasketch import from_datasketch

ordinary = DatasketchMinHash(num_perm=128, seed=7, scheme="affine32")
from_datasketch(ordinary)  # raises pari.CompatibilityError
```

Do not copy `hashvalues` into Pari and relabel them manually. Updating such a sketch would mix two different permutation families, and persisting it would make `.pari` compatibility metadata false.

## affine64 scope

The checked-in cross-implementation fixture proves that Datasketch `affine64` and Rust `pari_core::MinHash64` produce identical values when SHA-1 input hashing and Pari multiplier/offset arrays are used. Pari's current Python `MinHash` and `Index` surface is affine32, so the optional Python adapter rejects affine64 ingestion instead of narrowing values or mislabeling the scheme.

Rust applications can use `MinHash64::permutations()` to configure an equivalent external implementation and compare against `pari-affine64-v1`. A future 64-bit Python/index surface must carry an explicit 64-bit scheme identifier through storage before adapter support can be added safely.

## Executable compatibility evidence

`crates/pari-core/testdata/datasketch_v2_affine.json` was generated by Datasketch 2.0.0 using identical inputs and explicit Pari permutations. It covers:

- affine32 and affine64 multiplier/offset arrays;
- compatible signatures for `b"a"`, `b"b"`, and `b"c"`;
- Datasketch default equal-seed signatures, which must remain different.

Rust tests load the fixture and verify both Pari schemes. The optional Python interoperability job regenerates the fixture, exercises bidirectional affine32 updates, rejects default and affine64 adapter imports, and indexes/queries imported signatures with Pari.

Regenerate the fixture only with the pinned version and review every diff:

```bash
python -m pip install "datasketch==2.0.0"
python scripts/generate_datasketch_v2_golden.py
```

## Semantics-first benchmark

Run the focused migration benchmark after installing the current Pari checkout and Datasketch 2.0.0:

```bash
python benchmarks/datasketch_interop.py \
  --items 5000 \
  --queries 100 \
  --output datasketch-interop.json
```

The benchmark aborts unless every Datasketch signature equals the corresponding Pari signature and every converted self-query is found. It reports signature construction, adapter import, Pari index build/query throughput, and index bytes.

It does **not** compare Datasketch and Pari candidate sets or claim LSH speed superiority. The libraries may choose different bands, rows, bucket hashing, storage, and operating models even when signature values are identical. Broader comparisons belong in issue #47 and must state those differences.
