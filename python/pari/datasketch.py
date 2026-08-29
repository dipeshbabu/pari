"""Optional, exact Datasketch 2.x MinHash interoperability.

This module deliberately imports Datasketch only when an adapter function is
called. Core ``import pari`` remains independent of NumPy, SciPy, and
Datasketch.
"""

from __future__ import annotations

from functools import lru_cache
from importlib import import_module, metadata
from typing import Any

from ._native import CompatibilityError, MinHash

_MAX_U32 = (1 << 32) - 1
_MAX_U64 = (1 << 64) - 1


def datasketch_version() -> str:
    """Return the installed supported Datasketch version."""

    _datasketch, _numpy, version = _load_dependency()
    return version


def to_datasketch(sketch: MinHash) -> Any:
    """Create an update-compatible Datasketch affine32 MinHash.

    The returned object receives Pari's exact multiplier and offset arrays, so
    existing values and future updates remain value-for-value compatible.
    """

    datasketch, numpy, _version = _load_dependency()
    multipliers, offsets = sketch.permutations
    return datasketch.MinHash(
        num_perm=sketch.num_perm,
        seed=sketch.seed,
        scheme="affine32",
        hashvalues=numpy.asarray(sketch.signature, dtype=numpy.uint32),
        permutations=(
            numpy.asarray(multipliers, dtype=numpy.uint32),
            numpy.asarray(offsets, dtype=numpy.uint32),
        ),
    )


def from_datasketch(sketch: Any) -> MinHash:
    """Import an exactly compatible Datasketch affine32 MinHash.

    Equal seeds are insufficient: Datasketch and Pari use different default
    random-number generators. This function rejects sketches unless both
    affine permutation arrays match Pari's stable seed mapping exactly.
    """

    _datasketch, _numpy, _version = _load_dependency()
    try:
        scheme = str(sketch.scheme)
        seed = int(sketch.seed)
        num_perm = int(sketch.num_perm)
        hashfunc = sketch.hashfunc
        multipliers, offsets = sketch.permutations
        signature = [int(value) for value in sketch.hashvalues]
    except (AttributeError, TypeError, ValueError) as error:
        raise TypeError("expected a Datasketch MinHash-like object") from error

    if scheme != "affine32":
        if scheme == "affine64":
            raise CompatibilityError(
                "Datasketch affine64 matches Pari MinHash64 only when permutations match; "
                "the current Python Index accepts affine32 signatures"
            )
        raise CompatibilityError(
            f"Datasketch scheme {scheme!r} is not compatible with pari-affine32-v1"
        )
    expected_hashfunc = import_module("datasketch.hashfunc").sha1_hash32
    if hashfunc is not expected_hashfunc:
        raise CompatibilityError(
            "Datasketch custom hash functions are not update-compatible with Pari's SHA-1 input hash"
        )
    if not 0 <= seed <= _MAX_U64:
        raise CompatibilityError(f"Datasketch seed is outside Pari's u64 range: {seed}")
    if num_perm <= 0 or len(signature) != num_perm:
        raise CompatibilityError("Datasketch signature length does not match num_perm")
    if any(not 0 <= value <= _MAX_U32 for value in signature):
        raise CompatibilityError(
            "Datasketch affine32 signature contains non-u32 values"
        )

    expected_multipliers, expected_offsets = _expected_permutations(seed, num_perm)
    actual_multipliers = tuple(int(value) for value in multipliers)
    actual_offsets = tuple(int(value) for value in offsets)
    if actual_multipliers != expected_multipliers or actual_offsets != expected_offsets:
        raise CompatibilityError(
            "Datasketch permutation arrays do not match Pari's stable seed mapping; "
            "equal seeds alone are not signature-compatible"
        )
    return MinHash.from_signature(signature, seed=seed)


def is_compatible(sketch: Any) -> bool:
    """Return whether ``from_datasketch`` can import this sketch exactly."""

    try:
        from_datasketch(sketch)
    except (CompatibilityError, ImportError, TypeError):
        return False
    return True


@lru_cache(maxsize=64)
def _expected_permutations(
    seed: int, num_perm: int
) -> tuple[tuple[int, ...], tuple[int, ...]]:
    expected = MinHash(num_perm=num_perm, seed=seed)
    multipliers, offsets = expected.permutations
    return tuple(multipliers), tuple(offsets)


def _load_dependency() -> tuple[Any, Any, str]:
    try:
        datasketch = import_module("datasketch")
        numpy = import_module("numpy")
        version = metadata.version("datasketch")
    except (ImportError, metadata.PackageNotFoundError) as error:
        raise ImportError(
            "Datasketch interoperability is optional; install 'datasketch>=2,<3'"
        ) from error
    try:
        major = int(version.split(".", 1)[0])
    except ValueError as error:
        raise ImportError(
            f"could not interpret Datasketch version {version!r}"
        ) from error
    if major != 2:
        raise ImportError(
            f"Datasketch interoperability requires version 2.x, found {version}"
        )
    return datasketch, numpy, version
