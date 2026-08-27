#!/usr/bin/env python3
"""Release validation and artifact helpers for Pari.

Uses only the Python standard library so release validation does not introduce a
second packaging dependency chain.
"""

from __future__ import annotations

import argparse
import hashlib
import json
import os
from pathlib import Path
import re
import subprocess
import sys
import tarfile
import tempfile
import tomllib
from urllib.parse import quote
import uuid
import zipfile

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
SEMVER = re.compile(r"^(0|[1-9]\d*)\.(0|[1-9]\d*)\.(0|[1-9]\d*)(?:[-+][0-9A-Za-z.-]+)?$")


def load_toml(path: Path) -> dict:
    with path.open("rb") as handle:
        return tomllib.load(handle)


def workspace_version() -> str:
    data = load_toml(ROOT / "Cargo.toml")
    version = data.get("workspace", {}).get("package", {}).get("version")
    if not isinstance(version, str) or not SEMVER.fullmatch(version):
        raise SystemExit("root Cargo.toml must contain a valid [workspace.package] version")
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
        errors.append("pyproject version must remain dynamic and derive from pari-py/Cargo.toml")

    for name, manifest in PUBLIC_CRATES.items():
        data = load_toml(manifest)
        package = data.get("package", {})
        if package.get("name") != name:
            errors.append(f"{manifest}: package name must be {name}")
        if package.get("version", {}).get("workspace") is not True:
            errors.append(f"{manifest}: public crate version must use version.workspace = true")
        if package.get("publish") is False:
            errors.append(f"{manifest}: public crate must not set publish = false")

    for name, manifest in INTERNAL_CRATES.items():
        data = load_toml(manifest)
        package = data.get("package", {})
        if package.get("name") != name:
            errors.append(f"{manifest}: package name must be {name}")
        if package.get("publish") is not False:
            errors.append(f"{manifest}: internal crate must explicitly set publish = false")

    for crate, expected_dependencies in EXACT_INTERNAL_DEPENDENCIES.items():
        manifest = PUBLIC_CRATES[crate]
        data = load_toml(manifest)
        dependencies = data.get("dependencies", {})
        for dependency in expected_dependencies:
            spec = dependencies.get(dependency)
            if not isinstance(spec, dict):
                errors.append(f"{manifest}: {dependency} must use a path + exact registry version")
                continue
            if spec.get("version") != f"={version}":
                errors.append(
                    f"{manifest}: {dependency} registry version must be exactly ={version}"
                )
            expected_path = f"../{dependency}"
            if spec.get("path") != expected_path:
                errors.append(f"{manifest}: {dependency} path must be {expected_path!r}")

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
    for package in sorted(metadata["packages"], key=lambda item: (item["name"], item["version"], item["id"])):
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
    output.write_text(json.dumps(document, indent=2, sort_keys=True) + "\n", encoding="utf-8")
    print(output)


def write_checksums(directory: Path, output: Path) -> None:
    directory = directory.resolve()
    output = output.resolve()
    files = [path for path in directory.rglob("*") if path.is_file() and path.resolve() != output]
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


def package_cli(binary: Path, output_dir: Path, platform_name: str) -> None:
    version = workspace_version()
    if not binary.is_file():
        raise SystemExit(f"CLI binary does not exist: {binary}")
    output_dir.mkdir(parents=True, exist_ok=True)
    base = f"pari-{version}-{platform_name}"
    executable_name = "pari.exe" if binary.suffix.lower() == ".exe" else "pari"
    files = {
        executable_name: binary,
        "README.md": ROOT / "README.md",
        "LICENSE": ROOT / "LICENSE",
        "NOTICE": ROOT / "NOTICE",
    }

    if platform_name.startswith("windows"):
        destination = output_dir / f"{base}.zip"
        with zipfile.ZipFile(destination, "w", compression=zipfile.ZIP_DEFLATED) as archive:
            for archive_name, source in files.items():
                archive.write(source, f"{base}/{archive_name}")
    else:
        destination = output_dir / f"{base}.tar.gz"
        with tarfile.open(destination, "w:gz") as archive:
            for archive_name, source in files.items():
                archive.add(source, arcname=f"{base}/{archive_name}", recursive=False)
    print(destination)


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
    return root


def main() -> None:
    args = parser().parse_args()
    args.func(args)


if __name__ == "__main__":
    main()
