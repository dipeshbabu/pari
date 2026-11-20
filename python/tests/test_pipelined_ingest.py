"""Batches larger than one native ingest chunk insert while the next is collected."""

from __future__ import annotations

import tempfile
import unittest
from pathlib import Path

from pari import DedupeIndex, Index, StorageError

# More than two native ingest chunks of 8,192 rows in one batch.
RECORDS = 25_000
# The second call, from 5,000, fails in its second chunk, after the first
# chunk is inserted.
FAIL_AT = 17_000
# One native batch per call.
BATCH = 65_536


def pair_features(record: int) -> list[bytes]:
    """Records 2k and 2k + 1 share 29 of 32 features."""

    pair, member = divmod(record, 2)
    base = pair * 1_000
    unique = base + 29 + member * 3
    values = [*range(base, base + 29), *range(unique, unique + 3)]
    return [value.to_bytes(8, "little") for value in values]


def failing_features(record: int) -> list[bytes]:
    if record == FAIL_AT:
        raise RuntimeError("feature failed")
    return pair_features(record)


def failing_from_second_call(first_call: int):
    def features(record: int) -> list[bytes]:
        return (
            failing_features(record) if record >= first_call else pair_features(record)
        )

    return features


class PipelinedIngestTests(unittest.TestCase):
    def build(self, backend: str, threads: int | None, directory: str, **options):
        path = (
            Path(directory) / f"{backend}-{threads}.pari"
            if backend == "local"
            else None
        )
        index = DedupeIndex(
            pair_features,
            num_perm=32,
            seed=11,
            batch_size=BATCH,
            threads=threads,
            backend=backend,
            path=path,
            **options,
        )
        return index, path

    def test_pipelined_batches_match_sequential_ingest(self) -> None:
        for backend in ("memory", "local"):
            with (
                self.subTest(backend=backend),
                tempfile.TemporaryDirectory() as directory,
            ):
                pipelined, path = self.build(
                    backend, None, directory, min_similarity=0.5
                )
                sequential, _ = self.build(backend, 1, directory, min_similarity=0.5)
                try:
                    self.assertEqual(pipelined.add_many(range(RECORDS)), RECORDS)
                    self.assertEqual(sequential.add_many(range(RECORDS)), RECORDS)
                    self.assertEqual(len(pipelined), RECORDS)
                    self.assertEqual(
                        sorted(pipelined._engine.pairs()),
                        sorted(sequential._engine.pairs()),
                    )
                    groups = [group.member_indices for group in pipelined.groups()]
                    self.assertEqual(
                        groups, [group.member_indices for group in sequential.groups()]
                    )
                    self.assertGreater(len(groups), RECORDS // 10)
                    self.assertEqual(
                        pipelined.similarity(0, 1), sequential.similarity(0, 1)
                    )
                finally:
                    pipelined.close()
                    sequential.close()
                if path is not None:
                    persisted = Index.open(path)
                    try:
                        self.assertEqual(len(persisted), RECORDS)
                    finally:
                        persisted.close()

    def test_a_failure_after_inserted_chunks_rolls_back_the_whole_call(self) -> None:
        first = 5_000
        for backend in ("memory", "local"):
            for threads in (None, 1):
                with (
                    self.subTest(backend=backend, threads=threads),
                    tempfile.TemporaryDirectory() as directory,
                ):
                    path = (
                        Path(directory) / "rollback.pari"
                        if backend == "local"
                        else None
                    )
                    index = DedupeIndex(
                        failing_from_second_call(first),
                        num_perm=32,
                        seed=11,
                        batch_size=BATCH,
                        threads=threads,
                        backend=backend,
                        path=path,
                        min_similarity=0.5,
                    )
                    expected, _ = self.build(backend, 1, directory)
                    try:
                        index.add_many(range(first))
                        with self.assertRaisesRegex(RuntimeError, "feature failed"):
                            index.add_many(range(first, RECORDS))
                        self.assertEqual(len(index), first)
                        expected.add_many(range(first))
                        self.assertEqual(
                            sorted(index._engine.pairs()),
                            sorted(expected._engine.pairs()),
                        )

                        # Keys stay contiguous after the rollback.
                        self.assertEqual(
                            index.add_many(range(first, FAIL_AT)), FAIL_AT - first
                        )
                        expected.add_many(range(first, FAIL_AT))
                        self.assertEqual(len(index), FAIL_AT)
                        self.assertEqual(
                            sorted(index._engine.pairs()),
                            sorted(expected._engine.pairs()),
                        )
                        # Signatures of the re-added records line up with their keys.
                        self.assertGreater(index.similarity(first, first + 1), 0.5)
                        self.assertLess(index.similarity(first, first + 2), 0.5)
                    finally:
                        index.close()
                        expected.close()
                    if path is not None:
                        persisted = Index.open(path)
                        try:
                            self.assertEqual(len(persisted), FAIL_AT)
                        finally:
                            persisted.close()

    def test_a_failed_mirror_commit_rolls_back_the_parallel_index_insert(self) -> None:
        committed = 10
        # A full batch makes the next batch commit the mirror first.
        pending_end = committed + BATCH
        failed_end = pending_end + 9_000
        for threads in (None, 1):
            with (
                self.subTest(threads=threads),
                tempfile.TemporaryDirectory() as directory,
            ):
                path = Path(directory) / "mirror.pari"
                backup = Path(directory) / "committed.pari"
                index = DedupeIndex(
                    pair_features,
                    num_perm=32,
                    seed=11,
                    threads=threads,
                    batch_size=BATCH,
                    backend="local",
                    path=path,
                )
                expected, _ = self.build("memory", 1, directory)
                try:
                    index.add_many(range(committed))
                    index.sync()
                    index.add_many(range(committed, pending_end))
                    expected.add_many(range(pending_end))

                    path.rename(backup)
                    path.mkdir()
                    try:
                        with self.assertRaises(StorageError):
                            index.add_many(range(pending_end, failed_end))
                    finally:
                        path.rmdir()
                        backup.rename(path)
                    self.assertEqual(len(index), pending_end)
                    self.assertEqual(
                        sorted(index._engine.pairs()), sorted(expected._engine.pairs())
                    )

                    index.add_many(range(pending_end, failed_end))
                    expected.add_many(range(pending_end, failed_end))
                    self.assertEqual(
                        sorted(index._engine.pairs()), sorted(expected._engine.pairs())
                    )
                finally:
                    index.close()
                    expected.close()
                persisted = Index.open(path)
                try:
                    self.assertEqual(len(persisted), failed_end)
                finally:
                    persisted.close()


if __name__ == "__main__":
    unittest.main()
