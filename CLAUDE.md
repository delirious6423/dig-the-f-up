# CLAUDE.md: Dig-the-f-up

Rust port of DigUp (https://github.com/ARahim3/DigUp, Swift, macOS only): on-device semantic file search with
EmbeddingGemma 2, for macOS (Apple Silicon only; no Intel Mac targets), Windows and Linux, as fast as measurably possible.

- `CONTRIBUTING.md` has the rules (parity test, Q8_0 models only, index format, measure-don't-guess, portable builds),
  the layout and the commands. Follow it.
- Open work is in GitHub issues (`gh issue list`).
- `docs/porting/spec-*.md` describe the original's behaviour with `file:line` citations into `reference/digup-swift/`
  (fetch with `scripts/fetch-reference.sh`; read-only, never modify).
- Any speed claim goes in `docs/BENCHMARKS.md` with machine and command.
- Licence: Apache-2.0. Keep `NOTICE` (DigUp and llama.cpp MIT notices).
