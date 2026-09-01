# CI dependency pins

Every third-party GitHub Action in active workflows is pinned to a reviewed 40-character commit SHA. Service images are pinned to a release tag plus immutable multi-architecture digest. `scripts/check_workflow_pins.py` enforces both rules in normal CI.

The current Redis service is `redis:7.4.2-alpine` at index digest `sha256:ff02b58f971e7d7d156a1267e283fcbbeee91773b6aa36c49dac28ecfe28eadf`. Keeping the tag beside the digest makes the intended release visible; the digest controls what executes.

Dependabot checks GitHub Actions and Docker dependencies weekly and proposes reviewed pull requests. Pin updates must explain the upstream version, preserve least-privilege permissions, and pass Rust, Python, Redis, MSRV, and applicable Release Validation checks. Do not replace a SHA or digest with a floating major tag to make updates easier.

Pull-request workflows receive read-only repository contents by default and no publication environments or OIDC permissions. Publication and attestation permissions remain confined to tag-only jobs in `release.yml`.

The dependency-policy job may restore the exact `cargo-deny` binary produced by a successful `main` run. Its cache key binds the operating system, architecture, Rust toolchain, reviewed cargo-deny version, and CI workflow contents. Pull-request runs can read a matching default-branch cache but cannot save or replace it; a miss performs the locked, exact-version install before running the full policy check. Workflow changes therefore miss the old cache. The cache is only an acceleration layer: every run reports the restored or installed version and executes `cargo deny check advisories licenses bans sources`.

CI, Python, Redis, and Release Validation cancel an older run only when a newer commit updates the same pull request. Pushes to `main`, release tags, scheduled workflows, and manually dispatched workflows are never cancellation targets. Required check names and coverage apply to the latest mergeable pull-request head exactly as before.

The required formatting job installs Ruff from PyPI at the exact version declared in both `pyproject.toml` and the workflow environment. Ruff is a development tool, not a package runtime dependency. A formatting-tool upgrade must update both pins together, retain explicit lint selection, preserve the benchmark-fixture exclusions, and pass the complete Python and release suites before merge.
