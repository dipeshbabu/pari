# Architecture

Pari is a similarity engine, not a collection of unrelated sketch implementations. The architecture is organized around a small native core that can serve Rust, Python, and command-line users.

## Layers

### `pari-core`

Owns hashing, signatures, similarity estimation, and compatibility metadata. It has no Python dependency and should remain usable as a standalone Rust library.

### `pari-index`

Owns approximate candidate generation such as MinHash LSH. Batch insertion and batch query are the primary implementation paths. Scalar APIs are convenience wrappers.

### `pari-store`

Owns persistence and backend capabilities. Index algorithms must not hard-code Redis, local-file, or serialization behavior.

### `pari-py`

Thin PyO3 bindings plus a small Python usability layer. CPU-heavy work runs in Rust and releases the GIL when it does not need Python objects.

### `pari-cli`

Operational interface for indexing, search, deduplication, statistics, and validation. It calls the same Rust APIs as the bindings.

## Core principles

1. **Safe persistence.** Stored indexes are explicit and versioned; no executable object deserialization is allowed in core storage paths.
2. **Batch first.** Data movement and storage round trips are amortized across batches whenever possible.
3. **Measure before optimizing.** Rust, SIMD, parallelism, mmap, and GPU work must be justified by end-to-end benchmarks rather than language-level assumptions.
4. **Scale without API rewrites.** Users should be able to move from memory to local persistence to a remote backend while retaining the same indexing model.
5. **Separate candidate generation from verification.** Pari finds likely matches cheaply; exact application-specific verification remains optional and pluggable.
6. **Stable semantics.** Signature/index compatibility is explicit and validated before operations that would otherwise silently return invalid similarity results.

## Upstream provenance

The initial MinHash design work is informed by `ekzhu/datasketch`, particularly its version 2 affine permutation schemes. Pari intentionally does not inherit legacy pickle-based storage or historical compatibility modes unless a migration use case later justifies a separately scoped compatibility layer. See `NOTICE`.
