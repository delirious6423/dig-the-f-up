# DigUp embedding layer: Rust porting spec

Source: DigUp at commit 6faa29e (`scripts/fetch-reference.sh` puts it in `reference/digup-swift`). Read-only.
Line refs are real file lines (`File.swift:N`). Anything marked UNVERIFIED I could not confirm from the repo or network.

## 0. Architecture in one paragraph

One model (EmbeddingGemma 2, 768-d output) runs on llama.cpp, pinned to tag **b11461** plus a one-line patch. Text goes through the text GGUF. Images and audio go through llama.cpp's `mtmd` library using a second GGUF ("mmproj") that holds the vision and audio encoders. The encoder output rows are fed as *embeddings* (not tokens) into the same non-causal text model, which is run with mean pooling and the result is L2-normalised. Output is 768 floats per input. The app talks to it either in-process (`LlamaEmbedder`) or through a helper subprocess (`digup worker`) over JSON lines. The Python worker is an eval-only reference, not used at runtime.

## 1. Model files

Source: `Sources/DigUpKit/ModelFiles.swift`, `Sources/LlamaRuntime/LlamaModels.swift`.

| role | file | bytes | sha256 |
|---|---|---|---|
| text model | `embeddinggemma-2-Q8_0.gguf` | 309,855,456 | `2188ac1deca4b77dffefd603c2776a9d76d9d74ec01841392982ebb840b09135` (ModelFiles.swift:26-27) |
| image + audio encoders (mmproj) | `mmproj-embeddinggemma-2-Q8_0.gguf` | 554,821,024 | `c4a8a52691ecef40618438928bdf9e68379b854e24166f292592353db0aab64f` (ModelFiles.swift:29-30) |

- Repo `ggml-org/embeddinggemma-2-GGUF`, pinned revision `bfcd298762cc34d0357ece5ebdd31791a3a374d8` (ModelFiles.swift:11-12).
- URL pattern: `https://huggingface.co/ggml-org/embeddinggemma-2-GGUF/resolve/<revision>/<name>` (ModelFiles.swift:20-22). It redirects to a CDN that supports HTTP Range. The app downloads with ranges and only renames a file to its final name after the sha256 passes (ModelFiles.swift:56-57, 74-75 comments). The downloader itself is `Sources/DigUpApp/ModelDownload.swift`, which I did not read.
- Quantization: Q8_0 for both files. The code deliberately never uses F16 files because "the model's activations overflow float16" (ModelFiles.swift:3-5). Weights derive from google/embeddinggemma-2 at HF commit 914f7f8. `dtype` reported as `"text-q8_0+mmproj-q8_0"` (LlamaEmbedder.swift:30).
- Text-only mode (queries) loads only the 310 MB text file. Full mode also loads the 555 MB mmproj. Memory is about 1 GB full or 0.25 GB text-only (LlamaEmbedder.swift:14).
- Lookup order (LlamaModels.swift:20-25): if `$DIGUP_MODELS` is set, only that folder. Otherwise `~/Library/Application Support/DigUp/Models` (ModelFiles.swift:36), then the HF cache snapshot `$HF_HUB_CACHE | $HF_HOME/hub | ~/.cache/huggingface/hub` + `models--ggml-org--embeddinggemma-2-GGUF/snapshots/<revision>/`. For Rust, replace the macOS path with the platform data dir.
- Loading: text GGUF via `llama_model_load_from_file`. mmproj via `mtmd_init_from_file(mmproj_path, text_model, params)`, so mtmd reads encoder weights from the mmproj and shares the text model for config (section 2).
- Identity strings that go into index fingerprints: model `google/embeddinggemma-2`, revision = the HF revision, runtime `llama.cpp-vectors1`, dim 768 (LlamaEmbedder.swift:28-31). `vectorVersion = 1` (line 22) is bumped only when vectors change beyond cosine 0.999. Keep an equivalent constant in Rust. Rust vectors will not be bit-identical, so the same cosine rule applies.

I could not fetch the HF API (proxy returned 403), so the sizes and checksums above are from the repo, not independently re-verified.

## 2. Exact llama.cpp calls, in order

Build tag b11461 (`scripts/build-llama.sh:8`: "the first tags with EmbeddingGemma 2 (text + vision + audio) are b11452+"). I confirmed on GitHub raw that b11461 contains `LLM_ARCH_GEMMA_EMBEDDING2` (`gemma-embedding2`, non-causal, symmetric SWA, pattern 6), `llama_process`, `llama_batch_ext_*`, `llama_embd`.

### Process setup (LlamaEmbedder.swift:299-304, 48-51)
1. `llama_log_set(cb, ud)` and `mtmd_helper_log_set(cb, ud)`. Only WARN and above is kept (CONT lines inherit the previous level). Everything else is dropped because load is chatty (lines 345-350).
2. `llama_backend_init()` once.

### Model + context (lines 55-74)
3. `p = llama_model_default_params(); p.n_gpu_layers = 999; model = llama_model_load_from_file(text_path, p)`. Everything is offloaded.
4. `cp = llama_context_default_params()` then set:
   - `n_ctx = n_batch = n_ubatch = maxTokens`, where `maxTokens = textOnly ? 512 : 2048`. One input must fit one micro-batch, because a non-causal model has to see the whole input in one ubatch.
   - `n_seq_max = 16` (the `maxSequences` constant, line 42)
   - `kv_unified = true` (any sequence may use the whole context, as llama-embedding does)
   - `embeddings = true`
   - `pooling_type = LLAMA_POOLING_TYPE_MEAN`
   - NOT set, so b11461 defaults apply (checked in llama-context.cpp `llama_context_default_params`): `n_threads = n_threads_batch = GGML_DEFAULT_N_THREADS = 4`, `flash_attn_type = AUTO`, `offload_kqv = true`, `op_offload = true`, `swa_full = true`, KV types default. There is no explicit normalization flag in llama.cpp. L2 normalisation is done by hand in `run()`.
   - `context = llama_init_from_model(model, cp)`
5. Full mode only (lines 75-89): `mtmd_context_params_default()` then `use_gpu = true`, `print_timings = false`, `warmup = true`, `image_min_tokens = 1`, `image_max_tokens = 1120`. Default `n_threads = 4`, `flash_attn_type = AUTO` for mtmd (mtmd.cpp:463-466). Then `media = mtmd_init_from_file(mmproj_path, model, params)`. `image_min_tokens = 1` is deliberate: mtmd must never rescale because the Swift code resizes first (section 3). `image_max_tokens = 1120` must be at least the largest budget.
6. `batch = llama_batch_ext_init(context)`, `vocab = llama_model_get_vocab(model)`, `embeddingSize = llama_model_n_embd_out(model)` (768), `rowSize = llama_model_n_embd_inp(model)` (width of encoder rows; likely 768 but UNVERIFIED, the code reads it at runtime so do the same).
7. Warm-up: two `embedTexts([Prompts.query("warm up")])` calls (lines 102-107) to build Metal pipelines. Do the same for CUDA/Vulkan kernel and shader compile.

### Text embedding (lines 127-138, 234-247, 250-297)
- Tokenize: `llama_tokenize(vocab, utf8, len, buf, cap, add_special=true, parse_special=true)`. If the result is negative, reallocate `-count` and call again. Capacity starts at `bytes+8`. BOS/EOS come from the GGUF's tokenizer flags; the comment says `<bos> text <eos>`. Whether EOS is added by the GGUF flag or elsewhere is UNVERIFIED, so confirm in the Rust parity test.
- Truncation: if `tokens.count > maxTokens` then `tokens[0..<max-1] + [llama_vocab_eos(vocab)]` (BOS kept, EOS forced at the end, mirroring transformers `truncation=True`).
- Packing: `reset()` clears the batch (`llama_batch_ext_clear`). For each text: if `sequences == 16` or `batch_tokens + n > maxTokens`, flush with `run()`. Otherwise `begin()` starts a new sequence id (`sequences += 1`, `position = 0`) and each token is added with `llama_batch_ext_add_token(batch, seq_id = sequences-1, token)`, then `place()`.
- `place(idx)` (lines 273-281): `pos = [position, 0, 0, 0]` (a 4-element array even for non-M-RoPE), `llama_batch_ext_set_pos(batch, idx, pos)`, `llama_batch_ext_set_output_embd(batch, idx, true)`, `position += 1`.
- `run()` (lines 284-297): `llama_process(context, LLAMA_PROCESS_TYPE_ENCODE, batch)`. Non-zero means error. Then for each sequence `llama_get_embeddings_seq(context, seq)` returns the mean-pooled 768 floats. L2-normalise in f64 accumulation: `scale = 1/sqrt(sum(x^2))`, and 0 if the sum is 0. Then `reset()` in a defer.
- Many sequences are processed in one `llama_process` call, with at most 16 sequences and at most 2048 tokens (512 for the query worker).

### Image / audio embedding (lines 162-229)
Per file, one at a time, errors isolated per file:
1. `mtmd_helper_bitmap_init_from_file(media, path, placeholder=false, mtmd_helper_init_opt_default())` returns `{bitmap, video_ctx}`. This decodes images with stb_image and audio with miniaudio, resampled to the model rate (16 kHz per comment at line 191). If `video_ctx != nil`, free it and error. Check `mtmd_bitmap_is_audio(bitmap)` matches the requested kind.
2. Image: `mtmd_bitmap_get_nx/ny`, compute target size (section 3), resize in Swift if needed, and `mtmd_bitmap_init(w, h, rgb_bytes)`.
3. Audio: `n = mtmd_bitmap_get_n_bytes/4` samples. `limit = 30 * mtmd_get_audio_sample_rate(media)`. If `n > limit`, take the first `limit` f32 samples with `mtmd_bitmap_init_from_audio(limit, ptr)`.
4. `chunks = mtmd_input_chunks_init()`. `text = {text: mtmd_default_marker(), text_len, add_special: true, parse_special: true}`. `mtmd_tokenize(media, chunks, &text, &[bitmap], 1)`. Non-zero is an error. So the prompt is just the media marker. The BOS/EOS and `<|image>` / `<image|>` (or audio) wrapper tokens come from mtmd's own template, so there is no custom text prompt for media.
5. `reset(); begin()`, then for each chunk: if `mtmd_input_chunk_get_type == MTMD_INPUT_CHUNK_TYPE_TEXT` then `mtmd_input_chunk_get_tokens_text(chunk, &n)` and add each token. Otherwise `mtmd_encode_chunk(media, chunk)` (must be 0), `rows = mtmd_get_output_embd(media)` (n_tokens x rowSize floats), and for each row `llama_batch_ext_add_embd(batch, seq, llama_embd{data: row, n_rows: 1, n_embd: rowSize})` (the batch copies it), then `place()`.
6. `run()` returns a single vector (`[0]`). Free bitmap and chunks.
- Media is never batched across files (one `llama_process` call per file). Only text is packed.

### What `llama.cpp-no-logits.patch` changes (15 lines, src/llama-context.cpp `output_reserve`)
`bool has_logits = true;` becomes `bool has_logits = !(cparams.embeddings && !cparams.causal_attn);`. Without it, `encode()` reserves and clears `n_vocab` floats per token even for an embeddings-only non-causal model. With Gemma's 262k vocabulary that is 1 MiB per token (about 2 GiB of memset per 2048-token input). The patch only skips an unused buffer, so it does not alter numerics. Applied with `git apply` over tag b11461 (build-llama.sh:30-32). Rust must carry the same patch, or confirm upstream has fixed it (UNVERIFIED for later tags).

### Build flags (build-llama.sh:43-54), macOS-only today
`-DCMAKE_BUILD_TYPE=Release -DBUILD_SHARED_LIBS=OFF -DGGML_METAL=ON -DGGML_METAL_EMBED_LIBRARY=ON -DGGML_NATIVE=OFF -DLLAMA_BUILD_MTMD=ON` and common/tools/server/examples/tests/app/ui/openssl all OFF; targets `llama mtmd`. Static libs combined: libllama, libmtmd, libggml, ggml-base, ggml-cpu, ggml-metal, ggml-blas, vendor-hash. Headers needed: llama.h, ggml*.h, mtmd.h, mtmd-helper.h. `GGML_NATIVE=OFF` means a portable CPU baseline, which costs CPU speed on x86 unless you enable AVX2 explicitly. `CLlama/module.modulemap` just lists the headers and links `llamacpp`, `c++`, and the Accelerate/Metal frameworks.

## 3. Input preprocessing

### Text prompts (Sources/DigUpKit/Model.swift:88-100)
- Query: `task: search result | query: {text}`
- Code query (index of source code, triggered by user typing `code:`): `task: code retrieval | query: {text}`
- Document (any document chunk, PDF page text, code stretch): `title: {title or "none"} | text: {text}` (empty or nil title becomes the literal `none`)
- Media (image, audio): no prompt. The mtmd template supplies the framing.
- The embedder itself does not add prompts. The caller does.
- Caller-side limits: PDF page text `prefix(4000)` chars (Indexer.swift:546, 551); document chunks about 1800 chars with 200 overlap, about 450 tokens (Extractors.swift:221-222); code stretch 1800 chars. Token ceiling inside the embedder: 2048 full, 512 query worker, truncated as above.
- Scores differ in scale per modality (text 0.75-0.86 vs image/audio 0.67-0.78, Model.swift:30-31), so search normalises per modality. This is outside the embedder.

### Image sizing (Sources/LlamaRuntime/ImageSizing.swift)
Goal: reproduce the HF Gemma 4 image processor pixels, because mtmd alone would not.

`targetSize(height, width, budget)` (lines 9-26), budget is soft tokens from {70, 140, 280, 560, 1120}:
```
patch=16, pooling=3, side=48
maxPatches = budget*9
factor = sqrt(maxPatches*256 / (h*w))
th = floor(factor*h/48)*48 ; tw = floor(factor*w/48)*48
if th==0 && tw==0 -> None ("image too thin")
maxSide = (maxPatches/9)*48 = budget*48
if th==0: th=48; tw=min(w/h*48, maxSide)     // INTEGER division w/h in Swift
elif tw==0: tw=48; th=min(h/w*48, maxSide)   // INTEGER division
```
Images are always scaled to fill the budget, including upscaling small ones. Each side is a multiple of 48. Integer division in the degenerate branch is intentional (test: 10x4000 at 140 gives (48, 6720)). Expected targets from the unit test: 1440x900 at 280 gives (h624, w1008); at 140 gives (432, 672); at 70 gives (288, 480); 768x768 at 280 stays 768x768; at 140 gives 528x528; 300x300 at 140 gives 528x528.

`resizeBicubic` (lines 30-): bit-exact port of Pillow's `Image.resize(BICUBIC)` on packed RGB8: a = -0.5 cubic kernel, support `2*max(scale,1)`, kernel widened when shrinking (downscale), 22-bit fixed point (`precision = 22`), rounding `+ (1<<21)` then `>>22` and clamp 0..255. Two passes, horizontal first then vertical, and a pass is skipped on an axis whose size is unchanged. Per-output-pixel coefficient generation: `center=(out+0.5)*scale`, `first=max(int(center-support+0.5),0)`, `count=min(int(center+support+0.5),inSize)-first`, weights normalised by their sum, then `int(w*2^22 + (w<0 ? -0.5 : 0.5))`. This is checked "bit for bit" against Pillow by tests on a 7x5 image (ImageSizingTests.swift lines 22-42 of that file: shrink to 4x3, grow to 10x6, one axis to 7x2, identity). Copy the byte arrays from ImageSizingTests.swift as Rust unit tests. In Rust, `image` crate's CatmullRom is not identical (different a and rounding), so port this code directly.

Pre-resize (indexer, before the embedder): macOS ImageIO thumbnail to maxPixel 1536 for screenshots (PNG), 1024 for photos (JPEG q0.9), with EXIF orientation applied; min side 256; video frames at 1024 (Indexer.swift:371-376, 410; Extractors.swift:37-58). These are written to temp files that the embedder then reads via stb_image. In Rust, replace ImageIO with a decoder plus a resize or orient step. The parity test only feeds the embedder file-level inputs, so it does not constrain this stage. Note that JPEG-then-PNG re-encoding is lossy, so to match macOS vectors exactly you would need to match the ImageIO resampling, which is not feasible. Expect cosine of about 0.99+ against macOS, not 0.999.

### Audio
- The embedder accepts any file miniaudio decodes (wav, mp3, flac per mtmd-helper.h), auto-detected by magic bytes. It truncates to the first 30 s at the model's rate (`mtmd_get_audio_sample_rate`, 16 kHz). The reference "ignores audio past 30 s" (LlamaEmbedder.swift:11-13).
- Indexer side (MediaExtractors.swift:28-): decode with AVAssetReader to 16 kHz mono 16-bit PCM, cut into `audioWindow = 30 s` windows with `audioHop = 25 s` (5 s overlap; Model.swift:66-67), skip windows with RMS <= 0.003 (about -50 dBFS), tail window kept if it has at least 2 s of new audio, windows written as 44-byte-header 16-bit WAV files (`writeWAV`) and passed to the embedder by path. Whole audio is streamed, so memory is one window. The Rust port can pass f32 samples directly with `mtmd_bitmap_init_from_audio(n, f32*)` and skip the WAV write; samples are Int16/32768 as f32 (I assume; mtmd's own int16 WAV decode gives the same scaling, UNVERIFIED).
- Video: no direct support in the embedder (rejects `video_ctx`). The indexer feeds keyframes every 3 s (`frameInterval`) at budget 70 plus the soundtrack as audio windows.

### Where each budget is used (Model.swift:61-64)
photo 140, screenshot 280, PDF page image 280, video frame 70. The default for the worker op "image" with no budget is 280 (WorkerService.swift:29).

## 4. Worker protocol and lifecycle

### Spawn
- `EmbeddingWorker` (Sources/DigUpKit/Worker.swift:108-) launches a child `Process` with stdin and stdout pipes. stderr goes to a log file, or to /dev/null if none. Arguments: `["worker"]` plus `--text-only` for the query encoder (`WorkerConfig.helper`, lines 55-58). The executable is the bundled `Contents/Helpers/digup` (Engine.swift:921). Environment is the parent's plus overrides, notably `DIGUP_MODELS` (Engine.swift:922). QoS is `.userInitiated` for the query encoder (App Nap and E-cores can slow a search from 125 ms to 650 ms, Worker.swift:46-48) and `.utility` for indexing.
- Handshake: the child loads the model, runs warm-up, then prints one line `{"ready":true,"info":{model,revision,runtime,dtype,dim,text_only,load_seconds}}` (WorkerService.swift:8-24). On failure it prints `{"ready":false,"error":...}` and exits 1. The parent blocks on this line.

### Protocol: JSON lines over stdio, no binary framing
Requests, one JSON object per `\n`-terminated line on stdin:
```
{"id":N,"op":"text","inputs":["...prompted text...", ...]}
{"id":N,"op":"image","inputs":["/abs/path.png",...],"budget":280}
{"id":N,"op":"audio","inputs":["/abs/path.wav",...]}
{"id":N,"op":"info"}
{"id":N,"op":"quit"}
```
Reply, one line each:
```
{"id":N,"ok":true,"dim":768,"count":K,"vectors":"<base64 of K*768 little-endian float32>","errors":[null|"msg",...]}
{"id":N,"ok":false,"error":"..."}          // whole-request failure (500-char cap)
```
- A failed item gets a zero vector and a string in `errors`. Image and audio per-file failures are isolated; text failures fail the whole request.
- The reply must carry the same id. The parent treats a mismatch as an error ("reply out of order"). It is strictly request/response, one outstanding request, with no pipelining and no concurrency. The Swift parent decodes replies with snake_case keys (Worker.swift:217-219).
- Image budget must be in {70,140,280,560,1120}, else error.
- Input paths are absolute filesystem paths. The worker reads the files itself. Text is passed inline.

### Lifecycle
- Query encoder: started lazily on first search once the text model is downloaded (Engine.swift:910-925), kept resident, and exits after `encoderIdleSeconds` idle (about 0.25 GB returned to the OS), restarted on the next panel open (Engine.swift ~940-945 comments). `close()` sends `{"op":"quit"}`, closes stdin, waits up to 3 s polling at 20 ms, then `terminate()` (Worker.swift:175-184). The child also exits on stdin EOF. The parent ignores SIGPIPE (main.swift:7).
- Indexing (CLI and app indexer) uses the full embedder; `makeEmbedder` (main.swift:184-191) returns `LlamaEmbedder` in-process by default, so there is no subprocess for indexing unless `DIGUP_WORKER_DIR` is set. The reason for a subprocess at all: the model memory goes back to the OS when the process exits (Worker.swift:106-107).
- Batching and backpressure: the batch is whatever the caller sends in one request. Indexer sends one file's worth: all images of a job (`images(...)`, Indexer.swift:498), all pages/chunks of a doc or PDF (a step of 30 pages or 40 chunks), all audio windows or frames of a file. Internal packing is up to 16 text sequences per `llama_process` call; media runs one file per call. There is no queue: the stdio pipe is the backpressure, since the child reads the next line only after replying. Indexer overlaps CPU decode with embedding by preparing the next job on a utility queue while the current one is embedded (Indexer.swift:228-229, `PendingBatch` semaphore at 330-345).

### worker/digup_worker.py
Reference implementation of the same protocol on MLX/mlx-vlm (bf16), requiring uv, Python 3.12, `mlx-vlm @ git fec3f50...`, `mlx==0.32.3`, `transformers==5.19.0`, etc. (pyproject.toml). It is used only when `DIGUP_WORKER_DIR` is set (WorkerConfig.python, Worker.swift:61-79; "the reference runtime for evals"), and it is the ground truth that llama.cpp was validated against (cosine >= 0.999 text/images, 0.9998 on 30 s audio, LlamaEmbedder.swift:10-11). It is NOT used at runtime in the shipped app. Its knobs document the reference semantics: batch sizes text 16 / image 4 / audio 2 (line 29), `truncation=True, max_length=2048, padding=True` for text (line 112), image budget set via `processor.image_processor.max_soft_tokens`, media via `apply_chat_template` with `{"type": kind, "url": path}`, output `text_embeds` (pooled and normalised). It decodes audio with miniaudio and images with Pillow. For Rust, it is useful as a source of parity vectors, not as a dependency.

## 5. Reference-vector parity test

Files: `Tests/LlamaRuntimeTests/ReferenceVectorTests.swift`, `Reference/{vectors.json,photo.jpg,screen.png,speech.wav}`. The input files total about 300 KB.

- `vectors.json` structure: `{"made_with":"llama.cpp b11461","vector_version":1,"vectors":{name: [768 floats]}}`, keys sorted, 86 KB. Nine vectors, each already L2-normalised (I checked: norm 1.0 for all):
  `query`, `query-bn`, `query-ar`, `document`, `document-long`, `screen-280`, `photo-140`, `frame-70`, `speech`.
- Inputs (lines 21-33 of the test):
  - `query` = `Prompts.query("payment declined error")`
  - `query-bn` = `Prompts.query("ইলিশ মাছ রান্না")`, `query-ar` = `Prompts.query("متى تغلق المكتبة")` (multilingual)
  - `document` = `Prompts.document(title:"Tokyo itinerary", text:"Flight to Tokyo Haneda on November 3. Then the train to Kamakura to see the Great Buddha, and back for dinner in Shibuya.")`
  - `document-long` = `Prompts.document(title:nil, text: join(" ", for i in 1...60: "Note \(i): the review moved to room \(i % 7 + 2) because the projector in the main hall failed again, and the team agreed to send the slides the night before."))`. About 1,800 tokens, so it exceeds the sliding-attention window and exercises global layers and the 2048 limit.
  - images: (`screen-280`, screen.png, 280), (`photo-140`, photo.jpg, 140), (`frame-70`, photo.jpg, 70)
  - audio: `speech` = speech.wav (16 kHz WAV, 128 KB, roughly 4 s)
- Run mode: `LlamaEmbedder(textOnly:false)`, all 5 texts in a single `embedTexts` call (so the batching/packing path is covered too), then images one call each, then audio.
- Pass criterion (lines 19, 67-74): for every name, **cosine = dot(made, reference) >= 0.999** (plain dot product because both are unit-norm). The set of keys must match exactly. `vector_version` must equal the code's `vectorVersion`. No absolute element tolerance.
- Regeneration: `DIGUP_WRITE_REFERENCE=1`. It refuses to overwrite if the worst cosine < 0.999 while the version is unchanged (forces a version bump). Tests are skipped if model files are absent.
- Rust plan: load vectors.json with serde, build the same 9 inputs (Rust must produce identical prompt strings, including the Bengali and Arabic bytes), run the embedder, assert cosine >= 0.999 for text and the audio vector. For the image vectors use the same PNG/JPG files and the same ImageSizing port, so 0.999 is fair if the decoders agree (both go through mtmd's stb_image in the original). If Rust decodes images itself with another decoder, JPEG IDCT differences can cost a bit, so measure first and loosen to something like 0.995 only for photo-140 and frame-70 if needed. CPU vs Metal vs CUDA vs Vulkan floating-point differences also apply; the Q8_0 weights are the same, and I expect cosine >= 0.9995, but that is UNVERIFIED for non-Metal backends. Run the test per backend.
- Also reuse the Pillow-exactness tests (ImageSizingTests) and the targetSize table.

## 6. Rust options

Facts I verified by downloading crates 0.1.159 (published 2026-10-07; both `llama-cpp-2` and `llama-cpp-sys-2`) from crates.io and reading their sources, and by fetching raw files from github.com/ggml-org/llama.cpp at b11461.

### (a) llama-cpp-2 / llama-cpp-sys-2 0.1.159
- Features (sys crate Cargo.toml): `cuda`, `cuda-no-vmm`, `metal`, `vulkan`, `rocm`, `opencl`, `mkl`, `openmp`, `mtmd`, `dynamic-link`, `dynamic-backends`, `system-ggml`, `static-*`. The safe crate exposes `cuda, metal, vulkan?`: its feature list shows `cuda`, `metal`, `mkl`, `mtmd`, `opencl`, `openmp`, `rocm`, `system-ggml*`, `dynamic-*`. I did NOT see a `vulkan` feature forwarded in llama-cpp-2's Cargo.toml features block (only in the sys crate), so for Vulkan, depend on llama-cpp-sys-2 directly or enable the sys feature through a Cargo feature unification; UNVERIFIED that this works cleanly. build.rs has Vulkan (Windows, Linux, Android), ROCm, CUDA, Metal branches.
- Multimodal: `mtmd` feature gives a safe `MtmdContext` (init_from_file, tokenize, encode_chunk, support_audio, get_audio_sample_rate), `MtmdBitmap::{from_file, from_image_data, from_audio_data, is_audio, data}`, `MtmdInputChunks`. The sys crate bindgen allowlist `mtmd_.*` over mtmd.h and mtmd-helper.h, so every C function is reachable raw. The safe wrapper does NOT expose `mtmd_get_output_embd` (the only mention is a comment at mtmd.rs:750), which we need to take encoder rows. Use the raw sys call `llama_cpp_sys_2::mtmd_get_output_embd`. The safe `MtmdContextParams` field coverage (image_min_tokens / image_max_tokens) I did not check.
- **Blocking problem: the llama.cpp vendored in 0.1.159 is too old for this design.** Its `src/llama-arch.cpp` has `LLM_ARCH_GEMMA_EMBEDDING` (EmbeddingGemma 1) but no `gemma-embedding2`, and its `include/llama.h` has no `llama_process`, `llama_batch_ext_*` or `llama_embd` (grep count 0). It does have gemma4, gemma4v/gemma4a mtmd model files, and `mtmd_helper` video. So EmbeddingGemma 2 GGUFs will not load with the crate as published, and the Swift calls (`llama_process`, `llama_batch_ext_add_embd`, which are how encoder rows are injected) do not exist in its bindings. The classic API has `llama_batch` with an `embd` pointer and `llama_encode`, so the port could use that if the architecture were present, but only after a llama.cpp bump. The vendored source is a git submodule inside the crate package, so you must either wait for a crate release that bumps past b11452+, or vendor a fork with a newer `llama.cpp` submodule (path/git dependency). I could not tell how quickly the crate tracks upstream (versions 0.1.156 to 0.1.159 landed 2026-09-02 to 2026-10-07, roughly weekly, which suggests it may catch up soon, but that's inference).
- Safe API gaps: `LlamaContext::encode` takes the crate's `LlamaBatch`; embedding rows would need raw calls. `embeddings_seq_ith` exists. No safe `llama_batch_ext`.
- The no-logits patch would need to be applied to the vendored source (build.rs compiles `llama.cpp` in the package). Possible with a patched fork or a `[patch]` section; not possible via features.

### (b) Own bindgen over pinned llama.cpp b11461 (recommended)
- Make a `-sys` style crate: git submodule (or downloaded tarball with sha check) of llama.cpp at b11461, apply `llama.cpp-no-logits.patch`, build via `cmake` crate with `LLAMA_BUILD_MTMD=ON`, tools/server/common OFF, static libs, and bindgen over `llama.h`, `ggml*.h`, `mtmd.h`, `mtmd-helper.h` (exactly what Package.swift/CLlama does today). This is a direct translation of `build-llama.sh` and `LlamaEmbedder.swift`, and the call sequence in section 2 maps one-to-one, so risk is low and parity with Swift is by construction.
- Backend flags map to CMake: `-DGGML_METAL=ON` (macOS), `-DGGML_CUDA=ON`, `-DGGML_VULKAN=ON`, `-DGGML_HIP=ON`. Upstream ggml supports all four, and mtmd/clip uses the same backend registry with `use_gpu`. Whether the gemma-embedding2 graph and the gemma4 vision/audio encoder graphs have all ops implemented on each non-Metal backend (e.g. conformer/audio ops, SWA symmetric mask, flash-attn non-causal) is UNVERIFIED. Test on each backend, and note op_offload falls back to CPU per-op automatically if a backend lacks an op, at a speed cost.
- Cost: you own bumping the pin and re-running the reference test, which the project already does.
- `dynamic-backends` (GGML_BACKEND_DL) is worth considering so one binary can pick CUDA/Vulkan/CPU variants at runtime. I did not verify that mtmd/clip honours it. UNVERIFIED.

### (c) ONNX Runtime / candle
- Neither is verified for EmbeddingGemma 2. My web search found no authoritative source for an ONNX export or a candle implementation of EmbeddingGemma 2 with image + audio encoders (results were generic blogs). The model needs: a Gemma-style non-causal text transformer with sliding-window + global layers, a Gemma 4 vision tower with 3x3 pooling and variable-size input, an audio conformer front end (log-mel feature extraction) and projection. That is a large re-implementation, and parity to 0.999 against Q8_0 llama.cpp vectors is not guaranteed (the project's own reference is bf16 MLX, so f32/bf16 candle vectors would also be fine in cosine terms, but the preprocessing, tokenizer, mel front end must be ported).
- ONNX Runtime backends: CUDA, TensorRT, DirectML, CoreML, ROCm/MIGraphX, but Vulkan is not a mainstream EP. candle: CUDA, Metal; no Vulkan or ROCm that I know of (from memory, UNVERIFIED now).
- Verdict: not recommended unless llama.cpp dependency is a hard no.

### Backend summary (what the build/features give)

| option | Metal | CUDA | Vulkan | ROCm/HIP | multimodal (mtmd) for EG2 |
|---|---|---|---|---|---|
| (a) llama-cpp-2 0.1.159 | feature `metal` | `cuda` | sys feature `vulkan` (not forwarded by safe crate in what I read) | `rocm` | mtmd safe wrapper present, but vendored llama.cpp lacks EG2 arch and `llama_process`: **cannot run EG2 as published** |
| (b) own bindgen on b11461 | yes (`GGML_METAL`) | `GGML_CUDA` | `GGML_VULKAN` | `GGML_HIP` | yes, full mtmd + helper, identical to Swift |
| (c) ort / candle | CoreML EP / candle-metal | yes | no (ort) | partly | no ready EG2 support found |

## 7. Performance levers in this layer (with evidence from the Swift code)

Current state:
- GPU layers: `n_gpu_layers = 999`, `use_gpu = true` for mtmd (LlamaEmbedder.swift:56, 78). All offloaded.
- Context/batch sizes: `n_ctx = n_batch = n_ubatch = 2048` (512 for the query encoder), `n_seq_max = 16`, `kv_unified = true` (63-68). One ubatch per run, so a 2048-token slab is the maximum. There is no KV cache in play (non-causal encode only), so there is no KV/prefix reuse to exploit; the context exists only for buffers.
- Packing: texts are packed up to 16 sequences or 2048 tokens per `llama_process` (lines 127-138). This is the main throughput lever for text. Media has no cross-file batching (lines 140-149 map each path to its own call), so the vision/audio encoder GPU is underused when there are many small images. Candidates: batch several images' encoder rows into one `llama_process` (the text-model pass of a 140-token image is small, the encoder pass dominates; mtmd encodes one chunk at a time, so true batching of encoders needs mtmd changes).
- Flash attention: not set, so `AUTO` (llama.cpp enables it if the backend supports it). Setting `flash_attn_type = ENABLED` explicitly would remove the dependency on auto-detect; on CUDA/Metal AUTO normally picks it. UNVERIFIED whether the non-causal symmetric-SWA mask is supported by FA kernels on Vulkan/ROCm.
- Threads: not set; defaults 4 (`GGML_DEFAULT_N_THREADS`) for both llama ctx and mtmd. Irrelevant under full GPU offload but decisive for CPU-only machines (Linux/Windows without GPU): set `n_threads`/`n_threads_batch` to physical cores, and `mtmd_context_params.n_threads` too. Also `GGML_NATIVE=OFF` in build-llama.sh:44 limits CPU kernels to a baseline; for x86 builds enable AVX2/AVX512 variants (or runtime-dispatch via `GGML_CPU_ALL_VARIANTS` + dynamic backends; UNVERIFIED for this tag).
- Keep model resident: the query encoder stays up between searches and exits after an idle timeout (Engine.swift:~944); loading is expensive because of Metal pipeline build, hence the 2x warm-up (LlamaEmbedder.swift:102-107) and `mtmd warmup = true`. In Rust, keep one process-wide embedder and the same warm-up for shader/kernel compile (Vulkan pipeline compilation and CUDA JIT matter even more).
- Two embedder instances: text-only (512 ctx, 0.25 GB) for queries and full (2048 ctx, 1 GB) for indexing; they are separate processes/instances today.
- Pipelining: the indexer decodes/resizes the next job on a utility queue while the GPU embeds the current one (Indexer.swift:228-229). The embedder is single-threaded ("one thread at a time", LlamaEmbedder.swift:14) and `embedImages` iterates sequentially. Image decode+resize inside the embedder (stb_image + the Swift bicubic, which is scalar and runs on the embedder thread in `load()`) is on the critical path; moving `resizeBicubic` to the prep queue or using SIMD/rayon would help (a 1536 px screenshot to a 1008x624 resize is CPU-bound). The audio WAV round trip (write 30 s WAV, miniaudio decode and resample) can be avoided by passing f32 samples to `mtmd_bitmap_init_from_audio`.
- Logits patch: avoids about 1 MiB/token of useless memset, which is the biggest single per-token cost the project found (patch comment); keep it.
- Quantization: Q8_0 weights are fixed for vector compatibility. A different quantization changes vectors (cosine loss, a `vectorVersion` bump).
- Reference throughput numbers (query path 125 ms with app QoS vs 650 ms without; text 18 ms vs 6 ms warmed, from digup_worker.py:91) show warm-up and scheduling dominate for single queries.
- Output size: vectors stored as float16 in the index (Vectors.swift:1-12), float32 over the pipe (base64). A Rust in-process embedder can skip base64 entirely.

## 8. Rust port checklist (distilled)

1. Vendored llama.cpp b11461 + patch, `cmake` crate, flags per backend, bindgen for llama.h/ggml/mtmd/mtmd-helper.
2. Safe-ish `Embedder` with `embed_texts(&[String])`, `embed_images(&[Path], budget)`, `embed_audio(&[Path or &[f32]])`, `close()`, same limits (16 seq, 2048/512 tokens, 30 s audio, budgets set).
3. Port `ImageSizing` verbatim, with the Pillow-bit-exact tests.
4. Port `Prompts` verbatim.
5. Model download: ranges + sha256 + atomic rename; pinned revision; two files.
6. Optional helper-process mode with the JSON-lines protocol above (same ops, base64 f32 LE) if memory release on idle matters; otherwise in-process.
7. Parity test from vectors.json at cosine >= 0.999, per backend.

## 9. Not verified

- HF file sizes and checksums (HF unreachable through the proxy; values copied from the repo).
- Whether the GGUF adds EOS itself (the `llama_tokenize(add_special)` result is used as-is).
- `llama_model_n_embd_inp` value (read at runtime).
- Non-Metal backend op coverage for gemma-embedding2 and the gemma4 vision/audio encoders.
- Whether llama-cpp-2 forwards a `vulkan` feature (the sys crate has it; the safe crate's feature block did not show it).
- candle / ONNX Runtime capability for EmbeddingGemma 2 (no authoritative source found).
- Newer llama.cpp tags: whether the logits patch is still needed, and whether API names (`llama_process`, `llama_batch_ext_*`) stay stable (they are recent and the crate's own wrapper does not have them).
- Unread: `Sources/DigUpApp/ModelDownload.swift`, `Engine.swift` beyond lines ~900-945, `IndexService.swift`, main.swift beyond the embedder-related lines. The Vendor/ directory is not in the checkout (generated by build-llama.sh).
