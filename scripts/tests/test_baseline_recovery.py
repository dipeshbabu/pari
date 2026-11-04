from __future__ import annotations

import csv
import io
import tempfile
import unittest
import zipfile
from pathlib import Path

from scripts.recover_pypi_baseline import rebuild_wheel, record_digest


class BaselineRecoveryTests(unittest.TestCase):
    def fixture(self) -> tuple[bytes, dict[str, bytes]]:
        root = "pari_similarity-0.1.0.dist-info"
        members = {
            "pari/_native.so": b"unchanged native extension",
            f"{root}/METADATA": b"Name: pari-similarity\nVersion: 0.1.0\n",
            f"{root}/WHEEL": b"Wheel-Version: 1.0\nTag: cp310-abi3-linux_x86_64\n",
        }
        buffer = io.StringIO(newline="")
        writer = csv.writer(buffer, lineterminator="\n")
        for name, data in members.items():
            writer.writerow([name, record_digest(data), len(data)])
        writer.writerow([f"{root}/RECORD", "", ""])
        members[f"{root}/RECORD"] = buffer.getvalue().encode()
        archive = io.BytesIO()
        with zipfile.ZipFile(archive, "w") as wheel:
            for name, data in members.items():
                wheel.writestr(name, data)
        return archive.getvalue(), members

    def test_only_build_metadata_and_record_change(self) -> None:
        source, original = self.fixture()
        name = "pari_similarity-0.1.0-cp310-abi3-linux_x86_64.whl"
        with tempfile.TemporaryDirectory() as temporary:
            destination = rebuild_wheel(source, name, Path(temporary))
            self.assertEqual(
                destination.name,
                "pari_similarity-0.1.0-1-cp310-abi3-linux_x86_64.whl",
            )
            with zipfile.ZipFile(destination) as wheel:
                for member, data in original.items():
                    if not member.endswith(("/WHEEL", "/RECORD")):
                        self.assertEqual(wheel.read(member), data)
                rows = csv.reader(
                    io.StringIO(
                        wheel.read("pari_similarity-0.1.0.dist-info/RECORD").decode()
                    )
                )
                for member, digest, size in rows:
                    if not member.endswith("/RECORD"):
                        data = wheel.read(member)
                        self.assertEqual(digest, record_digest(data))
                        self.assertEqual(size, str(len(data)))

    def test_original_record_is_validated(self) -> None:
        source, _original = self.fixture()
        with tempfile.TemporaryDirectory() as temporary:
            corrupted = io.BytesIO()
            with (
                zipfile.ZipFile(io.BytesIO(source)) as wheel,
                zipfile.ZipFile(corrupted, "w") as changed,
            ):
                for member in wheel.infolist():
                    data = wheel.read(member)
                    if member.filename.endswith("/_native.so"):
                        data = b"altered binary"
                    changed.writestr(member, data)
            with self.assertRaisesRegex(ValueError, "invalid original wheel RECORD"):
                rebuild_wheel(
                    corrupted.getvalue(),
                    "pari_similarity-0.1.0-cp310-abi3-linux_x86_64.whl",
                    Path(temporary),
                )
