# CI dependency pins

Every third-party GitHub Action in active workflows is pinned to a reviewed 40-character commit SHA. Service images are pinned to a release tag plus immutable multi-architecture digest. `scripts/check_workflow_pins.py` enforces both rules in normal CI.

The current Redis service is `redis:7.4.2-alpine` at index digest `sha256:ff02b58f971e7d7d156a1267e283fcbbeee91773b6aa36c49dac28ecfe28eadf`. Keeping the tag beside the digest makes the intended release visible; the digest controls what executes.

Dependabot checks GitHub Actions and Docker dependencies weekly and proposes reviewed pull requests. Pin updates must explain the upstream version, preserve least-privilege permissions, and pass Rust, Python, Redis, MSRV, and applicable Release Validation checks. Do not replace a SHA or digest with a floating major tag to make updates easier.

Pull-request workflows receive read-only repository contents by default and no publication environments or OIDC permissions. Publication and attestation permissions remain confined to tag-only jobs in `release.yml`.
