from __future__ import annotations

import importlib.util
import json
import sys
import tempfile
import unittest
from pathlib import Path

from pari import CompatibilityError, Index, MinHash
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

    def test_affine64_fixture_matches_but_python_index_rejects_import(self) -> None:
        expected = FIXTURE["schemes"]["affine64"]
        external = DatasketchMinHash(
            num_perm=FIXTURE["num_perm"],
            seed=FIXTURE["seed"],
            scheme="affine64",
            permutations=(
                np.asarray(expected["multipliers"], dtype=np.uint64),
                np.asarray(expected["offsets"], dtype=np.uint64),
            ),
        )
        external.update_batch(VALUES)
        self.assertEqual(
            [int(value) for value in external.hashvalues],
            expected["pari_compatible_signature"],
        )
        with self.assertRaisesRegex(CompatibilityError, "current Python Index"):
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


if __name__ == "__main__":
    unittest.main()
