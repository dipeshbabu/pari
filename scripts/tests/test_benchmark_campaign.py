from __future__ import annotations

import importlib.util
import json
import sys
import tempfile
import unittest
from dataclasses import asdict
from pathlib import Path

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


if __name__ == "__main__":
    unittest.main()
