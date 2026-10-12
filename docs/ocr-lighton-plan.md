# Optional LightOnOCR-3-0.8B OCR backend (plan, 2026-10-12)

Status: planned, nothing built or measured. Resolves the "better OCR" part of HANDOFF Q4.

## Decision
Add LightOnOCR-3-0.8B as an **optional** OCR engine, run through the llama.cpp/mtmd we already vendor
(`digup-llama-sys` already binds `mtmd.h`). Defaults stay: Apple Vision on macOS, `ocrs` elsewhere.
Reasons: ParseBench Overall 74.6, Content Faithfulness 88.9 at ~0.9B params; Apache-2.0.
Shortlist and licence checks (model cards read 2026-10-12, not tested): LightOnOCR-3-0.8B, MinerU2.5-Pro-2605 (1.2B,
Apache-2.0, Qwen2-VL, official GGUFs), Infinity-Parser2-Flash (~2B, Apache-2.0). Rejected: rakedoc-nano / florin-parser-nano /
KDL-Frontier-Parser-nano (AGPL-3.0 chain), Chandra-ocr-2 (modified OpenRAIL-M).

## Facts from the repo
- `crates/digup-ocr` exposes stateless `recognize_rgb(w, h, rgb) -> Option<String>`; macOS = Vision, else `ocrs`.
- Vendored llama.cpp b11461 has `LLM_ARCH_QWEN35` and `tools/mtmd/models/qwen3vl.cpp`; it also has a `lightonocr`
  projector type (the older LightOnOCR). Whether LightOnOCR-3's vision tower loads is **unverified**.
- LightOnOCR-3 only follows an empty prompt or its `grounding` prompt (model card).

## Steps
1. (needs idle Mac) Get the GGUF + mmproj, run `llama-mtmd-cli` on a few screenshots. Pass = loads, text is sane.
   If it does not load: try MinerU2.5-Pro-2605 (Qwen2-VL, known to work in llama.cpp) or bump the llama.cpp pin.
2. Add `digup-ocr/src/llama_backend.rs` behind feature `lighton`: load model once (OnceLock + Mutex or a worker thread),
   empty prompt, greedy decode, cap output tokens, return text.
3. Engine selection by config/env (`vision` | `ocrs` | `lighton`); model download through `digup-models` with pinned
   revision and SHA-256, same as the embedder.
4. Eval on `evals/` screenshot sets: Vision vs ocrs vs lighton, search score and seconds per image. Record in
   `docs/BENCHMARKS.md` with machine and command (CLAUDE.md rule 6).
5. Add LightOnOCR-3 to `NOTICE`.

## Open
- Official GGUF/mmproj repo and quantization to pin (Q8_0 vs F16 not yet checked for this model).
- Speed on CPU/iGPU for Windows/Linux (unmeasured; estimate only).

## Model files (found and downloaded 2026-10-12)
No official GGUF exists (lightonai publishes safetensors only). Community GGUFs: mradermacher, prithivMLmods, Prathat.
Downloaded from `mradermacher/LightOnOCR-3-0.8B-GGUF` at revision `d76992384e501892b4e37bd55020d762d30b5b37` into
`models/lightonocr/` (git-ignored):
- `LightOnOCR-3-0.8B.Q8_0.gguf` 811,844,128 B, sha256 `b2955ffcce11b0c1fce6dd38d5303a0f3284161c48ade7f62bda9804ce5ba4f7`
- `LightOnOCR-3-0.8B.mmproj-f16.gguf` 204,987,200 B, sha256 `c70a0eeee9bf2c2626510631a4c707321d48a91f577673304dd8771f2664f9d2`
Not yet loaded or run. `build-llama` has `LLAMA_BUILD_TOOLS=OFF`, so there is no `llama-mtmd-cli`; step 1 will use a small Rust
example over `digup-llama-sys` (mtmd) instead of a second C++ build. Model card: use image longest side 2048 px, empty prompt.
