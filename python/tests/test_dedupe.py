from __future__ import annotations

import tempfile
import unittest
from pathlib import Path

from pari import (
    ClosedIndexError,
    ConfigurationError,
    DedupeIndex,
    Index,
    InvalidRepresentativeError,
    ProgressCancelledError,
    ProgressEvent,
    StorageError,
    deduplicate,
)


def text_features(record: dict[str, object]) -> list[bytes]:
    text = str(record["text"])
    return [token.encode() for token in text.casefold().split()]


class DeduplicateFunctionTests(unittest.TestCase):
    def test_concise_api_returns_groups_and_input_order_partitions(self) -> None:
        records = [
            {"id": "first", "text": "alpha beta gamma delta", "score": 1},
            {"id": "duplicate", "text": "alpha beta gamma delta", "score": 9},
            {"id": "unique", "text": "red green blue orange", "score": 5},
        ]

        result = deduplicate(
            records,
            feature=text_features,
            num_perm=64,
            seed=7,
            batch_size=2,
        )

        self.assertEqual(result.group_count, 1)
        self.assertEqual(result.duplicate_count, 1)
        self.assertEqual(result.groups[0].member_indices, (0, 1))
        self.assertIs(result.groups[0].representative, records[0])
        self.assertEqual(result.kept_indices, (0, 2))
        self.assertEqual(result.dropped_indices, (1,))
        self.assertEqual(result.kept, (records[0], records[2]))
        self.assertEqual(result.dropped, (records[1],))

    def test_exact_verifier_filters_approximate_collisions(self) -> None:
        records = [
            {"id": "a", "family": 1},
            {"id": "b", "family": 1},
            {"id": "c", "family": 2},
        ]

        result = deduplicate(
            records,
            feature=lambda _record: [b"shared-candidate"],
            exact=lambda left, right: left["family"] == right["family"],
            num_perm=32,
            seed=11,
        )

        self.assertEqual([group.member_indices for group in result.groups], [(0, 1)])
        self.assertEqual(result.kept_indices, (0, 2))
        self.assertEqual(result.dropped_indices, (1,))

    def test_representative_callback_controls_keep_drop(self) -> None:
        records = [
            {"id": "older", "score": 1},
            {"id": "preferred", "score": 10},
        ]

        result = deduplicate(
            records,
            feature=lambda _record: [b"same"],
            representative=lambda members: max(
                members, key=lambda item: int(item["score"])
            ),
            num_perm=32,
        )

        self.assertIs(result.groups[0].representative, records[1])
        self.assertEqual(result.groups[0].representative_index, 1)
        self.assertEqual(result.kept_indices, (1,))
        self.assertEqual(result.dropped_indices, (0,))

    def test_invalid_representative_is_a_stable_error(self) -> None:
        records = [{"id": 1}, {"id": 2}]
        with self.assertRaises(InvalidRepresentativeError):
            deduplicate(
                records,
                feature=lambda _record: [b"same"],
                representative=lambda _members: {"id": 99},
                num_perm=32,
            )


class DedupeIndexTests(unittest.TestCase):
    def test_progress_is_batch_granular_and_reports_exact_total(self) -> None:
        records = [{"text": f"record {index}"} for index in range(5)]
        events: list[ProgressEvent] = []
        index = DedupeIndex(text_features, num_perm=32, batch_size=2, threads=1)
        try:
            self.assertEqual(index.add_many(records, progress=events.append), 5)
            self.assertEqual([event.completed for event in events], [2, 4, 5])
            self.assertEqual([event.batch_size for event in events], [2, 2, 1])
            self.assertEqual([event.total for event in events], [5, 5, 5])
            self.assertEqual([event.final for event in events], [False, False, True])
            self.assertTrue(all(event.items_per_second > 0 for event in events))
        finally:
            index.close()

    def test_progress_cancellation_and_callback_errors_preserve_completed_batches(self) -> None:
        records = [{"text": f"record {index}"} for index in range(5)]
        index = DedupeIndex(text_features, num_perm=32, batch_size=2, threads=1)
        try:
            with self.assertRaises(ProgressCancelledError) as cancelled:
                index.add_many(records, progress=lambda _event: False)
            self.assertEqual(cancelled.exception.completed, 2)
            self.assertEqual(len(index), 2)
        finally:
            index.close()

        failing = DedupeIndex(text_features, num_perm=32, batch_size=2, threads=1)
        try:
            def fail(_event: ProgressEvent) -> None:
                raise RuntimeError("progress failed")

            with self.assertRaisesRegex(RuntimeError, "progress failed"):
                failing.add_many(records, progress=fail)
            self.assertEqual(len(failing), 2)
        finally:
            failing.close()

    def test_scalar_and_batch_ingestion_match(self) -> None:
        records = [
            {"text": "same values"},
            {"text": "same values"},
            {"text": "different tokens"},
        ]
        scalar = DedupeIndex(text_features, num_perm=64, seed=7)
        batch = DedupeIndex(text_features, num_perm=64, seed=7, batch_size=2)
        try:
            for expected_index, record in enumerate(records):
                self.assertEqual(scalar.add(record), expected_index)
            self.assertEqual(batch.add_many(iter(records)), len(records))

            self.assertEqual(
                [group.member_indices for group in scalar.groups()],
                [group.member_indices for group in batch.groups()],
            )
            self.assertEqual(scalar.result().kept_indices, batch.result().kept_indices)
        finally:
            scalar.close()
            batch.close()

    def test_precomputed_features_retain_only_supplied_records(self) -> None:
        records = [{"id": 1}, {"id": 2}, {"id": 3}]
        index = DedupeIndex[dict[str, int]](None, num_perm=32, batch_size=2)
        try:
            added = index.add_many_features(
                [
                    (records[0], [b"same"]),
                    (records[1], [b"same"]),
                    (records[2], [b"different"]),
                ]
            )
            self.assertEqual(added, 3)
            self.assertEqual(index.candidate_groups()[0].member_indices, (0, 1))
            self.assertEqual(index.result().dropped_indices, (1,))
        finally:
            index.close()

        without_feature = DedupeIndex[object](None, num_perm=32)
        try:
            with self.assertRaisesRegex(ConfigurationError, "use add_features"):
                without_feature.add(object())
        finally:
            without_feature.close()

    def test_local_backend_persists_the_same_native_batches(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / "dedupe.pari"
            index = DedupeIndex(
                text_features,
                path=path,
                backend="local",
                num_perm=32,
                seed=3,
            )
            index.add_many([{"text": "same"}, {"text": "same"}, {"text": "other"}])
            self.assertEqual(index.result().dropped_indices, (1,))
            index.close()

            persisted = Index.open(path)
            try:
                self.assertEqual(len(persisted), 3)
            finally:
                persisted.close()

    def test_configuration_and_closed_errors_are_stable(self) -> None:
        with self.assertRaises(ConfigurationError):
            DedupeIndex(text_features, batch_size=0)
        with self.assertRaises(ConfigurationError):
            DedupeIndex(text_features, threads=0)
        with self.assertRaises(ConfigurationError):
            DedupeIndex(text_features, backend="local")
        with self.assertRaises(ConfigurationError):
            DedupeIndex(text_features, backend="memory", path="unexpected.pari")

        index = DedupeIndex(text_features, num_perm=32, threads=4)
        self.assertEqual(index.threads, 4)
        index.close()
        index.close()
        self.assertTrue(index.closed)
        with self.assertRaises(ClosedIndexError):
            index.add({"text": "closed"})

    def test_exact_verifier_exceptions_propagate(self) -> None:
        def fail(_left: object, _right: object) -> bool:
            raise RuntimeError("verification failed")

        index = DedupeIndex(lambda _record: [b"same"], exact=fail, num_perm=32)
        try:
            index.add_many([object(), object()])
            with self.assertRaisesRegex(RuntimeError, "verification failed"):
                index.groups()
        finally:
            index.close()

    def test_exact_verifier_reentry_fails_instead_of_deadlocking(self) -> None:
        index: DedupeIndex[object]

        def reenter(_left: object, _right: object) -> bool:
            return index.closed

        index = DedupeIndex(lambda _record: [b"same"], exact=reenter, num_perm=32)
        try:
            index.add_many([object(), object()])
            with self.assertRaisesRegex(StorageError, "busy running a callback"):
                index.groups()
        finally:
            index.close()


if __name__ == "__main__":
    unittest.main()
