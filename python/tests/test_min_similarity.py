from __future__ import annotations

import unittest

from pari import ConfigurationError, DedupeIndex, PariError, deduplicate


def words(text: str) -> list[bytes]:
    return [word.encode() for word in text.split()]


RECORDS = [
    "a b c d e f g h i j",
    "a b c d e f g h i j",
    "a b c d e f g h i k",
    "x y z",
]


class MinSimilarityTests(unittest.TestCase):
    def test_threshold_drops_weaker_candidate_edges(self) -> None:
        loose = deduplicate(RECORDS, feature=words, threshold=0.5)
        strict = deduplicate(RECORDS, feature=words, threshold=0.5, min_similarity=0.95)
        self.assertEqual([g.member_indices for g in loose.groups], [(0, 1, 2)])
        self.assertEqual([g.member_indices for g in strict.groups], [(0, 1)])
        self.assertEqual(strict.dropped_indices, (1,))
        self.assertEqual(strict.kept_indices, (0, 2, 3))

    def test_candidate_views_stay_unverified(self) -> None:
        with DedupeIndex(words, threshold=0.5, min_similarity=0.95) as index:
            index.add_many(RECORDS)
            self.assertEqual(
                [g.member_indices for g in index.candidate_groups()], [(0, 1, 2)]
            )
            self.assertEqual([g.member_indices for g in index.groups()], [(0, 1)])
            self.assertEqual(index.similarity(0, 1), 1.0)
            self.assertLess(index.similarity(0, 2), 0.95)
            self.assertEqual(index.min_similarity, 0.95)

    def test_native_threshold_runs_before_exact_verifier(self) -> None:
        seen: list[tuple[str, str]] = []

        def exact(left: str, right: str) -> bool:
            seen.append((left, right))
            return True

        result = deduplicate(
            RECORDS, feature=words, threshold=0.5, min_similarity=0.95, exact=exact
        )
        self.assertEqual([g.member_indices for g in result.groups], [(0, 1)])
        self.assertTrue(all(left == right for left, right in seen))

    def test_invalid_thresholds_and_missing_signatures(self) -> None:
        for bad in (-0.1, 1.5, float("nan"), True, "0.9"):
            with self.subTest(bad=bad), self.assertRaises(ConfigurationError):
                DedupeIndex(words, min_similarity=bad)  # type: ignore[arg-type]
        with DedupeIndex(words) as index:
            index.add_many(RECORDS)
            with self.assertRaises(PariError):
                index.similarity(0, 1)


if __name__ == "__main__":
    unittest.main()
