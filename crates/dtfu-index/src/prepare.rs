//! Prepare stage: decode / extract ONE job into a `Payload`. No embedding, no database writes.
//! Port of Swift `Indexer.prepare` (Indexer.swift:358-440, docs/porting/spec-pipeline.md section 2.5).

use crate::options::IndexOptions;
use crate::types::{Job, Leave, Ocr, Payload, PdfPage, Prepared, VideoFrame};
use dtfu_core::FileKind;
use dtfu_extract_audio::{AudioStream, WindowConfig};
use dtfu_extract_image::{ExtractError, ThumbFormat};
use dtfu_extract_text::code::{self, CodeOptions, CodeOutcome};
use dtfu_extract_text::docs::{self, DocError};
use std::path::Path;

/// Prepare one job. Per-file errors become `Leave`, never a panic.
/// blake3 of a file's bytes as lowercase hex, read in 1 MiB chunks so large files never sit in memory.
pub fn content_hash(path: &Path) -> std::io::Result<String> {
    use std::io::Read;
    let mut file = std::fs::File::open(path)?;
    let mut hasher = blake3::Hasher::new();
    let mut buf = vec![0u8; 1 << 20];
    loop {
        let n = file.read(&mut buf)?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    Ok(hasher.finalize().to_hex().to_string())
}

pub fn prepare(job: &Job, opts: &IndexOptions, ocr: &dyn Ocr) -> Prepared {
    let path = Path::new(&job.file.path);
    let result = match job.file.kind {
        FileKind::Image | FileKind::Screenshot => prepare_image(job.file.kind, path, ocr),
        FileKind::Doc => prepare_doc(job, opts, path),
        FileKind::Audio => prepare_audio(opts, path),
        FileKind::Code => prepare_code(opts, path),
        FileKind::Pdf => prepare_pdf(job, opts, path, ocr),
        FileKind::Video => prepare_video(opts, path),
    };
    let mut job = job.clone();
    // The hash was taken when the run was planned, possibly long before this read. If the file changed in between (or
    // while it was read), the rows are built from other bytes than the hash names, so the hash must not be stored:
    // a later byte-identical copy would otherwise be handed the wrong rows.
    if job.content_hash.is_some() && !unchanged_since_crawl(path, &job.file) {
        job.content_hash = None;
    }
    Prepared { job, result }
}

/// An error while reading `path`. When the file's parent folder can no longer be stat'ed the volume vanished (drive
/// unplugged, share offline): `Unavailable`, so the row stays pending. Otherwise the file itself is at fault: `Failed`.
fn failed_or_unavailable(path: &Path, msg: String) -> Leave {
    match path.parent() {
        Some(parent) if !parent.as_os_str().is_empty() && std::fs::metadata(parent).is_err() => Leave::Unavailable(msg),
        _ => Leave::Failed(msg),
    }
}

/// Whether `path` still has the size and modification time that the crawl recorded.
fn unchanged_since_crawl(path: &Path, file: &dtfu_store::index::FileRecord) -> bool {
    let Ok(meta) = std::fs::metadata(path) else { return false };
    let mtime = meta
        .modified()
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map_or(f64::NAN, |d| d.as_secs_f64());
    meta.len() as i64 == file.size && (mtime - file.mtime).abs() <= 0.001
}

/// The thumbnail is decoded back to RGB8 and embedded (Swift embeds its scratch thumbnail file). Simplification:
/// Swift runs screenshot OCR on a second, full-resolution decode; here OCR sees the thumbnail pixels (max side 1536).
fn prepare_image(kind: FileKind, path: &Path, ocr: &dyn Ocr) -> Result<Payload, Leave> {
    let p = dtfu_extract_image::process_image(path).map_err(|e| match e {
        ExtractError::Skipped(reason) => Leave::Skipped(reason.to_string()),
        other => failed_or_unavailable(path, other.to_string()),
    })?;
    let fmt = match p.thumbnail.format {
        ThumbFormat::Png => image::ImageFormat::Png,
        _ => image::ImageFormat::Jpeg,
    };
    let rgb_img = image::load_from_memory_with_format(&p.thumbnail.bytes, fmt)
        .map_err(|e| failed_or_unavailable(path, format!("thumbnail decode: {e}")))?
        .into_rgb8();
    let (w, h) = (rgb_img.width(), rgb_img.height());
    let rgb = rgb_img.into_raw();
    let ocr_text = if kind == FileKind::Screenshot { ocr.recognize(w as usize, h as usize, &rgb) } else { None };
    Ok(Payload::Image {
        kind,
        width: w,
        height: h,
        rgb,
        orig_width: p.width,
        orig_height: p.height,
        taken: p.taken,
        ocr: ocr_text,
    })
}

fn prepare_doc(job: &Job, opts: &IndexOptions, path: &Path) -> Result<Payload, Leave> {
    let text = docs::extract_text(path).map_err(|e| match e {
        DocError::Unsupported(_) => Leave::Skipped("format not supported".into()),
        other => failed_or_unavailable(path, other.to_string()),
    })?;
    // Counted up to the production cap + 1 (docs::MAX_CHUNKS), which equals Swift's maxChunks + 1 at defaults.
    let total = docs::total_chunks(&text).min(opts.max_chunks + 1);
    if total == 0 {
        return Err(Leave::Skipped("no text".into()));
    }
    let cap = total.min(opts.max_chunks);
    let wanted = job.step.clone().unwrap_or(0..opts.doc_step);
    let end = wanted.end.min(cap);
    let start = wanted.start.min(end);
    let chunks = docs::chunks_range(&text, start, end);
    let title = path.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default();
    let chars = text.chars().count();
    Ok(Payload::Doc { title, total, range: start..end, chars, chunks })
}

/// `duration` is 0.0 here: the audio crate only knows the length after the windows were read (`Windows::total_secs`).
fn prepare_audio(opts: &IndexOptions, path: &Path) -> Result<Payload, Leave> {
    let cfg = WindowConfig { window_secs: opts.audio_window, hop_secs: opts.audio_hop, ..WindowConfig::default() };
    let stream = AudioStream::open(path).map_err(|e| failed_or_unavailable(path, e.to_string()))?;
    let windows = stream.windows_with(cfg).map(|r| r.map_err(anyhow::Error::from));
    Ok(Payload::Audio { duration: 0.0, windows: Box::new(windows) })
}

fn is_arabic(c: char) -> bool {
    matches!(c as u32, 0x0600..=0x06FF | 0x0750..=0x077F | 0x08A0..=0x08FF | 0xFB50..=0xFDFF | 0xFE70..=0xFEFF)
}

/// Port of `PDFExtractor.pages` (Extractors.swift:114-160): real text stays text; scans and slides are rendered;
/// garbled text layers are replaced by OCR (lost letters, Arabic) or kept beside the page picture (glyph order).
fn prepare_pdf(job: &Job, opts: &IndexOptions, path: &Path, ocr: &dyn Ocr) -> Result<Payload, Leave> {
    use dtfu_extract_pdf::{text_problem::page_text_problem, PdfDoc, PdfError};
    let doc = PdfDoc::open(path).map_err(|e| match e {
        PdfError::Locked => Leave::Skipped("password-protected".into()),
        other => failed_or_unavailable(path, other.to_string()),
    })?;
    let total = doc.page_count();
    let wanted = job
        .step
        .clone()
        .unwrap_or(if total > opts.big_pdf_pages { 0..opts.pdf_step } else { 0..opts.max_pages });
    let end = wanted.end.min(total.min(opts.max_pages));
    let range = wanted.start.min(end)..end;
    let mut pages = Vec::new();
    for index in range.clone() {
        let text = doc.text(index).unwrap_or_default().trim().to_string();
        let mut text = text;
        let mut garbled = None;
        match page_text_problem(&text).as_deref() {
            Some("lostLetters") => {
                if let Ok(r) = doc.render(index, 2000) {
                    let read = ocr.recognize(r.width as usize, r.height as usize, &r.rgb).unwrap_or_default();
                    let read = read.trim().to_string();
                    if read.chars().count() >= opts.min_page_text
                        && page_text_problem(&read).is_none()
                        && read.chars().any(is_arabic)
                    {
                        pages.push(PdfPage { number: index + 1, text: read, image: None, ocr: true, garbled: None });
                        continue;
                    }
                }
                if text.chars().count() >= opts.min_page_text {
                    pages.push(PdfPage { number: index + 1, text, image: None, ocr: false, garbled: None });
                    continue;
                }
            }
            Some("glyphOrder") => {
                garbled = Some(std::mem::take(&mut text));
            }
            _ => {
                if text.chars().count() >= opts.min_page_text {
                    pages.push(PdfPage { number: index + 1, text, image: None, ocr: false, garbled: None });
                    continue;
                }
            }
        }
        let Ok(r) = doc.render(index, 1400) else { continue };
        pages.push(PdfPage { number: index + 1, text, image: Some((r.width, r.height, r.rgb)), ocr: false, garbled });
    }
    Ok(Payload::Pdf { title: doc.title().or_else(|| path.file_stem().map(|s| s.to_string_lossy().into_owned())), total, range, pages })
}

fn prepare_video(opts: &IndexOptions, path: &Path) -> Result<Payload, Leave> {
    use dtfu_extract_video::{frames, probe, VideoError};
    let info = probe(path).map_err(|e| failed_or_unavailable(path, match e {
        VideoError::Missing => "ffmpeg not found".to_string(),
        VideoError::Unreadable(m) => m,
    }))?;
    // Swift keeps going with the sound when no frame can be read.
    let frames = match frames(path, opts.frame_interval, 1024) {
        Ok(f) => f,
        Err(_) if info.has_audio => Vec::new(),
        Err(e) => {
            return Err(failed_or_unavailable(path, match e {
                VideoError::Missing => "ffmpeg not found".to_string(),
                VideoError::Unreadable(m) => m,
            }))
        }
    };
    let mut kept: Vec<VideoFrame> = frames
        .into_iter()
        .map(|f| VideoFrame { start: f.time_secs, end: info.duration_secs, width: f.width, height: f.height, rgb: f.rgb })
        .collect();
    // Each kept frame stands for the stretch until the next kept frame.
    for i in 1..kept.len() {
        kept[i - 1].end = kept[i].start;
    }
    let sound: Option<Box<dyn Iterator<Item = anyhow::Result<dtfu_extract_audio::AudioWindow>> + Send>> =
        if info.has_audio {
            match AudioStream::open(path) {
                Ok(stream) => {
                    let cfg = WindowConfig { window_secs: opts.audio_window, hop_secs: opts.audio_hop, ..WindowConfig::default() };
                    Some(Box::new(stream.windows_with(cfg).map(|w| w.map_err(anyhow::Error::from))))
                }
                Err(_) => None,
            }
        } else {
            None
        };
    Ok(Payload::Video { duration: info.duration_secs, frames: kept, sound })
}

fn prepare_code(opts: &IndexOptions, path: &Path) -> Result<Payload, Leave> {
    let co = CodeOptions {
        stretch_size: opts.code_stretch,
        max_code_bytes: opts.max_code_bytes,
        max_notebook_bytes: opts.max_notebook_bytes,
    };
    let f = code::read_file(path, &co).map_err(|e| failed_or_unavailable(path, e.to_string()))?;
    match f.outcome {
        CodeOutcome::Skipped(reason) => Err(Leave::Skipped(reason.to_string())),
        CodeOutcome::Stretches(stretches) => {
            Ok(Payload::Code { title: f.title, repo: f.repo, lines: f.lines, cells: f.cells, stretches })
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::NoOcr;
    use dtfu_core::FileState;
    use dtfu_store::index::FileRecord;
    use std::path::PathBuf;

    fn tmp(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("dtfu-prepare-{}-{name}", std::process::id()));
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    fn job(path: &Path, kind: FileKind, step: Option<std::ops::Range<usize>>) -> Job {
        Job {
            content_hash: None,
            file: FileRecord {
                id: 1,
                path: path.to_string_lossy().into_owned(),
                kind,
                size: 0,
                mtime: 0.0,
                inode: 0,
                device: 0,
                state: FileState::Pending,
            },
            step,
        }
    }

    struct Hello;
    impl Ocr for Hello {
        fn recognize(&self, _: usize, _: usize, _: &[u8]) -> Option<String> {
            Some("hello".into())
        }
    }

    fn gradient(w: u32, h: u32) -> image::RgbImage {
        image::RgbImage::from_fn(w, h, |x, y| image::Rgb([(x % 256) as u8, (y % 256) as u8, 128]))
    }

    #[test]
    fn jpeg_photo() {
        let d = tmp("jpeg");
        let p = d.join("a.jpg");
        gradient(600, 400).save(&p).unwrap();
        let r = prepare(&job(&p, FileKind::Image, None), &IndexOptions::default(), &NoOcr);
        match r.result {
            Ok(Payload::Image { width, height, rgb, orig_width, orig_height, ocr, .. }) => {
                assert_eq!((orig_width, orig_height), (600, 400));
                assert_eq!(rgb.len(), (width * height * 3) as usize);
                assert!(ocr.is_none());
            }
            _ => panic!("expected image payload"),
        }
    }

    #[test]
    fn small_png_skipped() {
        let d = tmp("small");
        let p = d.join("s.png");
        gradient(128, 128).save(&p).unwrap();
        let r = prepare(&job(&p, FileKind::Image, None), &IndexOptions::default(), &NoOcr);
        assert_eq!(r.result.err(), Some(Leave::Skipped("small image 128x128".into())));
    }

    #[test]
    fn screenshot_gets_ocr() {
        let d = tmp("shot");
        let p = d.join("Screenshot 2024.png");
        gradient(800, 500).save(&p).unwrap();
        let r = prepare(&job(&p, FileKind::Screenshot, None), &IndexOptions::default(), &Hello);
        match r.result {
            Ok(Payload::Image { kind, ocr, .. }) => {
                assert_eq!(kind, FileKind::Screenshot);
                assert_eq!(ocr.as_deref(), Some("hello"));
            }
            _ => panic!("expected image payload"),
        }
    }

    #[test]
    fn text_doc_steps() {
        let d = tmp("doc");
        let p = d.join("notes.txt");
        let para = format!("{}\n\n", "word ".repeat(400));
        std::fs::write(&p, para.repeat(100)).unwrap();
        let o = IndexOptions::default();
        let first = prepare(&job(&p, FileKind::Doc, None), &o, &NoOcr);
        let total = match first.result {
            Ok(Payload::Doc { title, total, range, chunks, chars, .. }) => {
                assert_eq!(title, "notes");
                assert!(total > 80, "total {total}");
                assert_eq!(range, 0..40);
                assert_eq!(chunks.len(), 40);
                assert!(chars > 0);
                total
            }
            _ => panic!("expected doc payload"),
        };
        let later = prepare(&job(&p, FileKind::Doc, Some(40..80)), &o, &NoOcr);
        match later.result {
            Ok(Payload::Doc { range, chunks, total: t2, .. }) => {
                assert_eq!(range, 40..80);
                assert_eq!(chunks[0].index, 40);
                assert_eq!(t2, total);
            }
            _ => panic!("expected doc payload"),
        }
    }

    #[test]
    fn empty_doc_and_unsupported() {
        let d = tmp("emptydoc");
        let p = d.join("e.txt");
        std::fs::write(&p, "   \n").unwrap();
        let r = prepare(&job(&p, FileKind::Doc, None), &IndexOptions::default(), &NoOcr);
        assert_eq!(r.result.err(), Some(Leave::Skipped("no text".into())));
        let q = d.join("old.doc");
        std::fs::write(&q, "x").unwrap();
        let r = prepare(&job(&q, FileKind::Doc, None), &IndexOptions::default(), &NoOcr);
        assert_eq!(r.result.err(), Some(Leave::Skipped("format not supported".into())));
    }

    #[test]
    fn wav_windows() {
        let d = tmp("wav");
        let p = d.join("t.wav");
        let rate = 16000u32;
        let n = rate * 5;
        let mut b = Vec::new();
        b.extend_from_slice(b"RIFF");
        b.extend_from_slice(&(36 + n * 2).to_le_bytes());
        b.extend_from_slice(b"WAVEfmt ");
        b.extend_from_slice(&16u32.to_le_bytes());
        b.extend_from_slice(&1u16.to_le_bytes());
        b.extend_from_slice(&1u16.to_le_bytes());
        b.extend_from_slice(&rate.to_le_bytes());
        b.extend_from_slice(&(rate * 2).to_le_bytes());
        b.extend_from_slice(&2u16.to_le_bytes());
        b.extend_from_slice(&16u16.to_le_bytes());
        b.extend_from_slice(b"data");
        b.extend_from_slice(&(n * 2).to_le_bytes());
        for i in 0..n {
            let s = ((i as f32 * 440.0 * std::f32::consts::TAU / rate as f32).sin() * 12000.0) as i16;
            b.extend_from_slice(&s.to_le_bytes());
        }
        std::fs::write(&p, b).unwrap();
        let r = prepare(&job(&p, FileKind::Audio, None), &IndexOptions::default(), &NoOcr);
        match r.result {
            Ok(Payload::Audio { windows, .. }) => {
                let w: Vec<_> = windows.collect::<Result<_, _>>().unwrap();
                assert_eq!(w.len(), 1);
                assert!((w[0].end_secs() - 5.0).abs() < 0.1);
            }
            _ => panic!("expected audio payload"),
        }
    }

    #[test]
    fn rust_code() {
        let d = tmp("code");
        let p = d.join("main.rs");
        std::fs::write(&p, "fn main() {\n    println!(\"hi\");\n}\n").unwrap();
        let r = prepare(&job(&p, FileKind::Code, None), &IndexOptions::default(), &NoOcr);
        match r.result {
            Ok(Payload::Code { lines, stretches, .. }) => {
                assert_eq!(lines, 3);
                assert_eq!(stretches.len(), 1);
            }
            _ => panic!("expected code payload"),
        }
    }

    #[test]
    fn missing_and_corrupt_fail() {
        let d = tmp("bad");
        let o = IndexOptions::default();
        let missing = d.join("nope.jpg");
        assert!(matches!(prepare(&job(&missing, FileKind::Image, None), &o, &NoOcr).result, Err(Leave::Failed(_))));
        let corrupt = d.join("c.jpg");
        std::fs::write(&corrupt, b"not an image at all").unwrap();
        assert!(matches!(prepare(&job(&corrupt, FileKind::Image, None), &o, &NoOcr).result, Err(Leave::Failed(_))));
        let wav = d.join("c.wav");
        std::fs::write(&wav, b"garbage").unwrap();
        assert!(matches!(prepare(&job(&wav, FileKind::Audio, None), &o, &NoOcr).result, Err(Leave::Failed(_))));
        assert!(matches!(prepare(&job(&missing, FileKind::Code, None), &o, &NoOcr).result, Err(Leave::Failed(_))));
        assert!(matches!(prepare(&job(&missing, FileKind::Pdf, None), &o, &NoOcr).result, Err(Leave::Failed(_))));
    }

    #[test]
    fn file_in_a_vanished_folder_is_unavailable_not_failed() {
        let d = tmp("vanished");
        let vol = d.join("usb");
        std::fs::create_dir_all(&vol).unwrap();
        let p = vol.join("photo.jpg");
        gradient(600, 400).save(&p).unwrap();
        let t = vol.join("note.txt");
        std::fs::write(&t, "A note that lives on the drive.").unwrap();
        std::fs::remove_dir_all(&vol).unwrap();
        let o = IndexOptions::default();
        assert!(matches!(prepare(&job(&p, FileKind::Image, None), &o, &NoOcr).result, Err(Leave::Unavailable(_))));
        assert!(matches!(prepare(&job(&t, FileKind::Doc, None), &o, &NoOcr).result, Err(Leave::Unavailable(_))));
        // A missing file whose folder still exists stays failed.
        assert!(matches!(prepare(&job(&d.join("nope.jpg"), FileKind::Image, None), &o, &NoOcr).result, Err(Leave::Failed(_))));
    }

    #[test]
    fn content_hash_is_dropped_when_the_file_changed_since_the_crawl() {
        let dir = tmp("stale-hash");
        let path = dir.join("a.txt");
        std::fs::write(&path, "The first version of the note.").unwrap();
        let meta = std::fs::metadata(&path).unwrap();
        let mtime = meta.modified().unwrap().duration_since(std::time::UNIX_EPOCH).unwrap().as_secs_f64();
        let mut j = job(&path, FileKind::Doc, None);
        j.file.size = meta.len() as i64;
        j.file.mtime = mtime;
        j.content_hash = Some("abc".into());
        let opts = IndexOptions::default();
        assert_eq!(prepare(&j, &opts, &NoOcr).job.content_hash.as_deref(), Some("abc"), "unchanged: kept");
        // Same length, different bytes, later mtime: the hash no longer names the bytes that get read.
        std::fs::write(&path, "The other version of the note.").unwrap();
        let later = std::time::SystemTime::now() + std::time::Duration::from_secs(5);
        std::fs::File::options().write(true).open(&path).unwrap().set_modified(later).unwrap();
        assert_eq!(prepare(&j, &opts, &NoOcr).job.content_hash, None, "changed since the crawl: dropped");
    }
}
