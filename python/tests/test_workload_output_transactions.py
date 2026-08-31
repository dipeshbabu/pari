from __future__ import annotations

import importlib.util
import json
import sys
import tempfile
import unittest
from pathlib import Path
from types import SimpleNamespace
from unittest import mock

from pari._outputs import StagedOutputs

ROOT = Path(__file__).resolve().parents[2]


def load(name: str, relative: str):
    spec = importlib.util.spec_from_file_location(name, ROOT / relative)
    if spec is None or spec.loader is None:
        raise RuntimeError(f"could not load {relative}")
    module = importlib.util.module_from_spec(spec)
    sys.modules[spec.name] = module
    spec.loader.exec_module(module)
    return module


code = load("pari_code_transaction_test", "examples/code_workload.py")
entity = load("pari_entity_transaction_test", "examples/entity_matching.py")
text = load("pari_text_transaction_test", "examples/text_workload.py")


class WorkloadOutputTransactionTests(unittest.TestCase):
    def test_concurrent_destination_is_not_replaced_or_removed(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            namespace = SimpleNamespace(output=root / "result")
            real_link = __import__("os").link

            def raced_link(source, destination, *, follow_symlinks=True):
                Path(destination).write_text("other process", encoding="utf-8")
                return real_link(source, destination, follow_symlinks=follow_symlinks)

            with (
                mock.patch("os.link", side_effect=raced_link),
                self.assertRaises(FileExistsError),
                StagedOutputs(namespace, ("output",)),
            ):
                namespace.output.write_text("pari output", encoding="utf-8")
            self.assertEqual(
                (root / "result").read_text(encoding="utf-8"), "other process"
            )
            self.assertEqual(list(root.glob(".*.pari-workload-*.tmp*")), [])

    def test_later_claim_failure_rolls_back_only_owned_outputs(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            namespace = SimpleNamespace(first=root / "first", second=root / "second")
            real_link = __import__("os").link
            calls = 0

            def fail_second_link(source, destination, *, follow_symlinks=True):
                nonlocal calls
                calls += 1
                if calls == 2:
                    Path(destination).write_text("other process", encoding="utf-8")
                return real_link(source, destination, follow_symlinks=follow_symlinks)

            with (
                mock.patch("os.link", side_effect=fail_second_link),
                self.assertRaises(FileExistsError),
                StagedOutputs(namespace, ("first", "second")),
            ):
                namespace.first.write_text("first output", encoding="utf-8")
                namespace.second.write_text("second output", encoding="utf-8")
            self.assertFalse((root / "first").exists())
            self.assertEqual(
                (root / "second").read_text(encoding="utf-8"), "other process"
            )
            self.assertEqual(list(root.glob(".*.pari-workload-*.tmp*")), [])

    def test_cleanup_error_retains_original_failure_as_cause(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            namespace = SimpleNamespace(output=Path(directory) / "result")
            with (
                mock.patch.object(
                    Path, "unlink", side_effect=PermissionError("cleanup blocked")
                ),
                self.assertRaisesRegex(RuntimeError, "failed to clean") as captured,
                StagedOutputs(namespace, ("output",)),
            ):
                raise ValueError("ingestion failed")
            self.assertIsInstance(captured.exception.__cause__, ValueError)
            self.assertEqual(str(captured.exception.__cause__), "ingestion failed")

    def test_malformed_second_batch_leaves_no_workflow_outputs(self) -> None:
        cases = (
            (
                "code",
                code.main,
                {"repository": "repo", "path": "ok.py", "content": "value = 1"},
                lambda root, source, outputs: [
                    "--input-jsonl",
                    str(source),
                    "--groups-output",
                    str(outputs[0]),
                    "--decisions-output",
                    str(outputs[1]),
                    "--metrics-output",
                    str(outputs[2]),
                    "--index",
                    str(outputs[3]),
                    "--batch-size",
                    "1",
                ],
            ),
            (
                "entity",
                entity.main,
                {"id": "ok", "name": "Valid Person"},
                lambda root, source, outputs: [
                    "--input",
                    str(source),
                    "--profile",
                    "customer",
                    "--pairs-output",
                    str(outputs[0]),
                    "--groups-output",
                    str(outputs[1]),
                    "--metrics-output",
                    str(outputs[2]),
                    "--index",
                    str(outputs[3]),
                    "--batch-size",
                    "1",
                ],
            ),
            (
                "text",
                text.main,
                {"id": "ok", "text": "valid text"},
                lambda root, source, outputs: [
                    "dedupe",
                    "--input",
                    str(source),
                    "--groups-output",
                    str(outputs[0]),
                    "--decisions-output",
                    str(outputs[1]),
                    "--metrics-output",
                    str(outputs[2]),
                    "--index",
                    str(outputs[3]),
                    "--batch-size",
                    "1",
                ],
            ),
        )
        for name, main, valid, arguments in cases:
            with (
                self.subTest(workload=name),
                tempfile.TemporaryDirectory() as directory,
            ):
                root = Path(directory)
                source = root / "input.jsonl"
                source.write_text(
                    json.dumps(valid) + "\n{bad-json}\n", encoding="utf-8", newline="\n"
                )
                outputs = [
                    root / f"{name}-{suffix}"
                    for suffix in ("first", "second", "metrics", "index.pari")
                ]
                with self.assertRaises(ValueError):
                    main(arguments(root, source, outputs))
                self.assertTrue(all(not path.exists() for path in outputs))
                self.assertEqual(list(root.glob(".*.pari-workload-*.tmp*")), [])


if __name__ == "__main__":
    unittest.main()
