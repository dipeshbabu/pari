from __future__ import annotations

import tempfile
import unittest
from pathlib import Path

from pari import DedupeIndex, Index, Index64, MinHash, MinHash64, ReadOnlyIndex


def tokens(item: int, variant: int = 0) -> list[bytes]:
    group = item // 4
    shared = [f"g{group}-t{token}".encode() for token in range(24)]
    return [*shared, f"item-{item}-{variant}".encode()]


class DeltaCommitTests(unittest.TestCase):
    def test_small_commits_append_generations_readers_apply_them(self) -> None:
        for index_type, sketch_type in ((Index, MinHash), (Index64, MinHash64)):
            with (
                self.subTest(index=index_type.__name__),
                tempfile.TemporaryDirectory() as directory,
            ):
                path = Path(directory) / "index.pari"

                def sketch(
                    item: int, variant: int = 0, sketch_type: type = sketch_type
                ) -> MinHash | MinHash64:
                    return sketch_type.from_values(tokens(item, variant))

                with index_type.create(
                    path, threshold=0.5, store_signatures=True
                ) as index:
                    index.add_many([(item, sketch(item)) for item in range(1_500)])
                    index.sync()
                    base_bytes = index.stats().file_bytes

                    index.add(5_000, sketch(5_000))
                    index.remove(7)
                    index.remove(9)
                    index.add(9, sketch(9, 1))
                    index.flush()
                    stats = index.stats()
                    self.assertEqual(stats.delta_generations, 1)
                    self.assertGreater(stats.delta_bytes, 0)
                    self.assertLess(stats.delta_bytes, base_bytes // 4)
                    self.assertEqual(stats.file_bytes - stats.delta_bytes, base_bytes)
                    self.assertFalse(stats.dirty)

                    probes = [sketch(item) for item in range(0, 1_500, 37)]
                    probes += [sketch(9, 1), sketch(5_000), sketch(7)]
                    expected = index.search_many(probes)
                    scored = index.search_scored(sketch(9, 1))

                with (
                    index_type.open(path) as reopened,
                    ReadOnlyIndex.open(path) as reader,
                ):
                    self.assertEqual(len(reopened), 1_500)
                    self.assertEqual(len(reader), 1_500)
                    self.assertNotIn(7, reopened)
                    self.assertEqual(reopened.search_many(probes), expected)
                    self.assertEqual(reader.search_many(probes), expected)
                    self.assertEqual(reader.search_scored(sketch(9, 1)), scored)
                    self.assertEqual(reopened.stats().delta_generations, 1)

                with index_type.open(path) as index:
                    index.compact()
                    stats = index.stats()
                    self.assertEqual(stats.delta_generations, 0)
                    self.assertEqual(stats.delta_bytes, 0)
                    self.assertEqual(stats.suppressed_base_keys, 0)
                    self.assertEqual(index.search_many(probes), expected)
                with ReadOnlyIndex.open(path) as reader:
                    self.assertEqual(reader.search_many(probes), expected)

    def test_local_dedupe_commits_batches_and_compacts_on_close(self) -> None:
        items = 140_000

        def features(record: int) -> list[bytes]:
            pair = record // 2
            return [f"p{pair}-{token}".encode() for token in range(8)]

        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / "dedupe.pari"
            index = DedupeIndex(features, path=path, num_perm=16, seed=3)
            try:
                self.assertEqual(index.add_many(range(items)), items)
                # Earlier batches were committed as appended generations.
                with Index.open(path) as reader:
                    stats = reader.stats()
                    self.assertGreaterEqual(stats.delta_generations, 1)
                    self.assertLess(len(reader), items)
                groups = index.groups()
                self.assertEqual(len(groups), items // 2)
            finally:
                index.close()

            with Index.open(path) as persisted:
                stats = persisted.stats()
                self.assertEqual(len(persisted), items)
                self.assertEqual(stats.delta_generations, 0)
                self.assertEqual(stats.delta_bytes, 0)
                probe = MinHash.from_values(features(10), num_perm=16, seed=3)
                self.assertEqual(persisted.search(probe), [10, 11])


if __name__ == "__main__":
    unittest.main()
