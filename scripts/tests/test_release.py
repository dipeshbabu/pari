from __future__ import annotations

import importlib.util
import io
import os
import re
import tarfile
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
        self.assertEqual(release.workspace_version(), "0.2.0")
        release.validate()

    def test_tag_must_match_workspace_version(self) -> None:
        release.validate("v0.2.0")
        with self.assertRaises(SystemExit):
            release.validate("v9.9.9")


class ArtifactTests(unittest.TestCase):
    def write_sdist(self, path: Path, names: list[str]) -> None:
        with tarfile.open(path, "w:gz") as archive:
            for name in names:
                contents = b"fixture"
                info = tarfile.TarInfo(f"pari_similarity-0.2.0/{name}")
                info.size = len(contents)
                archive.addfile(info, io.BytesIO(contents))

    def test_sdist_manifest_rejects_missing_forbidden_and_duplicate_files(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            valid = root / "valid.tar.gz"
            expected = {"LICENSE", "python/pari/api.py"}
            self.write_sdist(valid, sorted(expected))
            release.validate_sdist_archive(valid, expected)

            missing = root / "missing.tar.gz"
            self.write_sdist(missing, ["LICENSE"])
            with self.assertRaisesRegex(SystemExit, "missing expected sdist file"):
                release.validate_sdist_archive(missing, expected)

            forbidden = root / "forbidden.tar.gz"
            self.write_sdist(forbidden, [*sorted(expected), ".env"])
            with self.assertRaisesRegex(SystemExit, "forbidden sdist file"):
                release.validate_sdist_archive(forbidden, expected)

            duplicate = root / "duplicate.tar.gz"
            self.write_sdist(duplicate, [*sorted(expected), "LICENSE"])
            with self.assertRaisesRegex(SystemExit, "duplicate archive member"):
                release.validate_sdist_archive(duplicate, expected)

    def test_declared_sdist_manifest_rejects_stale_and_duplicate_entries(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            for required in release.REQUIRED_SDIST_ROOTS:
                path = root / required
                path.parent.mkdir(parents=True, exist_ok=True)
                path.write_text("fixture", encoding="utf-8")
            package = root / "python" / "pari"
            package.mkdir(parents=True)
            (package / "__init__.py").write_text("", encoding="utf-8")
            asset = root / "docs" / "guide.md"
            asset.parent.mkdir()
            asset.write_text("guide", encoding="utf-8")
            pyproject = {
                "tool": {
                    "maturin": {
                        "include": [{"path": "docs/guide.md", "format": "sdist"}]
                    }
                }
            }
            expected = release.expected_sdist_paths(root, pyproject)
            self.assertIn("docs/guide.md", expected)
            self.assertIn("python/pari/__init__.py", expected)

            pyproject["tool"]["maturin"]["include"].append(
                {"path": "docs/guide.md", "format": "sdist"}
            )
            with self.assertRaisesRegex(ValueError, "duplicate"):
                release.expected_sdist_paths(root, pyproject)

            pyproject["tool"]["maturin"]["include"] = [
                {"path": "docs/missing.md", "format": "sdist"}
            ]
            with self.assertRaisesRegex(ValueError, "does not exist"):
                release.expected_sdist_paths(root, pyproject)

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
                staging = root / "pari-0.2.0-test"
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
