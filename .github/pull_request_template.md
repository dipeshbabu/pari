## What

Describe the change and the user or engineering problem it solves.

Closes #

## Correctness

- [ ] Tests cover new behavior or regressions.
- [ ] Compatibility and migration impact are documented where relevant.
- [ ] Persisted or serialized data changes are versioned and validated.

## Performance

- [ ] No performance-sensitive behavior changed, or benchmark evidence is included below.

Benchmark notes:

## Checks

- [ ] `cargo fmt --all -- --check`
- [ ] `cargo clippy --workspace --all-targets --all-features -- -D warnings`
- [ ] `cargo test --workspace --all-targets --all-features`
- [ ] Required GitHub Actions checks are green before merge.

## Attribution

- [ ] No third-party code was copied, or required license/copyright notices are preserved in `NOTICE`.
