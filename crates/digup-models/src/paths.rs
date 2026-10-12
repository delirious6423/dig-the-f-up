//! The one data-directory story for DigUp (macOS: everything lives under `~/Library/Application Support/DigUp`).
//!
//! Final layout (see `app/README.md`, "Data"):
//! - `<base>/DigUp/index/`, `<base>/DigUp/settings.json`: the desktop app's index and folder list
//! - `<base>/DigUp/Models/`: the two GGUF model files
//! - `<base>/DigUp/logs/digup.log` (+ `digup.log.1` after rotation): the desktop app's local log
//!
//! `<base>` is `dirs::data_dir()`. Earlier builds used `<base>/DigUp-rs` for the index. That folder is still used
//! when the new folder holds no index yet, so nothing is moved or copied; the models folder then follows the same
//! rule. Nothing here ever moves, renames or deletes user data.

use std::path::{Path, PathBuf};

/// Current application data folder name under the platform data directory.
pub const APP_DIR_NAME: &str = "DigUp";
/// Folder name used by earlier builds for the app's index and settings.
pub const LEGACY_APP_DIR_NAME: &str = "DigUp-rs";
/// Sub-folder of the app data folder that holds the model files.
pub const MODELS_DIR_NAME: &str = "Models";

/// The platform data directory (`dirs::data_dir()`), or the current directory if there is none.
fn platform_data_dir() -> PathBuf {
    dirs::data_dir().unwrap_or_else(|| PathBuf::from("."))
}

/// The app data folder for this user: `<base>/DigUp`, or the legacy `<base>/DigUp-rs` when only that one holds
/// an index or settings file.
pub fn app_data_dir() -> PathBuf {
    resolve_app_data_dir(&platform_data_dir())
}

/// Pure form of [`app_data_dir`] for a given platform data directory `base`.
pub fn resolve_app_data_dir(base: &Path) -> PathBuf {
    let current = base.join(APP_DIR_NAME);
    let legacy = base.join(LEGACY_APP_DIR_NAME);
    let current_has_data = current.join("index").exists() || current.join("settings.json").exists();
    if !current_has_data && legacy.is_dir() {
        legacy
    } else {
        current
    }
}

/// The models folder: `$DIGUP_MODELS` if set and non-empty, else [`resolve_models_dir`] for the platform data
/// directory.
pub fn models_dir() -> PathBuf {
    if let Some(dir) = std::env::var_os(crate::MODELS_ENV).filter(|v| !v.is_empty()) {
        return PathBuf::from(dir);
    }
    resolve_models_dir(&platform_data_dir())
}

/// Pure form of [`models_dir`] (without the environment override) for a platform data directory `base`.
/// Uses `<app data>/Models`, except that an existing `<base>/DigUp/Models` always wins, so models downloaded
/// into the current folder are found even when the legacy app folder is in use.
pub fn resolve_models_dir(base: &Path) -> PathBuf {
    let current_models = base.join(APP_DIR_NAME).join(MODELS_DIR_NAME);
    if current_models.is_dir() {
        return current_models;
    }
    resolve_app_data_dir(base).join(MODELS_DIR_NAME)
}

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
        assert_eq!(resolve_app_data_dir(base.path()), base.path().join("DigUp"));
        assert_eq!(
            resolve_models_dir(base.path()),
            base.path().join("DigUp/Models")
        );
    }

    #[test]
    fn legacy_index_is_kept_in_place_not_moved() {
        let base = tempfile::tempdir().unwrap();
        let legacy = base.path().join("DigUp-rs");
        std::fs::create_dir_all(legacy.join("index")).unwrap();
        std::fs::write(legacy.join("settings.json"), b"{}").unwrap();
        assert_eq!(resolve_app_data_dir(base.path()), legacy);
        // Models follow the app folder when nothing has been downloaded into the new folder yet.
        assert_eq!(resolve_models_dir(base.path()), legacy.join("Models"));
        // Nothing was moved.
        assert!(legacy.join("index").is_dir());
        assert!(!base.path().join("DigUp").exists());
    }

    #[test]
    fn current_folder_with_index_wins_over_legacy() {
        let base = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(base.path().join("DigUp-rs/index")).unwrap();
        std::fs::create_dir_all(base.path().join("DigUp/index")).unwrap();
        assert_eq!(resolve_app_data_dir(base.path()), base.path().join("DigUp"));
    }

    #[test]
    fn models_downloaded_into_new_folder_are_found_with_legacy_index() {
        let base = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(base.path().join("DigUp-rs/index")).unwrap();
        std::fs::create_dir_all(base.path().join("DigUp/Models")).unwrap();
        assert_eq!(
            resolve_models_dir(base.path()),
            base.path().join("DigUp/Models")
        );
    }

    #[test]
    fn folder_with_only_models_does_not_hide_legacy_index() {
        // An empty DigUp/ (created by an older download) must not shadow the legacy index.
        let base = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(base.path().join("DigUp")).unwrap();
        std::fs::create_dir_all(base.path().join("DigUp-rs/index")).unwrap();
        assert_eq!(
            resolve_app_data_dir(base.path()),
            base.path().join("DigUp-rs")
        );
    }
}
