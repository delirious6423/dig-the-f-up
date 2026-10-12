//! The one data-directory story for Dig-the-f-up (macOS: everything lives under
//! `~/Library/Application Support/Dig-the-f-up`).
//!
//! Final layout (see README.md, "Your data"):
//! - `<base>/Dig-the-f-up/index/`, `<base>/Dig-the-f-up/settings.json`: the desktop app's index and folder list
//! - `<base>/Dig-the-f-up/Models/`: the two GGUF model files
//! - `<base>/Dig-the-f-up/logs/dtfu.log` (+ `dtfu.log.1` after rotation): the desktop app's local log
//!
//! `<base>` is `dirs::data_dir()`. Earlier builds used `<base>/DigUp-rs` and then `<base>/DigUp` for the index. The
//! first of those folders that holds an index or settings file is still used while the new folder holds none, so
//! nothing is moved or copied. Models already in `<base>/DigUp/Models` (where the original Swift DigUp app keeps them)
//! are used rather than downloaded again. Nothing here ever moves, renames or deletes user data.

use std::path::{Path, PathBuf};

/// Current application data folder name under the platform data directory.
pub const APP_DIR_NAME: &str = "Dig-the-f-up";
/// Folder names used by earlier builds for the app's index and settings, newest first.
pub const LEGACY_APP_DIR_NAMES: [&str; 2] = ["DigUp", "DigUp-rs"];
/// Folder whose `Models/` sub-folder is shared with the original Swift DigUp app.
pub const SHARED_MODELS_APP_DIR: &str = "DigUp";
/// Sub-folder of the app data folder that holds the model files.
pub const MODELS_DIR_NAME: &str = "Models";

/// The platform data directory (`dirs::data_dir()`), or the current directory if there is none.
fn platform_data_dir() -> PathBuf {
    dirs::data_dir().unwrap_or_else(|| PathBuf::from("."))
}

/// The app data folder for this user: `<base>/Dig-the-f-up`, or a legacy folder when only that one holds an index or
/// settings file.
pub fn app_data_dir() -> PathBuf {
    resolve_app_data_dir(&platform_data_dir())
}

fn has_data(dir: &Path) -> bool {
    dir.join("index").exists() || dir.join("settings.json").exists()
}

/// Pure form of [`app_data_dir`] for a given platform data directory `base`.
pub fn resolve_app_data_dir(base: &Path) -> PathBuf {
    let current = base.join(APP_DIR_NAME);
    if has_data(&current) {
        return current;
    }
    LEGACY_APP_DIR_NAMES.iter().map(|n| base.join(n)).find(|d| has_data(d)).unwrap_or(current)
}

/// The models folder: `$DTFU_MODELS` if set and non-empty, else [`resolve_models_dir`] for the platform data
/// directory.
pub fn models_dir() -> PathBuf {
    if let Some(dir) = std::env::var_os(crate::MODELS_ENV).filter(|v| !v.is_empty()) {
        return PathBuf::from(dir);
    }
    resolve_models_dir(&platform_data_dir())
}

/// Pure form of [`models_dir`] (without the environment override) for a platform data directory `base`.
/// An existing `<base>/Dig-the-f-up/Models` wins, then the Swift app's `<base>/DigUp/Models`, then
/// `<app data>/Models`.
pub fn resolve_models_dir(base: &Path) -> PathBuf {
    [base.join(APP_DIR_NAME), base.join(SHARED_MODELS_APP_DIR)]
        .into_iter()
        .map(|d| d.join(MODELS_DIR_NAME))
        .find(|d| d.is_dir())
        .unwrap_or_else(|| resolve_app_data_dir(base).join(MODELS_DIR_NAME))
}

/// Where the `ocrs` OCR models are downloaded: `<base>/Dig-the-f-up/ocr`, one of the folders the `ocrs` engine
/// searches (see `dtfu-ocr`'s `ocrs_backend`). Used off macOS, where Apple Vision is the OCR engine.
pub fn ocrs_dir() -> PathBuf {
    platform_data_dir().join(APP_DIR_NAME).join(OCRS_DIR_NAME)
}

/// Sub-folder of `<base>/Dig-the-f-up` that holds the `ocrs` models.
pub const OCRS_DIR_NAME: &str = "ocr";

/// The log folder of the desktop app: `<app data>/logs`.
pub fn logs_dir() -> PathBuf {
    app_data_dir().join("logs")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fresh_machine_uses_new_name() {
        let base = tempfile::tempdir().unwrap();
        assert_eq!(resolve_app_data_dir(base.path()), base.path().join("Dig-the-f-up"));
        assert_eq!(resolve_models_dir(base.path()), base.path().join("Dig-the-f-up/Models"));
    }

    #[test]
    fn legacy_index_is_kept_in_place_not_moved() {
        let base = tempfile::tempdir().unwrap();
        let legacy = base.path().join("DigUp-rs");
        std::fs::create_dir_all(legacy.join("index")).unwrap();
        std::fs::write(legacy.join("settings.json"), b"{}").unwrap();
        assert_eq!(resolve_app_data_dir(base.path()), legacy);
        // Models follow the app folder when nothing has been downloaded anywhere yet.
        assert_eq!(resolve_models_dir(base.path()), legacy.join("Models"));
        // Nothing was moved.
        assert!(legacy.join("index").is_dir());
        assert!(!base.path().join("Dig-the-f-up").exists());
    }

    #[test]
    fn newer_legacy_folder_wins_over_older() {
        let base = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(base.path().join("DigUp-rs/index")).unwrap();
        std::fs::create_dir_all(base.path().join("DigUp/index")).unwrap();
        assert_eq!(resolve_app_data_dir(base.path()), base.path().join("DigUp"));
    }

    #[test]
    fn current_folder_with_index_wins_over_legacy() {
        let base = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(base.path().join("DigUp-rs/index")).unwrap();
        std::fs::create_dir_all(base.path().join("Dig-the-f-up/index")).unwrap();
        assert_eq!(resolve_app_data_dir(base.path()), base.path().join("Dig-the-f-up"));
    }

    #[test]
    fn swift_app_models_are_shared() {
        let base = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(base.path().join("DigUp-rs/index")).unwrap();
        std::fs::create_dir_all(base.path().join("DigUp/Models")).unwrap();
        assert_eq!(resolve_models_dir(base.path()), base.path().join("DigUp/Models"));
        // A models folder of our own wins.
        std::fs::create_dir_all(base.path().join("Dig-the-f-up/Models")).unwrap();
        assert_eq!(resolve_models_dir(base.path()), base.path().join("Dig-the-f-up/Models"));
    }

    #[test]
    fn folder_with_only_models_does_not_hide_legacy_index() {
        // A DigUp/ holding only models (the Swift app's) must not shadow the legacy index.
        let base = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(base.path().join("DigUp/Models")).unwrap();
        std::fs::create_dir_all(base.path().join("DigUp-rs/index")).unwrap();
        assert_eq!(resolve_app_data_dir(base.path()), base.path().join("DigUp-rs"));
    }
}
