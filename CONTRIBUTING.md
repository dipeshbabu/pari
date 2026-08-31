# Contributing to Pari

Pari is performance-sensitive infrastructure. Keep changes small enough to review and measure.

## Workflow

1. Start from a focused GitHub issue with scope and acceptance criteria.
2. Create a branch named for the issue or feature.
3. Add tests for new behavior and regression tests for fixes.
4. Run the required checks locally when possible:

   ```bash
   cargo fmt --all -- --check
   cargo clippy --workspace --all-targets --all-features -- -D warnings
   cargo test --workspace --all-targets --all-features
   ```

   Changes to public Rust crates or `Cargo.lock` must also preserve the declared Rust 1.81 MSRV:

   ```bash
   cargo +1.81 check --locked -p pari-core -p pari-format -p pari-index -p pari-store
   ```

5. Add benchmark evidence when a PR claims a performance improvement or changes a hot path.
6. Update documentation for public APIs and persisted formats.
7. Open a pull request that links the issue and explains correctness, compatibility, and performance impact.
8. Do not merge while required CI is failing or incomplete.

## Enforced merge policy

GitHub protects `main`; direct pushes, force pushes, and branch deletion are disabled for administrators as well as contributors. Every change uses a pull request whose branch is current with `main`, whose required cross-platform Rust/Python/Redis checks pass on the mergeable head, and whose review conversations are resolved.

The repository enables Squash and merge only. Merge commits and rebase merges are disabled, and GitHub deletes the short-lived source branch after merge. Release-sensitive changes must also complete the path-triggered Release Validation workflow even though that expensive workflow is not an always-present branch-protection context.

Tags matching `v*` have an active ruleset that blocks update, deletion, and non-fast-forward changes. There is no standing bypass. Emergency recovery requires an explicit, auditable settings change by the repository administrator, followed by immediate restoration of the rules before normal work resumes. Published tags are never moved to repair a release; publish a new version instead.

The required check names are repository configuration. When a workflow job is renamed, update branch protection in the same maintenance window so `main` is neither bypassable nor permanently blocked.

## Engineering rules

- Prefer safe Rust. The workspace forbids `unsafe` by default. Any future exception requires a dedicated issue, safety invariants, tests, and benchmark justification.
- Batch APIs are first-class. Avoid implementations that make N network or storage round trips for N items when batching is available.
- Persisted data must use explicit, versioned formats. Do not introduce executable deserialization such as Python pickle.
- Validate compatibility before comparing or merging signatures and indexes.
- Avoid speculative abstractions. Add a backend or optimization after a concrete use case and measurement justify it.
- Keep user-facing APIs simpler than the implementation; advanced tuning should be optional.
- Do not raise the workspace MSRV through a language feature or dependency update. An intentional MSRV increase is a minor-release decision that updates Cargo metadata, compatibility documentation, the changelog, and release notes together.
- Exact-version public dependencies can make standalone dependent-package verification impossible between coordinated releases. The MSRV job must still compile the local public graph and build every tarball; use `--no-verify` only for the dependent tarball whose matching registry dependency is not published yet, and remove that exception when the release transition permits registry verification.

## Third-party code

When copying or substantially deriving code from another project, confirm the license is compatible and preserve all required notices. Datasketch-derived work must keep the attribution recorded in `NOTICE`.
