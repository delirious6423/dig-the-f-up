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
    use unicode_normalization::UnicodeNormalization;

    fn vowel_sign(v: u32) -> bool {
        matches!(v, 0x09BE..=0x09C4 | 0x09C7 | 0x09C8 | 0x09CB | 0x09CC | 0x09D7 | 0x09E2 | 0x09E3
            | 0x093A | 0x093B | 0x093E..=0x094C | 0x094E | 0x094F | 0x0955..=0x0957 | 0x0962 | 0x0963)
    }
    fn consonant_or_nukta(v: u32) -> bool {
        matches!(v, 0x0995..=0x09B9 | 0x09BC | 0x09CE | 0x09DC | 0x09DD | 0x09DF | 0x09F0 | 0x09F1
            | 0x0915..=0x0939 | 0x093C | 0x0958..=0x095F | 0x0979..=0x097F)
    }

    /// Port of Swift `PageTextProblem.check`: `Some("glyphOrder")`, `Some("lostLetters")` or `None`.
    pub fn page_text_problem(text: &str) -> Option<String> {
        let (mut vowels, mut misplaced, mut lost) = (0usize, 0usize, 0usize);
        let (mut prev, mut before) = (' ', ' ');
        for c in text.nfc() {
            let v = c as u32;
            if vowel_sign(v) {
                vowels += 1;
                if !consonant_or_nukta(prev as u32) {
                    misplaced += 1;
                }
            }
            if before == '\u{0627}'
                && matches!(prev, '?' | '.' | '\u{FFFD}')
                && (0x0600..=0x06FF).contains(&v)
                && c.is_alphabetic()
            {
                lost += 1;
            }
            before = prev;
            prev = c;
        }
        if vowels >= 10 && misplaced * 20 >= vowels {
            return Some("glyphOrder".into());
        }
        if lost >= 2 {
            return Some("lostLetters".into());
        }
        None
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
