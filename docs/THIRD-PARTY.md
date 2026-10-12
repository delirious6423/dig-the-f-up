# Third-party components

What the DigUp desktop app (`app/`, macOS Apple Silicon) contains or needs at run time. Checked against the source
and the build scripts; the bundle contents were not inspected after a release build.

## Compiled into the app

| Component | Version / source | License | How it is included |
| --- | --- | --- | --- |
| llama.cpp (with mtmd) | b11461 plus `patches/llama.cpp-no-logits.patch` (from DigUp) | MIT | Static libraries from `scripts/build-llama.sh`, linked by `crates/digup-llama-sys` (its build script links `ggml-metal` on Apple targets). |
| Rust crates | see the table below and `scripts/gen-third-party-licenses.sh` | per crate | Compiled in (static). |

## Apple system frameworks (linked, not shipped)

Linked from the macOS SDK; they are part of macOS and are not in the bundle: Accelerate, Metal, MetalKit, Foundation
(`crates/digup-llama-sys/build.rs`) and PDFKit (PDF text and rendering, `crates/digup-extract-pdf`). Other system
frameworks reached through the Tauri and `objc2-*` crates are also system-provided; the full set was not enumerated.

## Not bundled

| Item | Why it is not in the app |
| --- | --- |
| EmbeddingGemma 2 model (`embeddinggemma-2-Q8_0.gguf`, `mmproj-embeddinggemma-2-Q8_0.gguf`) | Downloaded at run time from the pinned Hugging Face revision into the data folder. Not in the `.app`. Model license: Apache 2.0 (Google DeepMind). |
| ffmpeg / ffprobe | Video metadata and frames are read by running the `ffmpeg` and `ffprobe` programs that the user has installed (`crates/digup-extract-video`). Not bundled. Without them, video files cannot be indexed (`VideoError::Missing`, "ffmpeg not found"). |
| pdfium | Used only on non-macOS builds, or on macOS with the `pdfium` feature for tests. The macOS app uses PDFKit. Not bundled. |
| DigUp (Swift) | Not included. Its MIT copyright notice is kept in `NOTICE`. |

## Rust crates

The full list (name, version, declared license, repository) is produced by:

    scripts/gen-third-party-licenses.sh docs/THIRD-PARTY-CRATES.md

It is not committed yet: it lists about 670 crates from `cargo metadata`, including crates for other platforms
(for example Linux-only WebKit bindings), and licenses are as declared in each crate's manifest, not read from the
license files. Notes from the first run:

- Several crates are MPL-2.0 (file-level copyleft), mainly the Symphonia audio decoders and the Servo CSS
  crates; they are in the dependency graph. MPL-2.0 needs the source of those files to stay available if they are modified; unmodified use needs only the notice.
- One ring-based crate is "Apache-2.0 AND ISC"; the Unicode crates are "Unicode-3.0".
- A crate with no declared license is flagged `UNDECLARED` by the script. None were found in the last run.

Before a release, run the script, read the MPL and any unusual entries, and decide whether `NOTICE` needs more
than this document. The owner decides the final wording.
