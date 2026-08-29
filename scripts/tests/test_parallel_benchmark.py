from __future__ import annotations

import importlib.util
import sys
import unittest
from pathlib import Path

SCRIPT = Path(__file__).resolve().parents[1] / "parallel_benchmark.py"
SPEC = importlib.util.spec_from_file_location("pari_parallel_benchmark", SCRIPT)
if SPEC is None or SPEC.loader is None:
    raise RuntimeError("could not load parallel benchmark utility")
benchmark = importlib.util.module_from_spec(SPEC)
sys.modules[SPEC.name] = benchmark
SPEC.loader.exec_module(benchmark)


def metric(value: float) -> dict[str, object]:
    return {"direction": "neutral", "unit": "test", "value": value}


def report(*, elapsed: float, threads: int, items: int = 256) -> dict[str, object]:
    return {
        "config": {"items": items, "queries": 1},
        "metrics": {
            "signature.elapsed_ms": metric(elapsed),
            "signature.items_per_second": metric(items / elapsed * 1_000.0),
            "signature.threads": metric(float(threads)),
            "signature.parallel": metric(float(threads > 1)),
            "memory.signature_peak_rss_bytes": metric(1_024.0),
            "index.build_elapsed_ms": metric(2.0),
            "query.scalar_elapsed_ms": metric(0.1),
            "query.batch_elapsed_ms": metric(0.1),
            "query_signature.items_per_second": metric(10_000.0),
            "index.mutation_operations_per_second": metric(1_000.0),
            "grouping.index_elapsed_ms": metric(1.0),
            "grouping.stream_elapsed_ms": metric(1.0),
        },
    }


class ParallelBenchmarkTests(unittest.TestCase):
    def test_positive_csv_rejects_duplicates_and_nonpositive_values(self) -> None:
        self.assertEqual(benchmark.positive_csv("1, 4,8"), (1, 4, 8))
        for value in ("", "1,1", "0,1", "-1,2", "one"):
            with self.assertRaises(Exception):
                benchmark.positive_csv(value)

    def test_summary_uses_medians_and_preserves_effective_threads(self) -> None:
        summary = benchmark.summarize_group(
            256,
            8,
            [
                report(elapsed=4.0, threads=8),
                report(elapsed=2.0, threads=8),
                report(elapsed=3.0, threads=8),
            ],
        )

        self.assertEqual(summary["effective_threads"], 8)
        self.assertTrue(summary["parallel"])
        self.assertEqual(summary["signature_elapsed_ms"]["median"], 3.0)

    def test_speedup_uses_one_thread_result_as_baseline(self) -> None:
        scalar = benchmark.summarize_group(
            256, 1, [report(elapsed=6.0, threads=1)]
        )
        parallel = benchmark.summarize_group(
            256, 4, [report(elapsed=2.0, threads=4)]
        )

        speedup = benchmark.speedups([scalar, parallel])[0]
        self.assertEqual(speedup["signature_speedup"], 3.0)
        self.assertGreater(speedup["product_phase_speedup"], 1.0)


if __name__ == "__main__":
    unittest.main()
