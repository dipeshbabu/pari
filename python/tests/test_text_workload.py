from __future__ import annotations

import hashlib
import importlib.util
import json
import sys
import tempfile
import unittest
from pathlib import Path

ROOT = Path(__file__).resolve().parents[2]
SCRIPT = ROOT / "examples" / "text_workload.py"
SPEC = importlib.util.spec_from_file_location("pari_text_workload", SCRIPT)
if SPEC is None or SPEC.loader is None:
    raise RuntimeError("could not load text workload example")
workload = importlib.util.module_from_spec(SPEC)
sys.modules[SPEC.name] = workload
SPEC.loader.exec_module(workload)


def write_jsonl(path: Path, records: list[dict[str, object]]) -> None:
    path.write_text(
        "".join(json.dumps(record, sort_keys=True) + "\n" for record in records),
        encoding="utf-8",
        newline="\n",
    )


def read_jsonl(path: Path) -> list[dict[str, object]]:
    return [json.loads(line) for line in path.read_text(encoding="utf-8").splitlines()]


def sha256(path: Path) -> str:
    return hashlib.sha256(path.read_bytes()).hexdigest()


class TextDeduplicationWorkloadTests(unittest.TestCase):
    def test_streaming_dedup_emits_deterministic_groups_decisions_and_metrics(
        self,
    ) -> None:
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            source = root / "documents.jsonl"
            groups = root / "groups.jsonl"
            decisions = root / "decisions.jsonl"
            metrics = root / "metrics.json"
            write_jsonl(
                source,
                [
                    {"id": "first", "text": "Alpha beta gamma"},
                    {"id": "duplicate", "text": "Alpha beta gamma"},
                    {"id": "unique", "text": "Red green blue"},
                ],
            )

            self.assertEqual(
                workload.main(
                    [
                        "dedupe",
                        "--input",
                        str(source),
                        "--groups-output",
                        str(groups),
                        "--decisions-output",
                        str(decisions),
                        "--metrics-output",
                        str(metrics),
                        "--exact",
                        "--exact-threshold",
                        "1.0",
                        "--shingle-size",
                        "1",
                        "--num-perm",
                        "64",
                        "--seed",
                        "7",
                        "--batch-size",
                        "2",
                    ]
                ),
                0,
            )

            group_rows = read_jsonl(groups)
            self.assertEqual(len(group_rows), 1)
            self.assertEqual(
                [member["id"] for member in group_rows[0]["members"]],
                ["first", "duplicate"],
            )
            self.assertEqual(group_rows[0]["representative"]["id"], "first")

            decision_rows = read_jsonl(decisions)
            self.assertEqual(
                [(row["id"], row["keep"]) for row in decision_rows],
                [("first", True), ("duplicate", False), ("unique", True)],
            )
            report = json.loads(metrics.read_text(encoding="utf-8"))
            self.assertEqual(report["schema_version"], 1)
            self.assertEqual(report["workload"], "text-deduplication")
            self.assertEqual(report["metrics"]["input_items"]["value"], 3)
            self.assertEqual(report["metrics"]["duplicate_count"]["value"], 1)
            self.assertGreaterEqual(
                report["metrics"]["exact_pairs_checked"]["value"], 1
            )


class CrossCorpusAuditWorkloadTests(unittest.TestCase):
    def test_reference_index_is_reusable_and_audit_does_not_mutate_it(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            reference_source = root / "reference.jsonl"
            query_source = root / "queries.jsonl"
            manifest = root / "reference-manifest.json"
            write_jsonl(
                reference_source,
                [
                    {"id": "train-a", "text": "Alpha beta gamma"},
                    {"id": "train-b", "text": "Red green blue"},
                ],
            )
            write_jsonl(
                query_source,
                [
                    {"id": "eval-overlap", "text": "Alpha beta gamma"},
                    {"id": "eval-clean", "text": "Entirely novel words"},
                ],
            )

            self.assertEqual(
                workload.main(
                    [
                        "build-reference",
                        "--input",
                        str(reference_source),
                        "--manifest",
                        str(manifest),
                        "--shingle-size",
                        "1",
                        "--num-perm",
                        "64",
                        "--seed",
                        "7",
                        "--batch-size",
                        "1",
                    ]
                ),
                0,
            )

            manifest_data = json.loads(manifest.read_text(encoding="utf-8"))
            index_path = root / manifest_data["index_path"]
            before_hash = sha256(index_path)
            before_stat = index_path.stat()
            build_metrics_path = manifest.with_suffix(".metrics.json")
            build_report = json.loads(build_metrics_path.read_text(encoding="utf-8"))
            self.assertEqual(build_report["workload"], "text-reference-build")
            self.assertEqual(build_report["metrics"]["input_items"]["value"], 2)
            self.assertGreater(build_report["metrics"]["index_bytes"]["value"], 0)

            first_output = root / "audit-first.jsonl"
            first_metrics = root / "audit-first-metrics.json"
            audit_arguments = [
                "audit",
                "--input",
                str(query_source),
                "--manifest",
                str(manifest),
                "--output",
                str(first_output),
                "--metrics-output",
                str(first_metrics),
                "--exact",
                "--exact-threshold",
                "1.0",
                "--batch-size",
                "1",
            ]
            self.assertEqual(workload.main(audit_arguments), 0)
            self.assertEqual(sha256(index_path), before_hash)
            self.assertEqual(index_path.stat().st_mtime_ns, before_stat.st_mtime_ns)

            audit_rows = read_jsonl(first_output)
            self.assertEqual(len(audit_rows), 2)
            self.assertEqual(audit_rows[0]["query"]["id"], "eval-overlap")
            self.assertEqual(
                [match["id"] for match in audit_rows[0]["reference_matches"]],
                ["train-a"],
            )
            self.assertFalse(audit_rows[1]["matched"])

            report = json.loads(first_metrics.read_text(encoding="utf-8"))
            self.assertEqual(report["workload"], "text-cross-corpus-audit")
            self.assertEqual(report["metrics"]["matched_query_count"]["value"], 1)
            self.assertEqual(report["metrics"]["unmatched_query_count"]["value"], 1)
            self.assertGreater(report["metrics"]["candidate_reduction"]["value"], 0.0)

            second_output = root / "audit-second.jsonl"
            second_metrics = root / "audit-second-metrics.json"
            second_arguments = audit_arguments.copy()
            second_arguments[second_arguments.index(str(first_output))] = str(
                second_output
            )
            second_arguments[second_arguments.index(str(first_metrics))] = str(
                second_metrics
            )
            self.assertEqual(workload.main(second_arguments), 0)
            self.assertEqual(first_output.read_bytes(), second_output.read_bytes())
            self.assertEqual(sha256(index_path), before_hash)


if __name__ == "__main__":
    unittest.main()
