# Contributing

Open work lives in [issues](https://github.com/delirious6423/dig-the-f-up/issues). Contributions are under the Apache
License 2.0.

## Build and test

```bash
scripts/build-llama.sh                 # once: llama.cpp b11461 + DigUp's patch (Metal on macOS, CPU elsewhere; BACKEND=cuda|vulkan|hip)
scripts/link-models.sh                 # or `dtfu download`: the two GGUF files, SHA-256 checked
cargo test --workspace --release       # everything; model-dependent tests skip without the models
cargo clippy --workspace --all-targets --release -- -D warnings
cd app && cargo test --release         # the desktop app is a separate cargo project
```

| variable | what it does |
|---|---|
| `DTFU_MODELS` | folder that holds the model files |
| `DTFU_LLAMA_BUILD`, `DTFU_LLAMA_SRC` | where `dtfu-llama-sys` finds the llama.cpp build and headers |
| `DTFU_GPU_LAYERS=0` | CPU-only run of the `bench_embed` example |
| `DTFU_OCR` | OCR engine: `vision`, `ocrs` or `lighton` |
| `DTFU_SEARCH_TRACE=1` | timing for each search stage |

## Rules

1. **Parity is the safety net.** `cargo test -p dtfu-embed --release --test parity` must pass: all nine reference
   vectors at cosine >= 0.999 against the Swift app. Run it after any change to `dtfu-embed`, `dtfu-llama-sys` or
   `scripts/build-llama.sh`.
2. **Q8_0 model files only**, pinned Hugging Face revision. F16 GGUFs overflow in this model.
3. **Index format**: SQLite schema v5, 768-d float16 L2-normalised vectors, the original's prompts. Keep
   `fts.rowid == segments.id`, and delete fts rows before segments.
4. **Measure, don't guess.** Every speed claim needs a benchmark with machine and command in `docs/BENCHMARKS.md`.
5. **Portable binaries.** `RUSTFLAGS="-C target-cpu=native"` and `NATIVE=1` are for local benchmarks only.

## Layout

| path | contents |
|---|---|
| `crates/dtfu-core`, `dtfu-text` | shared types, prompts, text normalisation |
| `crates/dtfu-store`, `dtfu-search`, `dtfu-usage` | SQLite index and SIMD vector scan; hybrid search (vector z-scores fused with FTS5); open-frequency ranking |
| `crates/dtfu-crawl`, `dtfu-watch` | parallel crawl and file rules; live file watching |
| `crates/dtfu-extract-*`, `dtfu-ocr` | text, image, PDF, audio and video extraction; OCR engines |
| `crates/dtfu-llama-sys`, `dtfu-embed`, `dtfu-models` | llama.cpp bindings, the embedder, model download |
| `crates/dtfu-index`, `dtfu-engine` | indexing pipeline; engine shared by the app and CLI |
| `crates/dtfu-cli`, `dtfu-mcp` | the `dtfu` CLI; a read-only MCP server |
| `app/` | Tauri 2 desktop app (static front end in `app/dist`, no Node needed) |
| `scripts/fetch-reference.sh` | fetches the original Swift source (commit 6faa29e), which the `file:line` citations in comments point to |

## Things that are easy to get wrong

- llama.cpp: `llama_batch_ext_set_pos` takes 4 positions per token. A non-causal model must see a whole input in one
  ubatch (`n_ctx = n_batch = n_ubatch = 2048`). L2 normalisation is done by hand. Run two warm-up passes after load.
- Scores differ in scale per modality (text about 0.75-0.86, image and audio about 0.67-0.78). Search normalises them
  per modality.
- Vectors from different models or settings must never mix. `meta.vector_source` and `meta.fingerprint` guard this,
  and the app re-indexes when they change.
- A root folder that vanished (an unplugged drive) or can't be read keeps its rows.
- The original counts some lengths in Unicode scalars and others in grapheme clusters. Match each one; Bengali and
  Hindi tests cover the difference.

## Releases

Pushing a `v*` tag (equal to `version` in `app/tauri.conf.json`) runs `.github/workflows/release.yml`. It builds the
macOS arm64 app and attaches a zip and its SHA-256 to a draft release. It signs and notarizes only when the
`APPLE_CERTIFICATE`, `APPLE_CERTIFICATE_PASSWORD`, `APPLE_SIGNING_IDENTITY`, `APPLE_ID`, `APPLE_PASSWORD` and
`APPLE_TEAM_ID` secrets exist. Otherwise the zip is marked UNSIGNED, and users right-click > Open on first launch.
Regenerate the licence inventory with `scripts/gen-third-party-licenses.sh` when dependencies change.
