from __future__ import annotations

import unittest
from pathlib import Path

ROOT = Path(__file__).resolve().parents[2]


class DependencyUpdatePolicyTests(unittest.TestCase):
    def test_rust_181_dependencies_stay_on_compatible_release_lines(self) -> None:
        workspace = (ROOT / "Cargo.toml").read_text(encoding="utf-8")
        core = (ROOT / "crates" / "pari-core" / "Cargo.toml").read_text(
            encoding="utf-8"
        )
        backend = (ROOT / "crates" / "pari-backend" / "Cargo.toml").read_text(
            encoding="utf-8"
        )
        cli = (ROOT / "crates" / "pari-cli" / "Cargo.toml").read_text(encoding="utf-8")
        python = (ROOT / "crates" / "pari-py" / "Cargo.toml").read_text(
            encoding="utf-8"
        )
        criterion = (ROOT / "benchmarks" / "criterion" / "Cargo.toml").read_text(
            encoding="utf-8"
        )
        criterion_lockfile = (
            ROOT / "benchmarks" / "criterion" / "Cargo.lock"
        ).read_text(encoding="utf-8")
        ci = (ROOT / ".github" / "workflows" / "ci.yml").read_text(encoding="utf-8")
        lockfile = (ROOT / "Cargo.lock").read_text(encoding="utf-8")

        self.assertIn('rust-version = "1.81"', workspace)
        self.assertIn('sha1 = "0.10.6"', core)
        self.assertIn('redis = { version = "=0.32.7"', backend)
        self.assertIn('url = { version = "=2.5.2"', backend)
        self.assertIn('clap = { version = "=4.5.57"', cli)
        self.assertIn('clap_complete = "=4.5.67"', cli)
        self.assertIn('rust-version = "1.83"', python)
        self.assertIn('pyo3 = { version = "0.29.2"', python)
        self.assertIn('rust-version = "1.81"', criterion)
        self.assertIn('clap = "=4.5.57"', criterion)
        self.assertIn('redb = "=2.4.0"', criterion)
        self.assertIn('name = "sha1"\nversion = "0.10.7"', lockfile)
        self.assertIn('name = "redis"\nversion = "0.32.7"', lockfile)
        self.assertIn('name = "clap_lex"\nversion = "0.7.7"', lockfile)
        self.assertIn('name = "url"\nversion = "2.5.2"', lockfile)
        self.assertIn('name = "idna"\nversion = "0.5.0"', lockfile)
        self.assertIn('name = "pyo3"\nversion = "0.29.2"', lockfile)
        self.assertIn('name = "clap_lex"\nversion = "0.7.7"', criterion_lockfile)
        self.assertIn('name = "redb"\nversion = "2.4.0"', criterion_lockfile)
        self.assertIn(
            "cargo check --locked --workspace --all-features\n          --exclude pari-py",
            ci,
        )
        self.assertIn("cargo check --locked -p pari-cli", ci)
        self.assertIn(
            "cargo check --locked -p pari-backend --features redis",
            ci,
        )
        self.assertIn("--manifest-path benchmarks/criterion/Cargo.toml", ci)
        self.assertIn('toolchain: "1.83.0"', ci)
        self.assertIn("cargo +1.83.0 check --locked -p pari-py", ci)

    def test_dependabot_ignores_only_incompatible_update_classes(self) -> None:
        dependabot = (ROOT / ".github" / "dependabot.yml").read_text(encoding="utf-8")
        self.assertIn("- dependency-name: redis", dependabot)
        self.assertIn('update-types: ["version-update:semver-major"]', dependabot)
        self.assertIn("- dependency-name: sha1", dependabot)
        self.assertIn('"version-update:semver-minor"', dependabot)
        self.assertIn('"version-update:semver-major"', dependabot)
        self.assertNotIn('"version-update:semver-patch"', dependabot)
        self.assertIn("- dependency-name: clap", dependabot)
        self.assertIn('versions: [">= 4.5.58"]', dependabot)
        self.assertIn("- dependency-name: clap_complete", dependabot)
        self.assertIn('versions: [">= 4.6.0"]', dependabot)
        self.assertIn("- dependency-name: url", dependabot)
        self.assertIn('versions: [">= 2.5.4"]', dependabot)
        self.assertNotIn("- dependency-name: pyo3", dependabot)


if __name__ == "__main__":
    unittest.main()
