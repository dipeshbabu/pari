# Introduction to Pari

<p class="pari-lead">
Pari finds similar records and deduplicates datasets. It builds MinHash
signatures, finds approximate similarity candidates with locality-sensitive
hashing (LSH), and groups duplicate records, all through one Rust engine that
you can use from Python, Rust, or the command line.
</p>

Start in memory, save an index to a versioned `.pari` file, or process bounded
batches from larger datasets through the same native code path.

<div class="pari-cards">
<a class="pari-card" href="python/">
  <strong>Python API</strong>
  <span>Deduplicate records, build signatures, and query persistent indexes.</span>
</a>
<a class="pari-card" href="cli/">
  <strong>Command line</strong>
  <span>Index, search, deduplicate, plan, and verify JSONL datasets.</span>
</a>
<a class="pari-card" href="architecture/">
  <strong>Architecture</strong>
  <span>How signatures, LSH buckets, and storage layers fit together.</span>
</a>
<a class="pari-card" href="benchmarks/">
  <strong>Benchmarks</strong>
  <span>Methodology and recorded evidence for each published result.</span>
</a>
</div>

## Performance

Recorded on a 4-vCPU GitHub-hosted Linux VM with 1,000,000 synthetic items,
compared with Datasketch 2.0 on the same sets, threshold, and permutation count:

| Phase | Pari | Datasketch 2.0 | Difference |
| --- | ---: | ---: | ---: |
| Signatures per second | 322K | 4.6K | about 70× faster |
| Index inserts per second | 269K | 33.5K | about 8× faster |
| Single-query p99 latency | 0.0031 ms | 0.0322 ms | about 10× lower |
| Candidate recall at the threshold | 0.57 | 0.49 | higher |

Throughput depends on hardware and workload, so compare numbers only within the
recorded environment. The [benchmark evidence](benchmark-evidence.md) report
links each figure to its validated bundle, and the
[methodology](benchmarks.md) explains how to reproduce it.

## Getting started

Pari 0.1.3 is an alpha release. It targets Python 3.10+ and Rust 1.81+ for the
public crates; building the Python extension from source requires Rust 1.83.
The project is pre-1.0, so read the [compatibility contract](compatibility.md)
before depending on persisted formats.

=== "Python"

    ```sh
    python -m pip install "pari-similarity==0.1.3"
    ```

    The distribution name is `pari-similarity`; the import name is `pari`.

=== "Rust"

    ```sh
    cargo add pari-core@0.1.3 pari-index@0.1.3 pari-store@0.1.3
    ```

    Use only the layers your application needs, and keep every public crate on
    the same release version.

=== "CLI"

    Download an archive from the
    [0.1.3 release](https://github.com/dipeshbabu/pari/releases/tag/v0.1.3),
    verify it against `SHA256SUMS`, extract it, and put `pari` on your `PATH`.
    See [platform support](platforms.md) for the published targets.

## Compare records

Build two signatures and estimate their Jaccard similarity. Use the same
permutation count and seed for every signature you compare.

```python
from pari import MinHash

first = MinHash.from_values(
    [b"new york", b"rust", b"search"], num_perm=128, seed=7
)
second = MinHash.from_values(
    [b"new york", b"python", b"search"], num_perm=128, seed=7
)

print(first.jaccard(second))
```

## Deduplicate a dataset

`deduplicate` streams records through bounded native batches and returns
deterministic groups plus input-order keep and drop partitions.

```python
from pari import deduplicate

records = ["the quick brown fox", "the quick brown fox", "lorem ipsum"]
result = deduplicate(
    records,
    feature=lambda text: [word.encode() for word in text.split()],
    threshold=0.8,
)

print(result.kept)      # ('the quick brown fox', 'lorem ipsum')
print(result.dropped)   # ('the quick brown fox',)
```

The [Python guide](python.md) covers exact verification, representative
selection, progress callbacks, and the optional local backend.

## Index and search from the terminal

Create `documents.jsonl` with one record per line:

```json
{"key":1,"values":["new york","rust","search"]}
{"key":2,"values":["new york","python","search"]}
```

Build a persistent index, verify it, and group duplicate candidates:

```sh
pari index --input documents.jsonl --output documents.pari --json
pari verify --index documents.pari --json
pari dedup --input documents.jsonl --emit groups --json
```

!!! note "Candidates are approximate"
    LSH returns approximate candidates. Verify candidates against source
    features when your application needs an exact similarity threshold.

## Where to go next

- [Text deduplication and corpus audits](text-workloads.md),
  [code corpora](code-workloads.md), and [entity matching](entity-matching.md)
  show complete workloads.
- [LSH planning](planning.md) explains how bands and rows are chosen and how
  much memory an index needs.
- [Persistent indexes](persistence.md) and the [index format](index-format.md)
  describe crash-safe storage.
- [Datasketch 2.x interoperability](datasketch-v2.md) covers exact migration of
  existing sketches.
