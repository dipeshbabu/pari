from __future__ import annotations

import unittest
from pathlib import Path

ROOT = Path(__file__).resolve().parents[2]
WORKFLOWS = ROOT / ".github" / "workflows"
CACHE_SHA = "55cc8345863c7cc4c66a329aec7e433d2d1c52a9"


class CiWorkflowPolicyTests(unittest.TestCase):
    def workflow(self, name: str) -> str:
        return (WORKFLOWS / name).read_text(encoding="utf-8")

    def test_only_pull_request_runs_are_canceled(self) -> None:
        expected = "cancel-in-progress: ${{ github.event_name == 'pull_request' }}"
        group = (
            "group: ${{ github.workflow }}-"
            "${{ github.event.pull_request.number || github.run_id }}"
        )
        for name in ("ci.yml", "python.yml", "redis.yml", "release.yml"):
            with self.subTest(workflow=name):
                text = self.workflow(name)
                self.assertIn(expected, text)
                self.assertIn(group, text)
                self.assertNotIn("cancel-in-progress: true", text)
                self.assertNotIn("|| github.ref", text)

    def test_cargo_deny_cache_has_a_trusted_writer(self) -> None:
        text = self.workflow("ci.yml")
        save_step = text.split("      - name: Save reviewed cargo-deny binary", 1)[1]
        save_step = save_step.split("\n\n  test:", 1)[0]
        self.assertIn(f"uses: actions/cache/restore@{CACHE_SHA}", text)
        self.assertIn(f"uses: actions/cache/save@{CACHE_SHA}", save_step)
        self.assertIn("github.event_name == 'push'", save_step)
        self.assertIn("github.ref == 'refs/heads/main'", save_step)
        self.assertIn("steps.cargo-deny-cache.outputs.cache-hit != 'true'", text)
        self.assertIn('CARGO_DENY_VERSION: "0.20.2"', text)
        self.assertIn("${{ runner.os }}", text)
        self.assertIn("${{ runner.arch }}", text)
        self.assertIn("${{ env.RUST_TOOLCHAIN }}", text)
        self.assertIn("${{ env.CARGO_DENY_VERSION }}", text)
        self.assertIn("${{ hashFiles('.github/workflows/ci.yml') }}", text)
        self.assertIn(
            'cargo install cargo-deny --version "${CARGO_DENY_VERSION}" --locked',
            text,
        )
        self.assertIn("Report reviewed cargo-deny version", text)
        self.assertIn("cargo deny --version", text)
        self.assertIn("cargo deny check advisories licenses bans sources", text)


if __name__ == "__main__":
    unittest.main()
