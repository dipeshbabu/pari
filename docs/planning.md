# LSH planning and explanation

Pari's planner turns an expected corpus size and similarity target into one deterministic configuration report. Rust owns the tuning, probability, capacity, and recommendation logic; the CLI and Python APIs expose the same result.

Every output identifies model `pari-lsh-planner-v1` and labels estimates as analytical or benchmark-calibrated rather than measured guarantees. A plan does not inspect user data, so it cannot predict the dataset's actual duplicate rate, bucket skew, allocator behavior, Redis overhead, or query latency.

## CLI

Plan a one-million-item index with a 2 GiB local memory budget:

```bash
pari plan \
  --items 1000000 \
  --threshold 0.8 \
  --num-perm 128 \
  --memory-budget-mib 2048 \
  --storage auto \
  --json
```

`--storage` accepts `auto`, `memory`, `persistent`, `lazy`, or `redis`. Automatic selection follows a small published policy:

1. Recommend the in-memory index when its model fits the budget with 50% headroom.
2. Otherwise recommend persistent local storage when its resident metadata model fits with the same headroom.
3. If neither local model fits, recommend Redis as an external-storage option. This is capacity guidance, not a claim that operating Redis is always preferable.
4. With no budget, default to the mutable persistent store because capacity is unknown.

An explicit persistent, lazy, or Redis choice is retained. An explicit in-memory choice may be changed to persistent or Redis when its own supplied budget says it cannot fit. The `recommendation_reason` and `recommendation` fields state which rule fired.

Explain an existing local index:

```bash
pari explain --index documents.pari --json
```

This path reads configuration and item-count metadata already loaded when the index opens. It does not scan bucket memberships. `parameter_source` is `existing`; a new plan reports `tuned`.

Both commands keep machine output on stdout and errors on stderr. Their revision-1 JSON shape is pinned by CLI integration tests.

## Python

```python
from pari import Index, plan_lsh

plan = plan_lsh(
    1_000_000,
    threshold=0.8,
    num_perm=128,
    memory_budget_bytes=2 * 1024**3,
    storage="auto",
)

print(plan.bands, plan.rows)
print(plan.recommended_storage, plan.recommendation)
print(plan.persistent_index_bytes)

with Index.open("documents.pari") as index:
    current = index.explain()
    print(current.parameter_source, current.expected_items)
```

`LshPlan.candidate_probability(similarity)` evaluates the canonical LSH collision curve for any finite similarity in `[0, 1]`. Invalid planner inputs raise `ConfigurationError`.

## Rust

```rust
use pari_index::{plan_lsh, LshPlanOptions, StorageMode};

let options = LshPlanOptions::new(1_000_000, 0.8, 128)
    .memory_budget_bytes(2 * 1024 * 1024 * 1024)
    .storage_mode(StorageMode::Auto);
let plan = plan_lsh(options)?;

assert_eq!(plan.params.bands, 9);
assert_eq!(plan.params.rows, 13);
# Ok::<(), pari_index::LshPlanError>(())
```

`LshParams::tune` is the one canonical optimizer used by `LshIndex32::new` and `plan_lsh`. `explain_lsh` validates and explains explicit or persisted parameters without retuning them. `LshIndex32`, `PersistentIndex32`, and `LazyIndex32` provide convenience `explain` methods.

## What the size fields mean

Let `n` be items, `p` be permutations, and `b` be bands.

| Field | Version-1 model | Meaning |
| --- | ---: | --- |
| `signature_bytes_per_item` | `4p` | Exact width of a materialized `MinHash32` signature. The index does not retain the full signature. |
| `bucket_memberships_per_item` | `b` | Exact memberships inserted by the configured banding. |
| `index_metadata_bytes_per_item` | `8 + 16b` | Compact analytical payload for one key, band hashes, and membership identifiers; excludes container and allocator overhead. |
| `persistent_index_bytes_per_item` | `8 + 48b` | `.pari` model for mostly distinct buckets. Duplicate-heavy corpora can use fewer bucket-directory entries. |
| `in_memory_index_bytes_per_item` | `64 + 112b` | Calibrated working-set model for the current hash-map/vector implementation. |
| `lazy_resident_bytes_per_item` | `8 + 56b` | Calibrated resident key and bucket-directory model. Bucket members remain on disk until queried. |

Totals use checked integer arithmetic and fail rather than wrapping. Persistent totals include the model's fixed container allowance. The two `*_with_headroom_bytes` values add 50% for recommendation policy. They do not include the caller's raw records, retained signatures, Python objects, OS page cache, Redis server structures, or concurrent query scratch space.

## Validation and limitations

For threshold `0.8` and 128 permutations, canonical tuning chooses 9 bands by 13 rows. The planner's candidate probability is:

```text
1 - (1 - similarity^rows)^bands
```

The optimizer integrates this curve below and above the threshold with the same numerical routine used by the explanation output. `cargo run --release -p pari-index --example planner_validation` builds deterministic controlled signatures, runs them through the real index, and compares observed candidate rates at similarities 0.5, 0.8, and 0.9. The compiled test requires every absolute error to remain at or below 0.04.

The capacity coefficients are checked against Pari's existing 100,000- and 1,000,000-item Linux x86-64 campaigns:

| Quantity at 9 bands | Model bytes/item | 100k measured | 1m measured | Intended tolerance |
| --- | ---: | ---: | ---: | ---: |
| Persistent file | 440 | 440.002 | 440.001 | within 5% for mostly distinct buckets |
| Lazy reopen RSS delta | 512 | 519.496 | 504.582 | within 15% |
| In-memory index RSS delta | 1,072 | 824.361 | 1,067.565 | conservative and within 35% |

The source measurements are in the versioned `pari-scale-v1` benchmark bundles under `benchmarks/results/`. RSS is noisy and implementation-dependent, which is why the planner exposes a model version, retains headroom, and never calls these values guarantees. Re-run the benchmark campaigns and version the model before changing coefficients.

The probability model assumes independent MinHash rows. Real feature distributions, low-entropy inputs, adversarial keys, and correlated signatures can produce different candidate rates. Use `stats`, query observability, and representative production samples to validate a plan after building the index.
