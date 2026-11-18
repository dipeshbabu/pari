"""Pari approximate similarity search.

The public API intentionally stays small: deduplicate records directly, or
build :class:`MinHash` signatures and query a persistent :class:`Index`.
"""

from ._native import (
    ClosedIndexError,
    CompatibilityError,
    ConfigurationError,
    DuplicateKeyError,
    Index,
    Index64,
    IndexStats,
    LshPlan,
    MinHash,
    MinHash64,
    PariError,
    ReadOnlyIndex,
    StorageError,
    __version__,
    plan_lsh,
)
from .dedupe import (
    DedupeError,
    DedupeIndex,
    DeduplicationResult,
    DuplicateGroup,
    InvalidRepresentativeError,
    ProgressCancelledError,
    ProgressEvent,
    deduplicate,
)
from .features import shingles

__all__ = [
    "ClosedIndexError",
    "CompatibilityError",
    "ConfigurationError",
    "DedupeError",
    "DedupeIndex",
    "DeduplicationResult",
    "DuplicateGroup",
    "DuplicateKeyError",
    "Index",
    "Index64",
    "IndexStats",
    "InvalidRepresentativeError",
    "LshPlan",
    "MinHash",
    "MinHash64",
    "PariError",
    "ProgressCancelledError",
    "ProgressEvent",
    "ReadOnlyIndex",
    "StorageError",
    "__version__",
    "deduplicate",
    "plan_lsh",
    "shingles",
]
