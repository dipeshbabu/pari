#!/usr/bin/env bash
# Compare small pari-bench profiles between the checked-out change and a base
# revision, alternating between them so runner noise hits both alike, then
# fail if a watched metric is much worse (see scripts/check_performance.py).
#
# Usage: scripts/ci_performance.sh BASE_REVISION [ROUNDS] [ITEMS]
set -euo pipefail
script_dir=$(cd "$(dirname "$0")" && pwd)

base_revision=$1
rounds=${2:-3}
items=${3:-20000}
work=$(mktemp -d)
trap 'git worktree remove --force "$work/base" >/dev/null 2>&1 || true; rm -rf "$work"' EXIT

git worktree add --detach "$work/base" "$base_revision" >/dev/null
cargo build --locked --release -p pari-bench
(cd "$work/base" && cargo build --locked --release -p pari-bench)
current_bench=$PWD/target/release/pari-bench
base_bench=$work/base/target/release/pari-bench

baseline_args=()
current_args=()
for round in $(seq 1 "$rounds"); do
  for side in base current; do
    bench=$current_bench
    [[ $side == base ]] && bench=$base_bench
    out=$work/$side-$round
    mkdir -p "$out"
    TMPDIR=$work "$bench" run --items "$items" --output "$out/run.json" >/dev/null
    TMPDIR=$work "$bench" storage --items "$items" --output "$out/storage.json" >/dev/null
    if [[ $side == base ]]; then
      baseline_args+=(--baseline "$out")
    else
      current_args+=(--current "$out")
    fi
  done
done

python3 "$script_dir/check_performance.py" "${baseline_args[@]}" "${current_args[@]}"
