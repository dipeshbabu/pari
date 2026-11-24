# Pari CLI

The `pari` binary is a thin operational layer over the same Rust APIs used by the Python package. It does not reimplement MinHash, LSH, persistence, or duplicate grouping.

## Install a published binary

Pari 0.4.4 CLI archives are published to the [GitHub Release](https://github.com/dipeshbabu/pari/releases/tag/v0.4.4) after the tagged workflow succeeds:

- [Linux x86_64](https://github.com/dipeshbabu/pari/releases/download/v0.4.4/pari-0.4.4-linux.tar.gz)
- [Linux arm64](https://github.com/dipeshbabu/pari/releases/download/v0.4.4/pari-0.4.4-linux-arm64.tar.gz)
- [macOS arm64](https://github.com/dipeshbabu/pari/releases/download/v0.4.4/pari-0.4.4-macos.tar.gz)
- [Windows x86_64](https://github.com/dipeshbabu/pari/releases/download/v0.4.4/pari-0.4.4-windows.zip)

Each archive contains `LICENSE`, `NOTICE`, `README.md`, and the `pari` executable (`pari.exe` on Windows). Verify the archive against the release's [`SHA256SUMS`](https://github.com/dipeshbabu/pari/releases/download/v0.4.4/SHA256SUMS), extract it, and move the executable to a directory on `PATH` if desired.

## Build from source for contributors

```bash
cargo build --release -p pari-cli
```

The binary is written to `target/release/pari` (`pari.exe` on Windows).

## Compatibility

The supported command names, documented input fields, and machine-readable output field sets are interfaces for the 0.2.x release line; 0.1 inputs and outputs remain valid. Human-readable wording and whitespace are not parser contracts. See [compatibility.md](compatibility.md) for the full versioning and deprecation policy.

Machine-readable output revision 1 is pinned by compiled CLI integration tests. The current JSON/JSONL field sets are:

- `index --json`: `items`, `file_bytes`, `bands`, `rows`
- `search --json`: `query`, optional `id`, `candidates`, and `scores` only with `--scores`, `--min-similarity`, or `--top-k`
- `dedup --emit pairs --json`: `left`, `right`, and `similarity` only with `--min-similarity`
- `dedup --emit groups --json`: `representative`, `members`
- `stats --json`: `items`, `file_bytes`, `dirty`, `bands`, `rows`, `committed_buckets`, `overlay_buckets`, `suppressed_base_keys`, `delta_generations`, `delta_bytes`, `committed_bucket_distribution`, `overlay_bucket_distribution`, `query_metrics`, `num_perm`, `seed`, `threshold`, `stores_signatures`
- `verify --json`: `valid`, `sections`, `bucket_sections`, `buckets`, `members_checked`, `delta_generations`

The experimental `plan --json` and `explain --json` commands share the model-labeled field set documented in [planning.md](planning.md). Compiled tests pin their current shape, but the planner model may be versioned at a future 0.x minor release as workload evidence improves.

Revision-1 consumers should ignore unknown output fields so future releases can add information without breaking existing parsers. Removing, renaming, retyping, or changing the meaning of an existing field requires a new documented output revision.

## JSONL record format

`index` and `dedup` consume one record per line. A record may contain raw values:

```json
{"key":1,"values":["new york","rust","search"]}
```

or a precomputed Pari signature:

```json
{"key":2,"signature":[125,992,481,73],"scheme":"pari-affine32-v1"}
```

Affine32 remains the default: each raw value is fed to `MinHash32` as UTF-8
bytes, and existing commands need no new option. Select the explicit 64-bit
family for a new index or in-memory deduplication job with
`--signature-scheme pari-affine64-v1`. Raw values then construct `MinHash64`.

Precomputed affine64 signatures retain full JSON unsigned 64-bit values,
including values above `4294967295`, and must declare the matching scheme:

```json
{"key":3,"signature":[17179869191,18446744073709551557],"scheme":"pari-affine64-v1","seed":7}
```

A precomputed signature must contain exactly the configured `--num-perm`
values and declare the selected stable scheme. The optional `seed` field records
the producer seed; when present, Pari rejects it unless it matches the command
or opened index seed. Producers should include it when that metadata is
available. Affine32 values must fit `u32`. The CLI rejects missing or mismatched
schemes, seed mismatches, incorrect counts or widths, and records containing
both `values` and `signature`, with the input line number in the diagnostic.

Input is processed line by line. Raw JSONL records are never accumulated as a complete corpus.

`search` accepts the same two sketch forms and an optional query ID:

```json
{"id":"query-1","values":["new york","rust","search"]}
```

or:

```json
{"id":"query-2","signature":[125,992,481,73],"scheme":"pari-affine32-v1"}
```

`id` is copied to JSON output for correlation. `search` derives the signature
family, permutation count, and seed from validated index metadata; it does not
accept a second family option. Precomputed queries must declare the matching
scheme and count. A supplied `seed` must match the opened index. `stats`,
`explain`, and `verify` likewise validate and derive the family from the file,
so an affine32 index is never opened as affine64 or vice versa.

Human-readable search output escapes control characters in query IDs so each
result stays on one line and IDs cannot introduce terminal control sequences.
JSON output preserves the original ID value.

Use `-` as the input or output path for stdin/stdout.

## Build a persistent index

```bash
pari index \
  --input documents.jsonl \
  --output documents.pari \
  --threshold 0.8 \
  --num-perm 128 \
  --seed 7 \
  --batch-size 10000 \
  --json
```

To build the explicit affine64 equivalent, add the family option:

```bash
pari index \
  --input documents.jsonl \
  --output documents-64.pari \
  --signature-scheme pari-affine64-v1 \
  --threshold 0.8 \
  --num-perm 128 \
  --seed 7 \
  --json
```

The command inserts records into a sibling staging index in bounded input batches and commits between batches. After the complete input succeeds, Pari atomically claims the requested destination without replacement and removes the staging file. Invalid or unreadable input therefore leaves no partial index at the final path, and existing or concurrently created destinations are never overwritten implicitly.

Add `--progress` for human batch updates on stderr, or `--progress json` for structured schema-1 events. Machine-readable summaries remain on stdout.

## Search

```bash
pari search \
  --index documents.pari \
  --input queries.jsonl \
  --json
```

Example result:

```json
{"query":0,"id":"query-1","candidates":[1,2]}
```

Candidates are approximate LSH matches, sorted by key.

### Similarity scores

Build the index with `--store-signatures` to rank candidates by estimated
Jaccard similarity:

```bash
pari index --input documents.jsonl --output documents.pari --store-signatures
pari search --index documents.pari --input queries.jsonl --min-similarity 0.8 --top-k 10 --json
```

```json
{"query":0,"id":"query-1","candidates":[2,1],"scores":[1.0,0.875]}
```

`--scores` adds a `scores` array aligned with `candidates` and orders both from
most to least similar, with ties broken by key. `--min-similarity` (in `[0, 1]`)
and `--top-k` filter the scored list and imply `--scores`. Text output prints
`key:score` pairs with four decimal places. Scores are the fraction of equal
signature values, the standard MinHash estimate, so they are still estimates;
verify against source records when an exact threshold matters.

Stored signatures add `num_perm × 4` bytes per item for affine32 (`× 8` for
affine64). Searching an index built without them with any scoring flag exits
with status 2. `stats` reports `stores_signatures`.

Search progress is emitted every 1,000 queries by default. Use `--progress-every N` to change the interval. Final JSON progress includes the exact process-local candidate count and rate.

Use `--read-only` to search a committed index without loading the per-key
band hashes used for updates. It supports raw values, precomputed signatures,
score filters, and the same text and JSONL output:

```bash
pari search --index documents.pari --input queries.jsonl --read-only --json
```

Snapshots from older writers that lack bucket sections need a one-time sync
through the writable Python or Rust index before using this flag.

## Deduplicate

When writing to a file, `--output` must refer to a different file from `--input`.
Pari rejects the same path and aliases such as hard links or symlinks so the
source records are preserved.

Emit unique LSH candidate pairs:

```bash
pari dedup --input documents.jsonl --emit pairs --json
```

Or native connected components:

```bash
pari dedup \
  --input documents.jsonl \
  --emit groups \
  --min-size 2 \
  --batch-size 10000 \
  --json
```

Example group:

```json
{"representative":1,"members":[1,2]}
```

Add `--min-similarity X` (in `[0, 1]`) to drop LSH candidate pairs whose
estimated similarity is below `X`. Groups then join only through pairs that
pass, and pairs output adds each pair's `similarity` (a third text column, or a
`similarity` JSON field). The CLI keeps every record's signature in memory
while this flag is set.

```bash
pari dedup --input documents.jsonl --emit pairs --min-similarity 0.9 --json
```

```json
{"left":1,"right":2,"similarity":0.9453125}
```

The CLI streams JSONL records into the selected concrete `LshIndex32` or
`LshIndex64`; it does not retain the original corpus. Add
`--signature-scheme pari-affine64-v1` to either deduplication form for the
64-bit family. Group construction uses Pari's native union-find path from
`pari-index`.

### Deduplicate inputs larger than memory

The in-memory index needs memory for every band of every record, and
`--min-similarity` adds every signature. For larger inputs, pass
`--spill-dir DIR` to group in bounded memory instead:

```bash
pari dedup --input documents.jsonl --spill-dir /scratch/pari --json
```

Each record's band hashes are sorted in runs of `--max-buffer-records`
(default 4,194,304, about 64 MiB) and spilled to `DIR`. The runs are then
merged so each bucket's members arrive together and are unioned, after the
`--min-similarity` check when it is set. Signatures for that check stay in a
file in `DIR` and are read only for buckets whose members are not yet
connected. Memory holds the keys and a union-find, about 16 bytes per record,
plus one sort buffer. The groups are identical to the in-memory path, and
every temporary file is removed when the command ends.

`--spill-dir` supports `--emit groups` only: removing duplicate pairs across
bands needs either every pair in memory or more passes over the disk.

Measured locally on 1 million records with `--threshold 0.5`:

| | time | peak memory |
|---|---|---|
| in memory | 13.6 s | 1,325 MB |
| `--spill-dir` | 13.5 s | 124 MB |
| in memory, `--min-similarity 0.7` | 13.7 s | 2,390 MB |
| `--spill-dir`, `--min-similarity 0.7` | 21.8 s | 124 MB |

## Inspect and verify

Plan settings before a build, or explain the configuration of an existing index:

```bash
pari plan --items 1000000 --memory-budget-mib 2048 --storage auto --json
pari explain --index documents.pari --json
```

Planning is deterministic and does not read a corpus. Explanation reads persisted configuration and item-count metadata without scanning bucket memberships. All estimates are labeled analytical/model-based; see [LSH planning and explanation](planning.md) for formulas, calibration, policy, and limitations.

Inspect exact current storage state or verify all persisted data:

```bash
pari stats --index documents.pari --json
pari verify --index documents.pari --json
```

`verify` validates the container header, every outer section checksum, bucket directory invariants, global bucket ordering, and each bucket member checksum, for the snapshot and for every appended delta generation, and checks each generation's key changes. Its counts cover all generations: a bucket stored by several generations counts once per generation. A corrupt or unsupported index returns a nonzero exit code and an actionable error on stderr.

`index` commits once per `--batch-size` records. Later batches append delta generations to the file instead of rewriting it, and the file is rewritten as one snapshot whenever the generations grow past a quarter of it; see [persistence](persistence.md#delta-commits-and-compaction). Rewrite an index as one snapshot, for example before handing it to Pari 0.3 or earlier:

```bash
pari compact --index documents.pari
```

Write a verified, query-ready copy of an index without loading it into memory:

```bash
pari build-read-only --index documents.pari --output documents-ro.pari --json
```

`build-read-only` reads the source's key catalog and band hashes and writes the canonical snapshot format that `search --read-only`, `ReadOnlyIndex`, and the writers open. Memory stays bounded: records are sorted in runs of `--max-buffer-records` (default 262,144) and merged from disk. Use it to publish a copy for query-only processes while a writer keeps committing to the source, or to add bucket sections to a snapshot written by an older Pari without opening it in memory. If the source has appended commits, they are folded in while reading: the output is byte-for-byte what `pari compact` followed by `build-read-only` would write, but the source is left unchanged and never loaded into memory. The destination must not exist and is never replaced.

When the source is a compacted snapshot, its bucket sections are already what the build would write. The command checks that, comparing checksums, ordering, segment boundaries, and a randomly keyed hash of every bucket entry against the band hashes, then copies them instead of re-sorting. That is about 3.5x faster at 10 million items. If any check fails, the buckets are rebuilt, so the output is the same either way. Pass `--no-reuse-buckets` to always rebuild them. The summary reports `records`, `buckets`, `output_bytes`, `spill_runs`, `reused_source_buckets`, and `folded_generations`.

`stats` computes exact committed and overlay bucket distributions on demand. Query metrics are process-local and therefore `null` in a fresh `stats` process. Applications that keep an index open can enable query observation through the Rust or Python API. See [observability](observability.md).

`index`, `search`, `dedup`, and `verify` accept `--progress [human|json]`. All progress is written to stderr so stdout JSON and JSONL contracts remain clean.

## Shell completion

Completions are generated from the same Clap command definition as runtime parsing:

```bash
pari completion bash > pari.bash
pari completion zsh > _pari
pari completion fish > pari.fish
```

## Exit behavior

Successful commands return exit code `0`. Parsing, validation, I/O, compatibility, and storage failures return a nonzero exit code and print a single `pari: ...` diagnostic to stderr.
