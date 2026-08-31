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
