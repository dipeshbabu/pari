"""Publish verified crate archives without executing Cargo or package code."""

from __future__ import annotations

import argparse
import hashlib
import io
import json
import os
import re
import struct
import tarfile
import time
import urllib.error
import urllib.parse
import urllib.request
from dataclasses import dataclass
from pathlib import Path, PurePosixPath
from typing import Any

import tomllib

CRATES = ("pari-core", "pari-format", "pari-index", "pari-store")
API = "https://crates.io/api/v1/crates"
PUBLISH_URL = f"{API}/new"
MAX_ARCHIVE_BYTES = 20 * 1024 * 1024
MAX_METADATA_BYTES = 4 * 1024 * 1024
VERSION = re.compile(r"\d+\.\d+\.\d+(?:[-+][0-9A-Za-z.-]+)?\Z")


@dataclass(frozen=True)
class PreparedCrate:
    name: str
    version: str
    checksum: str
    metadata: dict[str, Any]
    archive: bytes

    def payload(self) -> bytes:
        encoded = json.dumps(self.metadata, separators=(",", ":")).encode("utf-8")
        return (
            struct.pack("<I", len(encoded))
            + encoded
            + struct.pack("<I", len(self.archive))
            + self.archive
        )


def safe_relative(value: str) -> PurePosixPath:
    path = PurePosixPath(value)
    if path.is_absolute() or ".." in path.parts or "\\" in value or ":" in value:
        raise ValueError("unsafe archive or bundle path")
    return path


def verify_bundle(directory: Path) -> dict[str, str]:
    checksum_path = directory / "SHA256SUMS"
    if checksum_path.is_symlink() or not checksum_path.is_file():
        raise ValueError("release checksum manifest must be a regular file")
    checksums: dict[str, str] = {}
    for line in checksum_path.read_text(encoding="utf-8").splitlines():
        digest, filename = line.split(maxsplit=1)
        if (
            not re.fullmatch(r"[0-9a-f]{64}", digest)
            or len(safe_relative(filename).parts) != 1
            or filename in checksums
            or filename == "SHA256SUMS"
        ):
            raise ValueError("invalid or duplicate release checksum entry")
        checksums[filename] = digest
    actual = {path.name for path in directory.iterdir() if path.name != "SHA256SUMS"}
    if not actual or actual != set(checksums):
        raise ValueError(
            "release checksum manifest must cover every artifact exactly once"
        )
    for filename, expected in checksums.items():
        path = directory / filename
        if path.is_symlink() or not path.is_file():
            raise ValueError("release artifacts must be regular files")
        digest = hashlib.sha256()
        with path.open("rb") as source:
            for block in iter(lambda: source.read(64 * 1024), b""):
                digest.update(block)
        if digest.hexdigest() != expected:
            raise ValueError(f"release checksum mismatch: {filename}")
    return checksums


def archive_metadata(blob: bytes, name: str, version: str) -> dict[str, Any]:
    prefix = f"{name}-{version}"
    with tarfile.open(fileobj=io.BytesIO(blob), mode="r:gz") as archive:
        members: dict[str, tarfile.TarInfo] = {}
        for member in archive:
            parts = safe_relative(member.name).parts
            if (
                not parts
                or parts[0] != prefix
                or str(PurePosixPath(member.name)) != member.name.rstrip("/")
                or member.name in members
                or not (member.isfile() or member.isdir())
                or member.mode & 0o6000
                or len(members) >= 10_000
            ):
                raise ValueError("invalid crate archive member")
            members[member.name] = member

        def read(relative: str) -> str:
            safe_relative(relative)
            member = members.get(f"{prefix}/{relative}")
            if (
                member is None
                or not member.isfile()
                or member.size > MAX_METADATA_BYTES
            ):
                raise ValueError("missing or oversized crate metadata")
            stream = archive.extractfile(member)
            if stream is None:
                raise ValueError("crate metadata is not readable")
            return stream.read().decode("utf-8")

        manifest = tomllib.loads(read("Cargo.toml"))
        package = manifest["package"]
        if package["name"] != name or package["version"] != version:
            raise ValueError("crate manifest identity does not match the release")
        read("LICENSE")
        if name in {"pari-core", "pari-index"}:
            read("NOTICE")
        metadata = {
            "name": name,
            "vers": version,
            "deps": [],
            "features": manifest.get("features", {}),
            "authors": package.get("authors", []),
            "keywords": package.get("keywords", []),
            "categories": package.get("categories", []),
            "badges": manifest.get("badges", {}),
            "readme": None,
            "readme_file": None,
            "rust_version": package.get("rust-version"),
            "license_file": package.get("license-file"),
        }
        for field in (
            "description",
            "documentation",
            "homepage",
            "license",
            "repository",
            "links",
        ):
            metadata[field] = package.get(field)
        readme = package.get("readme")
        if isinstance(readme, str):
            metadata["readme"] = read(readme)
            metadata["readme_file"] = readme
        for target, table in [(None, manifest), *manifest.get("target", {}).items()]:
            for section, kind in (
                ("dependencies", "normal"),
                ("dev-dependencies", "dev"),
                ("build-dependencies", "build"),
            ):
                for alias, specification in table.get(section, {}).items():
                    spec = (
                        {"version": specification}
                        if isinstance(specification, str)
                        else specification
                    )
                    registry = spec.get("registry-index") or spec.get("registry")
                    if registry not in (
                        None,
                        "crates-io",
                        "https://github.com/rust-lang/crates.io-index",
                    ):
                        raise ValueError("unexpected dependency registry")
                    metadata["deps"].append(
                        {
                            "name": spec.get("package", alias),
                            "version_req": spec["version"],
                            "features": spec.get("features", []),
                            "optional": spec.get("optional", False),
                            "default_features": spec.get("default-features", True),
                            "target": target,
                            "kind": kind,
                            "registry": None,
                            "explicit_name_in_toml": alias
                            if "package" in spec
                            else None,
                        }
                    )
    return metadata


def prepare_crates(directory: Path, version: str) -> list[PreparedCrate]:
    if not VERSION.fullmatch(version):
        raise ValueError("invalid release version")
    checksums = verify_bundle(directory)
    expected = {f"{name}-{version}.crate" for name in CRATES}
    if {name for name in checksums if name.endswith(".crate")} != expected:
        raise ValueError("expected exactly the four coordinated public crate archives")
    prepared = []
    for name in CRATES:
        filename = f"{name}-{version}.crate"
        path = directory / filename
        with path.open("rb") as source:
            blob = source.read(MAX_ARCHIVE_BYTES + 1)
        if len(blob) > MAX_ARCHIVE_BYTES:
            raise ValueError("crate archive exceeds the publication size limit")
        if hashlib.sha256(blob).hexdigest() != checksums[filename]:
            raise ValueError("crate archive changed after checksum validation")
        prepared.append(
            PreparedCrate(
                name,
                version,
                checksums[filename],
                archive_metadata(blob, name, version),
                blob,
            )
        )
    return prepared


class NoRedirects(urllib.request.HTTPRedirectHandler):
    def redirect_request(self, req, fp, code, msg, headers, newurl):
        return None


def registry_request(
    url: str, *, payload: bytes | None = None, token: str | None = None
) -> bytes | None:
    parsed = urllib.parse.urlsplit(url)
    if parsed.scheme != "https" or parsed.netloc not in {
        "crates.io",
        "index.crates.io",
    }:
        raise ValueError("unexpected registry endpoint")
    if token is not None and (url != PUBLISH_URL or payload is None):
        raise ValueError(
            "publication credentials may only be sent to the publish endpoint"
        )
    headers = {
        "User-Agent": "pari-release-publisher/1.0",
        "Accept": "application/json",
        "Cache-Control": "no-cache",
    }
    if token is not None:
        headers["Authorization"] = token
        headers["Content-Type"] = "application/octet-stream"
    request = urllib.request.Request(  # noqa: S310 - HTTPS registry hosts are restricted above
        url,
        data=payload,
        headers=headers,
        method="PUT" if payload is not None else "GET",
    )
    try:
        opener = urllib.request.build_opener(NoRedirects())
        with opener.open(request, timeout=60) as response:
            return response.read()
    except urllib.error.HTTPError as error:
        if error.code == 404 and payload is None:
            return None
        details = error.read().decode("utf-8", errors="replace")
        if token:
            details = details.replace(token, "[redacted]")
        raise RuntimeError(f"registry HTTP {error.code}: {details}") from None


def published_version(crate: PreparedCrate) -> dict[str, Any] | None:
    data = registry_request(
        f"{API}/{crate.name}/{crate.version}?verify={time.time_ns()}"
    )
    return json.loads(data)["version"] if data is not None else None


def require_checksum(crate: PreparedCrate, current: dict[str, Any]) -> None:
    if current["checksum"] != crate.checksum or current["yanked"]:
        raise RuntimeError(
            f"published {crate.name} {crate.version} does not match the verified archive"
        )


def wait_visible(crate: PreparedCrate) -> None:
    for _attempt in range(24):
        current = published_version(crate)
        if current is not None:
            require_checksum(crate, current)
            index = registry_request(
                f"https://index.crates.io/{crate.name[:2]}/{crate.name[2:4]}/{crate.name}?verify={time.time_ns()}"
            )
            if index is not None:
                entry = next(
                    (
                        item
                        for line in index.splitlines()
                        if (item := json.loads(line))["vers"] == crate.version
                    ),
                    None,
                )
                if entry is not None:
                    if entry["cksum"] != crate.checksum or entry["yanked"]:
                        raise RuntimeError(
                            "public index does not match the verified archive"
                        )
                    return
        time.sleep(5)
    raise RuntimeError(
        f"timed out waiting for {crate.name} {crate.version} in the index"
    )


def publish(prepared: list[PreparedCrate], token: str) -> None:
    if not token:
        raise ValueError("publication credential is missing")
    for crate in prepared:
        current = published_version(crate)
        if current is None:
            data = registry_request(PUBLISH_URL, payload=crate.payload(), token=token)
            response = json.loads(data)
            if response.get("errors"):
                raise RuntimeError("registry rejected the verified archive")
        else:
            require_checksum(crate, current)
        wait_visible(crate)
        print(f"verified publication: {crate.name} {crate.version}", flush=True)


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--directory", type=Path, required=True)
    parser.add_argument("--version", required=True)
    parser.add_argument("--validate-only", action="store_true")
    args = parser.parse_args()
    with (Path(__file__).resolve().parents[1] / "Cargo.toml").open("rb") as handle:
        version = tomllib.load(handle)["workspace"]["package"]["version"]
    if version != args.version:
        raise ValueError("release version must match the checked-out workspace")
    prepared = prepare_crates(args.directory, args.version)
    if args.validate_only:
        print(f"verified {len(prepared)} public crate archives for {args.version}")
        return
    publish(prepared, os.environ.get("CARGO_REGISTRY_TOKEN", ""))


if __name__ == "__main__":
    main()
