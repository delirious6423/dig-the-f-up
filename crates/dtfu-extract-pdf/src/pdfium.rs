//! PDF backend on pdfium, via the `pdfium-render` crate (dynamic binding, no bundled library).
//!
//! Compiled on every non-macOS target, and on macOS only with `--features pdfium` (where it
//! is tested side by side with the PDFKit backend in `mac.rs`).
//!
//! ## Obtaining the pdfium library
//! Download a prebuilt archive for your platform from the releases page of
//! <https://github.com/bblanchon/pdfium-binaries/releases> (e.g. `pdfium-win-x64.tgz`,
//! `pdfium-linux-x64.tgz`, `pdfium-mac-arm64.tgz`), extract it, and point `DTFU_PDFIUM_DIR`
//! at the folder that holds the library: `pdfium.dll`, `libpdfium.so` or `libpdfium.dylib`
//! (the archive keeps it in `bin/` or `lib/`). Do not commit the binaries to the repo.
//!
//! ## Library lookup order
//! 1. `$DTFU_PDFIUM_DIR`
//! 2. `<exe folder>/../lib/Dig-the-f-up` (Linux packages), then the folder containing the running executable
//! 3. the system library search path
//!
//! If none binds, every call returns `PdfError::Unsupported` naming `DTFU_PDFIUM_DIR`.
//! Nothing panics. The binding is done once per process and cached.
use super::{PdfError, Rendered};
use pdfium_render::prelude::*;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

const ENV_DIR: &str = "DTFU_PDFIUM_DIR";

static PDFIUM: OnceLock<Result<Pdfium, String>> = OnceLock::new();

fn candidate_dirs() -> Vec<PathBuf> {
    let mut dirs = Vec::new();
    if let Some(d) = std::env::var_os(ENV_DIR) {
        dirs.push(PathBuf::from(d));
    }
    if let Some(dir) = std::env::current_exe()
        .ok()
        .and_then(|e| e.parent().map(Path::to_path_buf))
    {
        // Linux packages (deb, AppImage) put bundled resources in <prefix>/lib/<product name>, next to <prefix>/bin.
        #[cfg(target_os = "linux")]
        dirs.push(dir.join("../lib/Dig-the-f-up"));
        dirs.push(dir);
    }
    // No "./pdfium" relative to the working directory: a library found there could be planted by a folder the user opens.
    dirs
}

fn bind() -> Result<Pdfium, String> {
    for dir in candidate_dirs() {
        let file = Pdfium::pdfium_platform_library_name_at_path(&dir);
        if let Ok(bindings) = Pdfium::bind_to_library(&file) {
            return Ok(Pdfium::new(bindings));
        }
    }
    match Pdfium::bind_to_system_library() {
        Ok(bindings) => Ok(Pdfium::new(bindings)),
        Err(e) => Err(format!(
            "pdf needs the pdfium library: set {ENV_DIR} to the folder containing it \
             (pdfium.dll / libpdfium.so / libpdfium.dylib; see dtfu-extract-pdf docs): {e}"
        )),
    }
}

fn pdfium() -> Result<&'static Pdfium, PdfError> {
    PDFIUM
        .get_or_init(bind)
        .as_ref()
        .map_err(|m| PdfError::Unsupported(m.clone()))
}

/// True when the pdfium library can be bound. Tests use this to skip.
#[cfg(test)]
pub(crate) fn available() -> bool {
    pdfium().is_ok()
}

/// Backed by a pdfium document. Not `Sync`-dependent on callers: use from one thread at a time.
pub struct PdfDoc {
    doc: PdfDocument<'static>,
}

impl PdfDoc {
    pub fn open(path: &Path) -> Result<PdfDoc, PdfError> {
        let lib = pdfium()?;
        let doc = lib.load_pdf_from_file(path, None).map_err(|e| match e {
            PdfiumError::PdfiumLibraryInternalError(PdfiumInternalError::PasswordError) => {
                PdfError::Locked
            }
            e => PdfError::Unreadable(format!("can't open PDF: {e}")),
        })?;
        Ok(PdfDoc { doc })
    }

    pub fn page_count(&self) -> usize {
        self.doc.pages().len() as usize
    }

    pub fn title(&self) -> Option<String> {
        let tag = self.doc.metadata().get(PdfDocumentMetadataTagType::Title)?;
        let t = tag.value().trim();
        (!t.is_empty()).then(|| t.to_string())
    }

    pub fn text(&self, page: usize) -> Option<String> {
        let idx = PdfPageIndex::try_from(page).ok()?;
        let p = self.doc.pages().get(idx).ok()?;
        Some(p.text().map(|t| t.all()).unwrap_or_default())
    }

    pub fn render(&self, page: usize, max_side: u32) -> Result<Rendered, PdfError> {
        let no_page = || PdfError::Unreadable("no such page".into());
        let idx = PdfPageIndex::try_from(page).map_err(|_| no_page())?;
        let p = self.doc.pages().get(idx).map_err(|_| no_page())?;
        // page.width()/height() already reflect /Rotate (pdfium swaps them for 90/270),
        // and render_with_config draws the page rotated, so the output size needs no swap.
        let (pw, ph) = (p.width().value as f64, p.height().value as f64);
        let max_side = max_side.min(8192);
        if !(pw.is_finite() && ph.is_finite() && pw > 0.0 && ph > 0.0) || max_side == 0 {
            return Err(PdfError::Unreadable("empty page".into()));
        }
        let scale = max_side as f64 / pw.max(ph);
        let w = ((pw * scale).round() as u32).max(1);
        let h = ((ph * scale).round() as u32).max(1);
        let cfg = PdfRenderConfig::new()
            .set_fixed_size(w as Pixels, h as Pixels)
            .set_clear_color(PdfColor::WHITE);
        let bmp = p
            .render_with_config(&cfg)
            .map_err(|e| PdfError::Unreadable(format!("render: {e}")))?;
        let rgba = bmp.as_rgba_bytes();
        if bmp.width() as u32 != w
            || bmp.height() as u32 != h
            || rgba.len() != w as usize * h as usize * 4
        {
            return Err(PdfError::Unreadable("render: unexpected bitmap size".into()));
        }
        let mut rgb = Vec::with_capacity(w as usize * h as usize * 3);
        #[allow(clippy::chunks_exact_to_as_chunks)]
        for px in rgba.chunks_exact(4) {
            rgb.extend_from_slice(&px[..3]);
        }
        Ok(Rendered { width: w, height: h, rgb })
    }
}
