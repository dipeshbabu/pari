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

    def test_high_signal_antipattern_rules_are_enforced_without_broad_groups(
        self,
    ) -> None:
        pyproject = (ROOT / "pyproject.toml").read_text(encoding="utf-8")
        workspace = (ROOT / "Cargo.toml").read_text(encoding="utf-8")
        criterion = (ROOT / "benchmarks/criterion/Cargo.toml").read_text(
            encoding="utf-8"
        )

        for rule in ("C4", "PERF", "PIE", "PLE", "PLW", "RET", "S"):
            self.assertIn(f'  "{rule}",', pyproject)
        for rule in (
            "TRY002",
            "TRY004",
            "TRY201",
            "TRY300",
            "TRY400",
            "TRY401",
            "RUF005",
            "RUF006",
            "RUF010",
            "RUF012",
            "RUF015",
            "RUF018",
            "RUF019",
            "RUF022",
            "RUF024",
        ):
            self.assertIn(f'  "{rule}",', pyproject)
        self.assertIn('ignore = ["PERF203"]', pyproject)
        self.assertNotIn('"ALL"', pyproject)

        for manifest in (workspace, criterion):
            self.assertIn('dbg_macro = "deny"', manifest)
            self.assertIn('todo = "deny"', manifest)
            self.assertIn('unimplemented = "deny"', manifest)
            self.assertNotIn('restriction = "deny"', manifest)


if __name__ == "__main__":
    unittest.main()
