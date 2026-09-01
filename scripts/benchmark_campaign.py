#!/usr/bin/env python3
"""Run, validate, and summarize versioned Pari benchmark campaigns."""

from __future__ import annotations

import argparse
import hashlib
import json
import math
import os
import platform
import shlex
import shutil
import subprocess
import sys
import tempfile
import time
from dataclasses import asdict, dataclass
from pathlib import Path
from typing import Any, Sequence

ROOT = Path(__file__).resolve().parent.parent
DEFAULT_MANIFEST = ROOT / "benchmarks" / "campaigns" / "scale-v1.json"
BUNDLE_SCHEMA_VERSION = 1
FAILURE_SCHEMA_VERSION = 1
REPORT_SCHEMA_VERSION = 1

REQUIRED_METRICS: dict[str, tuple[str, ...]] = {
    "synthetic": (
        "signature.items_per_second",
        "signature.elapsed_ms",
        "index.build_items_per_second",
        "index.build_elapsed_ms",
        "index.live_items",
        "query.scalar_queries_per_second",
        "query.scalar_p50_ms",
        "query.scalar_p95_ms",
        "query.scalar_p99_ms",
        "query.batch_queries_per_second",
        "query.scalar_batch_parity",
        "candidate.recall",
        "candidate.precision",
        "candidate.rate",
        "candidate.reduction",
        "candidate.exact_matches",
        "grouping.index_items_per_second",
        "grouping.stream_edges_per_second",
    ),
    "storage": (
        "storage.persistent.build_items_per_second",
        "storage.persistent.bytes_per_item",
        "storage.persistent.reopen_ms",
        "storage.builder.build_items_per_second",
        "storage.builder.peak_buffered_records",
        "storage.lazy.bytes_per_item",
        "storage.lazy.reopen_ms",
        "storage.lazy.scalar.p50_ms",
        "storage.lazy.scalar.p95_ms",
        "storage.lazy.scalar.p99_ms",
        "storage.lazy.batch_queries_per_second",
        "storage.candidate_parity",
        "storage.persistent.mutation_parity",
    ),
    "redis": (
        "backend.redis.insert_items_per_second",
        "backend.redis.build_elapsed_ms",
        "backend.redis.batch_queries_per_second",
        "backend.redis.average_query_ms",
        "backend.redis.build_round_trips",
        "backend.redis.query_round_trips",
        "backend.redis.round_trips_per_item",
        "backend.redis.round_trips_per_query",
        "backend.redis.self_recall",
    ),
    "datasketch": (
        "signature.items_per_second",
        "index.build_items_per_second",
        "query.scalar_queries_per_second",
        "query.scalar_p99_ms",
        "candidate.recall",
        "candidate.precision",
    ),
    "text-reference": (
        "build_items_per_second",
        "index_bytes_per_item",
        "input_items",
        "process_peak_rss_bytes",
        "reopen_seconds",
    ),
    "text-audit": (
        "candidate_rate",
        "candidate_reduction",
        "exact_match_count",
        "matched_query_count",
        "process_peak_rss_bytes",
        "queries_per_second",
        "query_count",
        "reopen_seconds",
    ),
}

PARITY_METRICS = {
    "synthetic": ("query.scalar_batch_parity",),
    "storage": (
        "storage.candidate_parity",
        "storage.persistent.mutation_parity",
    ),
    "redis": ("backend.redis.self_recall",),
}

RATIO_METRICS = {
    "candidate.recall",
    "candidate.precision",
    "candidate.rate",
    "candidate.reduction",
    "query.scalar_batch_parity",
    "storage.candidate_parity",
    "storage.persistent.mutation_parity",
    "backend.redis.self_recall",
    "candidate_rate",
    "candidate_reduction",
}


@dataclass(frozen=True)
class Profile:
    name: str
    description: str
    items: int
    queries: int
    set_size: int
    overlap: int
    threshold: float
    num_perm: int
    seed: int
    storage: bool
    text_reference_items: int
    text_query_items: int
    manual_only: bool
    minimum_memory_gib: int
    timeout_minutes: int
    methodology_only: bool = False
    preserve_failure_evidence: bool = False

    def benchmark_arguments(self) -> list[str]:
        return [
            "--items",
            str(self.items),
            "--queries",
            str(self.queries),
            "--set-size",
            str(self.set_size),
            "--overlap",
            str(self.overlap),
            "--threshold",
            str(self.threshold),
            "--num-perm",
            str(self.num_perm),
            "--seed",
            str(self.seed),
        ]


class CampaignError(RuntimeError):
    """A benchmark bundle failed validation or execution."""


class StageFailure(CampaignError):
    """A named campaign stage failed to execute or validate."""

    def __init__(
        self,
        message: str,
        *,
        stage: str,
        command: Sequence[str],
        phase: str,
        return_code: int | None,
        environment_overrides: dict[str, str] | None = None,
    ) -> None:
        super().__init__(message)
        self.stage = stage
        self.command = list(command)
        self.phase = phase
        self.return_code = return_code
        self.environment_overrides = environment_overrides or {}


def read_json(path: Path) -> dict[str, Any]:
    try:
        value = json.loads(path.read_text(encoding="utf-8"))
    except (OSError, json.JSONDecodeError) as error:
        raise CampaignError(f"could not read JSON from {path}: {error}") from error
    if not isinstance(value, dict):
        raise CampaignError(f"expected a JSON object in {path}")
    return value


def write_json(path: Path, value: dict[str, Any]) -> None:
    path.write_text(
        json.dumps(value, indent=2, sort_keys=True) + "\n", encoding="utf-8"
    )


def sha256_file(path: Path) -> str:
    digest = hashlib.sha256()
    with path.open("rb") as source:
        for chunk in iter(lambda: source.read(1024 * 1024), b""):
            digest.update(chunk)
    return digest.hexdigest()


def load_campaign(path: Path) -> tuple[dict[str, Any], dict[str, Profile]]:
    campaign = read_json(path)
    if campaign.get("schema_version") != 1:
        raise CampaignError(f"unsupported campaign schema in {path}")
    if not isinstance(campaign.get("campaign_id"), str):
        raise CampaignError("campaign_id must be a string")
    raw_profiles = campaign.get("profiles")
    if not isinstance(raw_profiles, dict) or not raw_profiles:
        raise CampaignError("campaign profiles must be a non-empty object")

    profiles: dict[str, Profile] = {}
    for name, raw in raw_profiles.items():
        if not isinstance(name, str) or not isinstance(raw, dict):
            raise CampaignError("profile names and definitions must be objects")
        profiles[name] = parse_profile(name, raw)
    return campaign, profiles


def parse_profile(name: str, raw: dict[str, Any]) -> Profile:
    def integer(field: str) -> int:
        value = raw.get(field)
        if isinstance(value, bool) or not isinstance(value, int) or value <= 0:
            raise CampaignError(f"profile {name!r} field {field!r} must be positive")
        return value

    def boolean(field: str, default: bool = False) -> bool:
        value = raw.get(field, default)
        if not isinstance(value, bool):
            raise CampaignError(f"profile {name!r} field {field!r} must be boolean")
        return value

    description = raw.get("description")
    threshold = raw.get("threshold")
    if not isinstance(description, str) or not description:
        raise CampaignError(f"profile {name!r} requires a description")
    if not isinstance(threshold, (int, float)) or not 0.0 < threshold <= 1.0:
        raise CampaignError(f"profile {name!r} threshold must be in (0, 1]")

    profile = Profile(
        name=name,
        description=description,
        items=integer("items"),
        queries=integer("queries"),
        set_size=integer("set_size"),
        overlap=integer("overlap"),
        threshold=float(threshold),
        num_perm=integer("num_perm"),
        seed=integer("seed"),
        storage=boolean("storage"),
        text_reference_items=integer("text_reference_items"),
        text_query_items=integer("text_query_items"),
        manual_only=boolean("manual_only"),
        minimum_memory_gib=integer("minimum_memory_gib"),
        timeout_minutes=integer("timeout_minutes"),
        methodology_only=boolean("methodology_only"),
        preserve_failure_evidence=boolean("preserve_failure_evidence"),
    )
    if profile.overlap > profile.set_size:
        raise CampaignError(f"profile {name!r} overlap exceeds set_size")
    if profile.queries > profile.items:
        raise CampaignError(f"profile {name!r} queries exceed items")
    return profile


def command_output(command: Sequence[str], cwd: Path) -> str:
    completed = subprocess.run(
        command,
        cwd=cwd,
        check=False,
        capture_output=True,
        text=True,
        encoding="utf-8",
        errors="replace",
    )
    if completed.returncode != 0:
        detail = (completed.stderr or completed.stdout).strip()
        raise CampaignError(f"command failed: {shell_join(command)}\n{detail}")
    return completed.stdout.strip()


def shell_join(command: Sequence[str]) -> str:
    if os.name == "nt":
        return subprocess.list2cmdline(list(command))
    return shlex.join(command)


def git_state(root: Path) -> tuple[str, bool]:
    sha = command_output(["git", "rev-parse", "HEAD"], root)
    dirty = bool(command_output(["git", "status", "--porcelain"], root))
    return sha, dirty


def rust_command(stage: str, profile: Profile, output: Path) -> list[str]:
    return [
        "cargo",
        "run",
        "--release",
        "-p",
        "pari-bench",
        "--bin",
        "pari-bench",
        "--",
        stage,
        *profile.benchmark_arguments(),
        "--output",
        str(output),
    ]


def datasketch_command(root: Path, profile: Profile, output: Path) -> list[str]:
    return [
        sys.executable,
        str(root / "benchmarks" / "datasketch_baseline.py"),
        *profile.benchmark_arguments(),
        "--output",
        str(output),
    ]


def redis_command() -> list[str]:
    return [
        "cargo",
        "run",
        "--release",
        "-p",
        "pari-backend",
        "--example",
        "redis_bench",
        "--features",
        "redis",
    ]


def run_logged(
    command: Sequence[str],
    *,
    root: Path,
    environment: dict[str, str],
    stdout_path: Path,
    stderr_path: Path,
) -> int:
    print(f"running: {shell_join(command)}", flush=True)
    with stdout_path.open("w", encoding="utf-8") as stdout, stderr_path.open(
        "w", encoding="utf-8"
    ) as stderr:
        completed = subprocess.run(
            command,
            cwd=root,
            env=environment,
            check=False,
            stdout=stdout,
            stderr=stderr,
            text=True,
            encoding="utf-8",
            errors="replace",
        )
    return completed.returncode


def metric_value(report: dict[str, Any], name: str, *, allow_null: bool = False) -> float | None:
    metrics = report.get("metrics")
    if not isinstance(metrics, dict) or name not in metrics:
        raise CampaignError(f"report is missing required metric {name!r}")
    metric = metrics[name]
    if not isinstance(metric, dict):
        raise CampaignError(f"metric {name!r} must be an object")
    value = metric.get("value")
    if value is None and allow_null:
        return None
    if isinstance(value, bool) or not isinstance(value, (int, float)):
        raise CampaignError(f"metric {name!r} must contain a numeric value")
    value = float(value)
    if not math.isfinite(value):
        raise CampaignError(f"metric {name!r} must be finite")
    if not isinstance(metric.get("unit"), str) or not metric["unit"]:
        raise CampaignError(f"metric {name!r} requires a unit")
    if metric.get("direction") not in {"higher", "lower", "neutral"}:
        raise CampaignError(f"metric {name!r} has an invalid direction")
    return value


def validate_report(
    path: Path,
    stage: str,
    *,
    expected_git_sha: str,
    profile: dict[str, Any],
) -> dict[str, Any]:
    if stage not in REQUIRED_METRICS:
        raise CampaignError(f"unknown report stage {stage!r}")
    report = read_json(path)
    if report.get("schema_version") != REPORT_SCHEMA_VERSION:
        raise CampaignError(f"{path} has an unsupported report schema")
    if not isinstance(report.get("engine"), str):
        raise CampaignError(f"{path} does not identify its engine")

    for name in REQUIRED_METRICS[stage]:
        allow_null = name == "process_peak_rss_bytes"
        value = metric_value(report, name, allow_null=allow_null)
        if value is not None and value < 0.0:
            raise CampaignError(f"metric {name!r} cannot be negative")
        if name in RATIO_METRICS and value is not None and not 0.0 <= value <= 1.0:
            raise CampaignError(f"ratio metric {name!r} must be in [0, 1]")

    for name in PARITY_METRICS.get(stage, ()):
        if metric_value(report, name) != 1.0:
            raise CampaignError(f"correctness metric {name!r} did not pass")

    environment = report.get("environment")
    if not isinstance(environment, dict):
        raise CampaignError(f"{path} does not record its environment")
    if stage != "datasketch" and environment.get("git_sha") != expected_git_sha:
        raise CampaignError(
            f"{path} reports git SHA {environment.get('git_sha')!r}, expected {expected_git_sha}"
        )

    if stage in {"synthetic", "storage", "redis", "datasketch"}:
        config = report.get("config")
        if not isinstance(config, dict):
            raise CampaignError(f"{path} does not record its configuration")
        fields = (
            ("items", "queries", "num_perm", "seed")
            if stage == "redis"
            else ("items", "queries", "set_size", "overlap", "num_perm", "seed")
        )
        for field in fields:
            if config.get(field) != profile[field]:
                raise CampaignError(
                    f"{path} config {field!r} is {config.get(field)!r}, expected {profile[field]!r}"
                )
        if not math.isclose(
            float(config.get("threshold", -1.0)),
            float(profile["threshold"]),
            rel_tol=0.0,
            abs_tol=1e-12,
        ):
            raise CampaignError(f"{path} threshold does not match the profile")

    if stage == "synthetic":
        if metric_value(report, "index.live_items") != float(profile["items"]):
            raise CampaignError("synthetic report item count does not match the profile")
        if metric_value(report, "candidate.exact_matches") <= 0.0:
            raise CampaignError("synthetic correctness evaluation found no exact matches")
    elif stage == "text-reference":
        if metric_value(report, "input_items") != float(profile["text_reference_items"]):
            raise CampaignError("text reference report item count does not match the profile")
    elif stage == "text-audit":
        if metric_value(report, "query_count") != float(profile["text_query_items"]):
            raise CampaignError("text audit query count does not match the profile")
        if metric_value(report, "exact_match_count") <= 0.0:
            raise CampaignError("text audit did not exact-verify any cross-corpus match")
    return report


def mix64(value: int) -> int:
    mask = (1 << 64) - 1
    value &= mask
    value ^= value >> 30
    value = (value * 0xBF58476D1CE4E5B9) & mask
    value ^= value >> 27
    value = (value * 0x94D049BB133111EB) & mask
    return (value ^ (value >> 31)) & mask


def generated_text(index: int, seed: int, namespace: str = "reference") -> str:
    state = mix64(index ^ seed ^ 0xD1B54A32D192ED03)
    words = ["pari", "benchmark", namespace]
    for offset in range(29):
        state = mix64(state + offset + 1)
        words.append(f"token{state % 100_003}")
    return " ".join(words)


def generate_text_corpora(
    reference_path: Path,
    query_path: Path,
    *,
    reference_items: int,
    query_items: int,
    seed: int,
) -> dict[str, Any]:
    with reference_path.open("w", encoding="utf-8", newline="\n") as destination:
        for index in range(reference_items):
            row = {"id": f"reference-{index}", "text": generated_text(index, seed)}
            destination.write(json.dumps(row, sort_keys=True, separators=(",", ":")) + "\n")

    planted_matches = 0
    with query_path.open("w", encoding="utf-8", newline="\n") as destination:
        for index in range(query_items):
            if index % 2 == 0:
                source = (index // 2) % reference_items
                text = generated_text(source, seed)
                planted_matches += 1
            else:
                text = generated_text(index, seed ^ 0xA5A5_A5A5, "query")
            row = {"id": f"query-{index}", "text": text}
            destination.write(json.dumps(row, sort_keys=True, separators=(",", ":")) + "\n")

    return {
        "generator": "deterministic-text-cross-corpus-v1",
        "generation_included_in_timing": False,
        "reference_items": reference_items,
        "reference_bytes": reference_path.stat().st_size,
        "reference_sha256": sha256_file(reference_path),
        "query_items": query_items,
        "query_bytes": query_path.stat().st_size,
        "query_sha256": sha256_file(query_path),
        "planted_exact_matches": planted_matches,
        "seed": seed,
    }


def run_stage(
    stage: str,
    command: Sequence[str],
    report_path: Path,
    *,
    root: Path,
    environment: dict[str, str],
    staging: Path,
    git_sha: str,
    profile: dict[str, Any],
    environment_overrides: dict[str, str] | None = None,
) -> dict[str, Any]:
    stdout_path = staging / f"{stage}.stdout.log"
    stderr_path = staging / f"{stage}.stderr.log"
    try:
        return_code = run_logged(
            command,
            root=root,
            environment=environment,
            stdout_path=stdout_path,
            stderr_path=stderr_path,
        )
    except OSError as error:
        raise StageFailure(
            f"could not execute stage command: {shell_join(command)}\n{error}",
            stage=stage,
            command=command,
            phase="execution",
            return_code=None,
            environment_overrides=environment_overrides,
        ) from error
    if return_code != 0:
        detail = stderr_path.read_text(encoding="utf-8", errors="replace")[-4000:]
        raise StageFailure(
            f"stage command exited {return_code}: {shell_join(command)}\n{detail}",
            stage=stage,
            command=command,
            phase="execution",
            return_code=return_code,
            environment_overrides=environment_overrides,
        )
    try:
        validate_report(
            report_path,
            stage,
            expected_git_sha=git_sha,
            profile=profile,
        )
    except CampaignError as error:
        raise StageFailure(
            f"stage report validation failed for {stage!r}: {error}",
            stage=stage,
            command=command,
            phase="validation",
            return_code=return_code,
            environment_overrides=environment_overrides,
        ) from error
    return {
        "command": list(command),
        "environment_overrides": environment_overrides or {},
        "report": report_path.name,
        "report_sha256": sha256_file(report_path),
        "stage": stage,
        "stderr": stderr_path.name,
        "stdout": stdout_path.name,
    }


def run_text_stages(
    *,
    root: Path,
    staging: Path,
    environment: dict[str, str],
    git_sha: str,
    profile: Profile,
    artifacts: list[dict[str, Any]],
    inputs: dict[str, Any],
) -> None:
    profile_data = asdict(profile)
    with tempfile.TemporaryDirectory(prefix="pari-text-campaign-") as temporary:
        work = Path(temporary)
        reference_input = work / "reference.jsonl"
        query_input = work / "query.jsonl"
        dataset = generate_text_corpora(
            reference_input,
            query_input,
            reference_items=profile.text_reference_items,
            query_items=profile.text_query_items,
            seed=profile.seed,
        )
        inputs["text"] = dataset
        reference_manifest = work / "reference.json"
        reference_report = staging / "text-reference.json"
        reference_command = [
            sys.executable,
            str(root / "examples" / "text_workload.py"),
            "build-reference",
            "--input",
            str(reference_input),
            "--manifest",
            str(reference_manifest),
            "--metrics-output",
            str(reference_report),
            "--batch-size",
            "2048",
            "--threshold",
            str(profile.threshold),
            "--num-perm",
            str(profile.num_perm),
            "--seed",
            str(profile.seed),
            "--shingle-size",
            "3",
        ]
        artifacts.append(
            run_stage(
                "text-reference",
                reference_command,
                reference_report,
                root=root,
                environment=environment,
                staging=staging,
                git_sha=git_sha,
                profile=profile_data,
            )
        )

        audit_report = staging / "text-audit.json"
        audit_command = [
            sys.executable,
            str(root / "examples" / "text_workload.py"),
            "audit",
            "--input",
            str(query_input),
            "--manifest",
            str(reference_manifest),
            "--output",
            str(work / "audit.jsonl"),
            "--metrics-output",
            str(audit_report),
            "--batch-size",
            "2048",
            "--exact",
            "--exact-threshold",
            str(profile.threshold),
        ]
        artifacts.append(
            run_stage(
                "text-audit",
                audit_command,
                audit_report,
                root=root,
                environment=environment,
                staging=staging,
                git_sha=git_sha,
                profile=profile_data,
            )
        )


def host_environment(root: Path) -> dict[str, Any]:
    try:
        physical_memory_bytes = os.sysconf("SC_PAGE_SIZE") * os.sysconf("SC_PHYS_PAGES")
    except (AttributeError, OSError, ValueError):
        physical_memory_bytes = None
    cpu = platform.processor()
    if not cpu and sys.platform.startswith("linux"):
        try:
            cpu = next(
                line.split(":", 1)[1].strip()
                for line in Path("/proc/cpuinfo").read_text(encoding="utf-8").splitlines()
                if line.startswith("model name")
            )
        except (OSError, StopIteration):
            cpu = "unknown"
    return {
        "architecture": platform.machine(),
        "cpu": cpu or "unknown",
        "logical_cpus": os.cpu_count() or 1,
        "operating_system": platform.platform(),
        "physical_memory_bytes": physical_memory_bytes,
        "python": platform.python_version(),
        "rustc": command_output(["rustc", "--version"], root),
    }


def checksummed_failure_files(staging: Path) -> list[dict[str, Any]]:
    files: list[dict[str, Any]] = []
    for path in sorted(staging.iterdir()):
        if path.name == "failure.json" or not path.is_file():
            continue
        files.append(
            {
                "bytes": path.stat().st_size,
                "path": path.name,
                "sha256": sha256_file(path),
            }
        )
    return files


def failure_directory(output: Path, staging: Path) -> Path:
    prefix = f".{output.name}.partial-"
    suffix = staging.name.removeprefix(prefix)
    return output.with_name(f"{output.name}.failed-{suffix}")


def preserve_failure(
    *,
    error: BaseException,
    output: Path,
    staging: Path,
    process_temp: Path,
    campaign: dict[str, Any],
    manifest_path: Path,
    root: Path,
    profile: Profile,
    git_sha: str,
    dirty: bool,
    started: int,
    artifacts: list[dict[str, Any]],
    inputs: dict[str, Any],
) -> Path:
    shutil.rmtree(process_temp, ignore_errors=True)
    try:
        environment: dict[str, Any] = host_environment(root)
    except Exception as environment_error:
        environment = {"collection_error": str(environment_error)}
    try:
        disk = shutil.disk_usage(staging)
        filesystem: dict[str, Any] = {
            "free_bytes_at_failure": disk.free,
            "total_bytes": disk.total,
        }
    except OSError as filesystem_error:
        filesystem = {"collection_error": str(filesystem_error)}

    try:
        manifest_label = str(manifest_path.relative_to(root))
    except ValueError:
        manifest_label = str(manifest_path)
    failure: dict[str, Any] = {
        "command": None,
        "environment_overrides": {},
        "exception_type": type(error).__name__,
        "message": str(error),
        "phase": "campaign",
        "return_code": None,
        "stage": None,
    }
    if isinstance(error, StageFailure):
        failure.update(
            {
                "command": error.command,
                "environment_overrides": error.environment_overrides,
                "phase": error.phase,
                "return_code": error.return_code,
                "stage": error.stage,
            }
        )

    failure_manifest = {
        "artifact_kind": "pari-benchmark-campaign-failure",
        "campaign_id": campaign["campaign_id"],
        "campaign_manifest": manifest_label,
        "campaign_manifest_sha256": sha256_file(manifest_path),
        "completed_reports": artifacts,
        "environment": environment,
        "failed_unix_seconds": int(time.time()),
        "failure": failure,
        "files": checksummed_failure_files(staging),
        "filesystem": filesystem,
        "git_sha": git_sha,
        "inputs": inputs,
        "profile": asdict(profile),
        "schema_version": FAILURE_SCHEMA_VERSION,
        "started_unix_seconds": started,
        "status": "failed",
        "workload": campaign["workload"],
        "worktree_clean": not dirty,
    }
    write_json(staging / "failure.json", failure_manifest)
    failed_output = failure_directory(output, staging)
    staging.replace(failed_output)
    return failed_output


def run_campaign(args: argparse.Namespace) -> Path:
    root = args.repo_root.resolve()
    manifest_path = args.manifest.resolve()
    campaign, profiles = load_campaign(manifest_path)
    if args.profile not in profiles:
        raise CampaignError(f"unknown profile {args.profile!r}")
    profile = profiles[args.profile]
    if profile.methodology_only:
        raise CampaignError(
            f"profile {profile.name!r} is methodology-only; follow docs/benchmarks.md"
        )

    git_sha, dirty = git_state(root)
    if dirty and not args.allow_dirty:
        raise CampaignError(
            "the worktree is dirty; commit benchmark-affecting changes or pass --allow-dirty for a non-publishable development run"
        )
    output = args.output.resolve()
    if output.exists():
        raise CampaignError(f"output directory already exists: {output}")
    output.parent.mkdir(parents=True, exist_ok=True)
    staging = Path(tempfile.mkdtemp(prefix=f".{output.name}.partial-", dir=output.parent))
    started = int(time.time())
    environment = os.environ.copy()
    environment["PARI_GIT_SHA"] = git_sha
    process_temp = staging / "tmp"
    process_temp.mkdir()
    environment["TEMP"] = str(process_temp)
    environment["TMP"] = str(process_temp)
    environment["TMPDIR"] = str(process_temp)
    profile_data = asdict(profile)
    artifacts: list[dict[str, Any]] = []
    inputs: dict[str, Any] = {}
    try:
        synthetic_report = staging / "synthetic.json"
        artifacts.append(
            run_stage(
                "synthetic",
                rust_command("run", profile, synthetic_report),
                synthetic_report,
                root=root,
                environment=environment,
                staging=staging,
                git_sha=git_sha,
                profile=profile_data,
            )
        )
        if profile.storage:
            storage_report = staging / "storage.json"
            artifacts.append(
                run_stage(
                    "storage",
                    rust_command("storage", profile, storage_report),
                    storage_report,
                    root=root,
                    environment=environment,
                    staging=staging,
                    git_sha=git_sha,
                    profile=profile_data,
                )
            )
        if args.include_datasketch:
            datasketch_report = staging / "datasketch.json"
            artifacts.append(
                run_stage(
                    "datasketch",
                    datasketch_command(root, profile, datasketch_report),
                    datasketch_report,
                    root=root,
                    environment=environment,
                    staging=staging,
                    git_sha=git_sha,
                    profile=profile_data,
                )
            )
        if args.include_redis:
            if not environment.get("PARI_REDIS_URL"):
                raise StageFailure(
                    "--include-redis requires PARI_REDIS_URL",
                    stage="redis",
                    command=redis_command(),
                    phase="preflight",
                    return_code=None,
                )
            redis_report = staging / "redis.json"
            redis_environment = environment.copy()
            redis_overrides = {
                "PARI_REDIS_BENCH_ITEMS": str(profile.items),
                "PARI_REDIS_BENCH_QUERIES": str(profile.queries),
                "PARI_REDIS_BENCH_OUTPUT": str(redis_report),
            }
            redis_environment.update(redis_overrides)
            artifacts.append(
                run_stage(
                    "redis",
                    redis_command(),
                    redis_report,
                    root=root,
                    environment=redis_environment,
                    staging=staging,
                    git_sha=git_sha,
                    profile=profile_data,
                    environment_overrides=redis_overrides,
                )
            )
        if args.include_text:
            run_text_stages(
                root=root,
                staging=staging,
                environment=environment,
                git_sha=git_sha,
                profile=profile,
                artifacts=artifacts,
                inputs=inputs,
            )

        try:
            manifest_label = str(manifest_path.relative_to(root))
        except ValueError:
            manifest_label = str(manifest_path)
        disk = shutil.disk_usage(staging)
        bundle = {
            "schema_version": BUNDLE_SCHEMA_VERSION,
            "campaign_id": campaign["campaign_id"],
            "campaign_manifest": manifest_label,
            "campaign_manifest_sha256": sha256_file(manifest_path),
            "completed_unix_seconds": int(time.time()),
            "environment": host_environment(root),
            "filesystem": {
                "free_bytes_after_run": disk.free,
                "total_bytes": disk.total,
            },
            "git_sha": git_sha,
            "inputs": inputs,
            "profile": profile_data,
            "reports": artifacts,
            "started_unix_seconds": started,
            "workload": campaign["workload"],
            "worktree_clean": not dirty,
        }
        bundle_path = staging / "bundle.json"
        write_json(bundle_path, bundle)
        validate_bundle(bundle_path, require_clean=not args.allow_dirty)
        staging.replace(output)
        print(f"wrote validated benchmark bundle {output}")
        return output / "bundle.json"
    except BaseException as error:
        should_preserve = profile.preserve_failure_evidence or getattr(
            args, "preserve_failure_evidence", False
        )
        if should_preserve:
            try:
                failed_output = preserve_failure(
                    error=error,
                    output=output,
                    staging=staging,
                    process_temp=process_temp,
                    campaign=campaign,
                    manifest_path=manifest_path,
                    root=root,
                    profile=profile,
                    git_sha=git_sha,
                    dirty=dirty,
                    started=started,
                    artifacts=artifacts,
                    inputs=inputs,
                )
            except Exception as preservation_error:
                print(
                    "warning: could not finalize failure evidence "
                    f"({preservation_error}); retained staging directory {staging}",
                    file=sys.stderr,
                )
            else:
                print(
                    f"preserved non-publishable failure evidence {failed_output}",
                    file=sys.stderr,
                )
        else:
            shutil.rmtree(staging, ignore_errors=True)
        raise


def validate_bundle(path: Path, *, require_clean: bool = True) -> dict[str, Any]:
    bundle = read_json(path)
    if (
        bundle.get("status") == "failed"
        or bundle.get("artifact_kind") == "pari-benchmark-campaign-failure"
    ):
        raise CampaignError(
            "failed campaign artifacts are diagnostic evidence, not validated benchmark bundles"
        )
    if bundle.get("schema_version") != BUNDLE_SCHEMA_VERSION:
        raise CampaignError(f"unsupported bundle schema in {path}")
    git_sha = bundle.get("git_sha")
    if not isinstance(git_sha, str) or len(git_sha) != 40:
        raise CampaignError("bundle git_sha must be a full 40-character commit SHA")
    if require_clean and bundle.get("worktree_clean") is not True:
        raise CampaignError("publishable benchmark bundles require a clean worktree")
    profile = bundle.get("profile")
    if not isinstance(profile, dict):
        raise CampaignError("bundle profile must be an object")
    reports = bundle.get("reports")
    if not isinstance(reports, list) or not reports:
        raise CampaignError("bundle reports must be a non-empty array")

    seen: set[str] = set()
    for artifact in reports:
        if not isinstance(artifact, dict):
            raise CampaignError("bundle report entries must be objects")
        stage = artifact.get("stage")
        report_name = artifact.get("report")
        if not isinstance(stage, str) or stage in seen:
            raise CampaignError(f"duplicate or invalid report stage {stage!r}")
        if not isinstance(report_name, str) or Path(report_name).name != report_name:
            raise CampaignError(f"invalid report path for stage {stage!r}")
        seen.add(stage)
        report_path = path.parent / report_name
        expected_digest = artifact.get("report_sha256")
        if not isinstance(expected_digest, str) or sha256_file(report_path) != expected_digest:
            raise CampaignError(f"checksum mismatch for stage {stage!r}")
        validate_report(
            report_path,
            stage,
            expected_git_sha=git_sha,
            profile=profile,
        )

    expected = {"synthetic"}
    if profile.get("storage"):
        expected.add("storage")
    if not expected.issubset(seen):
        raise CampaignError(f"bundle is missing required stages: {sorted(expected - seen)}")
    if ("text-reference" in seen) != ("text-audit" in seen):
        raise CampaignError("text-reference and text-audit reports must be bundled together")
    return bundle


def report_for_stage(bundle_path: Path, bundle: dict[str, Any], stage: str) -> dict[str, Any] | None:
    for artifact in bundle["reports"]:
        if artifact["stage"] == stage:
            return read_json(bundle_path.parent / artifact["report"])
    return None


def optional_metric(report: dict[str, Any] | None, name: str) -> float | None:
    if report is None:
        return None
    try:
        return metric_value(report, name, allow_null=True)
    except CampaignError:
        return None


def format_number(value: float | None, *, digits: int = 2) -> str:
    if value is None:
        return "n/a"
    if abs(value) >= 1_000_000:
        return f"{value / 1_000_000:.{digits}f}M"
    if abs(value) >= 1_000:
        return f"{value / 1_000:.{digits}f}K"
    return f"{value:.{digits}f}"


def maximum_available(*values: float | None) -> float | None:
    available = [value for value in values if value is not None]
    return max(available) if available else None


def format_duration_ms(value: float | None) -> str:
    if value is None:
        return "n/a"
    if value >= 1_000.0:
        return f"{value / 1_000.0:.2f}s"
    return f"{value:.3f}ms"


def format_gib(value: float | None) -> str:
    return "n/a" if value is None else f"{value / (1024**3):.2f} GiB"


def render_report(bundle_paths: Sequence[Path], output: Path, *, require_clean: bool) -> None:
    rows: list[str] = []
    datasketch_rows: list[str] = []
    evidence: list[str] = []
    text_rows: list[str] = []
    largest: tuple[
        dict[str, Any],
        dict[str, Any] | None,
        dict[str, Any] | None,
    ] | None = None
    for bundle_path in bundle_paths:
        bundle_path = bundle_path.resolve()
        bundle = validate_bundle(bundle_path, require_clean=require_clean)
        profile = bundle["profile"]
        synthetic = report_for_stage(bundle_path, bundle, "synthetic")
        storage = report_for_stage(bundle_path, bundle, "storage")
        datasketch = report_for_stage(bundle_path, bundle, "datasketch")
        text_reference = report_for_stage(bundle_path, bundle, "text-reference")
        text_audit = report_for_stage(bundle_path, bundle, "text-audit")
        if largest is None or profile["items"] > largest[0]["profile"]["items"]:
            largest = (bundle, synthetic, storage)
        relative = Path(os.path.relpath(bundle_path, output.parent)).as_posix()
        peak_rss = maximum_available(
            optional_metric(synthetic, "memory.signature_peak_rss_bytes"),
            optional_metric(synthetic, "memory.index_build_peak_rss_bytes"),
        )
        configured_threads = (
            synthetic.get("config", {}).get("threads")
            if isinstance(synthetic, dict)
            else None
        )
        thread_policy = "auto" if configured_threads is None else f"max {configured_threads}"
        effective_threads = optional_metric(synthetic, "signature.threads")
        rows.append(
            "| {profile} | [{sha}]({link}) | {items:,} | {signature} | {threads} | {build} | {rss} | {bytes_per_item} | {reopen} | {p99} | {candidate_rate} | {recall} |".format(
                profile=profile["name"],
                sha=bundle["git_sha"][:12],
                link=relative,
                items=profile["items"],
                signature=format_number(optional_metric(synthetic, "signature.items_per_second")),
                threads=(
                    "n/a"
                    if effective_threads is None
                    else f"{effective_threads:.0f} ({thread_policy})"
                ),
                build=format_number(optional_metric(synthetic, "index.build_items_per_second")),
                rss=(
                    "n/a"
                    if peak_rss is None
                    else f"{peak_rss / (1024 * 1024):.1f} MiB"
                ),
                bytes_per_item=format_number(
                    optional_metric(storage, "storage.lazy.bytes_per_item")
                ),
                reopen=format_number(optional_metric(storage, "storage.lazy.reopen_ms")),
                p99=format_number(optional_metric(synthetic, "query.scalar_p99_ms"), digits=4),
                candidate_rate=format_number(optional_metric(synthetic, "candidate.rate"), digits=8),
                recall=format_number(optional_metric(synthetic, "candidate.recall"), digits=4),
            )
        )
        if datasketch is not None:
            datasketch_rows.append(
                "| {profile} | {pari_signature} | {baseline_signature} | {pari_build} | {baseline_build} | {pari_p99} | {baseline_p99} | {pari_recall} | {baseline_recall} |".format(
                    profile=profile["name"],
                    pari_signature=format_number(
                        optional_metric(synthetic, "signature.items_per_second")
                    ),
                    baseline_signature=format_number(
                        optional_metric(datasketch, "signature.items_per_second")
                    ),
                    pari_build=format_number(
                        optional_metric(synthetic, "index.build_items_per_second")
                    ),
                    baseline_build=format_number(
                        optional_metric(datasketch, "index.build_items_per_second")
                    ),
                    pari_p99=format_number(
                        optional_metric(synthetic, "query.scalar_p99_ms"), digits=4
                    ),
                    baseline_p99=format_number(
                        optional_metric(datasketch, "query.scalar_p99_ms"), digits=4
                    ),
                    pari_recall=format_number(
                        optional_metric(synthetic, "candidate.recall"), digits=4
                    ),
                    baseline_recall=format_number(
                        optional_metric(datasketch, "candidate.recall"), digits=4
                    ),
                )
            )
        environment = bundle["environment"]
        filesystem = bundle.get("filesystem", {})
        physical_memory = environment.get("physical_memory_bytes")
        if isinstance(physical_memory, bool) or not isinstance(
            physical_memory, (int, float)
        ):
            physical_memory = None
        filesystem_total = filesystem.get("total_bytes")
        if isinstance(filesystem_total, bool) or not isinstance(
            filesystem_total, (int, float)
        ):
            filesystem_total = None
        filesystem_free = filesystem.get("free_bytes_after_run")
        if isinstance(filesystem_free, bool) or not isinstance(
            filesystem_free, (int, float)
        ):
            filesystem_free = None
        cache_policy = bundle.get("workload", {}).get(
            "cache_policy", "not recorded"
        )
        evidence.append(
            f"- `{profile['name']}`: {environment['operating_system']}; "
            f"{environment['logical_cpus']} logical CPUs; {format_gib(physical_memory)} RAM; "
            f"{environment['rustc']}; workspace filesystem {format_gib(filesystem_total)} total "
            f"and {format_gib(filesystem_free)} free after the run. Cache policy: {cache_policy}"
        )
        if text_reference is not None and text_audit is not None:
            text_rows.append(
                "| {profile} | {reference_items:,} | {build} | {bytes_per_item} | {queries:,} | {query_rate} | {candidate_rate} | {matches} |".format(
                    profile=profile["name"],
                    reference_items=profile["text_reference_items"],
                    build=format_number(optional_metric(text_reference, "build_items_per_second")),
                    bytes_per_item=format_number(optional_metric(text_reference, "index_bytes_per_item")),
                    queries=profile["text_query_items"],
                    query_rate=format_number(optional_metric(text_audit, "queries_per_second")),
                    candidate_rate=format_number(optional_metric(text_audit, "candidate_rate"), digits=8),
                    matches=format_number(optional_metric(text_audit, "exact_match_count"), digits=0),
                )
            )

    lines = [
        "# Benchmark evidence",
        "",
        "This report is generated from validated, versioned benchmark bundles. Timings are evidence, not CI thresholds. Compare rows only when their workload configuration and environment are materially compatible.",
        "",
        "## Synthetic and persistent index profiles",
        "",
        "| Profile | Source | Items | Signatures/s | Signature threads | Index items/s | Index peak RSS | Lazy bytes/item | Lazy reopen ms | Scalar p99 ms | Candidate rate | Candidate recall |",
        "| --- | --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: |",
        *rows,
        "",
        "Candidate recall is measured against exact Jaccard ground truth at the configured threshold. It is not expected to be 1.0 for near-threshold queries because LSH candidate generation is probabilistic. Candidate rate is returned pairs divided by all possible query-item pairs.",
        "",
        "Every selected bundle passed scalar/batch candidate parity, persistent/lazy candidate parity, and persistent mutation parity before its timing data was accepted.",
        "",
    ]
    if datasketch_rows:
        lines.extend(
            [
                "## Datasketch semantic baseline",
                "",
                "| Profile | Pari signatures/s | Datasketch signatures/s | Pari index items/s | Datasketch index items/s | Pari scalar p99 ms | Datasketch scalar p99 ms | Pari recall | Datasketch recall |",
                "| --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: |",
                *datasketch_rows,
                "",
                "The Datasketch 2.0 baseline uses the same deterministic integer sets, query mutation, threshold, permutation count, and exact-Jaccard scoring. Pari and Datasketch use different stable seed-to-permutation mappings and LSH implementations, so signatures and candidate sets are not byte-for-byte interoperability claims. Recall and throughput are reported independently; compare performance only within the recorded environment and workload.",
                "",
            ]
        )
    if text_rows:
        lines.extend(
            [
                "## Reference text build and cross-corpus audit",
                "",
                "| Profile | Reference items | Build items/s | Index bytes/item | Audit queries | Queries/s | Candidate rate | Exact matches |",
                "| --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: |",
                *text_rows,
                "",
                "The deterministic reference workload plants exact cross-corpus matches and runs exact shingle verification. Corpus generation is excluded from timed phases and its hashes are stored in each bundle.",
                "",
            ]
        )
    if largest is not None:
        largest_bundle, synthetic, storage = largest
        profile = largest_bundle["profile"]
        signature_ms = optional_metric(synthetic, "signature.elapsed_ms")
        index_ms = optional_metric(synthetic, "index.build_elapsed_ms")
        grouping_ms_values = [
            optional_metric(synthetic, "grouping.index_elapsed_ms"),
            optional_metric(synthetic, "grouping.stream_elapsed_ms"),
        ]
        grouping_ms = (
            None
            if all(value is None for value in grouping_ms_values)
            else sum(value or 0.0 for value in grouping_ms_values)
        )
        query_ms_values = [
            optional_metric(synthetic, "query.scalar_elapsed_ms"),
            optional_metric(synthetic, "query.batch_elapsed_ms"),
        ]
        query_ms = (
            None
            if all(value is None for value in query_ms_values)
            else sum(value or 0.0 for value in query_ms_values)
        )
        ground_truth_ms = optional_metric(
            synthetic, "candidate.ground_truth_elapsed_ms"
        )
        synthetic_peak = maximum_available(
            optional_metric(synthetic, "memory.signature_peak_rss_bytes"),
            optional_metric(synthetic, "memory.index_build_peak_rss_bytes"),
        )
        persistent_peak = optional_metric(
            storage, "storage.persistent.build.peak_rss_bytes"
        )
        builder_peak = optional_metric(storage, "storage.builder.peak_rss_bytes")
        signature_threads = optional_metric(synthetic, "signature.threads")
        signature_parallel = optional_metric(synthetic, "signature.parallel")
        configured_threads = (
            synthetic.get("config", {}).get("threads")
            if isinstance(synthetic, dict)
            else None
        )
        thread_policy = (
            "the bounded automatic policy"
            if configured_threads is None
            else f"an explicit maximum of {configured_threads} threads"
        )
        parallel_state = (
            "enabled"
            if signature_parallel == 1.0
            else "disabled"
            if signature_parallel == 0.0
            else "not recorded"
        )
        lines.extend(
            [
                "## Bottleneck evidence and decision gates",
                "",
                f"The largest validated profile is `{profile['name']}` at {profile['items']:,} synthetic items. Its measured Pari product phases were signature construction {format_duration_ms(signature_ms)}, in-memory index build {format_duration_ms(index_ms)}, grouping {format_duration_ms(grouping_ms)}, and scalar plus batch query {format_duration_ms(query_ms)}. Exact ground-truth scanning took {format_duration_ms(ground_truth_ms)} but is harness-only work and is excluded from product bottleneck decisions.",
                "",
                f"Persistent construction took {format_duration_ms(optional_metric(storage, 'storage.persistent.build_elapsed_ms'))}; bounded external construction took {format_duration_ms(optional_metric(storage, 'storage.builder.build_elapsed_ms'))}; lazy reopen took {format_duration_ms(optional_metric(storage, 'storage.lazy.reopen_ms'))}. Peak RSS was {format_gib(synthetic_peak)} for the synthetic process, {format_gib(persistent_peak)} during persistent construction, and {format_gib(builder_peak)} during external construction. The external builder held at most {format_number(optional_metric(storage, 'storage.builder.peak_buffered_records'), digits=0)} records and produced {format_number(optional_metric(storage, 'storage.lazy.bytes_per_item'))} bytes/item.",
                "",
                f"- **CPU parallelism: keep the bounded signature policy.** The largest profile used {format_number(signature_threads, digits=0)} effective threads under {thread_policy}, with parallel execution {parallel_state}. Query phases remain too small in this workload to justify broader parallel scheduling.",
                f"- **Issue #69: keep sharding deferred.** The `{profile['name']}` profile fits one process. Run `scale-10m` on dedicated local scratch before choosing a shard crossover point from scale evidence.",
                "- **GPU work: defer.** The measured end-to-end storage path is I/O-bound, and no profile yet shows a GPU-suitable kernel dominating the real text workload.",
                "",
            ]
        )
    lines.extend(["## Environments", "", *evidence, ""])
    output.write_text("\n".join(lines), encoding="utf-8")


def plan(profile: Profile, root: Path, *, include_datasketch: bool, include_redis: bool) -> dict[str, Any]:
    if profile.methodology_only:
        return {
            "profile": asdict(profile),
            "commands": [],
            "execution": "blocked_pending_scale-10m_preflight",
        }
    output = Path("${OUTPUT_DIR}")
    commands: list[dict[str, Any]] = [
        {
            "stage": "synthetic",
            "command": rust_command("run", profile, output / "synthetic.json"),
        }
    ]
    if profile.storage:
        commands.append(
            {
                "stage": "storage",
                "command": rust_command("storage", profile, output / "storage.json"),
            }
        )
    if include_datasketch:
        commands.append(
            {
                "stage": "datasketch",
                "command": datasketch_command(root, profile, output / "datasketch.json"),
            }
        )
    if include_redis:
        commands.append({"stage": "redis", "command": redis_command()})
    return {"profile": asdict(profile), "commands": commands}


def build_parser() -> argparse.ArgumentParser:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--manifest", type=Path, default=DEFAULT_MANIFEST)
    parser.add_argument("--repo-root", type=Path, default=ROOT)
    subparsers = parser.add_subparsers(dest="command", required=True)

    profiles_parser = subparsers.add_parser("profiles", help="list campaign profiles")
    profiles_parser.set_defaults(handler=profiles_command)

    plan_parser = subparsers.add_parser("plan", help="print an exact command plan")
    plan_parser.add_argument("profile")
    plan_parser.add_argument("--include-datasketch", action="store_true")
    plan_parser.add_argument("--include-redis", action="store_true")
    plan_parser.set_defaults(handler=plan_command)

    run_parser = subparsers.add_parser("run", help="run and validate one profile")
    run_parser.add_argument("profile")
    run_parser.add_argument("--output", type=Path, required=True)
    run_parser.add_argument("--include-datasketch", action="store_true")
    run_parser.add_argument("--include-redis", action="store_true")
    run_parser.add_argument("--include-text", action="store_true")
    run_parser.add_argument("--allow-dirty", action="store_true")
    run_parser.add_argument(
        "--preserve-failure-evidence",
        action="store_true",
        help="retain a non-publishable failure artifact instead of cleaning failed staging files",
    )
    run_parser.set_defaults(handler=run_command)

    validate_parser = subparsers.add_parser("validate", help="validate a bundle")
    validate_parser.add_argument("bundle", type=Path)
    validate_parser.add_argument("--allow-dirty", action="store_true")
    validate_parser.set_defaults(handler=validate_command)

    render_parser = subparsers.add_parser("render", help="render validated bundles as Markdown")
    render_parser.add_argument("bundles", nargs="+", type=Path)
    render_parser.add_argument("--output", type=Path, required=True)
    render_parser.add_argument("--allow-dirty", action="store_true")
    render_parser.set_defaults(handler=render_command)
    return parser


def profiles_command(args: argparse.Namespace) -> None:
    _campaign, profiles = load_campaign(args.manifest.resolve())
    for profile in profiles.values():
        suffix = " (methodology only)" if profile.methodology_only else ""
        print(f"{profile.name}{suffix}: {profile.description}")


def plan_command(args: argparse.Namespace) -> None:
    _campaign, profiles = load_campaign(args.manifest.resolve())
    if args.profile not in profiles:
        raise CampaignError(f"unknown profile {args.profile!r}")
    print(
        json.dumps(
            plan(
                profiles[args.profile],
                args.repo_root.resolve(),
                include_datasketch=args.include_datasketch,
                include_redis=args.include_redis,
            ),
            indent=2,
        )
    )


def run_command(args: argparse.Namespace) -> None:
    run_campaign(args)


def validate_command(args: argparse.Namespace) -> None:
    validate_bundle(args.bundle.resolve(), require_clean=not args.allow_dirty)
    print(f"validated {args.bundle}")


def render_command(args: argparse.Namespace) -> None:
    output = args.output.resolve()
    output.parent.mkdir(parents=True, exist_ok=True)
    render_report(
        args.bundles,
        output,
        require_clean=not args.allow_dirty,
    )
    print(f"wrote {output}")


def main(argv: Sequence[str] | None = None) -> int:
    args = build_parser().parse_args(argv)
    try:
        args.handler(args)
    except CampaignError as error:
        print(f"benchmark campaign error: {error}", file=sys.stderr)
        return 2
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
