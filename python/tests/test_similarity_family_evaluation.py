from __future__ import annotations

import importlib.util
import sys
import unittest
from collections import Counter
from pathlib import Path

ROOT = Path(__file__).resolve().parents[2]
SCRIPT = ROOT / "benchmarks" / "similarity_family_evaluation.py"
SPEC = importlib.util.spec_from_file_location(
    "pari_similarity_family_evaluation", SCRIPT
)
if SPEC is None or SPEC.loader is None:
    raise RuntimeError("could not load similarity-family evaluation")
evaluation = importlib.util.module_from_spec(SPEC)
sys.modules[SPEC.name] = evaluation
SPEC.loader.exec_module(evaluation)


class SimilarityFamilyEvaluationTests(unittest.TestCase):
    def test_weighted_jaccard_distinguishes_frequency_from_support(self) -> None:
        left = Counter({"view": 100, "buy": 5})
        different = Counter({"view": 10, "buy": 5})
        similar = Counter({"view": 95, "buy": 5})
        self.assertEqual(evaluation.binary_jaccard(left, different), 1.0)
        self.assertEqual(evaluation.binary_jaccard(left, similar), 1.0)
        self.assertLess(evaluation.weighted_jaccard(left, different), 0.15)
        self.assertGreater(evaluation.weighted_jaccard(left, similar), 0.95)

    def test_simhash_is_deterministic_and_separates_checked_fixture(self) -> None:
        first = evaluation.simhash64(Counter({"alpha": 4, "beta": 2}))
        self.assertEqual(first, evaluation.simhash64(Counter({"alpha": 4, "beta": 2})))
        report = evaluation.evaluate()
        pairs = report["simhash_code_workload"]["pairs"]
        duplicate = next(pair for pair in pairs if pair["same_label"])
        unrelated = [pair for pair in pairs if not pair["same_label"]]
        self.assertEqual(duplicate["simhash_similarity"], 1.0)
        self.assertTrue(all(pair["simhash_similarity"] < 0.75 for pair in unrelated))


if __name__ == "__main__":
    unittest.main()
