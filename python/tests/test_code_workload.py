from __future__ import annotations

import importlib.util
import json
import sys
import tempfile
import unittest
from pathlib import Path

ROOT = Path(__file__).resolve().parents[2]
SCRIPT = ROOT / "examples" / "code_workload.py"
FIXTURE = ROOT / "examples" / "code_corpus_fixture"
SPEC = importlib.util.spec_from_file_location("pari_code_workload", SCRIPT)
if SPEC is None or SPEC.loader is None:
    raise RuntimeError("could not load code workload example")
workload = importlib.util.module_from_spec(SPEC)
sys.modules[SPEC.name] = workload
SPEC.loader.exec_module(workload)


def read_jsonl(path: Path) -> list[dict[str, object]]:
    return [json.loads(line) for line in path.read_text(encoding="utf-8").splitlines()]


class CodeWorkloadTests(unittest.TestCase):
    def test_language_neutral_lexer_normalizes_literals_deterministically(self) -> None:
        first = workload.code_tokens("value = 17; name = 'alpha'")
        second = workload.code_tokens('value = 99; name = "beta"')
        self.assertEqual(first, second)
        self.assertEqual(
            first, ["value", "=", "<number>", ";", "name", "=", "<string>"]
        )

    def test_fixture_dedup_is_deterministic_for_memory_and_persistent_indexes(
        self,
    ) -> None:
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            memory = self.run_fixture(root, "memory", persistent=False)
            persistent = self.run_fixture(root, "persistent", persistent=True)

            self.assertEqual(
                memory["groups"].read_bytes(), persistent["groups"].read_bytes()
            )
            self.assertEqual(
                memory["decisions"].read_bytes(), persistent["decisions"].read_bytes()
            )

            groups = read_jsonl(memory["groups"])
            self.assertEqual(len(groups), 1)
            identities = [
                (member["repository"], member["path"])
                for member in groups[0]["members"]
            ]
            self.assertEqual(
                identities,
                [
                    ("repo-alpha", "src/checksum.py"),
                    ("repo-beta", "lib/checksum_copy.py"),
                ],
            )
            self.assertEqual(groups[0]["representative"]["repository"], "repo-alpha")

            decisions = read_jsonl(memory["decisions"])
            self.assertEqual([row["keep"] for row in decisions], [True, False, True])
            report = json.loads(memory["metrics"].read_text(encoding="utf-8"))
            self.assertEqual(report["schema_version"], 1)
            self.assertEqual(report["workload"], "code-corpus-deduplication")
            self.assertEqual(report["metrics"]["discovered_files"]["value"], 3)
            self.assertEqual(report["metrics"]["input_items"]["value"], 3)
            self.assertEqual(report["metrics"]["duplicate_count"]["value"], 1)
            self.assertGreaterEqual(
                report["metrics"]["exact_pairs_checked"]["value"], 1
            )
            self.assertGreater(report["metrics"]["candidate_item_rate"]["value"], 0.0)

            persistent_report = json.loads(
                persistent["metrics"].read_text(encoding="utf-8")
            )
            self.assertGreater(persistent_report["metrics"]["index_bytes"]["value"], 0)

    def test_jsonl_records_and_directory_iteration_are_streaming(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            source = root / "records.jsonl"
            rows = [
                {"repository": "one", "path": "a.py", "content": "answer = 17"},
                {"repository": "two", "path": "b.py", "content": "answer = 42"},
                {
                    "repository": "two",
                    "path": "c.py",
                    "content": "return_value = call()",
                },
            ]
            source.write_text(
                "".join(json.dumps(row, sort_keys=True) + "\n" for row in rows),
                encoding="utf-8",
                newline="\n",
            )
            groups = root / "groups.jsonl"
            decisions = root / "decisions.jsonl"
            metrics = root / "metrics.json"
            self.assertEqual(
                workload.main(
                    [
                        "--input-jsonl",
                        str(source),
                        "--groups-output",
                        str(groups),
                        "--decisions-output",
                        str(decisions),
                        "--metrics-output",
                        str(metrics),
                        "--shingle-size",
                        "1",
                        "--num-perm",
                        "64",
                        "--seed",
                        "7",
                        "--exact",
                        "--exact-threshold",
                        "1.0",
                    ]
                ),
                0,
            )
            self.assertEqual(len(read_jsonl(groups)), 1)

            files = root / "files"
            files.mkdir()
            (files / "a.py").write_text("first = 1\n", encoding="utf-8")
            (files / "z.py").write_text("last = 2\n", encoding="utf-8")
            stats = workload.TraversalStats()
            iterator = workload.iter_directory_inputs(
                [("stream", files)],
                frozenset({".py"}),
                1,
                1024,
                False,
                stats,
            )
            first, _features = next(iterator)
            self.assertEqual(first.path, "a.py")
            self.assertEqual(stats.discovered, 1)
            self.assertEqual(stats.accepted, 1)

    def test_directory_skip_policy_is_counted(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            (root / "accepted.py").write_text("value = 1\n", encoding="utf-8")
            (root / ".hidden.py").write_text("hidden = 1\n", encoding="utf-8")
            (root / "binary.py").write_bytes(b"value = \0")
            (root / "empty.py").write_text("   \n", encoding="utf-8")
            (root / "ignored.txt").write_text("text = 1\n", encoding="utf-8")
            (root / "oversize.py").write_text("value = 123456\n", encoding="utf-8")
            stats = workload.TraversalStats()
            records = list(
                workload.iter_directory_inputs(
                    [("repo", root)],
                    frozenset({".py"}),
                    1,
                    12,
                    False,
                    stats,
                )
            )
            self.assertEqual(
                [reference.path for reference, _features in records], ["accepted.py"]
            )
            self.assertEqual(stats.accepted, 1)
            self.assertEqual(stats.skipped_hidden, 1)
            self.assertEqual(stats.skipped_binary, 1)
            self.assertEqual(stats.skipped_empty, 1)
            self.assertEqual(stats.skipped_extension, 1)
            self.assertEqual(stats.skipped_oversize, 1)

    def run_fixture(
        self, root: Path, name: str, *, persistent: bool
    ) -> dict[str, Path]:
        groups = root / f"{name}-groups.jsonl"
        decisions = root / f"{name}-decisions.jsonl"
        metrics = root / f"{name}-metrics.json"
        arguments = [
            "--root",
            f"repo-alpha={FIXTURE / 'repo-alpha'}",
            "--root",
            f"repo-beta={FIXTURE / 'repo-beta'}",
            "--groups-output",
            str(groups),
            "--decisions-output",
            str(decisions),
            "--metrics-output",
            str(metrics),
            "--shingle-size",
            "3",
            "--num-perm",
            "64",
            "--seed",
            "7",
            "--batch-size",
            "2",
            "--threads",
            "2",
            "--exact",
            "--exact-threshold",
            "1.0",
        ]
        output = {"groups": groups, "decisions": decisions, "metrics": metrics}
        if persistent:
            index = root / f"{name}.pari"
            arguments.extend(["--index", str(index)])
            output["index"] = index
        self.assertEqual(workload.main(arguments), 0)
        return output


if __name__ == "__main__":
    unittest.main()
