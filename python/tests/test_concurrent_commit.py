from __future__ import annotations

import gc
import tempfile
import unittest
from pathlib import Path

from pari import Index, Index64, MinHash, MinHash64, StorageError


class ConcurrentCommitTests(unittest.TestCase):
    def test_stale_handle_cannot_overwrite_a_newer_commit(self) -> None:
        for index_type, sketch_type in ((Index, MinHash), (Index64, MinHash64)):
            with (
                self.subTest(index=index_type.__name__),
                tempfile.TemporaryDirectory() as directory,
            ):
                path = Path(directory) / "index.pari"
                first_value = sketch_type.from_values([b"first"], num_perm=32)
                second_value = sketch_type.from_values([b"second"], num_perm=32)
                index_type.create(path, num_perm=32).close()

                first = index_type.open(path)
                second = index_type.open(path)
                try:
                    first.add(1, first_value)
                    first.sync()
                    second.add(2, second_value)
                    with self.assertRaisesRegex(StorageError, "committed by another handle"):
                        second.sync()
                    # The refused handle keeps its changes and stays open.
                    self.assertIn(2, second)
                    self.assertTrue(second.stats().dirty)
                    with self.assertRaises(StorageError):
                        second.close()
                    self.assertFalse(second.closed)
                finally:
                    first.close()
                    # Release the refused handle's file so the directory can be removed.
                    del second
                    gc.collect()

                with index_type.open(path) as reopened:
                    self.assertIn(1, reopened)
                    self.assertNotIn(2, reopened)
                self.assertFalse((Path(directory) / "index.pari.lock").exists())


if __name__ == "__main__":
    unittest.main()
