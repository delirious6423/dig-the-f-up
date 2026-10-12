#!/usr/bin/env bash
# Fetches the original Swift DigUp source (MIT, https://github.com/ARahim3/DigUp) at the commit every `file:line`
# citation in docs/porting/spec-*.md and in code comments points to, into reference/digup-swift/ (git-ignored, read-only).
set -euo pipefail
COMMIT=6faa29e4c315a3a591de1b41bd78751bdf39588d
DEST="$(cd "$(dirname "$0")/.." && pwd)/reference/digup-swift"
if [ -d "$DEST/.git" ]; then
    git -C "$DEST" fetch --quiet origin "$COMMIT"
else
    mkdir -p "$(dirname "$DEST")"
    git clone --quiet https://github.com/ARahim3/DigUp.git "$DEST"
fi
git -C "$DEST" -c advice.detachedHead=false checkout --quiet "$COMMIT"
echo "reference/digup-swift is at $(git -C "$DEST" rev-parse --short HEAD)"
