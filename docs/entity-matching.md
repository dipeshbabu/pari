# Entity and record matching

`examples/entity_matching.py` is a reference candidate-generation pipeline for customer/entity resolution, product matching, and structured-data quality work. It streams JSONL records into Pari's batch APIs and exports unique LSH candidate pairs plus connected candidate groups for a downstream verifier or model.

Pari does not decide that two customers or products are the same. The example's normalization profiles are practical starting points, not domain truth.

## Labeled fixtures

The checked-in customer and product fixtures each contain five records, two labeled duplicate pairs, and one unrelated record:

```bash
python examples/entity_matching.py \
  --input examples/entity_matching_fixture/customers.jsonl \
  --profile customer \
  --label-field entity_id \
  --pairs-output customer-pairs.jsonl \
  --groups-output customer-groups.jsonl \
  --metrics-output customer-metrics.json \
  --index customer-index.pari \
  --threshold 0.4 \
  --num-perm 128 \
  --seed 7 \
  --batch-size 2
```

Replace `customers.jsonl` with `products.jsonl` and `--profile customer` with `--profile product` for the product fixture. Both fixtures produce two true candidate pairs from ten possible pairs: recall 1.0, precision 1.0, and a candidate reduction ratio of 0.8. The values describe these small labeled fixtures, not expected production quality.

Omit `--index` for the in-memory path. Memory and persistent runs use the same native batches and produce byte-identical pair and group files.

The index, pair, group, and metrics files are staged and published together after a successful run. Atomic no-replace claims protect destinations created concurrently. Failed ingestion or publication removes transaction-owned files and leaves unrelated destinations intact.

## Input contract

Every line is a JSON object with a unique `id` by default. Use `--id-field` when the identity column has another name. Empty lines are ignored; malformed JSON, duplicate identities, invalid field types, and records with no usable profile fields fail with line context.

The optional label column is only for evaluation:

```json
{"id":"customer-a","entity_id":"customer-1","name":"Alice Smith","address":"12 Main Street, Boston MA","email":"alice@example.com","phone":"+1 (617) 555-0101"}
```

`--label-field entity_id` treats records with the same non-null label as true matches. The label never enters MinHash features, so evaluation cannot leak the answer into candidate generation. Without a label field, recall, precision, and true-pair metrics are `null` while pair counts and reduction remain available.

## Customer profile

The customer profile accepts optional `name`, `address`, `email`, and `phone` strings:

- names contribute normalized word tokens and character trigrams;
- addresses contribute normalized word tokens;
- emails contribute a case-folded exact address and domain;
- phones contribute the final ten digits.

Unicode text uses NFKC normalization plus case folding. Punctuation and whitespace delimit words. Applications should replace this policy where transliteration, apartment handling, country-specific phone rules, nicknames, or privacy constraints require different behavior.

## Product profile

The product profile accepts optional `title`, `brand`, `sku`, and `category` strings:

- titles and categories contribute normalized word tokens;
- brands and SKUs contribute compact, case-folded exact features with punctuation removed.

Whole-title character grams are intentionally omitted. In the labeled fixture they overweight title length and hide strong SKU agreement. Production catalogs may add token-level fuzzy features, manufacturer part numbers, units, or taxonomy-aware normalization in the application layer.

## Candidate outputs

Pair JSONL contains actual unique LSH bucket-collision pairs, not every pair implied by a connected component:

```json
{"left":{"entity_id":"customer-1","id":"customer-a","key":0},"right":{"entity_id":"customer-1","id":"customer-b","key":1},"same_label":true}
```

`same_label` is `null` without complete labels. It is evaluation metadata, not a verification decision.

Group JSONL contains deterministic connected components and their first-record representative:

```json
{"members":[{"entity_id":"customer-1","id":"customer-a","key":0},{"entity_id":"customer-1","id":"customer-b","key":1}],"representative":{"entity_id":"customer-1","id":"customer-a","key":0}}
```

Feed pair rows into a learned matcher, business-rule verifier, review queue, or exact field policy. Group connected components only after deciding which candidate edges to accept when transitive false positives would be costly.

## Evaluation metrics

The schema-1 report includes:

- input items, total possible pairs, candidate pairs, and candidate groups;
- candidate reduction ratio `1 - candidates / possible_pairs`;
- labeled true pairs, true candidate pairs, recall, and precision when labels exist;
- throughput, elapsed time, process peak RSS where supported, output bytes, and optional `.pari` bytes.

The workload retains only lightweight identities, labels, and offsets after each batch. Raw record dictionaries and feature rows are released as iteration advances. The native index and candidate outputs still scale with accepted records and collisions.

Timing is evidence rather than a CI threshold. Set `PARI_GIT_SHA` when recording a run:

```bash
PARI_GIT_SHA=$(git rev-parse HEAD) python examples/entity_matching.py ...
```

Use [LSH planning](planning.md) to choose an initial capacity and threshold, then measure recall and reduction on labels representative of the real matching task. A lower threshold usually raises recall and candidate cost.

The exact customer and product fixture runs for this implementation are recorded in [`fixture-report.json`](../benchmarks/results/entity-matching/1b50580f21213774daac43ded4db21845db9342b/fixture-report.json). Their timing is a reproducibility record, not a throughput baseline.
