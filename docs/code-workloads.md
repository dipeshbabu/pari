# Code corpus deduplication

`examples/code_workload.py` is a reference pipeline for source-code clone detection, training-data cleanup, benchmark contamination screening, and repository overlap analysis. It uses public `DedupeIndex` and `MinHash` behavior; tokenization and file traversal remain outside Pari's Rust core.

This workflow targets lexical near-duplicates. It does not parse language syntax or claim that candidate groups are semantically equivalent programs.

## Reproducible fixture

The checked-in fixture has two repositories. Their checksum functions differ only in numeric literals, while the Rust file is unrelated.

```bash
python examples/code_workload.py \
  --root repo-alpha=examples/code_corpus_fixture/repo-alpha \
  --root repo-beta=examples/code_corpus_fixture/repo-beta \
  --groups-output code-groups.jsonl \
  --decisions-output code-decisions.jsonl \
  --metrics-output code-metrics.json \
  --index code-corpus.pari \
  --shingle-size 3 \
  --num-perm 64 \
  --seed 7 \
  --batch-size 2 \
  --exact \
  --exact-threshold 1.0
```

The result is deterministic for the same files and options. `repo-alpha/src/checksum.py` is the representative because repositories and paths are traversed in lexical order. The same logical run produces identical group and decision files with the in-memory or persistent backend.

Workflow-owned outputs are staged beside their requested destinations and published only after the index, groups, decisions, and metrics all succeed. A parsing or callback failure leaves none of those final artifacts.

## Directory input

Repeat `--root REPOSITORY=PATH` to scan multiple repositories. Repository names must be unique. Output identities retain the repository, slash-normalized relative path, and deterministic integer key:

```json
{"key":0,"path":"src/checksum.py","repository":"repo-alpha"}
```

The default extension allowlist covers common C/C++, C#, Go, Java, JavaScript/TypeScript, Kotlin, PHP, Python, Ruby, Rust, Scala, shell, and Swift files. Repeat `--extension py` or `--extension .rs` to replace that list. Hidden entries, symlinks, files larger than 1 MiB, NUL-containing files, and token-empty files are skipped. The report counts each applied skip policy except symlinks, which traversal never follows. Use `--include-hidden` or `--max-file-bytes` when the corpus needs a different policy.

Traversal sorts one directory at a time and reads one accepted file at a time. It does not build a repository-wide path list or retain raw file contents after their features enter a batch. Each accepted file must be UTF-8. A file that changes while it is read, or before exact verification reloads it, fails the run.

## JSONL input

Use line-oriented records when another system already owns traversal:

```json
{"repository":"repo-alpha","path":"src/checksum.py","content":"def checksum(values): ..."}
{"repository":"repo-beta","path":"lib/checksum.py","content":"def checksum(values): ..."}
```

```bash
python examples/code_workload.py \
  --input-jsonl code-records.jsonl \
  --groups-output groups.jsonl \
  --decisions-output decisions.jsonl \
  --metrics-output metrics.json
```

Records stream by byte offset. Duplicate `(repository, path)` identities, malformed JSON, missing strings, and source changes fail explicitly. Exact verification seeks back to the original record through a bounded cache rather than retaining the JSONL corpus.

## Default lexical features

The language-neutral lexer applies Unicode NFKC normalization and normalizes line endings. It recognizes identifiers, numeric literals, quoted strings, common multi-character operators, and remaining non-whitespace symbols. Numeric literals become `<number>` and quoted strings become `<string>`; identifiers and operators stay unchanged. Token shingles are ordered n-grams, with a default width of five.

Literal normalization catches common type-2 clones such as copied functions with changed constants. It can also group code whose literal changes matter. Raise the exact threshold, preserve literals in an application-specific feature extractor, or add a language-aware verifier when that distinction matters.

Comments are not removed because comment syntax is language-specific. Applications may replace the reference lexer with Tree-sitter, compiler tokens, AST paths, or another feature layer without changing Pari's index.

## Exact verification

`--exact` checks Jaccard similarity over the same normalized token shingles before candidate edges join duplicate groups. `--exact-threshold` defaults to 0.8. Directory files and JSONL rows are reloaded on demand through an LRU cache bounded by `--cache-size`.

LSH output remains candidate generation. Exact normalized-token overlap is still a reference policy, not proof of semantic equivalence or license compatibility.

## Outputs

Group JSONL identifies every member and the deterministic first member:

```json
{"members":[{"key":0,"path":"src/checksum.py","repository":"repo-alpha"},{"key":1,"path":"lib/checksum_copy.py","repository":"repo-beta"}],"representative":{"key":0,"path":"src/checksum.py","repository":"repo-alpha"}}
```

Decision JSONL covers each accepted input in key order and records whether to keep it. This makes the reference usable as a training-data filter without retaining raw code in the output.

The metrics report uses schema 1 and includes:

- accepted throughput and process peak RSS where the platform exposes it;
- discovered and skipped-file counts;
- token count and tokens/item;
- candidate-group item rate, group count, duplicate count, and duplicate rate;
- exact pairs checked and accepted;
- output bytes and optional persistent index bytes.

Timing and RSS are benchmark evidence, not CI thresholds. Set `PARI_GIT_SHA` when recording a run:

```bash
PARI_GIT_SHA=$(git rev-parse HEAD) python examples/code_workload.py ...
```

## Memory behavior

Raw content is bounded by `--max-file-bytes` and only the current feature batch is materialized. The native index, lightweight repository/path references, and duplicate-group state necessarily scale with accepted items. Exact verification retains at most `--cache-size` shingle sets. Persistent mode mirrors the same batches into a `.pari` index but does not make group construction constant-memory.

For very large corpora, use [LSH planning](planning.md) before the run, start with a representative sample, and inspect candidate rates through this report and Pari's [observability](observability.md) interfaces.

The checked-in fixture result for this implementation is [`fixture-report.json`](../benchmarks/results/code-corpus/c6873aa48dfa041f8c3721e4bd7fa07349f29cf7/fixture-report.json). It is a correctness and schema record; the three-file timing is not a throughput baseline.
