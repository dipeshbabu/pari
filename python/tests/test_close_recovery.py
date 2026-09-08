from __future__ import annotations

import tempfile
import unittest
from pathlib import Path

from pari import DedupeIndex, Index, Index64, MinHash, MinHash64, StorageError


class CloseRecoveryTests(unittest.TestCase):
    def test_failed_close_preserves_pending_mutations_for_retry(self) -> None:
        for index_type, sketch_type in ((Index, MinHash), (Index64, MinHash64)):
            for context_exit in (False, True):
                with (
                    self.subTest(index=index_type.__name__, context_exit=context_exit),
                    tempfile.TemporaryDirectory() as directory,
                ):
                    path = Path(directory) / "index.pari"
                    backup = Path(directory) / "committed.pari"
                    value = sketch_type.from_values([b"same"], num_perm=32)
                    index = index_type.create(path, num_perm=32)
                    try:
                        index.add(1, value)
                        index.sync()
                        index.remove(1)
                        index.add(2, value)

                        path.rename(backup)
                        path.mkdir()
                        try:
                            with self.assertRaises(StorageError):
                                if context_exit:
                                    with index:
                                        pass
                                else:
                                    index.close()
                        finally:
                            path.rmdir()
                            backup.rename(path)

                        self.assertFalse(index.closed)
                        self.assertTrue(index.stats().dirty)
                        self.assertEqual(index.search(value), [2])
                        index.close()
                        self.assertTrue(index.closed)
                        index.close()
                        with index_type.open(path) as reopened:
                            self.assertEqual(reopened.search(value), [2])
                            self.assertNotIn(1, reopened)
                    finally:
                        index.close()

    def test_failed_dedupe_close_preserves_records_and_persistence_mirror(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / "dedupe.pari"
            backup = Path(directory) / "committed.pari"
            index = DedupeIndex(path=path, num_perm=32)
            try:
                index.add_features("first", [b"same"])
                index.sync()
                index.add_features("second", [b"same"])

                path.rename(backup)
                path.mkdir()
                try:
                    with self.assertRaises(StorageError):
                        index.close()
                finally:
                    path.rmdir()
                    backup.rename(path)

                self.assertFalse(index.closed)
                self.assertEqual(index.candidate_pairs(), (("first", "second"),))
                index.close()
                self.assertTrue(index.closed)
                index.close()
                with Index.open(path) as reopened:
                    self.assertEqual(len(reopened), 2)
                    self.assertIn(0, reopened)
                    self.assertIn(1, reopened)
            finally:
                index.close()


if __name__ == "__main__":
    unittest.main()
