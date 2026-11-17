# Text deduplication and cross-corpus audits

`examples/text_workload.py` is Pari's reference workflow for streaming document deduplication and train/evaluation contamination auditing. It keeps normalization and shingling outside the engine while using the same public Python APIs as applications.

> **Availability:** this workflow is included in Pari 0.2.0 and newer source distributions. Install `pari-similarity==0.2.0` or the current checkout before running it.

## Input

Commands consume UTF-8 JSONL with configurable identity and text fields. The defaults are `id` and `text`:

```json
{"id":"doc-1","text":"A document to index."}
{"id":"doc-2","text":"Another document."}
```

Identities must be strings or integers. Empty lines are ignored; malformed records fail with line context. Text is normalized with Unicode NFKC plus case folding, tokenized into Unicode word tokens, and converted to configurable word n-grams. This is a reference policy, not behavior embedded in `pari-core` or `pari-index`.

## Deduplicate one corpus

```bash
python examples/text_workload.py dedupe \
  --input documents.jsonl \
  --groups-output duplicate-groups.jsonl \
  --decisions-output keep-drop.jsonl \
  --metrics-output dedupe-metrics.json \
  --index documents.pari \
  --threshold 0.8 \
  --shingle-size 3 \
  --num-perm 128 \
  --seed 7 \
  --batch-size 2048 \
  --threads 8 \
  --exact \
  --exact-threshold 0.8
```

The optional `--index` mirrors the same batches into a persistent `.pari` file. Without it, grouping uses only the in-memory native index. `--exact` checks shingle Jaccard before native candidate components are joined. Exact reference reads use a bounded LRU cache controlled by `--cache-size`.

Deduplication stages the optional index, groups, decisions, and metrics beside their final paths. It atomically claims final names without replacement after the whole workflow succeeds, so malformed input after a committed batch does not leave a partial index and a concurrent writer's destination is not overwritten.

Group rows identify the deterministic first-record representative and all members:

```json
{"members":[{"id":"doc-1","key":0},{"id":"doc-9","key":8}],"representative":{"id":"doc-1","key":0}}
```

Decision rows cover every input item in ingestion order:

```json
{"id":"doc-9","keep":false,"key":8,"representative":{"id":"doc-1","key":0}}
```

## Build a reusable reference index

Build a training/source reference once:

```bash
python examples/text_workload.py build-reference \
  --input training.jsonl \
  --manifest training-reference.json \
  --threshold 0.8 \
  --shingle-size 3 \
  --num-perm 128 \
  --seed 7 \
  --batch-size 2048 \
  --threads 8
```

This creates, beside the manifest:

- `training-reference.pari`: the persistent candidate index;
- `training-reference.records.sqlite3`: key-to-identity and source-offset metadata;
- `training-reference.metrics.json`: machine-readable build measurements.

The manifest records relative artifact paths, signature configuration, input size and modification time, item count, and the index SHA-256 digest. The SQLite sidecar avoids loading all reference identities into memory during audits.

## Audit another corpus without mutation

```bash
python examples/text_workload.py audit \
  --input evaluation.jsonl \
  --manifest training-reference.json \
  --output contamination.jsonl \
  --metrics-output contamination-metrics.json \
  --batch-size 2048 \
  --threads 8 \
  --exact \
  --exact-threshold 0.8
```

The audit opens the existing index, streams evaluation records in batches, and never inserts query records. It validates the reference index hash before and after the run. Exact mode also validates that the reference source has not changed since the manifest was built.

Every output row preserves both corpus identities:

```json
{"candidate_count":1,"matched":true,"query":{"id":"eval-7","key":6},"reference_matches":[{"exact_jaccard":1.0,"id":"train-42","key":41}]}
```

The same manifest can be reused for any number of evaluation sets. For a fixed input, reference, and configuration, candidate order and output rows are deterministic.

## Metrics for benchmark campaigns

Each command writes report schema 1 with `engine`, `workload`, environment, configuration, and a flat metric map compatible with later aggregation in issue #47.

Deduplication reports input throughput, process peak RSS where supported, index bytes, output bytes, candidate-group item rate, group/duplicate rate, and exact pair checks when enabled.

Reference builds report throughput, peak RSS, index bytes/item, build time, and reopen time. Audits report query throughput, peak RSS, reopen time, candidate count/rate/reduction, overlap rate, exact matches, unmatched queries, reference index bytes, and output bytes.

Timing is evidence, not a normal CI threshold. Reports include the Pari version and `PARI_GIT_SHA` when supplied:

```bash
PARI_GIT_SHA=$(git rev-parse HEAD) python examples/text_workload.py audit ...
```

## Memory model

- Raw JSONL is streamed and is never retained as a complete corpus.
- Deduplication retains one lightweight `(key, id, byte offset)` reference per item plus the native LSH index. Feature rows are bounded by `--batch-size`.
- Exact verification reloads source records by byte offset through a bounded cache.
- Reference builds retain only one feature/signature batch in Python while the persistent index and SQLite sidecar grow.
- Audits use bounded query batches, candidate lists for the current batch, the lazy persistent index, and the exact-verification cache.

The index itself necessarily scales with indexed items; batching bounds source payload and temporary feature memory rather than pretending the index is constant-size.

## Public-data transition: WikiText-103

[Salesforce WikiText](https://huggingface.co/datasets/Salesforce/wikitext) provides a public `wikitext-103-raw-v1` corpus. The following optional preparation step streams non-empty rows into Pari's JSONL contract without adding a runtime dependency to Pari:

```bash
python -m pip install datasets
python - <<'PY'
import json
from datasets import load_dataset

for split, output, limit in [
    ("train", "wikitext-train.jsonl", 100_000),
    ("validation", "wikitext-eval.jsonl", 3_000),
]:
    rows = load_dataset(
        "Salesforce/wikitext",
        "wikitext-103-raw-v1",
        split=split,
        streaming=True,
    )
    written = 0
    with open(output, "w", encoding="utf-8") as destination:
        for row_number, row in enumerate(rows):
            text = row["text"].strip()
            if not text:
                continue
            destination.write(json.dumps({"id": f"{split}-{row_number}", "text": text}) + "\n")
            written += 1
            if written == limit:
                break
PY
```

Use `wikitext-train.jsonl` with `build-reference`, then audit `wikitext-eval.jsonl`. Increase limits only after a smaller run establishes expected candidate volume, storage, and memory behavior. Review the dataset card and license before use.
