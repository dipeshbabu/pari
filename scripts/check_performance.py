"""Fail when a change makes a watched benchmark metric much worse.

CI runs the small `pari-bench` profiles several times, alternating between
the pull request and its merge base (see `scripts/ci_performance.sh`), then
calls this script with one report directory per run. Shared runners are
noisy, so it compares the median of each watched metric and fails only when
the pull request is worse than ``--tolerance`` allows.

The watched metrics are throughput and latency figures that matter to users.
``storage.persistent.mutation_operations_per_second`` is the metric pull
request #263 slowed 5.7x without any check noticing.
"""

from __future__ import annotations

import argparse
import json
import statistics
from pathlib import Path

# Metric name -> the profile that reports it.
WATCHED = {
    "index.build_items_per_second": "run",
    "index.mutation_operations_per_second": "run",
    "signature.items_per_second": "run",
    "grouping.index_items_per_second": "run",
    "query.scalar_queries_per_second": "run",
    "query.batch_queries_per_second": "run",
    "storage.persistent.build_items_per_second": "storage",
    "storage.persistent.mutation_operations_per_second": "storage",
    "storage.persistent.compact_ms": "storage",
    "storage.persistent.reopen_ms": "storage",
    "storage.builder.build_items_per_second": "storage",
    "storage.lazy.reopen_ms": "storage",
    "storage.lazy.batch_queries_per_second": "storage",
}

# Values below these are too small to compare reliably on a shared runner.
FLOOR = {"ms": 5.0}


def median_metric(directories: list[Path], profile: str, name: str) -> dict | None:
    """The metric's median value across runs, with its unit and direction."""

    found = []
    for directory in directories:
        report = json.loads((directory / f"{profile}.json").read_text(encoding="utf-8"))
        metric = report["metrics"].get(name)
        if metric is None:
            return None
        found.append(metric)
    units = {(metric["unit"], metric["direction"]) for metric in found}
    if len(units) != 1:
        return None
    unit, direction = units.pop()
    value = statistics.median(float(metric["value"]) for metric in found)
    return {"value": value, "unit": unit, "direction": direction}


def regressions(
    baseline: list[Path], current: list[Path], tolerance: float
) -> list[str]:
    """One line per watched metric worse by more than ``tolerance``."""

    found = []
    for name, profile in sorted(WATCHED.items()):
        before = median_metric(baseline, profile, name)
        after = median_metric(current, profile, name)
        if before is None or after is None:
            found.append(f"{name}: missing or inconsistent in the {profile} reports")
            continue
        if (before["unit"], before["direction"]) != (after["unit"], after["direction"]):
            found.append(f"{name}: unit or direction changed")
            continue
        old, new = before["value"], after["value"]
        if old <= 0 or new < 0:
            continue
        if max(old, new) < FLOOR.get(before["unit"], 0.0):
            continue
        # The fraction of the baseline's performance the change keeps.
        higher_is_better = before["direction"] == "higher"
        ratio = new / old if higher_is_better else old / max(new, 1e-12)
        if ratio < 1.0 - tolerance:
            found.append(
                f"{name}: median {old:.4g} -> {new:.4g} {before['unit']} "
                f"({(1.0 - ratio) * 100:.0f}% worse; {before['direction']} is better)"
            )
    return found


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument(
        "--baseline",
        type=Path,
        action="append",
        required=True,
        help="directory with run.json and storage.json from the merge base; repeat",
    )
    parser.add_argument(
        "--current",
        type=Path,
        action="append",
        required=True,
        help="directory with run.json and storage.json from the change; repeat",
    )
    parser.add_argument(
        "--tolerance",
        type=float,
        default=0.5,
        help="fraction worse allowed before failing (default 0.5: half as fast)",
    )
    arguments = parser.parse_args()
    if not 0.0 < arguments.tolerance < 1.0:
        parser.error("--tolerance must be between 0 and 1")

    found = regressions(arguments.baseline, arguments.current, arguments.tolerance)
    if not found:
        print(
            f"No watched metric is more than {arguments.tolerance:.0%} worse "
            f"across {len(arguments.current)} runs."
        )
        return 0
    print(f"Watched metrics more than {arguments.tolerance:.0%} worse:")
    for line in found:
        print(f"  {line}")
    return 1


if __name__ == "__main__":
    raise SystemExit(main())
