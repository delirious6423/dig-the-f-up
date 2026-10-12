# Benchmarks

All numbers were measured on one machine: Apple M4 (10 cores), 16 GB, macOS 26, Metal, release builds. Most rows are
single runs (n=1) on small or synthetic corpora, so treat them as indicative. The Swift reference is the DigUp 0.6.0
release CLI, using the same llama.cpp (b11461) and the same model files. Nothing on Windows, Linux or x86 has been
measured yet ([#1](https://github.com/delirious6423/dig-the-f-up/issues/1), [#3](https://github.com/delirious6423/dig-the-f-up/issues/3)).

## Same results as the original

- **Embeddings.** The parity test checks nine reference vectors from the Swift app (text, Arabic and Bengali queries,
  long document, photo, screenshot, video frame, speech). Every vector reaches cosine >= 0.999999.
  Command: `cargo test -p dtfu-embed --release --test parity -- --nocapture`.
- **Eval scores.** On DigUp's eval sets the results are equal. Synthetic (37 queries): R@1 0.95, R@5 1.00, MRR 0.97.
  Multilingual (48): 0.92, 0.98, 0.94. Code (41): 1.00 across the board.
- **Ranking.** Run on the same Swift-built index, all 126 eval queries return the identical top 10 through both CLIs
  (`scripts/compare-swift-search.py`).

## Indexing (88-file test corpus: screenshots, photos, PDFs, docs, audio, video)

| | Dig-the-f-up | Swift DigUp |
|---|---|---|
| first index, complete (first pass + long-file backfill) | 31.4 s (25.1 + 6.3) | 38.8 s |
| code index, 39 files | 1.7 s | 2.7 s |
| cold CLI query, process start to result | 0.60 s | 0.45 s |

The cold query is slower ([#11](https://github.com/delirious6423/dig-the-f-up/issues/11)). Almost all of it is model
load, which the desktop app avoids by keeping the model resident.

Embedding throughput, Metal against CPU only (`cargo run --release -p dtfu-embed --example bench_embed`):

| workload | Metal | CPU only | speed-up |
|---|---|---|---|
| text, 1800-char chunks | 17.9 chunks/s | 5.3 chunks/s | 3.4x |
| images, 140-token budget | 4.4 images/s | 0.9 images/s | 4.9x |
| images, 280-token budget | 1.9 images/s | 0.3 images/s | 6.3x |
| audio, 30 s windows | 2.2 windows/s | 0.6 windows/s | 3.7x |

## Search at scale (synthetic index, medians)

`cargo run --release -p dtfu-search --example bench_search -- N`. The vectors are random and the text uses a
2000-word vocabulary.

| segments | vector scan | vector-only search | text + vector, common word | text + vector, rare word |
|---|---|---|---|---|
| 100k | 1.7 ms | 3.3 ms | 14.1 ms | 6.7 ms |
| 1M | 17.1 ms | 39-46 ms | 138 ms (was 7.5 s) | 72 ms |

- **Scan kernel.** NEON reads 200k vectors in 3.05 ms (about 100 GB/s), against 123 ms for the naive loop
  (`cargo run --release -p dtfu-store --example bench_scan -- 200000`).
- **Keyword stage.** It takes the top 1000 rows from the full-text index before joining, which makes it 54x faster at
  1M with identical rankings. `crates/dtfu-search/tests/fts_equivalence.rs` checks this against the old query.
- **Resident searcher** (`--example resident_bench`). Keeping the loaded index in memory cuts a 100k query from 155 ms
  to 13 ms (p50). After a commit, an incremental refresh costs 18 ms against 93 ms for a full reload at 100k, and
  0.2-0.3 s against 2-3 s at 1M. Results are identical in every case. The 1M numbers vary 2-3x between runs on this
  16 GB machine.

## OCR (19 synthetic images, `evals/ocr/corpus`)

| engine | median per image | character error rate | peak memory |
|---|---|---|---|
| Apple Vision (default on macOS) | 0.07 s | 0.050 | 262 MB |
| LightOnOCR-3 0.8B (optional, feature `lighton`) | 1.48 s | 0.008 | 1.81 GB |
| ocrs 0.13 (default on Windows and Linux) | 0.13 s* | 0.183 (Latin-script images: 0.078) | not measured |

ocrs reads Latin script only: the Arabic, Russian and Chinese images score ~1.0. On the other 15 it is at or under
0.01 on ordinary text and weakest on tiny text and tables (0.14-0.33). *Measured on the Mac (`--features ocrs`)
under load (`OCR_BENCH_NOWAIT=1`), so its time is not final; its error rate is.

Most of LightOn's lead comes from two tiny-text images. The corpus is rendered on the same Mac, which may favour
Vision. Retrieval on the eval set is identical with either engine. Command:
`cargo run --release -p dtfu-ocr --features lighton --example ocr_bench -- evals/ocr/corpus 5 vision,lighton`.
