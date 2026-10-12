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

## R1. Resident Searcher vs per-query build (2026-10-12, M4 10 cores, n=1 per size, synthetic data)

Machine: `sysctl -n hw.model hw.memsize` gives `Mac16,3` and `17179869184` (16 GB); chip Apple M4, 10 cores, macOS 26.
Build: `--release`, NEON. Rayon threads 10.

Commands (same binary, one run per size):
```bash
cargo run --release -q -p digup-search --example resident_bench -- 100000  --db <scratch>/resident-100k.sqlite --churn 30
cargo run --release -q -p digup-search --example resident_bench -- 1000000 --db <scratch>/resident-1m.sqlite  --churn 30
```
The synthetic DBs were deleted after each run. The DB is 70% text, 20% image, 10% audio, f16 768-d vectors.
Fresh = `IndexStore::open` + `Searcher::new` per query (what the old engine did). Cached = one `OwnedSearcher` kept
across queries, reloaded only when `is_current()` is false. Query text and vector are fixed; 200 queries per phase.

Load gate: each phase starts only when the 1-minute load is below 4 (wait up to 25 min). Load labels are from the
START/END lines of the logs. Results marked LOADED are **not final**.

| Phase | 100k load (start -> end) | 100k result | 1M load (start -> end) | 1M result |
|---|---|---|---|---|
| build | 17.9 -> 17.9 (LOADED) | 1.59 s, 0.246 GB | 1.62 -> 1.69 (ok) | 37.2 s, 2.449 GB |
| 1. fresh per query (n=20) | 3.59 -> 3.46 (ok) | open+load p50 140.5 ms, p95 156.2; search p50 15.3, p95 23.3; total p50 155.4, p95 172.4 ms | 1.72 -> 4.46 (LOADED) | open+load p50 5595 ms, p95 8254; search p50 227, p95 384; total p50 5652, p95 8722 ms |
| 2. cached initial load | 3.46 (ok) | 141.7 ms | 3.93 (ok) | 2977 ms |
| 2. cached first query | 3.46 (ok) | 17.7 ms | 3.93 -> 3.64 (ok) | 145 ms |
| 2. cached query (n=200) | 3.46 (ok) | p50 13.3 ms, p95 19.0 ms | 3.93 -> 3.64 (ok) | p50 74.8 ms, p95 130.1 ms |
| 3. is_current (n=1000) | 3.46 (ok) | p50 1.0 us, p95 1.0 us | 3.64 (ok) | p50 1.1 us, p95 1.2 us |
| 4. identity, cached vs fresh (50 queries) | 3.46 -> 4.23 (LOADED) | 0 mismatches, 50/50 non-empty | 3.64 -> 4.49 (LOADED) | 0 mismatches, 50/50 non-empty |
| 5. rebuild detect after commit by another connection (n=5) | 3.66 (ok) | 0.0 us p50 | 3.93 -> 2.65 (ok) | 0.1 us p50 |
| 5. rebuild (open + load) after commit (n=5) | 3.66 (ok) | p50 102.4 ms, p95 125.3 ms | 3.93 -> 2.65 (ok) | p50 13186 ms, p95 20731 ms |
| 5. query right after rebuild (n=5) | 3.66 (ok) | p50 16.3 ms, p95 18.8 ms | 3.93 -> 2.65 (ok) | p50 570 ms, p95 1149 ms |
| 6. churn: 1 segment committed per second for 30 s | 3.66 -> 2.56 (ok) | 30 commits, 199 queries, 30 rebuilds. No-rebuild query p50 26.7 ms, p95 36.6 ms (n=169). Query incl. rebuild p50 136.6 ms, p95 196.0 ms (n=30). Rebuild only p50 120.5 ms, p95 183.7 ms | 2.65 -> 4.22 (LOADED) | 30 commits, 15 queries, 14 rebuilds. Query incl. rebuild p50 1430 ms, p95 8591 ms (n=14). Rebuild only p50 1376 ms, p95 8479 ms |

Notes:
- Phase 5 at 1M (rebuild p50 13.2 s) and the churn rebuild at 1M (p50 1.4 s) differ by about 10x. Both ran at load
  2.6 to 4 on a 16 GB machine with other apps running, so the spread is probably memory/IO pressure. It is not explained
  by this run and is not a final number.
- Churn at 1M completed only 15 queries in 30 s because each rebuild blocks the query path.

Verdict:
- With the index idle, the resident Searcher removes the per-query open+load cost. At 100k that is about 140 ms of a
  155 ms fresh query (p50), so cached queries are about 12x faster (13.3 ms p50). At 1M the same gap is about 5.6 s
  (fresh, loaded) against 75 ms cached p50. The fresh 1M figure is LOADED, so treat the ratio as indicative only.
- Results are identical: 0 mismatches over 50 queries at both sizes.
- `is_current()` costs about 1 us, so checking it per query is free.
- **Rebuild-on-every-commit during active indexing is unchanged.** Each commit by another connection still forces a
  full reload (100k about 120 ms; 1M about 1.4 s p50 but with a long tail under load). The cache does not help while
  files are being indexed. It only helps between commits. At 1M the churn test shows that rebuilds dominate the
  query path during active indexing. Incremental Searcher updates are the only fix for that case (not done here).
- Estimates: none. All numbers above are measured on this machine, once per size.

## R1b. Incremental `Searcher::refresh` vs full rebuild (2026-10-12, M4 10 cores, 16 GB, synthetic data)

What changed: `Searcher::refresh()` (used by `Inner::with_searcher` when `is_current()` is false) reads every file's
fingerprint plus the files with vectors past the last loaded id, merges only the dirty files' rows with the loaded
ones, appends the new rows, drops the removed ones with an order-preserving compaction and recomputes slots, so
results equal a fresh `Searcher`. It falls back to a full load when more than 25% of rows are removed or
re-evaluated, and every 500 refreshes. Why not scan `(id, file)` of all segments: measured 45 ms at 100k rows
(it touches every vector page), about the cost it was meant to avoid.

Correctness: `crates/digup-search/tests/refresh_equivalence.rs`, 240 random mutation steps (1 to 3 mutations each: add
file, delete file, re-embed with reused segment ids, later steps of a long pdf/doc with `info.pages` > 50 so the scale
flags change, kind change, info-only change), 30 queries per step (vector-only, with text, kinds filter, lowered
z gate) against a fresh `Searcher`, two refreshing searchers (default and never-fall-back): 7200 queries each,
34398 hits compared bitwise (f64 score, z, cosine bits, segment, keyword rank), 0 mismatches. Disabling the
rewritten-file vector check makes it fail at step 71, so the test does reach the id-reuse case.

Command (same binary; new vectors committed by another connection in one transaction, then refresh and a full
open+load of the same state are timed, then 50 queries compared by `Debug` output):
```bash
cargo build --release -p digup-search --example resident_bench
target/release/examples/resident_bench 100000  --only-refresh --db <scratch>/r100k.sqlite
target/release/examples/resident_bench 1000000 --only-refresh --db <scratch>/r1m.sqlite
```
DBs deleted afterwards. Load1 was 1.6 to 1.9 at the start of every row except where noted (gate: below 4).

| Index | New segments | refresh | full rebuild | ratio | identity |
|---|---|---|---|---|---|
| 100k | 1,000 | 17.6 ms | 92.8 ms | 5.3x | 0/50 mismatches |
| 100k | 10,000 | 44.7 ms | 102.3 ms | 2.3x | 0/50 |
| 1M | 1,000 | 293 ms (run 2: 181 ms) | 3196 ms (run 2: 1859 ms) | 10.9x (10.3x) | 0/50 |
| 1M | 10,000 | 1591 ms (run 2: 543 ms; load 3.9, near the gate) | 3148 ms (run 2: 2800 ms) | 2.0x (5.2x) | 0/50 |

Where the time goes: the fixed part is the scan of the `files` table for fingerprints (about 10 ms at 100k with
70k files, 100 to 270 ms at 1M with about 700k files) and the slot recompute (2 ms at 100k, 20 to 60 ms at 1M);
decoding the new blobs is about 2 us each.

Verdict (honest): refresh is a clear win for the case it was built for, a few segments committed at a time
while indexing: about 18 ms vs 93 ms at 100k and about 0.2 to 0.3 s vs 2 to 3 s at 1M, with identical results. The
gain shrinks as the batch grows (10k new segments at 1M: 2x to 5x) and a refresh never beats a full rebuild by
more than about 10x because the files scan and slot rebuild remain. The full rebuild here (1.9 to 3.2 s at 1M) is much
faster than the 13 s of R1 phase 5, which ran under memory pressure; 1M numbers on this shared 16 GB box vary 2x to
3x between runs, so the 1M rows are indicative, not final. The scan of the files table and the slot recompute could be
made incremental if 0.2 s per commit at 1M turns out to matter. The path relies on the indexer's invariant that any
delete or rewrite of a file's segments also rewrites or deletes its `files` row (checked in `digup-store/src/index.rs`);
the 500-refresh full reload bounds the damage if that is ever violated. No estimates in this section.

Caveats added after review:
- The refresh benchmark measures only inserts of new files with NULL `info` (the best case): no removed rows, no
  re-evaluated rows, no rewritten files. Removal and rewrite paths are covered for correctness, not timed.
- Fingerprint contract: every writer that inserts, deletes or replaces a file's segments or vectors must change that
  file's `files` row (`state`, `indexed_at` or `info`) in the same transaction, or delete the file. It is documented on
  `Searcher::refresh` and at `IndexStore::add_segments`; `add_segments` now bumps `indexed_at`. The fingerprint also
  covers path, size and mtime. A writer that breaks the contract leaves the cached searcher stale until the full
  reload after 500 refreshes (potentially a long time at a slow commit rate).
- Regression tests: `refresh_id_reuse.rs` (deterministic: the top file deleted, ids reused, rows added elsewhere; it
  fails with `add_segments` not bumping) and mutation `delete-top` in `refresh_equivalence.rs` (fails if its touch is removed).
