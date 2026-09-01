# Platform support and release targets

Pari's runtime code uses safe, portable Rust and does not enable architecture-specific `target-cpu=native`, unsafe SIMD, or host-only CPU features in release artifacts.

## Published 0.2.0 artifacts

The immutable 0.2.0 release contains:

| Interface | Operating system | Architecture | Artifact policy |
| --- | --- | --- | --- |
| Python wheel | Linux | x86-64 | manylinux2014 / glibc 2.17 |
| Python wheel | macOS | arm64 | macOS 11 or newer tag |
| Python wheel | Windows | x86-64 | CPython abi3 (`cp310`) |
| CLI | Linux | x86-64 | Native release binary |
| CLI | macOS | arm64 | Native release binary |
| CLI | Windows | x86-64 | Native release binary |

## Linux arm64 validation for subsequent releases

Release Validation adds these artifacts for the next tagged version:

| Interface | Rust target | Artifact |
| --- | --- | --- |
| Python wheel | `aarch64-unknown-linux-gnu` | manylinux2014 aarch64, CPython abi3 (`cp310`) |
| CLI | `aarch64-unknown-linux-gnu` | `pari-X.Y.Z-linux-arm64.tar.gz` |

The wheel is built with the pinned [`PyO3/maturin-action`](https://github.com/PyO3/maturin-action) and Maturin version under the manylinux2014 policy. It is then installed on GitHub's native [`ubuntu-24.04-arm`](https://github.com/actions/runner-images/blob/main/images/ubuntu/Ubuntu2404-Arm64-Readme.md) runner, where a focused smoke covers `MinHash`, memory-backed `DedupeIndex`, and persistent `Index` create/sync/reopen/query behavior before the full installed-package suite runs.

The CLI is compiled natively on `ubuntu-24.04-arm`. Validation checks `uname -m`, `pari --version`, `pari --help`, and a small JSONL index/verify workflow before packaging. This avoids claiming support for a binary that was only cross-compiled.

The target uses the Rust compiler's baseline `aarch64-unknown-linux-gnu` CPU assumptions. Pari does not require optional ARM extensions beyond the target baseline. Existing Linux x86-64, macOS arm64, and Windows x86-64 jobs remain unchanged.

Final release assembly gives x86-64 and arm64 Linux archives distinct names, includes both in `SHA256SUMS`, and subjects both to the same provenance attestation. Python wheel platform tags keep the Linux architectures unambiguous.
