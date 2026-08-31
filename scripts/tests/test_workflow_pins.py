from __future__ import annotations

import importlib.util
import tempfile
import unittest
from pathlib import Path

SCRIPT = Path(__file__).resolve().parents[1] / "check_workflow_pins.py"
SPEC = importlib.util.spec_from_file_location("pari_workflow_pins", SCRIPT)
if SPEC is None or SPEC.loader is None:
    raise RuntimeError("could not load workflow pin checker")
pins = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(pins)


class WorkflowPinTests(unittest.TestCase):
    def test_repository_workflows_are_immutable(self) -> None:
        self.assertEqual(pins.pin_errors(pins.ROOT / ".github" / "workflows"), [])

    def test_floating_action_and_image_are_rejected(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            (root / "bad.yml").write_text(
                "steps:\n  - uses: actions/checkout@v5\nservices:\n  redis:\n    image: redis:7\n",
                encoding="utf-8",
            )
            errors = pins.pin_errors(root)
            self.assertEqual(len(errors), 2)


if __name__ == "__main__":
    unittest.main()
