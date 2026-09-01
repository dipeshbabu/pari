from __future__ import annotations

import importlib.util
import json
import runpy
import sys
import tempfile
import unittest
from pathlib import Path

from pari import CompatibilityError, Index, Index64, MinHash, MinHash64
from pari import datasketch as adapter

DATASKETCH_LOADED_BY_ADAPTER_IMPORT = "datasketch" in sys.modules
try:
    import numpy as np
    from datasketch import MinHash as DatasketchMinHash
except ImportError:
    np = None
    DatasketchMinHash = None

ROOT = Path(__file__).resolve().parents[2]
FIXTURE = json.loads(
    (
        ROOT / "crates" / "pari-core" / "testdata" / "datasketch_v2_affine.json"
    ).read_text(encoding="utf-8")
)
VALUES = [bytes.fromhex(value) for value in FIXTURE["values_hex"]]


class OptionalDependencyTests(unittest.TestCase):
    def test_importing_adapter_does_not_import_datasketch(self) -> None:
        self.assertFalse(DATASKETCH_LOADED_BY_ADAPTER_IMPORT)

    @unittest.skipUnless(DatasketchMinHash is None, "Datasketch is installed")
    def test_call_without_optional_dependency_has_actionable_error(self) -> None:
        with self.assertRaisesRegex(ImportError, "datasketch>=2,<3"):
            adapter.datasketch_version()


@unittest.skipUnless(DatasketchMinHash is not None, "Datasketch 2.x is not installed")
class DatasketchInteropTests(unittest.TestCase):
    def test_checked_in_fixture_is_reproducible(self) -> None:
        script = ROOT / "scripts" / "generate_datasketch_v2_golden.py"
        spec = importlib.util.spec_from_file_location(
            "datasketch_fixture_generator", script
        )
        if spec is None or spec.loader is None:
            self.fail("could not load Datasketch fixture generator")
        generator = importlib.util.module_from_spec(spec)
        spec.loader.exec_module(generator)
        self.assertEqual(generator.generate(), FIXTURE)

    def test_benchmark_refuses_mismatched_signatures(self) -> None:
        benchmark = runpy.run_path(str(ROOT / "benchmarks" / "datasketch_interop.py"))
        native = MinHash64.from_values(VALUES, num_perm=8, seed=42)
        external = adapter.to_datasketch(native)
        external.hashvalues[0] ^= np.uint64(1)
        with self.assertRaisesRegex(RuntimeError, "signature parity failed"):
            benchmark["require_signature_parity"]([native], [external], "affine64-test")

    def test_affine32_round_trip_stays_update_compatible(self) -> None:
        pari = MinHash.from_values(
            VALUES, num_perm=FIXTURE["num_perm"], seed=FIXTURE["seed"]
        )
        external = adapter.to_datasketch(pari)
        expected = FIXTURE["schemes"]["affine32"]
        self.assertEqual(adapter.datasketch_version(), "2.0.0")
        self.assertEqual(external.scheme, "affine32")
        self.assertEqual(
            [int(value) for value in external.hashvalues],
            expected["pari_compatible_signature"],
        )
        self.assertEqual(
            [int(value) for value in external.permutations[0]],
            expected["multipliers"],
        )
        self.assertTrue(adapter.is_compatible(external))

        imported = adapter.from_datasketch(external)
        self.assertEqual(imported.signature, pari.signature)
        external.update(b"d")
        imported.update(b"d")
        self.assertEqual(
            imported.signature, [int(value) for value in external.hashvalues]
        )

    def test_default_equal_seed_is_rejected(self) -> None:
        external = DatasketchMinHash(
            num_perm=FIXTURE["num_perm"],
            seed=FIXTURE["seed"],
            scheme="affine32",
        )
        external.update_batch(VALUES)
        self.assertEqual(
            [int(value) for value in external.hashvalues],
            FIXTURE["schemes"]["affine32"]["datasketch_default_signature"],
        )
        self.assertFalse(adapter.is_compatible(external))
        with self.assertRaisesRegex(CompatibilityError, "equal seeds alone"):
            adapter.from_datasketch(external)

    def test_custom_hash_function_is_rejected(self) -> None:
        external = adapter.to_datasketch(
            MinHash.from_values(
                VALUES, num_perm=FIXTURE["num_perm"], seed=FIXTURE["seed"]
            )
        )
        external.hashfunc = lambda _value: 1
        with self.assertRaisesRegex(CompatibilityError, "custom hash functions"):
            adapter.from_datasketch(external)

    def test_affine64_round_trip_preserves_upper_bits_update_and_merge(self) -> None:
        expected = FIXTURE["schemes"]["affine64"]
        pari = MinHash64.from_values(
            VALUES, num_perm=FIXTURE["num_perm"], seed=FIXTURE["seed"]
        )
        external = adapter.to_datasketch(pari)
        self.assertEqual(external.scheme, "affine64")
        self.assertEqual(external.hashvalues.dtype, np.dtype(np.uint64))
        self.assertEqual(
            [int(value) for value in external.hashvalues],
            expected["pari_compatible_signature"],
        )
        self.assertEqual(
            [int(value) for value in external.permutations[0]],
            expected["multipliers"],
        )
        self.assertTrue(
            any(value > 2**32 - 1 for value in expected["pari_compatible_signature"])
        )
        self.assertTrue(adapter.is_compatible(external))

        imported = adapter.from_datasketch(external)
        self.assertIsInstance(imported, MinHash64)
        self.assertEqual(imported.signature, pari.signature)

        external.update(b"d")
        imported.update(b"d")
        self.assertEqual(
            imported.signature, [int(value) for value in external.hashvalues]
        )

        other = MinHash64.from_values(
            [b"d", b"e"], num_perm=FIXTURE["num_perm"], seed=FIXTURE["seed"]
        )
        external.merge(adapter.to_datasketch(other))
        imported.merge(other)
        self.assertEqual(
            imported.signature, [int(value) for value in external.hashvalues]
        )

    def test_affine64_default_equal_seed_is_rejected(self) -> None:
        external = DatasketchMinHash(
            num_perm=FIXTURE["num_perm"],
            seed=FIXTURE["seed"],
            scheme="affine64",
        )
        external.update_batch(VALUES)
        self.assertEqual(
            [int(value) for value in external.hashvalues],
            FIXTURE["schemes"]["affine64"]["datasketch_default_signature"],
        )
        self.assertFalse(adapter.is_compatible(external))
        with self.assertRaisesRegex(CompatibilityError, "equal seeds alone"):
            adapter.from_datasketch(external)

    def test_legacy_scheme_is_rejected(self) -> None:
        legacy = DatasketchMinHash(
            num_perm=FIXTURE["num_perm"],
            seed=FIXTURE["seed"],
            scheme="legacy",
        )
        legacy.update_batch(VALUES)
        with self.assertRaisesRegex(CompatibilityError, "legacy"):
            adapter.from_datasketch(legacy)

    def test_compatible_datasketch_signatures_index_and_query_in_pari(self) -> None:
        first = MinHash.from_values([b"alpha", b"beta"], num_perm=32, seed=7)
        second = MinHash.from_values([b"red", b"green"], num_perm=32, seed=7)
        external_first = adapter.to_datasketch(first)
        external_second = adapter.to_datasketch(second)
        imported_first = adapter.from_datasketch(external_first)
        imported_second = adapter.from_datasketch(external_second)

        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / "interop.pari"
            index = Index.create(path, threshold=0.8, num_perm=32, seed=7)
            try:
                index.add_many([(1, imported_first), (2, imported_second)])
                self.assertIn(1, index.search(adapter.from_datasketch(external_first)))
                self.assertNotIn(
                    2, index.search(adapter.from_datasketch(external_first))
                )
            finally:
                index.close()

    def test_affine64_import_persists_reopens_and_queries_in_index64(self) -> None:
        first = MinHash64.from_values([b"alpha", b"beta"], num_perm=32, seed=7)
        second = MinHash64.from_values([b"red", b"green"], num_perm=32, seed=7)
        external_first = adapter.to_datasketch(first)
        imported_first = adapter.from_datasketch(external_first)
        imported_second = adapter.from_datasketch(adapter.to_datasketch(second))

        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / "interop64.pari"
            with Index64.create(path, threshold=0.8, num_perm=32, seed=7) as index:
                index.add_many([(1, imported_first), (2, imported_second)])
                self.assertEqual(index.search(imported_first), [1])
                index.sync()

            with Index64.open(path) as reopened:
                query = adapter.from_datasketch(external_first)
                self.assertEqual(reopened.search(query), [1])

    def test_affine64_mismatches_fail_before_index_mutation(self) -> None:
        baseline = MinHash64.from_values(VALUES, num_perm=8, seed=42)

        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / "atomic-rejection.pari"
            with Index64.create(path, threshold=0.8, num_perm=8, seed=42) as index:
                index.add(1, baseline)

                bad_hash = adapter.to_datasketch(baseline)
                bad_hash.hashfunc = lambda _value: 1

                bad_seed = adapter.to_datasketch(baseline)
                bad_seed.seed += 1

                bad_count = adapter.to_datasketch(baseline)
                bad_count.num_perm += 1

                bad_permutations = adapter.to_datasketch(baseline)
                bad_permutations.permutations[0][0] ^= np.uint64(2)

                bad_signature = adapter.to_datasketch(baseline)
                bad_signature.hashvalues = [-1, *bad_signature.hashvalues[1:]]

                bad_width = adapter.to_datasketch(baseline)
                bad_width.hashvalues = bad_width.hashvalues.astype(np.uint32)

                cases = (
                    (bad_hash, "custom hash functions"),
                    (bad_seed, "equal seeds alone"),
                    (bad_count, "length does not match num_perm"),
                    (bad_permutations, "equal seeds alone"),
                    (bad_signature, "non-u64"),
                    (bad_width, "does not match u64 width"),
                )
                for incompatible, message in cases:
                    with self.subTest(message=message):
                        with self.assertRaisesRegex(CompatibilityError, message):
                            adapter.from_datasketch(incompatible)
                        self.assertEqual(len(index), 1)
                        self.assertTrue(index.contains(1))

                affine32 = adapter.from_datasketch(
                    adapter.to_datasketch(
                        MinHash.from_values(VALUES, num_perm=8, seed=42)
                    )
                )
                with self.assertRaises(CompatibilityError):
                    index.add(2, affine32)
                self.assertEqual(len(index), 1)
                self.assertFalse(index.contains(2))

    def test_non_finite_external_numbers_are_incompatible(self) -> None:
        baseline = MinHash64.from_values(VALUES, num_perm=8, seed=42)
        external = adapter.to_datasketch(baseline)
        external.seed = float("inf")

        self.assertFalse(adapter.is_compatible(external))
        with self.assertRaisesRegex(TypeError, "MinHash-like"):
            adapter.from_datasketch(external)


if __name__ == "__main__":
    unittest.main()
