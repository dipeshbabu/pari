# Dependency maintenance policy

Dependabot proposes weekly Cargo, Python, GitHub Actions, and Docker updates through pull requests. Updates are never auto-merged.

Every dependency PR must pass current-toolchain CI, Rust 1.81 MSRV, applicable release validation, and `cargo deny`. Persisted formats, signature semantics, workflow permissions, and publication tooling require explicit review even for patch updates.

`cargo deny` fails on known advisories, yanked crates, unapproved licenses, and unknown registries or Git sources. Multiple versions are review warnings. Exceptions must be narrow, documented in `deny.toml`, and include a removal condition.

If a dependency release raises MSRV, retain a maintained compatible version or make the increase an explicit minor-release change. Optional integration updates stay grouped separately and do not become core dependencies.
