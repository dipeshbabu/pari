#!/usr/bin/env python3
"""Benchmark exact affine32 and affine64 Datasketch-to-Pari migration."""

from __future__ import annotations

import argparse
import json
import os
import platform
import tempfile
import time
from dataclasses import dataclass
from importlib import metadata
from pathlib import Path
from typing import Any

import numpy as np
from datasketch import MinHash as DatasketchMinHash
from pari import Index, Index64, MinHash, MinHash64, __version__
from pari import datasketch as adapter

MASK64 = (1 << 64) - 1


@dataclass(frozen=True)
class WidthResult:
    metrics: dict[str, dict[str, object]]
    candidates: list[list[int]]


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
    rows: list[list[bytes]],
    num_perm: int,
    seed: int,
    scheme: str,
    sketch_type: Any,
    dtype: Any,
) -> list[DatasketchMinHash]:
    template = sketch_type(num_perm=num_perm, seed=seed)
    multipliers, offsets = template.permutations
    permutations = (
        np.asarray(multipliers, dtype=dtype),
        np.asarray(offsets, dtype=dtype),
    )
    output = []
    for row in rows:
        sketch = DatasketchMinHash(
            num_perm=num_perm,
            seed=seed,
            scheme=scheme,
            permutations=permutations,
        )
        sketch.update_batch(row)
        output.append(sketch)
    return output


def require_signature_parity(
    native: list[Any], external: list[DatasketchMinHash], scheme: str
) -> None:
    parity = len(native) == len(external) and all(
        sketch.signature == [int(value) for value in external_sketch.hashvalues]
        for sketch, external_sketch in zip(native, external, strict=True)
    )
    if not parity:
        raise RuntimeError(
            f"{scheme} signature parity failed before benchmark comparison"
        )


def benchmark_width(
    *,
    width: int,
    rows: list[list[bytes]],
    query_rows: list[list[bytes]],
    num_perm: int,
    seed: int,
    threshold: float,
    directory: Path,
) -> WidthResult:
    scheme = f"affine{width}"
    sketch_type = MinHash if width == 32 else MinHash64
    index_type = Index if width == 32 else Index64
    dtype = np.uint32 if width == 32 else np.uint64

    started = time.perf_counter()
    external = datasketch_signatures(rows, num_perm, seed, scheme, sketch_type, dtype)
    datasketch_elapsed = time.perf_counter() - started

    started = time.perf_counter()
    native = sketch_type.from_batch(rows, num_perm=num_perm, seed=seed)
    pari_elapsed = time.perf_counter() - started
    require_signature_parity(native, external, scheme)

    started = time.perf_counter()
    imported = [adapter.from_datasketch(sketch) for sketch in external]
    import_elapsed = time.perf_counter() - started
    require_signature_parity(imported, external, f"imported-{scheme}")

    external_queries = datasketch_signatures(
        query_rows, num_perm, seed, scheme, sketch_type, dtype
    )
    native_queries = sketch_type.from_batch(query_rows, num_perm=num_perm, seed=seed)
    require_signature_parity(native_queries, external_queries, f"query-{scheme}")
    imported_queries = [adapter.from_datasketch(sketch) for sketch in external_queries]

    path = directory / f"migration-{scheme}.pari"
    with index_type.create(
        path,
        threshold=threshold,
        num_perm=num_perm,
        seed=seed,
    ) as index:
        started = time.perf_counter()
        index.add_many(list(enumerate(imported)))
        index.sync()
        build_elapsed = time.perf_counter() - started

        started = time.perf_counter()
        candidates = index.search_many(imported_queries)
        query_elapsed = time.perf_counter() - started
        index_bytes = index.stats().file_bytes

    self_matches = sum(
        int(query_index in result) for query_index, result in enumerate(candidates)
    )
    self_recall = self_matches / len(query_rows)
    if self_recall != 1.0:
        raise RuntimeError(f"{scheme} converted self-recall failed: {self_recall}")

    items = len(rows)
    queries = len(query_rows)
    signature_bytes = items * num_perm * (width // 8)
    metrics = {
        "adapter_import_items_per_second": metric(
            items / import_elapsed, "items/second", "higher"
        ),
        "datasketch_signature_items_per_second": metric(
            items / datasketch_elapsed, "items/second", "higher"
        ),
        "index_build_items_per_second": metric(
            items / build_elapsed, "items/second", "higher"
        ),
        "index_bytes": metric(index_bytes, "bytes", "lower"),
        "query_queries_per_second": metric(
            queries / query_elapsed, "queries/second", "higher"
        ),
        "signature_bytes": metric(signature_bytes, "bytes", "lower"),
        "signature_bytes_per_item": metric(
            num_perm * (width // 8), "bytes/item", "lower"
        ),
        "signature_items_per_second": metric(
            items / pari_elapsed, "items/second", "higher"
        ),
        "self_recall": metric(self_recall, "ratio", "higher"),
        "signature_parity": metric(1.0, "boolean", "neutral"),
    }
    return WidthResult(metrics=metrics, candidates=candidates)


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
    with tempfile.TemporaryDirectory() as raw_directory:
        directory = Path(raw_directory)
        affine32 = benchmark_width(
            width=32,
            rows=rows,
            query_rows=query_rows,
            num_perm=args.num_perm,
            seed=args.seed,
            threshold=args.threshold,
            directory=directory,
        )
        affine64 = benchmark_width(
            width=64,
            rows=rows,
            query_rows=query_rows,
            num_perm=args.num_perm,
            seed=args.seed,
            threshold=args.threshold,
            directory=directory,
        )

    candidate_parity = affine32.candidates == affine64.candidates
    if not candidate_parity:
        raise RuntimeError(
            "Pari affine32/affine64 candidate parity failed for the matched workload"
        )

    metrics = {f"affine32.{name}": value for name, value in affine32.metrics.items()}
    metrics.update(
        {f"affine64.{name}": value for name, value in affine64.metrics.items()}
    )
    metrics["semantic.candidate_parity"] = metric(
        float(candidate_parity), "boolean", "neutral"
    )

    report = {
        "config": {
            "candidate_parity_required": True,
            "datasketch_lsh_candidate_sets_comparable": False,
            "items": args.items,
            "num_perm": args.num_perm,
            "pari_width_candidate_sets_comparable": True,
            "queries": args.queries,
            "seed": args.seed,
            "set_size": args.set_size,
            "signature_semantics": {
                "affine32": "datasketch-affine32-with-pari-permutations",
                "affine64": "datasketch-affine64-with-pari-permutations",
            },
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
        "metrics": metrics,
        "schema_version": 2,
    }
    args.output.write_text(
        json.dumps(report, indent=2, sort_keys=True) + "\n", encoding="utf-8"
    )
    print(f"wrote {args.output}")


if __name__ == "__main__":
    main()
