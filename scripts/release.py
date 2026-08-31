#!/usr/bin/env python3
"""Release validation and artifact helpers for Pari.

Uses only the Python standard library so release validation does not introduce a
second packaging dependency chain.
"""

from __future__ import annotations

import argparse
import gzip
import hashlib
import json
import os
import re
import shutil
import subprocess
import sys
import tarfile
import tempfile
import uuid
import zipfile
from pathlib import Path, PurePosixPath
from urllib.parse import quote

import tomllib

ROOT = Path(__file__).resolve().parents[1]
PUBLIC_CRATES = {
    "pari-core": ROOT / "crates/pari-core/Cargo.toml",
    "pari-format": ROOT / "crates/pari-format/Cargo.toml",
    "pari-index": ROOT / "crates/pari-index/Cargo.toml",
    "pari-store": ROOT / "crates/pari-store/Cargo.toml",
}
INTERNAL_CRATES = {
    "pari-backend": ROOT / "crates/pari-backend/Cargo.toml",
    "pari-bench": ROOT / "crates/pari-bench/Cargo.toml",
    "pari-cli": ROOT / "crates/pari-cli/Cargo.toml",
    "pari-py": ROOT / "crates/pari-py/Cargo.toml",
    "pari-store-build": ROOT / "crates/pari-store-build/Cargo.toml",
    "pari-store-lazy": ROOT / "crates/pari-store-lazy/Cargo.toml",
}
EXACT_INTERNAL_DEPENDENCIES = {
    "pari-index": {"pari-core"},
    "pari-store": {"pari-core", "pari-format", "pari-index"},
}
NOTICE_CRATES = {"pari-core", "pari-index"}
SEMVER = re.compile(
    r"^(0|[1-9]\d*)\.(0|[1-9]\d*)\.(0|[1-9]\d*)(?:[-+][0-9A-Za-z.-]+)?$"
)
ZIP_EPOCH = (1980, 1, 1, 0, 0, 0)
REQUIRED_SDIST_ROOTS = {
    "Cargo.lock",
    "Cargo.toml",
    "LICENSE",
    "NOTICE",
    "README.md",
    "pyproject.toml",
    "crates/pari-core/testdata/datasketch_v2_affine.json",
}
FORBIDDEN_SDIST_PARTS = {
    ".git",
    ".mypy_cache",
    ".pytest_cache",
    ".ruff_cache",
    ".tox",
    ".venv",
    "__pycache__",
    "target",
    "venv",
}
FORBIDDEN_SDIST_NAMES = {".env", "credentials", "credentials.json"}
FORBIDDEN_SDIST_SUFFIXES = {".key", ".pem", ".p12", ".pfx"}


def load_toml(path: Path) -> dict:
    with path.open("rb") as handle:
        return tomllib.load(handle)


def workspace_version() -> str:
    data = load_toml(ROOT / "Cargo.toml")
    version = data.get("workspace", {}).get("package", {}).get("version")
    if not isinstance(version, str) or not SEMVER.fullmatch(version):
        raise SystemExit(
            "root Cargo.toml must contain a valid [workspace.package] version"
        )
    return version


def validate(tag: str | None = None) -> None:
    version = workspace_version()
    errors: list[str] = []

    if tag and tag != f"v{version}":
        errors.append(f"tag {tag!r} does not match workspace version v{version}")

    pyproject = load_toml(ROOT / "pyproject.toml")
    project = pyproject.get("project", {})
    if project.get("name") != "pari-similarity":
        errors.append("Python distribution name must remain 'pari-similarity'")
    if project.get("dynamic") != ["version"]:
        errors.append(
            "pyproject version must remain dynamic and derive from pari-py/Cargo.toml"
        )
    try:
        expected_sdist_paths(ROOT, pyproject)
    except ValueError as error:
        errors.append(str(error))

    for name, manifest in PUBLIC_CRATES.items():
        data = load_toml(manifest)
        package = data.get("package", {})
        if package.get("name") != name:
            errors.append(f"{manifest}: package name must be {name}")
        if package.get("version", {}).get("workspace") is not True:
            errors.append(
                f"{manifest}: public crate version must use version.workspace = true"
            )
        if package.get("publish") is False:
            errors.append(f"{manifest}: public crate must not set publish = false")
        crate_dir = manifest.parent
        if not (crate_dir / "LICENSE").is_file():
            errors.append(f"{manifest}: public crate must package LICENSE")
        if name in NOTICE_CRATES:
            notice = crate_dir / "NOTICE"
            if not notice.is_file():
                errors.append(
                    f"{manifest}: datasketch-derived crate must package NOTICE"
                )
            elif "Copyright (c) 2015 ekzhu" not in notice.read_text(encoding="utf-8"):
                errors.append(f"{notice}: upstream datasketch copyright is missing")

    for name, manifest in INTERNAL_CRATES.items():
        data = load_toml(manifest)
        package = data.get("package", {})
        if package.get("name") != name:
            errors.append(f"{manifest}: package name must be {name}")
        if package.get("version", {}).get("workspace") is not True:
            errors.append(
                f"{manifest}: internal crate version must use version.workspace = true"
            )
        if package.get("publish") is not False:
            errors.append(
                f"{manifest}: internal crate must explicitly set publish = false"
            )

    for crate, expected_dependencies in EXACT_INTERNAL_DEPENDENCIES.items():
        manifest = PUBLIC_CRATES[crate]
        data = load_toml(manifest)
        dependencies = data.get("dependencies", {})
        for dependency in expected_dependencies:
            spec = dependencies.get(dependency)
            if not isinstance(spec, dict):
                errors.append(
                    f"{manifest}: {dependency} must use a path + exact registry version"
                )
                continue
            if spec.get("version") != f"={version}":
                errors.append(
                    f"{manifest}: {dependency} registry version must be exactly ={version}"
                )
            expected_path = f"../{dependency}"
            if spec.get("path") != expected_path:
                errors.append(
                    f"{manifest}: {dependency} path must be {expected_path!r}"
                )

    changelog = (ROOT / "CHANGELOG.md").read_text(encoding="utf-8")
    if f"## [{version}]" not in changelog:
        errors.append(f"CHANGELOG.md must contain a [{version}] release section")
    release_notes = ROOT / f"docs/releases/{version}.md"
    if not release_notes.is_file():
        errors.append(f"missing release notes: {release_notes.relative_to(ROOT)}")
    if not (ROOT / "docs/compatibility.md").is_file():
        errors.append("missing docs/compatibility.md compatibility contract")

    if errors:
        for error in errors:
            print(f"release validation error: {error}", file=sys.stderr)
        raise SystemExit(1)

    print(f"release metadata valid for Pari {version}")


def expected_sdist_paths(root: Path, pyproject: dict | None = None) -> set[str]:
    pyproject = pyproject or load_toml(root / "pyproject.toml")
    includes = pyproject.get("tool", {}).get("maturin", {}).get("include", [])
    declared: list[str] = []
    for entry in includes:
        if isinstance(entry, str):
            declared.append(entry)
        elif isinstance(entry, dict) and entry.get("format", "sdist") == "sdist":
            path = entry.get("path")
            if not isinstance(path, str):
                raise TypeError(
                    "tool.maturin.include sdist entries must have a string path"
                )
            declared.append(path)
    duplicates = sorted(path for path in set(declared) if declared.count(path) > 1)
    if duplicates:
        raise ValueError(f"duplicate tool.maturin.include paths: {duplicates}")

    expected = set(REQUIRED_SDIST_ROOTS)
    for value in declared:
        matches = sorted(root.glob(value))
        if not matches:
            raise ValueError(f"declared sdist path does not exist: {value}")
        for match in matches:
            if match.is_dir():
                expected.update(
                    path.relative_to(root).as_posix()
                    for path in match.rglob("*")
                    if path.is_file()
                )
            elif match.is_file():
                expected.add(match.relative_to(root).as_posix())

    package = root / "python" / "pari"
    expected.update(
        path.relative_to(root).as_posix()
        for path in package.rglob("*")
        if path.is_file() and path.suffix in {".py", ".pyi", ".typed"}
    )
    return expected


def validate_sdist_archive(archive: Path, expected: set[str] | None = None) -> None:
    expected = expected or expected_sdist_paths(ROOT)
    with tarfile.open(archive, "r:gz") as bundle:
        members = bundle.getmembers()
    names = [member.name for member in members]
    duplicates = sorted(name for name in set(names) if names.count(name) > 1)
    errors = [f"duplicate archive member: {name}" for name in duplicates]
    roots = {
        PurePosixPath(name).parts[0] for name in names if PurePosixPath(name).parts
    }
    if len(roots) != 1:
        errors.append(
            f"sdist must contain exactly one archive root, got {sorted(roots)}"
        )

    files: set[str] = set()
    for member in members:
        path = PurePosixPath(member.name)
        if path.is_absolute() or ".." in path.parts:
            errors.append(f"unsafe archive path: {member.name}")
            continue
        relative = PurePosixPath(*path.parts[1:])
        if member.issym() or member.islnk():
            errors.append(f"sdist must not contain links: {relative.as_posix()}")
        if not member.isfile():
            continue
        value = relative.as_posix()
        files.add(value)
        lowered = {part.casefold() for part in relative.parts}
        if lowered & FORBIDDEN_SDIST_PARTS:
            errors.append(f"forbidden sdist path: {value}")
        if relative.name.casefold() in FORBIDDEN_SDIST_NAMES:
            errors.append(f"forbidden sdist file: {value}")
        if relative.suffix.casefold() in FORBIDDEN_SDIST_SUFFIXES:
            errors.append(f"forbidden secret-like sdist file: {value}")

    for missing in sorted(expected - files):
        errors.append(f"missing expected sdist file: {missing}")
    if errors:
        raise SystemExit(
            "\n".join(f"sdist validation error: {error}" for error in errors)
        )
    print(f"source distribution manifest valid: {archive}")


def cargo_metadata() -> dict:
    result = subprocess.run(
        ["cargo", "metadata", "--locked", "--format-version", "1"],
        cwd=ROOT,
        check=True,
        capture_output=True,
        text=True,
    )
    return json.loads(result.stdout)


def build_sbom(output: Path) -> None:
    metadata = cargo_metadata()
    version = workspace_version()
    components = []
    for package in sorted(
        metadata["packages"],
        key=lambda item: (item["name"], item["version"], item["id"]),
    ):
        component = {
            "type": "library",
            "bom-ref": package["id"],
            "name": package["name"],
            "version": package["version"],
            "purl": f"pkg:cargo/{quote(package['name'], safe='')}@{quote(package['version'], safe='')}",
        }
        source = package.get("source")
        if source:
            component["properties"] = [{"name": "cargo:source", "value": source}]
        components.append(component)

    document = {
        "bomFormat": "CycloneDX",
        "specVersion": "1.5",
        "serialNumber": f"urn:uuid:{uuid.uuid5(uuid.NAMESPACE_URL, f'https://github.com/dipeshbabu/pari@{version}')}",
        "version": 1,
        "metadata": {
            "component": {
                "type": "application",
                "name": "pari",
                "version": version,
                "purl": f"pkg:github/dipeshbabu/pari@{quote(version, safe='')}",
            }
        },
        "components": components,
    }
    output.parent.mkdir(parents=True, exist_ok=True)
    output.write_text(
        json.dumps(document, indent=2, sort_keys=True) + "\n", encoding="utf-8"
    )
    print(output)


def write_checksums(directory: Path, output: Path) -> None:
    directory = directory.resolve()
    output = output.resolve()
    files = [
        path
        for path in directory.rglob("*")
        if path.is_file() and path.resolve() != output
    ]
    lines = []
    for path in sorted(files, key=lambda item: item.relative_to(directory).as_posix()):
        digest = hashlib.sha256()
        with path.open("rb") as handle:
            for block in iter(lambda: handle.read(1024 * 1024), b""):
                digest.update(block)
        lines.append(f"{digest.hexdigest()}  {path.relative_to(directory).as_posix()}")
    output.parent.mkdir(parents=True, exist_ok=True)
    output.write_text("\n".join(lines) + ("\n" if lines else ""), encoding="utf-8")
    print(output)


def source_date_epoch() -> int:
    raw = os.environ.get("SOURCE_DATE_EPOCH", "0")
    try:
        epoch = int(raw)
    except ValueError as error:
        raise SystemExit(
            f"SOURCE_DATE_EPOCH must be an integer, got {raw!r}"
        ) from error
    if epoch < 0:
        raise SystemExit("SOURCE_DATE_EPOCH must not be negative")
    return epoch


def normalize_tar_info(info: tarfile.TarInfo, epoch: int) -> tarfile.TarInfo:
    info.mtime = epoch
    info.uid = 0
    info.gid = 0
    info.uname = ""
    info.gname = ""
    return info


def write_deterministic_tar(archive: Path, staging: Path, archive_root: str) -> None:
    epoch = source_date_epoch()
    with (
        archive.open("wb") as raw,
        gzip.GzipFile(filename="", mode="wb", fileobj=raw, mtime=epoch) as compressed,
        tarfile.open(
            fileobj=compressed, mode="w", format=tarfile.USTAR_FORMAT
        ) as bundle,
    ):
        bundle.addfile(
            normalize_tar_info(bundle.gettarinfo(staging, archive_root), epoch)
        )
        for path in sorted(staging.iterdir(), key=lambda item: item.name):
            info = normalize_tar_info(
                bundle.gettarinfo(path, f"{archive_root}/{path.name}"), epoch
            )
            with path.open("rb") as handle:
                bundle.addfile(info, handle)


def write_deterministic_zip(archive: Path, staging: Path, archive_root: str) -> None:
    with zipfile.ZipFile(archive, "w", compression=zipfile.ZIP_DEFLATED) as bundle:
        for path in sorted(staging.iterdir(), key=lambda item: item.name):
            info = zipfile.ZipInfo(f"{archive_root}/{path.name}", date_time=ZIP_EPOCH)
            info.compress_type = zipfile.ZIP_DEFLATED
            info.external_attr = (path.stat().st_mode & 0xFFFF) << 16
            bundle.writestr(info, path.read_bytes())


def package_cli(binary: Path, output_dir: Path, platform_name: str) -> None:
    version = workspace_version()
    if not binary.is_file():
        raise SystemExit(f"CLI binary does not exist: {binary}")

    subprocess.run([str(binary), "--help"], check=True, stdout=subprocess.DEVNULL)
    reported = subprocess.check_output(
        [str(binary), "--version"], text=True, stderr=subprocess.STDOUT
    ).strip()
    if version not in reported:
        raise SystemExit(
            f"CLI reports {reported!r}, expected release version {version!r}"
        )

    output_dir.mkdir(parents=True, exist_ok=True)
    base = f"pari-{version}-{platform_name}"
    windows = platform_name.startswith("windows")
    executable_name = "pari.exe" if windows else "pari"

    with tempfile.TemporaryDirectory(prefix="pari-release-") as temporary:
        staging = Path(temporary) / base
        staging.mkdir()
        destination = staging / executable_name
        shutil.copyfile(binary, destination)
        if not windows:
            destination.chmod(binary.stat().st_mode)
        shutil.copyfile(ROOT / "README.md", staging / "README.md")
        shutil.copyfile(ROOT / "LICENSE", staging / "LICENSE")
        shutil.copyfile(ROOT / "NOTICE", staging / "NOTICE")

        if windows:
            archive = output_dir / f"{base}.zip"
            write_deterministic_zip(archive, staging, base)
        else:
            archive = output_dir / f"{base}.tar.gz"
            write_deterministic_tar(archive, staging, base)
    print(archive)


def command_version(_: argparse.Namespace) -> None:
    print(workspace_version())


def command_validate(args: argparse.Namespace) -> None:
    validate(args.tag)


def command_sbom(args: argparse.Namespace) -> None:
    build_sbom(args.output)


def command_checksums(args: argparse.Namespace) -> None:
    write_checksums(args.directory, args.output)


def command_package_cli(args: argparse.Namespace) -> None:
    package_cli(args.binary, args.output_dir, args.platform)


def command_validate_sdist(args: argparse.Namespace) -> None:
    validate_sdist_archive(args.archive)


def parser() -> argparse.ArgumentParser:
    root = argparse.ArgumentParser(description=__doc__)
    commands = root.add_subparsers(dest="command", required=True)

    version = commands.add_parser("version")
    version.set_defaults(func=command_version)

    validate_parser = commands.add_parser("validate")
    validate_parser.add_argument("--tag")
    validate_parser.set_defaults(func=command_validate)

    sbom = commands.add_parser("sbom")
    sbom.add_argument("--output", type=Path, required=True)
    sbom.set_defaults(func=command_sbom)

    checksums = commands.add_parser("checksums")
    checksums.add_argument("--directory", type=Path, required=True)
    checksums.add_argument("--output", type=Path, required=True)
    checksums.set_defaults(func=command_checksums)

    package = commands.add_parser("package-cli")
    package.add_argument("--binary", type=Path, required=True)
    package.add_argument("--output-dir", type=Path, required=True)
    package.add_argument("--platform", required=True)
    package.set_defaults(func=command_package_cli)

    sdist = commands.add_parser("validate-sdist")
    sdist.add_argument("--archive", type=Path, required=True)
    sdist.set_defaults(func=command_validate_sdist)
    return root


def main() -> None:
    args = parser().parse_args()
    args.func(args)


if __name__ == "__main__":
    main()
