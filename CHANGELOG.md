# Changelog

All notable changes to Pari are documented here.

The project follows Semantic Versioning. During the 0.x series, compatibility guarantees are intentionally narrower than 1.x and are defined in `docs/compatibility.md`.

## [Unreleased]

### Added

- Release automation and packaging hardening for future alpha releases.
- Deterministic, bounded CPU parallelism for batch MinHash construction in Rust and Python.
- Opt-in query metrics, exact bucket diagnostics, and batch-granular CLI/Python progress reporting.
- Deterministic LSH planning and existing-index explanation across Rust, CLI, and Python.
- Streaming source-code corpus deduplication with lexical features, exact verification, and reproducible metrics.
- Structured customer/product candidate generation with labeled recall and reduction evaluation.

## [0.1.0] - 2026-08-27

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

[Unreleased]: https://github.com/dipeshbabu/pari/compare/v0.1.0...HEAD
[0.1.0]: https://github.com/dipeshbabu/pari/releases/tag/v0.1.0
