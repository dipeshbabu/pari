# Pari v0.x compatibility contract

Pari is still pre-1.0, but pre-1.0 does not mean that every public surface may change without warning. This document defines the compatibility promises that begin with the 0.1 release line.

The binary `.pari` format has its own versioning rules and is not implicitly coupled to the package version.

## Stability levels

Pari classifies interfaces into three levels.

### Supported for v0.1

These interfaces are intended for normal users. Patch releases in the 0.1.x line must not intentionally break them.

Python:

- the `pari` import package
- `MinHash`
- `Index`
- `IndexStats`
- `DedupeIndex`
- `DuplicateGroup`
- `DeduplicationResult`
- `deduplicate`
- `PariError`
- `DedupeError`
- `InvalidRepresentativeError`
- `ConfigurationError`
- `CompatibilityError`
- `DuplicateKeyError`
- `StorageError`
- `ClosedIndexError`
- `__version__`

The exact top-level Python export set is pinned by installed-wheel tests.

Rust:

- `pari_core::MinHash32`
- `pari_core::MinHash64`
- the named `pari-affine32-v1` and `pari-affine64-v1` signature schemes
- `pari_index::LshIndex32` candidate semantics
- `pari_store::PersistentIndex32` local persistence behavior
- the version-1 `.pari` container reader and writer

CLI:

- the `index`, `search`, `dedup`, `stats`, `verify`, and `completion` command names
- documented command options in the 0.1.x line
- documented JSONL input fields in the 0.1.x line
- machine-readable JSON/JSONL output revision 1, pinned by compiled CLI integration tests

### Experimental

Experimental interfaces are usable, tested, and documented, but may change at a 0.x minor release when evidence from real users requires a better contract.

- `pari_backend::StorageBackend`
- custom backend extension points
- optional `pari.datasketch` interoperability adapters
- Redis namespace layout and descriptor bytes
- advanced low-level LSH parameter APIs
- direct use of `pari-store-lazy` and `pari-store-build`

A 0.1.x patch release should still avoid unnecessary breakage to experimental APIs. If a security or correctness fix requires a break, the release notes must state it explicitly.

### Internal tooling

These are not compatibility surfaces.

- `pari-bench` report implementation details beyond its documented schema
- Criterion benchmark package internals
- temporary files used while building or committing indexes
- private bucket, merge, and scratch structures
- human-readable CLI wording and whitespace

## Package versioning

Pari follows semantic-versioning intent while it is pre-1.0.

For `0.1.x` patch releases:

- no intentional breaking changes to supported Python, Rust, or CLI interfaces
- no incompatible reinterpretation of existing signature schemes
- no incompatible change to machine-readable CLI output revision 1
- no incompatible change to `.pari` format version 1
- bug fixes may reject data that was previously accepted only because validation was incorrect or unsafe

For a future `0.y.0` minor release:

- supported APIs may change only with release notes and a migration section
- deprecated supported APIs should normally remain available for at least one minor release when keeping them is safe and practical
- machine-readable output that removes fields, changes field types, or changes field meaning requires a new documented output revision
- persisted data must never be silently reinterpreted under old format or signature identifiers

## Python surface

`pari.__all__` is the supported top-level import contract for the 0.1 line. Adding a new top-level name is backward compatible. Removing or renaming an existing name is not.

Exception classes are part of the supported API. Callers may catch the documented classes and must not need to parse Rust error strings.

Type hints describe the supported calling contract. A change that makes a previously valid typed call invalid is treated as an API break even if Python could still call the underlying extension dynamically.

## Signature compatibility

The identifiers `pari-affine32-v1` and `pari-affine64-v1` describe stable signature semantics, including seed interpretation and value width.

An implementation change must use a new scheme identifier if it changes the signature values produced for the same input, seed, and permutation count. Existing identifiers must never be reused for new semantics.

Sketch comparison, merge, indexing, and persisted-index operations must continue to reject incompatible seeds, permutation counts, widths, or schemes rather than returning misleading similarity results.

## `.pari` file compatibility

Format version 1 is specified in [`index-format.md`](index-format.md). Its checked-in golden fixture is an executable compatibility contract: CI verifies that the current writer reproduces the expected bytes and that the current reader accepts them.

Rules:

- compatible extensions should use optional sections
- semantics required for correct interpretation must use a required section, required feature flag, or a new format version
- a new package release must not silently reinterpret an existing format version
- corrupt, truncated, unsupported, or security-sensitive data fails closed
- dropping read support for a published format requires an explicit migration plan and release note; it is not an ordinary patch-level change

## CLI machine-readable output

The JSON and JSONL field sets emitted by 0.1 `--json` commands define machine-readable output revision 1. The current payloads do not embed a separate `schema_version` field; the CLI package version identifies the producer version, while compiled integration tests pin the revision-1 field sets.

For revision 1:

- existing fields keep their meaning and JSON type
- new output fields may be added when they are backward compatible
- consumers should ignore unknown output fields
- removing or renaming a field requires a new documented output revision
- changing a field from scalar to collection, or vice versa, requires a new documented output revision

Adding an explicit schema-version field in a future release is itself backward compatible because revision-1 consumers are required to ignore unknown output fields.

Human-readable output is intentionally not a parser contract.

The 0.1 JSONL input records remain strict: unknown input fields are rejected so misspellings fail early. Producers targeting 0.1.x should use only the documented fields in [`cli.md`](cli.md).

## Redis compatibility

Redis is a shared runtime backend, not an archival persistence format. Pari owns the documented namespace keys and applications must not mutate those keys directly.

The Redis descriptor and namespace layout are experimental in the 0.1 line. Cross-version Redis reuse is supported only when the reader validates the stored descriptor as compatible. Long-term archival data should use the versioned `.pari` format instead.

## Deprecation and security exceptions

When a supported API must change, Pari should prefer this sequence:

1. document the replacement
2. mark the old path deprecated where the language surface supports it
3. keep the old path for at least one minor release when safe and practical
4. remove it in a later minor release with migration notes

Security, data-corruption, or fundamentally incorrect behavior may require a faster break. Such releases must state the reason clearly and must fail safely rather than preserve unsafe behavior for compatibility.
