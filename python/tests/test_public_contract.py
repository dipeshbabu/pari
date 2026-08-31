from __future__ import annotations

import unittest

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
    "IndexStats",
    "InvalidRepresentativeError",
    "MinHash",
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


if __name__ == "__main__":
    unittest.main()
