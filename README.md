# digup-rs

A cross-platform (macOS, Windows, Linux), performance-first Rust port of [DigUp](https://github.com/ARahim3/DigUp):
search your files by what's in them (photos, PDFs, documents, audio, video, code) with Google DeepMind's
EmbeddingGemma 2, entirely on-device.

Status: engine foundations done and verified against the original's reference vectors; indexing pipeline, extractors,
search, CLI and app are next. See `docs/HANDOFF.md` for the full picture and `CLAUDE.md` for how to work on it.

Licence: MIT. Based on DigUp (MIT) and llama.cpp (MIT); the model is EmbeddingGemma 2 (Apache 2.0). See `NOTICE`.
