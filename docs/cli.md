# Pari CLI

The `pari` binary is a thin operational layer over the same Rust APIs used by the Python package. It does not reimplement MinHash, LSH, persistence, or duplicate grouping.

## Install a published binary

Pari 0.1.0 CLI archives are attached to the [GitHub Release](https://github.com/dipeshbabu/pari/releases/tag/v0.1.0):

- [Linux x86_64](https://github.com/dipeshbabu/pari/releases/download/v0.1.0/pari-0.1.0-linux.tar.gz)
- [macOS arm64](https://github.com/dipeshbabu/pari/releases/download/v0.1.0/pari-0.1.0-macos.tar.gz)
- [Windows x86_64](https://github.com/dipeshbabu/pari/releases/download/v0.1.0/pari-0.1.0-windows.zip)

Each archive contains `LICENSE`, `NOTICE`, `README.md`, and the `pari` executable (`pari.exe` on Windows). Verify the archive against the release's [`SHA256SUMS`](https://github.com/dipeshbabu/pari/releases/download/v0.1.0/SHA256SUMS), extract it, and move the executable to a directory on `PATH` if desired.

## Build from source for contributors

```bash
cargo build --release -p pari-cli
```

The binary is written to `target/release/pari` (`pari.exe` on Windows).

## Compatibility

The command names, documented 0.1 input fields, and machine-readable output field sets are supported interfaces for the 0.1.x release line. Human-readable wording and whitespace are not parser contracts. See [compatibility.md](compatibility.md) for the full versioning and deprecation policy.

Machine-readable output revision 1 is pinned by compiled CLI integration tests. The current JSON/JSONL field sets are:

- `index --json`: `items`, `file_bytes`, `bands`, `rows`
- `search --json`: `query`, optional `id`, `candidates`
- `dedup --emit pairs --json`: `left`, `right`
- `dedup --emit groups --json`: `representative`, `members`
- `stats --json`: `items`, `file_bytes`, `dirty`, `bands`, `rows`, `committed_buckets`, `overlay_buckets`, `suppressed_base_keys`, `committed_bucket_distribution`, `overlay_bucket_distribution`, `query_metrics`, `num_perm`, `seed`, `threshold`
- `verify --json`: `valid`, `sections`, `bucket_sections`, `buckets`, `members_checked`

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

Each raw value is fed to Pari's `MinHash32` as UTF-8 bytes. A precomputed signature must contain exactly the configured `--num-perm` values and must explicitly declare `pari-affine32-v1`; the CLI rejects unknown schemes, missing scheme metadata, incorrect widths, and records containing both `values` and `signature`.

Input is processed line by line. Raw JSONL records are never accumulated as a complete corpus.

`search` accepts the same two sketch forms and an optional query ID:

```json
{"id":"query-1","values":["new york","rust","search"]}
```

or:

```json
{"id":"query-2","signature":[125,992,481,73],"scheme":"pari-affine32-v1"}
```

`id` is copied to JSON output for correlation. Precomputed query signatures must match the opened index's permutation count and use the index seed's affine32 permutation family.

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

The command inserts records in bounded input batches and commits between batches. Existing destination files are never overwritten implicitly.

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

Candidates are approximate LSH matches. Application-level exact verification remains the caller's responsibility.

Search progress is emitted every 1,000 queries by default. Use `--progress-every N` to change the interval. Final JSON progress includes the exact process-local candidate count and rate.

## Deduplicate

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

The CLI streams JSONL records into `LshIndex32`; it does not retain the original corpus. Group construction uses Pari's native union-find path from `pari-index`.

## Inspect and verify

```bash
pari stats --index documents.pari --json
pari verify --index documents.pari --json
```

`verify` validates the container header, every outer section checksum, bucket directory invariants, global bucket ordering, and each bucket member checksum. A corrupt or unsupported index returns a nonzero exit code and an actionable error on stderr.

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
