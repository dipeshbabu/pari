#!/usr/bin/env python3
"""Run and summarize Pari's deterministic signature parallelism matrix."""

from __future__ import annotations

import argparse
import json
import math
import os
import platform
import statistics
import subprocess
import sys
import tempfile
import time
from pathlib import Path
from typing import Any, Sequence

ROOT = Path(__file__).resolve().parent.parent
SCHEMA_VERSION = 1
DEFAULT_SIZES = (64, 128, 256, 512, 1_024, 2_048, 8_192, 100_000)
DEFAULT_THREADS = (1, 2, 4, 8, 12)


class BenchmarkError(RuntimeError):
    """The benchmark configuration, subprocess, or report is invalid."""


def positive_csv(value: str) -> tuple[int, ...]:
    try:
        parsed = tuple(int(item.strip()) for item in value.split(","))
    except ValueError as error:
        raise argparse.ArgumentTypeError("must be comma-separated integers") from error
    if not parsed or any(item <= 0 for item in parsed) or len(set(parsed)) != len(parsed):
        raise argparse.ArgumentTypeError("values must be unique positive integers")
    return parsed


def command_output(
    command: Sequence[str], root: Path, environment: dict[str, str] | None = None
) -> str:
    completed = subprocess.run(
        command,
        cwd=root,
        env=environment,
        check=False,
        capture_output=True,
        text=True,
        encoding="utf-8",
        errors="replace",
    )
    if completed.returncode != 0:
        detail = (completed.stderr or completed.stdout).strip()
        raise BenchmarkError(f"command failed: {' '.join(command)}\n{detail}")
    return completed.stdout.strip()


def read_report(path: Path, *, items: int, threads: int, git_sha: str) -> dict[str, Any]:
    try:
        report = json.loads(path.read_text(encoding="utf-8"))
    except (OSError, json.JSONDecodeError) as error:
        raise BenchmarkError(f"could not read report {path}: {error}") from error
    if not isinstance(report, dict) or report.get("schema_version") != 1:
        raise BenchmarkError(f"unsupported benchmark report in {path}")
    config = report.get("config")
    environment = report.get("environment")
    metrics = report.get("metrics")
    if not isinstance(config, dict) or not isinstance(environment, dict) or not isinstance(metrics, dict):
        raise BenchmarkError(f"incomplete benchmark report in {path}")
    if config.get("items") != items or config.get("threads") != threads:
        raise BenchmarkError(f"benchmark report configuration mismatch in {path}")
    if environment.get("git_sha") != git_sha:
        raise BenchmarkError(f"benchmark report Git SHA mismatch in {path}")
    for name in (
        "signature.elapsed_ms",
        "signature.items_per_second",
        "signature.threads",
        "signature.parallel",
        "query.scalar_batch_parity",
        "index.build_elapsed_ms",
        "grouping.index_elapsed_ms",
        "grouping.stream_elapsed_ms",
    ):
        metric = metrics.get(name)
        if not isinstance(metric, dict):
            raise BenchmarkError(f"missing metric {name!r} in {path}")
        value = metric.get("value")
        if isinstance(value, bool) or not isinstance(value, (int, float)) or not math.isfinite(value):
            raise BenchmarkError(f"invalid metric {name!r} in {path}")
    if metrics["query.scalar_batch_parity"]["value"] != 1.0:
        raise BenchmarkError(f"candidate parity failed in {path}")
    return report


def metric(report: dict[str, Any], name: str) -> float:
    return float(report["metrics"][name]["value"])


def product_phase_ms(report: dict[str, Any]) -> float:
    metrics = report["metrics"]
    total = sum(
        metric(report, name)
        for name in (
            "signature.elapsed_ms",
            "index.build_elapsed_ms",
            "query.scalar_elapsed_ms",
            "query.batch_elapsed_ms",
            "grouping.index_elapsed_ms",
            "grouping.stream_elapsed_ms",
        )
    )
    query_signature_rate = metric(report, "query_signature.items_per_second")
    queries = int(report["config"]["queries"])
    if query_signature_rate > 0.0:
        total += queries / query_signature_rate * 1_000.0
    mutation_rate = metric(report, "index.mutation_operations_per_second")
    mutation_items = min(math.ceil(int(report["config"]["items"]) / 100), 1_000) * 2
    if mutation_rate > 0.0:
        total += mutation_items / mutation_rate * 1_000.0
    return total


def distribution(values: Sequence[float]) -> dict[str, float]:
    return {
        "minimum": min(values),
        "median": statistics.median(values),
        "maximum": max(values),
    }


def summarize_group(items: int, requested_threads: int, reports: Sequence[dict[str, Any]]) -> dict[str, Any]:
    effective = {int(metric(report, "signature.threads")) for report in reports}
    parallel = {bool(metric(report, "signature.parallel")) for report in reports}
    if len(effective) != 1 or len(parallel) != 1:
        raise BenchmarkError("effective thread policy changed between repeats")
    return {
        "items": items,
        "requested_threads": requested_threads,
        "effective_threads": effective.pop(),
        "parallel": parallel.pop(),
        "signature_elapsed_ms": distribution(
            [metric(report, "signature.elapsed_ms") for report in reports]
        ),
        "signature_items_per_second": distribution(
            [metric(report, "signature.items_per_second") for report in reports]
        ),
        "product_phase_elapsed_ms": distribution(
            [product_phase_ms(report) for report in reports]
        ),
        "signature_peak_rss_bytes": distribution(
            [metric(report, "memory.signature_peak_rss_bytes") for report in reports]
        ),
    }


def speedups(results: Sequence[dict[str, Any]]) -> list[dict[str, Any]]:
    by_size: dict[int, dict[int, dict[str, Any]]] = {}
    for result in results:
        by_size.setdefault(result["items"], {})[result["requested_threads"]] = result
    output = []
    for items, thread_results in sorted(by_size.items()):
        baseline = thread_results.get(1)
        if baseline is None:
            raise BenchmarkError("thread matrix requires a one-thread baseline")
        for threads, current in sorted(thread_results.items()):
            if threads == 1:
                continue
            output.append(
                {
                    "items": items,
                    "requested_threads": threads,
                    "effective_threads": current["effective_threads"],
                    "signature_speedup": baseline["signature_elapsed_ms"]["median"]
                    / current["signature_elapsed_ms"]["median"],
                    "product_phase_speedup": baseline["product_phase_elapsed_ms"]["median"]
                    / current["product_phase_elapsed_ms"]["median"],
                }
            )
    return output


def run(args: argparse.Namespace) -> None:
    root = args.repo_root.resolve()
    output = args.output.resolve()
    if output.exists():
        raise BenchmarkError(f"output already exists: {output}")
    git_sha = command_output(["git", "rev-parse", "HEAD"], root)
    dirty = bool(command_output(["git", "status", "--porcelain"], root))
    if dirty and not args.allow_dirty:
        raise BenchmarkError("the worktree is dirty; commit changes or pass --allow-dirty")

    environment = os.environ.copy()
    environment["PARI_GIT_SHA"] = git_sha
    if args.target_dir is not None:
        environment["CARGO_TARGET_DIR"] = str(args.target_dir.resolve())
    command_output(
        ["cargo", "build", "--release", "-p", "pari-bench"], root, environment
    )
    target = Path(environment.get("CARGO_TARGET_DIR", root / "target"))
    binary = target / "release" / ("pari-bench.exe" if os.name == "nt" else "pari-bench")
    if not binary.is_file():
        raise BenchmarkError(f"benchmark binary was not created: {binary}")

    grouped: dict[tuple[int, int], list[dict[str, Any]]] = {}
    with tempfile.TemporaryDirectory(prefix="pari-parallel-benchmark-") as temporary:
        temporary_path = Path(temporary)
        for items in args.sizes:
            for threads in args.threads:
                for repeat in range(args.repeats):
                    report_path = temporary_path / f"{items}-{threads}-{repeat}.json"
                    command = [
                        str(binary),
                        "run",
                        "--items",
                        str(items),
                        "--queries",
                        "1",
                        "--set-size",
                        str(args.set_size),
                        "--overlap",
                        str(args.overlap),
                        "--threshold",
                        str(args.threshold),
                        "--num-perm",
                        str(args.num_perm),
                        "--seed",
                        str(args.seed),
                        "--threads",
                        str(threads),
                        "--output",
                        str(report_path),
                    ]
                    completed = subprocess.run(
                        command,
                        cwd=root,
                        env=environment,
                        check=False,
                        capture_output=True,
                        text=True,
                        encoding="utf-8",
                        errors="replace",
                    )
                    if completed.returncode != 0:
                        raise BenchmarkError(
                            f"benchmark failed: {' '.join(command)}\n{completed.stderr}"
                        )
                    grouped.setdefault((items, threads), []).append(
                        read_report(
                            report_path,
                            items=items,
                            threads=threads,
                            git_sha=git_sha,
                        )
                    )

    results = [
        summarize_group(items, threads, reports)
        for (items, threads), reports in sorted(grouped.items())
    ]
    report = {
        "schema_version": SCHEMA_VERSION,
        "workload": "parallel-signatures-v1",
        "generated_unix_seconds": int(time.time()),
        "git_sha": git_sha,
        "worktree_clean": not dirty,
        "environment": {
            "operating_system": platform.platform(),
            "architecture": platform.machine(),
            "logical_cpus": os.cpu_count() or 1,
            "rustc": command_output(["rustc", "--version"], root),
        },
        "config": {
            "sizes": list(args.sizes),
            "thread_limits": list(args.threads),
            "repeats": args.repeats,
            "set_size": args.set_size,
            "overlap": args.overlap,
            "threshold": args.threshold,
            "num_perm": args.num_perm,
            "seed": args.seed,
        },
        "results": results,
        "speedups": speedups(results),
    }
    output.parent.mkdir(parents=True, exist_ok=True)
    output.write_text(json.dumps(report, indent=2, sort_keys=True) + "\n", encoding="utf-8")
    print(f"wrote {output}")


def build_parser() -> argparse.ArgumentParser:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--repo-root", type=Path, default=ROOT)
    parser.add_argument("--output", type=Path, required=True)
    parser.add_argument("--target-dir", type=Path)
    parser.add_argument("--sizes", type=positive_csv, default=DEFAULT_SIZES)
    parser.add_argument("--threads", type=positive_csv, default=DEFAULT_THREADS)
    parser.add_argument("--repeats", type=int, default=3)
    parser.add_argument("--set-size", type=int, default=32)
    parser.add_argument("--overlap", type=int, default=29)
    parser.add_argument("--threshold", type=float, default=0.8)
    parser.add_argument("--num-perm", type=int, default=128)
    parser.add_argument("--seed", type=int, default=7)
    parser.add_argument("--allow-dirty", action="store_true")
    return parser


def main(argv: Sequence[str] | None = None) -> int:
    args = build_parser().parse_args(argv)
    if args.repeats <= 0 or args.set_size <= 0 or args.num_perm <= 0:
        print("parallel benchmark error: repeats, set-size, and num-perm must be positive", file=sys.stderr)
        return 2
    if not 0 <= args.overlap <= args.set_size:
        print("parallel benchmark error: overlap must be in 0..=set-size", file=sys.stderr)
        return 2
    if not 0.0 < args.threshold <= 1.0:
        print("parallel benchmark error: threshold must be in (0, 1]", file=sys.stderr)
        return 2
    try:
        run(args)
    except BenchmarkError as error:
        print(f"parallel benchmark error: {error}", file=sys.stderr)
        return 2
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
