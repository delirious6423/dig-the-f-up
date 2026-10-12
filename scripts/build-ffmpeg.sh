#!/usr/bin/env bash
# Builds Dig-the-f-up's own minimal, LGPL-only ffmpeg + ffprobe from source and places them where Tauri's
# externalBin expects them: app/binaries/dtfu-{ffmpeg,ffprobe}-<target-triple>[.exe].
# Source trees and objects live in ./build-ffmpeg (git-ignored). Release tarballs are pinned and SHA-256-checked.
# AV1 decoding comes from dav1d (BSD-2-Clause), built static into build-ffmpeg/prefix and linked into ffmpeg.
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
DAV1D_VERSION="1.5.4"   # latest 1.5.x at pin time
DAV1D_SHA256="686616b7c69eb88d44459391ab25cac13b6647a3b288835c5784e71c1514a5c5"
DAV1D_URL="https://downloads.videolan.org/pub/videolan/dav1d/$DAV1D_VERSION/dav1d-$DAV1D_VERSION.tar.xz"
WORK="build-ffmpeg"
TARBALL="$WORK/ffmpeg-$VERSION.tar.xz"
SRC="$WORK/ffmpeg-$VERSION"
OBJ="$WORK/obj"
DAV1D_TARBALL="$WORK/dav1d-$DAV1D_VERSION.tar.xz"
DAV1D_SRC="$WORK/dav1d-$DAV1D_VERSION"
DAV1D_OBJ="$WORK/dav1d-obj"
PREFIX="$ROOT/$WORK/prefix"
OUT="app/binaries"
OS="$(uname -s)"
ARCH="$(uname -m)"

TRIPLE="$(rustc -vV | sed -n 's/^host: //p')"
EXE=""
case "$OS" in MINGW*|MSYS*|CYGWIN*) EXE=".exe" ;; esac
# Prefixed names: a Linux .deb installs sidecars into /usr/bin, where plain "ffmpeg" would clash with the distro package.
FF_OUT="$OUT/dtfu-ffmpeg-$TRIPLE$EXE"
FP_OUT="$OUT/dtfu-ffprobe-$TRIPLE$EXE"

if [[ -f "$FF_OUT" && -f "$FP_OUT" && "$FF_OUT" -nt "$SELF" && "$FP_OUT" -nt "$SELF" ]]; then
  echo "note: $FF_OUT and $FP_OUT are newer than $SELF; skipping rebuild (delete them to force one)."
  exit 0
fi

sha256_of() {
  if command -v sha256sum >/dev/null 2>&1; then sha256sum "$1" | cut -d' ' -f1
  else shasum -a 256 "$1" | cut -d' ' -f1; fi
}

# 0. Check build tools before downloading anything.
PKG_CONFIG_BIN=""
for p in pkg-config pkgconf; do
  if command -v "$p" >/dev/null 2>&1; then PKG_CONFIG_BIN="$p"; break; fi
done
MISSING=()
command -v meson >/dev/null 2>&1 || MISSING+=(meson)
command -v ninja >/dev/null 2>&1 || MISSING+=(ninja)
[[ -n "$PKG_CONFIG_BIN" ]] || MISSING+=(pkg-config)
NASM_NEEDED=0
case "$ARCH" in x86_64|amd64) NASM_NEEDED=1 ;; esac   # dav1d's x86 SIMD needs nasm; arm64 does not
if [[ $NASM_NEEDED == 1 ]] && ! command -v nasm >/dev/null 2>&1; then MISSING+=(nasm); fi
if (( ${#MISSING[@]} )); then
  case "$OS" in
    Darwin) INSTALL="brew install meson ninja pkgconf" ;;
    MINGW*|MSYS*|CYGWIN*) INSTALL="pacman -S mingw-w64-x86_64-meson mingw-w64-x86_64-ninja mingw-w64-x86_64-pkgconf" ;;
    *) INSTALL="sudo apt install meson ninja-build pkg-config" ;;
  esac
  # nasm is a plain package name on every platform, not a MINGW-prefixed one.
  for t in "${MISSING[@]}"; do [[ $t == nasm ]] && INSTALL="$INSTALL nasm"; done
  echo "error: missing build tool(s): ${MISSING[*]}" >&2
  echo "install with: $INSTALL" >&2
  exit 1
fi

# 1. Download (if needed) and verify the pinned tarballs, then extract them.
mkdir -p "$WORK"
fetch() {   # fetch <url> <tarball> <sha256>
  if [[ ! -f "$2" || "$(sha256_of "$2")" != "$3" ]]; then
    echo "Downloading $1"
    curl -fL --retry 3 -o "$2.part" "$1"
    mv "$2.part" "$2"
  fi
  if [[ "$(sha256_of "$2")" != "$3" ]]; then
    echo "error: SHA-256 mismatch for $2 (expected $3); deleting it" >&2
    rm -f "$2"; exit 1
  fi
}
fetch "$URL" "$TARBALL" "$SHA256"
fetch "$DAV1D_URL" "$DAV1D_TARBALL" "$DAV1D_SHA256"
if [[ ! -f "$SRC/configure" ]]; then
  tar -xJf "$TARBALL" -C "$WORK"
fi
if [[ ! -f "$DAV1D_SRC/meson.build" ]]; then
  tar -xJf "$DAV1D_TARBALL" -C "$WORK"
fi

# 2. Build dav1d statically into $PREFIX (FFmpeg finds it through pkg-config below).
rm -rf "$DAV1D_OBJ" "$PREFIX"
echo "Building dav1d $DAV1D_VERSION (log: $DAV1D_OBJ.log)"
if ! ( meson setup "$DAV1D_OBJ" "$ROOT/$DAV1D_SRC" --prefix="$PREFIX" --libdir=lib --buildtype=release \
         --default-library=static -Denable_tools=false -Denable_tests=false > "$DAV1D_OBJ.log" 2>&1 \
    && ninja -C "$DAV1D_OBJ" install >> "$DAV1D_OBJ.log" 2>&1 ); then
  tail -40 "$DAV1D_OBJ.log" >&2
  echo "error: dav1d build failed (full log: $DAV1D_OBJ.log)" >&2
  exit 1
fi

# 3. Configure: nothing auto-detected, LGPL only, only what the app invokes.
FLAGS=(
  --disable-gpl --disable-nonfree --disable-version3
  --disable-autodetect --disable-doc --disable-network --disable-debug --disable-ffplay
  --enable-ffmpeg --enable-ffprobe --disable-shared --enable-static
  --disable-everything
  --enable-decoders --enable-demuxers --enable-parsers --enable-bsfs
  --enable-protocol=file,pipe
  --enable-filter=fps,scale,format,null,aformat,anull,trim,atrim,setpts
  --enable-encoder=ppm --enable-muxer=image2pipe
  --enable-libdav1d --enable-decoder=libdav1d
  --pkg-config="$PKG_CONFIG_BIN" --pkg-config-flags=--static
)
case "$OS" in
  MINGW*|MSYS*|CYGWIN*) FLAGS+=(--extra-ldflags=-static) ;;
esac

rm -rf "$OBJ"
mkdir -p "$OBJ"
echo "Configuring FFmpeg $VERSION in $OBJ"
if ! (cd "$OBJ" && PKG_CONFIG_PATH="$PREFIX/lib/pkgconfig" "$ROOT/$SRC/configure" "${FLAGS[@]}" > configure.log 2>&1); then
  tail -40 "$OBJ/configure.log" >&2
  echo "error: configure failed (full log: $OBJ/configure.log)" >&2
  exit 1
fi

# 4. Build and copy out. Strip the binaries.
JOBS="${JOBS:-$(getconf _NPROCESSORS_ONLN 2>/dev/null || echo 2)}"
echo "Building ffmpeg and ffprobe with -j$JOBS (log: $OBJ/make.log)"
if ! make -C "$OBJ" -j"$JOBS" "ffmpeg$EXE" "ffprobe$EXE" > "$OBJ/make.log" 2>&1; then
  tail -40 "$OBJ/make.log" >&2
  echo "error: build failed (full log: $OBJ/make.log)" >&2
  exit 1
fi

mkdir -p "$OUT"
cp "$OBJ/ffmpeg$EXE" "$FF_OUT"
cp "$OBJ/ffprobe$EXE" "$FP_OUT"
strip "$FF_OUT" "$FP_OUT"

# 5. Report.
echo "Built:"
for f in "$FF_OUT" "$FP_OUT"; do
  printf '  %s  (%s bytes)\n' "$f" "$(wc -c < "$f" | tr -d ' ')"
done
echo "  $("$FF_OUT" -version 2>/dev/null | sed -n 1p)"
