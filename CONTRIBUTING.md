# Contributing to Pari

Pari is performance-sensitive infrastructure. Keep changes small enough to review and measure.

## Workflow

1. Start from a focused GitHub issue with scope and acceptance criteria.
2. Create a branch named for the issue or feature.
3. Add tests for new behavior and regression tests for fixes.
4. Run the core checks locally:

   ```bash
   cargo fmt --all -- --check
   cargo fmt --manifest-path benchmarks/criterion/Cargo.toml --all -- --check
   python -m pip install "ruff==0.16.5"
   ruff format --check python scripts benchmarks examples
   ruff check python scripts benchmarks examples
   python scripts/check_workflow_pins.py
   cargo clippy --workspace --all-targets --all-features -- -D warnings
   cargo clippy --manifest-path benchmarks/criterion/Cargo.toml --all-targets -- -D warnings
   cargo test --workspace --all-targets --all-features
   cargo test --manifest-path benchmarks/criterion/Cargo.toml --all-targets
   python -m unittest scripts.tests.test_benchmark_campaign -v
   ```

   Changes to public Rust crates or `Cargo.lock` must also preserve the declared Rust 1.81 MSRV:

   ```bash
   cargo +1.81 check --locked \
     -p pari-core -p pari-format -p pari-index -p pari-store
   cargo +1.81 package --locked -p pari-core
   cargo +1.81 package --locked -p pari-format
   cargo +1.81 package --locked -p pari-index
   cargo +1.81 package --locked -p pari-store
   ```

   Run the checks for every surface the change affects. For Python bindings, build and test the installed wheel rather than importing from the source tree:

   ```bash
   python -m pip install "maturin==1.14.1"
   maturin build --release --out dist
   python python/tests/install_wheel.py dist
   python -m unittest discover -s python/tests -p "test_*.py" -v
   ```

   Dependency changes must also pass `cargo deny check advisories licenses bans sources`. Release metadata and packaging changes must pass `python scripts/release.py validate`. The platform, integration, Redis, and release workflows remain authoritative for checks that need their managed environment.

   CI may restore the reviewed cargo-deny binary from a cache written only by a successful `main` run. A cache miss is expected after a workflow, toolchain, version, operating-system, or architecture change and must fall back to the exact locked install. Do not make pull-request jobs cache publishers or remove the policy check on a cache hit.

### Formatting policy

- Rust uses the canonical `rustfmt` component from the pinned Rust toolchain. Run `cargo fmt --all` and the separate Criterion-workspace command before committing Rust changes. Repository settings live in `rustfmt.toml`.
- Maintained Python and `.pyi` files use Ruff 0.16.5 with the explicit stable policy in `pyproject.toml`. Run `ruff check --fix python scripts benchmarks examples` first, then `ruff format python scripts benchmarks examples`. CI uses the corresponding `--check` commands.
- `examples/code_corpus_fixture/**` is benchmark input, not maintained application source. It is deliberately excluded so a formatter upgrade cannot silently change workload evidence. Markdown is also excluded from Ruff; documentation formatting remains review-driven.
- Do not enable Ruff preview formatting or implicit default lint selection in CI. Version and rule changes require a focused review because they can mechanically rewrite public examples, generated evidence tooling, and shipped stubs.

5. Add benchmark evidence when a PR claims a performance improvement or changes a hot path.
6. Update documentation for public APIs and persisted formats.
7. Open a pull request that links the issue and explains correctness, compatibility, and performance impact.
8. Do not merge while required CI is failing or incomplete.

Superseded runs for an updated pull request are canceled to avoid spending runner time on an obsolete head. The newest mergeable head must still complete every required check; `main`, tag, scheduled, and manual runs are not canceled by this policy.

## Compatibility and performance evidence

Check the [v0.x compatibility contract](docs/compatibility.md) before changing a public Python, Rust, CLI, signature, or persisted-format surface. Supported 0.2.x interfaces must remain compatible in patch releases. Experimental interfaces can change in a minor release only when the release notes explain the impact and migration path. Persisted data and machine-readable output must never be silently reinterpreted.

A performance claim needs a reproducible baseline and comparison from the same machine, source revision, workload, and cache policy. Include the commands, environment, raw JSON reports, correctness/parity results, and an explanation of material variance in the pull request. Use the named campaign when the change affects end-to-end behavior:

```bash
python scripts/benchmark_campaign.py run smoke \
  --output benchmark-artifacts/smoke
python scripts/benchmark_campaign.py validate \
  benchmark-artifacts/smoke/bundle.json
```

Larger or publishable runs must follow the environment and artifact rules in the [benchmark guide](docs/benchmarks.md). CI timing on shared runners is not benchmark evidence.

For a CI-runtime change, record job and relevant step timings from multiple recent successful pull requests before implementation. After merge and any required cache warm-up, compare multiple successful runs, report hits and misses separately, and include medians and ranges without presenting shared-runner timing as application-performance evidence.

## Issues and triage

Use the issue form that matches the report. Bug, compatibility, and performance reports should contain a minimal reproduction, exact Pari version or commit, interface, operating system, architecture, toolchain/runtime versions, workload size, and relevant logs or artifacts. Report security problems privately through the [security policy](SECURITY.md).

Triage uses one type label where useful, one or more focused `area:` labels, and a `priority:` or `status:` label only after impact and scheduling are understood. New labels should represent a recurring queue, not a single issue.

## Enforced merge policy

GitHub protects `main`; direct pushes, force pushes, and branch deletion are disabled for administrators as well as contributors. Every change uses a pull request whose branch is current with `main`, whose required cross-platform Rust/Python/Redis checks pass on the mergeable head, and whose review conversations are resolved.

The repository enables Squash and merge only. Merge commits and rebase merges are disabled, and GitHub deletes the short-lived source branch after merge. Release-sensitive changes must also complete the path-triggered Release Validation workflow even though that expensive workflow is not an always-present branch-protection context.

Tags matching `v*` have an active ruleset that blocks update, deletion, and non-fast-forward changes. There is no standing bypass. Emergency recovery requires an explicit, auditable settings change by the repository administrator, followed by immediate restoration of the rules before normal work resumes. Published tags are never moved to repair a release; publish a new version instead.

The required check names are repository configuration. When a workflow job is renamed, update branch protection in the same maintenance window so `main` is neither bypassable nor permanently blocked.

Workflow dependency pins follow [the CI supply-chain policy](docs/ci-supply-chain.md). Dependabot proposes updates; reviewers verify the upstream release and keep immutable SHAs/digests.

Report vulnerabilities through [the private security process](SECURITY.md), not public issues. Dependency changes follow [the advisory, license, source, and MSRV policy](docs/dependency-policy.md).

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
