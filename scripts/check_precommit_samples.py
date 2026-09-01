from __future__ import annotations

import subprocess
import sys
import tempfile
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
CONFIG = ROOT / ".pre-commit-config.yaml"
SAMPLES = {
    "trailing-whitespace": ("trailing.txt", "value   \n"),
    "end-of-file-fixer": ("missing-eof.txt", "value"),
    "check-yaml": ("broken.yaml", "key: [\n"),
    "check-toml": ("broken.toml", "key = [\n"),
    "check-json": ("broken.json", '{"key": }\n'),
    "check-ast": ("broken.py", "def broken(:\n"),
    "check-merge-conflict": (
        "conflict.txt",
        "<<<<<<< HEAD\nleft\n=======\nright\n>>>>>>> branch\n",
    ),
    "detect-private-key": (
        "private.pem",
        "-----BEGIN "
        "RSA PRIVATE KEY-----\nnot-a-real-key\n-----END RSA PRIVATE KEY-----\n",
    ),
}


def run(command: list[str], *, cwd: Path) -> subprocess.CompletedProcess[str]:
    return subprocess.run(
        command,
        cwd=cwd,
        check=False,
        capture_output=True,
        text=True,
        encoding="utf-8",
        errors="replace",
        timeout=120,
    )


def hook_result(
    repository: Path, hook_id: str, name: str
) -> subprocess.CompletedProcess[str]:
    staged = run(["git", "add", "--", name], cwd=repository)
    if staged.returncode != 0:
        raise SystemExit(staged.stderr or staged.stdout)
    return run(
        [
            sys.executable,
            "-m",
            "pre_commit",
            "run",
            hook_id,
            "--config",
            str(CONFIG),
            "--files",
            name,
            "--color=never",
        ],
        cwd=repository,
    )


def main() -> None:
    try:
        import pre_commit  # noqa: F401
    except ImportError as error:
        raise SystemExit(
            "pre-commit is required; install the pinned version from CONTRIBUTING.md"
        ) from error

    with tempfile.TemporaryDirectory(
        prefix=".pari-precommit-samples-", dir=ROOT
    ) as temporary:
        repository = Path(temporary)
        initialized = run(["git", "init", "--quiet"], cwd=repository)
        if initialized.returncode != 0:
            raise SystemExit(initialized.stderr or initialized.stdout)

        failures: list[str] = []
        for hook_id, (name, content) in SAMPLES.items():
            sample = repository / name
            sample.write_text(content, encoding="utf-8", newline="")
            result = hook_result(repository, hook_id, name)
            if result.returncode == 0:
                failures.append(
                    f"{hook_id} accepted malformed sample {name}:\n"
                    f"{result.stdout}{result.stderr}"
                )

        (repository / "pyproject.toml").write_text(
            (ROOT / "pyproject.toml").read_text(encoding="utf-8"),
            encoding="utf-8",
        )
        ruff_lint = repository / "ruff-lint.py"
        ruff_lint.write_text("import os\n", encoding="utf-8")
        if hook_result(repository, "ruff-check", ruff_lint.name).returncode == 0:
            failures.append("ruff-check accepted an unused import")

        ruff_format = repository / "ruff-format.py"
        ruff_format.write_text('value={"a":1}\n', encoding="utf-8")
        if hook_result(repository, "ruff-format", ruff_format.name).returncode == 0:
            failures.append("ruff-format accepted unformatted Python")

        workspace = repository / "Cargo.toml"
        workspace.write_text(
            '[workspace]\nmembers = ["crates/sample"]\nresolver = "2"\n',
            encoding="utf-8",
        )
        workspace_crate = repository / "crates" / "sample"
        (workspace_crate / "src").mkdir(parents=True)
        (workspace_crate / "Cargo.toml").write_text(
            '[package]\nname = "sample"\nversion = "0.0.0"\nedition = "2021"\n',
            encoding="utf-8",
        )
        workspace_source = workspace_crate / "src" / "lib.rs"
        workspace_source.write_text("pub fn value()->u8{1}\n", encoding="utf-8")
        workspace_result = hook_result(
            repository, "cargo-fmt-workspace", "crates/sample/src/lib.rs"
        )
        if workspace_result.returncode == 0 or "Diff in" not in (
            workspace_result.stdout + workspace_result.stderr
        ):
            failures.append(
                "cargo-fmt-workspace did not report a rustfmt diff:\n"
                f"{workspace_result.stdout}{workspace_result.stderr}"
            )

        criterion = repository / "benchmarks" / "criterion"
        (criterion / "src").mkdir(parents=True)
        (criterion / "Cargo.toml").write_text(
            '[package]\nname = "criterion-sample"\nversion = "0.0.0"\n'
            'edition = "2021"\n\n[workspace]\n',
            encoding="utf-8",
        )
        criterion_source = criterion / "src" / "lib.rs"
        criterion_source.write_text("pub fn value()->u8{1}\n", encoding="utf-8")
        criterion_result = hook_result(
            repository,
            "cargo-fmt-criterion",
            "benchmarks/criterion/src/lib.rs",
        )
        if criterion_result.returncode == 0 or "Diff in" not in (
            criterion_result.stdout + criterion_result.stderr
        ):
            failures.append(
                "cargo-fmt-criterion did not report a rustfmt diff:\n"
                f"{criterion_result.stdout}{criterion_result.stderr}"
            )

        if failures:
            raise SystemExit("\n".join(failures))
    print(f"validated {len(SAMPLES) + 4} malformed pre-commit samples")


if __name__ == "__main__":
    main()
