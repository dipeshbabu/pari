# Releasing Pari

This document defines the operator procedure for Pari releases. The workspace version in the root `Cargo.toml` is the single release version source. Python derives that version from `pari-py` through maturin.

## Release artifacts

A version tag `vX.Y.Z` builds and validates:

- `pari-similarity` source distribution and abi3 wheels.
- `pari` CLI archives for Linux x86_64, macOS arm64, and Windows x86_64.
- crates.io packages for the public Rust crates: `pari-core`, `pari-format`, `pari-index`, and `pari-store`.
- SHA-256 checksums.
- a CycloneDX dependency SBOM generated from locked Cargo metadata.
- GitHub build provenance attestations for assembled release files.

The release workflow never builds in a job that has publication credentials. Build jobs have read-only repository access. OIDC permissions exist only in publication/attestation jobs.

## Before the first 0.1.0 release

### 1. Merge the release automation

All normal Rust, Python, Redis, and Release Validation jobs must be green on the exact release PR head. Merge with Squash and merge only.

### 2. Configure PyPI Trusted Publishing

Create a pending or normal Trusted Publisher for distribution `pari-similarity` with:

- owner: `dipeshbabu`
- repository: `pari`
- workflow: `release.yml`
- environment: `pypi`

Configure a GitHub environment named `pypi` and require manual approval if available. Do not add a PyPI API token secret.

### 3. Bootstrap the four crates.io packages once

crates.io trusted publishing can only be configured after a crate exists. For 0.1.0 only, publish from a clean checkout of the exact release commit using a local crates.io credential. Do not place that credential in GitHub Actions.

Publish in dependency order:

```bash
cargo publish -p pari-core
# Wait until pari-core 0.1.0 is visible in the crates.io index.

cargo publish -p pari-format
# Wait until pari-format 0.1.0 is visible.

cargo publish -p pari-index
# Wait until pari-index 0.1.0 is visible.

cargo publish -p pari-store
```

Before each real publish, run the corresponding `cargo publish --dry-run` from the clean checkout.

After the packages exist, configure crates.io Trusted Publishing for each public crate using this repository, `.github/workflows/release.yml`, and the `crates-io` GitHub environment. Future releases use OIDC and `rust-lang/crates-io-auth-action`; no long-lived crates.io token is stored in GitHub.

### 4. Protect publication environments and tags

Create GitHub environments `pypi` and `crates-io` with required reviewers where appropriate. The active `Protect release tags` ruleset permits new `v*` tags but blocks updates, deletion, and non-fast-forward changes. It has no standing bypass; an emergency requires an explicit auditable ruleset edit and immediate restoration.

`main` is protected separately: changes require an up-to-date pull request, the configured cross-platform check matrix, resolved conversations, and linear history. Normal pull requests use Squash and merge only, and their branches are deleted after merge.

## Validate a release candidate

From a clean checkout:

```bash
python scripts/release.py validate
cargo fmt --all -- --check
cargo clippy --workspace --all-targets --all-features -- -D warnings
cargo test --workspace --all-targets --all-features
```

Opening a PR that changes release-sensitive paths automatically runs the Release Validation workflow. It builds install-tested Python wheels, CLI archives, public root Rust packages, checks dependent public crate metadata, generates an SBOM, and assembles checksums without publish permissions.

## Create the 0.1.0 tag

Only after the four crates.io bootstrap packages are visible and `main` is green:

```bash
git checkout main
git pull --ff-only
git tag -s v0.1.0 -m "Pari 0.1.0 alpha"
git push origin v0.1.0
```

The immutable tag must exactly match the root workspace version. The release workflow rejects mismatches.

For v0.1.0, the crates.io publication job verifies the bootstrapped crates rather than publishing them again. The workflow publishes `pari-similarity` to PyPI through Trusted Publishing and creates the GitHub Release from the already-tested artifacts.

## Subsequent releases

For releases after 0.1.0:

1. update the workspace version and exact public-crate dependency versions in one PR;
2. update `CHANGELOG.md` and add `docs/releases/X.Y.Z.md`;
3. run the full release validation PR gate;
4. configure/verify Trusted Publishing for all public packages;
5. tag the exact green `main` commit;
6. allow the release workflow to publish Rust crates in dependency order using the temporary OIDC token, then publish the Python distribution and GitHub Release.

## Yank and rollback policy

Published package versions are immutable. If a release is broken:

- do not move or recreate the release tag;
- yank the affected crates.io version when appropriate;
- yank the PyPI release only when necessary for installation safety;
- publish a new patch version containing the fix;
- keep the GitHub Release and changelog entry with a visible note explaining the superseding release.

Never overwrite an existing `.pari` compatibility promise through a tag replacement.
