# Pari

Pari is a fast similarity indexing and deduplication engine for large datasets.

The project is being built around a Rust core with Python bindings and a CLI. The goal is to let users move from an in-memory prototype to persistent or shared indexes without rewriting their similarity logic.

## Status

Pari is pre-alpha. Public APIs may change while the core architecture is established.

## Design goals

- Batch-first compute, insert, and query paths.
- Safe, versioned persistence instead of executable object serialization.
- A persistent local index between RAM-only prototypes and remote databases.
- A small Rust core shared by Python and CLI frontends.
- Correctness tests and benchmark evidence for performance-sensitive changes.
- No merge to `main` while required CI is failing.

## Planned layers

- `pari-core`: signatures and similarity primitives.
- `pari-index`: LSH and candidate generation.
- `pari-store`: local and remote storage backends.
- `pari-py`: PyO3 Python bindings.
- `pari-cli`: command-line workflows.

See [docs/architecture.md](docs/architecture.md) and the GitHub issues for the implementation roadmap.

## Development

```bash
cargo fmt --all -- --check
cargo clippy --workspace --all-targets --all-features -- -D warnings
cargo test --workspace --all-targets --all-features
```

## License

Pari is MIT licensed. See [LICENSE](LICENSE). Portions derived from or informed by third-party MIT-licensed projects are documented in [NOTICE](NOTICE).
