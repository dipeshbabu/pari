# Shared storage backends

`pari-backend` provides a typed storage contract for similarity indexes that need to live outside one process. It is separate from `pari-store`, which remains the crash-safe local file backend.

The high-level type is `BackendIndex32<B>`, where `B` implements `StorageBackend`. The index layer owns MinHash compatibility checks, LSH band hashing, batch orchestration, and candidate aggregation. A backend only stores validated external keys, per-key band hashes, and bucket membership.

This separation is intentional. Redis command details, connection handling, TTL behavior, and backend serialization do not leak into `pari-index` or the public similarity algorithm.

## Backend contract

A `StorageBackend` declares explicit capabilities and implements batch operations for:

- namespace initialization and descriptor loading
- batch key existence checks
- atomic batch insertion
- batch bucket lookup
- batch deletion
- flush/completion barriers
- health checks and statistics
- namespace-owned cleanup

Scalar operations on `BackendIndex32` delegate to the batch surface. Remote implementations therefore do not need one network round trip per application record.

`MemoryBackend` is the in-process reference implementation. It runs the same logical contract tests as Redis and intentionally does not advertise TTL or remote capabilities.

## Implementing another backend

`StorageBackend` is designed to be implemented outside the `pari-backend` crate. A custom backend can build its capability set with `BackendCapabilities::empty().with(...)`, and can reconstruct persisted index metadata with the validated public `IndexDescriptor::new(...)` constructor.

`BackendIndex32` constructs `StoredItem` values after validating each MinHash sketch. Backend implementations receive those values through `insert_many` and can inspect the external key and per-band hashes through read-only getters. This keeps LSH hashing policy owned by Pari instead of letting each database adapter reimplement it.

A remote backend should make its native batch operations real batch operations. In particular, `contains_many` and `query_buckets` should not perform one network request per input, and `insert_many` should either commit the complete validated batch or fail without partial mutation.

The integration contract tests are intentionally compiled as an external crate. They verify that the public descriptor and capability construction APIs are sufficient for third-party backend implementations in addition to exercising the built-in memory and Redis adapters.

## Redis backend

Enable the optional Redis implementation with the `redis` feature:

```toml
pari-backend = { path = "crates/pari-backend", features = ["redis"] }
```

Create a shared index:

```rust
use pari_backend::{BackendIndex32, RedisBackend};
use pari_core::MinHash32;

let backend = RedisBackend::connect("redis://127.0.0.1:6379/", "documents")?;
let mut index = BackendIndex32::create(backend, 0.8, 128, 7, None)?;

let mut sketch = MinHash32::new(128, 7)?;
sketch.update_many([b"new york", b"rust", b"search"]);
index.insert(1, &sketch)?;
let candidates = index.query(&sketch)?;
# Ok::<(), Box<dyn std::error::Error>>(())
```

A second process can connect to the same namespace and call `BackendIndex32::open` to use the stored descriptor and data.

## Namespace ownership

A Redis namespace may contain only ASCII letters, digits, `.`, `_`, and `-`, with a maximum length of 128 bytes.

Pari owns exactly three Redis keys per namespace:

```text
pari:<namespace>:meta
pari:<namespace>:records
pari:<namespace>:buckets
```

`meta` contains a fixed-width, versioned, data-only index descriptor. `records` is a Redis hash keyed by the safe `U64Codec`. `buckets` is one lexicographically indexed sorted set of fixed-width bucket membership records.

Using one sorted set rather than one Redis key per LSH bucket keeps namespace ownership bounded and makes cleanup and TTL application deterministic.

`cleanup()` deletes only those three keys. It never scans Redis and never deletes data outside the selected Pari namespace.

## Serialization and security

Remote storage does not use pickle, native language object serialization, or executable payloads.

External integer keys are encoded through `pari-format`'s `U64Codec`. The index descriptor is fixed-width and bounds checked before use. Bucket members have a fixed binary width and include the band/hash prefix used for lookup; malformed records are rejected.

Redis connection URLs are used only to establish the connection. They are not retained in the backend object, debug output, or operational error messages. Backend transport errors report the operation and Redis error category without including credentials.

## Batch and atomicity behavior

Redis insertion and deletion are implemented as atomic Lua operations after complete batch validation. A duplicate key or malformed stored record therefore fails the complete batch instead of leaving a partially applied index mutation.

Batch existence checks and bucket queries use Redis pipelines. `BackendIndex32::query_many` also deduplicates repeated bucket requests across the query batch before calling the backend.

`BackendStats.round_trips` reports network round trips made by the current Redis handle. It is intended for benchmark and operational evidence, not billing accounting.

## TTL and retention

Redis can apply one retention duration to all keys owned by a namespace:

```rust
use std::time::Duration;
use pari_backend::{BackendIndex32, RedisBackend};

let backend = RedisBackend::connect("redis://127.0.0.1:6379/", "recent-events")?;
let index = BackendIndex32::create(
    backend,
    0.8,
    128,
    7,
    Some(Duration::from_secs(86_400)),
)?;
# Ok::<(), Box<dyn std::error::Error>>(())
```

Retention is refreshed after successful index mutations. Queries do not refresh retention, so TTL represents time since the last successful mutation rather than time since the last read.

All three namespace keys receive the same configured retention. When the metadata key expires, `BackendIndex32::open` returns `NotFound`. Empty Redis hashes or sorted sets may disappear before metadata after deleting their final member; this does not change namespace ownership or open semantics.

`MemoryBackend` rejects TTL configuration explicitly rather than silently ignoring it.

## CI and benchmark evidence

The Redis workflow uses a pinned Redis service on pull requests. It runs the shared backend contract against the real service, exercises cross-handle visibility and TTL expiry, and runs the Redis batch benchmark smoke.

The benchmark reports insert throughput, query throughput, live bucket membership count, and backend round trips. Timing values are evidence only; shared CI runners do not use wall-clock thresholds as correctness gates.
