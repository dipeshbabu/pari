from __future__ import annotations

import unittest
from pathlib import Path

ROOT = Path(__file__).resolve().parents[2]
RUFF_VERSION = "0.16.5"
RUFF_TARGETS = "python scripts benchmarks examples"


class FormattingPolicyTests(unittest.TestCase):
    def test_ruff_version_and_commands_are_pinned_consistently(self) -> None:
        pyproject = (ROOT / "pyproject.toml").read_text(encoding="utf-8")
        workflow = (ROOT / ".github/workflows/ci.yml").read_text(encoding="utf-8")

        self.assertIn(f'dev = ["ruff=={RUFF_VERSION}"]', pyproject)
        self.assertIn(f'RUFF_VERSION: "{RUFF_VERSION}"', workflow)
        self.assertIn('python -m pip install "ruff==${RUFF_VERSION}"', workflow)
        self.assertIn(f"ruff format --check {RUFF_TARGETS}", workflow)
        self.assertIn(f"ruff check {RUFF_TARGETS}", workflow)

    def test_policy_is_explicit_and_protects_benchmark_fixtures(self) -> None:
        pyproject = (ROOT / "pyproject.toml").read_text(encoding="utf-8")

        self.assertIn('target-version = "py310"', pyproject)
        self.assertIn("line-length = 88", pyproject)
        self.assertIn('"examples/code_corpus_fixture/**"', pyproject)
        self.assertIn('"*.md"', pyproject)
        self.assertIn("[tool.ruff.lint]", pyproject)
        self.assertIn("select = [", pyproject)
        self.assertNotIn("preview = true", pyproject)


if __name__ == "__main__":
    unittest.main()
