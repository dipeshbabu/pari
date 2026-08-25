# Pari

Pari is a fast similarity indexing and deduplication engine for large datasets.

The project is built around a Rust core with Python bindings and a CLI. The goal is to let users move from an in-memory prototype to persistent or shared indexes without rewriting their similarity logic.

## Status

Pari is pre-alpha. Public APIs may change while the core architecture is established.

## Python quick start

Install from the repository:

```bash
python -m pip install .
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

## Design goals

- Batch-first compute, insert, and query paths.
- Safe, versioned persistence instead of executable object serialization.
- A persistent local index between RAM-only prototypes and remote databases.
- A small Rust core shared by Python and CLI frontends.
- Correctness tests and benchmark evidence for performance-sensitive changes.
- No merge to `main` while required CI is failing.

## Layers

- `pari-core`: signatures and similarity primitives.
- `pari-index`: LSH and candidate generation.
- `pari-store`: local and remote storage backends.
- `pari-py`: PyO3 Python bindings and the `pari` package.
- `pari-cli`: command-line workflows.

See [docs/architecture.md](docs/architecture.md) and the GitHub issues for the implementation roadmap.

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
