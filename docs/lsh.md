# MinHash LSH index

`pari-index` provides Pari's first approximate candidate index: an in-memory threshold LSH for `MinHash32` signatures.

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

## Parameter tuning

`LshIndex32::new(threshold, num_perm, seed)` automatically chooses bands and rows by minimizing an equal-weight combination of:

- false-positive probability area below the target threshold
- false-negative probability area above the target threshold

The objective follows the useful tuning idea in `ekzhu/datasketch`'s `MinHash` LSH implementation. Pari evaluates the integrals with a small built-in Simpson integrator instead of depending on `SciPy`.

Automatic tuning is intentionally capped at 4096 permutation values. Larger signatures can still be indexed with `LshIndex32::with_params(...)`; the cap prevents accidental pathological constructor work.

## Memory layout

Buckets contain compact `u32` internal IDs rather than user keys. The index separately stores:

- external `u64` key to internal ID mapping
- internal ID to external key mapping
- per-item band hashes needed for removal

Full `MinHash` signatures are not duplicated inside the index.

## Determinism and concurrency

Candidate aggregation uses a hash set internally, but public query results are sorted by external key before being returned. Immutable queries only borrow the index, so independent readers can query concurrently without a global query lock.

## Current boundary

The first implementation is intentionally in-memory and `MinHash32` only. Persistent local storage, safe codecs, Python bindings, and Redis are separate roadmap issues so storage concerns do not leak into the indexing algorithm prematurely.
