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

## D1. Rust pipeline, first end-to-end run (2026-10-11) — INCOMPLETE (no PDF, video frames or OCR yet)

Machine/corpus as A2 (M4, 16 GB; `synthetic/` + `traps/`). Command: `digup index --index baseline/rs-index ~/DigUpTestbed/synthetic ~/DigUpTestbed/traps`
(release build, Metal, n=1). Not comparable to Swift's 38.8 s yet: the 5 PDFs, 2 videos and screenshot OCR are not done.

| measure | Rust | Swift (A2) |
|---|---|---|
| first pass, 88 files (72 done, 7 skipped, 7 failed = 5 pdf + 2 video not implemented yet) | 21.0 s | 38.8 s (all files) |
| long-file backfill (2 text docs) | 4.9 s | included above |
| process wall / peak RSS | 32.5 s incl. model load and backfill / 1.21 GB | n/a / 1.42 GB |
| vectors / segments | 166 / 311 | 302 / 457 |
| search "a penguin" (cold CLI) | encoder 0.60 s incl. start-up, scan 1 ms | 0.45 s, 1 ms |

Eval on that index (expected to trail Swift until PDF/video/OCR land; ALL R@1 / R@5 / R@10 / MRR):
synthetic 37 q: 0.65 / 0.73 / 0.73 / 0.68 (Swift 0.95 / 1.00 / 1.00 / 0.97);
multilingual 48 q: 0.62 / 0.69 / 0.69 / 0.66 (Swift 0.92 / 0.98 / 0.98 / 0.94).
The misses are the pdf-page, video-moment, ar/bn-pdf tags (0.00 by construction) and OCR'd screenshot exact-matches.

## D2. Eval quality after PDF / video / OCR / RTF-charset fixes (2026-10-11)

Index: `digup index` of `synthetic/` + `traps/` (M4, n=1). First pass 24.4 s + long-file backfill 7.3 s (Swift: 38.8 s for both).
ALL rows, R@1 / R@5 / R@10 / MRR: synthetic 0.95 / 0.97 / 0.97 / 0.96 (Swift 0.95 / 1.00 / 1.00 / 0.97);
multilingual 0.88 / 0.98 / 0.98 / 0.92 (Swift 0.92 / 0.98 / 0.98 / 0.94). Code eval not run yet (needs `--code` index).
Bug found by the multilingual eval: RTF `\'hh` bytes were always decoded as cp1252; now the font's `\fcharset` / `\ansicpg` is honoured.

Chinese (Simplified + Traditional), `evals/chinese.json`, corpus from `scripts/make-zh-testbed.sh` indexed on top of the synthetic testbed:
13 queries, R@1 0.92 / R@5 1.00 / MRR 0.96. Cross-script (simp to trad) and English to Chinese all rank first. One query
("支付", a word inside a sentence) ranks second: there is no word segmentation (same as Swift), so keyword search matches
only whole runs of ideographs. A small corpus alone (9 files) returns nothing for most queries because no score reaches the z-gate of 2.0.

## D3. Parity with Swift reached (2026-10-11, M4, n=1)

After reading macOS's screenshot attribute (`kMDItemIsScreenCapture`, 8 of the testbed's screenshots had been indexed as plain images) the Rust CLI matches the Swift CLI's eval numbers exactly (ALL: R@1 / R@5 / R@10 / MRR):

| eval | Rust | Swift (A2) |
|---|---|---|
| synthetic (37 q) | 0.95 / 1.00 / 1.00 / 0.97 | 0.95 / 1.00 / 1.00 / 0.97 |
| multilingual (48 q) | 0.92 / 0.98 / 0.98 / 0.94 | 0.92 / 0.98 / 0.98 / 0.94 |
| code (41 q) | 1.00 / 1.00 / 1.00 / 1.00 | 1.00 / 1.00 / 1.00 / 1.00 |

Search parity on the SAME index: `scripts/compare-swift-search.py` runs all 126 eval queries through both CLIs on the Swift-built `baseline/index`: 126 of 126 return the identical top-10 order.

Index time (synthetic + traps, first pass / long-file backfill / code index of 39 files): Rust 25.1 s / 6.3 s / 1.7 s; Swift 38.8 s total / 2.72 s code. One run each, small corpus.

## D4. Embedding throughput (`cargo run --release -p digup-embed --example bench_embed`, M4 10 cores, n=1)

| workload | Metal (all layers) | CPU only (`DIGUP_GPU_LAYERS=0`) | Metal speed-up |
|---|---|---|---|
| text, 1800-char chunks, batches of 16 | 17.9 chunks/s | 5.3 chunks/s | 3.4x |
| images, budget 140 (43 testbed photos) | 4.4 images/s | 0.9 images/s | 4.9x |
| images, budget 280 | 1.9 images/s | 0.3 images/s | 6.3x |
| audio, 30 s windows | 2.2 windows/s | 0.6 windows/s | 3.7x |

Image resize (Pillow-exact bicubic, Rust) takes 8 to 11 ms per image: negligible next to the 0.22 s (budget 140) to 0.53 s (budget 280) the model needs.
Scan kernel (`bench_scan 200000`, 768-d f16): NEON 3.05 ms (100 GB/s), serial naive 123 ms; top-100 selection 0.33 ms.
The x86 AVX2 / AVX-512 scan kernels exist but are UNTESTED: Rosetta does not report AVX2 here, so they need x86 CI hardware.
(x86_64 build under Rosetta: store tests pass with the portable "lanes" kernel, 44 ms for 200k rows.)

## B2. Ranking parity with the Swift CLI (Phase B acceptance)

Setup: DigUp 0.6.0 release CLI (`DigUp.app/Contents/Helpers/digup`, arm64) indexed `~/DigUpTestbed` (synthetic, traps, zh; code index separately) into a scratch index. The Rust `digup eval` and `digup search` then ran on a copy of that same index (so vectors are identical and only search/fusion differ). M4, Metal, same GGUFs.
Result: `eval` output of `evals/synthetic.json` (37 q), `multilingual.json` (48 q) and `code.json` (41 q) is line-for-line identical after whitespace squeezing (R@1/R@5/R@10/MRR: 0.92/1.00/1.00/0.95, 0.90/0.98/0.98/0.93, 1.00 x4). `search` for three ad hoc queries gives the same hits in the same order. Known cosmetic difference: Arabic excerpt truncation can end one character earlier (grapheme vs `char` counting); the scan output of the SIMD kernel changes no ranking.
Not covered: the x86 AVX2/AVX-512 kernels compile for `x86_64-unknown-linux-gnu` (scan.rs checked in a scratch crate) but have not run.

## B3. Search at scale (`cargo run --release -p digup-search --example bench_search -- N`, Apple M4 10 cores, NEON, n=1, synthetic data)

Random unit vectors and a 2000-word synthetic vocabulary (70% text / 20% image / 10% audio segments, 1.5 segments per file), so the z-score candidate set and word frequencies are not realistic. Medians in ms.

| N | DB build | DB size | Searcher::new | scan only | vector-only search | text+vector, common word | text+vector, rare word |
|---|---|---|---|---|---|---|---|
| 100k | 1.4 s | 0.25 GB | 0.09 s | 1.7 | 3.3 | 14.1 (was 50.7) | 6.7 (was 7.5) |
| 1M | 33.7 s | 2.4 GB | 8.4-10.4 s | 17.1 | 39-46 | 138 (was 7504) | 72 (was 182) |

The first FTS implementation joined every matched row into `segments` (341k rows for the common word at 1M); `keyword_stage` now takes the top 1000 rowids by bm25 from `fts` alone and joins only the rows it walks. Rankings are unchanged: `tests/fts_equivalence.rs` (seeded 20k segments, orphan rows, both caps, Arabic/Bengali, prefixes) compares against the kept legacy SQL, and `scripts/compare-swift-search.py` still gives 126/126 identical top-10. Set `DIGUP_SEARCH_TRACE=1` for per-stage timings. Swift's search was not measured at this scale (it was not run on a 1M index), so no speed-up over Swift is claimed here.
