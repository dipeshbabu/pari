#!/usr/bin/env python3
"""Generate the checked-in Datasketch 2.x affine interoperability fixture."""

from __future__ import annotations

import argparse
import json
from importlib import metadata
from pathlib import Path

import numpy as np
from datasketch import MinHash

MASK64 = (1 << 64) - 1
SCHEMES = (("affine32", 32, np.uint32), ("affine64", 64, np.uint64))


class SplitMix64:
    def __init__(self, seed: int) -> None:
        self.state = seed

    def next_u64(self) -> int:
        self.state = (self.state + 0x9E3779B97F4A7C15) & MASK64
        value = self.state
        value = ((value ^ (value >> 30)) * 0xBF58476D1CE4E5B9) & MASK64
        value = ((value ^ (value >> 27)) * 0x94D049BB133111EB) & MASK64
        return (value ^ (value >> 31)) & MASK64


def pari_permutations(
    seed: int, num_perm: int, width: int, dtype: type[np.unsignedinteger]
) -> tuple[np.ndarray, np.ndarray]:
    generator = SplitMix64(seed)
    mask = (1 << width) - 1
    multipliers = np.asarray(
        [(generator.next_u64() & mask) | 1 for _ in range(num_perm)], dtype=dtype
    )
    offsets = np.asarray(
        [generator.next_u64() & mask for _ in range(num_perm)], dtype=dtype
    )
    return multipliers, offsets


def generate() -> dict[str, object]:
    seed = 42
    num_perm = 8
    values = [b"a", b"b", b"c"]
    schemes: dict[str, object] = {}
    for scheme, width, dtype in SCHEMES:
        permutations = pari_permutations(seed, num_perm, width, dtype)
        compatible = MinHash(
            num_perm=num_perm,
            seed=seed,
            scheme=scheme,
            permutations=permutations,
        )
        compatible.update_batch(values)
        default = MinHash(num_perm=num_perm, seed=seed, scheme=scheme)
        default.update_batch(values)
        schemes[scheme] = {
            "datasketch_default_signature": [
                int(value) for value in default.hashvalues
            ],
            "offsets": [int(value) for value in permutations[1]],
            "pari_compatible_signature": [
                int(value) for value in compatible.hashvalues
            ],
            "multipliers": [int(value) for value in permutations[0]],
            "width": width,
        }
    return {
        "datasketch_version": metadata.version("datasketch"),
        "num_perm": num_perm,
        "schema_version": 1,
        "seed": seed,
        "schemes": schemes,
        "values_hex": [value.hex() for value in values],
    }


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument(
        "--output",
        type=Path,
        default=Path("crates/pari-core/testdata/datasketch_v2_affine.json"),
    )
    args = parser.parse_args()
    args.output.parent.mkdir(parents=True, exist_ok=True)
    args.output.write_text(
        json.dumps(generate(), indent=2, sort_keys=True) + "\n",
        encoding="utf-8",
        newline="\n",
    )


if __name__ == "__main__":
    main()
