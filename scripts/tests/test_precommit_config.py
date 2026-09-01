from __future__ import annotations

import re
import unittest
from pathlib import Path

ROOT = Path(__file__).resolve().parents[2]
CONFIG = ROOT / ".pre-commit-config.yaml"
CI = ROOT / ".github" / "workflows" / "ci.yml"
README = ROOT / "README.md"
CONTRIBUTING = ROOT / "CONTRIBUTING.md"
SAMPLES = ROOT / "scripts" / "check_precommit_samples.py"


class PreCommitPolicyTests(unittest.TestCase):
    def test_external_hooks_are_immutable_and_complete(self) -> None:
        text = CONFIG.read_text(encoding="utf-8")
        repositories = re.findall(
            r"(?m)^  - repo: (https://\S+)\n    rev: ([^\s#]+)", text
        )
        self.assertEqual(
            repositories,
            [
                (
                    "https://github.com/pre-commit/pre-commit-hooks",
                    "3e8a8703264a2f4a69428a0aa4dcb512790b2c8c",
                ),
                (
                    "https://github.com/astral-sh/ruff-pre-commit",
                    "1f1e8bf348ff38fc88619a38d3ca4d9c56abea49",
                ),
            ],
        )
        for _repository, revision in repositories:
            self.assertRegex(revision, r"^[0-9a-f]{40}$")

        hook_ids = set(re.findall(r"(?m)^      - id: ([a-z0-9-]+)$", text))
        self.assertTrue(
            {
                "trailing-whitespace",
                "end-of-file-fixer",
                "check-yaml",
                "check-toml",
                "check-json",
                "check-ast",
                "check-merge-conflict",
                "detect-private-key",
                "ruff-check",
                "ruff-format",
                "cargo-fmt-workspace",
                "cargo-fmt-criterion",
                "workflow-pins",
                "check-hooks-apply",
                "check-useless-excludes",
            }.issubset(hook_ids)
        )

    def test_commit_hooks_stay_fast_and_cover_repository_policy(self) -> None:
        text = CONFIG.read_text(encoding="utf-8")
        self.assertIn('minimum_pre_commit_version: "4.6.2"', text)
        self.assertIn("default_stages: [pre-commit]", text)
        self.assertIn("entry: cargo fmt --all -- --check", text)
        self.assertIn(
            "entry: cargo fmt --manifest-path benchmarks/criterion/Cargo.toml --all -- --check",
            text,
        )
        self.assertIn("entry: python scripts/check_workflow_pins.py", text)
        self.assertIn("args: [--assume-in-merge]", text)
        self.assertIn("args: [--fix]", text)
        for expensive in (
            "cargo clippy",
            "cargo test",
            "cargo deny",
            "maturin",
            "release.py",
            "benchmark_campaign.py",
            "redis-cli",
        ):
            self.assertNotIn(expensive, text)

    def test_ci_and_documentation_use_the_pinned_framework(self) -> None:
        ci = CI.read_text(encoding="utf-8")
        self.assertIn('PRE_COMMIT_VERSION: "4.6.2"', ci)
        self.assertIn('python -m pip install "pre-commit==${PRE_COMMIT_VERSION}"', ci)
        self.assertIn("python -m pre_commit run --all-files --show-diff-on-failure", ci)
        self.assertIn("python scripts/check_precommit_samples.py", ci)

        for document in (README, CONTRIBUTING):
            text = document.read_text(encoding="utf-8")
            normalized = " ".join(text.split())
            self.assertIn('python -m pip install "pre-commit==4.6.2"', text)
            self.assertIn("pre-commit install", text)
            self.assertIn("pre-commit run --all-files", text)
            self.assertIn("CI remains authoritative", normalized)

    def test_malformed_sample_probe_covers_every_external_hook(self) -> None:
        script = SAMPLES.read_text(encoding="utf-8")
        for hook_id in (
            "trailing-whitespace",
            "end-of-file-fixer",
            "check-yaml",
            "check-toml",
            "check-json",
            "check-ast",
            "check-merge-conflict",
            "detect-private-key",
            "ruff-check",
            "ruff-format",
            "cargo-fmt-workspace",
            "cargo-fmt-criterion",
        ):
            self.assertIn(f'"{hook_id}"', script)


if __name__ == "__main__":
    unittest.main()
