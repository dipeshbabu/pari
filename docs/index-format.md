# Pari index format v1

Pari uses an explicit binary container instead of serializing Rust or Python objects. Version 1 is little-endian and starts with a fixed 72-byte header followed by framed sections.

Package-level promises for retaining and migrating published format versions are defined in [compatibility.md](compatibility.md). This document specifies the bytes and interpretation of format version 1 itself.

## Header

| Offset | Size | Field |
| ---: | ---: | --- |
| 0 | 8 | magic `PARIIDX\0` |
| 8 | 2 | format version, currently `1` |
| 10 | 2 | header size, currently `72` |
| 12 | 2 | algorithm ID |
| 14 | 2 | signature scheme ID |
| 16 | 2 | signature value width in bits |
| 18 | 2 | key codec ID |
| 20 | 4 | permutation count |
| 24 | 4 | LSH band count |
| 28 | 4 | rows per band |
| 32 | 4 | section count |
| 36 | 4 | reserved, must be zero |
| 40 | 8 | signature seed |
| 48 | 8 | target similarity threshold as IEEE-754 `f64` |
| 56 | 8 | required feature flags |
| 64 | 4 | CRC32 of bytes 0 through 63 |
| 68 | 4 | reserved, must be zero |

The signature scheme and explicit width must agree. Version 1 currently defines no required feature bits, so a nonzero feature flag fails closed.

## Section framing

Each section starts with a 16-byte frame:

| Offset | Size | Field |
| ---: | ---: | --- |
| 0 | 2 | section kind |
| 2 | 2 | section flags; bit 0 means required |
| 4 | 8 | payload length |
| 12 | 4 | payload CRC32 |

Version 1 recognizes sections for keys, per-item band hashes, buckets, and tombstones. Readers skip unknown optional sections after validating their length and checksum. An unknown required section is an error so a reader never silently ignores semantics it needs to understand.

The in-memory decoder bounds a single section to 256 MiB and the section table to 1024 entries. Persistent storage can split large logical data into multiple framed chunks or stream the same framing without loading an entire index into RAM.

## Key codecs

The format records a stable key codec ID. Built-in codecs are:

1. bytes
2. UTF-8 string
3. unsigned 64-bit integer
4. signed 64-bit integer
5. JSON value

Integer codecs use exactly eight little-endian bytes. JSON is data-only `serde_json::Value`; it does not instantiate executable language objects. Variable-length in-memory codec payloads are bounded to 16 MiB.

## Compatibility policy

Pari v1 stores the signature scheme separately from the LSH algorithm. This is intentional: `pari-affine32-v1` and `pari-affine64-v1` have stable seed semantics, and persisted indexes must not silently reinterpret their signatures after an implementation change.

A future incompatible binary layout uses a new format version. Compatible extensions should prefer optional sections. A feature that changes required interpretation must use a required section, a required feature flag, or a new version.

Published package releases that read format v1 must not silently change these semantics. Dropping support for a published format requires the migration and release-note process defined in [compatibility.md](compatibility.md).

## Golden fixture

`crates/pari-format/testdata/index_v1.bin` is a checked-in affine32 golden fixture containing:

- MinHash LSH
- `pari-affine32-v1`
- `u64` key codec
- 128 permutations, seed 42
- threshold 0.8
- 32 bands × 4 rows
- one required key section containing key `7`

CI verifies both that the encoder produces the exact fixture bytes and that the decoder reads it back. This pins cross-language behavior for future Python, CLI, or alternate implementations.

The local-store compatibility suite additionally pins empty canonical snapshots
for both named signature families:

```text
crates/pari-store/tests/fixtures/v1-empty.hex
crates/pari-store/tests/fixtures/v1-empty-affine64.hex
```

The files differ only where the signature scheme, explicit width, and resulting
header checksum require it. CI reproduces and opens both byte sequences and
verifies that the affine32 and affine64 reader types reject the other width.
