# Dig-the-f-up

Find your files by what is in them. Photos, screenshots, PDFs, documents, audio, video and code go into one
on-device index. Type "the receipt from the hardware store" or "a penguin on the ice" and get the file, the page or
the moment.

Everything runs on your machine with Google DeepMind's EmbeddingGemma 2. Nothing is uploaded; the only network use is
the one-time model download (865 MB).

Dig-the-f-up is a fast Rust port of [DigUp](https://github.com/ARahim3/DigUp) by Abdur Rahim (Swift, macOS only), built
to run on macOS, Windows and Linux. The name nods to the f-tools that inspired the speed work (fff, fsearch and friends).

## Status: early, macOS first

- **macOS (Apple Silicon):** works. The CLI and desktop app run end to end with Metal. The app is not signed yet, so
  macOS asks you to confirm it once (see [Install](#install)). Release packages for Windows and Linux are coming with
  the first tagged release; until then, build from source.
- **Windows and Linux:** they build and pass the unit tests in CI, but have not been verified on real machines
  ([#1](https://github.com/delirious6423/dig-the-f-up/issues/1), [#2](https://github.com/delirious6423/dig-the-f-up/issues/2)).

Search quality matches the original. On DigUp's own eval sets the scores are equal, and all 126 eval queries return the
same top 10 on the same index. Speed numbers are in [`docs/BENCHMARKS.md`](docs/BENCHMARKS.md).

## Install

Release packages are coming with the first tagged release. Get them from the
[GitHub Releases page](https://github.com/delirious6423/dig-the-f-up/releases).

**Mac (Apple Silicon)**

1. Download the `.dmg` file.
2. Open it.
3. Drag **Dig-the-f-up** into **Applications**.
4. Open **Dig-the-f-up** from Applications.
5. The first time, macOS says it cannot verify the app. This is because the app is not signed yet. Do this once:
   open **System Settings > Privacy & Security**, scroll down, click **Open Anyway**, and confirm.

**Windows**

1. Download the **Setup** `.exe` file and run it.
2. If **Windows protected your PC** appears, click **More info**, then **Run anyway**.
3. The app installs for your user account only. You do not need administrator rights.

**Linux**

- AppImage: download the file, allow it to run (right-click the file, open **Properties > Permissions**, and tick
  **Allow executing as program**; or run `chmod +x` on the file), then double-click it.
- Or install the `.deb` file with `sudo apt install ./file.deb`.

**First start**

- The app downloads what it needs on first launch: the search model (about 1 GB) and the OCR model (about 1 GB).
  You need an internet connection for this step. After that, search runs on your computer.
- Video support (ffmpeg) and PDF support are included.

Windows and Linux have not been checked on real machines yet (see [Status](#status-early-macos-first)).

## Build and run (Apple Silicon Mac)

For developers. You need Rust, CMake (`brew install cmake`) and, for video in a source build, ffmpeg
(`brew install ffmpeg`). Release packages already include ffmpeg.

```bash
git clone https://github.com/delirious6423/dig-the-f-up && cd dig-the-f-up
scripts/build-llama.sh                  # once, 5-15 min: builds llama.cpp with Metal
```

Desktop app:

```bash
cd app && cargo run --release           # or build a .app: cargo install tauri-cli && cargo tauri build --bundles app
```

On first start it offers to download the model, then you add folders with the gear button.

Command line:

```bash
cargo build --release -p dtfu-cli
target/release/dtfu download            # the model, once
target/release/dtfu index ~/Documents
target/release/dtfu search "a penguin on the ice"
```

## Using the app

- Type to search. Up/Down select, Enter opens, Cmd+Enter (or Cmd+click) shows the file in Finder.
- Filter by kind with the chips: screenshots, images, PDFs, docs, audio, video.
- A PDF hit opens at its page with the matching words searched. Audio and video hits are meant to jump to the moment in
  QuickTime Player or IINA (not yet tested against the real players).
- Esc or closing the window hides it. Cmd+Shift+Space brings it back (Ctrl+Shift+Space on Windows and Linux).
- New and changed files are picked up while the app runs.
- Text in screenshots and images is searchable. The app downloads LightOnOCR-3 (about 1 GB) after the search model.
  On Windows and Linux it reads all screenshots (until it has downloaded, two small Latin-only OCR models, 12 MB, fill
  in). On macOS Apple Vision reads first and LightOnOCR takes over where Vision finds no text or is unsure, which
  covers scripts Vision cannot read (Hebrew, Greek, Hindi, Bengali, Tamil and others). Once LightOnOCR is installed,
  screenshots already indexed are read again once (on macOS only those where Vision found no text).

## Your data

Everything lives in `~/Library/Application Support/Dig-the-f-up/`: the index, your folder list, the model files and a
small local log (Help > Show Logs). The CLI keeps its index in `~/.dtfu`. If the original DigUp app is installed,
its downloaded model in `~/Library/Application Support/DigUp/Models` is reused.

To uninstall, quit the app, delete it, and delete `~/Library/Application Support/Dig-the-f-up/` (and `~/.dtfu` if you
used the CLI).

## Contributing

See [`CONTRIBUTING.md`](CONTRIBUTING.md). Open work is tracked in [issues](https://github.com/delirious6423/dig-the-f-up/issues).

## Licence

Apache License 2.0, see [`LICENSE`](LICENSE). Based on DigUp (MIT) and llama.cpp (MIT); EmbeddingGemma 2 is under
Apache 2.0. See [`NOTICE`](NOTICE) and [`docs/THIRD-PARTY.md`](docs/THIRD-PARTY.md).
