"""Failure-safe output staging for packaged reference workloads."""

from __future__ import annotations

import os
import uuid
from contextlib import AbstractContextManager
from pathlib import Path
from typing import Any


class StagedOutputs(AbstractContextManager["StagedOutputs"]):
    """Temporarily redirect namespace path attributes to sibling staging files."""

    def __init__(self, namespace: Any, attributes: tuple[str, ...]) -> None:
        self._namespace = namespace
        self._original: dict[str, Path] = {}
        self._staged: dict[str, Path] = {}
        nonce = f"{os.getpid()}-{uuid.uuid4().hex}"
        for attribute in attributes:
            value = getattr(namespace, attribute)
            if value is None:
                continue
            final = Path(value)
            stage = final.with_name(f".{final.name}.pari-workload-{nonce}.tmp")
            self._original[attribute] = final
            self._staged[attribute] = stage

    def __enter__(self) -> StagedOutputs:  # noqa: PYI034
        finals = [path.resolve() for path in self._original.values()]
        if len(set(finals)) != len(finals):
            raise ValueError("output paths must be distinct")
        existing = [str(path) for path in finals if path.exists()]
        if existing:
            raise FileExistsError(f"refusing to overwrite existing outputs: {existing}")
        for path in self._original.values():
            path.parent.mkdir(parents=True, exist_ok=True)
        for attribute, path in self._staged.items():
            setattr(self._namespace, attribute, path)
        return self

    def __exit__(self, exc_type: object, exc_value: object, traceback: object) -> bool:
        for attribute, path in self._original.items():
            setattr(self._namespace, attribute, path)
        if exc_value is not None:
            self._cleanup(exc_value)
            return False

        published: list[Path] = []
        try:
            missing = [str(path) for path in self._staged.values() if not path.exists()]
            if missing:
                raise RuntimeError(f"workflow did not create staged outputs: {missing}")
            for attribute, stage in self._staged.items():
                final = self._original[attribute]
                if final.exists():
                    raise FileExistsError(
                        f"refusing to overwrite output created during run: {final}"
                    )
                stage.rename(final)
                published.append(final)
        except BaseException as error:
            self._cleanup(error, published)
            raise
        return False

    def _cleanup(
        self, original: BaseException, published: list[Path] | None = None
    ) -> None:
        errors: list[OSError] = []
        targets = [*self._staged.values(), *(published or [])]
        targets.extend(Path(f"{path}.tmp") for path in self._staged.values())
        for path in targets:
            try:
                path.unlink(missing_ok=True)
            except OSError as error:
                errors.append(error)
        if errors:
            details = "; ".join(str(error) for error in errors)
            raise RuntimeError(
                f"failed to clean staged workload outputs: {details}"
            ) from original
