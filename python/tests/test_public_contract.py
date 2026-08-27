from __future__ import annotations

import unittest

import pari


V01_PUBLIC_EXPORTS = {
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
}


class PublicContractTests(unittest.TestCase):
    def test_v01_top_level_exports_are_pinned(self) -> None:
        self.assertEqual(set(pari.__all__), V01_PUBLIC_EXPORTS)
        for name in V01_PUBLIC_EXPORTS:
            self.assertTrue(hasattr(pari, name), name)

    def test_documented_exceptions_share_one_base_class(self) -> None:
        for error_type in (
            pari.ClosedIndexError,
            pari.CompatibilityError,
            pari.ConfigurationError,
            pari.DuplicateKeyError,
            pari.StorageError,
        ):
            self.assertTrue(issubclass(error_type, pari.PariError), error_type.__name__)


if __name__ == "__main__":
    unittest.main()
