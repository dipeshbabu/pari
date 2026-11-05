from __future__ import annotations

import hashlib
import io
import json
import struct
import tarfile
import tempfile
import unittest
import urllib.error
import urllib.parse
from pathlib import Path
from unittest import mock

from scripts import publish_crates as publisher

VERSION = "0.1.2"


def write_archive(directory: Path, name: str, *, extra: str | None = None) -> Path:
    prefix = f"{name}-{VERSION}"
    manifest = (
        f'[package]\nname = "{name}"\nversion = "{VERSION}"\n'
        'edition = "2021"\nrust-version = "1.81"\nlicense = "MIT"\n'
        'description = "Publication test fixture"\nreadme = "README.md"\n'
        '[dependencies.renamed]\npackage = "serde"\nversion = "1"\n'
        'default-features = false\nfeatures = ["derive"]\n'
        "[target.'cfg(unix)'.build-dependencies]\ncc = \"1\"\n"
    )
    files = {
        "Cargo.toml": manifest.encode(),
        "README.md": b"Fixture readme\n",
        "LICENSE": b"MIT\n",
        "NOTICE": b"Fixture attribution\n",
        "src/lib.rs": b"pub fn fixture() {}\n",
    }
    path = directory / f"{prefix}.crate"
    with tarfile.open(path, "w:gz") as archive:
        for filename, data in files.items():
            member = tarfile.TarInfo(f"{prefix}/{filename}")
            member.size = len(data)
            archive.addfile(member, io.BytesIO(data))
        if extra:
            archive.addfile(tarfile.TarInfo(extra), io.BytesIO())
    return path


def write_checksums(directory: Path) -> None:
    lines = [
        f"{hashlib.sha256(path.read_bytes()).hexdigest()}  {path.name}"
        for path in sorted(directory.iterdir())
        if path.name != "SHA256SUMS"
    ]
    (directory / "SHA256SUMS").write_text("\n".join(lines) + "\n", encoding="utf-8")


def fixture(directory: Path) -> list[publisher.PreparedCrate]:
    for name in publisher.CRATES:
        write_archive(directory, name)
    write_checksums(directory)
    return publisher.prepare_crates(directory, VERSION)


class PublicationTests(unittest.TestCase):
    def test_payload_retains_exact_verified_archive_and_dependency_semantics(
        self,
    ) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            directory = Path(temporary)
            prepared = fixture(directory)
            self.assertEqual([item.name for item in prepared], list(publisher.CRATES))
            for item in prepared:
                data = item.payload()
                metadata_length = struct.unpack("<I", data[:4])[0]
                metadata = json.loads(data[4 : 4 + metadata_length])
                offset = 4 + metadata_length
                archive_length = struct.unpack("<I", data[offset : offset + 4])[0]
                original = (directory / f"{item.name}-{VERSION}.crate").read_bytes()
                self.assertEqual(archive_length, len(original))
                self.assertEqual(data[offset + 4 :], original)
                self.assertEqual(metadata["vers"], VERSION)
                self.assertEqual(metadata["readme"], "Fixture readme\n")
                renamed, build = metadata["deps"]
                self.assertEqual(renamed["name"], "serde")
                self.assertEqual(renamed["explicit_name_in_toml"], "renamed")
                self.assertFalse(renamed["default_features"])
                self.assertEqual(renamed["features"], ["derive"])
                self.assertEqual(build["kind"], "build")
                self.assertEqual(build["target"], "cfg(unix)")

    def test_tampering_and_unlisted_artifacts_are_rejected(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            directory = Path(temporary)
            fixture(directory)
            target = directory / f"pari-core-{VERSION}.crate"
            original = target.read_bytes()
            target.write_bytes(original + b"changed")
            with self.assertRaisesRegex(ValueError, "checksum mismatch"):
                publisher.prepare_crates(directory, VERSION)
            target.write_bytes(original)
            (directory / "unlisted").write_bytes(b"unexpected")
            with self.assertRaisesRegex(ValueError, "cover every artifact"):
                publisher.prepare_crates(directory, VERSION)

    def test_unsafe_archive_members_are_rejected_even_with_matching_checksum(
        self,
    ) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            directory = Path(temporary)
            fixture(directory)
            write_archive(directory, "pari-core", extra="pari-core-0.1.2/../outside")
            write_checksums(directory)
            with self.assertRaisesRegex(ValueError, "unsafe"):
                publisher.prepare_crates(directory, VERSION)

    def test_publication_is_ordered_verified_and_idempotent(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            prepared = fixture(Path(temporary))
            expected = {item.name: item.checksum for item in prepared}
            published: list[str] = []

            def registry(url, *, payload=None, token=None):
                if payload is not None:
                    self.assertEqual(url, publisher.PUBLISH_URL)
                    self.assertEqual(token, "unit-test-token")
                    length = struct.unpack("<I", payload[:4])[0]
                    name = json.loads(payload[4 : 4 + length])["name"]
                    self.assertEqual(name, publisher.CRATES[len(published)])
                    published.append(name)
                    return b"{}"
                self.assertIsNone(token)
                parts = urllib.parse.urlsplit(url).path.split("/")
                if url.startswith("https://index.crates.io/"):
                    name = parts[-1]
                    return json.dumps(
                        {"vers": VERSION, "cksum": expected[name], "yanked": False}
                    ).encode()
                name = parts[-2]
                if name not in published:
                    return None
                return json.dumps(
                    {"version": {"checksum": expected[name], "yanked": False}}
                ).encode()

            with mock.patch.object(publisher, "registry_request", side_effect=registry):
                publisher.publish(prepared, "unit-test-token")
                publisher.publish(prepared, "unit-test-token")
            self.assertEqual(published, list(publisher.CRATES))

    def test_existing_mismatched_version_stops_before_upload(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            prepared = fixture(Path(temporary))
            with (
                mock.patch.object(
                    publisher,
                    "published_version",
                    return_value={"checksum": "wrong", "yanked": False},
                ),
                mock.patch.object(publisher, "registry_request") as request,
                self.assertRaisesRegex(RuntimeError, "does not match"),
            ):
                publisher.publish(prepared, "unit-test-token")
            request.assert_not_called()

    def test_credentials_are_not_forwarded_or_exposed_in_errors(self) -> None:
        sample_value = "unit-test-credential"
        with mock.patch("urllib.request.build_opener") as build:
            with self.assertRaisesRegex(ValueError, "endpoint"):
                publisher.registry_request(
                    "https://example.invalid/", payload=b"test", token=sample_value
                )
            build.assert_not_called()
            build.return_value.open.side_effect = urllib.error.HTTPError(
                publisher.PUBLISH_URL,
                302,
                "redirect",
                {},
                io.BytesIO(sample_value.encode()),
            )
            with self.assertRaisesRegex(RuntimeError, "redacted") as captured:
                publisher.registry_request(
                    publisher.PUBLISH_URL, payload=b"test", token=sample_value
                )
            self.assertNotIn(sample_value, str(captured.exception))
            self.assertIsInstance(build.call_args.args[0], publisher.NoRedirects)
            self.assertIsNone(
                publisher.NoRedirects().redirect_request(
                    None, None, 302, "", {}, "https://example.invalid/"
                )
            )
