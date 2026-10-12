//! Port of Swift `Estimator` (reference/digup-swift/Sources/DigUpKit/Estimator.swift:1-147) and of the text output of
//! `dtfu estimate` (main.swift:198-273). Formulas: docs/porting/spec-pipeline.md section 3.14. `--json` is not ported here.
//!
//! Code mode: the Swift `--code` flag is `IndexOptions.include_code` in this CLI (see `cmd_index` in main.rs).
//!
//! Unlike Swift there is no Spotlight: PDF page counts come from `PdfDoc::open` (fallback 1) and media durations from
//! `ffprobe` through `dtfu_extract_video::probe` (fallback 0), so a file that can't be read costs nothing.

use std::collections::{BTreeMap, HashMap};
use std::ops::Add;
use std::path::{Path, PathBuf};
use std::time::Instant;

use anyhow::{bail, Result};
use dtfu_crawl::{crawl, Candidate, CrawlOptions, FileKind, FolderSelection};
use dtfu_index::IndexOptions;

/// Arguments of `dtfu estimate`: the folders to look at and the index options (`include_code` = code mode).
#[derive(Debug, Clone)]
pub struct EstimateArgs {
    pub folders: Vec<PathBuf>,
    pub options: IndexOptions,
}

/// Seconds of indexing per unit, measured 2026-10-07 on an M4 Pro with llama.cpp (Q8_0) (Estimator.swift:4-17).
#[derive(Debug, Clone, PartialEq)]
pub struct Costs {
    pub image: f64,
    pub screenshot: f64,
    pub pdf_page: f64,
    pub doc_chunk: f64,
    pub audio_second: f64,
    pub video_second: f64,
    pub code_stretch: f64,
}

impl Default for Costs {
    fn default() -> Self {
        Self {
            image: 0.12,
            screenshot: 0.32,
            pdf_page: 0.09,
            doc_chunk: 0.095,
            audio_second: 0.0089,
            video_second: 0.019,
            code_stretch: 0.037,
        }
    }
}

/// CodeIndexSize (Estimator.swift:21-26).
const CODE_BYTES_PER_CHARACTER: f64 = 2.4;
const CODE_BYTES_PER_STRETCH: f64 = 1600.0;
const CODE_CHARACTERS_PER_STRETCH: i64 = 1400;

/// Estimator.swift:28-61.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Estimate {
    pub files: BTreeMap<FileKind, usize>,
    /// Pages the first pass will read.
    pub pdf_pages: usize,
    pub pdf_pages_total: usize,
    pub audio_seconds: f64,
    pub video_seconds: f64,
    /// Chunks the first pass will read.
    pub doc_chunks: usize,
    /// The first pass.
    pub seconds: f64,
    /// Long PDFs and documents that get a first step in the first pass, and how long the rest takes afterwards.
    pub long_files: usize,
    pub later_seconds: f64,
    pub code_stretches: usize,
    /// Bytes of the code index.
    pub bytes: f64,
}

impl Add for Estimate {
    type Output = Estimate;

    fn add(mut self, b: Estimate) -> Estimate {
        for (kind, count) in b.files {
            *self.files.entry(kind).or_insert(0) += count;
        }
        self.pdf_pages += b.pdf_pages;
        self.pdf_pages_total += b.pdf_pages_total;
        self.audio_seconds += b.audio_seconds;
        self.video_seconds += b.video_seconds;
        self.doc_chunks += b.doc_chunks;
        self.seconds += b.seconds;
        self.long_files += b.long_files;
        self.later_seconds += b.later_seconds;
        self.code_stretches += b.code_stretches;
        self.bytes += b.bytes;
        self
    }
}

/// Estimate for a list of crawl candidates with the default costs (Estimator.estimate, Estimator.swift:66-113).
#[cfg(test)]
fn estimate_candidates(c: &[Candidate], opts: &IndexOptions) -> Estimate {
    estimate_iter(c, opts, &Costs::default())
}

fn estimate_iter<'a>(
    candidates: impl IntoIterator<Item = &'a Candidate>,
    opts: &IndexOptions,
    costs: &Costs,
) -> Estimate {
    let mut e = Estimate::default();
    for file in candidates {
        *e.files.entry(file.kind).or_insert(0) += 1;
        match file.kind {
            FileKind::Image => e.seconds += costs.image,
            FileKind::Screenshot => e.seconds += costs.screenshot,
            FileKind::Pdf => {
                let total = pdf_pages(&file.path);
                let all = total.min(opts.max_pages);
                let first = if total > opts.big_pdf_pages { opts.pdf_step.min(all) } else { all };
                e.pdf_pages_total += total;
                e.pdf_pages += first;
                e.seconds += first as f64 * costs.pdf_page;
                if all > first {
                    e.long_files += 1;
                    e.later_seconds += (all - first) as f64 * costs.pdf_page;
                }
            }
            FileKind::Doc => {
                // 2000 bytes a chunk (the real chunks hold about 1800 characters).
                let all = (file.size / 2000).min(opts.max_chunks as i64).max(1) as usize;
                let first = all.min(opts.doc_step);
                e.doc_chunks += first;
                e.seconds += first as f64 * costs.doc_chunk;
                if all > first {
                    e.long_files += 1;
                    e.later_seconds += (all - first) as f64 * costs.doc_chunk;
                }
            }
            FileKind::Audio => {
                let seconds = media_duration(&file.path);
                e.audio_seconds += seconds;
                e.seconds += seconds * costs.audio_second;
            }
            FileKind::Video => {
                let seconds = media_duration(&file.path);
                e.video_seconds += seconds;
                e.seconds += seconds * costs.video_second;
            }
            FileKind::Code => {
                // A notebook's file is mostly its outputs, which aren't read: a guess at its cells' share.
                let is_notebook = file.path.to_lowercase().ends_with(".ipynb");
                let chars = (if is_notebook { file.size / 8 } else { file.size }).max(0);
                let stretches = ((chars + CODE_CHARACTERS_PER_STRETCH - 1) / CODE_CHARACTERS_PER_STRETCH).max(1);
                e.code_stretches += stretches as usize;
                e.seconds += stretches as f64 * costs.code_stretch;
                e.bytes += chars as f64 * CODE_BYTES_PER_CHARACTER + stretches as f64 * CODE_BYTES_PER_STRETCH;
            }
        }
    }
    e
}

/// Estimates per subfolder one level below `folder`, and for the files directly in it (keyed by `folder` itself).
/// Keys are named under `folder` as given; the walk reports real paths, so both prefixes are matched (Estimator.swift:115-130).
pub fn estimates_by_subfolder(
    candidates: &[Candidate],
    folder: &str,
    opts: &IndexOptions,
    costs: &Costs,
) -> HashMap<String, Estimate> {
    let prefix = if folder.ends_with('/') { folder.to_string() } else { format!("{folder}/") };
    let real = format!("{}/", FolderSelection::real_path(folder));
    let mut groups: HashMap<String, Vec<&Candidate>> = HashMap::new();
    for file in candidates {
        let rest = file.path.strip_prefix(prefix.as_str()).or_else(|| file.path.strip_prefix(real.as_str()));
        let key = rest
            .and_then(|r| r.find('/').map(|i| format!("{prefix}{}", &r[..i])))
            .unwrap_or_else(|| folder.to_string());
        groups.entry(key).or_default().push(file);
    }
    groups.into_iter().map(|(k, files)| (k, estimate_iter(files, opts, costs))).collect()
}

/// Page count (Estimator.pages): the PDF's pages, falling back to 1 when it can't be opened.
fn pdf_pages(path: &str) -> usize {
    dtfu_extract_pdf::PdfDoc::open(Path::new(path)).map(|doc| doc.page_count()).unwrap_or(1)
}

/// Media duration (Estimator.duration): ffprobe's seconds, falling back to 0.
fn media_duration(path: &str) -> f64 {
    dtfu_extract_video::probe(Path::new(path))
        .map(|info| info.duration_secs)
        .ok()
        .filter(|d| d.is_finite() && *d > 0.0)
        .unwrap_or(0.0)
}

fn crawl_options(opts: &IndexOptions) -> CrawlOptions {
    CrawlOptions {
        code: opts.include_code,
        include_code: opts.include_code,
        include_datasets: opts.include_datasets,
        max_code_bytes: opts.max_code_bytes as i64,
        max_notebook_bytes: opts.max_notebook_bytes as i64,
        excluded_folders: opts.excluded_folders.iter().map(|p| p.to_string_lossy().into_owned()).collect(),
        ..CrawlOptions::default()
    }
}

/// `Arguments.folderURL`: expand a leading `~`, make the path absolute and standardize it.
fn folder_string(raw: &Path) -> String {
    let s = raw.to_string_lossy().into_owned();
    let home = std::env::var("HOME").unwrap_or_default();
    let expanded = if s == "~" {
        home
    } else if let Some(rest) = s.strip_prefix("~/") {
        format!("{home}/{rest}")
    } else if s.starts_with('/') {
        s
    } else {
        let cwd = std::env::current_dir().map(|p| p.to_string_lossy().into_owned()).unwrap_or_default();
        format!("{cwd}/{s}")
    };
    dtfu_crawl::crawler::standardize(&expanded)
}

/// `path` with `$HOME` shortened to `~`.
pub fn tilde(path: &str) -> String {
    match std::env::var("HOME") {
        Ok(home) if !home.is_empty() && path.starts_with(&home) => format!("~{}", &path[home.len()..]),
        _ => path.to_string(),
    }
}

/// main.swift:149. `.formatted(.number)` in en_US: thousands separated by commas.
fn number(value: usize) -> String {
    let digits = value.to_string();
    let mut out = String::new();
    for (i, ch) in digits.chars().enumerate() {
        if i > 0 && (digits.len() - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(ch);
    }
    out
}

/// main.swift:151-156.
fn duration(seconds: f64) -> String {
    if seconds < 90.0 {
        format!("{} s", seconds.round() as i64)
    } else if seconds < 5400.0 {
        format!("{} min", (seconds / 60.0).round() as i64)
    } else {
        format!("{:.1} h", seconds / 3600.0)
    }
}

/// `ByteCountFormatter` with the `.file` style (decimal units). Approximates its rounding: one decimal below 10.
fn byte_count(bytes: f64) -> String {
    if bytes < 1000.0 {
        return format!("{} bytes", bytes.round() as i64);
    }
    const UNITS: [&str; 5] = ["KB", "MB", "GB", "TB", "PB"];
    let mut value = bytes / 1000.0;
    let mut unit = 0;
    while value >= 1000.0 && unit < UNITS.len() - 1 {
        value /= 1000.0;
        unit += 1;
    }
    let text = if value < 10.0 { format!("{value:.1}") } else { format!("{:.0}", value) };
    format!("{} {}", text.strip_suffix(".0").unwrap_or(&text), UNITS[unit])
}

/// main.swift:177-179: "N name · N name", biggest first.
fn skipped_line(skipped: &HashMap<String, usize>) -> String {
    let mut items: Vec<(&String, &usize)> = skipped.iter().collect();
    items.sort_by(|a, b| b.1.cmp(a.1).then_with(|| a.0.cmp(b.0)));
    items.iter().map(|(name, count)| format!("{} {}", number(**count), name)).collect::<Vec<_>>().join(" · ")
}

/// `dtfu estimate <folders>`: prints the first-index estimate of each folder (main.swift:198-273, text output).
pub fn estimate(args: &EstimateArgs) -> Result<()> {
    if args.folders.is_empty() {
        bail!("which folders? e.g. dtfu estimate ~/Documents");
    }
    let opts = &args.options;
    let costs = Costs::default();
    let mut total = 0.0;
    for raw in &args.folders {
        let folder = folder_string(raw);
        // One folder that isn't there or can't be read mustn't sink the others.
        let is_folder = Path::new(&folder).is_dir();
        if !is_folder || std::fs::read_dir(&folder).is_err() {
            println!("{}    can't read it (not there, or not allowed in macOS privacy settings)", tilde(&folder));
            continue;
        }
        let started = Instant::now();
        let report = crawl(std::slice::from_ref(&folder), &crawl_options(opts));
        let parts = estimates_by_subfolder(&report.candidates, &folder, opts, &costs);
        let estimate = parts.values().fold(Estimate::default(), |acc, part| acc + part.clone());
        // Biggest first; a long first pass can be cut down by looking at the subfolders.
        let mut subfolders: Vec<(&String, &Estimate)> = parts.iter().filter(|(k, _)| k.as_str() != folder).collect();
        subfolders.sort_by(|a, b| b.1.seconds.total_cmp(&a.1.seconds).then_with(|| a.0.cmp(b.0)));
        total += estimate.seconds;

        let mut kinds: Vec<String> = Vec::new();
        for (kind, &count) in &estimate.files {
            if count == 0 {
                continue;
            }
            let plural = if count == 1 { "" } else { "s" };
            kinds.push(match kind {
                FileKind::Pdf => format!(
                    "{} PDFs ({} pages; {} in the first pass)",
                    number(count),
                    number(estimate.pdf_pages_total),
                    number(estimate.pdf_pages)
                ),
                FileKind::Audio => format!("{} audio ({})", number(count), duration(estimate.audio_seconds)),
                FileKind::Video => format!("{} video ({})", number(count), duration(estimate.video_seconds)),
                FileKind::Code => format!(
                    "{} code {} ({} stretches)",
                    number(count),
                    if count == 1 { "file" } else { "files" },
                    number(estimate.code_stretches)
                ),
                other => format!("{} {}{}", number(count), other.as_str(), plural),
            });
        }

        let mut head = format!("{}    first index ≈ {}", tilde(&folder), duration(estimate.seconds));
        if opts.include_code {
            head.push_str(&format!(", index ≈ {}", byte_count(estimate.bytes)));
        }
        println!("{head}");
        println!("  {}", if kinds.is_empty() { "nothing to index".to_string() } else { kinds.join(" · ") });
        if estimate.long_files > 0 {
            let noun = if estimate.long_files == 1 { "file" } else { "files" };
            println!(
                "  then the rest of {} long {} ≈ {}",
                number(estimate.long_files),
                noun,
                duration(estimate.later_seconds)
            );
        }
        if !subfolders.is_empty() {
            let shown: Vec<String> = subfolders
                .iter()
                .take(5)
                .map(|(key, part)| {
                    let name = Path::new(key.as_str()).file_name().map(|n| n.to_string_lossy().into_owned());
                    format!("{} ≈ {}", name.unwrap_or_else(|| key.to_string()), duration(part.seconds))
                })
                .collect();
            let more = if subfolders.len() > 5 { format!(" · {} more", subfolders.len() - 5) } else { String::new() };
            println!("  biggest subfolders: {}{}", shown.join(" · "), more);
        }
        if !report.skipped.is_empty() {
            println!("  skipped: {}", skipped_line(&report.skipped));
        }
        if !report.skipped_folders.is_empty() {
            println!("  folders not walked: {}", skipped_line(&report.skipped_folders));
        }
        let mut datasets: Vec<(&String, &usize)> = report.datasets.iter().collect();
        datasets.sort_by(|a, b| b.1.cmp(a.1).then_with(|| a.0.cmp(b.0)));
        for (dataset, count) in datasets {
            println!("  looks like a dataset (skipped): {} ({} files)", tilde(dataset), number(*count));
        }
        println!("  (looked at it in {:.2} s)", started.elapsed().as_secs_f64());
    }
    if args.folders.len() > 1 {
        println!("all together ≈ {}", duration(total));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cand(path: &str, kind: FileKind, size: i64) -> Candidate {
        Candidate { path: path.to_string(), kind, size, mtime: 0.0, inode: 0, device: 0 }
    }

    fn close(a: f64, b: f64) -> bool {
        (a - b).abs() < 1e-9
    }

    #[test]
    fn estimate_docs_by_size() {
        // 10_000 B -> 5 chunks (all, under docStep 40); 200_000 B -> 100 chunks, 40 first + 60 later;
        // 0 B -> max(1, 0) = 1 chunk.
        let files = [
            cand("/x/a.txt", FileKind::Doc, 10_000),
            cand("/x/b.doc", FileKind::Doc, 200_000),
            cand("/x/c.txt", FileKind::Doc, 0),
        ];
        let e = estimate_candidates(&files, &IndexOptions::default());
        assert_eq!(e.files.get(&FileKind::Doc), Some(&3));
        assert_eq!(e.doc_chunks, 5 + 40 + 1);
        assert!(close(e.seconds, 46.0 * 0.095));
        assert_eq!(e.long_files, 1);
        assert!(close(e.later_seconds, 60.0 * 0.095));
        assert_eq!(e.bytes, 0.0);
    }

    #[test]
    fn estimate_code_formula_and_notebooks() {
        // 1400 chars -> 1 stretch; 1401 -> 2; notebook 8000 B -> 1000 chars -> 1; empty -> max(1, 0) = 1.
        let files = [
            cand("/x/a.rs", FileKind::Code, 1400),
            cand("/x/b.rs", FileKind::Code, 1401),
            cand("/x/N.IPYNB", FileKind::Code, 8000),
            cand("/x/e.rs", FileKind::Code, 0),
        ];
        let e = estimate_candidates(&files, &IndexOptions::default());
        assert_eq!(e.code_stretches, 5);
        assert!(close(e.seconds, 5.0 * 0.037));
        // 3801 characters * 2.4 + 5 stretches * 1600
        assert!(close(e.bytes, 3801.0 * 2.4 + 5.0 * 1600.0));
    }

    #[test]
    fn estimate_image_and_screenshot_constants() {
        let files = [
            cand("/x/1.jpg", FileKind::Image, 1),
            cand("/x/2.jpg", FileKind::Image, 1),
            cand("/x/3.png", FileKind::Image, 1),
            cand("/x/s.png", FileKind::Screenshot, 1),
            cand("/x/t.png", FileKind::Screenshot, 1),
        ];
        let e = estimate_candidates(&files, &IndexOptions::default());
        assert_eq!(e.files.get(&FileKind::Image), Some(&3));
        assert_eq!(e.files.get(&FileKind::Screenshot), Some(&2));
        assert!(close(e.seconds, 3.0 * 0.12 + 2.0 * 0.32));
    }

    #[test]
    fn estimate_pdf_missing_file_counts_one_page() {
        // Unreadable PDF: 1 page (fallback), 1 page in the first pass, no later part.
        let files = [cand("/nonexistent-dtfu-test/x.pdf", FileKind::Pdf, 0)];
        let e = estimate_candidates(&files, &IndexOptions::default());
        assert_eq!(e.pdf_pages_total, 1);
        assert_eq!(e.pdf_pages, 1);
        assert_eq!(e.long_files, 0);
        assert!(close(e.seconds, 0.09));
    }

    #[test]
    fn estimate_media_missing_file_is_free() {
        let files = [cand("/nonexistent-dtfu-test/a.mp3", FileKind::Audio, 1)];
        let e = estimate_candidates(&files, &IndexOptions::default());
        assert_eq!(e.audio_seconds, 0.0);
        assert_eq!(e.seconds, 0.0);
    }

    #[test]
    fn subfolder_grouping_and_sum() {
        let files = [
            cand("/nonexistent-dtfu-test/root/a/f.txt", FileKind::Doc, 10_000), // 5 chunks in "a"
            cand("/nonexistent-dtfu-test/root/a/b/g.rs", FileKind::Code, 1400), // 1 stretch in "a"
            cand("/nonexistent-dtfu-test/root/top.txt", FileKind::Doc, 0),      // 1 chunk under the folder itself
        ];
        let folder = "/nonexistent-dtfu-test/root";
        let parts = estimates_by_subfolder(&files, folder, &IndexOptions::default(), &Costs::default());
        assert_eq!(parts.len(), 2);
        let sub = &parts[&format!("{folder}/a")];
        assert_eq!(sub.doc_chunks, 5);
        assert_eq!(sub.code_stretches, 1);
        let top = &parts[folder];
        assert_eq!(top.doc_chunks, 1);
        let sum = parts.values().fold(Estimate::default(), |acc, p| acc + p.clone());
        let whole = estimate_candidates(&files, &IndexOptions::default());
        assert!(close(sum.seconds, whole.seconds));
        assert!(close(sum.seconds, 6.0 * 0.095 + 0.037));
    }

    #[test]
    fn formatting_helpers() {
        assert_eq!(number(0), "0");
        assert_eq!(number(365), "365");
        assert_eq!(number(1245), "1,245");
        assert_eq!(number(1_234_567), "1,234,567");
        assert_eq!(duration(0.4), "0 s");
        assert_eq!(duration(89.4), "89 s");
        assert_eq!(duration(90.0), "2 min");
        assert_eq!(duration(5399.0), "90 min");
        assert_eq!(duration(5400.0), "1.5 h");
        assert_eq!(byte_count(6_300_000.0), "6.3 MB");
        assert_eq!(byte_count(256_000.0), "256 KB");
        assert_eq!(byte_count(1_000_000.0), "1 MB");
        assert_eq!(byte_count(500.0), "500 bytes");
    }
}
