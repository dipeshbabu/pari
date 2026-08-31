#!/usr/bin/env python3
"""Fail when active workflows use mutable third-party Actions or images."""

from __future__ import annotations

import re
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]
ACTION = re.compile(r"\buses:\s*([^@\s]+)@([^\s#]+)")
IMAGE = re.compile(r"\bimage:\s*([^\s#]+)")
SHA = re.compile(r"[0-9a-f]{40}")
DIGEST = re.compile(r".+@sha256:[0-9a-f]{64}")


def pin_errors(workflows: Path) -> list[str]:
    errors: list[str] = []
    for path in sorted(workflows.glob("*.yml")):
        text = path.read_text(encoding="utf-8")
        for line_number, line in enumerate(text.splitlines(), 1):
            action = ACTION.search(line)
            if (
                action
                and not action.group(1).startswith("./")
                and not SHA.fullmatch(action.group(2))
            ):
                errors.append(
                    f"{path.name}:{line_number}: action is not pinned to a full SHA: {action.group(0)}"
                )
            image = IMAGE.search(line)
            if image and not DIGEST.fullmatch(image.group(1)):
                errors.append(
                    f"{path.name}:{line_number}: image is not pinned by digest: {image.group(1)}"
                )
    return errors


def main() -> int:
    errors = pin_errors(ROOT / ".github" / "workflows")
    for error in errors:
        print(error)
    return int(bool(errors))


if __name__ == "__main__":
    raise SystemExit(main())
