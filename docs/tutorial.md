# Tutorial: deduplicate a CSV

This tutorial finds near-duplicate rows in a product list, keeps one row from
each group, and then searches the kept rows by similarity. It takes about five
minutes and needs only Python 3.10 or newer.

The complete script is
[`examples/quickstart.py`](https://github.com/dipeshbabu/pari/blob/main/examples/quickstart.py);
each step below explains one part of it.

## 1. Install Pari

```sh
python -m pip install pari-similarity
```

## 2. Get some data

Any CSV with an ID column and a text column works. To follow along, write the
bundled sample, eight product titles with three near-duplicate pairs:

```sh
python examples/quickstart.py --write-sample products.csv
```

```text
id,title
1,Apple iPhone 15 Pro 128GB Natural Titanium
2,Apple iPhone 15 Pro 128 GB - Natural Titanium
3,Samsung Galaxy S24 Ultra 256GB Titanium Black
4,"Samsung Galaxy S24 Ultra 256 GB, Titanium Black"
5,Sony WH-1000XM5 Wireless Noise Cancelling Headphones
6,Sony WH1000XM5 wireless noise-cancelling headphones
7,Logitech MX Master 3S Performance Wireless Mouse
8,Kindle Paperwhite 16GB 6.8 inch display
```

## 3. Turn rows into features

Pari compares rows by their sets of features. For short, messy titles,
character 3-grams work well: "128GB" and "128 GB" still share most of their
3-grams, and `shingles` lowercases and normalizes the text first.

```python
import pari

def features(row):
    return pari.shingles(row["title"], 3, unit="char")
```

For longer text such as articles, word shingles (`pari.shingles(text, 3)`) are
a better fit.

## 4. Group near-duplicates

```python
result = pari.deduplicate(
    rows,
    feature=features,
    threshold=0.5,
    min_similarity=0.6,
)
for group in result.groups:
    print(group.representative["id"], [row["id"] for row in group.members])
```

`threshold` tunes the LSH index that finds candidate pairs quickly.
`min_similarity` then rejects candidate pairs whose estimated similarity is
below 0.6, so weak matches never join a group. Each group keeps its first row
as the representative; pass `representative=` to choose differently.

Running the script prints:

```text
8 rows, 3 duplicate groups
  keep 1 from [1, 2]
  keep 3 from [3, 4]
  keep 5 from [5, 6]
wrote 5 rows to products.deduplicated.csv
```

`result.kept` holds the rows to keep in their original order, and
`result.dropped` holds the duplicates removed.

## 5. Search by similarity

To answer "which kept products look like this text?", store the kept rows in a
persistent index that keeps signatures, then ask for scored results:

```python
with pari.Index.create("products.pari", threshold=0.5, store_signatures=True) as index:
    index.add_many(
        [(int(row["id"]), pari.MinHash.from_values(features(row))) for row in result.kept]
    )
    query = pari.MinHash.from_values(
        pari.shingles("iphone 15 pro 128gb titanium", 3, unit="char")
    )
    for key, similarity in index.search_scored(query, top_k=3):
        print(key, round(similarity, 2))
```

```text
most similar to 'iphone 15 pro 128gb titanium':
  id 1: 0.56
```

Scores are estimated Jaccard similarities between feature sets, from 0 (nothing
shared) to 1 (identical). The index file persists, so `pari.Index.open` can
search it again later without rebuilding.

## Next steps

- Run the whole script on your own file:
  `python examples/quickstart.py data.csv --text-column name --id-column sku`.
- Read large Parquet, Arrow, Polars, or Hugging Face data in bounded batches
  with the [dataset integrations](dataset-integrations.md).
- Learn how `threshold` trades recall against speed in
  [LSH planning](planning.md).
- See the full [Python API](python.md) and the [command line](cli.md), which
  deduplicates JSONL files without writing code.
