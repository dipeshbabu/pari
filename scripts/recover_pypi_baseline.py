"""Recover archived release wheels using a new, standards-defined build tag.

PyPI never accepts a filename that was uploaded before, even after deletion.
When a release's wheel filenames are blocked that way, this script downloads
the checksum-verified wheels attached to that version's GitHub Release and
rewrites them with wheel build tag ``1``. Only the ``WHEEL`` metadata and the
``RECORD`` entry for it change; every other member is copied byte for byte.
"""

from __future__ import annotations

import argparse
import base64
import csv
import hashlib
import io
import json
import re
import urllib.request
import zipfile
from pathlib import Path, PurePosixPath

REPOSITORY = "https://github.com/dipeshbabu/pari"
API = "https://api.github.com/repos/dipeshbabu/pari/releases/tags"
DEFAULT_VERSION = "0.1.0"
MINIMUM_PLATFORMS = 3
VERSION_PATTERN = re.compile(r"\d+\.\d+\.\d+")


def validate_version(version: str) -> str:
    if not VERSION_PATTERN.fullmatch(version):
        raise ValueError(f"expected a MAJOR.MINOR.PATCH version, got {version!r}")
    return version


def release_url(version: str) -> str:
    return f"{API}/v{validate_version(version)}"


def download_prefix(version: str) -> str:
    return f"{REPOSITORY}/releases/download/v{validate_version(version)}/"


def fetch(url: str, version: str) -> bytes:
    if url != release_url(version) and not url.startswith(download_prefix(version)):
        raise ValueError("only the archived release for this version may be downloaded")
    request = urllib.request.Request(  # noqa: S310 - fixed HTTPS release URLs above
        url, headers={"User-Agent": "pari-baseline-recovery"}
    )
    with urllib.request.urlopen(request, timeout=60) as response:  # noqa: S310
        return response.read()


def record_digest(data: bytes) -> str:
    digest = base64.urlsafe_b64encode(hashlib.sha256(data).digest()).rstrip(b"=")
    return f"sha256={digest.decode('ascii')}"


def rebuild_wheel(
    source: bytes, filename: str, output: Path, version: str = DEFAULT_VERSION
) -> Path:
    version = validate_version(version)
    prefix = f"pari_similarity-{version}-"
    if not filename.startswith(prefix) or not filename.endswith(".whl"):
        raise ValueError("expected an original Pari release wheel")
    with zipfile.ZipFile(io.BytesIO(source)) as archive:
        entries = archive.infolist()
        names = [entry.filename for entry in entries]
        if len(names) != len(set(names)):
            raise ValueError("duplicate wheel archive member")
        for name in names:
            path = PurePosixPath(name)
            if path.is_absolute() or ".." in path.parts:
                raise ValueError("unsafe wheel archive member")
        contents = {entry.filename: archive.read(entry) for entry in entries}

    dist_info = f"pari_similarity-{version}.dist-info"
    metadata = f"{dist_info}/METADATA"
    wheel = f"{dist_info}/WHEEL"
    record = f"{dist_info}/RECORD"
    if f"Version: {version}\n".encode() not in contents[metadata]:
        raise ValueError("release package version does not match its filename")
    records = list(csv.reader(io.StringIO(contents[record].decode("utf-8"))))
    if {row[0] for row in records} != set(names):
        raise ValueError("original wheel RECORD does not cover every member")
    for name, digest, size in records:
        if name != record and (
            digest != record_digest(contents[name]) or size != str(len(contents[name]))
        ):
            raise ValueError(f"invalid original wheel RECORD entry: {name}")

    if b"Build:" in contents[wheel]:
        raise ValueError("original wheel already contains a build number")
    contents[wheel] = contents[wheel].rstrip(b"\n") + b"\nBuild: 1\n"
    for row in records:
        if row[0] == wheel:
            row[1:] = [record_digest(contents[wheel]), str(len(contents[wheel]))]
    buffer = io.StringIO(newline="")
    csv.writer(buffer, lineterminator="\n").writerows(records)
    contents[record] = buffer.getvalue().encode("utf-8")

    output.mkdir(parents=True, exist_ok=True)
    destination = output / filename.replace(prefix, f"{prefix}1-", 1)
    with zipfile.ZipFile(destination, "w") as archive:
        for entry in entries:
            archive.writestr(entry, contents[entry.filename])
    return destination


def recover(output: Path, version: str = DEFAULT_VERSION) -> None:
    release = json.loads(fetch(release_url(version), version))
    assets = {
        asset["name"]: asset["browser_download_url"] for asset in release["assets"]
    }
    checksums = {
        name: digest
        for digest, name in (
            line.split(None, 1)
            for line in fetch(assets["SHA256SUMS"], version)
            .decode("utf-8")
            .splitlines()
        )
    }
    wheels = sorted(name for name in assets if name.endswith(".whl"))
    listed = sorted(name for name in checksums if name.endswith(".whl"))
    if wheels != listed or len(wheels) < MINIMUM_PLATFORMS:
        raise ValueError("expected every checksummed release wheel platform")
    for name in wheels:
        data = fetch(assets[name], version)
        if hashlib.sha256(data).hexdigest() != checksums[name]:
            raise ValueError(f"archived wheel checksum mismatch: {name}")
        print(rebuild_wheel(data, name, output, version))


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--output", type=Path, required=True)
    parser.add_argument("--version", default=DEFAULT_VERSION)
    arguments = parser.parse_args()
    recover(arguments.output, arguments.version)
