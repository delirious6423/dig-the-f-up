# Dig-the-f-up desktop app (Tauri 2)

A minimal desktop shell over `dtfu-engine`. It is a separate cargo project (own `[workspace]`), so
`cargo test --workspace` at the repo root does not build it.

Node, npm and `tauri-cli` are **not** required: the front end is plain static files in `dist/`
(`withGlobalTauri` is on).

## Run

    cd app
    cargo run --release

The first build takes several minutes. For a `.app` bundle you would need `cargo install tauri-cli`; this is optional.

## Model

Without the embedding model the status bar shows "No model" and a "Download model (865 MB)" button. The size comes
from the pinned manifest in `dtfu-models` (two files: 309,855,456 + 554,821,024 bytes).

The files go to `dtfu_models::models_dir()` (override with `DTFU_MODELS`). Downloads resume and are checksummed.
Before a download starts, the app checks that the volume has room for the bytes still missing plus a 1 GiB margin,
and stops with a message if not. Cancel stops after the current chunk; partial files are kept and the next
download resumes from them. Errors appear in a dismissible banner above the results.

## Data

One folder, `~/Library/Application Support/Dig-the-f-up/` (`dirs::data_dir()/Dig-the-f-up`):

| Path | What it is |
| --- | --- |
| `index/` | the search index (SQLite) |
| `settings.json` | your indexed-folder list and options |
| `Models/` | the two model files (`embeddinggemma-2-Q8_0.gguf`, `mmproj-embeddinggemma-2-Q8_0.gguf`) |
| `logs/dtfu.log` | the local log (see Logs); `dtfu.log.1` holds the previous 1 MiB. Lives in whichever of `Dig-the-f-up/`, `DigUp/` or `DigUp-rs/` is in use (see below) |

Earlier builds used `~/Library/Application Support/DigUp/` or `DigUp-rs/` for the index and settings. Nothing is
moved: if `Dig-the-f-up/` has no index or settings yet and `DigUp/` (then `DigUp-rs/`) has them, the app keeps using
that folder in place, read-only fallback for the lookup. Models in the original Swift app's
`~/Library/Application Support/DigUp/Models` are shared and found if present. The Hugging Face cache is only read
(when the model is found there), never written or removed by Dig-the-f-up.

## Logs

The app writes `logs/dtfu.log` (UTC timestamps, one line per event: startup, indexing errors, download failures and
cancels). It rotates to `dtfu.log.1` at 1 MiB, so at most about 2 MiB is kept. Help > Show Logs reveals the file in
Finder. The log never leaves your Mac. It can contain file paths of the folders you index. Attach it to a bug report
only if you choose to.

## Uninstall

1. Quit Dig-the-f-up (Cmd+Q) and delete `Dig-the-f-up.app` from Applications.
2. To remove your index, settings, logs and models, delete `~/Library/Application Support/Dig-the-f-up/`.
3. For installs from earlier builds, also delete `~/Library/Application Support/DigUp-rs/` if it exists. Do not
   delete `~/Library/Application Support/DigUp/` if you also use the original Swift DigUp app (it may hold the shared
   models); otherwise it can go too.
4. If you set `DTFU_MODELS`, the models live there instead; delete that folder too. The `dtfu` CLI (default index `~/.dtfu`, falling back to `~/.digup`) uses the same
   `Models/` rule as the app. Nothing else is written outside these folders.

## Use

- Search is debounced 200 ms; Enter searches now. Up/Down select, Enter opens, Cmd/Ctrl+Enter or Cmd/Ctrl+click reveals.
- Gear button: add or remove indexed folders.
- Esc or closing the window hides it; Cmd+Shift+Space (Ctrl+Shift+Space on Linux/Windows) toggles it. Quit with Cmd+Q.

## Bundle (macOS, Apple Silicon)

    cargo install tauri-cli --version '^2' --locked
    cd app && cargo tauri build --bundles app

Output: `app/target/release/bundle/macos/Dig-the-f-up.app` (36 MiB, arm64, links only system libraries; llama.cpp and Metal
are static). Measured 2026-10-12: it is ad-hoc signed only. Developer ID signing and notarization need the owner's Apple
Developer account and are not done.

Opening a result: a PDF opens in its default app with search words from the hit's page (Apple Event `stxt`, as the Swift
app does), audio/video jump to the moment in QuickTime Player (osascript) or IINA (`iina://` URL); everything else opens
plainly. These paths are compile- and unit-tested only; they have not been exercised against the real apps.
