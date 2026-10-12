//! Port of Swift `IndexOptions` (docs/spec-pipeline.md section 3.1).

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;

#[derive(Debug, Clone)]
pub struct IndexOptions {
    pub photo_budget: usize,
    pub screenshot_budget: usize,
    pub page_budget: usize,
    pub frame_budget: usize,
    pub frame_interval: f64,
    pub audio_window: f64,
    pub audio_hop: f64,
    pub min_image_side: u32,
    pub big_pdf_pages: usize,
    pub pdf_step: usize,
    pub doc_step: usize,
    pub max_pages: usize,
    pub max_chunks: usize,
    pub min_page_text: usize,
    pub code_stretch: usize,
    pub max_code_bytes: u64,
    pub max_notebook_bytes: u64,
    pub include_code: bool,
    pub include_datasets: bool,
    pub excluded_folders: Vec<PathBuf>,
    /// New files indexed at once regardless of power state (Swift: 50).
    pub new_files_limit: usize,
    /// Skip embedding a file whose bytes equal an already indexed file of the same kind (copies its rows instead).
    /// Not part of the fingerprint: vectors are identical either way.
    pub dedup_by_content: bool,
    /// Files above this size are never hashed for deduplication.
    pub dedup_max_bytes: u64,
    /// Frecency score per path (files the user opens often). Pending files with a score are indexed before the rest
    /// of their kind, highest first; the kind order and newest-first order are otherwise unchanged. `None` keeps the
    /// Swift order exactly. Not part of the fingerprint: it only changes the order of work.
    pub priority: Option<Arc<HashMap<String, f64>>>,
}

impl Default for IndexOptions {
    fn default() -> Self {
        Self {
            photo_budget: 140,
            screenshot_budget: 280,
            page_budget: 280,
            frame_budget: 70,
            frame_interval: 3.0,
            audio_window: 30.0,
            audio_hop: 25.0,
            min_image_side: 256,
            big_pdf_pages: 50,
            pdf_step: 30,
            doc_step: 40,
            max_pages: 5000,
            max_chunks: 5000,
            min_page_text: 200,
            code_stretch: 1800,
            max_code_bytes: 1 << 20,
            max_notebook_bytes: 20 << 20,
            include_code: false,
            include_datasets: false,
            excluded_folders: Vec::new(),
            new_files_limit: 50,
            dedup_by_content: true,
            dedup_max_bytes: 512 << 20,
            priority: None,
        }
    }
}

/// Bump when extraction changes in a way that should re-embed (chunk size, render sizes, OCR choice...). Swift has no
/// such version, so changes there never trigger a reindex (docs/spec-pipeline.md section 2.3).
pub const EXTRACTOR_VERSION: u32 = 1;

impl IndexOptions {
    /// Settings that change the vectors. Swift's literal plus our extractor version.
    pub fn fingerprint(&self) -> String {
        format!(
            "photo{} shot{} page{} frame{}/{:.1}s audio{:.1}/{:.1}s ext{}",
            self.photo_budget,
            self.screenshot_budget,
            self.page_budget,
            self.frame_budget,
            self.frame_interval,
            self.audio_window,
            self.audio_hop,
            EXTRACTOR_VERSION
        )
    }

    /// Fingerprint of the code index (Swift: `code1 lines{1800}`).
    pub fn code_fingerprint(&self) -> String {
        format!("code1 lines{} ext{}", self.code_stretch, EXTRACTOR_VERSION)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fingerprint_matches_swift_literal_plus_version() {
        assert_eq!(IndexOptions::default().fingerprint(), "photo140 shot280 page280 frame70/3.0s audio30.0/25.0s ext1");
        assert_eq!(IndexOptions::default().code_fingerprint(), "code1 lines1800 ext1");
    }
}
