# dig-the-f-up handoff

This document hands a running project from a Claude session (cloud workspace, Linux x86, 2 cores, no GPU) to a Claude
Code session on the owner's Apple Silicon Mac. You are expected to **continue autonomously to a finished, shippable
cross-platform product**. Everything you need to understand intent, state, decisions and next steps is here.

**Kickoff prompt the owner will paste:** *"Read CLAUDE.md and docs/HANDOFF.md in full, run the bootstrap in section 6,
confirm the parity test passes on this Mac, then continue the roadmap in section 8 as a continuous autonomous agent."*

---------------------------------------------------------------------------------------------------

## 1. Mission (what the owner is trying to do)

The owner found **DigUp** (https://github.com/ARahim3/DigUp, MIT, ~15k lines of Swift, 5 commits, 7 stars): a macOS
menu-bar app that indexes a user's chosen folders (photos, screenshots, PDFs, documents, audio, video, optionally code)
into one SQLite file of 768-d vectors plus an FTS5 keyword index, using Google DeepMind's **EmbeddingGemma 2**
(740M-param multimodal embedding model, Apache 2.0, released Oct 2026) through llama.cpp on the Mac GPU. Search is
"describe it, dig it up": a query embeds in milliseconds and is matched against every vector, fused with exact-word
matches. Everything is on-device.

The owner's words: *"Can we write this in rust so it can run on all platforms not just mac. Estimate the speed up if we
write in rust and pull all levers for max performance."* Then, after seeing the plan: *"Let's kick things off"*, and now
they want to continue in Claude Code.

So the goals, in priority order:
1. **Cross-platform:** macOS (**Apple Silicon only; Intel Macs are explicitly dropped by owner decision, do not add x86_64-apple-darwin targets, CI jobs or Universal binaries**), Windows, Linux. Same behaviour, same quality.
2. **Maximum performance**, with every lever pulled, and **measured** speedups against the Swift original, not guesses.
3. **Quality parity** with DigUp: same embeddings (verified), same ranking behaviour, same eval scores.
4. A usable product: a CLI first (index, search, status, eval), then a desktop app (tray, global hotkey, results UI).
5. Keep DigUp's privacy promise: nothing leaves the machine except the one-time model download and an optional
   update check; no telemetry; never modifies user files.

Not a goal: a different model, a cloud service, or reimplementing inference in pure Rust (see decision D1).

---------------------------------------------------------------------------------------------------

## 2. How DigUp works (compressed; the specs have the detail)

```
chosen folders -> crawler -> readers (ImageIO, PDFKit, AVFoundation, Vision OCR)
               -> EmbeddingGemma 2 on llama.cpp (Metal) [text model 310 MB + mmproj image/audio encoders 555 MB]
               -> one SQLite file: segments(vec f16 x768) + fts5 keyword table      (a second file for code search)
query -> same model's text half (~250 MB, loaded only while searching) -> brute-force dot product over ALL vectors
      -> per-modality z-scores -> fuse with FTS5 bm25 word coverage -> one result per file
```
Key facts (cited with file:line in the specs):
- Index = two SQLite files (`index.sqlite`, `code.sqlite`), schema v5, WAL. Vectors are 1536-byte float16 blobs.
- Search is an **exact brute-force scan of every vector, single-threaded in Swift**, then z-score fusion. No ANN, by
  design: the z-score needs the whole score distribution. Do not introduce ANN.
- Prompts: query `task: search result | query: {text}` (code: `task: code retrieval | query: {text}`), document
  `title: {title|none} | text: {text}`; images/audio use no prompt (mtmd supplies the template).
- Pipeline: two-stage (one serial prepare thread, one batch ahead of a single-threaded embed+commit). Change detection
  is metadata only (size, mtime +-1 ms, kind); moves are detected by (device, inode, size, mtime).
- Long files are read in steps (first 30 PDF pages / 40 doc chunks first, the rest when idle; caps 5,000 pages/chunks).
- Audio: 30 s windows, 25 s hop, 16 kHz mono, silent windows skipped (RMS < 0.003). Video: a frame every 3 s at
  budget 70 (deduped by 8x8 average hash within 4 bits), plus the soundtrack as audio windows.
- Image token budgets: photo 140, screenshot 280, PDF page image 280, video frame 70. Sides are multiples of 48 px.

---------------------------------------------------------------------------------------------------

## 3. The speed-up thesis (what to expect, and what NOT to promise)

**Rewriting in Rust alone buys ~1.0-1.15x.** Almost all wall time is in the model (llama.cpp, already C++/GPU). The
real wins are architectural, and Rust makes them cheap and safe to build. My earlier estimates (ESTIMATES, not
measurements; your job is to replace each with a measurement):

| Lever | Expected effect | Evidence in the Swift code |
|---|---|---|
| Pipeline decode/OCR/resize on all cores while the GPU embeds, bounded queues | 1.3-2x on the first indexing pass | prepare stage is one serial thread (spec-pipeline s5); crawl is single-threaded with a second `readdir` per folder; screenshots decoded twice |
| Batch media across files, bigger batches, keep model resident | 1.2-2x on embed stage | media runs one file per `llama_process` (spec-embedder s7); only text packs (<=16 seqs / 2048 tok) |
| Warm resident query model | cold search ~10x+ faster (hundreds of ms -> tens of ms) | query encoder is a helper process that quits when idle; a cold start was 125 -> 650 ms (Worker.swift:46-48) |
| SIMD + multicore vector scan, dense arrays, top-k select | 2-5x on the scan slice; matters at >=1M vectors | single-threaded; per-row Dictionary/closure loops (spec-store-search s6) |
| Audio without WAV round-trip, O(1) buffering | big win on long audio | 3-hour file writes ~415 MB of WAVs, `removeFirst` is O(n) per window |
| Long-doc steps without re-reading/re-chunking | big win on long documents | re-read and re-chunked in full every 40-chunk step |
| Runtime CPU-feature dispatch (AVX2/AVX-512/NEON), native flags | up to ~9x on the scan kernel (measured on one box, below) | `GGML_NATIVE=OFF` in the Swift build |
| GPU backend choice (CUDA/Vulkan/ROCm/Metal) | hardware dependent | n/a |

Honest expectation for the **first-pass index time**: roughly **1.5-2.5x** with everything pulled, on the same Mac
GPU; more on machines with a faster GPU than an M-series. Metal is the same Metal either way. State this plainly in
any report; don't oversell "Rust is faster".

**Measured so far (all on the cloud box: Intel Xeon 2.8 GHz, 2 cores, AVX2+AVX-512, no GPU, Linux):**
- Vector scan, 200k x 768 f16 vectors (0.31 GB): naive loop 431 ms (default build) -> 8-lane loop 328 ms -> +2 threads
  173 ms. With `-C target-cpu=native`: naive 192 ms, lanes 44 ms (7 GB/s), lanes+2 threads **22 ms** (13.9 GB/s).
  Takeaway: f16->f32 conversion is the bottleneck without F16C/NEON hardware conversion, so ship runtime dispatch
  (`multiversion`/`pulp`, or `#[target_feature]` + `is_x86_feature_detected!`; NEON is baseline on aarch64).
- `top_k(100)` over 200k scores: 1.1-1.8 ms.
- Crawl: `/usr` with a debug build: 1 thread 167k files/s, 2 threads 364k files/s.
- Embedding on CPU only (2 cores, the parity test): 5 prompted texts in one call 11.9 s; screenshot (budget 280) 14.1 s;
  photo (140) 5.7 s; video frame (70) 2.6 s; 4 s of speech 1.3 s; load+warmup 1.7 s. **Meaningless as a speed
  comparison against Metal**; useful only as a CPU-fallback floor.

**Baseline you must establish first on the Mac** (no Swift baseline exists yet; I couldn't run macOS software):
- Easiest, no Xcode: download the release app https://github.com/ARahim3/DigUp/releases/latest, and use its bundled CLI
  `DigUp.app/Contents/Helpers/digup` (UNVERIFIED that it runs standalone; the app invokes it, Engine.swift:921). It
  looks for models in `~/Library/Application Support/DigUp/Models` (ModelFiles.swift), so copy/link the two GGUFs there.
  `digup --help` lists estimate / index / search / status / eval.
- Or build from source (needs Xcode 26 + CMake): see `reference/digup-swift/README.md` "Build from source".
- Corpus: `reference/digup-swift/scripts/make-testbed.sh` builds a test folder (needs ffmpeg); `evals/*.json` (126
  queries; English/Bengali/Arabic/code) point at `~/DigUpTestbed`.
- Record: first-pass index wall time per modality, CPU/GPU utilisation, peak RSS, cold and warm query latency, and
  eval R@1/R@5/R@10/MRR. Put results in `docs/BENCHMARKS.md` with the exact machine and commands.

---------------------------------------------------------------------------------------------------

## 4. Current state (updated 2026-10-11, end of the first autonomous session)

Everything below runs on Apple Silicon macOS with Metal. 200 tests pass (`cargo test --workspace --release`, parity
test included); `cargo clippy --workspace --all-targets --release` is clean. The work is NOT committed beyond commit 3c51112.

| Crate | Status | Notes |
|---|---|---|
| `dtfu-core`, `dtfu-text` | done | enums/consts/prompts; text normalization, tokenizer helpers (NFKC, Arabic folding, `is_word_char` keeps virama/combining marks) |
| `dtfu-store` | done | `IndexStore` (schema v5, insert/requeue/move/delete/prune/adopt/finish, `add_segments_and_finish` single transaction), NEON/AVX2/AVX-512/lanes scan kernels (x86 kernels UNTESTED: no AVX2 under Rosetta) |
| `dtfu-search` | done | `Searcher` (z-score fusion, FTS, code index with `CodeLookup`/focus); same top-10 order as Swift on all 126 eval queries |
| `dtfu-crawl` | done | now reads the macOS `kMDItemIsScreenCapture` xattr |
| `dtfu-extract-text/-image/-audio` | done | docs (RTF honours `\fcharset`/`\ansicpg`), code, images, audio. Gaps: legacy `.doc` unsupported; lossy audio untested |
| `dtfu-extract-pdf` | macOS only | PDFKit via objc2, rotation handled; other OSes return Unsupported until pdfium is wired |
| `dtfu-extract-video` | stopgap | shells out to ffmpeg/ffprobe, streams frames and dedups while reading; frame interval 3.0 s hardcoded in options |
| `dtfu-ocr` | macOS only | Apple Vision via objc2; other OSes return None (Q4: `ocrs`/Windows.Media.Ocr still to do) |
| `dtfu-embed`, `dtfu-llama-sys` | done | Metal parity 1.000; `examples/bench_embed` |
| `dtfu-index` | done | `sync`, `prepare`, `commit`, `run` (rayon prepare + one embed thread), `service` (scheduler loop for the app) |
| `dtfu-models` | done | locate + resumable verified download; `digup download` |
| `dtfu-cli` | partly | `index`, `search` (`--json`, `code:`), `status`, `eval`, `estimate`, `download`. Missing: `index-service` front end (library exists), `worker` (not needed: the embedder runs in-process) |

Evidence (details and commands in `docs/BENCHMARKS.md`): eval ALL R@1/R@5 equals the Swift app on synthetic 0.95/1.00,
multilingual 0.92/0.98 and code 1.00/1.00; Chinese (simp+trad) 0.92/1.00; first index pass of the small testbed 25 s
vs 38.8 s Swift; Metal is 3.4-6.3x faster than CPU-only on this M4.

**Update 2026-10-12:** E2 desktop shell exists in `app/` (Tauri 2, static front end): search, kind chips, thumbnails, first-run welcome panel, folder/pause controls, model download, open-at-PDF-page / QuickTime / IINA moment (compile- and unit-tested only), unsigned arm64 `.app` bundle builds (`app/README.md`). Search keyword stage is 54x faster at 1M segments with identical rankings (BENCHMARKS B3). Off-Mac backends written but never run against the real libraries: pdfium (`dtfu-extract-pdf/src/pdfium.rs`, set `DTFU_PDFIUM_DIR`, no cwd search) and `ocrs` OCR (`dtfu-ocr/src/ocrs_backend.rs`, set `DTFU_OCR_MODELS`; model URLs in its module doc); both compile-check for x86_64-linux. Still open: x86 scan kernels on real hardware (compile-checked for x86_64-linux only), Windows/Linux, signing/notarization, the GUI has not been exercised end to end with a model.

**Next, in order:** (1) E2 desktop shell (Tauri 2, Q3 default) on top of `dtfu_index::service` and `dtfu-search`;
(2) E3 Windows/Linux: pdfium (Q5), OCR backends (Q4), ffmpeg bundling, x86 scan kernels on real hardware, run the new
`.github/workflows/ci.yml` (never run yet; Windows job is allowed to fail); (3) larger-corpus benchmarks (real photos/PDFs,
100k+ vectors) and profiling with samply; (4) E4 packaging. Image/audio embedding is GPU-bound (about 0.22 s per 140-token
image on Metal), so the remaining levers are in llama.cpp/mtmd batching, not in our code.

**Needs the owner:** git remote / repo name and visibility (Q2); whether to commit the uncommitted work; LICENSE holder
name (placeholder "dig-the-f-up contributors"); a Windows/Linux machine or CI to verify non-Mac builds.

---------------------------------------------------------------------------------------------------

## 5. Decisions made (and why). Don't relitigate without new evidence.

- **D1. Keep llama.cpp for inference; wrap it, don't rewrite it.** It is the fastest portable runtime for this model, has
  every GPU backend, and is what DigUp validated against. ONNX/candle have no EmbeddingGemma 2 support (checked).
- **D2. Own bindings over llama.cpp tag `b11461` + DigUp's patch, not the `llama-cpp-2` crate.** `llama-cpp-2` 0.1.159's
  vendored llama.cpp predates EmbeddingGemma 2 (`gemma-embedding2` arch) and lacks `llama_process`/`llama_batch_ext_*`;
  its safe wrapper also omits `mtmd_get_output_embd`. Re-evaluate if a newer release catches up.
- **D3. The one-line patch (`patches/llama.cpp-no-logits.patch`) is required.** Without it llama.cpp memsets a 1 MiB
  logits row per token (about 2 GiB per 2048-token input) for an embeddings-only model. Check whether newer upstream
  tags fixed it before bumping the pin.
- **D4. Image-size path is ours, bit-exact.** `mtmd` is configured with `image_min_tokens=1` so it never rescales; we
  resize to the reference's pixels (`sizing.rs`, Pillow bicubic, 22-bit fixed point). Don't swap in the `image` crate's
  resizer for the embedder input (different kernel/rounding). For the *thumbnail* stage before it (ImageIO in Swift),
  any good resampler is acceptable; expect ~0.99 cosine vs Mac-made vectors on images.
- **D5. Stay file-compatible with schema v5** (exact DDL, tokenizer `porter unicode61 remove_diacritics 2 categories
  'L* N* Co M*'`, `fts.rowid == segments.id` kept by hand, `user_version=5`) so tooling and evals stay shared.
- **D6. Exact brute-force search, f16 storage**, parallelised and SIMD'd; an optional int8 or mmap'd sidecar is
  allowed as an *accelerator* but must give the same ranking within tolerance (verify with the evals).
- **D7. No telemetry, no account.** Update check only if the owner wants it, off by default.
- **D8. Project is MIT with `NOTICE`** crediting DigUp (MIT), llama.cpp (MIT), EmbeddingGemma 2 (Apache 2.0).

---------------------------------------------------------------------------------------------------

## 6. Bootstrap on the Mac (do this first)

The owner is on Apple Silicon (`arm64-apple-darwin`), appears to have rustup/cargo (`~/.cargo` and `~/.rustup` exist), and has already
downloaded the two model files into `~/Downloads` (SHA-256 verified):
`embeddinggemma-2-Q8_0.gguf` (309,855,456 B) and `mmproj-embeddinggemma-2-Q8_0.gguf` (554,821,024 B).
If they are ever missing, direct links (pinned revision):
- https://huggingface.co/ggml-org/embeddinggemma-2-GGUF/resolve/bfcd298762cc34d0357ece5ebdd31791a3a374d8/embeddinggemma-2-Q8_0.gguf
- https://huggingface.co/ggml-org/embeddinggemma-2-GGUF/resolve/bfcd298762cc34d0357ece5ebdd31791a3a374d8/mmproj-embeddinggemma-2-Q8_0.gguf

```bash
cd <this repo>
brew list cmake >/dev/null 2>&1 || brew install cmake     # also needs libclang for bindgen: Xcode CLT provides it
scripts/link-models.sh            # links models into ./models and verifies SHA-256
scripts/build-llama.sh            # clones llama.cpp b11461, applies the patch, builds with Metal (Metal shaders are embedded)
cargo test --workspace --release  # expect: 2+4+26+6 unit tests and the parity test passing
```
Then **confirm the parity test passes on Metal** (it was only run on x86 CPU). Expect cosines >= 0.9995. If linking
fails on macOS, fix `crates/dtfu-llama-sys/build.rs` (frameworks, `c++`, missing `ggml-blas`/`ggml-metal` libs) and
re-run. If a vector is below 0.999 on Metal, stop and investigate before building anything else.
Note: when the Mac's GPU is used, `Options::gpu_layers=999` offloads everything and `mtmd` runs with `use_gpu=true`.
The model's activations overflow in F16, so never switch these GGUFs for F16 variants.

If you also want the Swift baseline CLI, copy (don't move) the GGUFs to `~/Library/Application Support/DigUp/Models/`.

---------------------------------------------------------------------------------------------------

## 7. Target architecture (crates to add)

```
Layering (each crate independently testable):
dtfu-core      types/constants (done)
dtfu-store     IndexStore (writes, sync, move detection, reset/adopt, fingerprint), Searcher, vector scan (partial)
dtfu-crawl     (done)
dtfu-extract   readers -> segments: image, pdf, doc, audio, video, code, ocr; chunking; hasWords/PageTextProblem
dtfu-embed     (done) + an EmbedPool/worker wrapper so the app can run it in a child process that exits when idle
dtfu-index     pipeline orchestration: crawl -> prepare (rayon) -> embed (single GPU thread, batched) -> commit;
                pause/resume, power policy, long-file stepping, backfill
dtfu-search    (or inside store) z-score fusion, FTS query building, text normalization (NFKC, Arabic folding)
dtfu-cli       `digup estimate|index|search|status|eval|worker|index-service` (mirror the Swift CLI; same JSON-lines
                worker protocol so tools can be shared)
dtfu-app       desktop shell (see Open Question Q3): tray, global hotkey, search panel, results, onboarding, settings
```
Process model to keep from DigUp: indexing in-process or in a helper; the **query encoder is a separate lightweight
mode** (`Embedder::new(text, None, ..)`, 512-token context, ~250 MB) so memory returns to the OS when idle. Improvement
over Swift: keep it resident while the panel is likely to be used and unload after a configurable idle period.

**Cross-platform replacement matrix** (macOS API in Swift -> portable Rust):
| Need | macOS (Swift) | Windows / Linux / portable Rust |
|---|---|---|
| Image decode, EXIF orient, thumbnails | ImageIO | `image`, `zune-jpeg`, `libheif-rs`, `rawloader`, `kamadak-exif`, `fast_image_resize` (thumbnails only; see D4) |
| PDF text + page raster | PDFKit | `pdfium-render` (ship the pdfium binary per OS); fallback `mupdf` |
| Audio decode to 16 kHz mono | AVFoundation | `symphonia` + `rubato` (resample); or `ffmpeg` |
| Video frames + soundtrack | AVFoundation | `ffmpeg-next` or a bundled `ffmpeg` binary; `symphonia` cannot decode video |
| OCR of screenshots | Vision | macOS: keep Vision via `objc2-vision` (best quality); Windows: `Windows.Media.Ocr` via `windows` crate; portable: `ocrs` or Tesseract/PaddleOCR. Expect a quality regression off-Mac; evals will show it |
| Docs (txt, md, rtf, doc, docx, odt, html) | NSAttributedString | `docx-rs`/`zip`+XML for docx/odt, `rtf-parser`, `scraper`/`html2text`, `antiword`-class for .doc (hard; may be best-effort) |
| File watching | FSEvents | `notify` (debounce 0.75 s; code index 60 s quiet / 600 s max) |
| Power policy | IOKit | `battery`/`starship-battery`; Windows power status; Linux upower |
| Global hotkey, tray, panel | AppKit | Tauri 2 (`tauri-plugin-global-shortcut`, tray) or `tao`+`wry`/`egui` |
| Open at page/moment, reveal, Quick Look | Preview/QuickTime | per-OS `open` equivalents; PDF page via `file://...#page=N` where supported; editor `--goto` for code |
| Auto-update | Sparkle | Tauri updater (optional) |
| File type database | UTType | extension + magic-byte tables (done in `dtfu-crawl`) |
| Cloud placeholders | `SF_DATALESS` | Windows `FILE_ATTRIBUTE_RECALL_ON_DATA_ACCESS` / OneDrive attrs |

---------------------------------------------------------------------------------------------------

## 8. Roadmap (do in this order; each item has an acceptance test)

Parallelism guidance is in italics. Use subagents per CLAUDE.md. After each phase run a verification round.

**Phase A. Prove the base on the Mac (inline, quick)**
- A1. Bootstrap (section 6). *Acceptance:* `cargo test --workspace --release` green on macOS with Metal; record parity
  cosines in `docs/BENCHMARKS.md`. Fix `build.rs` for macOS if needed.
- A2. Establish the **Swift baseline** (section 3) on the owner's Mac with a realistic testbed. *Acceptance:* numbers
  recorded in `docs/BENCHMARKS.md` (index time per modality, query latency cold/warm, RSS, eval metrics).
- A3. `git init` and make a first commit if the owner agrees (they haven't asked yet; ask once, with a recommended yes).

**Phase B. Storage and search (parallel with C and D)**
- B1. `dtfu-store::IndexStore`: port `IndexStore.swift` (insert file/segments+fts in one transaction, delete (fts first),
  move detection, pending queue ordering, reset/adopt with fingerprint, `info` JSON handling, counts). Use
  `prepare_cached`, WAL, and batch inserts. *Acceptance:* port `IndexStoreTests.swift` and `LongFilesTests` store parts.
- B2. `Searcher`: exact port of spec-store-search s2 (z-score per modality with one global std floor 0.02, FTS OR-query,
  `bm25(3.0,1.0)`, coverage^1.5, weights (1, 2.0) / lookup (0.5, 4.0) / code 1.0, thresholds z>=2.0 or coverage>=0.5,
  kind boost 1.0, one result per file, tie-break by file id) with the dense-array rewrite (spec s6.2). Text
  normalization (NFKC, joiner/direction-mark stripping, Arabic folding, Bengali/Arabic prefix `*`).
  *Acceptance:* ports of `SearchTextTests`, the fusion tests, and identical ranking to the Swift CLI on the testbed.
- B3. Scan acceleration: runtime SIMD dispatch (AVX2+F16C, AVX-512, NEON), rayon over blocks, optional mmap sidecar.
  *Acceptance:* bench on the Mac and the cloud numbers in section 3 reproduced or beaten; rankings identical.

**Phase C. Extractors (fully parallel: one subagent per modality, each with its own tests and a fixtures dir)**
- C1. Images/screenshots: decode, orient, thumbnail (max 1024 photos / 1536 screenshots, min side 256), screenshot detection,
  OCR hook. C2. PDF: text per page (needs >=200 chars, else render at 1400 px), `PageTextProblem` heuristics for
  Bengali/Arabic (re-validate against pdfium), 30-page first step. C3. Docs: txt/md/rtf/docx/odt/html -> 1800-char
  chunks, 200 overlap, `hasWords` (letters*4 >= other non-space) gate. C4. Audio: decode -> 16 kHz mono f32, 30 s windows
  / 25 s hop, RMS gate 0.003, 2 s minimum tail, **no WAV round-trip** (use `embed_audio_samples`). C5. Video: frames
  every 3 s (<=1200/file), 8x8 average-hash dedup (<=4 bits), soundtrack through C4. C6. Code: 1800-char line-aligned
  stretches (prefer blank lines/block starts, 40% min fill, 2-line overlap on forced cut), notebooks by cell, git-aware
  ignore (`ignore` crate), secrets list, 1 MiB cap. C7. OCR backends per OS.
  *Acceptance:* the "tests worth porting" list in spec-pipeline s6 plus the added gaps listed there.

**Phase D. Pipeline and CLI**
- D1. `dtfu-index`: crawl -> prepare (rayon, bounded channel) -> single embed thread (batch text up to 16 seq/2048 tok;
  batch images; group media across files where the model allows) -> commit transactions. Pause/resume via SQLite state.
  Long-file stepping (30 pages / 40 chunks per step, idle backfill, 5,000 caps). Power policy. Fingerprint check.
  *Acceptance:* `LongFilesTests` ported with a FakeEmbedder; end-to-end index of the testbed.
- D2. `dtfu-cli`: `estimate`, `index`, `search`, `status`, `eval`, `worker`, `index-service` with the JSON-lines
  protocol in spec-embedder s4. *Acceptance:* eval R@1/R@5/R@10/MRR within noise of the Swift CLI on the same corpus
  (English, Bengali, Arabic, code). Document any regression and its cause (OCR is the likely one off-Mac).
- D3. Benchmarks: compare against the Swift baseline on identical corpus, per modality and total; update
  `docs/BENCHMARKS.md`; profile (`samply`/Instruments) and iterate on the biggest remaining bottleneck.

**Phase E. App and cross-platform**
- E1. Model downloader (resumable, HTTP ranges, SHA-256 per file from `ModelFiles`), onboarding, settings.
- E2. Desktop shell (Q3), global hotkey, search panel, results with thumbnails, open-at-page/moment, filters.
- E3. Windows and Linux: build llama.cpp with CUDA/Vulkan/HIP variants; fix `build.rs` link details; run the parity test
  per backend (expect >=0.9995; the spec flags non-Metal op coverage as UNVERIFIED); CI matrix (GitHub Actions
  macos-14, ubuntu, windows) building and running unit tests; model-dependent tests skip when GGUFs are absent.
- E4. Packaging: signed/notarized macOS bundle, MSI/MSIX, AppImage/deb; ship **portable** CPU builds
  (`NATIVE=0` + runtime dispatch, or ggml `GGML_CPU_ALL_VARIANTS` with dynamic backends).

**Definition of done:** CLI + app run on macOS, Windows and Linux; parity and eval gates green; `docs/BENCHMARKS.md`
shows measured speedups vs the Swift original with methodology; README for users; CI green; NOTICE/licences correct.

---------------------------------------------------------------------------------------------------

## 9. Pitfalls and non-obvious facts (read before touching the related code)

Embedder / llama.cpp
- Position array passed to `llama_batch_ext_set_pos` has **4 elements** (`[pos,0,0,0]`) even for non-M-RoPE: copy it.
- Context: `n_ctx=n_batch=n_ubatch=2048` (512 text-only), `n_seq_max=16`, `kv_unified=true`, mean pooling, embeddings on.
  A non-causal model must see a whole input in one ubatch. Threads default to 4 and flash-attn is AUTO: **tune both**
  (CPU-only machines especially); this is a real performance lever.
- L2 normalisation is done by hand (f64 accumulation); zero vector if sum is 0. Failed items get a zero vector.
- Truncate to `max_tokens-1` + EOS when too long (keeps BOS and EOS like transformers).
- Run two warm-up passes after load (kernel/shader compilation on Metal/CUDA/Vulkan).
- mtmd: `image_min_tokens=1`, `image_max_tokens=1120`; audio cut to first 30 s at the model rate (16 kHz).
- `GGML_NATIVE=ON` is fine locally but crashes older CPUs if shipped. Linux builds link `gomp` (OpenMP) in `build.rs`.
- llama.cpp video support is not used; video is frames + audio (the embedder rejects `video_ctx`).

Storage / search
- Keep `fts.rowid == segments.id`; delete fts rows before segments (no triggers, `ON DELETE CASCADE` only covers segments).
- Mismatched vectors must never mix: compare `meta.vector_source` and `meta.fingerprint`; the Swift CLI refuses to search
  if they differ. Chunk sizes, render sizes and OCR choices are NOT in the Swift fingerprint (so changing them doesn't
  trigger a reindex): include them in ours.
- Scores differ in scale per modality (text ~0.75-0.86, image/audio ~0.67-0.78); search normalises per modality.
- `isSecret` filtering applies only to the **code** index in Swift. Main index relies on skip rules.
- `files.info` is JSON with string values; inode/device are stored as `UInt64` bit-cast to `Int64`.
- Swift counts chunk sizes in grapheme clusters (`Character`), Rust `chars()` differ slightly: pick one and test.

Pipeline
- Pending order: screenshot, image, pdf, doc, audio, video, newest first; up to 50 new files indexed immediately on any
  power source, backfill waits for the power policy; watcher debounce 0.75 s.
- Metadata-only change detection (size, mtime within 0.001 s, kind). A vanished *root* (unplugged drive) must keep its rows.
- Dataset heuristic skips folders with 300+ media files and 70%+ machine-made names. Project folders (`.git`,
  `package.json`, `Cargo.toml`...) index screenshots only unless code search is on.

Process
- The cloud box that built this had **no GPU, 2 cores, no internet except package registries, GitHub and crates.io**. Hugging
  Face was blocked there (the owner downloaded the models). Don't copy its timings as performance data.

---------------------------------------------------------------------------------------------------

## 10. Verification protocol

Before reporting any milestone:
1. `cargo test --workspace --release` green, including the parity test; `cargo clippy --workspace --all-targets` clean
   (not yet enforced; make it so).
2. For anything touching unsafe/FFI: an independent strong-model review focused on ownership, lifetimes, error paths.
3. For anything touching ranking or extraction: run the evals and compare with the Swift CLI on the same corpus.
4. For any speed claim: benchmark command, machine, and numbers in `docs/BENCHMARKS.md`; compare to the baseline.
5. Re-run tests yourself after every subagent batch; never trust a hand-back.

---------------------------------------------------------------------------------------------------

## 11. Open questions (decide with the recommended default unless it blocks you; tell the owner what you chose)

- **Q1. Reuse Swift-made indexes?** Parity is 0.9997+ for text/audio but images will be ~0.99 (different decoders).
  *Recommended:* separate index directory and a distinct `vector_source` runtime tag for Rust-made indexes; never write
  into a Swift-made index; optionally allow read-only search of one with a warning. Revisit after evals.
- **Q2. Repo hygiene:** `git init`, first commit, GitHub remote, licence file (MIT), contributing notes. *Recommended:* yes;
  ask the owner once for the repo name/visibility, then proceed.
- **Q3. Desktop shell:** Tauri 2 (web UI, small, cross-platform tray/hotkey/updater) vs `egui`/`iced` (pure Rust).
  *Recommended:* Tauri 2 with a minimal front end; keep the engine headless behind a clean API so the CLI and app share it.
- **Q4. OCR strategy off-Mac:** `ocrs` (pure Rust, lighter) vs Tesseract/PaddleOCR. *Recommended:* start with `ocrs`
  behind a trait, add Windows.Media.Ocr and macOS Vision backends, pick defaults by eval results.
- **Q5. Distribution of pdfium/ffmpeg:** bundle vs require system install. *Recommended:* bundle pdfium; use a bundled
  ffmpeg binary (LGPL build) for video, with `symphonia` for audio-only.
- **Q6. Naming/branding:** this is a port of DigUp; the owner hasn't chosen a name. *Recommended:* keep `dig-the-f-up` as the
  working name and don't ship a rename without asking.

---------------------------------------------------------------------------------------------------

## 12. Session history (so you know where everything came from)

1. Read DigUp's README and `Package.swift` (GitHub blocks automated browsing of `Sources/`, so the source was cloned).
2. Gave the speed-up thesis (section 3). The owner said "let's kick things off".
3. Cloned DigUp to `/root/dtfu-src` (now `reference/digup-swift`). Three parallel read-only agents wrote the three
   specs in `docs/`. Findings that shaped the plan: `llama-cpp-2` too old (D2), serial prepare stage, per-file media
   embedding, WAV round-trip, double screenshot decode.
4. Scaffolded the workspace; benchmarked the scan; built llama.cpp b11461 with the patch (CPU); wrote bindings and the
   embedder; ported the Pillow resizer with tests generated from the Swift tests; passed the 9-vector parity test.
5. A sonnet agent ported the crawler/rules (26 tests, re-run by me); an opus agent reviewed the FFI code; fixes applied.
6. The owner's models were fetched via the desktop link: both SHA-256 values matched `ModelFiles.swift`. (The embedder
   file had to be split to cross a 400 MB per-file transfer limit; a temp folder in Downloads was deleted afterwards.)
7. Added portable `scripts/build-llama.sh` and `scripts/link-models.sh`, this handoff, and `CLAUDE.md`.
   A clean-room rebuild of the repo from scratch (clone -> build -> test) was run to prove the handoff reproduces
   (result recorded in section 13).

## 13. Clean-room reproduction result

Done on the cloud box (Linux x86, 2 cores, CPU only) from a fresh copy of this repo with no build artifacts:
`scripts/build-llama.sh` (clone b11461, apply patch, cmake build, about 8 minutes on 2 cores) then
`cargo test --workspace --release`. Result: **all green, 2 + 4 + 26 + 6 unit tests and the parity test**, with the same
nine cosines as in section 4 (document 0.999768 ... speech 0.999665). So the repo and scripts reproduce from nothing.
macOS/Metal, Windows and GPU backends have **not** been run; that is Phase A1 and E3.
