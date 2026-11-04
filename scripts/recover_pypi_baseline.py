"""Recover archived baseline wheels using a new, standards-defined build tag."""

from __future__ import annotations

import argparse
import base64
import csv
import hashlib
import io
import json
import urllib.request
import zipfile
from pathlib import Path, PurePosixPath

RELEASE = "https://api.github.com/repos/dipeshbabu/pari/releases/tags/v0.1.0"
DOWNLOAD_PREFIX = "https://github.com/dipeshbabu/pari/releases/download/v0.1.0/"


def fetch(url: str) -> bytes:
    if url != RELEASE and not url.startswith(DOWNLOAD_PREFIX):
        raise ValueError("only the archived baseline release may be downloaded")
    request = urllib.request.Request(  # noqa: S310 - fixed HTTPS release URLs above
        url, headers={"User-Agent": "pari-baseline-recovery"}
    )
    with urllib.request.urlopen(request, timeout=60) as response:  # noqa: S310
        return response.read()


def record_digest(data: bytes) -> str:
    digest = base64.urlsafe_b64encode(hashlib.sha256(data).digest()).rstrip(b"=")
    return f"sha256={digest.decode('ascii')}"


def rebuild_wheel(source: bytes, filename: str, output: Path) -> Path:
    prefix = "pari_similarity-0.1.0-"
    if not filename.startswith(prefix) or not filename.endswith(".whl"):
        raise ValueError("expected an original Pari baseline wheel")
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

    metadata = "pari_similarity-0.1.0.dist-info/METADATA"
    wheel = "pari_similarity-0.1.0.dist-info/WHEEL"
    record = "pari_similarity-0.1.0.dist-info/RECORD"
    if b"Version: 0.1.0\n" not in contents[metadata]:
        raise ValueError("baseline package version does not match its filename")
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


def recover(output: Path) -> None:
    release = json.loads(fetch(RELEASE))
    assets = {
        asset["name"]: asset["browser_download_url"] for asset in release["assets"]
    }
    checksums = {
        name: digest
        for digest, name in (
            line.split(None, 1)
            for line in fetch(assets["SHA256SUMS"]).decode("utf-8").splitlines()
        )
    }
    wheels = sorted(name for name in assets if name.endswith(".whl"))
    if len(wheels) != 3:
        raise ValueError("expected all three archived baseline platforms")
    for name in wheels:
        data = fetch(assets[name])
        if hashlib.sha256(data).hexdigest() != checksums[name]:
            raise ValueError(f"archived wheel checksum mismatch: {name}")
        print(rebuild_wheel(data, name, output))


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--output", type=Path, required=True)
    recover(parser.parse_args().output)
