from __future__ import annotations

import contextlib
import csv
import importlib.util
import io
import sys
import tempfile
import unittest
from pathlib import Path

ROOT = Path(__file__).resolve().parents[2]
SCRIPT = ROOT / "examples" / "quickstart.py"
SPEC = importlib.util.spec_from_file_location("pari_quickstart", SCRIPT)
if SPEC is None or SPEC.loader is None:
    raise RuntimeError("could not load the quickstart example")
quickstart = importlib.util.module_from_spec(SPEC)
sys.modules[SPEC.name] = quickstart
SPEC.loader.exec_module(quickstart)


class QuickstartTests(unittest.TestCase):
    """The tutorial's script must run and print what the tutorial shows."""

    def test_sample_deduplicates_and_searches(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            sample = Path(directory) / "products.csv"
            self.assertEqual(quickstart.main(["--write-sample", str(sample)]), 0)
            output = io.StringIO()
            with contextlib.redirect_stdout(output):
                self.assertEqual(quickstart.main([str(sample)]), 0)
            printed = output.getvalue()
            self.assertIn("8 rows, 3 duplicate groups", printed)
            for line in (
                "keep 1 from [1, 2]",
                "keep 3 from [3, 4]",
                "keep 5 from [5, 6]",
            ):
                self.assertIn(line, printed)
            self.assertIn("id 1: 0.56", printed)

            with (Path(directory) / "products.deduplicated.csv").open(
                newline="", encoding="utf-8"
            ) as handle:
                kept = [row["id"] for row in csv.DictReader(handle)]
            self.assertEqual(kept, ["1", "3", "5", "7", "8"])

    def test_missing_csv_is_a_usage_error(self) -> None:
        with (
            contextlib.redirect_stderr(io.StringIO()),
            self.assertRaises(SystemExit) as raised,
        ):
            quickstart.main([])
        self.assertEqual(raised.exception.code, 2)


if __name__ == "__main__":
    unittest.main()
