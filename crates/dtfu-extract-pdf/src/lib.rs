//! PDF page text, page count, title and page rendering (PDFKit on macOS, pdfium elsewhere).
//! `PdfDoc` is not `Send`/`Sync`: use it from one thread at a time.
use std::fmt;
#[allow(unused_imports)]
use std::path::Path;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PdfError {
    Locked,
    Unreadable(String),
    Unsupported(String),
}
impl fmt::Display for PdfError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            PdfError::Locked => write!(f, "pdf is locked"),
            PdfError::Unreadable(s) => write!(f, "pdf unreadable: {s}"),
            PdfError::Unsupported(s) => write!(f, "{s}"),
        }
    }
}
impl std::error::Error for PdfError {}

pub struct Page {
    pub index: usize,
    pub text: String,
}
pub struct Rendered {
    pub width: u32,
    pub height: u32,
    pub rgb: Vec<u8>,
}

pub mod text_problem {
    use dtfu_text::PageTextProblem;

    /// Swift `PageTextProblem.check` as a string: `Some("glyphOrder")`, `Some("lostLetters")` or `None`.
    pub fn page_text_problem(text: &str) -> Option<String> {
        PageTextProblem::check(text).map(|problem| {
            match problem {
                PageTextProblem::GlyphOrder => "glyphOrder",
                PageTextProblem::LostLetters => "lostLetters",
            }
            .to_string()
        })
    }
}

#[cfg(target_os = "macos")]
mod mac;
#[cfg(target_os = "macos")]
pub use mac::PdfDoc;

#[cfg(any(not(target_os = "macos"), feature = "pdfium"))]
pub mod pdfium;
#[cfg(not(target_os = "macos"))]
pub use pdfium::PdfDoc;

#[cfg(test)]
mod tests;
