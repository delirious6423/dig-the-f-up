#!/usr/bin/env bash
# Builds Dig-the-f-up's own minimal, LGPL-only ffmpeg + ffprobe from source and places them where Tauri's
# externalBin expects them: app/binaries/{ffmpeg,ffprobe}-<target-triple>[.exe].
# Source tree and objects live in ./build-ffmpeg (git-ignored). The release tarball is pinned and SHA-256-checked.
#
#   scripts/build-ffmpeg.sh            # build (skipped when both outputs are newer than this script)
#   JOBS=4 scripts/build-ffmpeg.sh     # parallel jobs (default: all cores)
#
# Platforms: macOS arm64 (Xcode CLT), Linux x86_64/aarch64 (gcc), Windows via an MSYS2 MINGW64 shell (static).
# Only the components the app uses are enabled (see the configure flags below); everything else is disabled.
set -euo pipefail

SELF="$(cd "$(dirname "$0")" && pwd)/$(basename "$0")"   # absolute, before we cd
cd "$(dirname "$0")/.."
ROOT="$PWD"

VERSION="7.1.5"   # latest 7.1.x point release at pin time
SHA256="de668509caf9e35e3cd162473441fdb29538c6d96ed080292b3cf9e6fc5d558f"
URL="https://ffmpeg.org/releases/ffmpeg-$VERSION.tar.xz"
WORK="build-ffmpeg"
TARBALL="$WORK/ffmpeg-$VERSION.tar.xz"
SRC="$WORK/ffmpeg-$VERSION"
OBJ="$WORK/obj"
OUT="app/binaries"
OS="$(uname -s)"
ARCH="$(uname -m)"

TRIPLE="$(rustc -vV | sed -n 's/^host: //p')"
EXE=""
case "$OS" in MINGW*|MSYS*|CYGWIN*) EXE=".exe" ;; esac
FF_OUT="$OUT/ffmpeg-$TRIPLE$EXE"
FP_OUT="$OUT/ffprobe-$TRIPLE$EXE"

if [[ -f "$FF_OUT" && -f "$FP_OUT" && "$FF_OUT" -nt "$SELF" && "$FP_OUT" -nt "$SELF" ]]; then
  echo "note: $FF_OUT and $FP_OUT are newer than $SELF; skipping rebuild (delete them to force one)."
  exit 0
fi

sha256_of() {
  if command -v sha256sum >/dev/null 2>&1; then sha256sum "$1" | cut -d' ' -f1
  else shasum -a 256 "$1" | cut -d' ' -f1; fi
}

# 1. Download (if needed) and verify the pinned tarball, then extract it.
mkdir -p "$WORK"
if [[ ! -f "$TARBALL" || "$(sha256_of "$TARBALL")" != "$SHA256" ]]; then
  echo "Downloading $URL"
  curl -fL --retry 3 -o "$TARBALL.part" "$URL"
  mv "$TARBALL.part" "$TARBALL"
fi
if [[ "$(sha256_of "$TARBALL")" != "$SHA256" ]]; then
  echo "error: SHA-256 mismatch for $TARBALL (expected $SHA256); deleting it" >&2
  rm -f "$TARBALL"; exit 1
fi
if [[ ! -f "$SRC/configure" ]]; then
  tar -xJf "$TARBALL" -C "$WORK"
fi

# 2. Configure: nothing auto-detected, LGPL only, only what the app invokes.
FLAGS=(
  --disable-gpl --disable-nonfree --disable-version3
  --disable-autodetect --disable-doc --disable-network --disable-debug --disable-ffplay
  --enable-ffmpeg --enable-ffprobe --disable-shared --enable-static
  --disable-everything
  --enable-decoders --enable-demuxers --enable-parsers --enable-bsfs
  --enable-protocol=file,pipe
  --enable-filter=fps,scale,format,null,aformat,anull,trim,atrim,setpts
  --enable-encoder=ppm --enable-muxer=image2pipe
)
case "$ARCH" in
  x86_64|amd64)
    if ! command -v nasm >/dev/null 2>&1 && ! command -v yasm >/dev/null 2>&1; then
      echo "warning: nasm/yasm not found; configuring with --disable-x86asm. Decoding will be slower; install nasm for full speed." >&2
      FLAGS+=(--disable-x86asm)
    fi
    ;;
esac
case "$OS" in
  MINGW*|MSYS*|CYGWIN*) FLAGS+=(--extra-ldflags=-static) ;;
esac

rm -rf "$OBJ"
mkdir -p "$OBJ"
echo "Configuring FFmpeg $VERSION in $OBJ"
if ! (cd "$OBJ" && "$ROOT/$SRC/configure" "${FLAGS[@]}" > configure.log 2>&1); then
  tail -40 "$OBJ/configure.log" >&2
  echo "error: configure failed (full log: $OBJ/configure.log)" >&2
  exit 1
fi

# 3. Build and copy out. Strip the binaries.
JOBS="${JOBS:-$(getconf _NPROCESSORS_ONLN 2>/dev/null || echo 2)}"
echo "Building ffmpeg and ffprobe with -j$JOBS (log: $OBJ/make.log)"
if ! make -C "$OBJ" -j"$JOBS" ffmpeg ffprobe > "$OBJ/make.log" 2>&1; then
  tail -40 "$OBJ/make.log" >&2
  echo "error: build failed (full log: $OBJ/make.log)" >&2
  exit 1
fi

mkdir -p "$OUT"
cp "$OBJ/ffmpeg$EXE" "$FF_OUT"
cp "$OBJ/ffprobe$EXE" "$FP_OUT"
strip "$FF_OUT" "$FP_OUT"

# 4. Report.
echo "Built:"
for f in "$FF_OUT" "$FP_OUT"; do
  printf '  %s  (%s bytes)\n' "$f" "$(wc -c < "$f" | tr -d ' ')"
done
echo "  $("$FF_OUT" -version 2>/dev/null | sed -n 1p)"
