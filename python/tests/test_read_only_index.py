from __future__ import annotations

import tempfile
import unittest
from concurrent.futures import ThreadPoolExecutor
from pathlib import Path

from pari import (
    ClosedIndexError,
    CompatibilityError,
    ConfigurationError,
    Index,
    Index64,
    MinHash,
    MinHash64,
    ReadOnlyIndex,
    StorageError,
)


class ReadOnlyIndexTests(unittest.TestCase):
    def test_signed_query_parity_and_snapshot_integrity(self) -> None:
        for width, writer_type, sketch_type in (
            (32, Index, MinHash),
            (64, Index64, MinHash64),
        ):
            with self.subTest(width=width), tempfile.TemporaryDirectory() as directory:
                path = Path(directory) / "index.pari"
                seed = 2**64 - 1
                num_perm = 128
                base = 2**32 if width == 64 else 0
                signature = list(range(base, base + num_perm))
                query = sketch_type.from_signature(signature, seed=seed)
                other = sketch_type.from_signature(
                    [value + 10_000 for value in signature], seed=seed
                )
                keys = [0, 2**64 - 1]
                with writer_type.create(
                    path, num_perm=num_perm, seed=seed, store_signatures=True
                ) as writer:
                    writer.add_many([(keys[1], query), (1, other), (keys[0], query)])
                original = path.read_bytes()

                with (
                    writer_type.open(path) as writable,
                    ReadOnlyIndex.open(path) as index,
                ):
                    self.assertEqual(
                        (index.signature_width, index.num_perm, index.seed),
                        (width, num_perm, seed),
                    )
                    self.assertTrue(index.stores_signatures)
                    self.assertEqual(len(index), 3)
                    self.assertFalse(index.closed)
                    self.assertEqual(index.search(query), keys)
                    self.assertEqual(
                        index.search_many([other, query]),
                        writable.search_many([other, query]),
                    )
                    self.assertEqual(
                        index.search_scored(query), [(key, 1.0) for key in keys]
                    )
                    self.assertEqual(
                        index.search_scored_many(
                            [other, query], top_k=1, min_similarity=0.9
                        ),
                        writable.search_scored_many(
                            [other, query], top_k=1, min_similarity=0.9
                        ),
                    )
                    self.assertEqual(index.search_many([]), [])
                    self.assertEqual(index.search_scored_many([]), [])
                    self.assertFalse(hasattr(index, "add"))
                    self.assertFalse(hasattr(index, "sync"))
                    with self.assertRaises(ConfigurationError):
                        index.search_scored(query, min_similarity=float("nan"))
                    wrong_width = MinHash if width == 64 else MinHash64
                    with self.assertRaises(CompatibilityError):
                        index.search_many(
                            [query, wrong_width(num_perm=num_perm, seed=seed)]
                        )
                    with self.assertRaises(CompatibilityError):
                        index.search(sketch_type(num_perm=num_perm, seed=0))
                    with self.assertRaises(CompatibilityError):
                        index.search(sketch_type(num_perm=num_perm - 1, seed=seed))

                self.assertTrue(index.closed)
                index.close()
                with self.assertRaises(ClosedIndexError):
                    index.search(query)
                with self.assertRaises(ClosedIndexError):
                    index.__enter__()
                self.assertEqual(path.read_bytes(), original)

    def test_unsigned_empty_and_invalid_snapshots(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / "index.pari"
            with self.assertRaises(StorageError):
                ReadOnlyIndex.open(path)
            self.assertFalse(path.exists())
            Index.create(path).close()
            with ReadOnlyIndex.open(path) as index:
                self.assertEqual(len(index), 0)
                self.assertFalse(index.stores_signatures)
                self.assertEqual(index.search(MinHash()), [])
                with self.assertRaisesRegex(ConfigurationError, "store signatures"):
                    index.search_scored(MinHash())
            path.write_bytes(b"invalid index")
            with self.assertRaises(StorageError):
                ReadOnlyIndex.open(path)
            self.assertEqual(path.read_bytes(), b"invalid index")

    def test_open_reader_keeps_its_snapshot_across_a_new_commit(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / "index.pari"
            query = MinHash.from_values([b"product", b"example"])
            with Index.create(path) as writer:
                writer.add(1, query)
                writer.sync()
                with ReadOnlyIndex.open(path) as first:
                    writer.add(2, query)
                    writer.sync()
                    with (
                        ReadOnlyIndex.open(path) as second,
                        ThreadPoolExecutor(4) as workers,
                    ):
                        self.assertEqual(first.search(query), [1])
                        self.assertEqual(
                            list(workers.map(second.search, [query] * 16)),
                            [[1, 2]] * 16,
                        )


if __name__ == "__main__":
    unittest.main()
