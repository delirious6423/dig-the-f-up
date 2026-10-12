# CLAUDE.md: digup-rs

Cross-platform, maximum-performance Rust port of **DigUp** (https://github.com/ARahim3/DigUp, MIT): on-device semantic
search over a user's files (photos, PDFs, documents, audio, video, code) with Google DeepMind's EmbeddingGemma 2.
The original is Swift and macOS-only. This repo must run on **macOS (Apple Silicon only; Intel Macs are dropped, owner decision), Windows and Linux** and be as fast as we can make it.

**Read `docs/HANDOFF.md` fully before doing anything.** It has the goal, current state, decisions, verified facts,
the ordered roadmap with acceptance criteria, and the open questions. You are taking over a running project and are
expected to continue it autonomously to completion, not to start over or re-plan from scratch.

## How the owner wants you to work (standing preferences)
- **Be autonomous.** Work through the roadmap in `docs/HANDOFF.md` in order. Do not stop to ask for permission on
  things the roadmap already decides. Ask only for the items listed under "Open questions", and only when you are
  actually blocked on them. Pick the recommended default and keep going when you are not.
- **Use subagents proactively, without asking.** At every phase boundary, check whether the next step splits into
  independent pieces; if so run them in parallel in a single turn. Cheaper models for mechanical search and
  test-running, a strong model for review. Stay inline for small, single-file or tightly coupled steps.
- **Verify before you summarize.** After a batch of implementation subagents returns, or after substantial multi-file
  edits, run a verification round (independent agents that re-run the checks and review the diff), or state in one line
  why it is not needed. Read-only research batches don't need one. Never accept a subagent's "tests pass" without
  re-running the tests yourself.
- **Make anything you need from the owner frictionless.** If you need a file, link or action from them, give the
  direct link/command and nothing else; never make them hunt. They are on an Apple Silicon Mac.
- Report outcomes in a sentence or two, with measured numbers. Don't narrate steps.

## Non-negotiables
1. **The parity test is the safety net.** `cargo test -p digup-embed --release --test parity` must keep passing
   (all 9 reference vectors at cosine >= 0.999 vs the Swift app's). Run it after any change to `digup-embed`,
   `digup-llama-sys`, or `scripts/build-llama.sh`.
2. **Never use F16 GGUFs** for this model (activations overflow). Q8_0 files only, pinned HF revision `bfcd298...`.
3. **Index compatibility:** SQLite schema v5, 768-d float16 L2-normalized vectors, same prompts. Don't change any of
   these without updating `docs/spec-store-search.md` and deciding the `vector_source` tag question (HANDOFF section 11).
4. **Don't modify `reference/digup-swift/`.** It is the read-only Swift source at commit `6faa29e` that every
   `file:line` citation in `docs/spec-*.md` points to.
5. **Licensing:** keep `NOTICE` (DigUp is MIT; llama.cpp MIT; EmbeddingGemma 2 Apache 2.0).
6. Measure, don't guess. Every performance claim in code comments, docs or your reports must come from a benchmark you
   ran, with the machine and command noted. Estimates are labelled as estimates.
7. Don't commit or push unless the owner asks. When you do commit, end the message with the attribution lines your
   session's system prompt specifies.

## Layout
```
crates/digup-core        shared enums, constants, prompts            (done, 2 tests)
crates/digup-store       SQLite schema v5 + f16 vector scan          (partial, 4 tests, bench example)
crates/digup-crawl       file rules, classification, parallel crawl  (done, 26 tests, bench example)
crates/digup-llama-sys   bindgen + static link of llama.cpp b11461   (done)
crates/digup-embed       safe embedder (text/image/audio) + Pillow-exact resizer (done, 6 tests + parity test)
docs/HANDOFF.md          START HERE
docs/spec-pipeline.md    crawler/extractors/indexer spec, with Swift file:line citations
docs/spec-embedder.md    llama.cpp call sequence, preprocessing, worker protocol, parity test
docs/spec-store-search.md  schema, search/ranking algorithm, code extractor, evals
reference/digup-swift/   read-only Swift source of the original (commit 6faa29e)
scripts/build-llama.sh   builds llama.cpp b11461 + DigUp's patch into ./build-llama (Metal on macOS)
scripts/link-models.sh   finds the two GGUF files, links them into ./models, verifies SHA-256
patches/                 llama.cpp-no-logits.patch (from DigUp)
vendor/ build-llama/ target/ models/   generated or local, git-ignored
```

## Commands
```bash
scripts/link-models.sh            # finds/verifies the model files (looks in ~/Downloads first)
scripts/build-llama.sh            # ~5-15 min once; Metal on macOS, CPU elsewhere; BACKEND=cuda|vulkan|hip
cargo test --workspace --release  # everything, incl. the parity test (needs models + build-llama)
cargo test -p digup-embed --release --test parity -- --nocapture   # the safety net, prints cosines
cargo run --release -p digup-store --example bench_scan -- 200000
cargo run --release -p digup-crawl --example crawl_bench -- <dir> [threads]
```
`DIGUP_LLAMA_BUILD` / `DIGUP_LLAMA_SRC` override where `digup-llama-sys` looks for the build and headers.
`DIGUP_MODELS` overrides where the parity test looks for the GGUF files.
Use `RUSTFLAGS="-C target-cpu=native"` only for local benchmarks, never for shipped binaries (see HANDOFF section 9).

## Subagents (learned the hard way)
Coding agents must be `subagent_type: "builder"` (haiku, small tool set, starts ~9K tokens). General-purpose agents start at ~46K tokens of tool schemas and die with "Autocompact is thrashing" on haiku; keep them for research. Briefs must give exact line ranges (`grep -n '^## ' docs/*.md`); never have an agent read whole specs or HANDOFF.md.
