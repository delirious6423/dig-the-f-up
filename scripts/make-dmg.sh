#!/usr/bin/env bash
# Packs the built macOS app into a drag-to-install disk image: the app plus an Applications shortcut.
# Usage: scripts/make-dmg.sh [app-path] [out.dmg]
# Defaults: app/target/release/bundle/macos/Dig-the-f-up.app -> app/target/release/bundle/dmg/Dig-the-f-up_<version>_aarch64.dmg
# Uses hdiutil only (no Finder scripting, unlike Tauri's dmg bundler), so it also works on CI runners.
set -euo pipefail
cd "$(dirname "$0")/.."
app="${1:-app/target/release/bundle/macos/Dig-the-f-up.app}"
version="$(sed -n 's/^  *"version": *"\([^"]*\)".*/\1/p' app/tauri.conf.json | head -1)"
out="${2:-app/target/release/bundle/dmg/Dig-the-f-up_${version}_aarch64.dmg}"
[[ -d "$app" ]] || { echo "error: $app not found (build it with: cd app && cargo tauri build --bundles app)" >&2; exit 1; }
stage="$(mktemp -d)"
trap 'rm -rf "$stage"' EXIT
cp -R "$app" "$stage/"
ln -s /Applications "$stage/Applications"
mkdir -p "$(dirname "$out")"
hdiutil create -quiet -volname "Dig-the-f-up" -srcfolder "$stage" -ov -format UDZO "$out"
echo "$out"
