# Pari

Pari is a fast similarity indexing and deduplication engine for large datasets.

The project is built around a Rust core with Python bindings and a CLI. The goal is to let users move from an in-memory prototype to persistent or shared indexes without rewriting their similarity logic.

## Status

Pari **0.2.0 alpha** is publicly available from PyPI, crates.io, and [GitHub Releases](https://github.com/dipeshbabu/pari/releases/tag/v0.2.0). Pari is still pre-1.0: supported 0.2.x interfaces follow the [v0.x compatibility policy](docs/compatibility.md), while experimental surfaces may change in a future minor release with release notes and migration guidance.

Choose the interface that fits your workload:

| Interface | Install or download | Start here |
| --- | --- | --- |
| Python 3.10+ | `python -m pip install "pari-similarity==0.2.0"` | [Python API guide](docs/python.md) |
| Rust 1.81+ | `cargo add pari-core@0.2.0 pari-index@0.2.0` | [Architecture and crate layers](docs/architecture.md) |
| CLI | [Download a 0.2.0 archive](https://github.com/dipeshbabu/pari/releases/tag/v0.2.0) | [CLI guide](docs/cli.md) |

The [project links](#project-links) collect release notes, compatibility and security policies, integrations, workload guides, and benchmark evidence.

## Python quick start

Install the published distribution (CPython 3.10 or newer):

```bash
python -m pip install "pari-similarity==0.2.0"
```

The distribution name is `pari-similarity`; the import name is `pari`:

```python
from pari import MinHash

first = MinHash.from_values(
    [b"new york", b"rust", b"search"],
    num_perm=128,
    seed=7,
)
second = MinHash.from_values(
    [b"new york", b"python", b"search"],
    num_perm=128,
    seed=7,
)

print(first.jaccard(second))
```

See the [Python API guide](docs/python.md) for persistent indexes, batch operations, and the 0.2 API surface.

## Rust quick start

The four public crates are versioned and released together at 0.2.0:

| Crate | Use it for |
| --- | --- |
| [`pari-core`](https://crates.io/crates/pari-core/0.2.0) | MinHash signatures and similarity primitives |
| [`pari-format`](https://crates.io/crates/pari-format/0.2.0) | Safe codecs and the versioned index format |
| [`pari-index`](https://crates.io/crates/pari-index/0.2.0) | In-memory LSH indexing and duplicate grouping |
| [`pari-store`](https://crates.io/crates/pari-store/0.2.0) | Crash-safe local persistent indexes |

Add only the layers your application uses:

```bash
cargo add pari-core@0.2.0 pari-index@0.2.0 pari-store@0.2.0
```

Or declare them directly:

```toml
[dependencies]
pari-core = "0.2.0"
pari-index = "0.2.0"
pari-store = "0.2.0"
```

Add `pari-format = "0.2.0"` only when working directly with codecs or format metadata.

## CLI quick start

Download the published archive for your platform:

| Platform | Archive |
| --- | --- |
| Linux x86_64 | [`pari-0.2.0-linux.tar.gz`](https://github.com/dipeshbabu/pari/releases/download/v0.2.0/pari-0.2.0-linux.tar.gz) |
| macOS arm64 | [`pari-0.2.0-macos.tar.gz`](https://github.com/dipeshbabu/pari/releases/download/v0.2.0/pari-0.2.0-macos.tar.gz) |
| Windows x86_64 | [`pari-0.2.0-windows.zip`](https://github.com/dipeshbabu/pari/releases/download/v0.2.0/pari-0.2.0-windows.zip) |

Linux:

```bash
curl -LO https://github.com/dipeshbabu/pari/releases/download/v0.2.0/pari-0.2.0-linux.tar.gz
tar -xzf pari-0.2.0-linux.tar.gz
./pari-0.2.0-linux/pari --version
```

macOS:

```bash
curl -LO https://github.com/dipeshbabu/pari/releases/download/v0.2.0/pari-0.2.0-macos.tar.gz
tar -xzf pari-0.2.0-macos.tar.gz
./pari-0.2.0-macos/pari --version
```

Windows PowerShell:

```powershell
Invoke-WebRequest -Uri "https://github.com/dipeshbabu/pari/releases/download/v0.2.0/pari-0.2.0-windows.zip" -OutFile "pari-0.2.0-windows.zip"
Expand-Archive -Path "pari-0.2.0-windows.zip" -DestinationPath .
.\pari-0.2.0-windows\pari.exe --version
```

Verify downloads with the published [`SHA256SUMS`](https://github.com/dipeshbabu/pari/releases/download/v0.2.0/SHA256SUMS) before installing the binary on `PATH`.

Given JSONL records such as:

```json
{"key":1,"values":["new york","rust","search"]}
{"key":2,"values":["new york","python","search"]}
```

build and verify a persistent index:

```bash
pari index --input documents.jsonl --output documents.pari --json
pari verify --index documents.pari --json
```

Run native duplicate grouping without sending candidate edges through Python:

```bash
pari dedup --input documents.jsonl --emit groups --json
```

See [docs/cli.md](docs/cli.md) for indexing, search, deduplication, inspection, JSONL output, verification, and shell completion.

Source users evaluating shared memory or Redis backends can follow the [storage backend guide](docs/storage-backends.md); those experimental crates are not part of the published 0.2.0 Rust set.

## Project links

- [PyPI: `pari-similarity`](https://pypi.org/project/pari-similarity/0.2.0/)
- [GitHub Release and binary downloads](https://github.com/dipeshbabu/pari/releases/tag/v0.2.0)
- [0.2.0 release notes](docs/releases/0.2.0.md)
- [Compatibility policy](docs/compatibility.md)
- [Security policy](SECURITY.md) and [dependency maintenance](docs/dependency-policy.md)
- [Python guide](docs/python.md), [CLI guide](docs/cli.md), [dataset integrations](docs/dataset-integrations.md), [text workloads](docs/text-workloads.md), [code corpus deduplication](docs/code-workloads.md), and [entity matching](docs/entity-matching.md)
- [Datasketch 2.x interoperability and migration](docs/datasketch-v2.md)
- [Weighted MinHash and SimHash evaluation](docs/similarity-family-evaluation.md)
- [Shardable and mergeable index evaluation](docs/sharding-evaluation.md)
- [Benchmark methodology](docs/benchmarks.md), [native-Linux and historical evidence](docs/benchmark-evidence.md), [CPU parallelism](docs/parallelism.md), [LSH planning](docs/planning.md), [observability](docs/observability.md), and [workload roadmap](https://github.com/dipeshbabu/pari/issues)

## Design goals

- Batch-first compute, insert, query, and remote storage paths.
- Safe, versioned persistence instead of executable object serialization.
- A persistent local index between RAM-only prototypes and remote databases.
- Shared backends without coupling LSH code to database-specific commands.
- A small Rust core shared by Python and CLI frontends.
- Correctness tests and benchmark evidence for performance-sensitive changes.
- No merge to `main` while required CI is failing.

## Layers

- `pari-core`: signatures and similarity primitives.
- `pari-index`: LSH and candidate generation.
- `pari-store`: crash-safe local persistent indexes.
- `pari-backend`: typed in-process and shared remote storage backends, including Redis.
- `pari-py`: PyO3 Python bindings and the `pari` package.
- `pari-cli`: command-line workflows.

See [docs/architecture.md](docs/architecture.md), [docs/compatibility.md](docs/compatibility.md), and the GitHub issues for the implementation roadmap.

## Development

```bash
cargo fmt --all -- --check
cargo clippy --workspace --all-targets --all-features -- -D warnings
cargo test --workspace --all-targets --all-features
```

Python wheel development additionally uses maturin:

```bash
python -m pip install "maturin>=1.14,<2"
python -m pip install .
maturin build --release
```

Build the CLI from source with `cargo build --release -p pari-cli`.

## License

Pari is MIT licensed. See [LICENSE](LICENSE). Portions derived from or informed by third-party MIT-licensed projects are documented in [NOTICE](NOTICE).
