# MinHash LSH index

`pari-index` provides distinct in-memory threshold LSH indexes for `MinHash32`
and `MinHash64` signatures. Use `LshIndex32` for `pari-affine32-v1` sketches
and `LshIndex64` for `pari-affine64-v1`; the two types do not reinterpret or
narrow signatures across widths.

Use the canonical [planner and explain API](planning.md) to select bands, rows, and storage capacity without reproducing Pari's probability formulas.

## Why batch first

The scalar `insert` and `query` methods are convenience APIs. The engine also exposes `insert_many` and `query_many`, and the batch paths are designed to avoid repeated setup work. `query_many` reuses its candidate scratch set across queries, while batch insertion validates the complete batch before mutating buckets.

## Candidate semantics

An LSH hit means that at least one signature band collided. It does **not** prove that two original sets meet the configured Jaccard threshold. Pari therefore calls the returned values candidates. A later verification layer can compare original features or application-specific evidence before accepting a match.

This separation is useful for deduplication:

```text
large corpus
    |
    v
MinHash signatures
    |
    v
Pari LSH candidate generation
    |
    v
optional exact verification
    |
    v
duplicate groups
```

### Similarity scores

An in-memory index can keep each item's full signature and rank candidates by
estimated Jaccard similarity:

```rust
use pari_index::{LshIndex32, ScoreFilter};

let mut index = LshIndex32::new(0.8, 128, 7)?.with_signature_storage();
index.insert(1, &sketch)?;
for candidate in index.query_scored(&query, ScoreFilter::new().min_similarity(0.8).top_k(10))? {
    println!("{} {:.3}", candidate.key, candidate.similarity);
}
```

Call `with_signature_storage` before inserting; it costs `num_perm` `u64`
values per item. `query_scored` and `query_scored_many` return only LSH
candidates, sorted by similarity and then key. A candidate without a stored
signature returns `LshError::SignaturesUnavailable`, and an out-of-range filter
returns `LshError::InvalidScoreFilter`. `ScoreFilter` and `ScoredCandidate` are
shared with the persistent and lazy indexes.

## Parameter tuning

`LshIndex32::new(threshold, num_perm, seed)` and
`LshIndex64::new(threshold, num_perm, seed)` automatically choose bands and
rows by minimizing an equal-weight combination of:

- false-positive probability area below the target threshold
- false-negative probability area above the target threshold

The objective follows the useful tuning idea in `ekzhu/datasketch`'s `MinHash` LSH implementation. Pari evaluates the integrals with a small built-in Simpson integrator instead of depending on `SciPy`.

Automatic tuning is intentionally capped at 4096 permutation values. Larger
signatures can still be indexed with the width-matched `with_params(...)`
constructor; the cap prevents accidental pathological constructor work.

## Memory layout

Buckets contain compact `u32` internal IDs rather than user keys. The index separately stores:

- external `u64` key to internal ID mapping
- internal ID to external key mapping
- per-item band hashes needed for removal

Full `MinHash` signatures are not duplicated inside the index.

Affine64 band hashes consume each complete `u64` value. Upper bits are not
discarded or relabeled as affine32 values. `LshIndex64::explain()` consequently
reports eight signature bytes per permutation while retaining the canonical
width-independent LSH probability model.

## Determinism and concurrency

Candidate aggregation uses a hash set internally, but public query results are sorted by external key before being returned. Immutable queries only borrow the index, so independent readers can query concurrently without a global query lock.

## Current boundary

`LshIndex64` is intentionally in-memory and Rust-only in this first slice of
[issue #124](https://github.com/dipeshbabu/pari/issues/124). Persistent local
storage, safe codecs, Python bindings, CLI support, and Datasketch affine64
migration are later, separately reviewed slices. Existing affine32 persistence
must not be used to store or relabel affine64 signatures.
