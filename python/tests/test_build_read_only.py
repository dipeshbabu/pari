from __future__ import annotations

import shutil
import tempfile
import unittest
from pathlib import Path

from pari import (
    BuildStats,
    ConfigurationError,
    Index,
    Index64,
    MinHash,
    MinHash64,
    ReadOnlyIndex,
    StorageError,
    build_read_only_index,
)


def sketches(sketch_type: type, count: int) -> list:
    result = []
    for item in range(count):
        group = item // 3
        features = [f"g{group}-t{token}".encode() for token in range(20)]
        features.append(f"item-{item}".encode())
        sketch = sketch_type(num_perm=64)
        sketch.update_many(features)
        result.append(sketch)
    return result


class BuildReadOnlyIndexTests(unittest.TestCase):
    def test_compacted_index_builds_a_query_ready_copy(self) -> None:
        for writer_type, sketch_type in ((Index, MinHash), (Index64, MinHash64)):
            with (
                self.subTest(writer=writer_type.__name__),
                tempfile.TemporaryDirectory() as directory,
            ):
                source = Path(directory) / "source.pari"
                items = sketches(sketch_type, 90)
                with writer_type.create(
                    source, threshold=0.5, num_perm=64, store_signatures=True
                ) as writer:
                    writer.add_many(list(enumerate(items)))
                    writer.compact()
                    expected = writer.search_many(items)

                reused = Path(directory) / "reused.pari"
                stats = build_read_only_index(str(source), reused)
                self.assertIsInstance(stats, BuildStats)
                self.assertTrue(stats.reused_source_buckets)
                self.assertEqual(stats.spill_runs, 0)
                self.assertEqual(stats.output_bytes, reused.stat().st_size)
                self.assertGreater(stats.records, 0)
                self.assertIn("reused_source_buckets=True", repr(stats))

                rebuilt = Path(directory) / "rebuilt.pari"
                full = build_read_only_index(
                    source, rebuilt, reuse_buckets=False, max_buffer_records=100
                )
                self.assertFalse(full.reused_source_buckets)
                self.assertGreater(full.spill_runs, 1)
                self.assertEqual(
                    (full.records, full.buckets), (stats.records, stats.buckets)
                )
                self.assertEqual(reused.read_bytes(), rebuilt.read_bytes())

                with ReadOnlyIndex.open(reused) as reader:
                    self.assertEqual(len(reader), len(items))
                    self.assertTrue(reader.stores_signatures)
                    self.assertEqual(reader.search_many(items), expected)

    def test_appended_commits_are_folded_like_a_compaction(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            source = Path(directory) / "source.pari"
            items = sketches(MinHash, 400)
            with Index.create(
                source, threshold=0.5, num_perm=64, store_signatures=True
            ) as writer:
                writer.add_many(list(enumerate(items[:300])))
                writer.flush()
                for step in range(4):
                    start = 300 + step * 25
                    writer.add_many(
                        [(key, items[key]) for key in range(start, start + 25)]
                    )
                    self.assertTrue(writer.remove(step * 7))
                    writer.flush()
                self.assertGreater(writer.stats().delta_generations, 0)
                expected = writer.search_many(items)

            folded = Path(directory) / "folded.pari"
            stats = build_read_only_index(source, folded)
            self.assertGreater(stats.folded_generations, 0)
            self.assertFalse(stats.reused_source_buckets)
            self.assertIn(f"folded_generations={stats.folded_generations}", repr(stats))

            compacted = Path(directory) / "compacted.pari"
            shutil.copyfile(source, compacted)
            with Index.open(compacted) as writer:
                writer.compact()
            self.assertEqual(folded.read_bytes(), compacted.read_bytes())

            with ReadOnlyIndex.open(folded) as reader:
                self.assertEqual(len(reader), 396)
                self.assertEqual(reader.search_many(items), expected)

    def test_destination_and_options_are_checked(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            source = Path(directory) / "source.pari"
            with Index.create(source, num_perm=64) as writer:
                writer.add_many(list(enumerate(sketches(MinHash, 6))))
            destination = Path(directory) / "copy.pari"
            build_read_only_index(source, destination)
            before = destination.read_bytes()
            with self.assertRaises(StorageError):
                build_read_only_index(source, destination)
            self.assertEqual(destination.read_bytes(), before)
            with self.assertRaises(ConfigurationError):
                build_read_only_index(
                    source, Path(directory) / "other.pari", max_buffer_records=0
                )
            with self.assertRaises(StorageError):
                build_read_only_index(
                    Path(directory) / "missing.pari", Path(directory) / "x.pari"
                )


if __name__ == "__main__":
    unittest.main()
