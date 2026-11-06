<h1 align="center">Pari</h1>

<p align="center">
  Find similar records and deduplicate datasets with Rust, Python, or your terminal.
</p>

<p align="center">
  <a href="https://pypi.org/project/pari-similarity/0.1.3/"><img alt="PyPI" src="https://img.shields.io/badge/PyPI-v0.1.3-3ECF8E?style=flat-square"></a>
  <a href="https://pepy.tech/projects/pari-similarity"><img alt="PyPI downloads per month" src="https://static.pepy.tech/badge/pari-similarity/month"></a>
  <a href="https://crates.io/crates/pari-core/0.1.3"><img alt="crates.io" src="https://img.shields.io/badge/crates.io-v0.1.3-3ECF8E?style=flat-square"></a>
  <a href="https://github.com/dipeshbabu/pari/actions/workflows/ci.yml"><img alt="Build" src="https://img.shields.io/github/actions/workflow/status/dipeshbabu/pari/ci.yml?branch=main&label=build&style=flat-square&color=3ECF8E"></a>
  <a href="https://github.com/dipeshbabu/pari/blob/v0.1.3/LICENSE"><img alt="MIT license" src="https://img.shields.io/badge/license-MIT-3ECF8E?style=flat-square"></a>
</p>

---

Pari builds MinHash signatures, finds approximate similarity candidates with LSH,
and groups duplicate records. Start in memory, save an index to a versioned `.pari`
file, or process bounded batches from larger datasets through the same Rust engine.

**Pari 0.1.3 alpha** targets Python 3.10+ and Rust 1.81+ for the public crates.
The project is pre-1.0; see the [compatibility policy](https://github.com/dipeshbabu/pari/blob/v0.1.3/docs/compatibility.md).
The Python extension requires Rust 1.83 when built from source.

## Installation

### Python

```sh
python -m pip install "pari-similarity==0.1.3"
```

The distribution name is `pari-similarity`; the import name is `pari`.

### Rust

```sh
cargo add pari-core@0.1.3 pari-index@0.1.3 pari-store@0.1.3
```

Use only the layers your application needs. Add `pari-format = "0.1.3"` when
working directly with codecs or persisted-format metadata. Keep public crates on
the same release version.

### CLI

Download an archive from the [0.1.3 release](https://github.com/dipeshbabu/pari/releases/tag/v0.1.3),
verify it against `SHA256SUMS`, extract it, and put `pari` on your `PATH`.

| Platform | Archive |
| --- | --- |
| Linux x86_64 | [`pari-0.1.3-linux.tar.gz`](https://github.com/dipeshbabu/pari/releases/download/v0.1.3/pari-0.1.3-linux.tar.gz) |
| Linux arm64 | [`pari-0.1.3-linux-arm64.tar.gz`](https://github.com/dipeshbabu/pari/releases/download/v0.1.3/pari-0.1.3-linux-arm64.tar.gz) |
| macOS arm64 | [`pari-0.1.3-macos.tar.gz`](https://github.com/dipeshbabu/pari/releases/download/v0.1.3/pari-0.1.3-macos.tar.gz) |
| Windows x86_64 | [`pari-0.1.3-windows.zip`](https://github.com/dipeshbabu/pari/releases/download/v0.1.3/pari-0.1.3-windows.zip) |

Packages and archives become available when the tagged release workflow completes.
See [release notes](https://github.com/dipeshbabu/pari/blob/v0.1.3/docs/releases/0.1.3.md) and [platform support](https://github.com/dipeshbabu/pari/blob/v0.1.3/docs/platforms.md).

## Compare Records

Build two signatures and estimate their Jaccard similarity:

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

Use the same permutation count and seed for signatures you compare.
The [Python guide](https://github.com/dipeshbabu/pari/blob/v0.1.3/docs/python.md) covers persistent indexes, batch operations,
and the explicit `MinHash64` and `Index64` APIs.

## Index and Search

Create `documents.jsonl` with one record per line:

```json
{"key":1,"values":["new york","rust","search"]}
{"key":2,"values":["new york","python","search"]}
```

Build a persistent index and verify its contents:

```sh
pari index --input documents.jsonl --output documents.pari --json
pari verify --index documents.pari --json
pari search --help
```

Indexes use safe, versioned persistence. LSH returns approximate candidates;
verify candidates against source features when your application needs an exact
similarity threshold.

## Deduplicate a Dataset

Group duplicate candidates through the native engine:

```sh
pari dedup --input documents.jsonl --emit groups --json
```

Python's `DedupeIndex` and `deduplicate` add bounded batches, exact verification,
representative selection, and progress callbacks. Optional adapters read
PyArrow/Parquet, Polars, and Hugging Face datasets.

- [Python deduplication](https://github.com/dipeshbabu/pari/blob/v0.1.3/docs/python.md)
- [Dataset integrations](https://github.com/dipeshbabu/pari/blob/v0.1.3/docs/dataset-integrations.md)
- [Text deduplication and corpus audits](https://github.com/dipeshbabu/pari/blob/v0.1.3/docs/text-workloads.md)
- [Code corpus deduplication](https://github.com/dipeshbabu/pari/blob/v0.1.3/docs/code-workloads.md)
- [Entity matching](https://github.com/dipeshbabu/pari/blob/v0.1.3/docs/entity-matching.md)

## Reference

Use `--help` to explore commands and options:

```sh
pari --help
pari index --help
pari dedup --help
```

- [CLI reference](https://github.com/dipeshbabu/pari/blob/v0.1.3/docs/cli.md) and [Python API](https://github.com/dipeshbabu/pari/blob/v0.1.3/docs/python.md)
- [Architecture](https://github.com/dipeshbabu/pari/blob/v0.1.3/docs/architecture.md) and [storage backends](https://github.com/dipeshbabu/pari/blob/v0.1.3/docs/storage-backends.md)
- [Datasketch 2.x interoperability](https://github.com/dipeshbabu/pari/blob/v0.1.3/docs/datasketch-v2.md)
- [LSH planning](https://github.com/dipeshbabu/pari/blob/v0.1.3/docs/planning.md), [observability](https://github.com/dipeshbabu/pari/blob/v0.1.3/docs/observability.md), and [CPU parallelism](https://github.com/dipeshbabu/pari/blob/v0.1.3/docs/parallelism.md)
- [Benchmark methodology](https://github.com/dipeshbabu/pari/blob/v0.1.3/docs/benchmarks.md) and [recorded benchmark evidence](https://github.com/dipeshbabu/pari/blob/v0.1.3/docs/benchmark-evidence.md)
- [Compatibility](https://github.com/dipeshbabu/pari/blob/v0.1.3/docs/compatibility.md), [changelog](https://github.com/dipeshbabu/pari/blob/v0.1.3/CHANGELOG.md), and [release procedure](https://github.com/dipeshbabu/pari/blob/v0.1.3/docs/releasing.md)

The committed benchmark scale envelope is 1M records. Performance figures apply
to the environment and workload recorded with each result.

## Developing

```sh
git clone https://github.com/dipeshbabu/pari.git
cd pari

python -m pip install "pre-commit==4.6.2"
pre-commit install
pre-commit run --all-files

cargo fmt --all -- --check
cargo clippy --workspace --all-targets --all-features -- -D warnings
cargo test --workspace --all-targets --all-features
```

CI remains authoritative. Build the CLI with `cargo build --release -p pari-cli`.
For Python development, install the checkout with `python -m pip install .`.

| Path | Purpose |
| --- | --- |
| `crates/pari-core` | MinHash signatures and similarity primitives |
| `crates/pari-index` | LSH candidates and duplicate grouping |
| `crates/pari-format` | Codecs and versioned file format |
| `crates/pari-store` | Persistent local indexes |
| `crates/pari-cli` | Command-line workflows |
| `crates/pari-py`, `python/pari` | Python bindings and APIs |

## Contributing

See [CONTRIBUTING.md](https://github.com/dipeshbabu/pari/blob/v0.1.3/CONTRIBUTING.md) for setup, checks, and compatibility rules.
Report bugs and propose changes through [GitHub issues](https://github.com/dipeshbabu/pari/issues).
For security reports, follow [SECURITY.md](https://github.com/dipeshbabu/pari/blob/v0.1.3/SECURITY.md).

## License

Pari is released under the [MIT license](https://github.com/dipeshbabu/pari/blob/v0.1.3/LICENSE). Third-party attribution is
recorded in [NOTICE](https://github.com/dipeshbabu/pari/blob/v0.1.3/NOTICE).
