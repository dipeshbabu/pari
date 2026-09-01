from __future__ import annotations

import tempfile
import unittest
from pathlib import Path

from pari import (
    ClosedIndexError,
    CompatibilityError,
    ConfigurationError,
    DuplicateKeyError,
    Index,
    Index64,
    LshPlan,
    MinHash,
    MinHash64,
    plan_lsh,
)


def sketch(base: int, *, num_perm: int = 64, seed: int = 7) -> MinHash:
    values = [(value).to_bytes(8, "little") for value in range(base, base + 40)]
    return MinHash.from_values(values, num_perm=num_perm, seed=seed)


def sketch64(base: int, *, num_perm: int = 64, seed: int = 7) -> MinHash64:
    values = [(value).to_bytes(8, "little") for value in range(base, base + 40)]
    return MinHash64.from_values(values, num_perm=num_perm, seed=seed)


class MinHashTests(unittest.TestCase):
    def test_scalar_and_batch_updates_match(self) -> None:
        values = [b"alpha", bytearray(b"beta"), memoryview(b"gamma")]
        scalar = MinHash(num_perm=64, seed=7)
        for value in values:
            scalar.update(value)

        batch = MinHash(num_perm=64, seed=7)
        batch.update_many(values)

        self.assertEqual(scalar.signature, batch.signature)
        self.assertEqual(scalar.seed, 7)
        self.assertEqual(scalar.num_perm, 64)
        self.assertEqual(len(scalar), 64)
        self.assertEqual(scalar.scheme, "pari-affine32-v1")
        self.assertAlmostEqual(scalar.jaccard(batch), 1.0)

    def test_from_values_accepts_generic_byte_buffers(self) -> None:
        values = [memoryview(b"one"), bytearray(b"two"), b"three"]
        first = MinHash.from_values(values, num_perm=32, seed=11)
        second = MinHash(num_perm=32, seed=11)
        second.update_many(values)
        self.assertEqual(first.signature, second.signature)

    def test_from_batch_matches_scalar_construction(self) -> None:
        rows = [[b"alpha", b"beta"], [b"gamma"], []]
        batch = MinHash.from_batch(rows, num_perm=32, seed=11)
        scalar = [MinHash.from_values(row, num_perm=32, seed=11) for row in rows]
        self.assertEqual(
            [sketch.signature for sketch in batch],
            [sketch.signature for sketch in scalar],
        )

    def test_parallel_batch_is_ordered_and_deterministic(self) -> None:
        rows = [
            [f"row-{row}-value-{value}".encode() for value in range(8)]
            for row in range(512)
        ]
        expected = MinHash.from_batch(rows, num_perm=32, seed=11, threads=1)
        expected_signatures = [sketch.signature for sketch in expected]
        for threads in (2, 4, None):
            actual = MinHash.from_batch(
                rows, num_perm=32, seed=11, threads=threads
            )
            self.assertEqual(
                [sketch.signature for sketch in actual], expected_signatures
            )

        with self.assertRaises(ConfigurationError):
            MinHash.from_batch(rows, num_perm=32, seed=11, threads=0)

    def test_clear_and_merge(self) -> None:
        left = MinHash.from_values([b"a", b"b"], num_perm=32, seed=1)
        right = MinHash.from_values([b"b", b"c"], num_perm=32, seed=1)
        left.merge(right)
        self.assertFalse(left.is_empty)
        left.clear()
        self.assertTrue(left.is_empty)

    def test_incompatible_sketches_raise_stable_exception(self) -> None:
        left = MinHash(num_perm=32, seed=1)
        right = MinHash(num_perm=32, seed=2)
        with self.assertRaises(CompatibilityError):
            left.jaccard(right)


class MinHash64Tests(unittest.TestCase):
    def test_scalar_batch_update_and_golden_upper_bits(self) -> None:
        values = [b"a", bytearray(b"b"), memoryview(b"c")]
        scalar = MinHash64(num_perm=8, seed=42)
        for value in values:
            scalar.update(value)

        batch = MinHash64(num_perm=8, seed=42)
        batch.update_many(values)
        self.assertEqual(scalar.signature, batch.signature)
        self.assertEqual(
            scalar.signature,
            [
                398_824_617_996_340_472,
                4_985_036_841_737_875_763,
                430_169_245_876_064_069,
                4_830_488_362_799_227_617,
                9_007_712_658_965_612_972,
                6_350_542_169_249_656_984,
                14_245_705_141_267_314_417,
                3_755_630_306_339_185_935,
            ],
        )
        self.assertTrue(any(value > 2**32 - 1 for value in scalar.signature))
        self.assertTrue(any(value > 2**32 - 1 for value in scalar.permutations[0]))
        self.assertEqual(scalar.scheme, "pari-affine64-v1")
        self.assertEqual((scalar.seed, scalar.num_perm, len(scalar)), (42, 8, 8))
        self.assertAlmostEqual(scalar.jaccard(batch), 1.0)

    def test_from_values_batch_signature_merge_and_clear(self) -> None:
        rows = [[b"alpha", b"beta"], [b"gamma"], []]
        batched = MinHash64.from_batch(rows, num_perm=32, seed=11, threads=1)
        scalar = [MinHash64.from_values(row, num_perm=32, seed=11) for row in rows]
        self.assertEqual(
            [sketch.signature for sketch in batched],
            [sketch.signature for sketch in scalar],
        )

        reconstructed = MinHash64.from_signature(batched[0].signature, seed=11)
        self.assertEqual(reconstructed.signature, batched[0].signature)
        reconstructed.update(b"delta")
        batched[0].update(b"delta")
        self.assertEqual(reconstructed.signature, batched[0].signature)

        left = MinHash64.from_values([b"a", b"b"], num_perm=32, seed=1)
        right = MinHash64.from_values([b"b", b"c"], num_perm=32, seed=1)
        left.merge(right)
        self.assertFalse(left.is_empty)
        left.clear()
        self.assertTrue(left.is_empty)
        self.assertEqual(left.signature, [2**64 - 1] * 32)

    def test_parallel_batch_is_ordered_and_cross_width_is_explicit(self) -> None:
        rows = [
            [f"row-{row}-value-{value}".encode() for value in range(8)]
            for row in range(512)
        ]
        expected = MinHash64.from_batch(rows, num_perm=32, seed=11, threads=1)
        expected_signatures = [sketch.signature for sketch in expected]
        for threads in (2, 4, None):
            actual = MinHash64.from_batch(
                rows, num_perm=32, seed=11, threads=threads
            )
            self.assertEqual(
                [sketch.signature for sketch in actual], expected_signatures
            )
        with self.assertRaises(ConfigurationError):
            MinHash64.from_batch(rows, num_perm=32, seed=11, threads=0)

        affine32 = MinHash(num_perm=32, seed=11)
        affine64 = MinHash64(num_perm=32, seed=11)
        affine32_before = affine32.signature
        affine64_before = affine64.signature
        for operation in (
            lambda: affine32.jaccard(affine64),
            lambda: affine32.merge(affine64),
            lambda: affine64.jaccard(affine32),
            lambda: affine64.merge(affine32),
        ):
            with self.subTest(operation=operation):
                with self.assertRaises(CompatibilityError):
                    operation()
        self.assertEqual(affine32.signature, affine32_before)
        self.assertEqual(affine64.signature, affine64_before)


class PlannerTests(unittest.TestCase):
    def test_plan_is_deterministic_and_model_labeled(self) -> None:
        plan = plan_lsh(
            1_000_000,
            threshold=0.8,
            num_perm=128,
            memory_budget_bytes=2 * 1024**3,
            storage="auto",
        )
        self.assertIsInstance(plan, LshPlan)
        self.assertEqual(plan.model, "pari-lsh-planner-v1")
        self.assertIn("not a measured guarantee", plan.estimate_semantics)
        self.assertEqual((plan.bands, plan.rows), (9, 13))
        self.assertEqual(plan.parameter_source, "tuned")
        self.assertEqual(plan.signature_bytes_per_item, 512)
        self.assertEqual(plan.index_metadata_bytes_per_item, 152)
        self.assertEqual(plan.persistent_index_bytes, 440_000_736)
        self.assertEqual(plan.recommended_storage, "memory")
        self.assertTrue(plan.in_memory_fits_budget)
        self.assertAlmostEqual(
            plan.candidate_probability(0.8),
            plan.candidate_probability_at_threshold,
        )

    def test_invalid_plan_inputs_raise_configuration_error(self) -> None:
        for kwargs in (
            {"expected_items": 0},
            {"expected_items": 1, "memory_budget_bytes": 0},
            {"expected_items": 1, "storage": "unknown"},
        ):
            with self.subTest(kwargs=kwargs):
                with self.assertRaises(ConfigurationError):
                    plan_lsh(**kwargs)

        plan = plan_lsh(1)
        with self.assertRaises(ConfigurationError):
            plan.candidate_probability(1.1)


class IndexTests(unittest.TestCase):
    def test_create_batch_query_remove_reopen_and_stats(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / "index.pari"
            first = sketch(0)
            near = sketch(4)
            far = sketch(10_000)

            index = Index.create(
                path,
                threshold=0.8,
                num_perm=64,
                seed=7,
                observability=True,
            )
            index.add(10, first)
            index.add_many([(20, near), (30, far)])

            scalar = index.search(first)
            batch = index.search_many([first, far])
            self.assertEqual(scalar, batch[0])
            self.assertIn(10, scalar)
            self.assertIn(10, index)
            self.assertTrue(index.contains(20))
            self.assertEqual(len(index), 3)

            stats = index.stats()
            self.assertEqual(stats.items, 3)
            self.assertGreater(stats.file_bytes, 0)
            self.assertGreater(stats.bands, 0)
            self.assertGreater(stats.rows, 0)
            self.assertTrue(stats.dirty)
            self.assertGreater(stats.overlay_memberships, 0)
            self.assertIsNotNone(stats.query_operations)
            self.assertEqual(stats.query_operations, 2)
            self.assertEqual(stats.query_count, 3)
            self.assertGreater(stats.candidate_count or 0, 0)
            self.assertGreater(stats.candidate_rate or 0.0, 0.0)
            self.assertGreater(stats.average_query_ms or 0.0, 0.0)

            explanation = index.explain()
            self.assertEqual(explanation.expected_items, 3)
            self.assertEqual(explanation.parameter_source, "existing")
            self.assertEqual((explanation.bands, explanation.rows), (stats.bands, stats.rows))
            self.assertEqual(explanation.requested_storage, "persistent")

            self.assertTrue(index.remove(20))
            self.assertFalse(index.remove(20))
            index.sync()
            synced_stats = index.stats()
            self.assertFalse(synced_stats.dirty)
            self.assertGreater(synced_stats.committed_memberships, 0)
            self.assertGreaterEqual(synced_stats.committed_bucket_p95, 1)
            self.assertGreaterEqual(synced_stats.committed_bucket_maximum, 1)
            index.close()
            self.assertTrue(index.closed)

            reopened = Index.open(path)
            self.assertIsNone(reopened.stats().query_operations)
            reopened.set_observability()
            self.assertEqual(len(reopened), 2)
            self.assertIn(10, reopened.search(first))
            self.assertEqual(reopened.stats().query_operations, 1)
            self.assertNotIn(20, reopened.search(near))
            reopened.close()

    def test_context_manager_syncs_and_closes(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / "context.pari"
            value = sketch(100)
            index = Index.create(path, threshold=0.8, num_perm=64, seed=7)
            with index as active:
                active.add(1, value)
                self.assertFalse(active.closed)
            self.assertTrue(index.closed)

            reopened = Index.open(path)
            self.assertEqual(reopened.search(value), [1])
            reopened.close()

    def test_duplicate_and_compatibility_errors(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / "errors.pari"
            index = Index.create(path, threshold=0.8, num_perm=32, seed=7)
            good = sketch(0, num_perm=32, seed=7)
            wrong_seed = sketch(0, num_perm=32, seed=8)
            index.add(1, good)
            with self.assertRaises(DuplicateKeyError):
                index.add(1, good)
            with self.assertRaises(CompatibilityError):
                index.search(wrong_seed)
            index.close()

    def test_closed_index_operations_fail(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / "closed.pari"
            index = Index.create(path, threshold=0.8, num_perm=32, seed=7)
            index.close()
            index.close()
            with self.assertRaises(ClosedIndexError):
                index.stats()


class Index64Tests(unittest.TestCase):
    def test_create_batch_query_remove_stats_persist_and_reopen(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / "index64.pari"
            first = sketch64(0)
            near = sketch64(4)
            far = sketch64(10_000)

            index = Index64.create(
                path,
                threshold=0.8,
                num_perm=64,
                seed=7,
                observability=True,
            )
            index.add(10, first)
            index.add_many([(20, near), (30, far)])
            scalar = index.search(first)
            batch = index.search_many([first, far])
            self.assertEqual(scalar, batch[0])
            self.assertIn(10, scalar)
            self.assertIn(10, index)
            self.assertTrue(index.contains(20))
            self.assertEqual(len(index), 3)

            stats = index.stats()
            self.assertEqual(stats.items, 3)
            self.assertGreater(stats.file_bytes, 0)
            self.assertTrue(stats.dirty)
            self.assertGreater(stats.overlay_memberships, 0)
            self.assertEqual(stats.query_operations, 2)
            self.assertEqual(stats.query_count, 3)
            self.assertGreater(stats.candidate_count or 0, 0)

            explanation = index.explain()
            self.assertEqual(explanation.signature_bytes_per_item, 512)
            self.assertEqual(explanation.parameter_source, "existing")
            self.assertEqual((explanation.bands, explanation.rows), (stats.bands, stats.rows))

            self.assertTrue(index.remove(20))
            self.assertFalse(index.remove(20))
            index.flush()
            index.sync()
            self.assertFalse(index.stats().dirty)
            index.close()
            self.assertTrue(index.closed)

            reopened = Index64.open(path)
            self.assertEqual(len(reopened), 2)
            self.assertIn(10, reopened.search(first))
            self.assertNotIn(20, reopened.search(near))
            reopened.close()

    def test_context_closed_and_cross_width_errors(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            path64 = Path(directory) / "context64.pari"
            path32 = Path(directory) / "context32.pari"
            affine64 = sketch64(100, num_perm=32)
            affine32 = sketch(100, num_perm=32)

            index64 = Index64.create(path64, num_perm=32, seed=7)
            with index64 as active:
                active.add(1, affine64)
                with self.assertRaises(CompatibilityError):
                    active.add(2, affine32)
                self.assertEqual(len(active), 1)
                with self.assertRaises(CompatibilityError):
                    active.add_many([(2, affine64), (3, affine32)])
                self.assertEqual(len(active), 1)
                with self.assertRaises(CompatibilityError):
                    active.search(affine32)
                with self.assertRaises(CompatibilityError):
                    active.search_many([affine64, affine32])
            self.assertTrue(index64.closed)
            index64.close()
            with self.assertRaises(ClosedIndexError):
                index64.stats()

            with Index.create(path32, num_perm=32, seed=7) as index32:
                with self.assertRaises(CompatibilityError):
                    index32.add(1, affine64)
                with self.assertRaises(CompatibilityError):
                    index32.search(affine64)

            with self.assertRaises(CompatibilityError):
                Index.open(path64)
            with self.assertRaises(CompatibilityError):
                Index64.open(path32)


if __name__ == "__main__":
    unittest.main()
