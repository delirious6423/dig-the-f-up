//! OCR via the pure-Rust `ocrs` engine (rten models). Compiled on every non-macOS
//! target, and on macOS only with `--features ocrs`.
//!
//! Model files are not downloaded by this crate: the desktop app and `dtfu download` fetch them (pinned by size and
//! SHA-256 in `dtfu-models`, `OCRS_FILES`) into the data-dir folder below. They come from the ocrs-models bucket
//! (https://github.com/robertknight/ocrs-models, linked from the ocrs README). The engine looks in:
//!
//! 1. `$DTFU_OCR_MODELS` (a directory),
//! 2. `<dir of the executable>/models/ocr/`,
//! 3. `<platform data dir>/Dig-the-f-up/ocr/` (`~/Library/Application Support` on macOS,
//!    `~/.local/share` on Linux, `%APPDATA%` on Windows).
//!
//! The files it looks for are:
//!
//! * `text-detection.rten`:
//!   <https://ocrs-models.s3-accelerate.amazonaws.com/text-detection.rten>
//! * `text-recognition.rten`:
//!   <https://ocrs-models.s3-accelerate.amazonaws.com/text-recognition.rten>
//!
//! e.g. `curl -LO https://ocrs-models.s3-accelerate.amazonaws.com/text-detection.rten`.
//! The first directory that contains both files wins. If none does, calls return `None` (a single note is printed
//! to stderr) and loading is retried every 30 s, so models downloaded while the app runs are picked up. Nothing
//! panics.
//!
//! The engine is loaded lazily on first use and cached in a `OnceLock`.

use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

use ocrs::{ImageSource, OcrEngine, OcrEngineParams};
use rten::Model;

/// Environment variable naming a directory that holds the model files.
pub const MODELS_ENV: &str = "DTFU_OCR_MODELS";
/// Detection model file name.
pub const DETECTION_MODEL: &str = "text-detection.rten";
/// Recognition model file name.
pub const RECOGNITION_MODEL: &str = "text-recognition.rten";

static ENGINE: OnceLock<OcrEngine> = OnceLock::new();
/// When loading last failed. The app downloads the models in the background, so a failed load is retried (at most
/// once per [`RETRY_EVERY`]) instead of disabling OCR until the next launch.
static LAST_FAILURE: Mutex<Option<Instant>> = Mutex::new(None);
const RETRY_EVERY: Duration = Duration::from_secs(30);

/// True when the engine is loaded or can be loaded now (it is loaded by this call).
pub fn ready() -> bool {
    engine().is_some()
}

/// The loaded engine, loading it on first use or after [`RETRY_EVERY`] since the last failed attempt.
fn engine() -> Option<&'static OcrEngine> {
    if let Some(e) = ENGINE.get() {
        return Some(e);
    }
    let mut last = LAST_FAILURE.lock().unwrap_or_else(|e| e.into_inner());
    if let Some(e) = ENGINE.get() {
        return Some(e); // loaded by another thread while this one waited
    }
    if last.is_some_and(|t| t.elapsed() < RETRY_EVERY) {
        return None;
    }
    match init_engine(last.is_none()) {
        Some(e) => {
            *last = None;
            Some(ENGINE.get_or_init(|| e))
        }
        None => {
            *last = Some(Instant::now());
            None
        }
    }
}

/// Recognize text in a packed RGB8 image. Returns lines joined by "\n", or `None`
/// if the image is empty, the engine is unavailable, or no text was found.
pub fn recognize(width: usize, height: usize, rgb: &[u8]) -> Option<String> {
    let n = width.checked_mul(height)?;
    if n == 0 || rgb.len() < n.checked_mul(3)? {
        return None;
    }
    let engine = engine()?;
    let w = u32::try_from(width).ok()?;
    let h = u32::try_from(height).ok()?;
    let src = ImageSource::from_bytes(&rgb[..n * 3], (w, h)).ok()?;
    let input = engine.prepare_input(src).ok()?;
    let text = engine.get_text(&input).ok()?;
    if text.trim().is_empty() {
        None
    } else {
        Some(text)
    }
}

/// Pure search-order logic, with the environment and filesystem injected so it can be
/// tested. `env` is the value of `DTFU_OCR_MODELS`, `exe_dir` the executable's
/// directory and `data_dir` the platform data directory. `has_models` reports whether
/// a directory holds both model files. Returns the first directory that does.
pub fn find_model_dir(
    env: Option<OsString>,
    exe_dir: Option<&Path>,
    data_dir: Option<&Path>,
    has_models: impl Fn(&Path) -> bool,
) -> Option<PathBuf> {
    let mut candidates: Vec<PathBuf> = Vec::with_capacity(3);
    if let Some(env) = env.filter(|v| !v.is_empty()) {
        candidates.push(PathBuf::from(env));
    }
    if let Some(exe) = exe_dir {
        candidates.push(exe.join("models").join("ocr"));
    }
    if let Some(data) = data_dir {
        candidates.push(data.join("Dig-the-f-up").join("ocr"));
    }
    candidates.into_iter().find(|d| has_models(d))
}

/// Model directory search using the real environment and filesystem.
fn default_model_dir() -> Option<PathBuf> {
    let env = std::env::var_os(MODELS_ENV);
    let exe_dir = std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(Path::to_path_buf));
    let data_dir = dirs::data_dir();
    find_model_dir(env, exe_dir.as_deref(), data_dir.as_deref(), |d| {
        d.join(DETECTION_MODEL).is_file() && d.join(RECOGNITION_MODEL).is_file()
    })
}

/// True if the model files can be found (used by tests to skip cleanly).
#[cfg(test)]
pub fn models_available() -> bool {
    default_model_dir().is_some()
}

/// `first`: report a missing model folder (only on the first attempt; later retries stay quiet).
fn init_engine(first: bool) -> Option<OcrEngine> {
    let Some(dir) = default_model_dir() else {
        if !first {
            return None;
        }
        eprintln!(
            "dtfu-ocr: ocrs models not found; OCR disabled. Set {MODELS_ENV} or place \
             {DETECTION_MODEL} and {RECOGNITION_MODEL} in models/ocr/ next to the binary \
             or in the data dir Dig-the-f-up/ocr/ (see crate docs for URLs)."
        );
        return None;
    };
    match load_engine(&dir) {
        Ok(engine) => Some(engine),
        Err(e) => {
            if first {
                eprintln!("dtfu-ocr: failed to load ocrs models from {}: {e}; OCR disabled for now", dir.display());
            }
            None
        }
    }
}

fn load_engine(dir: &Path) -> Result<OcrEngine, String> {
    let detection = Model::load_file(dir.join(DETECTION_MODEL)).map_err(|e| e.to_string())?;
    let recognition = Model::load_file(dir.join(RECOGNITION_MODEL)).map_err(|e| e.to_string())?;
    OcrEngine::new(OcrEngineParams {
        detection_model: Some(detection),
        recognition_model: Some(recognition),
        ..Default::default()
    })
    .map_err(|e| e.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dirs_with_models<'a>(list: &'a [&'a str]) -> impl Fn(&Path) -> bool + 'a {
        move |d: &Path| list.iter().any(|s| Path::new(s) == d)
    }

    #[test]
    fn search_order_env_then_exe_then_data() {
        let exe = Path::new("/opt/app");
        let data = Path::new("/home/u/.local/share");
        let env_dir = "/custom/models";
        let exe_models = "/opt/app/models/ocr";
        let data_models = "/home/u/.local/share/Dig-the-f-up/ocr";

        // Env wins when it has the models, even if the others also have them.
        let all = [env_dir, exe_models, data_models];
        let got = find_model_dir(Some(OsString::from(env_dir)), Some(exe), Some(data), dirs_with_models(&all));
        assert_eq!(got, Some(PathBuf::from(env_dir)));

        // Env set but empty on disk: fall through to exe dir.
        let rest = [exe_models, data_models];
        let got = find_model_dir(Some(OsString::from(env_dir)), Some(exe), Some(data), dirs_with_models(&rest));
        assert_eq!(got, Some(PathBuf::from(exe_models)));

        // Only the data dir has models.
        let only_data = [data_models];
        let got = find_model_dir(None, Some(exe), Some(data), dirs_with_models(&only_data));
        assert_eq!(got, Some(PathBuf::from(data_models)));

        // Nothing anywhere.
        let none: [&str; 0] = [];
        assert_eq!(find_model_dir(None, Some(exe), Some(data), dirs_with_models(&none)), None);
    }

    #[test]
    fn empty_env_value_is_ignored() {
        let exe_models = "/opt/app/models/ocr";
        let list = [exe_models];
        let got = find_model_dir(Some(OsString::new()), Some(Path::new("/opt/app")), None, dirs_with_models(&list));
        assert_eq!(got, Some(PathBuf::from(exe_models)));
    }

    #[test]
    fn no_dirs_known_returns_none() {
        assert_eq!(find_model_dir(None, None, None, |_| true), None);
    }

    #[test]
    fn empty_image_is_none() {
        assert_eq!(recognize(0, 0, &[]), None);
        assert_eq!(recognize(0, 5, &[]), None);
        assert_eq!(recognize(5, 0, &[]), None);
        // Short buffer is rejected before any engine work.
        assert_eq!(recognize(10, 10, &[0u8; 5]), None);
    }
}
