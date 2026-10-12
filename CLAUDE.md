# CLAUDE.md: Dig-the-f-up

Cross-platform, maximum-performance Rust port of **DigUp** (the original; this product is **Dig-the-f-up**, CLI `dtfu`) (https://github.com/ARahim3/DigUp, MIT): on-device semantic
search over a user's files (photos, PDFs, documents, audio, video, code) with Google DeepMind's EmbeddingGemma 2.
The original is Swift and macOS-only. This repo must run on **macOS (Apple Silicon only; Intel Macs are dropped, owner decision), Windows and Linux** and be as fast as we can make it.

Start with `docs/HANDOFF.md`: goal, current state, decisions, verified facts, the roadmap with acceptance criteria,
and open questions.

## Non-negotiables
1. **The parity test is the safety net.** `cargo test -p dtfu-embed --release --test parity` must keep passing
   (all 9 reference vectors at cosine >= 0.999 vs the Swift app's). Run it after any change to `dtfu-embed`,
   `dtfu-llama-sys`, or `scripts/build-llama.sh`.
2. **Never use F16 GGUFs** for this model (activations overflow). Q8_0 files only, pinned HF revision `bfcd298...`.
3. **Index compatibility:** SQLite schema v5, 768-d float16 L2-normalized vectors, same prompts. Don't change any of
   these without updating `docs/spec-store-search.md` and deciding the `vector_source` tag question (HANDOFF section 11).
4. **Don't modify `reference/digup-swift/`** (fetched by `scripts/fetch-reference.sh`, git-ignored, read-only). It is the Swift source at commit `6faa29e` that every
   `file:line` citation in `docs/spec-*.md` points to.
5. **Licensing:** the project is Apache-2.0 (`LICENSE`). Keep `NOTICE` (DigUp is MIT; llama.cpp MIT; EmbeddingGemma 2 Apache 2.0).
6. Measure, don't guess. Every performance claim in code comments, docs or reports must come from a benchmark that
   was run, with the machine and command noted. Estimates are labelled as estimates.

## Layout
```
crates/dtfu-core           shared enums, constants, prompts (index schema v5)
crates/dtfu-text           text normalization and tokenizer helpers (port of SearchText.swift)
crates/dtfu-store          SQLite schema v5 + f16 vector scan kernels
crates/dtfu-search         hybrid search: vector z-scores fused with FTS5 keyword matches
crates/dtfu-usage          local open-usage log and frecency scores (separate SQLite file)
crates/dtfu-crawl          parallel crawler, file classification, folder-selection rules
crates/dtfu-extract-text   document and source-code text extraction and chunking
crates/dtfu-extract-image  image decode (EXIF), thumbnails, screenshot detection
crates/dtfu-extract-audio  audio decode (symphonia) for embedding
crates/dtfu-extract-pdf    PDF text, titles and page rendering (PDFKit on macOS, pdfium elsewhere)
crates/dtfu-extract-video  video keyframe extraction (ffmpeg/ffprobe)
crates/dtfu-ocr            screenshot OCR (Apple Vision, ocrs, optional LightOn)
crates/dtfu-llama-sys      bindgen + static link of llama.cpp b11461
crates/dtfu-embed          safe embedder (text/image/audio) + Pillow-exact resizer
crates/dtfu-models         locate and download the EmbeddingGemma 2 GGUF files
crates/dtfu-index          indexing pipeline: sync, prepare, embed + commit, service loop
crates/dtfu-engine         UI-free thread-safe facade over indexing and search for the app
crates/dtfu-watch          filesystem change watcher that triggers sync requests
crates/dtfu-mcp            minimal MCP server over stdio for agents
crates/dtfu-cli            the `dtfu` command line
app/                       Tauri 2 desktop shell
docs/HANDOFF.md           START HERE
docs/spec-pipeline.md     crawler/extractors/indexer spec, with Swift file:line citations
docs/spec-embedder.md     llama.cpp call sequence, preprocessing, worker protocol, parity test
docs/spec-store-search.md schema, search/ranking algorithm, code extractor, evals
reference/digup-swift/    Swift source of the original (commit 6faa29e); fetched by scripts/fetch-reference.sh, git-ignored
scripts/build-llama.sh    builds llama.cpp b11461 + DigUp's patch into ./build-llama (Metal on macOS)
scripts/link-models.sh    finds the two GGUF files, links them into ./models, verifies SHA-256
patches/                  llama.cpp-no-logits.patch (from DigUp)
vendor/ build-llama/ target/ models/ reference/   generated or local, git-ignored
```

## Commands
```bash
scripts/link-models.sh            # finds/verifies the model files (looks in ~/Downloads first)
scripts/build-llama.sh            # ~5-15 min once; Metal on macOS, CPU elsewhere; BACKEND=cuda|vulkan|hip
cargo test --workspace --release  # everything, incl. the parity test (needs models + build-llama)
cargo test -p dtfu-embed --release --test parity -- --nocapture   # the safety net, prints cosines
cargo run --release -p dtfu-store --example bench_scan -- 200000
cargo run --release -p dtfu-crawl --example crawl_bench -- <dir> [threads]
```
`DTFU_LLAMA_BUILD` / `DTFU_LLAMA_SRC` override where `dtfu-llama-sys` looks for the build and headers.
`DTFU_MODELS` overrides where the parity test looks for the GGUF files.
Use `RUSTFLAGS="-C target-cpu=native"` only for local benchmarks, never for shipped binaries (see HANDOFF section 9).
