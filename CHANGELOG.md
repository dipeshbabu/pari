# Changelog

All notable changes to Pari are documented here.

The project follows Semantic Versioning. During the 0.x series, compatibility guarantees are intentionally narrower than 1.x and are defined in `docs/compatibility.md`.

## [Unreleased]

### Added

- Release Validation builds a manylinux2014 aarch64 Python wheel, executes it on native Linux arm64, and packages a natively smoke-tested Linux arm64 CLI archive for subsequent releases.

### Fixed

- Lazy index publication preserves destinations created concurrently after conversion begins.

### Repository policy

- Full registry-backed package verification is restored for all four public crates after the coordinated 0.2.0 publication.

## [0.2.0] - 2026-08-31

### Added

- Release automation and packaging hardening for future alpha releases.
- High-level typed Python deduplication through `DedupeIndex` and `deduplicate`, with exact verification, representative selection, bounded batches, local persistence, and cancellation-aware progress.
- Streaming text reference-build, deduplication, and cross-corpus audit workflows with exact verification and transactional outputs.
- Datasketch 2.x affine32 signature interoperability, golden fixtures, migration adapters, and a semantics-matched comparison harness.
- Deterministic, bounded CPU parallelism for batch MinHash construction in Rust and Python.
- Opt-in query metrics, exact bucket diagnostics, and batch-granular CLI/Python progress reporting.
- Deterministic LSH planning and existing-index explanation across Rust, CLI, and Python.
- Streaming source-code corpus deduplication with lexical features, exact verification, and reproducible metrics.
- Structured customer/product candidate generation with labeled recall and reduction evaluation.
- Optional bounded-batch adapters for PyArrow/Parquet, Polars, and Hugging Face Datasets.
- Evidence-based defer decision for Weighted MinHash and SimHash future families.
- Evidence-based defer decision and deterministic fan-out prototype for sharded indexes.
- Refreshed native-Linux 100K and 1M campaign evidence with persistent storage, text-audit, and Datasketch stages.

### Fixed

- Reference deduplication and matching workloads no longer retain partial final outputs after failure.
- External and lazy storage builders no longer report false parent-directory sync failures on Windows.
- Rust storage tests now use parallel-safe temporary filenames that are valid on Windows.
- PyArrow dataset adapters now accept preconfigured `Scanner` inputs without invalid batch arguments.
- Reference workload publication now claims final paths atomically without replacing concurrent files.
- Benchmark campaign commands select the main harness explicitly after the package gained an evaluation binary.

### Repository policy

- GitHub now enforces protected `main`, immutable `v*` release tags, squash-only merges, exact-head checks, and automatic merged-branch deletion.
- CI continuously enforces the declared Rust 1.81 minimum for every published crate and package.
- Release Validation derives and checks the complete Python source-distribution manifest, including forbidden-file rejection.
- Active CI actions and Redis service images are immutable and checked automatically.
- Private vulnerability reporting, dependency proposals, and advisory/license/source CI policy are enabled.
- Repository metadata, focused issue forms, and contributor evidence guidance provide consistent public entry points.

### Compatibility

- Existing supported 0.1 Python, Rust, CLI, signature, and `.pari` format-v1 behavior remains compatible in 0.2.0.
- CLI machine-readable output remains revision 1; new fields are additive, while the new `plan` and `explain` output remains experimental.
- The four public Rust crates remain on Rust 1.81 and move together to exact version 0.2.0 dependencies.
- Planner models, optional dataset and Datasketch adapters, Redis layout, observability measurement policy beyond supported fields, and direct builder/lazy-store APIs remain experimental.

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

[Unreleased]: https://github.com/dipeshbabu/pari/compare/v0.2.0...HEAD
[0.2.0]: https://github.com/dipeshbabu/pari/compare/v0.1.0...v0.2.0
[0.1.0]: https://github.com/dipeshbabu/pari/releases/tag/v0.1.0
