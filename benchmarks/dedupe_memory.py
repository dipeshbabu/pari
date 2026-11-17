"""Measure the resident-memory model of the high-level ``DedupeIndex``.

Issue #96 asks whether high-level deduplication needs an out-of-core path. This
benchmark measures the current path rather than inferring it: it streams
deterministic synthetic records through ``DedupeIndex`` and records throughput,
grouping cost, peak RSS, and resident bytes per record for the in-memory and
local (persistent mirror) backends.

Records are near-duplicate pairs of ``set_size`` integer features that share
``overlap`` features, matching the scale campaign's synthetic workload. Each
record object is its integer ingestion index, so the measured memory is the
engine's plus the minimum Python bookkeeping a caller must retain.
"""

from __future__ import annotations

import argparse
import json
import os
import platform
import resource
import sys
import tempfile
import time
from collections.abc import Iterator
from pathlib import Path

import pari

PAIR_STRIDE = 1_000


def current_rss_bytes() -> int | None:
    """Return the current resident set size on Linux, or ``None`` elsewhere."""

    try:
        with open("/proc/self/status", encoding="utf-8") as status:
            for line in status:
                if line.startswith("VmRSS:"):
                    return int(line.split()[1]) * 1024
    except OSError:
        return None
    return None


def peak_rss_bytes() -> int:
    """Return the process peak RSS (``ru_maxrss`` is KiB on Linux)."""

    peak = resource.getrusage(resource.RUSAGE_SELF).ru_maxrss
    return peak if sys.platform == "darwin" else peak * 1024


def features(record: int, set_size: int, overlap: int) -> list[bytes]:
    """Features for ``record``; records ``2k`` and ``2k + 1`` share ``overlap``."""

    pair, member = divmod(record, 2)
    base = pair * PAIR_STRIDE
    shared = [base + offset for offset in range(overlap)]
    unique_start = base + overlap + member * (set_size - overlap)
    unique = range(unique_start, unique_start + set_size - overlap)
    return [value.to_bytes(8, "little") for value in (*shared, *unique)]


def records(items: int) -> Iterator[int]:
    yield from range(items)


def measure(args: argparse.Namespace, backend: str, scratch: Path) -> dict:
    path = scratch / f"dedupe-{backend}.pari" if backend == "local" else None
    baseline_rss = current_rss_bytes()
    index = pari.DedupeIndex(
        lambda record: features(record, args.set_size, args.overlap),
        threshold=args.threshold,
        num_perm=args.num_perm,
        seed=args.seed,
        batch_size=args.batch_size,
        path=path,
        backend=backend,
    )
    started = time.perf_counter()
    added = index.add_many(records(args.items))
    ingest_seconds = time.perf_counter() - started
    after_ingest_rss = current_rss_bytes()

    started = time.perf_counter()
    groups = index.groups()
    grouping_seconds = time.perf_counter() - started

    started = time.perf_counter()
    index.close()
    close_seconds = time.perf_counter() - started
    file_bytes = path.stat().st_size if path is not None and path.exists() else None

    members = sum(len(group.member_indices) for group in groups)
    resident = (
        None
        if after_ingest_rss is None or baseline_rss is None
        else after_ingest_rss - baseline_rss
    )
    return {
        "backend": backend,
        "items": added,
        "ingest_seconds": round(ingest_seconds, 3),
        "ingest_items_per_second": round(added / ingest_seconds, 1),
        "grouping_seconds": round(grouping_seconds, 3),
        "close_seconds": round(close_seconds, 3),
        "groups": len(groups),
        "grouped_members": members,
        "rss_before_bytes": baseline_rss,
        "rss_after_ingest_bytes": after_ingest_rss,
        "resident_growth_bytes": resident,
        "resident_bytes_per_item": None
        if resident is None
        else round(resident / added, 1),
        "peak_rss_bytes": peak_rss_bytes(),
        "local_file_bytes": file_bytes,
    }


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--items", type=int, required=True)
    parser.add_argument("--backend", choices=("memory", "local"), required=True)
    parser.add_argument("--output", type=Path, required=True)
    parser.add_argument("--scratch", type=Path, default=None)
    parser.add_argument("--batch-size", type=int, default=65_536)
    parser.add_argument("--threshold", type=float, default=0.8)
    parser.add_argument("--num-perm", type=int, default=128)
    parser.add_argument("--seed", type=int, default=7)
    parser.add_argument("--set-size", type=int, default=32)
    parser.add_argument("--overlap", type=int, default=29)
    args = parser.parse_args()
    if args.items <= 0 or not 0 < args.overlap <= args.set_size:
        parser.error("items must be positive and 0 < overlap <= set_size")

    with tempfile.TemporaryDirectory(dir=args.scratch) as scratch:
        result = measure(args, args.backend, Path(scratch))
    expected_pairs = args.items // 2
    report = {
        "schema_version": 1,
        "benchmark": "pari-dedupe-memory",
        "pari_version": pari.__version__,
        "git_sha": os.environ.get("PARI_GIT_SHA"),
        "python": platform.python_version(),
        "platform": platform.platform(),
        "logical_cpus": os.cpu_count(),
        "config": {
            key: getattr(args, key)
            for key in (
                "items",
                "batch_size",
                "threshold",
                "num_perm",
                "seed",
                "set_size",
                "overlap",
            )
        },
        "expected_duplicate_pairs": expected_pairs,
        "result": result,
    }
    args.output.parent.mkdir(parents=True, exist_ok=True)
    args.output.write_text(json.dumps(report, indent=2) + "\n", encoding="utf-8")
    print(json.dumps(result))
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
