//! Minimal Tauri 2 desktop shell over `digup-engine`.
#![cfg_attr(all(not(debug_assertions), target_os = "windows"), windows_subsystem = "windows")]

use std::path::PathBuf;
use std::sync::Arc;

use digup_engine::{Engine, EngineConfig, EngineEvent, Hit, IndexCounts, Status};
use serde::Serialize;
use tauri::menu::{Menu, PredefinedMenuItem, Submenu};
use tauri::{AppHandle, Emitter, Manager, State, WindowEvent};
use tauri_plugin_dialog::DialogExt;
use tauri_plugin_global_shortcut::{Code, Modifiers, Shortcut, ShortcutState};
use tauri_plugin_opener::OpenerExt;

mod opener;

type Eng<'a> = State<'a, Arc<Engine>>;
type Res<T> = Result<T, String>;

fn err<E: std::fmt::Display>(e: E) -> String {
    format!("{e:#}")
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

fn event_json(ev: EngineEvent) -> serde_json::Value {
    use serde_json::json;
    match ev {
        EngineEvent::Pruned { removed } => json!({"type":"pruned","removed":removed}),
        EngineEvent::Synced { added, changed, moved, removed, unchanged } => {
            json!({"type":"synced","added":added,"changed":changed,"moved":moved,"removed":removed,"unchanged":unchanged})
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
    digup_models::locate(&digup_models::TEXT).is_some() && digup_models::locate(&digup_models::MMPROJ).is_some()
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
async fn counts(engine: Eng<'_>) -> Res<CountsDto> {
    engine.counts().map(counts_dto).map_err(err)
}

#[tauri::command]
async fn folders(engine: Eng<'_>) -> Res<Vec<String>> {
    Ok(engine.folders().iter().map(|p| p.to_string_lossy().into_owned()).collect())
}

#[tauri::command]
async fn add_folder(engine: Eng<'_>, path: String) -> Res<()> {
    if !std::path::Path::new(&path).is_dir() {
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
fn suggested_folders() -> Vec<String> {
    let Some(home) = dirs::home_dir() else { return Vec::new() };
    existing_dirs(&["Documents", "Desktop", "Pictures", "Downloads"].map(|d| home.join(d)))
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

#[tauri::command]
fn download_models(app: AppHandle) -> Res<()> {
    static BUSY: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
    if BUSY.swap(true, std::sync::atomic::Ordering::SeqCst) {
        return Err("download already running".into());
    }
    std::thread::spawn(move || {
        struct Done;
        impl Drop for Done {
            fn drop(&mut self) {
                BUSY.store(false, std::sync::atomic::Ordering::SeqCst);
            }
        }
        let _done = Done;
        let dir = digup_models::models_dir();
        for file in [&digup_models::TEXT, &digup_models::MMPROJ] {
            let mut last = 0u64;
            let mut progress = |done: u64, total: u64| {
                // Throttle to roughly every 256 KiB, always pass the final tick.
                if done == total || done.saturating_sub(last) >= 256 * 1024 {
                    last = done;
                    let _ = app.emit("model-progress", serde_json::json!({"name": file.name, "done": done, "total": total}));
                }
            };
            if let Err(e) = digup_models::download(file, &dir, None, &mut progress) {
                let _ = app.emit("model-error", format!("{}: {e:#}", file.name));
                return;
            }
        }
        let _ = app.emit("model-ready", ());
        if let Some(engine) = app.try_state::<Arc<Engine>>() {
            let _ = engine.start_indexing();
        }
    });
    Ok(())
}

/// Only existing files under an indexed folder may be opened or revealed.
fn checked_path(engine: &Engine, path: &str) -> Res<PathBuf> {
    let p = std::fs::canonicalize(path).map_err(err)?;
    if !p.is_file() {
        return Err("not a file".into());
    }
    let under = engine.folders().iter().any(|f| std::fs::canonicalize(f).map(|f| p.starts_with(f)).unwrap_or(false));
    if under { Ok(p) } else { Err("path is outside the indexed folders".into()) }
}

/// Opens a search hit at its place (`opener::open_at`). `loc`: a PDF's page or a recording's seconds; `query`: the search.
#[tauri::command]
async fn open_hit(app: AppHandle, engine: Eng<'_>, path: String, kind: String, loc: Option<f64>, query: String) -> Res<()> {
    let p = checked_path(&engine, &path)?;
    // Feeds ranking and indexing order when the user turned open-history on; a no-op otherwise. Never blocks the open.
    let _ = engine.record_open(&path);
    let handle = app.clone();
    tauri::async_runtime::spawn_blocking(move || opener::open_at(&handle, &p, &kind, loc, &query)).await.map_err(err)?
}

#[tauri::command]
fn reveal_path(app: AppHandle, engine: Eng<'_>, path: String) -> Res<()> {
    let p = checked_path(&engine, &path)?;
    app.opener().reveal_item_in_dir(p).map_err(err)
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
    let img = reader.decode().ok()?;
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
    let p = checked_path(&engine, &path)?;
    tauri::async_runtime::spawn_blocking(move || thumb_file(&p)).await.map_err(err)?
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

fn build_menu(app: &AppHandle) -> tauri::Result<Menu<tauri::Wry>> {
    let app_menu = Submenu::with_items(app, "DigUp", true, &[&PredefinedMenuItem::hide(app, None)?, &PredefinedMenuItem::quit(app, None)?])?;
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
    Menu::with_items(app, &[&app_menu, &edit])
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
        .setup(move |app| {
            use tauri_plugin_global_shortcut::GlobalShortcutExt;
            let base = dirs::data_dir().unwrap_or_else(std::env::temp_dir).join("DigUp-rs");
            std::fs::create_dir_all(&base)?;
            let engine = Arc::new(Engine::open(EngineConfig { index_dir: base.join("index"), settings_file: base.join("settings.json") })?);

            // Forward engine events to the front end.
            let rx = engine.subscribe();
            let handle = app.handle().clone();
            std::thread::spawn(move || {
                while let Ok(ev) = rx.recv() {
                    let _ = handle.emit("engine-event", event_json(ev));
                }
            });

            if !engine.folders().is_empty() && model_ok() {
                if let Err(e) = engine.start_indexing() {
                    eprintln!("start_indexing: {e:#}");
                }
            }
            app.manage(engine);

            app.set_menu(build_menu(app.handle())?)?;
            // Cmd+Shift+Space on macOS, Ctrl+Shift+Space elsewhere.
            let sc = if cfg!(target_os = "macos") { shortcut } else { shortcut_ctrl };
            if let Err(e) = app.global_shortcut().register(sc) {
                eprintln!("global shortcut: {e}");
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
            search, status, counts, folders, add_folder, remove_folder, suggested_folders, pick_folder, start_indexing,
            pause, resume, model_present, download_models, open_hit, reveal_path, quit, thumbnail
        ])
        .build(tauri::generate_context!())
        .expect("error while building DigUp")
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
