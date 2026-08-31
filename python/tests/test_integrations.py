from __future__ import annotations

import importlib.util
import tempfile
import unittest
from pathlib import Path
from unittest import mock

from pari import DedupeIndex, integrations

HAS_PYARROW = importlib.util.find_spec("pyarrow") is not None
HAS_POLARS = importlib.util.find_spec("polars") is not None
HAS_DATASETS = importlib.util.find_spec("datasets") is not None


class OptionalDependencyTests(unittest.TestCase):
    def test_module_imports_without_optional_libraries(self) -> None:
        self.assertIn("iter_pyarrow_rows", integrations.__all__)

    def test_missing_dependency_error_is_actionable(self) -> None:
        error = ModuleNotFoundError("No module named 'pyarrow'")
        error.name = "pyarrow"
        with (
            mock.patch.object(
                integrations.importlib, "import_module", side_effect=error
            ),
            self.assertRaisesRegex(
                integrations.IntegrationDependencyError,
                r"pari-similarity\[pyarrow\]",
            ),
        ):
            list(integrations.iter_pyarrow_rows(object()))

    def test_batch_size_must_be_positive_before_loading_dependency(self) -> None:
        with self.assertRaisesRegex(ValueError, "batch_size must be positive"):
            list(integrations.iter_pyarrow_rows(object(), batch_size=0))


@unittest.skipUnless(HAS_PYARROW, "PyArrow is not installed")
class PyArrowIntegrationTests(unittest.TestCase):
    def test_table_parquet_projection_nulls_and_direct_ingestion(self) -> None:
        import pyarrow as pa
        import pyarrow.parquet as pq

        table = pa.table(
            {
                "id": ["a", "b", "c", "d", "e"],
                "text": ["same value", "same value", None, "other", "last"],
                "unused": [1, 2, 3, 4, 5],
            }
        )
        rows = list(
            integrations.iter_pyarrow_rows(table, columns=["id", "text"], batch_size=2)
        )
        self.assertEqual(len(rows), 5)
        self.assertEqual(set(rows[0]), {"id", "text"})
        self.assertIsNone(rows[2]["text"])

        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / "records.parquet"
            pq.write_table(table, path, row_group_size=2)
            parquet_rows = list(
                integrations.iter_pyarrow_rows(
                    path, columns=["id", "text"], batch_size=2
                )
            )
            self.assertEqual(parquet_rows, rows)

        index = DedupeIndex[str](None, num_perm=32, batch_size=2)
        try:
            added = integrations.add_pyarrow(
                index,
                table,
                columns=["id", "text"],
                batch_size=2,
                record=lambda row: row["id"],
                features=lambda row: [
                    token.encode() for token in (row["text"] or "missing").split()
                ],
            )
            self.assertEqual(added, 5)
            self.assertIn(("a", "b"), index.candidate_pairs())
        finally:
            index.close()


@unittest.skipUnless(HAS_POLARS, "Polars is not installed")
class PolarsIntegrationTests(unittest.TestCase):
    def test_dataframe_lazyframe_projection_and_nulls(self) -> None:
        import polars as pl

        frame = pl.DataFrame(
            {
                "id": ["a", "b", "c", "d", "e"],
                "text": ["one", "two", None, "four", "five"],
                "unused": [1, 2, 3, 4, 5],
            }
        )
        expected = list(
            integrations.iter_polars_rows(frame, columns=["id", "text"], batch_size=2)
        )
        self.assertEqual(len(expected), 5)
        self.assertIsNone(expected[2]["text"])
        actual = list(
            integrations.iter_polars_rows(
                frame.lazy(), columns=["id", "text"], batch_size=2
            )
        )
        self.assertEqual(actual, expected)


@unittest.skipUnless(HAS_DATASETS, "Hugging Face Datasets is not installed")
class HuggingFaceIntegrationTests(unittest.TestCase):
    def test_dataset_and_iterable_dataset_batches_preserve_order_and_nulls(
        self,
    ) -> None:
        from datasets import Dataset, IterableDataset

        values = {
            "id": ["a", "b", "c", "d", "e"],
            "text": ["one", "two", None, "four", "five"],
            "unused": [1, 2, 3, 4, 5],
        }
        dataset = Dataset.from_dict(values)
        expected = list(
            integrations.iter_huggingface_rows(
                dataset, columns=["id", "text"], batch_size=2
            )
        )
        self.assertEqual(len(expected), 5)
        self.assertIsNone(expected[2]["text"])

        iterable = IterableDataset.from_generator(
            lambda: (
                {name: column[position] for name, column in values.items()}
                for position in range(5)
            )
        )
        actual = list(
            integrations.iter_huggingface_rows(
                iterable, columns=["id", "text"], batch_size=2
            )
        )
        self.assertEqual(actual, expected)


if __name__ == "__main__":
    unittest.main()
