# Pari

Pari is a fast similarity indexing and deduplication engine for large datasets.

The project is built around a Rust core with Python bindings and a CLI. The goal is to let users move from an in-memory prototype to persistent or shared indexes without rewriting their similarity logic.

## Status

Pari is pre-alpha. Public APIs may still evolve, but the supported 0.1 surface, machine-readable CLI contract, signature semantics, and persisted-format guarantees are defined in [docs/compatibility.md](docs/compatibility.md).

## Python quick start

Install Pari from PyPI:

```bash
python -m pip install pari
```

Build signatures and a persistent index:

```python
from pari import Index, MinHash

first = MinHash.from_values([b"new york", b"rust", b"search"], num_perm=128, seed=7)
second = MinHash.from_values([b"new york", b"python", b"search"], num_perm=128, seed=7)

with Index.create("documents.pari", threshold=0.8, num_perm=128, seed=7) as index:
    index.add_many([(1, first), (2, second)])
    print(index.search(first))
```

`Index.search` returns approximate LSH candidates, not exact duplicate decisions. See [docs/python.md](docs/python.md) for the full typed API and [docs/persistence.md](docs/persistence.md) for durability semantics.

## CLI quick start

Build the command-line binary:

```bash
cargo build --release -p pari-cli
```

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

## Shared Redis indexes

Rust services that need one index shared across processes can use `pari-backend` with its optional Redis feature. The same `BackendIndex32` logic runs against both the in-process memory backend and Redis; Redis details are kept out of `pari-index`.

```rust
use pari_backend::{BackendIndex32, RedisBackend};

let backend = RedisBackend::connect("redis://127.0.0.1:6379/", "documents")?;
let index = BackendIndex32::create(backend, 0.8, 128, 7, None)?;
# Ok::<(), Box<dyn std::error::Error>>(())
```

See [docs/storage-backends.md](docs/storage-backends.md) for the typed backend contract, batching, namespace ownership, TTL semantics, cleanup, security, and benchmark behavior.

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
maturin build --release
```

## License

Pari is MIT licensed. See [LICENSE](LICENSE). Portions derived from or informed by third-party MIT-licensed projects are documented in [NOTICE](NOTICE).
