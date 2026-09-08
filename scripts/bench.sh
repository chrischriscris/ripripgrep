#!/usr/bin/env bash
# Repeated filesystem-scan benchmark; requires hyperfine, rg, and a release build.
# Usage: scripts/bench.sh [corpus-dir] [--quick]
set -euo pipefail

CORPUS="${1:-$HOME/.cache/rrg-corpora/linux}"
RRG="$(git rev-parse --show-toplevel)/target/release/rrg"
[ -d "$CORPUS" ] || { echo "corpus not found: $CORPUS" >&2; exit 1; }
[ -x "$RRG" ] || { echo "build first: cargo build --release --locked" >&2; exit 1; }
command -v hyperfine >/dev/null || { echo "hyperfine not installed" >&2; exit 1; }
RG="$(command -v rg)" || { echo "ripgrep not installed" >&2; exit 1; }

WARMUP=3
RUNS=20
if [ "${2:-}" = "--quick" ]; then WARMUP=1; RUNS=5; fi

QUERIES=("spin_lock" "out of memory" "TODO" "spin_lock|mutex_unlock|wait_queue")
for i in "${!QUERIES[@]}"; do
  q="${QUERIES[$i]}"
  flags=()
  if [ "$i" -lt 3 ]; then flags=(-F); fi
  # Quote complete argument vectors for hyperfine's explicit bash shell.
  printf -v rrg_cmd '%q ' "$RRG" --color never --no-heading --with-filename --line-number "${flags[@]}" -- "$q" "$CORPUS"
  printf -v rg_cmd '%q ' "$RG" --no-config --color never --no-heading --with-filename --line-number "${flags[@]}" -- "$q" "$CORPUS"
  echo "== query: $q"
  hyperfine --shell bash --warmup "$WARMUP" --runs "$RUNS" \
    --style basic --export-json "bench-$(basename "$CORPUS")-$i.json" \
    "$rrg_cmd" "$rg_cmd"
done
