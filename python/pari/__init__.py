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
    IndexStats,
    MinHash,
    PariError,
    StorageError,
    __version__,
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
    "IndexStats",
    "InvalidRepresentativeError",
    "MinHash",
    "PariError",
    "ProgressCancelledError",
    "ProgressEvent",
    "StorageError",
    "__version__",
    "deduplicate",
]
