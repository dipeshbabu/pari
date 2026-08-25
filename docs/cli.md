# Pari CLI

The `pari` binary is a thin operational layer over the same Rust APIs used by the Python package. It does not reimplement MinHash, LSH, persistence, or duplicate grouping.

## Build

```bash
cargo build --release -p pari-cli
```

The binary is written to `target/release/pari` (`pari.exe` on Windows).

## JSONL record format

`index` and `dedup` consume one record per line:

```json
{"key":1,"values":["new york","rust","search"]}
{"key":2,"values":["new york","python","search"]}
```

Each value is fed to Pari's `MinHash32` as UTF-8 bytes. Input is processed line by line. Raw JSONL records are never accumulated as a complete corpus.

`search` consumes query records:

```json
{"id":"query-1","values":["new york","rust","search"]}
```

`id` is optional and is copied to JSON output for correlation.

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

## Shell completion

Completions are generated from the same Clap command definition as runtime parsing:

```bash
pari completion bash > pari.bash
pari completion zsh > _pari
pari completion fish > pari.fish
```

## Exit behavior

Successful commands return exit code `0`. Parsing, validation, I/O, compatibility, and storage failures return a nonzero exit code and print a single `pari: ...` diagnostic to stderr.
