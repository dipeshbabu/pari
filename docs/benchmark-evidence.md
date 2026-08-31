# Benchmark evidence

This report is generated from validated, versioned benchmark bundles. Timings are evidence, not CI thresholds. Compare rows only when their workload configuration and environment are materially compatible.

## Run provenance

The selected bundles were produced from green `main` commit [`ecfe0525585a8033860b75988b8b37ab973256a4`](https://github.com/dipeshbabu/pari/commit/ecfe0525585a8033860b75988b8b37ab973256a4) by the [100K workflow run](https://github.com/dipeshbabu/pari/actions/runs/33446935223) and [1M workflow run](https://github.com/dipeshbabu/pari/actions/runs/33446938541). Both ran on separate GitHub-hosted `ubuntu-latest` x86-64 VMs and used each VM's local ephemeral workspace for generated data, temporary files, and indexes. No remote storage backend or network path was configured.

The bundle environment identifies a native Azure Linux kernel, process-visible CPU and memory limits, toolchains, filesystem capacity, and the campaign's cache policy. The old WSL bundles remain under their original SHAs. Their measurements are historical evidence, not a same-machine baseline for calculating speedup from these runs.

## Synthetic and persistent index profiles

| Profile | Source | Items | Signatures/s | Signature threads | Index items/s | Index peak RSS | Lazy bytes/item | Lazy reopen ms | Scalar p99 ms | Candidate rate | Candidate recall |
| --- | --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: |
| scale-100k | [ecfe0525585a](../benchmarks/results/pari-scale-v1/ecfe0525585a8033860b75988b8b37ab973256a4/scale-100k/bundle.json) | 100,000 | 295.25K | 4 (auto) | 477.79K | 171.2 MiB | 440.00 | 17.96 | 0.0053 | 0.00000570 | 0.5700 |
| scale-1m | [ecfe0525585a](../benchmarks/results/pari-scale-v1/ecfe0525585a8033860b75988b8b37ab973256a4/scale-1m/bundle.json) | 1,000,000 | 322.04K | 4 (auto) | 269.33K | 1902.6 MiB | 440.00 | 363.42 | 0.0031 | 0.00000057 | 0.5700 |

Candidate recall is measured against exact Jaccard ground truth at the configured threshold. It is not expected to be 1.0 for near-threshold queries because LSH candidate generation is probabilistic. Candidate rate is returned pairs divided by all possible query-item pairs.

Every selected bundle passed scalar/batch candidate parity, persistent/lazy candidate parity, and persistent mutation parity before its timing data was accepted.

## Datasketch semantic baseline

| Profile | Pari signatures/s | Datasketch signatures/s | Pari index items/s | Datasketch index items/s | Pari scalar p99 ms | Datasketch scalar p99 ms | Pari recall | Datasketch recall |
| --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: |
| scale-100k | 295.25K | 4.55K | 477.79K | 35.71K | 0.0053 | 0.0296 | 0.5700 | 0.4900 |
| scale-1m | 322.04K | 4.59K | 269.33K | 33.48K | 0.0031 | 0.0322 | 0.5700 | 0.4900 |

The Datasketch 2.0 baseline uses the same deterministic integer sets, query mutation, threshold, permutation count, and exact-Jaccard scoring. Pari and Datasketch use different stable seed-to-permutation mappings and LSH implementations, so signatures and candidate sets are not byte-for-byte interoperability claims. Recall and throughput are reported independently; compare performance only within the recorded environment and workload.

## Reference text build and cross-corpus audit

| Profile | Reference items | Build items/s | Index bytes/item | Audit queries | Queries/s | Candidate rate | Exact matches |
| --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: |
| scale-100k | 10,000 | 26.49K | 440.02 | 1,000 | 13.63K | 0.00005000 | 500 |
| scale-1m | 100,000 | 22.12K | 440.00 | 10,000 | 14.15K | 0.00000500 | 5K |

The deterministic reference workload plants exact cross-corpus matches and runs exact shingle verification. Corpus generation is excluded from timed phases and its hashes are stored in each bundle.

## Bottleneck evidence and decision gates

The largest validated profile is `scale-1m` at 1,000,000 synthetic items. Its measured Pari product phases were signature construction 3.11s, in-memory index build 3.71s, grouping 1.43s, and scalar plus batch query 0.198ms. Exact ground-truth scanning took 30.80s but is harness-only work and is excluded from product bottleneck decisions.

Persistent construction took 16.84s; bounded external construction took 4.92s; lazy reopen took 363.417ms. Peak RSS was 1.86 GiB for the synthetic process, 3.87 GiB during persistent construction, and 2.41 GiB during external construction. The external builder held at most 262K records and produced 440.00 bytes/item.

- **CPU parallelism: keep the bounded signature policy.** The largest profile used 4 effective threads under the bounded automatic policy, with parallel execution enabled. Query phases remain too small in this workload to justify broader parallel scheduling.
- **Issue #69: keep sharding deferred.** The `scale-1m` profile fits one process. Run `scale-10m` on dedicated local scratch before choosing a shard crossover point from scale evidence.
- **GPU work: defer.** The measured end-to-end storage path is I/O-bound, and no profile yet shows a GPU-suitable kernel dominating the real text workload.

## Environments

- `scale-100k`: Linux-6.17.0-1022-azure-x86_64-with-glibc2.39; 4 logical CPUs; 15.61 GiB RAM; rustc 1.97.1 (8bab26f4f 2026-07-14); workspace filesystem 144.26 GiB total and 84.40 GiB free after the run. Cache policy: Dataset generation, signature construction, index construction, and the first query pass are included. Filesystem cache state is not reset between persistent build and reopen.
- `scale-1m`: Linux-6.17.0-1022-azure-x86_64-with-glibc2.39; 4 logical CPUs; 15.61 GiB RAM; rustc 1.97.1 (8bab26f4f 2026-07-14); workspace filesystem 144.26 GiB total and 84.41 GiB free after the run. Cache policy: Dataset generation, signature construction, index construction, and the first query pass are included. Filesystem cache state is not reset between persistent build and reopen.
