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

## A2. Swift baseline (DigUp 0.6.0 release CLI, llama.cpp b11461, Metal)

Machine: Apple M4, 16 GB, macOS 26.5.2. Corpus: `~/DigUpTestbed` WITHOUT the `real/` folder (it copies personal
files), so only `synthetic/` (8 screenshots, 43 images, 5 PDFs/128 pages, 12 docs, 10 audio/65 s, 1 video/18 s),
`traps/` and `code/`. Built by `baseline/bin/build-testbed.sh` (DEVELOPER_DIR=CommandLineTools, no Xcode).
Runner: `baseline/bin/run-baseline.sh`; raw output in `baseline/results/` (git-ignored). Single run each (n=1),
tiny corpus: treat as indicative, not final. Re-run on a bigger corpus before quoting speedups.

| measure | result |
|---|---|
| `estimate` (Swift's own prediction) | first pass ~22 s, plus ~19 s for deep pages of 3 long files |
| `index` synthetic+traps, first pass incl. long-file backfill | **38.79 s wall**, 88 new files, 302 vectors / 457 segments |
| peak RSS during index | 1.42 GB (peak footprint 1.09 GB) |
| `index` re-run, nothing changed | 0.07 s |
| code index (39 files) | 2.72 s wall, RSS 1.17 GB |
| search "a penguin", process start to result (3 runs) | 0.45 s wall each; query encoder 0.43-0.44 s incl. start-up; scan 1 ms (302 vectors) |
| search peak RSS | 520 MB (footprint 216 MB) |

Eval quality (`digup eval`, n, R@1 / R@5 / R@10 / MRR):
- synthetic: 37 queries, 0.95 / 1.00 / 1.00 / 0.97
- multilingual: 48 queries, 0.92 / 0.98 / 0.98 / 0.94 (weak spot: `bn->en`, R@1 0.50)
- code: 41 queries, 1.00 / 1.00 / 1.00 / 1.00

Notes: the 0.45 s per CLI search is the cold path (model load each call); the Rust port should keep the query
encoder resident. Not measured yet: real photo/PDF/video corpus, large-index scan time, CPU/GPU utilisation.
