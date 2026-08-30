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
    LshPlan,
    MinHash,
    plan_lsh,
)


def sketch(base: int, *, num_perm: int = 64, seed: int = 7) -> MinHash:
    values = [(value).to_bytes(8, "little") for value in range(base, base + 40)]
    return MinHash.from_values(values, num_perm=num_perm, seed=seed)


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


if __name__ == "__main__":
    unittest.main()
