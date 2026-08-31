#!/usr/bin/env python3
"""Deterministic semantics-first evaluation of future similarity families."""

from __future__ import annotations

import argparse
import hashlib
import json
import sys
import time
from collections import Counter
from pathlib import Path
from typing import Any

ROOT = Path(__file__).resolve().parents[1]
sys.path.insert(0, str(ROOT))

from examples.code_workload import code_tokens


def weighted_jaccard(left: Counter[str], right: Counter[str]) -> float:
    keys = left.keys() | right.keys()
    denominator = sum(max(left[key], right[key]) for key in keys)
    return (
        1.0
        if denominator == 0
        else sum(min(left[key], right[key]) for key in keys) / denominator
    )


def binary_jaccard(left: Counter[str], right: Counter[str]) -> float:
    left_keys, right_keys = set(left), set(right)
    union = len(left_keys | right_keys)
    return 1.0 if union == 0 else len(left_keys & right_keys) / union


def simhash64(weights: Counter[str]) -> int:
    totals = [0] * 64
    for token, weight in weights.items():
        value = int.from_bytes(hashlib.sha256(token.encode()).digest()[:8], "little")
        for bit in range(64):
            totals[bit] += weight if value & (1 << bit) else -weight
    return sum(1 << bit for bit, total in enumerate(totals) if total >= 0)


def hamming_similarity(left: int, right: int) -> float:
    return 1.0 - (left ^ right).bit_count() / 64


def evaluate() -> dict[str, Any]:
    weighted = [
        (
            "same-support-different-frequency",
            Counter({"view": 100, "buy": 5}),
            Counter({"view": 10, "buy": 5}),
        ),
        (
            "similar-frequency",
            Counter({"view": 100, "buy": 5}),
            Counter({"view": 95, "buy": 5}),
        ),
        (
            "different-support",
            Counter({"view": 100, "buy": 5}),
            Counter({"search": 80, "click": 4}),
        ),
    ]
    started = time.perf_counter()
    weighted_rows = [
        {
            "case": name,
            "binary_jaccard": binary_jaccard(left, right),
            "weighted_jaccard": weighted_jaccard(left, right),
        }
        for name, left, right in weighted
    ]
    weighted_elapsed = time.perf_counter() - started

    root = ROOT / "examples" / "code_corpus_fixture"
    files = [
        ("checksum-a", root / "repo-alpha/src/checksum.py", "checksum"),
        ("checksum-b", root / "repo-beta/lib/checksum_copy.py", "checksum"),
        ("clamp", root / "repo-beta/src/clamp.rs", "clamp"),
    ]
    started = time.perf_counter()
    fingerprints = {
        name: simhash64(Counter(code_tokens(path.read_text(encoding="utf-8"))))
        for name, path, _label in files
    }
    pairs = []
    for left_index, (left, _path, left_label) in enumerate(files):
        for right, _path, right_label in files[left_index + 1 :]:
            pairs.append(
                {
                    "left": left,
                    "right": right,
                    "same_label": left_label == right_label,
                    "simhash_similarity": hamming_similarity(
                        fingerprints[left], fingerprints[right]
                    ),
                }
            )
    simhash_elapsed = time.perf_counter() - started
    return {
        "schema_version": 1,
        "weighted_frequency_workload": {
            "cases": weighted_rows,
            "elapsed_seconds": weighted_elapsed,
            "finding": "binary MinHash cannot distinguish equal support with different frequencies",
        },
        "simhash_code_workload": {
            "signature_bytes_per_item": 8,
            "pairs": pairs,
            "elapsed_seconds": simhash_elapsed,
            "finding": "fingerprints are compact, but require a Hamming index and cosine-oriented threshold calibration",
        },
    }


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--output", type=Path, required=True)
    args = parser.parse_args()
    args.output.write_text(json.dumps(evaluate(), indent=2, sort_keys=True) + "\n")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
