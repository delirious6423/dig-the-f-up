#!/usr/bin/env bash
# Links the two EmbeddingGemma 2 GGUF files into ./models, from wherever they already are.
# Verified file names/sizes/SHA-256 come from DigUp's ModelFiles.swift (pinned HF revision bfcd298...).
set -euo pipefail
cd "$(dirname "$0")/.."
mkdir -p models
REV=bfcd298762cc34d0357ece5ebdd31791a3a374d8
# bash 3.2 (the macOS default) has no associative arrays, so use "name:sha" pairs.
PAIRS="embeddinggemma-2-Q8_0.gguf:2188ac1deca4b77dffefd603c2776a9d76d9d74ec01841392982ebb840b09135
mmproj-embeddinggemma-2-Q8_0.gguf:c4a8a52691ecef40618438928bdf9e68379b854e24166f292592353db0aab64f"
for pair in $PAIRS; do
  f="${pair%%:*}"; want="${pair##*:}"
  dest="models/$f"
  if [[ ! -e "$dest" ]]; then
    for d in "$HOME/Downloads" "$HOME/Library/Application Support/DigUp/Models" \
             "$HOME/.cache/huggingface/hub/models--ggml-org--embeddinggemma-2-GGUF/snapshots/$REV"; do
      [[ -f "$d/$f" ]] && { ln -sf "$d/$f" "$dest"; break; }
    done
  fi
  if [[ ! -e "$dest" ]]; then
    echo "missing $f. Download: https://huggingface.co/ggml-org/embeddinggemma-2-GGUF/resolve/$REV/$f" >&2; exit 1
  fi
  got="$( (shasum -a 256 "$dest" 2>/dev/null || sha256sum "$dest") | cut -d' ' -f1)"
  [[ "$got" == "$want" ]] && echo "ok  $f" || { echo "BAD checksum: $f" >&2; exit 1; }
done
