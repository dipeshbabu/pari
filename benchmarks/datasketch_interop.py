#!/usr/bin/env python3
"""Benchmark an exact Datasketch 2.x affine32-to-Pari migration workflow."""

from __future__ import annotations

import argparse
import json
import os
import platform
import tempfile
import time
from importlib import metadata
from pathlib import Path

import numpy as np
from datasketch import MinHash as DatasketchMinHash
from pari import Index, MinHash, __version__
from pari import datasketch as adapter

MASK64 = (1 << 64) - 1


def mix64(value: int) -> int:
    value &= MASK64
    value ^= value >> 30
    value = (value * 0xBF58476D1CE4E5B9) & MASK64
    value ^= value >> 27
    value = (value * 0x94D049BB133111EB) & MASK64
    return (value ^ (value >> 31)) & MASK64


def corpus(items: int, set_size: int, seed: int) -> list[list[bytes]]:
    stride = set_size + 1
    return [
        [
            mix64(item * stride + offset + seed).to_bytes(8, "little")
            for offset in range(set_size)
        ]
        for item in range(items)
    ]


def metric(value: float, unit: str, direction: str) -> dict[str, object]:
    return {"direction": direction, "unit": unit, "value": value}


def datasketch_signatures(
    rows: list[list[bytes]], num_perm: int, seed: int
) -> list[DatasketchMinHash]:
    template = MinHash(num_perm=num_perm, seed=seed)
    multipliers, offsets = template.permutations
    permutations = (
        np.asarray(multipliers, dtype=np.uint32),
        np.asarray(offsets, dtype=np.uint32),
    )
    output = []
    for row in rows:
        sketch = DatasketchMinHash(
            num_perm=num_perm,
            seed=seed,
            scheme="affine32",
            permutations=permutations,
        )
        sketch.update_batch(row)
        output.append(sketch)
    return output


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--items", type=int, default=1_000)
    parser.add_argument("--queries", type=int, default=100)
    parser.add_argument("--set-size", type=int, default=100)
    parser.add_argument("--threshold", type=float, default=0.8)
    parser.add_argument("--num-perm", type=int, default=128)
    parser.add_argument("--seed", type=int, default=7)
    parser.add_argument("--output", type=Path, default=Path("datasketch-interop.json"))
    args = parser.parse_args()
    if args.items <= 0 or args.queries <= 0 or args.set_size <= 0:
        parser.error("items, queries, and set-size must be positive")
    if args.queries > args.items:
        parser.error("queries cannot exceed items")

    rows = corpus(args.items, args.set_size, args.seed)
    query_rows = rows[: args.queries]

    started = time.perf_counter()
    external = datasketch_signatures(rows, args.num_perm, args.seed)
    datasketch_elapsed = time.perf_counter() - started

    started = time.perf_counter()
    native = MinHash.from_batch(rows, num_perm=args.num_perm, seed=args.seed)
    pari_elapsed = time.perf_counter() - started
    parity = all(
        sketch.signature == [int(value) for value in external_sketch.hashvalues]
        for sketch, external_sketch in zip(native, external)
    )
    if not parity:
        raise RuntimeError("signature parity failed before benchmark comparison")

    started = time.perf_counter()
    imported = [adapter.from_datasketch(sketch) for sketch in external]
    import_elapsed = time.perf_counter() - started
    if any(left.signature != right.signature for left, right in zip(native, imported)):
        raise RuntimeError("adapter changed signature values")

    external_queries = datasketch_signatures(query_rows, args.num_perm, args.seed)
    imported_queries = [adapter.from_datasketch(sketch) for sketch in external_queries]
    with tempfile.TemporaryDirectory() as directory:
        path = Path(directory) / "migration.pari"
        index = Index.create(
            path,
            threshold=args.threshold,
            num_perm=args.num_perm,
            seed=args.seed,
        )
        started = time.perf_counter()
        index.add_many(list(enumerate(imported)))
        index.sync()
        build_elapsed = time.perf_counter() - started

        started = time.perf_counter()
        results = index.search_many(imported_queries)
        query_elapsed = time.perf_counter() - started
        index_bytes = index.stats().file_bytes
        index.close()

    self_matches = sum(
        int(query_index in candidates) for query_index, candidates in enumerate(results)
    )
    self_recall = self_matches / args.queries
    if self_recall != 1.0:
        raise RuntimeError(f"converted signature self-recall failed: {self_recall}")

    report = {
        "config": {
            "items": args.items,
            "lsh_candidate_sets_comparable": False,
            "num_perm": args.num_perm,
            "queries": args.queries,
            "seed": args.seed,
            "set_size": args.set_size,
            "signature_semantics": "datasketch-affine32-with-pari-permutations",
            "threshold": args.threshold,
        },
        "engine": "pari-datasketch-interop",
        "environment": {
            "architecture": platform.machine(),
            "datasketch_version": metadata.version("datasketch"),
            "logical_cpus": os.cpu_count() or 1,
            "operating_system": platform.system(),
            "pari_version": __version__,
            "python_version": platform.python_version(),
        },
        "generated_unix_seconds": int(time.time()),
        "metrics": {
            "adapter.import_items_per_second": metric(
                args.items / import_elapsed, "items/second", "higher"
            ),
            "datasketch.signature_items_per_second": metric(
                args.items / datasketch_elapsed, "items/second", "higher"
            ),
            "pari.index_build_items_per_second": metric(
                args.items / build_elapsed, "items/second", "higher"
            ),
            "pari.index_bytes": metric(index_bytes, "bytes", "lower"),
            "pari.query_queries_per_second": metric(
                args.queries / query_elapsed, "queries/second", "higher"
            ),
            "pari.signature_items_per_second": metric(
                args.items / pari_elapsed, "items/second", "higher"
            ),
            "semantic.self_recall": metric(self_recall, "ratio", "higher"),
            "semantic.signature_parity": metric(float(parity), "boolean", "neutral"),
        },
        "schema_version": 1,
    }
    args.output.write_text(
        json.dumps(report, indent=2, sort_keys=True) + "\n", encoding="utf-8"
    )
    print(f"wrote {args.output}")


if __name__ == "__main__":
    main()
