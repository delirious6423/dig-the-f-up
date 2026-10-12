#!/usr/bin/env bash
# Fetches the pinned prebuilt pdfium library (bblanchon/pdfium-binaries) for this platform into app/pdfium/, where
# the Windows and Linux release configs (app/tauri.release.*.conf.json) bundle it from. macOS reads PDFs with PDFKit
# and needs nothing. The archive is pinned and SHA-256-checked.
#
#   scripts/fetch-pdfium.sh
set -euo pipefail
cd "$(dirname "$0")/.."

RELEASE="chromium/8066"   # published 2026-09-21
BASE="https://github.com/bblanchon/pdfium-binaries/releases/download/$RELEASE"
OUT="app/pdfium"

case "$(uname -s)" in
  Linux)
    ARCHIVE="pdfium-linux-x64.tgz"; SHA256="0b43f405477cf2cfc4dbff06905093c3309756c6bca1fb9da99234a2ca97fed2"
    LIB="lib/libpdfium.so" ;;
  MINGW*|MSYS*|CYGWIN*)
    ARCHIVE="pdfium-win-x64.tgz"; SHA256="739a57d597d864297909cc40a2411eba728490c76a0fa25e3ea299c7f6b07020"
    LIB="bin/pdfium.dll" ;;
  Darwin) echo "macOS reads PDFs with PDFKit; no pdfium needed."; exit 0 ;;
  *) echo "error: unsupported platform $(uname -s)" >&2; exit 1 ;;
esac

sha256_of() {
  if command -v sha256sum >/dev/null 2>&1; then sha256sum "$1" | cut -d' ' -f1
  else shasum -a 256 "$1" | cut -d' ' -f1; fi
}

work="$(mktemp -d)"
trap 'rm -rf "$work"' EXIT
curl -fL --retry 3 -o "$work/$ARCHIVE" "$BASE/$ARCHIVE"
if [[ "$(sha256_of "$work/$ARCHIVE")" != "$SHA256" ]]; then
  echo "error: SHA-256 mismatch for $ARCHIVE (expected $SHA256)" >&2
  exit 1
fi
tar -xzf "$work/$ARCHIVE" -C "$work" "$LIB" LICENSE
mkdir -p "$OUT"
cp "$work/$LIB" "$OUT/"
cp "$work/LICENSE" "$OUT/LICENSE-pdfium.txt"
echo "pdfium $RELEASE: $OUT/$(basename "$LIB")"
