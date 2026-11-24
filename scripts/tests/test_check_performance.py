from __future__ import annotations

import json
import sys
import tempfile
import unittest
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parents[1]))

import check_performance  # noqa: E402


def write_reports(directory: Path, overrides: dict[str, float] | None = None) -> Path:
    """Write run.json and storage.json with every watched metric at 100."""

    overrides = overrides or {}
    for profile in ("run", "storage"):
        metrics = {}
        for name, owner in check_performance.WATCHED.items():
            if owner != profile:
                continue
            unit = "ms" if name.endswith("_ms") else "items/s"
            direction = "lower" if unit == "ms" else "higher"
            metrics[name] = {
                "value": overrides.get(name, 100.0),
                "unit": unit,
                "direction": direction,
            }
        (directory / f"{profile}.json").write_text(
            json.dumps({"metrics": metrics}), encoding="utf-8"
        )
    return directory


class CheckPerformanceTests(unittest.TestCase):
    def setUp(self) -> None:
        self._temporary = tempfile.TemporaryDirectory()
        self.root = Path(self._temporary.name)
        self._count = 0

    def tearDown(self) -> None:
        self._temporary.cleanup()

    def reports(self, overrides: dict[str, float] | None = None) -> Path:
        self._count += 1
        directory = self.root / f"run-{self._count}"
        directory.mkdir()
        return write_reports(directory, overrides)

    def test_unchanged_and_moderately_noisy_metrics_pass(self) -> None:
        baseline = [self.reports() for _ in range(3)]
        current = [
            self.reports({"index.build_items_per_second": 60.0}),
            self.reports({"storage.persistent.compact_ms": 180.0}),
            self.reports(),
        ]
        self.assertEqual(check_performance.regressions(baseline, current, 0.5), [])

    def test_a_large_throughput_drop_fails(self) -> None:
        # The #263 regression: single-item mutations 5.7x slower.
        mutation = "storage.persistent.mutation_operations_per_second"
        baseline = [self.reports() for _ in range(3)]
        current = [self.reports({mutation: 100.0 / 5.7}) for _ in range(3)]
        found = check_performance.regressions(baseline, current, 0.5)
        self.assertEqual(len(found), 1)
        self.assertIn(mutation, found[0])
        self.assertIn("82% worse", found[0])

    def test_a_latency_that_more_than_doubles_fails(self) -> None:
        baseline = [self.reports() for _ in range(3)]
        current = [
            self.reports({"storage.persistent.reopen_ms": 250.0}) for _ in range(3)
        ]
        found = check_performance.regressions(baseline, current, 0.5)
        self.assertEqual(len(found), 1)
        self.assertIn("storage.persistent.reopen_ms", found[0])

    def test_one_slow_run_does_not_fail_the_median(self) -> None:
        baseline = [self.reports() for _ in range(3)]
        current = [
            self.reports({"signature.items_per_second": 10.0}),
            self.reports(),
            self.reports(),
        ]
        self.assertEqual(check_performance.regressions(baseline, current, 0.5), [])

    def test_tiny_latencies_are_not_compared(self) -> None:
        baseline = [self.reports({"storage.lazy.reopen_ms": 1.0})]
        current = [self.reports({"storage.lazy.reopen_ms": 4.0})]
        self.assertEqual(check_performance.regressions(baseline, current, 0.5), [])

    def test_a_missing_metric_fails(self) -> None:
        baseline = [self.reports()]
        current_directory = self.reports()
        report = json.loads(
            (current_directory / "run.json").read_text(encoding="utf-8")
        )
        del report["metrics"]["signature.items_per_second"]
        (current_directory / "run.json").write_text(
            json.dumps(report), encoding="utf-8"
        )
        found = check_performance.regressions(baseline, [current_directory], 0.5)
        self.assertEqual(len(found), 1)
        self.assertIn("signature.items_per_second", found[0])


if __name__ == "__main__":
    unittest.main()
