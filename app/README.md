# DigUp-rs desktop app (Tauri 2)

A minimal desktop shell over `digup-engine`. It is a separate cargo project (own `[workspace]`), so
`cargo test --workspace` at the repo root does not build it.

Node, npm and `tauri-cli` are **not** required: the front end is plain static files in `dist/`
(`withGlobalTauri` is on).

## Run

    cd app
    cargo run --release

The first build takes several minutes. For a `.app` bundle you would need `cargo install tauri-cli`; this is optional.

## Model

Without the embedding model the status bar shows "No model" and a "Download model (860 MB)" button.
The files go to `digup_models::models_dir()` (override with `DIGUP_MODELS`). Downloads resume and are checksummed.

## Data

`~/Library/Application Support/DigUp-rs/` (`dirs::data_dir()/DigUp-rs`): `index/` and `settings.json` (folder list).

## Use

- Search is debounced 200 ms; Enter searches now. Up/Down select, Enter opens, Cmd/Ctrl+Enter or Cmd/Ctrl+click reveals.
- Gear button: add or remove indexed folders.
- Esc or closing the window hides it; Cmd+Shift+Space (Ctrl+Shift+Space on Linux/Windows) toggles it. Quit with Cmd+Q.

## Bundle (macOS, Apple Silicon)

    cargo install tauri-cli --version '^2' --locked
    cd app && cargo tauri build --bundles app

Output: `app/target/release/bundle/macos/DigUp-rs.app` (36 MiB, arm64, links only system libraries; llama.cpp and Metal
are static). Measured 2026-10-12: it is ad-hoc signed only. Developer ID signing and notarization need the owner's Apple
Developer account and are not done.

Opening a result: a PDF opens in its default app with search words from the hit's page (Apple Event `stxt`, as the Swift
app does), audio/video jump to the moment in QuickTime Player (osascript) or IINA (`iina://` URL); everything else opens
plainly. These paths are compile- and unit-tested only; they have not been exercised against the real apps.
