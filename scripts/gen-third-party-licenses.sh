#!/usr/bin/env bash
# Lists every Rust crate in the desktop app's dependency graph with its version and declared license.
#
# Usage: scripts/gen-third-party-licenses.sh [output-file]
#   Without an argument the table is printed to stdout. Nothing is installed or downloaded.
#
# This is a first-pass inventory from `cargo metadata` (the Cargo.lock must already be present). It does not read
# license texts, so a crate that declares no license is flagged and needs a manual check. Hand-written component
# notes (llama.cpp, Apple frameworks, EmbeddingGemma) live in docs/THIRD-PARTY.md.
#
# Optional, stricter tool: cargo-about. Not required. If you want it:
#   cargo install cargo-about --locked      (installs into ~/.cargo/bin; run it yourself)
set -euo pipefail

repo="$(cd "$(dirname "$0")/.." && pwd)"
out="${1:-}"

if ! command -v cargo >/dev/null 2>&1; then
  echo "error: cargo not found on PATH" >&2
  exit 1
fi
if ! command -v python3 >/dev/null 2>&1; then
  echo "error: python3 not found on PATH (used to read cargo metadata JSON)" >&2
  exit 1
fi

meta="$(cargo metadata --format-version 1 --locked --manifest-path "$repo/app/Cargo.toml")"

render() {
  printf '%s' "$meta" | python3 -I -c '
import json, sys
meta = json.load(sys.stdin)
pkgs = {p["id"]: p for p in meta["packages"]}
# Skip this project: workspace members and local path dependencies have no registry or git "source".
rows = []
for p in meta["packages"]:
    if p.get("source") is None:
        continue
    lic = p.get("license") or ""
    if not lic and p.get("license_file"):
        lic = "(see license file)"
    if not lic:
        lic = "UNDECLARED - check manually"
    rows.append((p["name"], p["version"], lic, p.get("repository") or ""))
rows.sort(key=lambda r: (r[0].lower(), r[1]))
print("| Crate | Version | License (as declared) | Repository |")
print("| --- | --- | --- | --- |")
for name, ver, lic, repo in rows:
    print(f"| {name} | {ver} | {lic} | {repo} |")
print()
print(f"{len(rows)} third-party crates. Generated from cargo metadata; verify before publishing a release.")
'
}

if [[ -n "$out" ]]; then
  render > "$out"
  echo "wrote $out" >&2
else
  render
fi
