#!/usr/bin/env bash
# Run the scale-10m decision campaign (issue #95) on a rented dedicated Linux host.
#
# Usage, from a fresh clone on the host's local NVMe scratch:
#   bash scripts/run_dedicated_scale.sh [SCRATCH_DIR]
#
# The script refuses hosts that do not meet the documented decision environment,
# runs the 1M preflight before 10M, records OS and storage telemetry, and writes
# one checksummed archive to hand back for validation and reporting. It never
# relaxes correctness checks and preserves partial evidence if a phase fails.
set -euo pipefail

RUST_TOOLCHAIN="1.97.1"
MATURIN_VERSION="1.14.1"
# Overrides exist only so CI can smoke-test this script on a small runner. They
# are recorded in the evidence; a decision run must use the defaults.
MIN_MEMORY_GIB="${PARI_SCALE_MIN_MEMORY_GIB:-128}"
MIN_FREE_DISK_GIB="${PARI_SCALE_MIN_FREE_DISK_GIB:-200}"
PROFILES="${PARI_SCALE_PROFILES:-scale-1m scale-10m}"
SCRATCH="${1:-$HOME/pari-scale-run}"
REPO="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
case "$(realpath -m "$SCRATCH")/" in
  "$REPO"/*) echo "preflight: scratch must be outside the checkout" >&2; exit 1 ;;
esac

fail() { echo "preflight: $*" >&2; exit 1; }

# --- Host preflight --------------------------------------------------------
[[ "$(uname -s)" == "Linux" ]] || fail "native Linux is required"
if grep -qi -e microsoft -e wsl /proc/version 2>/dev/null; then
  fail "WSL is not an accepted decision environment"
fi
memory_gib=$(( $(awk '/MemTotal/ {print $2}' /proc/meminfo) / 1024 / 1024 ))
# A 128 GiB machine reports slightly less usable memory to the kernel.
(( memory_gib + 4 >= MIN_MEMORY_GIB )) || fail "need ${MIN_MEMORY_GIB} GiB RAM, found ${memory_gib} GiB"
mkdir -p "$SCRATCH"
free_gib=$(( $(df --output=avail -k "$SCRATCH" | tail -1) / 1024 / 1024 ))
(( free_gib >= MIN_FREE_DISK_GIB )) || fail "need ${MIN_FREE_DISK_GIB} GiB free on $SCRATCH, found ${free_gib} GiB"
cd "$REPO"
[[ -z "$(git status --porcelain)" ]] || fail "the checkout must be clean"
SHA="$(git rev-parse HEAD)"
echo "host ok: ${memory_gib} GiB RAM, ${free_gib} GiB free on $SCRATCH, Pari $SHA"

# --- Pinned toolchain ------------------------------------------------------
if ! command -v rustup >/dev/null; then
  curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y --default-toolchain none
  # shellcheck disable=SC1091
  source "$HOME/.cargo/env"
fi
rustup toolchain install "$RUST_TOOLCHAIN" --profile minimal
rustup default "$RUST_TOOLCHAIN"
python3 -m venv "$SCRATCH/venv"
# shellcheck disable=SC1091
source "$SCRATCH/venv/bin/activate"
python -m pip install --quiet --upgrade pip "maturin==${MATURIN_VERSION}"
maturin build --release --out "$SCRATCH/dist"
python python/tests/install_wheel.py "$SCRATCH/dist"

# --- Telemetry -------------------------------------------------------------
OUT="$SCRATCH/evidence-$SHA"
mkdir -p "$OUT"
{
  uname -a; cat /etc/os-release; lscpu; free -b; df -hT "$SCRATCH"
  lsblk -o NAME,MODEL,SIZE,ROTA,TYPE,MOUNTPOINT
  findmnt -T "$SCRATCH"; rustc --version; python --version
  echo "profiles: $PROFILES; minimum memory GiB: $MIN_MEMORY_GIB; minimum free disk GiB: $MIN_FREE_DISK_GIB"
} > "$OUT/host.txt" 2>&1 || true
vmstat -t 5 > "$OUT/vmstat.log" 2>&1 &
telemetry=($!)
if command -v iostat >/dev/null; then
  iostat -xmt 5 > "$OUT/iostat.log" 2>&1 &
  telemetry+=($!)
fi
trap 'kill "${telemetry[@]}" 2>/dev/null || true' EXIT

# --- Campaign: 1M preflight, then 10M --------------------------------------
export PARI_GIT_SHA="$SHA" TMPDIR="$SCRATCH/tmp"
mkdir -p "$TMPDIR"
overall=0
for profile in $PROFILES; do
  echo "running $profile"
  status=0
  python scripts/benchmark_campaign.py run "$profile" \
    --output "$OUT/$profile" --include-text --preserve-failure-evidence \
    > >(tee "$OUT/$profile.log") 2>&1 || status=$?
  if (( status != 0 )); then
    echo "$profile failed with status $status; its partial evidence is preserved" | tee -a "$OUT/$profile.log"
    overall=$status
    break
  fi
  python scripts/benchmark_campaign.py render "$OUT/$profile/bundle.json" \
    --output "$OUT/$profile/report.md"
done

# --- Hand-back archive -----------------------------------------------------
kill "${telemetry[@]}" 2>/dev/null || true
archive="$SCRATCH/pari-scale-evidence-$SHA.tar.gz"
tar -C "$SCRATCH" -czf "$archive" "$(basename "$OUT")"
sha256sum "$archive" | tee "$archive.sha256"
echo "done: send $archive and its .sha256 for validation"
exit "$overall"
