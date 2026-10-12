#!/usr/bin/env bash
# Builds llama.cpp at DigUp's pinned tag, with DigUp's one-line patch, into ./build-llama (static libs).
# Idempotent: re-running only re-applies what is missing. Works on macOS, Linux and Windows (Git Bash).
#
#   scripts/build-llama.sh                # macOS: Metal. Linux/Windows: CPU.
#   BACKEND=cuda   scripts/build-llama.sh # NVIDIA
#   BACKEND=vulkan scripts/build-llama.sh # AMD/Intel/NVIDIA via Vulkan
#   BACKEND=hip    scripts/build-llama.sh # AMD ROCm
#   NATIVE=0 scripts/build-llama.sh       # portable CPU baseline (use for binaries you ship)
#
# Mirrors DigUp's scripts/build-llama.sh (reference/digup-swift/scripts/build-llama.sh); keep the TAG in sync with it.
set -euo pipefail
cd "$(dirname "$0")/.."

TAG="${TAG:-b11461}"
SRC="${SRC:-vendor/llama.cpp}"
BUILD="${BUILD:-build-llama}"
PATCH="patches/llama.cpp-no-logits.patch"
PATCH_ABS="$PWD/$PATCH"   # absolute: `git -C "$SRC" apply` resolves relative paths against $SRC
WIN_ENV="${OS:-}"         # Windows sets OS=Windows_NT; read before OS is overwritten below
OS="$(uname -s)"
BACKEND="${BACKEND:-auto}"

if [[ ! -d "$SRC/.git" ]]; then
  mkdir -p "$(dirname "$SRC")"
  git -c advice.detachedHead=false clone --quiet --depth 1 --branch "$TAG" https://github.com/ggml-org/llama.cpp "$SRC"
fi
[[ "$(git -C "$SRC" describe --tags --exact-match 2>/dev/null || true)" == "$TAG" ]] || { echo "error: $SRC is not at $TAG" >&2; exit 1; }

# The patch stops llama.cpp reserving a 1 MiB logits buffer per token for an embeddings-only model.
if git -C "$SRC" apply --check "$PATCH_ABS" 2>/dev/null; then
  git -C "$SRC" apply "$PATCH_ABS"
elif ! git -C "$SRC" apply --check --reverse "$PATCH_ABS" 2>/dev/null; then
  echo "error: $PATCH neither applies nor is already applied" >&2; exit 1
fi

FLAGS=(-DCMAKE_BUILD_TYPE=Release -DBUILD_SHARED_LIBS=OFF -DLLAMA_BUILD_MTMD=ON
       -DLLAMA_BUILD_COMMON=OFF -DLLAMA_BUILD_TOOLS=OFF -DLLAMA_BUILD_SERVER=OFF -DLLAMA_BUILD_EXAMPLES=OFF
       -DLLAMA_BUILD_TESTS=OFF -DLLAMA_BUILD_APP=OFF -DLLAMA_BUILD_UI=OFF -DLLAMA_OPENSSL=OFF)

if [[ "$BACKEND" == auto ]]; then [[ "$OS" == Darwin ]] && BACKEND=metal || BACKEND=cpu; fi
case "$BACKEND" in
  metal)  FLAGS+=(-DGGML_METAL=ON -DGGML_METAL_EMBED_LIBRARY=ON -DCMAKE_OSX_DEPLOYMENT_TARGET=14.0) ;;
  cuda)   FLAGS+=(-DGGML_CUDA=ON) ;;
  vulkan) FLAGS+=(-DGGML_VULKAN=ON) ;;
  hip)    FLAGS+=(-DGGML_HIP=ON) ;;
  cpu)    ;;
  *) echo "unknown BACKEND=$BACKEND" >&2; exit 1 ;;
esac
# Native CPU tuning is great for local runs but crashes on older CPUs if you ship the binary.
# Shipped binaries must be built with NATIVE=0 (the portable baseline). CI sets NATIVE=0.
if [[ "$OS" == Darwin ]]; then NATIVE="${NATIVE:-0}"; else NATIVE="${NATIVE:-1}"; fi
FLAGS+=(-DGGML_NATIVE=$([[ "$NATIVE" == 1 ]] && echo ON || echo OFF))
# OpenMP on Windows (MSVC vcomp / MinGW libgomp) would need an extra link step digup-llama-sys does not do; build without it.
case "$OS" in MINGW*|MSYS*|CYGWIN*) IS_WIN=1 ;; *) IS_WIN=0 ;; esac
[[ "$WIN_ENV" == Windows_NT ]] && IS_WIN=1
[[ "$IS_WIN" == 1 ]] && FLAGS+=(-DGGML_OPENMP=OFF)

JOBS="$( (nproc || sysctl -n hw.ncpu || echo 4) 2>/dev/null | head -1)"
echo "llama.cpp $TAG backend=$BACKEND native=$NATIVE jobs=$JOBS -> $BUILD"
cmake -S "$SRC" -B "$BUILD" "${FLAGS[@]}" > "$BUILD.configure.log" 2>&1 || { tail -30 "$BUILD.configure.log"; exit 1; }
cmake --build "$BUILD" --config Release -j "$JOBS" --target llama mtmd
echo "done. Static libs are under $BUILD (cargo finds them via digup-llama-sys/build.rs)."
