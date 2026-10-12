//! Minimal Tauri 2 desktop shell over `dtfu-engine`.
#![cfg_attr(all(not(debug_assertions), target_os = "windows"), windows_subsystem = "windows")]

use std::path::PathBuf;
use std::sync::Arc;

use dtfu_engine::{Engine, EngineConfig, EngineEvent, Hit, IndexCounts, Status, WatchPolicy};
use serde::Serialize;
use tauri::menu::{Menu, MenuItem, PredefinedMenuItem, Submenu};
use tauri::{AppHandle, Emitter, Manager, State, WindowEvent};
use tauri_plugin_dialog::DialogExt;
use tauri_plugin_global_shortcut::{Code, Modifiers, Shortcut, ShortcutState};
use tauri_plugin_opener::OpenerExt;

mod logging;
mod opener;

type Eng<'a> = State<'a, Arc<Engine>>;
type Res<T> = Result<T, String>;

fn err<E: std::fmt::Display>(e: E) -> String {
    format!("{e:#}")
}

/// How long a filesystem call may take before we give up (SMB shares and dead drives can block for minutes).
const FS_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(3);

/// Runs blocking filesystem work off the async workers with a deadline of `FS_TIMEOUT`.
/// On timeout the blocking thread is abandoned: a hung syscall cannot be cancelled, so it keeps running until the OS
/// returns and its result is dropped. Pass only owned data into `f`.
async fn fs_call<T: Send + 'static>(what: &str, f: impl FnOnce() -> Result<T, String> + Send + 'static) -> Result<T, String> {
    fs_call_within(FS_TIMEOUT, what, f).await
}

/// Handing a file to another app (open, reveal) can take seconds while that app starts; the path itself was already
/// checked within `FS_TIMEOUT`, so the handoff gets this longer deadline.
const HANDOFF_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(20);

async fn fs_call_within<T: Send + 'static>(
    limit: std::time::Duration,
    what: &str,
    f: impl FnOnce() -> Result<T, String> + Send + 'static,
) -> Result<T, String> {
    match tokio::time::timeout(limit, tokio::task::spawn_blocking(f)).await {
        Ok(joined) => joined.map_err(err)?,
        Err(_) => Err(format!("{what}: the drive or network share is not responding")),
    }
}

#[derive(Serialize, Debug, PartialEq)]
#[serde(rename_all = "camelCase")]
struct HitDto {
    path: String,
    kind: String,
    segment: String,
    score: f64,
    place: String,
    excerpt: Option<String>,
    loc: Option<f64>,
    focus: Option<f64>,
    file_name: String,
    folder: String,
}

fn hit_dto(h: Hit) -> HitDto {
    let p = std::path::Path::new(&h.path);
    let file_name = p.file_name().map(|s| s.to_string_lossy().into_owned()).unwrap_or_else(|| h.path.clone());
    let folder = p.parent().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default();
    HitDto {
        path: h.path,
        kind: h.kind,
        segment: h.segment,
        score: h.score,
        place: h.place,
        excerpt: h.excerpt,
        loc: h.loc,
        focus: h.focus,
        file_name,
        folder,
    }
}

#[derive(Serialize, Debug, PartialEq)]
#[serde(rename_all = "camelCase")]
struct StatusDto {
    state: &'static str,
    done: usize,
    total: usize,
    current: String,
}

fn status_dto(s: Status) -> StatusDto {
    let (state, done, total, current) = match s {
        Status::NoModel => ("noModel", 0, 0, String::new()),
        Status::Ready => ("ready", 0, 0, String::new()),
        Status::Indexing { done, total, current } => ("indexing", done, total, current),
        Status::Idle => ("idle", 0, 0, String::new()),
        Status::Paused => ("paused", 0, 0, String::new()),
    };
    StatusDto { state, done, total, current }
}

#[derive(Serialize, Debug, PartialEq)]
#[serde(rename_all = "camelCase")]
struct CountsDto {
    files_by_kind_state: std::collections::BTreeMap<String, std::collections::BTreeMap<String, usize>>,
    vectors: usize,
    pending: usize,
    unfinished: usize,
}

fn counts_dto(c: IndexCounts) -> CountsDto {
    CountsDto { files_by_kind_state: c.files_by_kind_state, vectors: c.vectors, pending: c.pending, unfinished: c.unfinished }
}

/// Live-update state for the status pill and the settings panel.
#[derive(Serialize, Debug, PartialEq)]
#[serde(rename_all = "camelCase")]
struct LiveDto {
    watch_enabled: bool,
    /// `live`, `plugged_in` or `manual`.
    policy: &'static str,
    running: bool,
    mode: String,
    /// Changes recorded and waiting for conditions to allow a sync.
    pending: usize,
    /// Why they are waiting (on battery, manual policy, ...).
    deferred: Option<String>,
    usage_tracking: bool,
}

fn live_dto(engine: &Engine) -> LiveDto {
    let (watch_enabled, policy) = engine.watch_settings();
    let info = engine.watch_info();
    LiveDto {
        watch_enabled,
        policy: policy.as_str(),
        running: info.running,
        mode: info.mode,
        pending: info.pending,
        deferred: info.deferred,
        usage_tracking: engine.usage_tracking(),
    }
}

fn event_json(ev: EngineEvent) -> serde_json::Value {
    use serde_json::json;
    match ev {
        EngineEvent::Pruned { removed } => json!({"type":"pruned","removed":removed}),
        EngineEvent::Synced { added, changed, moved, removed, unchanged, offline, different_drive } => {
            json!({"type":"synced","added":added,"changed":changed,"moved":moved,"removed":removed,"unchanged":unchanged,
                   "offline":offline,"different_drive":different_drive})
        }
        EngineEvent::Progress { done, total, path, kind, state, note } => {
            json!({"type":"progress","done":done,"total":total,"path":path,"kind":kind,"state":state,"note":note})
        }
        EngineEvent::Rest { done, total, path, kind, state, note } => {
            json!({"type":"rest","done":done,"total":total,"path":path,"kind":kind,"state":state,"note":note})
        }
        EngineEvent::Waiting => json!({"type":"waiting"}),
        EngineEvent::Error(m) => json!({"type":"error","message":m}),
        EngineEvent::Idle => json!({"type":"idle"}),
        EngineEvent::Stopped => json!({"type":"stopped"}),
    }
}

fn model_ok() -> bool {
    dtfu_models::locate(&dtfu_models::TEXT).is_some() && dtfu_models::locate(&dtfu_models::MMPROJ).is_some()
}

#[tauri::command]
async fn search(engine: Eng<'_>, query: String, limit: Option<usize>, kinds: Option<Vec<String>>) -> Res<Vec<HitDto>> {
    // Model load and inference take hundreds of ms: keep them off the main thread.
    let engine = Arc::clone(engine.inner());
    let kinds = kinds.filter(|k| !k.is_empty());
    let hits = tauri::async_runtime::spawn_blocking(move || engine.search(&query, limit.unwrap_or(30), kinds))
        .await
        .map_err(err)?
        .map_err(err)?;
    Ok(hits.into_iter().map(hit_dto).collect())
}

#[tauri::command]
async fn status(engine: Eng<'_>) -> Res<StatusDto> {
    Ok(status_dto(engine.status()))
}

#[tauri::command]
async fn live(engine: Eng<'_>) -> Res<LiveDto> {
    Ok(live_dto(&engine))
}

#[tauri::command]
async fn set_watch(engine: Eng<'_>, enabled: bool, policy: String) -> Res<LiveDto> {
    let policy = WatchPolicy::parse(&policy).ok_or_else(|| format!("unknown policy {policy}"))?;
    let engine = Arc::clone(engine.inner());
    // Restarting the watcher joins its thread: keep it off the main thread.
    tauri::async_runtime::spawn_blocking(move || engine.set_watch(enabled, policy).map(|()| live_dto(&engine)))
        .await
        .map_err(err)?
        .map_err(err)
}

#[tauri::command]
async fn set_usage_tracking(engine: Eng<'_>, on: bool) -> Res<LiveDto> {
    engine.set_usage_tracking(on).map_err(err)?;
    Ok(live_dto(&engine))
}

#[tauri::command]
async fn clear_usage(engine: Eng<'_>) -> Res<()> {
    engine.clear_usage().map_err(err)
}

#[tauri::command]
async fn counts(engine: Eng<'_>) -> Res<CountsDto> {
    engine.counts().map(counts_dto).map_err(err)
}

#[tauri::command]
async fn folders(engine: Eng<'_>) -> Res<Vec<String>> {
    Ok(engine.folders().iter().map(|p| p.to_string_lossy().into_owned()).collect())
}

/// Each folder with its drive state: "online", "offline", "different_drive" or "unknown" (see `Engine::folder_states`).
#[tauri::command]
async fn folder_states(engine: Eng<'_>) -> Res<Vec<serde_json::Value>> {
    Ok(engine
        .folder_states()
        .into_iter()
        .map(|(p, state)| serde_json::json!({"path": p.to_string_lossy(), "state": state}))
        .collect())
}

/// Index the drive now mounted at `path` in place of the one the folder was indexed on.
#[tauri::command]
async fn use_drive_now(engine: Eng<'_>, path: String) -> Res<()> {
    let p = std::path::PathBuf::from(&path);
    if !engine.folders().contains(&p) {
        return Err("not one of your folders".into());
    }
    let e = engine.inner().clone();
    tokio::task::spawn_blocking(move || e.use_drive_now(&p)).await.map_err(err)?.map_err(err)
}

#[tauri::command]
async fn add_folder(engine: Eng<'_>, path: String) -> Res<()> {
    let probe = path.clone();
    if !fs_call("add folder", move || Ok(std::path::Path::new(&probe).is_dir())).await? {
        return Err("not a directory".into());
    }
    let engine = Arc::clone(engine.inner());
    tauri::async_runtime::spawn_blocking(move || engine.add_folder(PathBuf::from(path))).await.map_err(err)?.map_err(err)
}

#[tauri::command]
async fn remove_folder(engine: Eng<'_>, path: String) -> Res<()> {
    let engine = Arc::clone(engine.inner());
    tauri::async_runtime::spawn_blocking(move || engine.remove_folder(std::path::Path::new(&path))).await.map_err(err)?.map_err(err)
}

/// Pure core: the candidates that are existing directories, in their given order, as strings.
fn existing_dirs(candidates: &[PathBuf]) -> Vec<String> {
    candidates.iter().filter(|p| p.is_dir()).map(|p| p.to_string_lossy().into_owned()).collect()
}

/// First-run suggestions: the common user folders under home that exist.
#[tauri::command]
async fn suggested_folders() -> Vec<String> {
    let Some(home) = dirs::home_dir() else { return Vec::new() };
    let mut out = Vec::new();
    for d in ["Documents", "Desktop", "Pictures", "Downloads"] {
        let p = home.join(d);
        // On timeout (or error) the folder is skipped.
        if let Ok(v) = fs_call("suggested folder", move || Ok(existing_dirs(&[p]))).await {
            out.extend(v);
        }
    }
    out
}

#[tauri::command]
async fn pick_folder(app: AppHandle) -> Res<Option<String>> {
    let picked = app.dialog().file().blocking_pick_folder();
    Ok(picked.and_then(|p| p.into_path().ok()).map(|p| p.to_string_lossy().into_owned()))
}

#[tauri::command]
async fn start_indexing(engine: Eng<'_>) -> Res<()> {
    let engine = Arc::clone(engine.inner());
    tauri::async_runtime::spawn_blocking(move || engine.start_indexing()).await.map_err(err)?.map_err(err)
}

#[tauri::command]
async fn pause(engine: Eng<'_>) -> Res<()> {
    // `Engine::pause` joins the indexing thread, which can take seconds: keep it off the main thread.
    let engine = Arc::clone(engine.inner());
    tauri::async_runtime::spawn_blocking(move || engine.pause()).await.map_err(err)
}

#[tauri::command]
fn resume(engine: Eng<'_>) -> Res<()> {
    engine.resume();
    Ok(())
}

#[tauri::command]
async fn model_present() -> bool {
    model_ok()
}

/// Set by `cancel_download`; checked between download chunks.
static CANCEL: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// Total size of the two model files, from the pinned manifest (not from disk).
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct ModelInfo {
    total_bytes: u64,
    text_bytes: u64,
    mmproj_bytes: u64,
}

#[tauri::command]
fn model_info() -> ModelInfo {
    let (text, mmproj) = (dtfu_models::TEXT.bytes, dtfu_models::MMPROJ.bytes);
    ModelInfo { total_bytes: text + mmproj, text_bytes: text, mmproj_bytes: mmproj }
}

#[tauri::command]
fn download_models(app: AppHandle) -> Res<()> {
    static BUSY: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
    if BUSY.swap(true, std::sync::atomic::Ordering::SeqCst) {
        return Err("download already running".into());
    }
    CANCEL.store(false, std::sync::atomic::Ordering::SeqCst);
    std::thread::spawn(move || {
        struct Done;
        impl Drop for Done {
            fn drop(&mut self) {
                BUSY.store(false, std::sync::atomic::Ordering::SeqCst);
            }
        }
        let _done = Done;
        let dir = dtfu_models::models_dir();
        let files = [dtfu_models::TEXT, dtfu_models::MMPROJ];
        let mut last: Option<(String, u64)> = None;
        let mut progress = |name: &str, done: u64, total: u64| {
            // Throttle to roughly every 256 KiB per file, always pass the final tick.
            let stepped = match &last {
                Some((n, l)) if n == name => done.saturating_sub(*l) >= 256 * 1024,
                _ => true,
            };
            if done == total || stepped {
                last = Some((name.to_string(), done));
                let _ = app.emit("model-progress", serde_json::json!({"name": name, "done": done, "total": total}));
            }
        };
        match dtfu_models::download_all(&files, &dir, None, &CANCEL, &mut progress) {
            Ok(_) => {
                let _ = app.emit("model-ready", ());
                if let Some(engine) = app.try_state::<Arc<Engine>>() {
                    let _ = engine.start_indexing();
                }
            }
            Err(e) if dtfu_models::is_cancelled(&e) => {
                logging::info("model download cancelled; partial files kept");
                let _ = app.emit("model-cancelled", "Download cancelled. Partial files are kept and will resume.");
            }
            Err(e) => {
                logging::error(&format!("model download failed: {e:#}"));
                let _ = app.emit("model-error", format!("{e:#}"));
            }
        }
    });
    Ok(())
}

/// Asks a running download to stop after the current chunk. Partial files are kept.
#[tauri::command]
fn cancel_download() {
    CANCEL.store(true, std::sync::atomic::Ordering::SeqCst);
}

/// Only existing files under an indexed folder may be opened or revealed.
async fn checked_path(engine: &Engine, path: &str) -> Res<PathBuf> {
    let folders = engine.folders();
    let path = path.to_string();
    fs_call("check path", move || {
        let p = std::fs::canonicalize(&path).map_err(err)?;
        if !p.is_file() {
            return Err("not a file".into());
        }
        let under = folders.iter().any(|f| std::fs::canonicalize(f).map(|f| p.starts_with(f)).unwrap_or(false));
        if under { Ok(p) } else { Err("path is outside the indexed folders".into()) }
    })
    .await
}

/// Only files the index contains may be opened or revealed: folder check first, then an exact path lookup.
/// The stored path is what the UI passes; the canonical form is tried too (symlinked folders).
async fn checked_indexed(engine: &Eng<'_>, path: &str) -> Res<PathBuf> {
    let p = checked_path(engine, path).await?;
    let candidates = indexed_candidates(path, &p);
    // What gets opened is the canonical `p`. If the file itself is a symlink (say an indexed entry later replaced by
    // a link), its target must be indexed too; symlinked parent folders stay allowed.
    let owned = path.to_string();
    let file_is_link =
        fs_call("check path", move || Ok(std::fs::symlink_metadata(&owned).map(|m| m.file_type().is_symlink()).unwrap_or(false))).await?;
    let eng = engine.inner().clone();
    let found = tauri::async_runtime::spawn_blocking(move || -> Res<bool> {
        if file_is_link {
            return match candidates.get(1) {
                Some(c) => eng.is_indexed_file(c).map_err(err),
                None => Ok(false),
            };
        }
        for c in &candidates {
            if eng.is_indexed_file(c).map_err(err)? {
                return Ok(true);
            }
        }
        Ok(false)
    })
    .await
    .map_err(err)??;
    if found { Ok(p) } else { Err("file is not in the index".into()) }
}

/// Path strings to look up in the index: as given, then canonical (when different and valid UTF-8).
fn indexed_candidates(given: &str, canonical: &std::path::Path) -> Vec<String> {
    let mut v = vec![given.to_string()];
    if let Some(c) = canonical.to_str() {
        if c != given {
            v.push(c.to_string());
        }
    }
    v
}

/// Opens a search hit at its place (`opener::open_at`). `loc`: a PDF's page or a recording's seconds; `query`: the search.
#[tauri::command]
async fn open_hit(app: AppHandle, engine: Eng<'_>, path: String, kind: String, loc: Option<f64>, query: String) -> Res<()> {
    let p = checked_indexed(&engine, &path).await?;
    // Feeds ranking and indexing order when the user turned open-history on; a no-op otherwise. Never blocks the open.
    let _ = engine.record_open(&path);
    let handle = app.clone();
    fs_call_within(HANDOFF_TIMEOUT, "open file", move || opener::open_at(&handle, &p, &kind, loc, &query)).await
}

#[tauri::command]
async fn reveal_path(app: AppHandle, engine: Eng<'_>, path: String) -> Res<()> {
    let p = checked_indexed(&engine, &path).await?;
    let handle = app.clone();
    fs_call_within(HANDOFF_TIMEOUT, "reveal file", move || handle.opener().reveal_item_in_dir(p).map_err(err)).await
}

const THUMB_SIDE: u32 = 96;
const THUMB_MAX_BYTES: u64 = 64 * 1024 * 1024;

/// Extensions the `image` crate decodes in this build (HEIC is not supported, so it gets no thumbnail).
fn thumb_ext_supported(path: &std::path::Path) -> bool {
    let ext = path.extension().map(|e| e.to_string_lossy().to_ascii_lowercase()).unwrap_or_default();
    matches!(ext.as_str(), "jpg" | "jpeg" | "png" | "gif" | "webp" | "bmp" | "tiff" | "tif")
}

/// Composite onto white so transparent pixels do not turn black in JPEG.
fn flatten_on_white(img: &image::DynamicImage) -> image::RgbImage {
    let rgba = img.to_rgba8();
    let (w, h) = rgba.dimensions();
    let mut out = image::RgbImage::new(w, h);
    for (x, y, p) in rgba.enumerate_pixels() {
        let a = p[3] as u32;
        let c = |v: u8| ((v as u32 * a + 255 * (255 - a)) / 255) as u8;
        out.put_pixel(x, y, image::Rgb([c(p[0]), c(p[1]), c(p[2])]));
    }
    out
}

/// Pure core: decode, shrink to fit `THUMB_SIDE` (aspect kept), encode as JPEG, return a data URL.
/// Returns None when the bytes are not a decodable image.
fn thumb_data_url(bytes: &[u8]) -> Option<String> {
    use base64::Engine as _;
    // Bound dimensions and allocation so a small file that claims a huge image cannot exhaust memory.
    let mut reader = image::ImageReader::new(std::io::Cursor::new(bytes)).with_guessed_format().ok()?;
    let mut limits = image::Limits::default();
    limits.max_image_width = Some(16384);
    limits.max_image_height = Some(16384);
    limits.max_alloc = Some(256 * 1024 * 1024);
    reader.limits(limits);
    let mut decoder = reader.into_decoder().ok()?;
    let orientation = image::ImageDecoder::orientation(&mut decoder).unwrap_or(image::metadata::Orientation::NoTransforms);
    let mut img = image::DynamicImage::from_decoder(decoder).ok()?;
    img.apply_orientation(orientation);
    let small = flatten_on_white(&img.thumbnail(THUMB_SIDE, THUMB_SIDE));
    let mut out = std::io::Cursor::new(Vec::new());
    image::DynamicImage::ImageRgb8(small).write_to(&mut out, image::ImageFormat::Jpeg).ok()?;
    Some(format!("data:image/jpeg;base64,{}", base64::engine::general_purpose::STANDARD.encode(out.into_inner())))
}

/// Blocking file side: size cap, read, then the pure core. Runs off the main thread.
fn thumb_file(p: &std::path::Path) -> Res<Option<String>> {
    if std::fs::metadata(p).map_err(err)?.len() > THUMB_MAX_BYTES {
        return Ok(None);
    }
    let bytes = std::fs::read(p).map_err(err)?;
    Ok(thumb_data_url(&bytes))
}

#[tauri::command]
async fn thumbnail(engine: Eng<'_>, path: String) -> Res<Option<String>> {
    if !thumb_ext_supported(std::path::Path::new(&path)) {
        return Ok(None);
    }
    let p = checked_path(&engine, &path).await?;
    // At most THUMB_CONCURRENCY decodes at once; a scrolling result list must not spawn dozens of full decodes.
    // The permit stays with this future; the blocking closure only owns data, so a hung read cannot leak it.
    let _permit = thumb_semaphore().acquire().await.map_err(err)?;
    match tokio::time::timeout(THUMB_TIMEOUT, tokio::task::spawn_blocking(move || thumb_file(&p))).await {
        Ok(joined) => joined.map_err(err)?,
        // The abandoned blocking thread finishes on its own when the read returns.
        Err(_) => Err("thumbnail: the drive or network share is not responding".into()),
    }
}

const THUMB_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);

const THUMB_CONCURRENCY: usize = 3;

fn thumb_semaphore() -> &'static tokio::sync::Semaphore {
    static S: std::sync::OnceLock<tokio::sync::Semaphore> = std::sync::OnceLock::new();
    S.get_or_init(|| tokio::sync::Semaphore::new(THUMB_CONCURRENCY))
}

#[tauri::command]
fn quit(app: AppHandle) {
    app.exit(0);
}

fn toggle_window(app: &AppHandle) {
    if let Some(w) = app.get_webview_window("main") {
        if w.is_visible().unwrap_or(false) && w.is_focused().unwrap_or(false) {
            let _ = w.hide();
        } else {
            let _ = w.show();
            let _ = w.unminimize();
            let _ = w.set_focus();
        }
    }
}

/// Menu id of "Show Logs".
const SHOW_LOGS: &str = "show_logs";

/// Reveals the local log file in Finder (or the file manager). Logs are only ever shown, never uploaded.
fn show_logs(app: &AppHandle) {
    let Some(path) = logging::log_path() else { return };
    if let Err(e) = app.opener().reveal_item_in_dir(path) {
        logging::error(&format!("show logs: {e}"));
    }
}

fn build_menu(app: &AppHandle) -> tauri::Result<Menu<tauri::Wry>> {
    let app_menu = Submenu::with_items(app, "Dig-the-f-up", true, &[&PredefinedMenuItem::hide(app, None)?, &PredefinedMenuItem::quit(app, None)?])?;
    let show_logs = MenuItem::with_id(app, SHOW_LOGS, "Show Logs", true, None::<&str>)?;
    let help = Submenu::with_items(app, "Help", true, &[&show_logs])?;
    let edit = Submenu::with_items(
        app,
        "Edit",
        true,
        &[
            &PredefinedMenuItem::undo(app, None)?,
            &PredefinedMenuItem::redo(app, None)?,
            &PredefinedMenuItem::separator(app)?,
            &PredefinedMenuItem::cut(app, None)?,
            &PredefinedMenuItem::copy(app, None)?,
            &PredefinedMenuItem::paste(app, None)?,
            &PredefinedMenuItem::select_all(app, None)?,
        ],
    )?;
    Menu::with_items(app, &[&app_menu, &edit, &help])
}

fn main() {
    let shortcut = Shortcut::new(Some(Modifiers::SUPER | Modifiers::SHIFT), Code::Space);
    let shortcut_ctrl = Shortcut::new(Some(Modifiers::CONTROL | Modifiers::SHIFT), Code::Space);
    let gs = tauri_plugin_global_shortcut::Builder::new()
        .with_handler(|app, _sc, ev| {
            if ev.state == ShortcutState::Pressed {
                toggle_window(app);
            }
        })
        .build();

    tauri::Builder::default()
        .plugin(tauri_plugin_dialog::init())
        .plugin(tauri_plugin_opener::init())
        .plugin(gs)
        .on_menu_event(|app, event| {
            if event.id() == SHOW_LOGS {
                show_logs(app);
            }
        })
        .setup(move |app| {
            use tauri_plugin_global_shortcut::GlobalShortcutExt;
            let base = dtfu_models::app_data_dir();
            std::fs::create_dir_all(&base)?;
            // A log file that cannot be created must not stop the app; logging is then a no-op.
            if let Err(e) = logging::init(&dtfu_models::logs_dir()) {
                eprintln!("log file: {e}");
            }
            logging::info(&format!("Dig-the-f-up {} starting; data folder {}", env!("CARGO_PKG_VERSION"), base.display()));
            let engine = Arc::new(Engine::open(EngineConfig { index_dir: base.join("index"), settings_file: base.join("settings.json") })?);

            // Forward engine events to the front end.
            let rx = engine.subscribe();
            let handle = app.handle().clone();
            std::thread::spawn(move || {
                while let Ok(ev) = rx.recv() {
                    if let EngineEvent::Error(m) = &ev {
                        logging::error(&format!("indexing: {m}"));
                    }
                    let _ = handle.emit("engine-event", event_json(ev));
                }
            });

            if !engine.folders().is_empty() && model_ok() {
                if let Err(e) = engine.start_indexing() {
                    logging::error(&format!("start_indexing: {e:#}"));
                }
            }
            app.manage(engine);

            app.set_menu(build_menu(app.handle())?)?;
            // Cmd+Shift+Space on macOS, Ctrl+Shift+Space elsewhere.
            let sc = if cfg!(target_os = "macos") { shortcut } else { shortcut_ctrl };
            if let Err(e) = app.global_shortcut().register(sc) {
                logging::error(&format!("global shortcut: {e}"));
            }
            Ok(())
        })
        .on_window_event(|window, event| {
            if let WindowEvent::CloseRequested { api, .. } = event {
                api.prevent_close();
                let _ = window.hide();
            }
        })
        .invoke_handler(tauri::generate_handler![
            search, status, counts, live, set_watch, set_usage_tracking, clear_usage, folders, folder_states, use_drive_now, add_folder, remove_folder, suggested_folders, pick_folder, start_indexing,
            pause, resume, model_present, model_info, download_models, cancel_download, open_hit, reveal_path, quit, thumbnail
        ])
        .build(tauri::generate_context!())
        .expect("error while building Dig-the-f-up")
        .run(|app, event| {
            // Dock click while the window is hidden brings it back (macOS).
            #[cfg(target_os = "macos")]
            if let tauri::RunEvent::Reopen { has_visible_windows: false, .. } = event {
                if let Some(w) = app.get_webview_window("main") {
                    let _ = w.show();
                    let _ = w.set_focus();
                }
            }
            #[cfg(not(target_os = "macos"))]
            let _ = (app, event);
        });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fs_call_times_out_on_a_hung_closure() {
        let rt = tokio::runtime::Builder::new_multi_thread().enable_time().build().unwrap();
        let t = std::time::Instant::now();
        let r: Res<()> = rt.block_on(fs_call("stat", || {
            std::thread::sleep(std::time::Duration::from_secs(5));
            Ok(())
        }));
        assert_eq!(r.unwrap_err(), "stat: the drive or network share is not responding");
        assert!(t.elapsed() < std::time::Duration::from_secs(4));
        let ok: Res<u8> = rt.block_on(fs_call("stat", || Ok(7)));
        assert_eq!(ok, Ok(7));
        rt.shutdown_background();
    }

    #[test]
    fn indexed_candidates_adds_canonical_only_when_different() {
        let c = indexed_candidates("/a/b", std::path::Path::new("/a/b"));
        assert_eq!(c, vec!["/a/b".to_string()]);
        let c = indexed_candidates("/l/b", std::path::Path::new("/a/b"));
        assert_eq!(c, vec!["/l/b".to_string(), "/a/b".to_string()]);
    }

    /// A 40x20 landscape JPEG carrying EXIF orientation 6 (rotate 90 CW).
    fn jpeg_with_orientation(o: u8) -> Vec<u8> {
        let img = image::DynamicImage::ImageRgb8(image::RgbImage::from_pixel(40, 20, image::Rgb([200, 30, 30])));
        let mut out = std::io::Cursor::new(Vec::new());
        img.write_to(&mut out, image::ImageFormat::Jpeg).unwrap();
        let jpeg = out.into_inner();
        let mut exif: Vec<u8> = b"Exif\0\0MM\0*\0\0\0\x08".to_vec();
        exif.extend_from_slice(&[0, 1, 0x01, 0x12, 0, 3, 0, 0, 0, 1, 0, o, 0, 0, 0, 0, 0, 0]);
        let mut seg = vec![0xFF, 0xE1];
        seg.extend_from_slice(&((exif.len() + 2) as u16).to_be_bytes());
        seg.extend_from_slice(&exif);
        let mut v = jpeg[..2].to_vec();
        v.extend(seg);
        v.extend_from_slice(&jpeg[2..]);
        v
    }

    fn thumb_dims(bytes: &[u8]) -> (u32, u32) {
        use base64::Engine as _;
        let url = thumb_data_url(bytes).unwrap();
        let b = base64::engine::general_purpose::STANDARD.decode(url.strip_prefix("data:image/jpeg;base64,").unwrap()).unwrap();
        let i = image::load_from_memory(&b).unwrap();
        (i.width(), i.height())
    }

    #[test]
    fn thumbnail_applies_exif_orientation() {
        let (w, h) = thumb_dims(&jpeg_with_orientation(6));
        assert!(h > w, "orientation 6 must give a portrait thumbnail, got {w}x{h}");
        let (w, h) = thumb_dims(&jpeg_with_orientation(1));
        assert!(w > h, "orientation 1 stays landscape, got {w}x{h}");
    }

    #[test]
    fn hit_maps_to_camel_case() {
        let h = Hit {
            path: "/a/b/doc.pdf".into(),
            kind: "pdf".into(),
            segment: "s".into(),
            score: 0.5,
            place: "p. 3".into(),
            excerpt: Some("x «y» z".into()),
            loc: None,
            focus: None,
        };
        let d = hit_dto(h);
        assert_eq!(d.file_name, "doc.pdf");
        assert_eq!(d.folder, "/a/b");
        let v = serde_json::to_value(&d).unwrap();
        assert_eq!(v["fileName"], "doc.pdf");
        assert!(v.get("file_name").is_none());
    }

    #[test]
    fn thumbnail_core_makes_96px_jpeg_data_url() {
        use base64::Engine as _;
        let src = image::RgbImage::from_pixel(200, 100, image::Rgb([200, 30, 30]));
        let mut png = std::io::Cursor::new(Vec::new());
        image::DynamicImage::ImageRgb8(src).write_to(&mut png, image::ImageFormat::Png).unwrap();
        let url = thumb_data_url(&png.into_inner()).expect("png decodes");
        let b64 = url.strip_prefix("data:image/jpeg;base64,").expect("jpeg data url prefix");
        let jpeg = base64::engine::general_purpose::STANDARD.decode(b64).unwrap();
        let back = image::load_from_memory_with_format(&jpeg, image::ImageFormat::Jpeg).unwrap();
        assert_eq!((back.width(), back.height()), (96, 48));
    }

    #[test]
    fn thumbnail_core_rejects_non_image() {
        assert!(thumb_data_url(b"plain text, not an image").is_none());
    }

    #[test]
    fn thumbnail_extensions() {
        use std::path::Path;
        assert!(thumb_ext_supported(Path::new("/a/B.JPG")));
        assert!(thumb_ext_supported(Path::new("/a/shot.png")));
        assert!(!thumb_ext_supported(Path::new("/a/photo.heic")));
        assert!(!thumb_ext_supported(Path::new("/a/doc.pdf")));
    }

    #[test]
    fn existing_dirs_keeps_only_directories_in_order() {
        let d = tempfile::tempdir().unwrap();
        let sub = d.path().join("sub");
        std::fs::create_dir(&sub).unwrap();
        let file = d.path().join("f.txt");
        std::fs::write(&file, b"x").unwrap();
        let missing = d.path().join("nope");
        let got = existing_dirs(&[missing, file, sub.clone()]);
        assert_eq!(got, vec![sub.to_string_lossy().into_owned()]);
        assert!(existing_dirs(&[]).is_empty());
    }

    #[test]
    fn status_maps() {
        let s = status_dto(Status::Indexing { done: 2, total: 5, current: "f".into() });
        assert_eq!((s.state, s.done, s.total), ("indexing", 2, 5));
        assert_eq!(status_dto(Status::NoModel).state, "noModel");
    }
}
