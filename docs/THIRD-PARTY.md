# Third-party components

What the Dig-the-f-up desktop app (`app/`; macOS Apple Silicon, Windows x64, Linux x86_64) contains or needs at run time. Checked against the source
and the build scripts; the bundle contents were not inspected after a release build.

## Compiled into the app

| Component | Version / source | License | How it is included |
| --- | --- | --- | --- |
| llama.cpp (with mtmd) | b11461 plus `patches/llama.cpp-no-logits.patch` (from DigUp) | MIT | Static libraries from `scripts/build-llama.sh`, linked by `crates/dtfu-llama-sys` (its build script links `ggml-metal` on Apple targets). |
| Rust crates | see the table below and `scripts/gen-third-party-licenses.sh` | per crate | Compiled in (static). |

## Shipped as separate programs or libraries (release packages)

| Component | Version / source | License | How it is included |
| --- | --- | --- | --- |
| FFmpeg: `ffmpeg` and `ffprobe` executables | 7.1.5. LGPL-2.1-or-later build made by `scripts/build-ffmpeg.sh` with `--disable-gpl --disable-nonfree`, plus dav1d 1.5.4 (BSD-2-Clause, VideoLAN) statically linked for AV1 decoding. Unmodified source: https://ffmpeg.org/releases/ffmpeg-7.1.5.tar.xz (SHA-256 `de668509caf9e35e3cd162473441fdb29538c6d96ed080292b3cf9e6fc5d558f`); dav1d: https://downloads.videolan.org/pub/videolan/dav1d/1.5.4/dav1d-1.5.4.tar.xz (SHA-256 `686616b7c69eb88d44459391ab25cac13b6647a3b288835c5784e71c1514a5c5`) | LGPL-2.1-or-later (FFmpeg); BSD-2-Clause (dav1d) | Bundled in the app packages. Run as separate programs, not linked into the app. The script and its configure flags let anyone rebuild the programs or replace them with their own build. |
| pdfium | Prebuilt library from the bblanchon/pdfium-binaries release `chromium/8066`. Fetched and SHA-256-checked by `scripts/fetch-pdfium.sh`. | BSD-3-Clause / Apache-2.0 (Chromium and PDFium authors) | Windows and Linux packages only. The licence file ships in the package as `LICENSE-pdfium.txt`. macOS uses PDFKit instead. |

## Apple system frameworks (linked, not shipped)

Linked from the macOS SDK; they are part of macOS and are not in the bundle: Accelerate, Metal, MetalKit, Foundation
(`crates/dtfu-llama-sys/build.rs`) and PDFKit (PDF text and rendering, `crates/dtfu-extract-pdf`). Other system
frameworks reached through the Tauri and `objc2-*` crates are also system-provided; the full set was not enumerated.

## Not bundled

| Item | Why it is not in the app |
| --- | --- |
| EmbeddingGemma 2 model (`embeddinggemma-2-Q8_0.gguf`, `mmproj-embeddinggemma-2-Q8_0.gguf`) | Downloaded at run time from the pinned Hugging Face revision into the data folder. Not in the `.app`. Model license: Apache 2.0 (Google DeepMind). |
| LightOnOCR-3-0.8B (`LightOnOCR-3-0.8B.Q8_0.gguf`, `LightOnOCR-3-0.8B.mmproj-f16.gguf`) | Windows and Linux: downloaded by the app at run time (about 1 GB); macOS: only with `dtfu download`. Fetched from the community GGUF conversion mradermacher/LightOnOCR-3-0.8B-GGUF at a pinned revision into `<models dir>/lightonocr`, pinned by size and SHA-256 (`crates/dtfu-models`). Not bundled. Model by LightOn, Apache-2.0 (https://huggingface.co/lightonai/LightOnOCR-3-0.8B); used unmodified. |
| ocrs OCR models (`text-detection.rten`, `text-recognition.rten`) | Windows and Linux only: downloaded at run time (12 MB) from the ocrs project's model bucket into `<data dir>/Dig-the-f-up/ocr`, pinned by size and SHA-256 (`crates/dtfu-models`). Not bundled. Models by Robert Knight, CC-BY-SA 4.0 (https://huggingface.co/robertknight/ocrs), trained on HierText (CC-BY-SA 4.0); used unmodified. The `ocrs` and `rten` crates are MIT/Apache-2.0. |
| ffmpeg / ffprobe | Source builds: read by running the `ffmpeg` and `ffprobe` programs that the user has installed (`crates/dtfu-extract-video`). Without them, video files cannot be indexed (`VideoError::Missing`, "ffmpeg not found"). Release packages bundle them; see the section above. |
| pdfium | Used only on non-macOS builds, or on macOS with the `pdfium` feature for tests. The macOS app uses PDFKit. Source builds: not bundled. Windows and Linux release packages bundle it; see the section above. |
| DigUp (original Swift app by Abdur Rahim) | Not included. Its MIT copyright notice is kept in `NOTICE`. |

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
