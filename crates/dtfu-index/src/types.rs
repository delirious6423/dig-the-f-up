//! Types shared by the pipeline stages. `prepare` produces `Prepared`, `commit` consumes it.

use anyhow::Result;
use dtfu_core::FileKind;
use dtfu_extract_audio::AudioWindow;
use dtfu_extract_text::code::CodeStretch;
use dtfu_extract_text::docs::DocChunk;
use dtfu_store::index::FileRecord;
use std::ops::Range;
use std::path::PathBuf;

/// The model, as the pipeline needs it. `dtfu_embed::Embedder` implements it (feature `llama`); tests use a fake.
/// All vectors are L2-normalised, 768 floats.
pub trait Embed {
    /// `vector_source` recorded in the store `meta` (what made the vectors). Different value = never mix.
    fn vector_source(&self) -> String;
    /// Texts are already prompt-formatted (`dtfu_core::doc_prompt`). One result per input, in order.
    fn embed_texts(&mut self, texts: &[String]) -> Result<Vec<Vec<f32>>>;
    /// Packed RGB8 pixels; `budget` is one of 70/140/280/560/1120 vision tokens.
    fn embed_image_rgb(&mut self, width: usize, height: usize, rgb: &[u8], budget: usize) -> Result<Vec<f32>>;
    /// Mono 16 kHz samples (at most the first 30 s are used).
    fn embed_audio_samples(&mut self, samples: &[f32]) -> Result<Vec<f32>>;
}

/// Text recognition for screenshots (see [`SystemOcr`] for the engines).
pub trait Ocr: Sync {
    /// `rgb` is packed RGB8. `None` means no text found or OCR unavailable.
    fn recognize(&self, width: usize, height: usize, rgb: &[u8]) -> Option<String>;
    /// Whether text can be read now (false while an engine's model files are still missing).
    fn ready(&self) -> bool {
        true
    }
    /// Whether the best engine (LightOnOCR) is installed, so screenshots read by a weaker one are worth reading again.
    fn best_ready(&self) -> bool {
        false
    }
}

/// The platform's OCR (`dtfu_ocr::recognize_rgb`): Apple Vision with LightOnOCR as its fallback on macOS; LightOnOCR
/// elsewhere, `ocrs` until LightOn's models are downloaded.
pub struct SystemOcr;
impl Ocr for SystemOcr {
    fn recognize(&self, width: usize, height: usize, rgb: &[u8]) -> Option<String> {
        dtfu_ocr::recognize_rgb(width, height, rgb)
    }
    fn ready(&self) -> bool {
        dtfu_ocr::ready()
    }
    fn best_ready(&self) -> bool {
        dtfu_ocr::lighton_available()
    }
}

/// No OCR: screenshots are embedded as pictures only.
pub struct NoOcr;
impl Ocr for NoOcr {
    fn recognize(&self, _: usize, _: usize, _: &[u8]) -> Option<String> {
        None
    }
    fn ready(&self) -> bool {
        false
    }
}

/// One unit of work. `step` is a range of pages (pdf) or chunks (doc); `None` = the first step as Swift defines it.
#[derive(Debug, Clone)]
pub struct Job {
    pub file: FileRecord,
    pub step: Option<Range<usize>>,
    /// blake3 of the file's bytes, set when the file was hashed for deduplication. Commit stores it in `info`.
    pub content_hash: Option<String>,
}

/// Why a file was not indexed. `Skipped` is normal (too small, no text); `Failed` is an error, permanent until the
/// file changes. `Unavailable` means the file's folder is gone or unreadable (drive unplugged or share offline): the
/// row stays `pending` so the file is retried when the drive comes back.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Leave {
    Skipped(String),
    Failed(String),
    /// The file's folder is gone or unreadable: drive unplugged or share offline.
    Unavailable(String),
}

/// What prepare extracted from a file, ready to embed. No embedding happens before commit.
pub enum Payload {
    /// `kind` is `Image` or `Screenshot`. `rgb` are the pixels of the thumbnail (decoded from the JPEG/PNG thumbnail
    /// exactly as Swift embeds its scratch file), `width` x `height` of that thumbnail. `orig_*` are the full size.
    Image {
        kind: FileKind,
        width: u32,
        height: u32,
        rgb: Vec<u8>,
        orig_width: u32,
        orig_height: u32,
        taken: Option<String>,
        ocr: Option<String>,
    },
    /// `chunks` are the slice `range` of `total` chunks (total counted up to `max_chunks + 1`).
    Doc { title: String, total: usize, range: Range<usize>, chars: usize, chunks: Vec<DocChunk> },
    /// Windows are produced lazily so a 3-hour file never sits in memory.
    Audio { duration: f64, windows: Box<dyn Iterator<Item = Result<AudioWindow>> + Send> },
    /// `range` is the 0-based pages wanted this step, `total` the page count of the file.
    Pdf { title: Option<String>, total: usize, range: Range<usize>, pages: Vec<PdfPage> },
    /// Frames are few (at most 1200, deduplicated); the soundtrack is lazy like `Audio`.
    Video {
        duration: f64,
        frames: Vec<VideoFrame>,
        sound: Option<Box<dyn Iterator<Item = Result<AudioWindow>> + Send>>,
    },
    /// Code index only.
    Code { title: String, repo: Option<PathBuf>, lines: usize, cells: Option<usize>, stretches: Vec<CodeStretch> },
}

/// One PDF page as prepare read it. Pages with real text are text; scans and slides carry a rendered picture.
pub struct PdfPage {
    /// 1-based.
    pub number: usize,
    pub text: String,
    /// Packed RGB8 `(width, height, pixels)` of the rendered page; `None` for a text page.
    pub image: Option<(u32, u32, Vec<u8>)>,
    /// The text came from OCR (the text layer was garbled: lost letters).
    pub ocr: bool,
    /// Text read in glyph order (Bengali): never shown or matched, only embedded beside the page picture.
    pub garbled: Option<String>,
}

/// One kept video frame: a stretch `[start, end]` of the video it stands for.
pub struct VideoFrame {
    pub start: f64,
    pub end: f64,
    pub width: u32,
    pub height: u32,
    pub rgb: Vec<u8>,
}

/// Result of the prepare stage for one job.
pub struct Prepared {
    pub job: Job,
    pub result: std::result::Result<Payload, Leave>,
}

#[cfg(feature = "llama")]
impl Embed for dtfu_embed::Embedder {
    /// A tag distinct from the Swift app's: Rust-made indexes never mix with Swift-made ones.
    fn vector_source(&self) -> String {
        "dig-the-f-up llama.cpp b11461 embeddinggemma-2 q8_0".to_string()
    }
    fn embed_texts(&mut self, texts: &[String]) -> Result<Vec<Vec<f32>>> {
        dtfu_embed::Embedder::embed_texts(self, texts)
    }
    fn embed_image_rgb(&mut self, width: usize, height: usize, rgb: &[u8], budget: usize) -> Result<Vec<f32>> {
        dtfu_embed::Embedder::embed_image_rgb(self, width, height, rgb, budget)
    }
    fn embed_audio_samples(&mut self, samples: &[f32]) -> Result<Vec<f32>> {
        dtfu_embed::Embedder::embed_audio_samples(self, samples)
    }
}
