"""Optional, exact Datasketch 2.x MinHash interoperability.

This module deliberately imports Datasketch only when an adapter function is
called. Core ``import pari`` remains independent of NumPy, SciPy, and
Datasketch.
"""

from __future__ import annotations

from functools import lru_cache
from importlib import import_module, metadata
from typing import Any

from ._native import CompatibilityError, MinHash, MinHash64

_MAX_U32 = (1 << 32) - 1
_MAX_U64 = (1 << 64) - 1


def datasketch_version() -> str:
    """Return the installed supported Datasketch version."""

    _datasketch, _numpy, version = _load_dependency()
    return version


def to_datasketch(sketch: MinHash | MinHash64) -> Any:
    """Create an update-compatible Datasketch affine MinHash.

    The returned object receives Pari's exact multiplier and offset arrays, so
    existing values and future updates remain value-for-value compatible.
    """

    datasketch, numpy, _version = _load_dependency()
    if isinstance(sketch, MinHash):
        scheme = "affine32"
        dtype = numpy.uint32
    elif isinstance(sketch, MinHash64):
        scheme = "affine64"
        dtype = numpy.uint64
    else:
        raise TypeError("expected a Pari MinHash or MinHash64 sketch")
    multipliers, offsets = sketch.permutations
    return datasketch.MinHash(
        num_perm=sketch.num_perm,
        seed=sketch.seed,
        scheme=scheme,
        hashvalues=numpy.asarray(sketch.signature, dtype=dtype),
        permutations=(
            numpy.asarray(multipliers, dtype=dtype),
            numpy.asarray(offsets, dtype=dtype),
        ),
    )


def from_datasketch(sketch: Any) -> MinHash | MinHash64:
    """Import an exactly compatible Datasketch affine32 or affine64 MinHash.

    Equal seeds are insufficient: Datasketch and Pari use different default
    random-number generators. This function rejects sketches unless both
    affine permutation arrays match Pari's stable seed mapping exactly.
    """

    _datasketch, numpy, _version = _load_dependency()
    try:
        scheme = str(sketch.scheme)
        seed = int(sketch.seed)
        num_perm = int(sketch.num_perm)
        hashfunc = sketch.hashfunc
        multipliers, offsets = sketch.permutations
        hashvalues = sketch.hashvalues
        signature = [int(value) for value in hashvalues]
    except (AttributeError, OverflowError, TypeError, ValueError) as error:
        raise TypeError("expected a Datasketch MinHash-like object") from error

    if scheme not in {"affine32", "affine64"}:
        raise CompatibilityError(
            f"Datasketch scheme {scheme!r} is not compatible with a supported Pari affine scheme"
        )
    width = 32 if scheme == "affine32" else 64
    pari_scheme = f"pari-affine{width}-v1"
    hashfunc_name = f"sha1_hash{width}"
    expected_hashfunc = getattr(import_module("datasketch.hashfunc"), hashfunc_name)
    if hashfunc is not expected_hashfunc:
        raise CompatibilityError(
            f"Datasketch custom hash functions are not update-compatible with {pari_scheme}'s "
            f"SHA-1 {width}-bit input hash"
        )
    if not 0 <= seed <= _MAX_U64:
        raise CompatibilityError(f"Datasketch seed is outside Pari's u64 range: {seed}")
    if num_perm <= 0 or len(signature) != num_perm:
        raise CompatibilityError("Datasketch signature length does not match num_perm")
    maximum = _MAX_U32 if scheme == "affine32" else _MAX_U64
    if any(not 0 <= value <= maximum for value in signature):
        raise CompatibilityError(
            f"Datasketch {scheme} signature contains non-u{width} values"
        )
    expected_dtype = numpy.dtype(numpy.uint32 if width == 32 else numpy.uint64)
    if getattr(hashvalues, "dtype", None) != expected_dtype:
        raise CompatibilityError(
            f"Datasketch {scheme} signature storage does not match u{width} width"
        )

    try:
        actual_multipliers = tuple(int(value) for value in multipliers)
        actual_offsets = tuple(int(value) for value in offsets)
    except (OverflowError, TypeError, ValueError) as error:
        raise TypeError("expected numeric Datasketch permutation arrays") from error
    if len(actual_multipliers) != num_perm or len(actual_offsets) != num_perm:
        raise CompatibilityError(
            "Datasketch permutation array lengths do not match num_perm"
        )
    if any(
        not 0 <= value <= maximum for value in (*actual_multipliers, *actual_offsets)
    ):
        raise CompatibilityError(
            f"Datasketch {scheme} permutation arrays contain non-u{width} values"
        )
    if (
        getattr(multipliers, "dtype", None) != expected_dtype
        or getattr(offsets, "dtype", None) != expected_dtype
    ):
        raise CompatibilityError(
            f"Datasketch {scheme} permutation storage does not match u{width} width"
        )

    expected_multipliers, expected_offsets = _expected_permutations(
        scheme, seed, num_perm
    )
    if actual_multipliers != expected_multipliers or actual_offsets != expected_offsets:
        raise CompatibilityError(
            "Datasketch permutation arrays do not match Pari's stable seed mapping; "
            "equal seeds alone are not signature-compatible"
        )
    sketch_type = MinHash if scheme == "affine32" else MinHash64
    return sketch_type.from_signature(signature, seed=seed)


def is_compatible(sketch: Any) -> bool:
    """Return whether ``from_datasketch`` can import this sketch exactly."""

    try:
        from_datasketch(sketch)
    except (CompatibilityError, ImportError, OverflowError, TypeError):
        return False
    return True


@lru_cache(maxsize=64)
def _expected_permutations(
    scheme: str, seed: int, num_perm: int
) -> tuple[tuple[int, ...], tuple[int, ...]]:
    sketch_type = MinHash if scheme == "affine32" else MinHash64
    expected = sketch_type(num_perm=num_perm, seed=seed)
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
