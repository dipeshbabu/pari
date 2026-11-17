# Dedicated 10M scale run

The `scale-10m` campaign answers the scaling questions in issue #95: whether one
in-memory index still fits, whether persistent and external construction are
CPU-, allocation-, or I/O-bound, and whether sharding, out-of-core
deduplication, or GPU work should be reopened. It must run on a dedicated
native Linux host so peak memory and storage throughput are interpretable.

## Host requirements

| Requirement | Minimum | Why |
| --- | --- | --- |
| Operating system | Native Linux VM or bare metal | WSL and container overlays distort storage and memory measurements |
| Memory | 128 GiB (192–256 GiB preferred) | The 1M run peaked at 3.87 GiB; 10M is expected near 40 GiB plus page cache headroom |
| CPU | 16 or more dedicated cores | Shared or burstable vCPUs make throughput unrepeatable |
| Scratch storage | 200 GiB free on local NVMe | Generated data, indexes, external-build spills, and preserved failure evidence |
| Tenancy | No other workloads | Noisy neighbors invalidate peak RSS and I/O results |
| Duration | Up to 12 hours | The profile timeout is 720 minutes |

Shared GPU marketplaces that run workloads inside containers on other people's
machines do not meet the tenancy and storage requirements.

## Run

```bash
git clone https://github.com/dipeshbabu/pari.git
cd pari
bash scripts/run_dedicated_scale.sh /mnt/nvme/scale-run
```

Point the argument at the local NVMe mount, outside the checkout. The script:

1. refuses non-Linux hosts, WSL, less than 128 GiB of RAM, less than 200 GiB of
   free scratch space, and dirty checkouts;
2. installs the pinned Rust toolchain and builds the Python wheel used by the
   text stage;
3. records host details plus `vmstat` and, when installed, `iostat` telemetry;
4. runs the `scale-1m` preflight, then `scale-10m`, both with the text stage and
   with failure evidence preserved;
5. measures the high-level `DedupeIndex` memory model with
   `benchmarks/dedupe_memory.py` for the in-memory and local backends at 10M
   records, one process each (issue #96);
6. writes `pari-scale-evidence-<sha>.tar.gz` and its SHA-256, and exits non-zero
   if a profile failed.

Run it inside `tmux` or `screen` so an SSH disconnect does not stop it. Install
`sysstat` first for `iostat`. If a phase fails, the archive still contains its
partial telemetry: a failure within the host budget is itself the measured
scaling result and must be recorded before any limit is raised.

## After the run

Copy the archive off the host, then delete the host. The bundles are validated
with `python scripts/benchmark_campaign.py validate`, rendered into
[benchmark evidence](benchmark-evidence.md), and used to answer the decision
questions in issues #95 and #96. Historical 100K and 1M evidence stays
unchanged.
