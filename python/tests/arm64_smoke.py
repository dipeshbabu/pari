from __future__ import annotations

import json
import platform
import tempfile
from pathlib import Path

from pari import DedupeIndex, Index, MinHash, __version__


def features(value: str) -> list[bytes]:
    return [word.encode("utf-8") for word in value.split()]


def main() -> None:
    machine = platform.machine().lower()
    if machine not in {"aarch64", "arm64"}:
        raise SystemExit(f"native arm64 execution required, got {machine!r}")

    signature = MinHash.from_values(
        [b"arm64", b"pari", b"release"], num_perm=32, seed=7
    )
    if len(signature) != 32:
        raise AssertionError("MinHash smoke returned the wrong signature width")

    with DedupeIndex[str](
        feature=features,
        threshold=0.8,
        num_perm=32,
        seed=7,
        backend="memory",
    ) as index:
        index.add_many(["arm64 pari release", "arm64 pari release", "unique record"])
        groups = index.groups()
        if len(groups) != 1 or groups[0].member_indices != (0, 1):
            raise AssertionError(f"unexpected in-memory grouping result: {groups!r}")

    with tempfile.TemporaryDirectory(prefix="pari-arm64-smoke-") as temporary:
        path = Path(temporary) / "index.pari"
        with Index.create(path, threshold=0.8, num_perm=32, seed=7) as index:
            index.add(1, signature)
            index.sync()
        with Index.open(path) as reopened:
            if reopened.search(signature) != [1]:
                raise AssertionError("persistent create/reopen query did not round-trip")

    print(
        json.dumps(
            {
                "architecture": machine,
                "pari_version": __version__,
                "python": platform.python_version(),
                "smoke": "passed",
            },
            sort_keys=True,
        )
    )


if __name__ == "__main__":
    main()
