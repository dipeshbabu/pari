# Changelog

All notable changes to Pari are documented here.

The project follows Semantic Versioning. During the 0.x series, compatibility guarantees are intentionally narrower than 1.x and are defined in `docs/compatibility.md`.

## [Unreleased]

### Changed

- `DedupeIndex.add_many` and `deduplicate` handle each batch in chunks of
  8,192 records in three overlapping stages: the calling thread collects a
  chunk from Python while a sketch thread builds the previous chunk's
  signatures and an insert thread adds the one before, instead of running the
  steps one after another. With `backend="local"`, the in-memory index and
  the local mirror (including its per-batch commit) are updated on separate
  threads, and the mirror may fall up to about one batch behind. A batch remains all-or-nothing: a failure removes every
  record of the batch again. `threads=1` keeps the sequential path.
- `MinHash.from_batch`, `MinHash64.from_batch`, `DedupeIndex` and
  `deduplicate` copy a batch's feature rows into one contiguous buffer instead
  of allocating every feature separately, and read rows that are exact lists
  or tuples without a Python iterator. Signatures are unchanged.
- `duplicate_groups` and the other grouping methods gather components with
  flat arrays indexed by node instead of a hash map with one vector per
  component, and no longer sort the default groups a second time. Output is
  unchanged.
- `duplicate_groups` (and `DedupeIndex.groups()` without a verifier) scans
  the LSH bands on several threads for large indexes, collecting links only
  from buckets with two or more live keys, while the calling thread joins
  them. Output is unchanged.
- `DedupeIndex.groups()` and `candidate_groups()` build `DuplicateGroup`
  objects in bulk when no `representative` callback is set: the native engine
  returns member tuples, fields are set directly, and cyclic garbage
  collection is paused while the groups are built. The groups are unchanged.
- `LshIndex32::insert_many` and `LshIndex64::insert_many` fill the band
  tables of a batch of at least 1,024 items on several threads, a group of
  bands each, while the calling thread records the keys. The index built is
  the same as before.
- A persistent-index snapshot rewrite (automatic compaction, `compact()`,
  `compact_and_close()`, and `DedupeIndex(backend="local")` `close()`)
  flushes the staged file to disk every 64 MiB on a background thread while
  it is written, instead of writing all of it in the final sync. A failed
  background flush fails the rewrite.
- `LshIndex32` and `LshIndex64` store a bucket's only member inline instead of
  in a separate heap allocation, and allocate a vector only once a second
  member arrives. Most buckets of a large index hold one member, so this
  saves memory and allocation time; the index's behavior is unchanged.
- `DedupeIndex` splits small batches, including the default `batch_size` of
  1,024, into about four chunks of at least 256 records, so collecting,
  sketching and inserting overlap within each batch too. Batches of 32,768
  records or more keep 8,192-record chunks.

## [0.4.2] - 2026-11-19

### Added

- `pari_format::BucketDirectory::split_keys` and `range_cursor`, and the
  `BucketRangeCursor` type, read the buckets of one key range of a snapshot or
  delta generation, verifying each directory block and member list, so
  several threads can read disjoint ranges at once.
- `PersistentIndex32::compact_and_close` and
  `PersistentIndex64::compact_and_close` compact and consume the handle
  without reopening the rewritten file.
- `pari_format::encode_bucket_segment_parts` encodes a bucket segment from
  member lists that are already encoded and checksummed.

### Changed

- A persistent-index snapshot rewrite (automatic compaction, `compact()`,
  `sync()` of a large change, and `DedupeIndex(backend="local")` `close()`)
  merges the committed generations, which already store keys and buckets in
  sorted order, instead of loading every live key's band hashes and sorting
  every membership again. Bucket ranges are merged on all CPU cores and the
  output is byte-for-byte the same. Rewrites no longer hold every live key's
  band hashes in memory. On a 1M-item index, a rewrite took 1.1 s instead of
  1.8 s and `DedupeIndex(backend="local")` closed in 5.3 s instead of 12.1 s.
  Files from older releases whose generations do not store keys in order fall
  back to the full rebuild.
- `DedupeIndex(backend="local")` `close()` no longer reopens the file it just
  rewrote, and both backends free the in-memory index on a background thread
  instead of making `close()` wait for it. At 10M records, `close()` took
  18.1 s instead of 65.4 s with the local backend and returned at once instead
  of after 17.8 s with the memory backend.

## [0.4.1] - 2026-11-19

### Added

- `PersistentIndex32::flush_delta` and `PersistentIndex64::flush_delta` commit
  pending changes as an appended delta generation whatever their size, for
  bulk ingest that commits often and compacts once at the end.

### Changed

- Persistent indexes no longer keep committed keys' band hashes in memory, only
  the set of live keys and the band hashes of uncommitted keys, about 300 bytes
  per key less. Opening a snapshot checks band-hash sections without decoding
  them, so reopening 10M items takes 5.9 s instead of 7.1 s. A snapshot rewrite
  reads committed band hashes back from the file, which makes it about 11%
  slower at 10M items (39.1 s instead of 35.2 s).
- `DedupeIndex(backend="local")` commits each ingest batch of about 65,536
  records as a delta generation instead of holding every record in the
  persistence mirror until `close()`, then compacts the file on `close()` after
  releasing its in-memory index. At 10M items, resident memory fell from 1,426
  to 1,063 bytes per item and peak memory from 15.9 GB to 12.1 GB (the `memory`
  backend uses 975 bytes and 11.3 GB); `close()` takes 89 s instead of 83 s. A
  crash during ingest keeps every committed batch.

## [0.4.0] - 2026-11-19

### Added

- Persistent-index commits append small changes to the index file as a delta
  generation (a checksummed record of inserted keys with their memberships and
  signatures, plus removed keys) instead of rewriting the whole snapshot, so a
  commit costs time proportional to the change. Queries, reopen, `ReadOnlyIndex`
  and `pari verify` apply every generation. A commit rewrites the snapshot
  automatically once 32 generations exist, the generations would exceed a
  quarter of the snapshot, or hidden memberships of removed keys exceed a
  quarter of the live keys. This covers `PersistentIndex32`/`PersistentIndex64`,
  Python's `Index`/`Index64`, `DedupeIndex(backend="local")`, and batched
  `pari index` builds.
- `compact()` on persistent indexes and Python's `Index`/`Index64`, and
  `pari compact --index PATH`, rewrite the file as one snapshot.
- `StoreStats::delta_generations` and `StoreStats::delta_bytes`, the matching
  Python `IndexStats` properties and `pari stats --json` fields,
  `LazyStats::delta_generations`, and `delta_generations` in
  `pari verify --json`.
- `pari_format::FileLayout::read_with_generations`, `refresh_generations`,
  `deltas`, `next_delta`, `base_length` and `uncommitted_bytes`,
  `ContainerWriter::write_delta`, and the `DeltaLayout`, `NextDelta` and
  `DeltaCommit` types for reading and writing appended generations.

### Changed

- `FileLayout::read_from` rejects a file with appended generations with the
  new `FormatError::AppendedGenerations` instead of reading only its base;
  damaged generations fail with `FormatError::InvalidGeneration`. Pari 0.3 and
  earlier cannot open a file with appended generations; compact it first.
  `StoreStats` and `LazyStats` gain fields, so code that builds them with
  struct literals must add them.
- `StoreStats::suppressed_base_keys` and `committed_buckets` now count every
  committed generation.

## [0.3.2] - 2026-11-18

### Fixed

- A persistent-index commit no longer silently overwrites a snapshot that
  another handle or process committed after this handle opened it. Commits
  serialize on an operating-system lock on a sibling `<index>.lock` file, which
  exists only while a commit runs, and fail with an I/O error, keeping the
  uncommitted changes in memory, when the file on disk has changed. This
  covers `PersistentIndex32`, `PersistentIndex64`, Python's `Index`/`Index64`,
  `DedupeIndex(backend="local")`, and `pari index`.

### Added

- `pari_format::BucketDirectory`, a validated sparse index over a snapshot's
  bucket directories that rejects most absent buckets from memory and finds
  present ones by reading one checksummed directory block of at most 2 KiB,
  and `BucketDistribution::from_size_counts` in `pari-index`.

### Changed

- Opened indexes keep bucket directories on disk. Persistent indexes,
  `ReadOnlyIndex`, `LazyIndex32`/`LazyIndex64`, and `pari verify` validate
  every directory when they open a snapshot but retain about 4.6 bytes per
  bucket instead of a 56-byte location per bucket, which was about 5 GB for a
  10M-item snapshot. Snapshot bytes are unchanged.
- Persistent indexes use less memory for uncommitted inserts: each overlay
  bucket keeps its first key inline and allocates a list only when it holds
  more than one key.

## [0.3.1] - 2026-11-18

### Added

- Open committed snapshots through Python's `ReadOnlyIndex` or
  `pari search --read-only`. Both select the signature width from the file and
  support scored queries when the snapshot stores signatures.

### Changed

- Persistent commits cap bucket sorting scratch space at 128 MiB, encode
  metadata one section at a time, and release the old mutation overlay before
  reopening the published snapshot. Snapshot bytes remain identical.

## [0.3.0] - 2026-11-18

### Added

- In-memory `LshIndex32`/`LshIndex64` can store signatures
  (`with_signature_storage`) and answer `query_scored`/`query_scored_many`;
  read-only `LazyIndex32`/`LazyIndex64` answer the same scored queries from
  files with stored signatures.
- A getting-started tutorial (`docs/tutorial.md`) backed by the runnable
  `examples/quickstart.py`, which deduplicates a CSV with `shingles` and
  `min_similarity` and searches the kept rows with scored queries.

### Changed

- `ScoreFilter` and `ScoredCandidate` moved to `pari-index` (still re-exported
  by `pari-store`), and `ScoreFilter::validate` now returns
  `ScoreFilterError`. `LshError` gains `SignaturesUnavailable` and
  `InvalidScoreFilter`, and `LazyStoreError` gains signature variants. These
  public-type changes are why this is a minor release; the Python API is
  unchanged.
- Persistent commits stream the snapshot instead of building it in memory:
  bucket memberships are sorted in parallel and written one segment at a time
  through the new `pari_format::ContainerWriter`. Snapshot bytes are unchanged
  (issue #235).

## [0.2.2] - 2026-11-18

### Added

- `benchmarks/dedupe_memory.py` measures the high-level `DedupeIndex` resident
  memory, throughput, and grouping cost for the in-memory and local backends,
  and the dedicated scale runner records it at 10M records for issue #96.
- Validated 10M scale evidence from a dedicated Linux host, with a same-host
  1M comparison, `DedupeIndex` memory measurements, and decisions for issues
  #95 and #96 in `docs/scale-10m-evidence.md`.

### Changed

- Generated benchmark reports derive the sharding and GPU decision bullets from
  bundle measurements instead of stating that storage is I/O-bound, which the
  bundles do not measure.
- The MIT license copyright line in the repository and every packaged public
  crate names Dipesh Tharu Mahato as the sole copyright holder. Third-party
  notices for datasketch and redis-rs are unchanged.

### Fixed

- Persistent indexes larger than about 3.7 million items failed to commit with
  `SectionTooLarge` because all band hashes were written as one section above
  the 256 MiB container limit (found by the 10M scale run, issue #95). Key
  metadata is now written as row-aligned `Keys`/`BandHashes` section pairs of
  about 64 MiB, and the persistent, lazy, and external-builder readers accept
  them. Indexes that fit in one pair keep the same bytes as before.
- The release workflow's PyPI recovery accepts a `recover_version` input
  instead of only restoring 0.1.0, so any release whose wheel filenames PyPI
  blocked can publish its archived, checksum-verified wheels with build tag 1.
- Release validation refuses versions whose PyPI filenames were previously
  uploaded and deleted (0.1.0 and 0.2.0), so a release cannot fail at the final
  PyPI upload after other registries have published.
- PyPI now serves 0.2.0 as build-1 wheels; the 0.2.0 release notes explain why
  its source distribution is only available as 0.2.1.

## [0.2.1] - 2026-11-17

### Fixed

- Republish the 0.2.0 contents for Python. PyPI rejected the 0.2.0 wheels and
  source distribution because their filenames were used by an earlier, deleted
  upload; 0.2.0 therefore exists only on crates.io. 0.2.1 publishes all four
  Rust crates, the Python distribution, CLI archives, and the GitHub Release
  together, with no code changes from 0.2.0.
- The release guide documents how to handle reused PyPI filenames.

## [0.2.0] - 2026-11-17

### Added

- Persistent indexes can store full signatures
  (`PersistentIndexOptions::store_signatures`) and answer
  `query_scored`/`query_scored_many` with estimated Jaccard similarity,
  `min_similarity`, and `top_k` filters. Signatures live in new optional
  `Signatures` sections that older readers skip, and the bounded external
  builder carries them through.
- Python `Index.create(..., store_signatures=True)` and `Index64` gain
  `search_scored`, `search_scored_many`, `signature`, and
  `stores_signatures`; `IndexStats.stores_signatures` reports the setting.
- `pari index --store-signatures` and `pari search --scores`,
  `--min-similarity`, and `--top-k` return aligned `scores` in JSONL output and
  `key:score` pairs in text output. `stats --json` adds `stores_signatures`.
- `deduplicate(..., min_similarity=...)`, `DedupeIndex(min_similarity=...)`,
  and `DedupeIndex.similarity` reject candidate pairs below an estimated
  similarity natively before any `exact` verifier runs.
- `pari dedup --min-similarity` filters pairs and groups the same way and adds
  each pair's `similarity` to pairs output.
- Experimental `pari.shingles(text, size=3, unit="word" | "char")` returns
  normalized word or character n-grams as bytes for use as a feature.

### Changed

- `pari_format::SectionKind` gains a `Signatures` variant, `StoreError` gains
  signature and score-filter variants, and `StoreStats` gains
  `stores_signatures`. These additions to public enums and structs are why this
  is a minor release.
- The docs site, README, and release documentation describe the 0.2 line.

### Fixed

- CLI commands exit successfully instead of panicking or reporting an error
  when a downstream reader such as `head` closes stdout early.
- Lazy readers validate key-catalog checksums, counts, and uniqueness and filter
  stale bucket members against live keys in scalar and batch queries.
- Optional known sections receive the same streamed checksum validation as
  optional unknown sections before they can be ignored.
- Snapshot converters reject duplicate keys and unsupported required tombstone
  semantics while preserving unordered, unique legacy key catalogs.
- External builds create missing destination directories and reserve spill
  buffers only for actual input, including empty inputs and large buffer limits.
- Explicit LSH configuration validation no longer constructs temporary hash
  tables, and clean persistent indexes allocate mutation tables on first use.
- Datasketch imports reject fractional, textual, boolean, and multidimensional
  metadata instead of silently coercing it; NumPy integer scalars and strided
  vectors retain exact update compatibility.
- `pari dedup --output` creates missing parent directories, matching
  `pari index`, and names the output path when it cannot be created.

## [0.1.3] - 2026-11-06

### Fixed

- Published README links target the matching tagged documentation, and release
  version badges use immutable version-specific images.
- Non-callable progress handlers are rejected before input inspection or batch
  ingestion, preserving existing memory and persistent index contents.
  The convenience helper also rejects non-callable feature/progress values
  before creating a local index.
- Dataset adapters reject non-integer and boolean batch sizes before importing
  optional libraries or touching a source.
- Explicit invalid backend values no longer silently select memory storage.
- Code-corpus verification rechecks cached source snapshots and refuses to
  publish outputs when a source changes during grouping.
- Checksummed benchmark evidence keeps its original bytes on Windows and Unix
  checkouts instead of being rewritten by line-ending conversion or fixers.

### Changed

- Correct package-author and copyright credits to Dipesh Tharu Mahato.
- Define patch/backport eligibility, protected maintenance branches, coordinated
  validation, publication, and retirement without a standing support branch.
- Add checksum-pinned public text, code, contamination, and labeled product
  workloads with exact, Datasketch, and SciPy correctness baselines.
- Record a validated four-track public-corpus run, including labeled candidate
  recall/reduction tradeoffs and explicit limits on performance conclusions.

## [0.1.2] - 2026-11-05

### Fixed

- Human-readable search output escapes control characters in query IDs while
  preserving their original values in JSON output.
- Reference workload metrics use exclusive file creation to preserve existing
  files and linked targets, and staged outputs must be regular files.
- Redis CI and benchmark services use the reviewed, digest-pinned security patch.
- Rust publication uploads fully verified prebuilt crate archives without running
  Cargo or package build scripts in the credentialed job.

- Unknown optional index sections have their checksums verified before the lazy
  reader skips them, preventing damaged extensions from passing `pari verify`.
- `pari verify` rejects containers with missing, duplicate, or malformed required
  index metadata even when their outer frames and checksums are valid.
- `pari dedup` rejects output paths that alias the input file, preserving source
  records for both pair and group output and both signature families.
- Parquet column projection is applied once, so nested fields such as
  `document.text` retain their nested row structure without a second-selection error.

### Changed

- Coordinate the alpha release across Rust, Python, and CLI.
- Update the maintained PyO3 patch release while preserving Rust 1.83 for the
  Python extension, and accept synchronized compatible patch updates in CI.

## [0.1.1] - 2026-11-03

Pari 0.1.1 alpha coordinates the Rust, Python, and CLI release. Supported
APIs and format-v1 data remain compatible.

### Added

- Pinned fast pre-commit checks cover repository text and syntax policy, both
  Rust formatting workspaces, and workflow dependency pins without moving
  expensive protected-CI gates into the normal commit path.
- Distinct batch-first Rust `LshIndex64` candidate generation over full-width
  affine64 signatures, including deterministic grouping and observability.
- Explicit Rust `PersistentIndex64` and `LazyIndex64` storage, canonical
  affine64 reference/external builds, and a pinned format-v1 fixture without
  changing affine32 APIs or bytes.
- Explicit CLI affine64 indexing and deduplication, full-width
  `MinHash64::from_signature` reconstruction, and metadata-derived search,
  inspection, explanation, and verification; affine32 remains the default and
  revision-1 machine-readable fields are unchanged.
- Explicit Python `MinHash64` and `Index64` types with full-width signature
  reconstruction, bounded batch construction, and affine64 local persistence;
  existing `MinHash` and `Index` remain affine32.
- Exact optional Datasketch 2.x affine64 import/export with full-width
  validation, installed-wheel persistence coverage, and a semantics-matched
  affine32/affine64 benchmark.
- Release Validation builds a manylinux2014 aarch64 Python wheel, executes it on native Linux arm64, and packages a natively smoke-tested Linux arm64 CLI archive for subsequent releases.
- An opt-in backend conformance kit lets third-party `StorageBackend` implementations reuse Pari's atomicity, query-parity, lifecycle, capability, and statistics contract tests without enabling Redis.

### Fixed

- Python MinHash inputs hash raw buffer bytes consistently across scalar and
  batch APIs, preserving multi-byte element widths and strided view contents.
- Benchmark timeouts and interruptions stop descendant processes before
  cleaning staging or finalizing failure evidence.
- Direct Redis batch deletion counts each live key once, even when the batch
  repeats keys.
- Python index handles retain unsaved changes when `close()` fails, allowing
  storage errors to be resolved and the commit retried without data loss.
- Persistent indexes retain query observation and accumulated counters across
  flush, sync, and commit retries.
- Redis's URL dependency now uses patched IDNA while retaining Rust 1.81;
  dependency audits include optional features.
- All non-Python Rust workspace crates and the Criterion harness now compile
  under Rust 1.81; the secure PyO3 binding declares and tests Rust 1.83.
- Initial persistent-index creation now atomically refuses occupied or
  concurrently created destinations instead of replacing them.
- Persistent commits now use uniquely owned temporary files instead of
  following or truncating a predictable sibling path.
- Benchmark campaign stages now enforce each profile's declared timeout and
  preserve the exact limit and partial logs in timeout failure evidence.
- Backend retention rejects values that cannot round-trip through whole-second
  Redis metadata instead of silently truncating or disabling TTL.
- Lazy index publication preserves destinations created concurrently after conversion begins.
- Bounded external index publication now atomically rejects existing destinations and rolls back transaction-owned outputs after cleanup or durability failures.
- `pari index` publishes its destination only after successful ingestion and no longer leaves a valid-looking partial index after input failure.
- Failed `scale-10m` campaigns preserve checksummed logs, reports, environment telemetry, and exact failure metadata in a clearly non-publishable diagnostic artifact after verified temporary-file cleanup, including interruptions, partial bundle writes, fail-closed wrapper creation, canonical bundle validation, and post-validation publication failures.

### Repository policy

- Dependabot suppresses Redis 1.x and SHA-1 0.11+ while they exceed the Rust
  1.81 MSRV, but continues proposing compatible patch updates.
- High-signal Ruff anti-pattern/security rules and selected Clippy restriction lints now reject unsafe exception flow, avoidable performance traps, loop-variable overwrites, and Rust debug/placeholder macros without enabling noisy rule groups wholesale.
- Rust remains formatted by the pinned canonical `rustfmt`; maintained Python and stub files now use pinned Ruff 0.16.7 with explicit stable formatting/lint rules and protected benchmark-fixture exclusions.
- During coordinated releases, dependent crate manifests and file lists are checked before publication; the publish job builds and registry-verifies all four tarballs in dependency order. The complete local Rust 1.81 graph remains enforced.

## [0.1.0] - 2026-08-28

First public alpha of the Pari similarity and deduplication engine.

### Added

- Safe Rust `MinHash32` and `MinHash64` affine signature implementations.
- Batch-first MinHash LSH indexing with deterministic candidate results.
- Native candidate-pair and connected-component duplicate grouping.
- Safe, versioned `.pari` persistence with checksums and no executable deserialization.
- Crash-safe local persistence, lazy bucket reads, mutation overlays, and bounded-memory external index construction.
- Reproducible Criterion and end-to-end benchmark tooling, including storage benchmarks and datasketch comparison support.
- Typed Python package exposing `pari.Index` and `pari.MinHash` through PyO3 and abi3 wheels.
- Streaming `pari` CLI for indexing, search, deduplication, statistics, verification, and shell completion.
- Typed backend contract with in-process memory and Redis implementations, batching, TTL, namespace isolation, and integration CI.
- Explicit v0.x Python, Rust, CLI, backend, signature, and on-disk compatibility policy.

### Release status

This is an alpha release. Public 0.x compatibility is governed by `docs/compatibility.md`; interfaces marked experimental or internal may change before 1.0.

[Unreleased]: https://github.com/dipeshbabu/pari/compare/v0.2.2...HEAD
[0.2.2]: https://github.com/dipeshbabu/pari/compare/v0.2.1...v0.2.2
[0.2.1]: https://github.com/dipeshbabu/pari/compare/v0.2.0...v0.2.1
[0.2.0]: https://github.com/dipeshbabu/pari/compare/v0.1.3...v0.2.0
[0.1.3]: https://github.com/dipeshbabu/pari/compare/v0.1.2...v0.1.3
[0.1.2]: https://github.com/dipeshbabu/pari/compare/v0.1.1...v0.1.2
[0.1.1]: https://github.com/dipeshbabu/pari/compare/v0.1.0...v0.1.1
[0.1.0]: https://github.com/dipeshbabu/pari/releases/tag/v0.1.0
