from __future__ import annotations

import tempfile
import unittest
from pathlib import Path

from pari import (
    ClosedIndexError,
    CompatibilityError,
    DuplicateKeyError,
    Index,
    MinHash,
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


class IndexTests(unittest.TestCase):
    def test_create_batch_query_remove_reopen_and_stats(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / "index.pari"
            first = sketch(0)
            near = sketch(4)
            far = sketch(10_000)

            index = Index.create(path, threshold=0.8, num_perm=64, seed=7)
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

            self.assertTrue(index.remove(20))
            self.assertFalse(index.remove(20))
            index.sync()
            self.assertFalse(index.stats().dirty)
            index.close()
            self.assertTrue(index.closed)

            reopened = Index.open(path)
            self.assertEqual(len(reopened), 2)
            self.assertIn(10, reopened.search(first))
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
