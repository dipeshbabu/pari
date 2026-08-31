# Shardable and mergeable index evaluation

## Decision

Defer implementation. Pari's largest validated campaign is one million items and fits a single process. The required dedicated `scale-10m` run has not been recorded, so there is no measured capacity failure or operational need that sharding would solve today.

The fan-out prototype proves that deterministic logical composition is feasible, but it also measures the cost: on 50,000 items and 100 exact-self queries, four sequential shards cost 3.14 times the direct unsharded query, eight cost 2.64 times, and sixteen cost 3.71 times. Candidate results and total bucket memberships remained identical at every shard count.

This satisfies the evidence gate with a defer decision. No manifest, fan-out, or physical-merge implementation issues are opened until the revisit criteria below are met.

## Existing scale evidence

The checked-in `scale-1m` campaign reports:

- 1,000,000 items in one process;
- roughly 2.9 GiB peak RSS for the measured in-memory workload;
- 440 bytes/item for persistent and lazy files;
- 2.60K scalar queries/second and candidate rate 0.00000057;
- parity across in-memory, persistent, lazy, scalar, and batch paths.

The product phases completed without a sharding bottleneck. External construction was I/O dominated. The benchmark policy already requires a dedicated Linux host with at least 128 GiB RAM and fast local scratch before `scale-10m`; a WSL extrapolation is not a substitute.

## Fan-out prototype

Run the public-API prototype with:

```bash
cargo run --release -p pari-bench --bin shard_evaluation -- shard-evaluation.json
```

It constructs deterministic signatures, builds an equivalent unsharded index and disjoint key-modulo shards, queries every shard, and merges candidate keys through an ordered set. It fails evidence review if any result differs from the unsharded index.

| Shards | Maximum items/shard | Query overhead | Candidate parity |
| ---: | ---: | ---: | --- |
| 1 | 50,000 | 2.01x | exact |
| 2 | 25,000 | 1.15x | exact |
| 4 | 12,500 | 3.14x | exact |
| 8 | 6,250 | 2.64x | exact |
| 16 | 3,125 | 3.71x | exact |

The one-shard prototype includes the generic fan-out/ordered-merge path, so its number is not the direct baseline. Timings are sub-millisecond and noisy; the direction after four shards is the relevant result. Parallel fan-out could reduce wall time while increasing scheduling, memory, and storage concurrency. That tradeoff needs a large persistent workload, not this in-memory smoke test.

## Manifest design if the gate changes

A future logical manifest should be data-only JSON or a similarly non-executable format with a versioned schema. Each entry needs:

```json
{
  "schema": "pari-shard-manifest-v1",
  "algorithm": "minhash-lsh",
  "signature_scheme": "pari-affine32-v1",
  "num_perm": 128,
  "seed": 7,
  "threshold": 0.8,
  "bands": 9,
  "rows": 13,
  "key_codec": "u64",
  "shards": [
    {
      "id": "part-0000",
      "path": "part-0000.pari",
      "items": 250000,
      "bytes": 110000736,
      "sha256": "...",
      "key_min": 0,
      "key_max": 999996
    }
  ]
}
```

Opening must validate every shard's format, algorithm, signature scheme, seed, threshold, bands, rows, codec, size, and digest before returning a queryable object. Missing, corrupt, duplicate-ID, or incompatible shards must fail the whole open. Paths must remain data references and must not trigger executable deserialization.

## Key ownership and deterministic queries

The simplest safe first contract is globally unique external keys. Manifest creation must reject overlapping keys; min/max ranges are only a fast precheck, not proof when ranges are sparse. A later composite `(shard_id, local_key)` API would be a different public key type and must not be introduced implicitly.

Queries fan out in manifest order. Each shard returns sorted candidates; aggregation performs a bounded k-way merge and removes duplicate keys. A fixed manifest and shard contents therefore produce deterministic sorted output regardless of execution framework or completion order.

Aggregation memory is proportional to active shard result heads plus the final unique candidates, not total index size. A configurable candidate ceiling should fail explicitly before a high-collision query exhausts memory.

## Grouping across shards

Grouping cannot be performed independently per shard because candidate edges can connect records in different shards. A correct logical path streams each shard's internal edges plus cross-shard query edges into the existing union-find grouping API. It must normalize and deduplicate edges deterministically and preserve exact-verifier semantics.

This is potentially quadratic in shard count if implemented as pairwise shard scans. A production design therefore needs evidence about cross-shard edge volume and an orchestration-neutral edge stream before grouping work begins.

## Physical merge and compaction

Logical composition is cheaper to implement and keeps independently built immutable files. It pays fan-out on every query and retains per-shard metadata overhead.

A physical merge removes fan-out but must externally merge sorted bucket directories, validate key ownership, rewrite checksums and offsets, and remain bounded by the existing builder's spill limits. It must write a new atomic destination and never mutate input shards. The existing version-1 `.pari` files remain valid either way.

No merge implementation is justified until repeated query cost or shard-count operations exceed the cost of rebuilding one canonical index.

## Orchestration boundary

Pari should accept shard files and produce deterministic files/results. It should not schedule workers, discover nodes, retry cluster tasks, or depend on Ray, Spark, Slurm, Kubernetes, or another execution system. Those systems can partition input and invoke the same local builder independently.

## Revisit criteria

Open implementation issues only after a dedicated `scale-10m` or real corpus run demonstrates at least one of:

- a single index exceeds the validated host memory/storage budget despite bounded external construction;
- independent partition builds materially reduce wall time or recovery cost;
- operational ownership requires immutable independently replaceable partitions;
- query or grouping evidence shows that bounded fan-out is acceptable at the required shard count.

That run must record unsharded and prototype-sharded build time, peak RSS, bytes/item, reopen time, query latency, candidate parity/rate, cross-shard edge volume, and merge cost on local scratch. Until then, sharding would add a manifest compatibility contract and permanent operational complexity without a demonstrated payoff.

The exact prototype run for this decision is recorded in [`fanout.json`](../benchmarks/results/sharding/13ca457c115ea905f94552f68d6888d17be06de5/fanout.json).
