from __future__ import annotations

import importlib.util
import os
import re
import tempfile
import unittest
from pathlib import Path

SCRIPT = Path(__file__).resolve().parents[1] / "release.py"
ROOT = SCRIPT.parent.parent
SPEC = importlib.util.spec_from_file_location("pari_release", SCRIPT)
if SPEC is None or SPEC.loader is None:
    raise RuntimeError("could not load release utility")
release = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(release)


class ReleaseMetadataTests(unittest.TestCase):
    def test_release_contract_is_self_consistent(self) -> None:
        self.assertEqual(release.workspace_version(), "0.1.0")
        release.validate()

    def test_tag_must_match_workspace_version(self) -> None:
        release.validate("v0.1.0")
        with self.assertRaises(SystemExit):
            release.validate("v9.9.9")


class ArtifactTests(unittest.TestCase):
    def test_checksums_are_sorted_and_exclude_output_file(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            directory = Path(temporary)
            output = directory / "SHA256SUMS"
            (directory / "z.bin").write_bytes(b"z")
            (directory / "a.bin").write_bytes(b"a")

            release.write_checksums(directory, output)
            first = output.read_text(encoding="utf-8")
            release.write_checksums(directory, output)
            second = output.read_text(encoding="utf-8")

            self.assertEqual(first, second)
            lines = first.splitlines()
            self.assertTrue(lines[0].endswith("  a.bin"))
            self.assertTrue(lines[1].endswith("  z.bin"))
            self.assertNotIn("SHA256SUMS", first)

    def test_archives_ignore_source_mtime_and_owner_metadata(self) -> None:
        previous_epoch = os.environ.get("SOURCE_DATE_EPOCH")
        os.environ["SOURCE_DATE_EPOCH"] = "123456789"
        try:
            with tempfile.TemporaryDirectory() as temporary:
                root = Path(temporary)
                staging = root / "pari-0.1.0-test"
                staging.mkdir()
                binary = staging / "pari"
                binary.write_bytes(b"binary")
                binary.chmod(0o755)
                license_file = staging / "LICENSE"
                license_file.write_text("license", encoding="utf-8")

                first_tar = root / "first.tar.gz"
                second_tar = root / "second.tar.gz"
                first_zip = root / "first.zip"
                second_zip = root / "second.zip"

                release.write_deterministic_tar(first_tar, staging, staging.name)
                release.write_deterministic_zip(first_zip, staging, staging.name)

                os.utime(binary, (1_900_000_000, 1_900_000_000))
                os.utime(license_file, (1_800_000_000, 1_800_000_000))

                release.write_deterministic_tar(second_tar, staging, staging.name)
                release.write_deterministic_zip(second_zip, staging, staging.name)

                self.assertEqual(first_tar.read_bytes(), second_tar.read_bytes())
                self.assertEqual(first_zip.read_bytes(), second_zip.read_bytes())
        finally:
            if previous_epoch is None:
                os.environ.pop("SOURCE_DATE_EPOCH", None)
            else:
                os.environ["SOURCE_DATE_EPOCH"] = previous_epoch


class ReadmeOnboardingTests(unittest.TestCase):
    def test_public_install_commands_track_workspace_version(self) -> None:
        version = release.workspace_version()
        readme = (ROOT / "README.md").read_text(encoding="utf-8")

        expected = [
            f"pari-similarity=={version}",
            f"cargo add pari-core@{version} pari-index@{version} pari-store@{version}",
            f'pari-format = "{version}"',
            f"pari-{version}-linux.tar.gz",
            f"pari-{version}-macos.tar.gz",
            f"pari-{version}-windows.zip",
            "The distribution name is `pari-similarity`; the import name is `pari`",
        ]
        for value in expected:
            self.assertIn(value, readme)

    def test_relative_readme_links_exist(self) -> None:
        readme = (ROOT / "README.md").read_text(encoding="utf-8")
        links = re.findall(r"\]\(([^)]+)\)", readme)
        relative_links = [
            link.split("#", 1)[0]
            for link in links
            if "://" not in link and not link.startswith("#")
        ]
        for link in relative_links:
            self.assertTrue((ROOT / link).exists(), link)


if __name__ == "__main__":
    unittest.main()
