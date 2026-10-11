# Benchmarks

## A1. Parity on Apple Silicon (Metal), 2026-10-11

Machine: owner's Apple Silicon Mac (arm64, macOS 26 / Darwin 25.5), llama.cpp b11461 built with Metal
(`scripts/build-llama.sh`, deployment target 14.0).
Command: `cargo test --workspace --release` (parity: `-p digup-embed --release --test parity -- --nocapture`).

Cosine vs the Swift/Metal reference vectors (threshold 0.999):

| vector | cosine |
|---|---|
| document | 1.000000 |
| document-long | 1.000000 |
| frame-70 | 1.000000 |
| photo-140 | 1.000000 |
| query | 1.000000 |
| query-ar | 1.000000 |
| query-bn | 1.000000 |
| screen-280 | 0.999999 |
| speech | 1.000000 |

Unit tests: core 2, crawl 26 (10 + 16), embed 6, store 4. Parity test: 9.8 s wall incl. model load.

macOS fixes needed to link: `build.rs` now links clang's `libclang_rt.osx.a` (for `___isPlatformVersionAtLeast`),
and `.cargo/config.toml` sets `MACOSX_DEPLOYMENT_TARGET=14.0` to match the llama.cpp build.

## A2. Swift baseline

Not yet measured.
