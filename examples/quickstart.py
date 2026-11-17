"""Deduplicate a CSV file and search it by similarity.

This is the script behind the getting-started tutorial
(``docs/tutorial.md``). It reads a CSV with an ``id`` column and a text
column, groups near-duplicate rows, writes the rows to keep, and builds a
persistent index that answers "what is most similar to this text?".

Run it on the bundled sample:

    python examples/quickstart.py --write-sample products.csv
    python examples/quickstart.py products.csv --text-column title
"""

from __future__ import annotations

import argparse
import csv
import sys
import tempfile
from pathlib import Path

import pari

SAMPLE_ROWS = [
    ("1", "Apple iPhone 15 Pro 128GB Natural Titanium"),
    ("2", "Apple iPhone 15 Pro 128 GB - Natural Titanium"),
    ("3", "Samsung Galaxy S24 Ultra 256GB Titanium Black"),
    ("4", "Samsung Galaxy S24 Ultra 256 GB, Titanium Black"),
    ("5", "Sony WH-1000XM5 Wireless Noise Cancelling Headphones"),
    ("6", "Sony WH1000XM5 wireless noise-cancelling headphones"),
    ("7", "Logitech MX Master 3S Performance Wireless Mouse"),
    ("8", "Kindle Paperwhite 16GB 6.8 inch display"),
]


def write_sample(path: Path) -> None:
    with path.open("w", newline="", encoding="utf-8") as handle:
        writer = csv.writer(handle)
        writer.writerow(["id", "title"])
        writer.writerows(SAMPLE_ROWS)


def read_rows(path: Path) -> list[dict[str, str]]:
    with path.open(newline="", encoding="utf-8") as handle:
        return list(csv.DictReader(handle))


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    parser.add_argument("csv", type=Path, nargs="?")
    parser.add_argument("--text-column", default="title")
    parser.add_argument("--id-column", default="id")
    parser.add_argument("--threshold", type=float, default=0.5)
    parser.add_argument("--min-similarity", type=float, default=0.6)
    parser.add_argument("--output", type=Path, default=None)
    parser.add_argument("--write-sample", type=Path, default=None)
    args = parser.parse_args(argv)

    if args.write_sample is not None:
        write_sample(args.write_sample)
        print(f"wrote sample CSV to {args.write_sample}")
        return 0
    if args.csv is None:
        parser.error("pass a CSV path, or --write-sample PATH to create one")

    rows = read_rows(args.csv)

    # 1. Turn each row into features: character 3-grams tolerate typos and
    #    formatting differences such as "128GB" versus "128 GB".
    def features(row: dict[str, str]) -> list[bytes]:
        return pari.shingles(row[args.text_column], 3, unit="char")

    # 2. Group near-duplicates. LSH finds candidates above `threshold`, and
    #    `min_similarity` drops weaker candidate pairs before rows are grouped.
    result = pari.deduplicate(
        rows,
        feature=features,
        threshold=args.threshold,
        min_similarity=args.min_similarity,
    )
    print(f"{len(rows)} rows, {result.group_count} duplicate groups")
    for group in result.groups:
        keep = group.representative[args.id_column]
        members = ", ".join(row[args.id_column] for row in group.members)
        print(f"  keep {keep} from [{members}]")

    # 3. Write the rows to keep, in input order.
    output = args.output or args.csv.with_name(f"{args.csv.stem}.deduplicated.csv")
    with output.open("w", newline="", encoding="utf-8") as handle:
        writer = csv.DictWriter(handle, fieldnames=list(rows[0]))
        writer.writeheader()
        writer.writerows(result.kept)
    print(f"wrote {len(result.kept)} rows to {output}")

    # 4. Build a persistent index with stored signatures and search it.
    with tempfile.TemporaryDirectory() as directory:
        index_path = Path(directory) / "products.pari"
        with pari.Index.create(
            index_path, threshold=args.threshold, store_signatures=True
        ) as index:
            index.add_many(
                [
                    (int(row[args.id_column]), pari.MinHash.from_values(features(row)))
                    for row in result.kept
                ]
            )
            query = "iphone 15 pro 128gb titanium"
            sketch = pari.MinHash.from_values(pari.shingles(query, 3, unit="char"))
            print(f"most similar to {query!r}:")
            for key, similarity in index.search_scored(sketch, top_k=3):
                print(f"  id {key}: {similarity:.2f}")
    return 0


if __name__ == "__main__":
    sys.exit(main())
