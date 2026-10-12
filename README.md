# Dig-the-f-up

Find your files by what is in them. Photos, screenshots, PDFs, documents, audio, video and code go into one
on-device index. Type "the receipt from the hardware store" or "a penguin on the ice" and get the file, the page or
the moment.

Everything runs on your computer with Google DeepMind's EmbeddingGemma 2. Nothing is uploaded; the only network use is
downloading the model once.

## Download

| | |
|---|---|
| **Mac** (Apple Silicon: M1 or newer) | [**Download for Mac**](https://github.com/delirious6423/dig-the-f-up/releases/latest/download/Dig-the-f-up-macos-arm64.dmg) |
| **Windows** (64-bit) | [**Download for Windows**](https://github.com/delirious6423/dig-the-f-up/releases/latest/download/Dig-the-f-up-windows-x64-setup.exe) |
| **Linux** (x86_64) | [**AppImage**](https://github.com/delirious6423/dig-the-f-up/releases/latest/download/Dig-the-f-up-linux-x86_64.AppImage) or [**.deb**](https://github.com/delirious6423/dig-the-f-up/releases/latest/download/Dig-the-f-up-linux-amd64.deb) |

All versions and checksums: [Releases](https://github.com/delirious6423/dig-the-f-up/releases).

The apps are free and not signed with a paid certificate, so your computer asks you to confirm the first start.

**Mac**

1. Open the `.dmg` and drag **Dig-the-f-up** into **Applications**.
2. Open it from Applications. macOS says it cannot verify the app.
3. Open **System Settings > Privacy & Security**, scroll down, click **Open Anyway**, and confirm. You only do this once.

**Windows**

1. Run the Setup file. If **Windows protected your PC** appears, click **More info**, then **Run anyway**.
2. It installs for your account only; no administrator rights needed.

**Linux**

- AppImage: right-click the file, choose **Properties > Permissions**, tick **Allow executing as program** (or run
  `chmod +x` on it), then double-click it.
- `.deb`: `sudo apt install ./Dig-the-f-up-linux-amd64.deb`.

Windows and Linux builds pass the tests but have not been tried on real machines yet
([#1](https://github.com/delirious6423/dig-the-f-up/issues/1), [#2](https://github.com/delirious6423/dig-the-f-up/issues/2)).
Reports are welcome.

## Getting started

1. Start the app and click **Download model** (865 MB, once).
2. Add the folders you want to search, such as Documents, Desktop and Pictures. Indexing a whole drive works, but takes
   hours, so the app asks first.
3. Start typing. Results appear while indexing continues; the bar at the bottom shows how far it is and the time left.
   **Pause** stops it at once and **Resume** carries on where it left off.

System folders, app data, caches, backups (Time Machine, File History), the Trash and files that are only in the cloud
are skipped.

## Using the app

- Type to search. Up/Down selects, Enter opens, Cmd+Enter (Ctrl+Enter) or Cmd+click shows the file in its folder.
- Filter by type with the chips: screenshots, images, PDFs, docs, audio, video.
- A PDF hit opens at its page. Audio and video hits name the moment they match.
- New and changed files are picked up while the app runs.
- Esc clears the search. On a Mac, Esc on an empty search or closing the window hides the app;
  Cmd+Shift+Space brings it back. On Windows and Linux, closing the window quits.
- Text in screenshots is searchable. On a Mac, Apple's built-in text recognition reads it. On Windows and Linux the app
  downloads a text reader (LightOnOCR-3, about 1 GB) in the background after the search model; until it is there, a
  small reader for Latin script fills in. On a Mac you can add LightOnOCR for scripts Apple's reader misses (Hebrew,
  Greek, Hindi and others) with the command line: `dtfu download`.

## Your data

Everything lives in one folder: the index, your folder list, the models and a small log (**Help > Show Logs**).

| | |
|---|---|
| Mac | `~/Library/Application Support/Dig-the-f-up/` |
| Windows | `%APPDATA%\Dig-the-f-up\` |
| Linux | `~/.local/share/Dig-the-f-up/` |

The command line keeps its index in `~/.dtfu`. If the original DigUp app is installed, its model is reused.

To uninstall, quit the app, delete it, and delete that folder (and `~/.dtfu` if you used the command line).

## About

Dig-the-f-up is a fast Rust port of [DigUp](https://github.com/ARahim3/DigUp) by Abdur Rahim (Swift, macOS only), built
to run on macOS, Windows and Linux. Search quality matches the original: on DigUp's own eval sets the scores are equal,
and all 126 eval queries return the same top 10 on the same index. Speed numbers are in
[`docs/BENCHMARKS.md`](docs/BENCHMARKS.md). The name nods to the f-tools that inspired the speed work (fff, fsearch
and friends).

## Build from source

For developers, on an Apple Silicon Mac. You need Rust and CMake (`brew install cmake`).

```bash
git clone https://github.com/delirious6423/dig-the-f-up && cd dig-the-f-up
scripts/build-llama.sh                  # once, 5-15 min: llama.cpp with Metal
cd app && cargo run --release           # the desktop app
```

Command line:

```bash
cargo build --release -p dtfu-cli
target/release/dtfu download            # the models, once
target/release/dtfu index ~/Documents
target/release/dtfu search "a penguin on the ice"
```

Video in a source build uses ffmpeg from your system (`brew install ffmpeg`), or build the bundled one with
`scripts/build-ffmpeg.sh`. More in [`CONTRIBUTING.md`](CONTRIBUTING.md); open work is in
[issues](https://github.com/delirious6423/dig-the-f-up/issues).

## Licence

Apache License 2.0, see [`LICENSE`](LICENSE). Based on DigUp (MIT) and llama.cpp (MIT); EmbeddingGemma 2 is under
Apache 2.0. See [`NOTICE`](NOTICE) and [`docs/THIRD-PARTY.md`](docs/THIRD-PARTY.md).
