# digup-rs

A cross-platform (macOS, Windows, Linux), performance-first Rust port of [DigUp](https://github.com/ARahim3/DigUp):
search your files by what's in them (photos, PDFs, documents, audio, video, code) with Google DeepMind's
EmbeddingGemma 2, entirely on-device.

## Status

The engine, indexing pipeline and CLI work end to end on Apple Silicon (Metal). On the original's eval sets the Rust
CLI scores the same as the Swift app (synthetic, multilingual and code), and returns the identical top-10 order for
all 126 eval queries on the same index. Measured numbers and methods: `docs/BENCHMARKS.md`.

Done: crawler, SQLite index (schema v5, same as the original), search (z-score fusion), extractors for images,
screenshots (Apple Vision OCR), PDF (PDFKit), documents (txt, md, rtf, docx, odt, html), audio, video (ffmpeg),
code; the `digup` CLI (`index`, `search`, `status`, `eval`, `estimate`).
Not done: Windows/Linux PDF (pdfium) and OCR backends, model downloader wiring, the desktop app, packaging.
`docs/HANDOFF.md` has the roadmap; `CLAUDE.md` says how to work on the repo.

## Try it (Apple Silicon Mac)

```bash
scripts/link-models.sh && scripts/build-llama.sh      # once: finds the two GGUF files, builds llama.cpp with Metal
cargo build --release -p digup-cli
export DIGUP_MODELS=$PWD/models
target/release/digup index --index ~/.digup ~/Documents
target/release/digup search --index ~/.digup "a penguin on the ice"
```

Licence: MIT. Based on DigUp (MIT) and llama.cpp (MIT); the model is EmbeddingGemma 2 (Apache 2.0). See `NOTICE`.
