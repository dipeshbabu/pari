# Benchmark evidence

This report is generated from validated, versioned benchmark bundles. Timings are evidence, not CI thresholds. Compare rows only when their workload configuration and environment are materially compatible.

## Synthetic and persistent index profiles

| Profile | Source | Items | Signatures/s | Index items/s | Index peak RSS | Lazy bytes/item | Lazy reopen ms | Scalar p99 ms | Candidate rate | Candidate recall |
| --- | --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: |
| scale-100k | [929a62b5ab7a](../benchmarks/results/pari-scale-v1/929a62b5ab7a59ade32878d5b54537e5f73562c1/scale-100k/bundle.json) | 100,000 | 54.69K | 293.80K | 272.3 MiB | 440.00 | 205.55 | 0.0055 | 0.00000570 | 0.5700 |
| scale-1m | [ae58cbf63a2f](../benchmarks/results/pari-scale-v1/ae58cbf63a2f8bec2edcd1601ea7d27c99f1226a/scale-1m/bundle.json) | 1,000,000 | 58.89K | 175.13K | 2925.1 MiB | 440.00 | 2.60K | 0.0246 | 0.00000057 | 0.5700 |

Candidate recall is measured against exact Jaccard ground truth at the configured threshold. It is not expected to be 1.0 for near-threshold queries because LSH candidate generation is probabilistic. Candidate rate is returned pairs divided by all possible query-item pairs.

## Reference text build and cross-corpus audit

| Profile | Reference items | Build items/s | Index bytes/item | Audit queries | Queries/s | Candidate rate | Exact matches |
| --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: |
| scale-100k | 10,000 | 7.89K | 440.02 | 1,000 | 2.57K | 0.00005000 | 500 |
| scale-1m | 100,000 | 7.88K | 440.00 | 10,000 | 6.90K | 0.00000500 | 5K |

The deterministic reference workload plants exact cross-corpus matches and runs exact shingle verification. Corpus generation is excluded from timed phases and its hashes are stored in each bundle.

## Bottleneck evidence and decision gates

The largest validated profile is `scale-1m` at 1,000,000 synthetic items. Its measured Pari product phases were signature construction 16.98s, in-memory index build 5.71s, grouping 3.39s, and scalar plus batch query 0.342ms. Exact ground-truth scanning took 42.33s but is harness-only work and is excluded from product bottleneck decisions.

Persistent construction took 49.38s; bounded external construction took 427.40s; lazy reopen took 2.60s. Peak RSS was 2.86 GiB for the synthetic process, 4.87 GiB during persistent construction, and 3.41 GiB during external construction. The external builder held at most 262K records and produced 440.00 bytes/item.

- **Issue #48: defer implementation pending call-stack profiles.** Signature construction is the first CPU target; query parallelism would optimize a negligible phase in this workload. Capture the documented flamegraph and allocation profile on a dedicated Linux host before changing execution policy.
- **Issue #69: defer sharding implementation.** The 1M profile fits one process, while external construction is I/O-dominant. Run `scale-10m` on dedicated local scratch before using this WSL-backed result to choose a shard crossover point.
- **GPU work: defer.** The measured end-to-end storage path is I/O-bound, and no profile yet shows a GPU-suitable kernel dominating the real text workload.

## Environments

- `scale-100k`: Linux-5.15.167.4-microsoft-standard-WSL2-x86_64-with-glibc2.39; 12 logical CPUs; rustc 1.97.1 (8bab26f4f 2026-07-14).
- `scale-1m`: Linux-5.15.167.4-microsoft-standard-WSL2-x86_64-with-glibc2.39; 12 logical CPUs; rustc 1.97.1 (8bab26f4f 2026-07-14).
