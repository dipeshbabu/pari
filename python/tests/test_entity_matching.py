from __future__ import annotations

import importlib.util
import json
import sys
import tempfile
import unittest
from pathlib import Path

ROOT = Path(__file__).resolve().parents[2]
SCRIPT = ROOT / "examples" / "entity_matching.py"
FIXTURE = ROOT / "examples" / "entity_matching_fixture"
SPEC = importlib.util.spec_from_file_location("pari_entity_matching", SCRIPT)
if SPEC is None or SPEC.loader is None:
    raise RuntimeError("could not load entity matching example")
workload = importlib.util.module_from_spec(SPEC)
sys.modules[SPEC.name] = workload
SPEC.loader.exec_module(workload)


def read_jsonl(path: Path) -> list[dict[str, object]]:
    return [json.loads(line) for line in path.read_text(encoding="utf-8").splitlines()]


class EntityMatchingTests(unittest.TestCase):
    def test_feature_profiles_are_deterministic_and_domain_specific(self) -> None:
        customer = {
            "id": "customer",
            "name": "Alice B. Smith",
            "email": "Alice@Example.COM",
            "phone": "+1 (617) 555-0101",
        }
        first = workload.customer_features(customer)
        second = workload.customer_features(dict(reversed(customer.items())))
        self.assertEqual(first, second)
        labeled = dict(customer, entity_id="ground-truth-only")
        self.assertEqual(first, workload.customer_features(labeled))
        self.assertIn(b"email:exact:alice@example.com", first)
        self.assertIn(b"phone:exact:6175550101", first)

        product = workload.product_features(
            {
                "title": "Wireless Mouse X-100",
                "brand": "Home Co.",
                "sku": "MUG-12",
            }
        )
        self.assertIn(b"brand:exact:homeco", product)
        self.assertIn(b"sku:exact:mug12", product)
        self.assertNotEqual(first, product)

    def test_labeled_customer_and_product_fixtures_have_full_candidate_recall(
        self,
    ) -> None:
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            for profile, expected_pairs in (
                (
                    "customer",
                    {("customer-a", "customer-b"), ("customer-d", "customer-e")},
                ),
                ("product", {("product-a", "product-b"), ("product-d", "product-e")}),
            ):
                with self.subTest(profile=profile):
                    memory = self.run_fixture(root, profile, "memory", persistent=False)
                    persistent = self.run_fixture(
                        root, profile, "persistent", persistent=True
                    )
                    self.assertEqual(
                        memory["pairs"].read_bytes(), persistent["pairs"].read_bytes()
                    )
                    self.assertEqual(
                        memory["groups"].read_bytes(), persistent["groups"].read_bytes()
                    )

                    pairs = read_jsonl(memory["pairs"])
                    actual_pairs = {
                        (row["left"]["id"], row["right"]["id"]) for row in pairs
                    }
                    self.assertEqual(actual_pairs, expected_pairs)
                    self.assertTrue(all(row["same_label"] for row in pairs))

                    report = json.loads(memory["metrics"].read_text(encoding="utf-8"))
                    self.assertEqual(report["schema_version"], 1)
                    self.assertEqual(report["workload"], "entity-record-matching")
                    self.assertEqual(report["metrics"]["input_items"]["value"], 5)
                    self.assertEqual(
                        report["metrics"]["candidate_pair_count"]["value"], 2
                    )
                    self.assertEqual(
                        report["metrics"]["candidate_recall"]["value"], 1.0
                    )
                    self.assertEqual(
                        report["metrics"]["candidate_precision"]["value"], 1.0
                    )
                    self.assertEqual(
                        report["metrics"]["candidate_reduction_ratio"]["value"], 0.8
                    )

                    persistent_report = json.loads(
                        persistent["metrics"].read_text(encoding="utf-8")
                    )
                    self.assertGreater(
                        persistent_report["metrics"]["index_bytes"]["value"], 0
                    )

    def test_jsonl_iteration_is_streaming_and_validation_is_explicit(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            source = Path(directory) / "records.jsonl"
            source.write_text(
                json.dumps({"id": "first", "name": "First Person"}) + "\n{not-json}\n",
                encoding="utf-8",
                newline="\n",
            )
            iterator = workload.iter_records(source, "customer", "id", None, None)
            first, features = next(iterator)
            self.assertEqual(first.identity, "first")
            self.assertTrue(features)
            with self.assertRaisesRegex(ValueError, "line 2"):
                next(iterator)

            empty = Path(directory) / "empty.jsonl"
            empty.write_text('{"id":"empty","unknown":"value"}\n', encoding="utf-8")
            with self.assertRaisesRegex(ValueError, "no usable customer fields"):
                list(workload.iter_records(empty, "customer", "id", None, None))

    def run_fixture(
        self,
        root: Path,
        profile: str,
        mode: str,
        *,
        persistent: bool,
    ) -> dict[str, Path]:
        prefix = f"{profile}-{mode}"
        pairs = root / f"{prefix}-pairs.jsonl"
        groups = root / f"{prefix}-groups.jsonl"
        metrics = root / f"{prefix}-metrics.json"
        arguments = [
            "--input",
            str(FIXTURE / f"{profile}s.jsonl"),
            "--profile",
            profile,
            "--label-field",
            "entity_id",
            "--pairs-output",
            str(pairs),
            "--groups-output",
            str(groups),
            "--metrics-output",
            str(metrics),
            "--threshold",
            "0.4",
            "--num-perm",
            "128",
            "--seed",
            "7",
            "--batch-size",
            "2",
        ]
        output = {"pairs": pairs, "groups": groups, "metrics": metrics}
        if persistent:
            index = root / f"{prefix}.pari"
            arguments.extend(["--index", str(index)])
            output["index"] = index
        self.assertEqual(workload.main(arguments), 0)
        return output


if __name__ == "__main__":
    unittest.main()
