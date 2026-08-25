"""Pari approximate similarity search.

The public API intentionally stays small: build :class:`MinHash` signatures,
store them in a persistent :class:`Index`, and query approximate candidates.
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

__all__ = [
    "ClosedIndexError",
    "CompatibilityError",
    "ConfigurationError",
    "DuplicateKeyError",
    "Index",
    "IndexStats",
    "MinHash",
    "PariError",
    "StorageError",
    "__version__",
]
