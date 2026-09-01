from __future__ import annotations

import unittest
from importlib.resources import files

import pari


V02_PUBLIC_EXPORTS = {
    "ClosedIndexError",
    "CompatibilityError",
    "ConfigurationError",
    "DeduplicationResult",
    "DedupeError",
    "DedupeIndex",
    "DuplicateKeyError",
    "DuplicateGroup",
    "Index",
    "Index64",
    "IndexStats",
    "InvalidRepresentativeError",
    "MinHash",
    "MinHash64",
    "LshPlan",
    "PariError",
    "ProgressCancelledError",
    "ProgressEvent",
    "StorageError",
    "__version__",
    "deduplicate",
    "plan_lsh",
}


class PublicContractTests(unittest.TestCase):
    def test_v02_top_level_exports_are_pinned(self) -> None:
        self.assertEqual(set(pari.__all__), V02_PUBLIC_EXPORTS)
        for name in V02_PUBLIC_EXPORTS:
            self.assertTrue(hasattr(pari, name), name)

    def test_documented_exceptions_share_one_base_class(self) -> None:
        for error_type in (
            pari.ClosedIndexError,
            pari.CompatibilityError,
            pari.ConfigurationError,
            pari.DuplicateKeyError,
            pari.DedupeError,
            pari.InvalidRepresentativeError,
            pari.ProgressCancelledError,
            pari.StorageError,
        ):
            self.assertTrue(issubclass(error_type, pari.PariError), error_type.__name__)

    def test_affine64_stub_shape_is_shipped(self) -> None:
        stub = files(pari).joinpath("__init__.pyi").read_text(encoding="utf-8")
        for declaration in (
            "class MinHash64:",
            "def jaccard(self, other: MinHash64) -> float:",
            "class Index64:",
            "def add(self, key: int, sketch: MinHash64) -> None:",
            "def search(self, sketch: MinHash64) -> list[int]:",
        ):
            with self.subTest(declaration=declaration):
                self.assertIn(declaration, stub)


if __name__ == "__main__":
    unittest.main()
