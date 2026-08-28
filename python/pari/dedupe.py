"""High-level, batch-first dataset deduplication."""

from __future__ import annotations

from collections.abc import Callable, Iterable, Sequence
from dataclasses import dataclass
from itertools import islice
from os import PathLike
from typing import Generic, Literal, TypeVar

from ._native import (
    ClosedIndexError,
    ConfigurationError,
    PariError,
    _DedupeEngine,
)

T = TypeVar("T")
ReadableBuffer = bytes | bytearray | memoryview
FeatureExtractor = Callable[[T], Iterable[ReadableBuffer]]
ExactVerifier = Callable[[T, T], bool]
RepresentativeSelector = Callable[[Sequence[T]], T]
Backend = Literal["memory", "local"]
PathInput = str | PathLike[str]


class DedupeError(PariError):  # type: ignore[misc]
    """Base class for high-level deduplication errors."""


class InvalidRepresentativeError(DedupeError):
    """A representative callback returned an object outside its group."""


@dataclass(frozen=True, slots=True)
class DuplicateGroup(Generic[T]):
    """One deterministic connected component of duplicate records."""

    representative: T
    members: tuple[T, ...]
    representative_index: int
    member_indices: tuple[int, ...]


@dataclass(frozen=True, slots=True)
class DeduplicationResult(Generic[T]):
    """Duplicate groups plus deterministic input-order keep/drop partitions."""

    groups: tuple[DuplicateGroup[T], ...]
    kept: tuple[T, ...]
    dropped: tuple[T, ...]
    kept_indices: tuple[int, ...]
    dropped_indices: tuple[int, ...]

    @property
    def group_count(self) -> int:
        """Return the number of non-singleton duplicate groups."""

        return len(self.groups)

    @property
    def duplicate_count(self) -> int:
        """Return the number of records excluded by representative selection."""

        return len(self.dropped)


class DedupeIndex(Generic[T]):
    """Incrementally deduplicate records through Pari's native grouping path.

    Feature extraction runs in Python. Each bounded batch is then copied into
    Rust-owned memory before signature construction and index insertion run
    outside the GIL. Records are assigned stable keys in ingestion order.
    """

    __slots__ = (
        "_batch_size",
        "_engine",
        "_exact",
        "_feature",
        "_records",
        "_representative",
        "backend",
        "num_perm",
        "seed",
        "threshold",
    )

    def __init__(
        self,
        feature: FeatureExtractor[T] | None = None,
        *,
        threshold: float = 0.8,
        num_perm: int = 128,
        seed: int = 1,
        batch_size: int = 1024,
        path: PathInput | None = None,
        backend: Backend | None = None,
        exact: ExactVerifier[T] | None = None,
        representative: RepresentativeSelector[T] | None = None,
    ) -> None:
        if feature is not None and not callable(feature):
            raise ConfigurationError("feature must be callable or None")
        if (
            isinstance(batch_size, bool)
            or not isinstance(batch_size, int)
            or batch_size <= 0
        ):
            raise ConfigurationError("batch_size must be a positive integer")
        if exact is not None and not callable(exact):
            raise ConfigurationError("exact must be callable or None")
        if representative is not None and not callable(representative):
            raise ConfigurationError("representative must be callable or None")

        selected_backend: Backend = backend or (
            "local" if path is not None else "memory"
        )
        if selected_backend not in ("memory", "local"):
            raise ConfigurationError("backend must be 'memory' or 'local'")
        if selected_backend == "memory" and path is not None:
            raise ConfigurationError("path is only valid with the local backend")
        if selected_backend == "local" and path is None:
            raise ConfigurationError("the local backend requires path")

        self.threshold = threshold
        self.num_perm = num_perm
        self.seed = seed
        self.backend = selected_backend
        self._batch_size = batch_size
        self._feature = feature
        self._exact = exact
        self._representative = representative
        self._records: list[T] = []
        self._engine = _DedupeEngine(
            threshold=threshold,
            num_perm=num_perm,
            seed=seed,
            path=path,
        )

    @property
    def batch_size(self) -> int:
        """Return the maximum number of feature rows copied per native call."""

        return self._batch_size

    @property
    def closed(self) -> bool:
        """Return whether this handle has been closed."""

        return bool(self._engine.closed)

    def add(self, record: T) -> int:
        """Add one record and return its stable ingestion index."""

        feature = self._require_feature()
        return self.add_features(record, feature(record))

    def add_many(self, records: Iterable[T]) -> int:
        """Add records in bounded, atomic native batches and return the count."""

        feature = self._require_feature()
        return self.add_many_features((record, feature(record)) for record in records)

    def add_features(self, record: T, features: Iterable[ReadableBuffer]) -> int:
        """Add one record with precomputed features and return its stable index."""

        self._ensure_open()
        key = len(self._records)
        self._engine.add_many([key], [features])
        self._records.append(record)
        return key

    def add_many_features(
        self, items: Iterable[tuple[T, Iterable[ReadableBuffer]]]
    ) -> int:
        """Add precomputed feature rows without retaining source payloads."""

        self._ensure_open()
        iterator = iter(items)
        added = 0
        while True:
            batch = list(islice(iterator, self._batch_size))
            if not batch:
                return added

            start = len(self._records)
            keys = list(range(start, start + len(batch)))
            records = [record for record, _features in batch]
            feature_rows = [features for _record, features in batch]
            self._engine.add_many(keys, feature_rows)
            self._records.extend(records)
            added += len(batch)

    def candidate_groups(self) -> tuple[DuplicateGroup[T], ...]:
        """Return unverified LSH candidate groups for measurement or review."""

        self._ensure_open()
        raw_groups = self._engine.groups(verifier=None)
        return tuple(
            self._convert_group(member_indices) for _, member_indices in raw_groups
        )

    def groups(self) -> tuple[DuplicateGroup[T], ...]:
        """Return deterministic duplicate groups for the current records."""

        self._ensure_open()
        raw_groups = self._engine.groups(verifier=self._native_verifier())
        return tuple(
            self._convert_group(member_indices) for _, member_indices in raw_groups
        )

    def result(self) -> DeduplicationResult[T]:
        """Return groups and deterministic input-order keep/drop partitions."""

        groups = self.groups()
        dropped_indices = {
            member_index
            for group in groups
            for member_index in group.member_indices
            if member_index != group.representative_index
        }
        kept_index_tuple = tuple(
            index for index in range(len(self._records)) if index not in dropped_indices
        )
        dropped_index_tuple = tuple(sorted(dropped_indices))
        self.sync()
        return DeduplicationResult(
            groups=groups,
            kept=tuple(self._records[index] for index in kept_index_tuple),
            dropped=tuple(self._records[index] for index in dropped_index_tuple),
            kept_indices=kept_index_tuple,
            dropped_indices=dropped_index_tuple,
        )

    def sync(self) -> None:
        """Commit the optional local persistence mirror."""

        self._ensure_open()
        self._engine.sync()

    def close(self) -> None:
        """Sync the optional local backend and close this handle."""

        self._engine.close()

    def __len__(self) -> int:
        return len(self._records)

    def __enter__(self) -> DedupeIndex[T]:
        self._ensure_open()
        return self

    def __exit__(
        self, exc_type: object, exc_value: object, traceback: object
    ) -> Literal[False]:
        self.close()
        return False

    def _ensure_open(self) -> None:
        if self.closed:
            raise ClosedIndexError("dedupe index is closed")

    def _require_feature(self) -> FeatureExtractor[T]:
        if self._feature is None:
            raise ConfigurationError(
                "feature is required for add/add_many; use add_features/add_many_features"
            )
        return self._feature

    def _native_verifier(self) -> Callable[[int, int], bool] | None:
        if self._exact is None:
            return None
        records = self._records
        exact = self._exact

        def verify(left: int, right: int) -> bool:
            return bool(exact(records[left], records[right]))

        return verify

    def _convert_group(self, raw_indices: Sequence[int]) -> DuplicateGroup[T]:
        member_indices = tuple(raw_indices)
        members = tuple(self._records[index] for index in member_indices)
        representative_index = member_indices[0]
        representative = members[0]

        if self._representative is not None:
            selected = self._representative(members)
            for index, member in zip(member_indices, members):
                if selected is member:
                    representative_index = index
                    representative = member
                    break
            else:
                raise InvalidRepresentativeError(
                    "representative must return one of the group member objects"
                )

        return DuplicateGroup(
            representative=representative,
            members=members,
            representative_index=representative_index,
            member_indices=member_indices,
        )


def deduplicate(
    records: Iterable[T],
    *,
    feature: FeatureExtractor[T],
    threshold: float = 0.8,
    num_perm: int = 128,
    seed: int = 1,
    batch_size: int = 1024,
    path: PathInput | None = None,
    backend: Backend | None = None,
    exact: ExactVerifier[T] | None = None,
    representative: RepresentativeSelector[T] | None = None,
) -> DeduplicationResult[T]:
    """Deduplicate an iterable with a concise, typed batch-first API."""

    index = DedupeIndex(
        feature,
        threshold=threshold,
        num_perm=num_perm,
        seed=seed,
        batch_size=batch_size,
        path=path,
        backend=backend,
        exact=exact,
        representative=representative,
    )
    try:
        index.add_many(records)
        return index.result()
    finally:
        index.close()
