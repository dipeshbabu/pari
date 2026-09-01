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
| affine64 with explicit Pari multiplier/offset arrays | Supported | `pari-affine64-v1` | Value-for-value compatible and convertible |
| Custom input hash function | User-defined | SHA-1 width-specific default | State may match accidentally but future updates are incompatible; adapter rejects |
| Datasketch `legacy` scheme | 61-bit-prime legacy permutations | Not implemented | Unsupported; rebuild from source values |
| Datasketch default affine32 signature state | Valid Datasketch state | Different permutation family | Not convertible without original values |
| Python affine64 indexing | Datasketch supports signatures | Pari has explicit `MinHash64` / `Index64` types | Exact adapter conversion and native indexing are supported |
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

`to_datasketch` creates a width-matched Datasketch `affine32` or `affine64` object with Pari's exact multiplier and offset arrays. The returned object remains update-compatible in both libraries:

```python
from pari import MinHash
from pari.datasketch import to_datasketch

pari_sketch = MinHash.from_values([b"a", b"b"], num_perm=128, seed=7)
datasketch_sketch = to_datasketch(pari_sketch)
```

Use the explicit affine64 type to preserve the complete signature width:

```python
from pari import MinHash64
from pari.datasketch import to_datasketch

pari_sketch = MinHash64.from_values([b"a", b"b"], num_perm=128, seed=7)
datasketch_sketch = to_datasketch(pari_sketch)
assert datasketch_sketch.scheme == "affine64"
```

If a Datasketch sketch was created with those explicit arrays, import it without rehashing original features:

```python
from pari.datasketch import from_datasketch

pari_sketch = from_datasketch(datasketch_sketch)
```

The import returns `MinHash` for `affine32` and `MinHash64` for `affine64`. It checks every field needed to prove the named semantics before constructing Pari state:

1. Datasketch major version 2;
2. `scheme` is exactly `affine32` or `affine64`;
3. the width-matched Datasketch SHA-1 input hash function;
4. valid u64 seed and width-matched signature values, counts, and array storage;
5. width-matched permutation array values, counts, and storage;
6. exact multiplier and offset equality with Pari's stable seed mapping.

The reconstructed Pari sketch remains safe to update, merge, index, query, and persist because the permutation family is proven identical. `is_compatible` performs the same conservative check and returns a boolean.

Default Datasketch construction is intentionally rejected:

```python
from datasketch import MinHash as DatasketchMinHash
from pari.datasketch import from_datasketch

ordinary = DatasketchMinHash(num_perm=128, seed=7, scheme="affine32")
from_datasketch(ordinary)  # raises pari.CompatibilityError
```

Do not copy `hashvalues` into Pari and relabel them manually. Updating such a sketch would mix two different permutation families, and persisting it would make `.pari` compatibility metadata false.

## Exact affine64 migration

The checked-in cross-implementation fixture proves that Datasketch `affine64` and `MinHash64` produce identical values, including values above `u32::MAX`, when SHA-1 input hashing and Pari multiplier/offset arrays are used. The adapter preserves those values without narrowing and the result can be updated, merged, queried through `Index64`, and persisted or reopened as `pari-affine64-v1`.

The two families remain explicit. An affine32 import never becomes `MinHash64`, an affine64 import never becomes `MinHash`, and either width is rejected by the other width's index before mutation. Ordinary equal-seed Datasketch affine64 sketches remain incompatible because their permutation arrays do not match Pari's stable mapping.

## Executable compatibility evidence

`crates/pari-core/testdata/datasketch_v2_affine.json` was generated by Datasketch 2.0.0 using identical inputs and explicit Pari permutations. It covers:

- affine32 and affine64 multiplier/offset arrays;
- compatible signatures for `b"a"`, `b"b"`, and `b"c"`;
- Datasketch default equal-seed signatures, which must remain different.

Rust tests load the fixture and verify both Pari schemes. The optional Python interoperability job regenerates the fixture, exercises bidirectional updates and merges for both widths, rejects default and mismatched adapter imports, and persists, reopens, indexes, and queries imported affine64 signatures with Pari.

Regenerate the fixture only with the pinned version and review every diff:

```bash
python -m pip install "datasketch==2.0.0"
python scripts/generate_datasketch_v2_golden.py
```

## Semantics-matched width benchmark

Run the focused migration benchmark after installing the current Pari checkout and Datasketch 2.0.0:

```bash
python benchmarks/datasketch_interop.py \
  --items 5000 \
  --queries 100 \
  --output datasketch-interop.json
```

The benchmark runs the same rows and LSH configuration through affine32 and affine64. It aborts unless every Datasketch signature equals the corresponding Pari signature, every converted self-query is found, and the Pari candidate sets match on this controlled corpus. It reports width-labeled signature construction, adapter import, Pari index build/query throughput, total and per-item signature bytes, and persisted index bytes.

The candidate gate compares Pari's two explicit widths for this benchmark workload; it is not a general promise that two independent MinHash samples produce identical candidates. The benchmark does **not** compare Datasketch and Pari LSH candidate sets or claim LSH speed superiority. The libraries may choose different bands, bucket hashing, storage, and operating models even when signature values are identical. Broader comparisons belong in issue #47 and must state those differences.
