from __future__ import annotations

import tempfile
import unittest
from pathlib import Path

from pari import ConfigurationError, Index, Index64, MinHash, MinHash64


def values(start: int, stop: int) -> list[bytes]:
    return [value.to_bytes(8, "little") for value in range(start, stop)]


class ScoredSearchTests(unittest.TestCase):
    def setUp(self) -> None:
        self._directory = tempfile.TemporaryDirectory()
        self.root = Path(self._directory.name)

    def tearDown(self) -> None:
        self._directory.cleanup()

    def test_scores_match_jaccard_and_survive_reopen(self) -> None:
        query = MinHash.from_values(values(0, 100), seed=7)
        close = MinHash.from_values(values(0, 95), seed=7)
        medium = MinHash.from_values([*values(0, 80), *values(500, 520)], seed=7)
        path = self.root / "scored.pari"
        with Index.create(path, threshold=0.5, seed=7, store_signatures=True) as index:
            self.assertTrue(index.stores_signatures)
            self.assertTrue(index.stats().stores_signatures)
            index.add_many([(1, close), (2, medium)])
            before = index.search_scored(query)

        with Index.open(path) as index:
            after = index.search_scored(query)
            self.assertEqual(before, after)
            self.assertEqual(after[0][0], 1)
            expected = {1: query.jaccard(close), 2: query.jaccard(medium)}
            for key, similarity in after:
                self.assertAlmostEqual(similarity, expected[key])
            self.assertEqual(sorted(key for key, _ in after), index.search(query))
            self.assertEqual(index.signature(1).signature, close.signature)
            self.assertIsNone(index.signature(99))

    def test_filters_and_batches(self) -> None:
        query = MinHash.from_values(values(0, 100), seed=7)
        items = [MinHash.from_values(values(0, 100 - 5 * n), seed=7) for n in range(5)]
        with Index.create(
            self.root / "filters.pari", threshold=0.5, seed=7, store_signatures=True
        ) as index:
            index.add_many(list(enumerate(items)))
            everything = index.search_scored(query)
            self.assertEqual(index.search_scored(query, top_k=2), everything[:2])
            floor = everything[2][1]
            self.assertTrue(
                all(
                    similarity >= floor
                    for _, similarity in index.search_scored(
                        query, min_similarity=floor
                    )
                )
            )
            batch = index.search_scored_many([query, items[4]], top_k=1)
            self.assertEqual(batch[0], everything[:1])
            self.assertEqual(batch[1], [(4, 1.0)])
            for bad in ({"min_similarity": 1.5}, {"top_k": 0}):
                with self.assertRaises(ConfigurationError):
                    index.search_scored(query, **bad)

    def test_indexes_without_signatures_report_configuration_error(self) -> None:
        sketch = MinHash.from_values(values(0, 10), seed=1)
        with Index.create(self.root / "plain.pari") as index:
            index.add(1, sketch)
            self.assertFalse(index.stores_signatures)
            self.assertIsNone(index.signature(1))
            with self.assertRaisesRegex(
                ConfigurationError, "does not store signatures"
            ):
                index.search_scored(sketch)

    def test_index64_scores_full_width_signatures(self) -> None:
        query = MinHash64.from_values(values(0, 100), seed=3)
        close = MinHash64.from_values(values(0, 95), seed=3)
        with Index64.create(
            self.root / "wide.pari", threshold=0.5, seed=3, store_signatures=True
        ) as index:
            index.add(7, close)
            scored = index.search_scored(query)
            self.assertEqual([key for key, _ in scored], [7])
            self.assertAlmostEqual(scored[0][1], query.jaccard(close))
            stored = index.signature(7)
            self.assertIsInstance(stored, MinHash64)
            self.assertEqual(stored.signature, close.signature)


if __name__ == "__main__":
    unittest.main()
