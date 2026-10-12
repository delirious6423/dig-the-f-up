# Dig-the-f-up

Search your files by what is in them, not by their names. Photos, screenshots, PDFs, documents, audio, video and
code all go into one on-device index built with Google DeepMind's EmbeddingGemma 2. Ask "the receipt from the
hardware store" or "a penguin on the ice" and get the file, the page or the moment. Nothing leaves your machine.

Dig-the-f-up is a performance-first Rust port of [DigUp](https://github.com/ARahim3/DigUp) by Abdur Rahim (Swift,
macOS only), aiming at macOS (Apple Silicon), Windows and Linux. The name nods to the f-tools that inspired the
speed work (fff, fsearch and friends).

## Status: early, macOS first

What works on an Apple Silicon Mac:
- The `dtfu` CLI: `index`, `search`, `status`, `eval`, `estimate`, `download`, `mcp` (a read-only MCP server for AI
  agents).
- Search quality equal to the original. On DigUp's own eval sets (synthetic, multilingual, code) the scores match the
  Swift app, and all 126 eval queries return the same top-10 order on the same index. Embeddings match the Swift app
  at cosine >= 0.999999 (Metal).
- A desktop app (Tauri 2) in `app/`: search as you type, kind filters, thumbnails, open a PDF at the matching page,
  folders, model download, live re-indexing. It is an unsigned build for now.

Not done yet:
- Windows and Linux have only been through compile checks and the unit tests in CI. The PDF (pdfium) and OCR (ocrs)
  backends for those platforms have never run against the real libraries.
- There are no signed or notarized macOS releases.

Measured numbers, machines and commands are in [`docs/BENCHMARKS.md`](docs/BENCHMARKS.md). Some highlights (M4 Mac):
- Vector scan: about 100 GB/s with NEON.
- Keyword stage at 1M segments: 138 ms, down from 7.5 s.
- First index of the 88-file test corpus: 31.4 s against the Swift app's 38.8 s (one run).
- A cold one-shot CLI query is still slower than Swift: 0.60 s against 0.45 s. The desktop app keeps the model
  loaded instead.

## Try it (Apple Silicon Mac)

```bash
scripts/link-models.sh && scripts/build-llama.sh   # once: finds/verifies the two GGUF files, builds llama.cpp with Metal
cargo build --release -p dtfu-cli
target/release/dtfu download                       # or set DTFU_MODELS to a folder that has them
target/release/dtfu index ~/Documents
target/release/dtfu search "a penguin on the ice"
```

The desktop app: see [`app/README.md`](app/README.md).

## Working on it

- [`docs/HANDOFF.md`](docs/HANDOFF.md): goal, state, decisions and roadmap.
- [`CLAUDE.md`](CLAUDE.md): how to work on the repo.
- `docs/spec-*.md`: the behaviour being ported, with `file:line` citations into the original. Fetch that source with
  `scripts/fetch-reference.sh`.
- `cargo test --workspace --release` runs everything, including the embedding parity test when the models are
  present.

## Licence

Apache License 2.0, see `LICENSE`. Dig-the-f-up is based on DigUp (MIT) and links llama.cpp (MIT). The embedding model is
EmbeddingGemma 2 and has its own terms. See `NOTICE` and `docs/THIRD-PARTY.md`.
