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
        lockfile = (ROOT / "Cargo.lock").read_text(encoding="utf-8")

        self.assertIn('rust-version = "1.81"', workspace)
        self.assertIn('sha1 = "0.10.6"', core)
        self.assertIn('redis = { version = "=0.32.7"', backend)
        self.assertIn('name = "sha1"\nversion = "0.10.7"', lockfile)
        self.assertIn('name = "redis"\nversion = "0.32.7"', lockfile)

    def test_dependabot_ignores_only_incompatible_update_classes(self) -> None:
        dependabot = (ROOT / ".github" / "dependabot.yml").read_text(
            encoding="utf-8"
        )
        self.assertIn("- dependency-name: redis", dependabot)
        self.assertIn(
            'update-types: ["version-update:semver-major"]', dependabot
        )
        self.assertIn("- dependency-name: sha1", dependabot)
        self.assertIn('"version-update:semver-minor"', dependabot)
        self.assertIn('"version-update:semver-major"', dependabot)
        self.assertNotIn('"version-update:semver-patch"', dependabot)


if __name__ == "__main__":
    unittest.main()
