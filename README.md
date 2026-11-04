<h1 align="center">Pari</h1>

<p align="center">
  Find similar records and deduplicate datasets with Rust, Python, or your terminal.
</p>

<p align="center">
  <a href="https://pypi.org/project/pari-similarity/"><img alt="PyPI" src="https://img.shields.io/pypi/v/pari-similarity?style=flat-square&color=3ECF8E"></a>
  <a href="https://crates.io/crates/pari-core"><img alt="crates.io" src="https://img.shields.io/crates/v/pari-core?style=flat-square&color=3ECF8E"></a>
  <a href="https://github.com/dipeshbabu/pari/actions/workflows/ci.yml"><img alt="Build" src="https://img.shields.io/github/actions/workflow/status/dipeshbabu/pari/ci.yml?branch=main&label=build&style=flat-square&color=3ECF8E"></a>
  <a href="LICENSE"><img alt="MIT license" src="https://img.shields.io/badge/license-MIT-3ECF8E?style=flat-square"></a>
</p>

---

Pari builds MinHash signatures, finds approximate similarity candidates with LSH,
and groups duplicate records. Start in memory, save an index to a versioned `.pari`
file, or process bounded batches from larger datasets through the same Rust engine.

**Pari 0.1.2 alpha** targets Python 3.10+ and Rust 1.81+ for the public crates.
The project is pre-1.0; see the [compatibility policy](docs/compatibility.md).
The Python extension requires Rust 1.83 when built from source.

## Installation

### Python

```sh
python -m pip install "pari-similarity==0.1.2"
```

The distribution name is `pari-similarity`; the import name is `pari`.

### Rust

```sh
cargo add pari-core@0.1.2 pari-index@0.1.2 pari-store@0.1.2
```

Use only the layers your application needs. Add `pari-format = "0.1.2"` when
working directly with codecs or persisted-format metadata. Keep public crates on
the same release version.

### CLI

Download an archive from the [0.1.2 release](https://github.com/dipeshbabu/pari/releases/tag/v0.1.2),
verify it against `SHA256SUMS`, extract it, and put `pari` on your `PATH`.

| Platform | Archive |
| --- | --- |
| Linux x86_64 | [`pari-0.1.2-linux.tar.gz`](https://github.com/dipeshbabu/pari/releases/download/v0.1.2/pari-0.1.2-linux.tar.gz) |
| Linux arm64 | [`pari-0.1.2-linux-arm64.tar.gz`](https://github.com/dipeshbabu/pari/releases/download/v0.1.2/pari-0.1.2-linux-arm64.tar.gz) |
| macOS arm64 | [`pari-0.1.2-macos.tar.gz`](https://github.com/dipeshbabu/pari/releases/download/v0.1.2/pari-0.1.2-macos.tar.gz) |
| Windows x86_64 | [`pari-0.1.2-windows.zip`](https://github.com/dipeshbabu/pari/releases/download/v0.1.2/pari-0.1.2-windows.zip) |

Packages and archives become available when the tagged release workflow completes.
See [release notes](docs/releases/0.1.2.md) and [platform support](docs/platforms.md).

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
The [Python guide](docs/python.md) covers persistent indexes, batch operations,
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

- [Python deduplication](docs/python.md)
- [Dataset integrations](docs/dataset-integrations.md)
- [Text deduplication and corpus audits](docs/text-workloads.md)
- [Code corpus deduplication](docs/code-workloads.md)
- [Entity matching](docs/entity-matching.md)

## Reference

Use `--help` to explore commands and options:

```sh
pari --help
pari index --help
pari dedup --help
```

- [CLI reference](docs/cli.md) and [Python API](docs/python.md)
- [Architecture](docs/architecture.md) and [storage backends](docs/storage-backends.md)
- [Datasketch 2.x interoperability](docs/datasketch-v2.md)
- [LSH planning](docs/planning.md), [observability](docs/observability.md), and [CPU parallelism](docs/parallelism.md)
- [Benchmark methodology](docs/benchmarks.md) and [recorded benchmark evidence](docs/benchmark-evidence.md)
- [Compatibility](docs/compatibility.md), [changelog](CHANGELOG.md), and [release procedure](docs/releasing.md)

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

See [CONTRIBUTING.md](CONTRIBUTING.md) for setup, checks, and compatibility rules.
Report bugs and propose changes through [GitHub issues](https://github.com/dipeshbabu/pari/issues).
For security reports, follow [SECURITY.md](SECURITY.md).

## License

Pari is released under the [MIT license](LICENSE). Third-party attribution is
recorded in [NOTICE](NOTICE).
