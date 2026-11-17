# 10M scale evidence and decisions

This report records the dedicated `scale-10m` campaign required by issue #95
and the high-level deduplication memory measurement required by issue #96. All
figures come from validated bundles and raw telemetry committed under
[`benchmarks/results/pari-scale-v1/4d9782209601244ba2cdb280b5a98aa7f7ad8672`](../benchmarks/results/pari-scale-v1/4d9782209601244ba2cdb280b5a98aa7f7ad8672)
and
[`benchmarks/results/dedupe-memory-v1/4d9782209601244ba2cdb280b5a98aa7f7ad8672`](../benchmarks/results/dedupe-memory-v1/4d9782209601244ba2cdb280b5a98aa7f7ad8672).
Historical 100K and 1M evidence in [benchmark evidence](benchmark-evidence.md)
is unchanged.

## Run provenance

- **Pari commit:** `4d9782209601244ba2cdb280b5a98aa7f7ad8672` on `main`, run with
  `scripts/run_dedicated_scale.sh` from a clean checkout. Both bundles pass
  `python scripts/benchmark_campaign.py validate`, including scalar/batch,
  persistent/lazy, and persistent mutation parity.
- **Host:** Google Compute Engine `n2-custom-12-135168-ext` in `us-central1-a`,
  dedicated to this run: 12 vCPU (Intel Xeon, 2.80 GHz), 129.63 GiB usable RAM,
  Ubuntu 24.04.5 LTS (kernel `7.0.0-1017-gcp`), Rust 1.97.1, CPython 3.12.
- **Storage:** a local NVMe SSD formatted ext4 (368 GiB) holding all generated
  data, indexes, spills, and evidence. No network storage was in the data path.
- **Telemetry:** `vmstat` every 5 s and `iostat -xmt` every 5 s for the whole
  run, plus `host.txt` with CPU, memory, disks, mounts, and toolchains.
- **Deviation:** the host has 12 vCPUs rather than the runbook's preferred 16
  because the project's quota caps all regions at 12. Memory, tenancy, native
  Linux, and local NVMe requirements were met. Signature construction used its
  bounded automatic policy of 8 threads on both profiles.

## Same-host scaling, 1M to 10M

| Measurement | `scale-1m` | `scale-10m` | Ratio |
| --- | ---: | ---: | ---: |
| Signature construction | 3.05 s (327.96K/s) | 60.83 s (164.39K/s) | 19.9× |
| In-memory index build | 4.05 s (246.75K/s) | 48.74 s (205.18K/s) | 12.0× |
| Grouping | 2.04 s | 26.11 s | 12.8× |
| Scalar plus batch query | 0.183 ms | 0.216 ms | 1.2× |
| Synthetic process peak RSS | 1.86 GiB | 17.27 GiB | 9.3× |
| Persistent construction | 22.99 s | 345.97 s | 15.0× |
| Persistent construction peak RSS | 3.78 GiB | 36.16 GiB | 9.6× |
| Bounded external construction | 6.46 s | 70.39 s | 10.9× |
| External construction peak RSS | 2.41 GiB | 22.53 GiB | 9.3× |
| Lazy bytes per item | 440.00 | 440.00 | 1.0× |
| Lazy reopen | 0.648 s | 6.43 s | 9.9× |
| Scalar query p99 | 0.0032 ms | 0.0047 ms | 1.5× |
| Candidate recall | 0.5700 | 0.5700 | — |
| Text reference build | 18.94K items/s (100K) | 17.10K items/s (1M) | — |
| Text audit | 12.04K queries/s (10K) | 11.28K queries/s (100K) | — |

Memory, bytes per item, reopen, and the external builder scale close to
linearly. Signature construction, persistent construction, grouping, and index
build scale worse than linearly; signature throughput halves at 10M.

## Where the time and memory go

Host telemetry separates CPU-, allocation-, and I/O-bound phases:

- Across the 137 samples with more than 20 GiB in use, user CPU averaged 7.1%
  (one of 12 cores is 8.3%), I/O wait averaged 0.5%, and about one process was
  runnable. Only 12 of those samples wrote more than 100 MB/s.
- Only 17 of 417 samples used more than 40% CPU; those are the 8-thread
  signature phases. Index build, grouping, persistent construction, the
  external builder, and the text stages each ran on one core.

**First meaningful bottleneck: persistent construction.** It is
single-threaded and CPU- and allocation-bound, not I/O-bound: it took 345.97 s
with a 36.16 GiB peak, 4.9× slower and 1.6× larger than bounded external
construction of the same index (70.39 s, 22.53 GiB). The commit materializes
every bucket and the full snapshot in memory before writing it.

The first 10M attempt (commit `7d6740b`) failed in that stage with
`Format(SectionTooLarge { length: 720000008 })`: a single `BandHashes` section
exceeded the 256 MiB container limit above about 3.7M items. That measured
failure point was fixed by splitting key metadata into row-aligned section
pairs (#233) and verified by a full 10M run before this evidence run.

## High-level `DedupeIndex` memory model

`benchmarks/dedupe_memory.py` streamed 10,000,000 near-duplicate records (32
features, 29 shared within each pair) through `DedupeIndex` in a fresh process
per backend, with integer records so the measurement covers the engine plus
the minimum Python bookkeeping.

| Backend | Ingest | Grouping | Close/sync | Peak RSS | Resident growth | Bytes per record | File |
| --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: |
| `memory` | 198.1 s (50.5K/s) | 24.7 s | 18.5 s | 10.51 GiB | 9.08 GiB | 975 | — |
| `local` | 209.6 s (47.7K/s) | 24.3 s | 315.2 s | 37.08 GiB | 19.03 GiB | 2,043 | 3.98 GiB |

Both backends found the same 2,790,330 duplicate groups. Ingest time includes
building 32 Python byte strings per record; the native in-memory index alone
inserted 205.18K items/s in the synthetic profile. The `local` backend's extra cost is the persistent mirror: its
final sync is the same single-threaded, allocation-bound commit measured above.

## Decisions for issue #95

1. **Does a single in-memory index still fit the intended host budget?** Yes.
   The 10M synthetic process peaked at 17.27 GiB, 13% of the 129.63 GiB host.
2. **Is persistent/external construction CPU-, allocation-, or I/O-bound?**
   Single-threaded CPU- and allocation-bound. Persistent construction used one
   core with 0.5% I/O wait and peaked at 36.16 GiB; the bounded external builder
   is 4.9× faster and 38% smaller for the same output.
3. **Does high-level deduplication retain too much resident state?** Not at
   10M on this host: the `memory` backend peaked at 10.51 GiB (about 975 bytes
   per record). The `local` backend's 37.08 GiB peak comes from the persistent
   commit, not from retained records.
4. **Should sharding be reopened?** No. 10M fits one process with large
   headroom, and the measured bottleneck is a single-threaded commit, which
   sharding would not address. Sharding stays deferred (#69).
5. **Is a GPU-suitable phase dominant?** No. Signature construction, the only
   GPU-suitable kernel, took 60.83 s of 135.68 s in the in-memory product phases
   and already runs on 8 threads; the larger cost is the single-threaded
   persistent commit. GPU work stays deferred.
6. **Are bytes/item and reopen/query behavior stable for projections?** Bytes
   per item are identical (440.00) at 1M and 10M, scalar p99 stays below
   0.005 ms, and lazy reopen grows linearly (9.9× for 10× items). Signature and
   persistent construction scale worse than linearly and should not be
   extrapolated from 10M.

## Decision for issue #96: defer out-of-core high-level deduplication

The current high-level path is not a material memory bottleneck at 10M: the
in-memory backend used 10.51 GiB on a 129.63 GiB host. An out-of-core
`DedupeIndex` would add a new API, record-reloading callbacks, and spillable
grouping to remove a cost that the evidence does not show binding. The
measured problem is narrower: the persistent commit used by the `local`
backend and by `PersistentIndex` is single-threaded and materializes the whole
snapshot, which is what bounds `local` at 37.08 GiB and 315 s.

**Recommendation: defer**, with these revisit gates:

- the in-memory backend's measured resident memory for a target corpus exceeds
  50% of the intended host's RAM (about 975 bytes per record at this feature
  shape, so roughly 70M records on a 128 GiB host, to be confirmed by
  measurement at that scale rather than this projection); or
- a workload needs high-level deduplication without retaining application
  records in Python.

Until then, large corpora should use bounded external construction for
persistent output. Reducing the persistent commit's peak memory and making it
stream buckets like the external builder is tracked in #235,
because it improves `PersistentIndex` and the `local` backend without changing
`DedupeIndex` semantics. No public or persisted semantics change as part of
this decision.
