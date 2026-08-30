# Observability and progress

Pari keeps observation optional. Disabled query observation does not start timers, update counters, allocate metric objects, or add backend round trips. Bucket summaries are calculated only when `stats()` is called.

## Metric semantics

Exact counters:

- live item and persisted byte counts;
- non-empty bucket count, total memberships, minimum, nearest-rank p50/p95/p99, and maximum bucket size;
- process-local query operation count, query count, returned candidates, and possible candidates;
- Redis round trips performed by the current backend handle.

Observed wall-clock values:

- total, maximum, and average query-operation latency;
- progress elapsed time and completed-items rate;
- benchmark RSS samples.

Wall-clock values depend on the host, cache state, scheduler, filesystem, and backend. They are measurements rather than service-level guarantees. Streaming CLI and Python iterables report `total=null` when the total is not known exactly; Pari does not present a length hint as an exact total.

Persistent bucket statistics distinguish committed storage from the mutation overlay. Committed membership counts include suppressed generations until the next compaction. Redis reports live items, memberships, round trips, and TTL without scanning every bucket to manufacture a distribution.

## Rust query metrics

Observation is disabled by default:

```rust
use pari_index::LshIndex32;

let mut index = LshIndex32::new(0.8, 128, 7)?;
index.set_observability(true);

// Insert and query as usual.
let stats = index.stats();
if let Some(queries) = stats.queries {
    println!("candidate rate: {}", queries.candidate_rate());
    println!("average operation ms: {}", queries.average_operation_ms());
}
# Ok::<(), Box<dyn std::error::Error>>(())
```

`LshIndex32`, `PersistentIndex32`, `LazyIndex32`, and `BackendIndex32` expose the same opt-in query metrics. A scalar query counts as one operation and one query. `query_many` counts as one operation and records every query in the batch.

## Python query metrics

Enable metrics when creating or opening an index, or toggle them later:

```python
from pari import Index

index = Index.open("documents.pari", observability=True)
results = index.search_many(queries)
stats = index.stats()

print(stats.candidate_rate)
print(stats.average_query_ms)
print(stats.committed_bucket_p95)

index.set_observability(False)
```

Query metrics are process-local and reset whenever observation is enabled. They are not written into the `.pari` file.

## Python progress callbacks

`DedupeIndex.add_many`, `add_many_features`, and `deduplicate` accept a callback:

```python
from pari import ProgressEvent, deduplicate

def progress(event: ProgressEvent) -> bool:
    print(event.completed, event.total, event.items_per_second)
    return True

result = deduplicate(
    records,
    feature=features,
    batch_size=2048,
    progress=progress,
)
```

Callbacks run after a complete native batch is inserted. Returning `False` raises `ProgressCancelledError` with the committed `completed` count and prevents the next batch. Callback exceptions propagate unchanged. Cancellation does not roll back batches that already completed.

No callback means Pari does not create a timer or `ProgressEvent` objects.

## CLI progress

Long-running `index`, `search`, `dedup`, and `verify` commands accept progress options:

```bash
pari index --input documents.jsonl --output documents.pari --progress
pari search --index documents.pari --input queries.jsonl --json --progress json
pari verify --index documents.pari --json --progress json --progress-every 10000
```

Progress always goes to stderr. Result JSON and JSONL stay on stdout. Human events are concise lines; JSON events use schema 1 and contain `phase`, `completed`, optional exact `total`, elapsed/rate observations, a final-event flag, and candidate fields where available.

Index and dedup progress is emitted once per configured input batch. Search and verify use `--progress-every`, which defaults to 1,000 records or buckets. There is no per-item log call in hot loops.

## Disabled overhead evidence

The Criterion query benchmark compares the pre-change main commit, disabled observation, and enabled observation on the same host. The [selected clean-SHA result](../benchmarks/results/observability/7848cad5eb2c16a5678fcfd1d7112e216c9016cc/overhead-summary.json) measured 1.751 µs on the baseline, 1.826 µs with observation disabled, and 1.919 µs with observation enabled. Disabled overhead was 75 ns (4.3%) on this deliberately tiny query; enabled timers and counters added another 93 ns. Exact stats calculation took 221 µs on a 1,000-item index and runs only when requested.

Timing evidence is not a CI threshold. Correctness tests instead verify that disabled mode has no query metrics, backend round-trip behavior is unchanged, progress stays off stdout, and callback cancellation/error behavior is deterministic.
