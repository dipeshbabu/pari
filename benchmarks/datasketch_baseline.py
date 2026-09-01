#!/usr/bin/env python3
"""Produce a Pari-schema baseline for overlapping datasketch MinHash/LSH workloads."""

from __future__ import annotations

import argparse
import json
import math
import os
import platform
import statistics
import sys
import time
from collections.abc import Iterable
from importlib import metadata
from pathlib import Path

from datasketch import MinHash, MinHashLSH

MASK64 = (1 << 64) - 1


def mix64(value: int) -> int:
    value &= MASK64
    value ^= value >> 30
    value = (value * 0xBF58476D1CE4E5B9) & MASK64
    value ^= value >> 27
    value = (value * 0x94D049BB133111EB) & MASK64
    return (value ^ (value >> 31)) & MASK64


def corpus(items: int, set_size: int, seed: int) -> list[list[int]]:
    stride = set_size + 1
    return [
        sorted({mix64(item * stride + offset + seed) for offset in range(set_size)})
        for item in range(items)
    ]


def queries(
    rows: list[list[int]], count: int, overlap: int, seed: int
) -> list[list[int]]:
    output: list[list[int]] = []
    for query_index in range(count):
        source = rows[query_index % len(rows)]
        retained = min(overlap, len(source))
        query = list(source[:retained])
        for replacement in range(len(source) - retained):
            value = (
                0xF000000000000000
                ^ seed
                ^ ((query_index * 0x9E3779B9) & MASK64)
                ^ replacement
            )
            query.append(mix64(value))
        output.append(sorted(set(query)))
    return output


def signature(values: Iterable[int], num_perm: int, seed: int) -> MinHash:
    sketch = MinHash(num_perm=num_perm, seed=seed, scheme="affine32")
    sketch.update_batch(value.to_bytes(8, "little") for value in values)
    return sketch


def exact_jaccard(left: list[int], right: list[int]) -> float:
    left_index = 0
    right_index = 0
    intersection = 0
    while left_index < len(left) and right_index < len(right):
        if left[left_index] < right[right_index]:
            left_index += 1
        elif left[left_index] > right[right_index]:
            right_index += 1
        else:
            intersection += 1
            left_index += 1
            right_index += 1
    union = len(left) + len(right) - intersection
    return 1.0 if union == 0 else intersection / union


def metric(value: float, unit: str, direction: str) -> dict[str, object]:
    return {"value": value, "unit": unit, "direction": direction}


def percentile(values: list[float], fraction: float) -> float:
    if not values:
        return 0.0
    ordered = sorted(values)
    index = math.ceil(fraction * (len(ordered) - 1))
    return ordered[index]


def main() -> None:
    parser = argparse.ArgumentParser()
    parser.add_argument("--items", type=int, default=5_000)
    parser.add_argument("--queries", type=int, default=100)
    parser.add_argument("--set-size", type=int, default=100)
    parser.add_argument("--overlap", type=int, default=90)
    parser.add_argument("--threshold", type=float, default=0.8)
    parser.add_argument("--num-perm", type=int, default=128)
    parser.add_argument("--seed", type=int, default=7)
    parser.add_argument(
        "--output", type=Path, default=Path("datasketch-benchmark.json")
    )
    args = parser.parse_args()

    rows = corpus(args.items, args.set_size, args.seed)
    query_rows = queries(rows, args.queries, args.overlap, args.seed)

    started = time.perf_counter()
    signatures = [signature(row, args.num_perm, args.seed) for row in rows]
    signature_elapsed = time.perf_counter() - started

    query_signatures = [signature(row, args.num_perm, args.seed) for row in query_rows]

    index = MinHashLSH(threshold=args.threshold, num_perm=args.num_perm)
    started = time.perf_counter()
    with index.insertion_session() as session:
        for key, sketch in enumerate(signatures):
            session.insert(key, sketch)
    build_elapsed = time.perf_counter() - started

    latencies_ms: list[float] = []
    results: list[list[int]] = []
    started = time.perf_counter()
    for sketch in query_signatures:
        query_started = time.perf_counter()
        result = index.query(sketch)
        latencies_ms.append((time.perf_counter() - query_started) * 1_000.0)
        results.append(result)
    scalar_elapsed = time.perf_counter() - started

    exact_matches = 0
    found_exact = 0
    total_candidates = 0
    exact_candidates = 0
    for query, candidates in zip(query_rows, results, strict=True):
        candidate_set = set(candidates)
        total_candidates += len(candidates)
        for key, row in enumerate(rows):
            if exact_jaccard(query, row) + sys.float_info.epsilon >= args.threshold:
                exact_matches += 1
                if key in candidate_set:
                    found_exact += 1
        for key in candidates:
            if (
                exact_jaccard(query, rows[key]) + sys.float_info.epsilon
                >= args.threshold
            ):
                exact_candidates += 1

    def ratio(numerator: int, denominator: int) -> float:
        if denominator == 0:
            return 1.0 if numerator == 0 else 0.0
        return numerator / denominator

    metrics = {
        "signature.items_per_second": metric(
            args.items / signature_elapsed, "items/s", "higher"
        ),
        "signature.elapsed_ms": metric(signature_elapsed * 1_000.0, "ms", "lower"),
        "index.build_items_per_second": metric(
            args.items / build_elapsed, "items/s", "higher"
        ),
        "index.build_elapsed_ms": metric(build_elapsed * 1_000.0, "ms", "lower"),
        "index.live_items": metric(float(args.items), "items", "neutral"),
        "query.scalar_queries_per_second": metric(
            args.queries / scalar_elapsed, "items/s", "higher"
        ),
        "query.scalar_p50_ms": metric(percentile(latencies_ms, 0.50), "ms", "lower"),
        "query.scalar_p95_ms": metric(percentile(latencies_ms, 0.95), "ms", "lower"),
        "query.scalar_p99_ms": metric(percentile(latencies_ms, 0.99), "ms", "lower"),
        "candidate.recall": metric(
            ratio(found_exact, exact_matches), "ratio", "higher"
        ),
        "candidate.precision": metric(
            ratio(exact_candidates, total_candidates), "ratio", "higher"
        ),
        "candidate.average_candidates": metric(
            ratio(total_candidates, args.queries), "items", "lower"
        ),
        "candidate.exact_matches": metric(float(exact_matches), "pairs", "neutral"),
    }

    report = {
        "schema_version": 1,
        "engine": "datasketch",
        "generated_unix_seconds": int(time.time()),
        "environment": {
            "os": platform.system().lower(),
            "arch": platform.machine(),
            "logical_cpus": os.cpu_count() or 1,
            "rustc": f"python {platform.python_version()}",
            "git_sha": f"datasketch-{metadata.version('datasketch')}",
        },
        "config": {
            "items": args.items,
            "queries": args.queries,
            "set_size": args.set_size,
            "overlap": args.overlap,
            "threshold": args.threshold,
            "num_perm": args.num_perm,
            "seed": args.seed,
            "dataset": None,
        },
        "metrics": metrics,
    }
    args.output.write_text(
        json.dumps(report, indent=2, sort_keys=True) + "\n", encoding="utf-8"
    )
    print(f"wrote {args.output}")
    print(f"median query latency: {statistics.median(latencies_ms):.6f} ms")


if __name__ == "__main__":
    main()
