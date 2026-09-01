# Dependency maintenance policy

Dependabot proposes weekly Cargo, Python, GitHub Actions, and Docker updates through pull requests. Updates are never auto-merged.

Every dependency PR must pass current-toolchain CI, Rust 1.81 MSRV, applicable release validation, and `cargo deny`. Persisted formats, signature semantics, workflow permissions, and publication tooling require explicit review even for patch updates.

`cargo deny` fails on known advisories, yanked crates, unapproved licenses, and unknown registries or Git sources. Multiple versions are review warnings. Exceptions must be narrow, documented in `deny.toml`, and include a removal condition.

If a dependency release raises MSRV, retain a maintained compatible version or make the increase an explicit minor-release change. Optional integration updates stay grouped separately and do not become core dependencies.

The Rust 1.81 line currently retains `redis` 0.32.7 and `sha1` 0.10.x.
`redis` 1.0 already requires Rust 1.85 and the current 1.6 release requires
Rust 1.88; `sha1` 0.11 requires Rust 1.85. Dependabot therefore ignores the
incompatible Redis major and SHA-1 minor/major update classes while continuing
to propose compatible patch releases. Remove those ignore rules only as part of
an intentional MSRV increase or after an upstream release again supports the
declared toolchain, with the full local, package, and CI graph revalidated.
