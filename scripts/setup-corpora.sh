#!/usr/bin/env bash
# Clone the benchmark corpora (shallow, so it's cheap) and generate query sets.
# Usage: scripts/setup-corpora.sh [target-dir]
set -euo pipefail

TARGET="${1:-$HOME/.cache/rrg-corpora}"
mkdir -p "$TARGET"

clone() {
  local name="$1" url="$2"
  if [ -d "$TARGET/$name/.git" ]; then
    echo "[corpora] $name already present"
  else
    echo "[corpora] shallow-cloning $name ..."
    git clone --depth 1 --single-branch "$url" "$TARGET/$name"
  fi
}

# linux: ~96K files — the exact corpus tgrep reports 21-35x wins on
clone linux https://github.com/torvalds/linux.git
# ripgrep itself: small but the reference repo for our direct competitor
clone ripgrep https://github.com/BurntSushi/ripgrep.git

# Optional larger corpora (chromium/gecko-dev require tens of GB)
# clone chromium https://github.com/chromium/chromium.git
# clone gecko-dev https://github.com/mozilla/gecko-dev.git

echo "[corpora] done: $TARGET"
