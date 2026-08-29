# CPU parallelism

Pari parallelizes batch signature construction because the issue #47 profiles identify it as the first CPU-bound product phase. Scalar updates, index mutation, queries, and grouping remain single-threaded.

## Evidence

The 100K-row profile uses 32 byte features per row and 128 affine32 permutations. Linux `perf` captured 2,595 samples with none lost. `MinHash32::update` accounted for 35.6% self-time, led by the affine `update_hashed` multiplication loop. Scalar and batch query work stayed below 1 ms at 1M items in the published scale report.

Heaptrack recorded 300,003 allocation calls and 153.6 MB peak heap on the 100K signature call stack. Before this change, each sketch owned three arrays: its signature plus identical multiplier and offset arrays. Batch construction now creates the permutations once and shares them as immutable arrays. Every returned sketch still owns its signature buffer. The commands and profiler summary are stored in [the versioned profile artifact](../benchmarks/results/parallel-cpu/0a9fc94856b1d7e618435c24cc4f39768f859998/profile-summary.json).

The committed parallel benchmark report records medians across three runs for each batch size and thread limit. Run the matrix with:

```bash
python scripts/parallel_benchmark.py \
  --output parallel-signatures.json \
  --sizes 64,128,256,512,1024,2048,8192,100000 \
  --threads 1,2,4,8,12 \
  --repeats 3
```

The runner builds the current `pari-bench` release binary, rejects dirty worktrees by default, checks every report's Git SHA and scalar/batch candidate parity, then records signature and product-phase speedups in versioned JSON.

## Execution policy

- Batches below 256 rows use the scalar loop and do not initialize a worker pool.
- Automatic execution uses at most eight workers. The 100K profile improved through eight workers and regressed at twelve on the measured 12-logical-CPU host.
- Explicit limits are capped by `std::thread::available_parallelism`, so container and affinity CPU limits take precedence.
- The most recently used worker pool is cached. Repeated bounded batches with the same limit do not recreate operating-system threads, while changing limits cannot accumulate an unbounded pool cache.
- Ordered Rayon chunks preserve input order. Tests compare signatures across sequential, two-, four-, and eight-thread execution.

## Rust API

Byte-like feature rows can use `MinHash32::from_batch`:

```rust
use std::num::NonZeroUsize;

use pari_core::{BatchThreads, MinHash32};

let rows = vec![vec![b"alpha".as_slice()], vec![b"beta".as_slice()]];
let sketches = MinHash32::from_batch(
    &rows,
    128,
    7,
    BatchThreads::max(NonZeroUsize::new(4).unwrap()),
)?;
# Ok::<(), pari_core::MinHashError>(())
```

Structured inputs can use `from_batch_with` to update sketches without allocating temporary byte rows. `BatchThreads::Auto` is the default policy used by the Python bindings and benchmark harness.

## Python API

`MinHash.from_batch`, `DedupeIndex`, and `deduplicate` accept `threads=None` for automatic execution or a positive integer limit:

```python
from pari import MinHash, deduplicate

sketches = MinHash.from_batch(feature_rows, num_perm=128, seed=7, threads=4)

result = deduplicate(
    records,
    feature=features,
    batch_size=2048,
    threads=4,
)
```

Python feature iteration and buffer copying happen under the GIL. Rust signature construction starts only after those buffers are owned by Rust, and the whole parallel phase runs outside the GIL. Increasing `threads` cannot accelerate Python feature extraction.

## Limits

Parallel index mutation is intentionally excluded. Batch insertion still validates the complete batch before mutation, and persistent/local writes keep their existing atomic semantics. Query parallelism is also excluded because the measured query phase is too small to justify scheduling overhead.
