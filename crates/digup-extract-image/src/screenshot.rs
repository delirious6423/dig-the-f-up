//! Screenshot detection: the macOS capture xattr first, then file-name prefixes.
//! Same rules as Swift Rules.isScreenshot. Names such as
//! "Screenshot 2026-10-05 at 9.41.07 PM.png" (U+202F before AM/PM) match the prefix.

use std::path::Path;

pub const SCREENSHOT_PREFIXES: [&str; 4] = ["Screenshot", "Screen Shot", "CleanShot", "SCR-"];
pub const SCREENSHOT_XATTR: &str = "com.apple.metadata:kMDItemIsScreenCapture";

/// True when the file carries the macOS screen-capture mark, or its name starts with a screenshot prefix.
pub fn is_screenshot(path: &Path) -> bool {
    if has_capture_xattr(path) {
        return true;
    }
    path.file_name()
        .and_then(|name| name.to_str())
        .is_some_and(|name| SCREENSHOT_PREFIXES.iter().any(|prefix| name.starts_with(prefix)))
}

#[cfg(target_os = "macos")]
fn has_capture_xattr(path: &Path) -> bool {
    matches!(xattr::get(path, SCREENSHOT_XATTR), Ok(Some(value)) if !value.is_empty())
}

#[cfg(not(target_os = "macos"))]
fn has_capture_xattr(_path: &Path) -> bool {
    false
}
