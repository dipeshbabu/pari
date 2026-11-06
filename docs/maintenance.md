# Patch releases and backports

Pari supports the latest published minor line. `main` is forward development,
not a promise to maintain every older alpha. This policy supplements the
[compatibility contract](compatibility.md), [release procedure](releasing.md),
and [security policy](../SECURITY.md).

## Choosing a release base

If `main` contains only changes eligible for the current minor line, use the
normal release PR. If it already contains work for a later minor line, cut a
patch from the latest published tag in the affected line and select fixes
individually. Never merge future feature development into a maintenance branch.

For example, if `v0.1.2` is published while `main` contains later minor work,
`maintenance/0.1` starts at `v0.1.2`. Selected fixes and coordinated metadata can
then prepare `v0.1.3` without including the future APIs. This is an example, not
an instruction to create a release or a standing maintenance branch.

```sh
git fetch origin --tags
git switch --create maintenance/0.1 v0.1.2
git switch --create fix/maintenance-example
git cherry-pick -x <reviewed-fix-commit>
```

Use `maintenance/MAJOR.MINOR` for the release line and short-lived fix branches
for PRs into it. Start from the latest published patch, not an arbitrary older
tag. Create the branch only when a confirmed bug and users of that line justify
a separate release. `main` remains the only forward-development branch.

## Eligible changes

- Correctness, crash, data-loss, and security fixes with a focused reproducer.
- Packaging, installation, portability, and build fixes for supported targets.
- Release-tooling repairs needed to produce and verify the patch safely.
- Narrow documentation corrections relevant to the maintained release.
- Minimal dependency updates needed to address a verified vulnerability or
  restore the declared compiler/platform support.

Do not backport new public features, experimental surfaces, signature or format
changes, machine-output revisions, broad refactors, or routine dependency churn.
An implementation adjustment necessary for a fix must be explained and tested
against the target branch's existing contracts. Keep supported APIs, format-v1
bytes and readers, signature identities, and CLI revision-1 semantics compatible.
Rejecting previously accepted corrupt or invalid inputs can be a bug fix.

Rust 1.81 for the non-Python workspace and Rust 1.83 for the Python extension
remain required unless a separately documented minor-release decision changes
them. Do not suppress advisories, remove optional-feature audits, or relax
minimum-version checks to make a backport pass. Avoid unrelated lockfile changes.

## Review and provenance

Fix the forward branch first when possible, then cherry-pick the reviewed
squash commit with `-x`. Link the original issue, original PR/commit, affected
release, and target maintenance branch in the backport PR. Describe why the fix
is needed there and what changed during conflict resolution. A maintenance-first
emergency must have a linked forward-port PR so `main` does not retain the bug.

Resolve conflicts on the fix branch; do not silently drop tests or prerequisites.
If a minimal compatible backport is not practical, document why and direct users
to a fixed newer minor release instead. Security details follow private
coordination in `SECURITY.md` until disclosure is appropriate.

Merge only after the complete configured Rust, Python, optional integration,
Redis, MSRV, dependency-policy, and Release Validation matrix passes for the
exact up-to-date backport head. Use a squash merge and retain the original-fix
reference in the final commit body. Re-run the gates after any conflict fix or
base update; success on the original `main` PR is not a maintenance-branch result.

## Branch and publication controls

Before using a maintenance branch, copy `main`'s applicable protections: required
PRs and check contexts, up-to-date heads, resolved conversations, linear history,
and no force pushes or deletion while the line is active. CI runs on pushes to
`maintenance/*` and on PRs. Keep the protected checks enabled; branch naming alone
does not establish protection.

The publication environments normally admit `main` and `v*` tags. When activating
a maintenance line, explicitly allow that exact protected branch in both
`crates-io` and `pypi` for authentication preflight. Do not add a broad maintenance
wildcard or weaken release-tag creation/immutability rules. Remove the temporary
branch environment policies when the line retires. Do not add long-lived tokens.

## Producing a patch

1. Bump the workspace, both lockfiles, exact public-crate dependencies, Python
   metadata-derived version, installation examples, and release-contract tests
   together to the next unused patch. All four public crates, Python, and CLI
   remain on the same version.
2. Date the changelog and write release notes listing the selected fixes, original
   PRs, affected versions, and any validation or migration implications. Release
   notes must not imply that unrelated future features were backported.
3. Pass the full PR matrix, including all four verified prebuilt Rust archives,
   installed wheels, the sdist, CLI platforms, and coordinated MSRV checks.
4. Squash-merge into the protected maintenance branch and run the full manual
   release preflight from that exact branch commit, including the crates.io OIDC
   authentication check. Verify the trusted publishers still match the repository,
   workflow, and environments.
5. After branch CI and preflight pass, create and verify the signed version tag
   on that exact commit. A maintenance tag must never point to future `main` work.
6. Let the tag workflow publish the verified, unchanged artifacts using OIDC.
   Verify all registry versions and checksums, CLI/wheel installation, SBOM,
   provenance attestations, release notes, and deployment results.

Tags, uploaded files, and published versions are immutable. A failed publication
is retried only with the same validated bytes; a conflicting or broken published
artifact requires a new patch. Do not replace a tag or rebuild different bytes
under a published filename. Record security advisories or yanks when needed for
installation safety, following the release and security policies.

## Support lifecycle

The latest minor line is eligible for focused patch releases. An older minor
line stops receiving routine patches when a newer minor is published. There is
no automatic LTS promise, dependency-update stream, or permanent branch per alpha.

A maintainer can document one time-boxed exception for a confirmed critical
security/data-loss issue affecting active users who cannot yet upgrade. Record
the reason, scope, owner, and an explicit retirement date no more than 30 days
away in the tracking issue; renewal requires a new recorded decision. An
exception never weakens the validation or compatibility requirements above.

On retirement, close or retarget outstanding backports, identify the fixed
supported replacement in the issue/release notes, remove the branch from
publication environments, and delete the maintenance branch when no release work
remains. Preserve all published tags, release artifacts, provenance, and history.
