from __future__ import annotations

import importlib.util
import json
import sys
import tempfile
import unittest
from argparse import Namespace
from dataclasses import asdict
from pathlib import Path
from unittest.mock import patch

SCRIPT = Path(__file__).resolve().parents[1] / "benchmark_campaign.py"
ROOT = SCRIPT.parent.parent
SPEC = importlib.util.spec_from_file_location("pari_benchmark_campaign", SCRIPT)
if SPEC is None or SPEC.loader is None:
    raise RuntimeError("could not load benchmark campaign utility")
campaign = importlib.util.module_from_spec(SPEC)
sys.modules[SPEC.name] = campaign
SPEC.loader.exec_module(campaign)

GIT_SHA = "a" * 40


def metric(value: float | None, unit: str = "count", direction: str = "neutral") -> dict[str, object]:
    return {"direction": direction, "unit": unit, "value": value}


def report_for(stage: str, profile: dict[str, object]) -> dict[str, object]:
    metrics = {name: metric(1.0) for name in campaign.REQUIRED_METRICS[stage]}
    for name in campaign.RATIO_METRICS:
        if name in metrics:
            metrics[name] = metric(0.5, "ratio")
    for name in campaign.PARITY_METRICS.get(stage, ()):
        metrics[name] = metric(1.0, "ratio")
    if stage == "synthetic":
        metrics["index.live_items"] = metric(float(profile["items"]), "items")
        metrics["candidate.exact_matches"] = metric(1.0, "pairs")
    elif stage == "text-reference":
        metrics["input_items"] = metric(float(profile["text_reference_items"]), "items")
        metrics["process_peak_rss_bytes"] = metric(None, "bytes", "lower")
    elif stage == "text-audit":
        metrics["query_count"] = metric(float(profile["text_query_items"]), "queries")
        metrics["exact_match_count"] = metric(1.0, "pairs")
        metrics["process_peak_rss_bytes"] = metric(None, "bytes", "lower")

    config = {
        key: profile[key]
        for key in (
            "items",
            "queries",
            "set_size",
            "overlap",
            "threshold",
            "num_perm",
            "seed",
        )
    }
    return {
        "config": config,
        "engine": "datasketch" if stage == "datasketch" else "pari",
        "environment": {"git_sha": "datasketch-2.0.0" if stage == "datasketch" else GIT_SHA},
        "generated_unix_seconds": 1,
        "metrics": metrics,
        "schema_version": 1,
    }


class ProfileTests(unittest.TestCase):
    def test_manifest_exposes_reproducible_scale_profiles(self) -> None:
        manifest, profiles = campaign.load_campaign(campaign.DEFAULT_MANIFEST)

        self.assertEqual(manifest["campaign_id"], "pari-scale-v1")
        self.assertEqual(profiles["scale-100k"].items, 100_000)
        self.assertEqual(profiles["scale-1m"].items, 1_000_000)
        self.assertEqual(profiles["scale-10m"].items, 10_000_000)
        self.assertTrue(profiles["scale-10m"].preserve_failure_evidence)
        self.assertFalse(profiles["smoke"].preserve_failure_evidence)
        self.assertTrue(profiles["scale-100m-methodology"].methodology_only)

    def test_plan_uses_one_configuration_for_memory_and_storage(self) -> None:
        _manifest, profiles = campaign.load_campaign(campaign.DEFAULT_MANIFEST)
        planned = campaign.plan(
            profiles["scale-100k"],
            ROOT,
            include_datasketch=True,
            include_redis=True,
        )

        stages = [entry["stage"] for entry in planned["commands"]]
        self.assertEqual(stages, ["synthetic", "storage", "datasketch", "redis"])
        synthetic = planned["commands"][0]["command"]
        self.assertEqual(
            synthetic[:9],
            [
                "cargo",
                "run",
                "--release",
                "-p",
                "pari-bench",
                "--bin",
                "pari-bench",
                "--",
                "run",
            ],
        )
        self.assertIn("100000", synthetic)
        self.assertIn("--num-perm", synthetic)

    def test_methodology_profile_does_not_emit_executable_commands(self) -> None:
        _manifest, profiles = campaign.load_campaign(campaign.DEFAULT_MANIFEST)

        planned = campaign.plan(
            profiles["scale-100m-methodology"],
            ROOT,
            include_datasketch=True,
            include_redis=True,
        )

        self.assertEqual(planned["commands"], [])
        self.assertEqual(
            planned["execution"], "blocked_pending_scale-10m_preflight"
        )


class TextCorpusTests(unittest.TestCase):
    def test_generator_is_deterministic_and_records_planted_matches(self) -> None:
        with tempfile.TemporaryDirectory() as first, tempfile.TemporaryDirectory() as second:
            first_root = Path(first)
            second_root = Path(second)
            first_metadata = campaign.generate_text_corpora(
                first_root / "reference.jsonl",
                first_root / "query.jsonl",
                reference_items=8,
                query_items=6,
                seed=7,
            )
            second_metadata = campaign.generate_text_corpora(
                second_root / "reference.jsonl",
                second_root / "query.jsonl",
                reference_items=8,
                query_items=6,
                seed=7,
            )

            self.assertEqual(first_metadata, second_metadata)
            self.assertEqual(first_metadata["planted_exact_matches"], 3)
            self.assertEqual(
                (first_root / "reference.jsonl").read_bytes(),
                (second_root / "reference.jsonl").read_bytes(),
            )


class ValidationTests(unittest.TestCase):
    def setUp(self) -> None:
        _manifest, profiles = campaign.load_campaign(campaign.DEFAULT_MANIFEST)
        self.profile = asdict(profiles["smoke"])

    def test_report_validation_requires_correctness_parity(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            path = Path(temporary) / "synthetic.json"
            report = report_for("synthetic", self.profile)
            report["metrics"]["query.scalar_batch_parity"] = metric(0.0, "ratio")
            path.write_text(json.dumps(report), encoding="utf-8")

            with self.assertRaisesRegex(campaign.CampaignError, "did not pass"):
                campaign.validate_report(
                    path,
                    "synthetic",
                    expected_git_sha=GIT_SHA,
                    profile=self.profile,
                )

    def test_bundle_checks_checksums_and_renders_evidence(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            artifacts = []
            for stage in ("synthetic", "storage", "datasketch"):
                report_path = root / f"{stage}.json"
                report = report_for(stage, self.profile)
                if stage == "synthetic":
                    report["metrics"]["signature.threads"] = metric(1.0, "threads")
                    report["metrics"]["signature.parallel"] = metric(
                        0.0, "boolean"
                    )
                report_path.write_text(
                    json.dumps(report), encoding="utf-8"
                )
                artifacts.append(
                    {
                        "command": ["test", stage],
                        "environment_overrides": {},
                        "report": report_path.name,
                        "report_sha256": campaign.sha256_file(report_path),
                        "stage": stage,
                        "stderr": f"{stage}.stderr.log",
                        "stdout": f"{stage}.stdout.log",
                    }
                )
            bundle = {
                "schema_version": 1,
                "campaign_id": "test",
                "environment": {
                    "logical_cpus": 4,
                    "operating_system": "test-os",
                    "physical_memory_bytes": 16 * 1024**3,
                    "rustc": "rustc test",
                },
                "filesystem": {
                    "free_bytes_after_run": 80 * 1024**3,
                    "total_bytes": 150 * 1024**3,
                },
                "git_sha": GIT_SHA,
                "profile": self.profile,
                "reports": artifacts,
                "workload": {"cache_policy": "test cache policy."},
                "worktree_clean": True,
            }
            bundle_path = root / "bundle.json"
            bundle_path.write_text(json.dumps(bundle), encoding="utf-8")

            campaign.validate_bundle(bundle_path)
            output = root / "report.md"
            campaign.render_report([bundle_path], output, require_clean=True)
            rendered = output.read_text(encoding="utf-8")
            self.assertIn("Benchmark evidence", rendered)
            self.assertIn("Bottleneck evidence and decision gates", rendered)
            self.assertIn("Datasketch semantic baseline", rendered)
            self.assertIn("persistent/lazy candidate parity", rendered)
            self.assertIn("1 (auto)", rendered)
            self.assertIn("workspace filesystem 150.00 GiB total", rendered)
            self.assertIn("Cache policy: test cache policy.", rendered)
            self.assertNotIn("WSL-backed", rendered)
            self.assertIn("smoke", rendered)
            self.assertIn(GIT_SHA[:12], rendered)

            (root / "storage.json").write_text("{}", encoding="utf-8")
            with self.assertRaisesRegex(campaign.CampaignError, "checksum mismatch"):
                campaign.validate_bundle(bundle_path)


class CampaignExecutionTests(unittest.TestCase):
    def arguments(
        self,
        root: Path,
        *,
        profile: str,
        preserve_failure_evidence: bool = False,
    ) -> Namespace:
        return Namespace(
            repo_root=ROOT,
            manifest=campaign.DEFAULT_MANIFEST,
            profile=profile,
            output=root / profile,
            include_datasketch=False,
            include_redis=False,
            include_text=False,
            allow_dirty=False,
            preserve_failure_evidence=preserve_failure_evidence,
        )

    @staticmethod
    def failed_command(
        _command: list[str],
        *,
        root: Path,
        environment: dict[str, str],
        stdout_path: Path,
        stderr_path: Path,
    ) -> int:
        del root, environment
        stdout_path.write_text("partial telemetry\n", encoding="utf-8")
        stderr_path.write_text("killed: out of memory\n", encoding="utf-8")
        return 137

    @staticmethod
    def host() -> dict[str, object]:
        return {
            "architecture": "test",
            "cpu": "test",
            "logical_cpus": 4,
            "operating_system": "test-os",
            "physical_memory_bytes": 128 * 1024**3,
            "python": "test",
            "rustc": "rustc test",
        }

    def test_scale_10m_preserves_failed_stage_evidence(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            args = self.arguments(root, profile="scale-10m")
            with (
                patch.object(campaign, "git_state", return_value=(GIT_SHA, False)),
                patch.object(campaign, "run_logged", side_effect=self.failed_command),
                patch.object(campaign, "host_environment", return_value=self.host()),
                self.assertRaisesRegex(campaign.CampaignError, "exited 137"),
            ):
                campaign.run_campaign(args)

            self.assertFalse(args.output.exists())
            failures = list(root.glob("scale-10m.failed-*"))
            self.assertEqual(len(failures), 1)
            failure_path = failures[0] / "failure.json"
            failure = campaign.read_json(failure_path)
            self.assertEqual(failure["status"], "failed")
            self.assertEqual(
                failure["artifact_kind"], "pari-benchmark-campaign-failure"
            )
            self.assertEqual(failure["failure"]["stage"], "synthetic")
            self.assertEqual(failure["failure"]["phase"], "execution")
            self.assertEqual(failure["failure"]["return_code"], 137)
            command = failure["failure"]["command"]
            self.assertEqual(command[command.index("--") + 1], "run")
            self.assertEqual(command[command.index("--items") + 1], "10000000")
            self.assertEqual(failure["environment"], self.host())
            self.assertEqual(failure["completed_reports"], [])
            files = {entry["path"]: entry for entry in failure["files"]}
            self.assertEqual(
                set(files), {"synthetic.stderr.log", "synthetic.stdout.log"}
            )
            for name, entry in files.items():
                self.assertEqual(
                    entry["sha256"], campaign.sha256_file(failures[0] / name)
                )
            with self.assertRaisesRegex(
                campaign.CampaignError, "diagnostic evidence"
            ):
                campaign.validate_bundle(failure_path)
            with self.assertRaisesRegex(
                campaign.CampaignError, "diagnostic evidence"
            ):
                campaign.render_report(
                    [failure_path], root / "should-not-render.md", require_clean=True
                )
            self.assertFalse((root / "should-not-render.md").exists())

    def test_ordinary_failure_is_cleaned_unless_retention_is_requested(self) -> None:
        for preserve in (False, True):
            with self.subTest(preserve=preserve), tempfile.TemporaryDirectory() as temporary:
                root = Path(temporary)
                args = self.arguments(
                    root,
                    profile="smoke",
                    preserve_failure_evidence=preserve,
                )
                with (
                    patch.object(campaign, "git_state", return_value=(GIT_SHA, False)),
                    patch.object(campaign, "run_logged", side_effect=self.failed_command),
                    patch.object(
                        campaign, "host_environment", return_value=self.host()
                    ),
                    self.assertRaises(campaign.CampaignError),
                ):
                    campaign.run_campaign(args)

                failures = list(root.glob("smoke.failed-*"))
                self.assertEqual(len(failures), 1 if preserve else 0)
                self.assertEqual(list(root.glob(".smoke.partial-*")), [])

    def test_validation_failure_preserves_completed_and_partial_files(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            args = self.arguments(root, profile="scale-10m")
            _manifest, profiles = campaign.load_campaign(campaign.DEFAULT_MANIFEST)
            profile = asdict(profiles["scale-10m"])

            def write_reports(
                command: list[str],
                *,
                root: Path,
                environment: dict[str, str],
                stdout_path: Path,
                stderr_path: Path,
            ) -> int:
                del root, environment
                stdout_path.write_text("stage output\n", encoding="utf-8")
                stderr_path.write_text("", encoding="utf-8")
                stage = command[command.index("--") + 1]
                report_path = Path(command[command.index("--output") + 1])
                if stage == "run":
                    report_path.write_text(
                        json.dumps(report_for("synthetic", profile)), encoding="utf-8"
                    )
                else:
                    report_path.write_text("{}\n", encoding="utf-8")
                return 0

            with (
                patch.object(campaign, "git_state", return_value=(GIT_SHA, False)),
                patch.object(campaign, "run_logged", side_effect=write_reports),
                patch.object(campaign, "host_environment", return_value=self.host()),
                self.assertRaisesRegex(campaign.CampaignError, "validation failed"),
            ):
                campaign.run_campaign(args)

            failure_root = next(root.glob("scale-10m.failed-*"))
            failure = campaign.read_json(failure_root / "failure.json")
            self.assertEqual(failure["failure"]["stage"], "storage")
            self.assertEqual(failure["failure"]["phase"], "validation")
            self.assertEqual(failure["failure"]["return_code"], 0)
            self.assertEqual(
                [entry["stage"] for entry in failure["completed_reports"]],
                ["synthetic"],
            )
            files = {entry["path"] for entry in failure["files"]}
            self.assertEqual(
                files,
                {
                    "storage.json",
                    "storage.stderr.log",
                    "storage.stdout.log",
                    "synthetic.json",
                    "synthetic.stderr.log",
                    "synthetic.stdout.log",
                },
            )

    def test_success_still_publishes_only_a_validated_bundle(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            args = self.arguments(root, profile="smoke")
            _manifest, profiles = campaign.load_campaign(campaign.DEFAULT_MANIFEST)
            profile = asdict(profiles["smoke"])

            def write_reports(
                command: list[str],
                *,
                root: Path,
                environment: dict[str, str],
                stdout_path: Path,
                stderr_path: Path,
            ) -> int:
                del root, environment
                stdout_path.write_text("stage output\n", encoding="utf-8")
                stderr_path.write_text("", encoding="utf-8")
                command_stage = command[command.index("--") + 1]
                report_stage = "synthetic" if command_stage == "run" else "storage"
                report_path = Path(command[command.index("--output") + 1])
                report_path.write_text(
                    json.dumps(report_for(report_stage, profile)), encoding="utf-8"
                )
                return 0

            with (
                patch.object(campaign, "git_state", return_value=(GIT_SHA, False)),
                patch.object(campaign, "run_logged", side_effect=write_reports),
                patch.object(campaign, "host_environment", return_value=self.host()),
            ):
                bundle_path = campaign.run_campaign(args)

            self.assertEqual(bundle_path, args.output.resolve() / "bundle.json")
            self.assertTrue(bundle_path.exists())
            campaign.validate_bundle(bundle_path)
            self.assertEqual(list(root.glob("smoke.failed-*")), [])
            self.assertEqual(list(root.glob(".smoke.partial-*")), [])


if __name__ == "__main__":
    unittest.main()
