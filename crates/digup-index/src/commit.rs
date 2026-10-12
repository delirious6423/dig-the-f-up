//! Commit stage: embed a prepared job and write its segments (docs/spec-pipeline.md section 2.5).
//! Port of Swift `Indexer.commit` and its `images`/`doc`/`audio`/`code` helpers.

use crate::options::IndexOptions;
use crate::types::{Embed, Leave, Payload, PdfPage, Prepared};
use anyhow::{bail, Result};
use digup_core::{doc_prompt, FileKind, FileState, Modality, SegmentKind, DIM};
use digup_extract_text::code::summary;
use digup_extract_text::docs::has_words;
use digup_store::index::{IndexStore, NewSegment};
use std::collections::BTreeMap;

/// One line of at most `limit` characters, whitespace collapsed, `…` when cut (Swift `excerpt`).
fn excerpt(text: &str, limit: usize) -> String {
    let line = text.split_whitespace().collect::<Vec<_>>().join(" ");
    if line.chars().count() > limit {
        let mut cut: String = line.chars().take(limit).collect();
        cut.push('\u{2026}');
        cut
    } else {
        line
    }
}

fn check(vector: Vec<f32>) -> Result<Vec<f32>> {
    if vector.len() != DIM {
        bail!("embedder returned {} values, expected {DIM}", vector.len());
    }
    Ok(vector)
}

/// What a payload turned into: segments to add and the final state of the file.
struct Outcome {
    segments: Vec<NewSegment>,
    state: FileState,
    detail: Option<String>,
    info: BTreeMap<String, String>,
}

fn skipped(why: &str) -> Outcome {
    Outcome { segments: Vec::new(), state: FileState::Skipped, detail: Some(why.into()), info: BTreeMap::new() }
}

/// Embeds `prepared` and writes its segments and final state. `Err` only for store failures; a file that cannot be
/// embedded or has nothing to index is recorded as failed or skipped and returns `Ok`.
pub fn commit(
    store: &mut IndexStore,
    embedder: &mut dyn Embed,
    prepared: Prepared,
    opts: &IndexOptions,
) -> Result<()> {
    let id = prepared.job.file.id;
    let outcome = match prepared.result {
        Err(Leave::Skipped(why)) => skipped(&why),
        Err(Leave::Failed(e)) => {
            Outcome { segments: Vec::new(), state: FileState::Failed, detail: Some(e), info: BTreeMap::new() }
        }
        Ok(payload) => match build(embedder, payload, opts) {
            Ok(o) => o,
            Err(e) => Outcome {
                segments: Vec::new(),
                state: FileState::Failed,
                detail: Some(format!("{e:#}")),
                info: BTreeMap::new(),
            },
        },
    };
    let mut outcome = outcome;
    if prepared.job.step.is_some() && outcome.info.contains_key("scanned_pages") {
        let before = store.info(id)?;
        let prev = before.get("scanned_pages").and_then(|v| v.parse::<usize>().ok()).unwrap_or(0);
        let now = outcome.info["scanned_pages"].parse::<usize>().unwrap_or(0);
        outcome.info.insert("scanned_pages".into(), (prev + now).to_string());
    }
    // A later step that cannot read on keeps what earlier steps stored (Swift `stopReading`).
    if let (Some(step), false) = (&prepared.job.step, matches!(outcome.state, FileState::Done | FileState::Partial)) {
        let unit = if prepared.job.file.kind == FileKind::Pdf { "page" } else { "chunk" };
        let mut info = store.info(id)?;
        info.remove("read_to");
        info.remove("read_end");
        let problem = outcome.detail.take().unwrap_or_default();
        outcome = Outcome {
            segments: Vec::new(),
            state: FileState::Done,
            detail: Some(format!("read to {unit} {}, not after: {problem}", step.start)),
            info,
        };
    }
    // The content hash names the bytes this file's rows were built from; copies of a done file reuse it (see run.rs).
    // Later steps of a long file carry the hash of the first step forward.
    if matches!(outcome.state, FileState::Done | FileState::Partial) {
        match &prepared.job.content_hash {
            Some(hash) => {
                outcome.info.insert(CONTENT_HASH.into(), hash.clone());
                outcome.info.insert(CONTENT_SIZE.into(), prepared.job.file.size.to_string());
            }
            None if prepared.job.step.is_some() => {
                let before = store.info(id)?;
                for key in [CONTENT_HASH, CONTENT_SIZE] {
                    if let Some(v) = before.get(key) {
                        outcome.info.entry(key.into()).or_insert_with(|| v.clone());
                    }
                }
            }
            None => {}
        }
    }
    store.add_segments_and_finish(id, &outcome.segments, outcome.state, outcome.detail.as_deref(), &outcome.info)?;
    Ok(())
}

/// `files.info` keys written by commit and read by run's deduplication.
pub const CONTENT_HASH: &str = "content_hash";
pub const CONTENT_SIZE: &str = "content_size";

fn build(embedder: &mut dyn Embed, payload: Payload, opts: &IndexOptions) -> Result<Outcome> {
    match payload {
        Payload::Image { kind, width, height, rgb, orig_width, orig_height, taken, ocr } => {
            let screenshot = kind == FileKind::Screenshot;
            let budget = if screenshot { opts.screenshot_budget } else { opts.photo_budget };
            let vector = check(embedder.embed_image_rgb(width as usize, height as usize, &rgb, budget)?)?;
            let mut segments = vec![NewSegment {
                kind: SegmentKind::Image,
                modality: Modality::Image,
                loc: None,
                loc_end: None,
                excerpt: None,
                text: None,
                vector: Some(vector),
            }];
            let ocr = ocr.unwrap_or_default();
            // Words on screen are for keyword search only; meaning comes from the picture (Swift Indexer.swift:509).
            if !ocr.trim().is_empty() {
                segments.push(NewSegment {
                    kind: SegmentKind::Ocr,
                    modality: Modality::Text,
                    loc: None,
                    loc_end: None,
                    excerpt: Some(excerpt(&ocr, 240)),
                    text: Some(ocr.clone()),
                    vector: None,
                });
            }
            let mut info = BTreeMap::new();
            info.insert("width".to_string(), orig_width.to_string());
            info.insert("height".to_string(), orig_height.to_string());
            if let Some(t) = taken {
                info.insert("taken".to_string(), t);
            }
            if screenshot {
                info.insert("ocr_chars".to_string(), ocr.chars().count().to_string());
            }
            let detail = screenshot.then(|| format!("ocr {} chars", ocr.chars().count()));
            Ok(Outcome { segments, state: FileState::Done, detail, info })
        }
        Payload::Doc { title, total, range, chars, chunks } => {
            if total == 0 {
                return Ok(skipped("no text"));
            }
            let wordy: Vec<usize> = (0..chunks.len()).filter(|&i| has_words(&chunks[i].text)).collect();
            let mut vectors: Vec<Option<Vec<f32>>> = vec![None; chunks.len()];
            if !wordy.is_empty() {
                let texts: Vec<String> =
                    wordy.iter().map(|&i| doc_prompt(Some(&title), &chunks[i].text)).collect();
                let out = embedder.embed_texts(&texts)?;
                if out.len() != texts.len() {
                    bail!("embedder returned {} vectors for {} texts", out.len(), texts.len());
                }
                for (&i, v) in wordy.iter().zip(out) {
                    vectors[i] = Some(check(v)?);
                }
            }
            let segments = chunks
                .iter()
                .zip(vectors)
                .map(|(c, v)| NewSegment {
                    kind: SegmentKind::Chunk,
                    modality: Modality::Text,
                    loc: Some((c.index + 1) as f64),
                    loc_end: None,
                    excerpt: Some(excerpt(&c.text, 240)),
                    text: Some(c.text.clone()),
                    vector: v,
                })
                .collect();
            let end = total.min(opts.max_chunks);
            let unread = range.end < end;
            let mut info = BTreeMap::new();
            info.insert("chunks".to_string(), end.to_string());
            info.insert("chars".to_string(), chars.to_string());
            if unread {
                info.insert("read_to".to_string(), range.end.to_string());
                info.insert("read_end".to_string(), end.to_string());
            }
            let detail = if unread {
                Some(format!("read to chunk {} of {end}", range.end))
            } else if end < total {
                Some(format!("the first {end} chunks"))
            } else {
                None
            };
            Ok(Outcome {
                segments,
                state: if unread { FileState::Partial } else { FileState::Done },
                detail,
                info,
            })
        }
        Payload::Audio { duration, windows } => {
            let mut segments = Vec::new();
            for window in windows {
                let window = window?;
                let vector = check(embedder.embed_audio_samples(&window.samples)?)?;
                segments.push(NewSegment {
                    kind: SegmentKind::Audio,
                    modality: Modality::Audio,
                    loc: Some(window.start_secs),
                    loc_end: Some(window.end_secs()),
                    excerpt: None,
                    text: None,
                    vector: Some(vector),
                });
            }
            if segments.is_empty() {
                return Ok(skipped("silent or shorter than 2 s"));
            }
            let mut info = BTreeMap::new();
            let duration = segments.last().and_then(|s: &NewSegment| s.loc_end).unwrap_or(duration);
            info.insert("duration".to_string(), format!("{duration:.1}"));
            Ok(Outcome { segments, state: FileState::Done, detail: None, info })
        }
        Payload::Pdf { title, total, range, pages } => {
            let end = total.min(opts.max_pages);
            let unread = range.end < end;
            let later = range.start > 0;
            if pages.is_empty() && !later && !unread {
                return Ok(skipped("no pages"));
            }
            let title = title.unwrap_or_default();
            let (text_pages, image_pages): (Vec<&PdfPage>, Vec<&PdfPage>) = pages.iter().partition(|p| p.image.is_none());
            let word_pages: Vec<&PdfPage> = text_pages.iter().copied().filter(|p| has_words(&p.text)).collect();
            let garbled_pages: Vec<&PdfPage> = image_pages.iter().copied().filter(|p| p.garbled.is_some()).collect();
            let prefix = |t: &str| t.chars().take(4000).collect::<String>();
            let mut text_vectors: std::collections::HashMap<usize, Vec<f32>> = Default::default();
            let mut garbled_vectors: Vec<Vec<f32>> = Vec::new();
            let mut image_vectors: Vec<Vec<f32>> = Vec::new();
            if !word_pages.is_empty() {
                let texts: Vec<String> = word_pages.iter().map(|p| doc_prompt(Some(&title), &prefix(&p.text))).collect();
                let out = embedder.embed_texts(&texts)?;
                if out.len() != texts.len() {
                    bail!("embedder returned {} vectors for {} texts", out.len(), texts.len());
                }
                for (p, v) in word_pages.iter().zip(out) {
                    text_vectors.insert(p.number, check(v)?);
                }
            }
            for p in &image_pages {
                let (w, h, rgb) = p.image.as_ref().expect("image page");
                image_vectors.push(check(embedder.embed_image_rgb(*w as usize, *h as usize, rgb, opts.page_budget)?)?);
            }
            if !garbled_pages.is_empty() {
                let texts: Vec<String> =
                    garbled_pages.iter().map(|p| doc_prompt(Some(&title), &prefix(p.garbled.as_deref().unwrap_or("")))).collect();
                let out = embedder.embed_texts(&texts)?;
                if out.len() != texts.len() {
                    bail!("embedder returned {} vectors for {} texts", out.len(), texts.len());
                }
                for v in out {
                    garbled_vectors.push(check(v)?);
                }
            }
            let mut segments = Vec::new();
            // Meaning only: the garbled words are never shown or matched against what you type.
            for (p, v) in garbled_pages.iter().zip(garbled_vectors) {
                segments.push(NewSegment {
                    kind: SegmentKind::Page,
                    modality: Modality::Text,
                    loc: Some(p.number as f64),
                    loc_end: None,
                    excerpt: None,
                    text: None,
                    vector: Some(v),
                });
            }
            for p in &text_pages {
                segments.push(NewSegment {
                    kind: SegmentKind::Page,
                    modality: Modality::Text,
                    loc: Some(p.number as f64),
                    loc_end: None,
                    excerpt: Some(excerpt(&p.text, 240)),
                    text: Some(p.text.clone()),
                    vector: text_vectors.remove(&p.number),
                });
            }
            for (p, v) in image_pages.iter().zip(image_vectors) {
                segments.push(NewSegment {
                    kind: SegmentKind::PageImage,
                    modality: Modality::Image,
                    loc: Some(p.number as f64),
                    loc_end: None,
                    excerpt: (!p.text.is_empty()).then(|| excerpt(&p.text, 240)),
                    text: Some(p.text.clone()),
                    vector: Some(v),
                });
            }
            let mut info = BTreeMap::new();
            info.insert("pages".to_string(), total.to_string());
            // `commit` adds the scanned pages of earlier steps.
            info.insert("scanned_pages".to_string(), image_pages.len().to_string());
            if unread {
                info.insert("read_to".to_string(), range.end.to_string());
                info.insert("read_end".to_string(), end.to_string());
            }
            let detail = if unread {
                Some(format!("read to page {} of {}", range.end, total))
            } else if end < total {
                Some(format!("the first {end} of {total} pages"))
            } else {
                None
            };
            Ok(Outcome { segments, state: if unread { FileState::Partial } else { FileState::Done }, detail, info })
        }
        Payload::Video { duration, frames, sound } => {
            let mut segments = Vec::new();
            for f in &frames {
                let v = check(embedder.embed_image_rgb(f.width as usize, f.height as usize, &f.rgb, opts.frame_budget)?)?;
                segments.push(NewSegment {
                    kind: SegmentKind::Frame,
                    modality: Modality::Image,
                    loc: Some(f.start),
                    loc_end: Some(f.end),
                    excerpt: None,
                    text: None,
                    vector: Some(v),
                });
            }
            let mut sound_windows = 0usize;
            if let Some(windows) = sound {
                for window in windows {
                    let window = window?;
                    let vector = check(embedder.embed_audio_samples(&window.samples)?)?;
                    segments.push(NewSegment {
                        kind: SegmentKind::Audio,
                        modality: Modality::Audio,
                        loc: Some(window.start_secs),
                        loc_end: Some(window.end_secs()),
                        excerpt: None,
                        text: None,
                        vector: Some(vector),
                    });
                    sound_windows += 1;
                }
            }
            if segments.is_empty() {
                return Ok(skipped("no frames or sound"));
            }
            let mut info = BTreeMap::new();
            info.insert("duration".to_string(), format!("{duration:.1}"));
            info.insert("frames".to_string(), frames.len().to_string());
            info.insert("sound_windows".to_string(), sound_windows.to_string());
            Ok(Outcome { segments, state: FileState::Done, detail: None, info })
        }
        Payload::Code { title, repo, lines, cells, stretches } => {
            if stretches.is_empty() {
                return Ok(skipped("no code"));
            }
            let wordy: Vec<usize> = (0..stretches.len()).filter(|&i| has_words(&stretches[i].text)).collect();
            let mut vectors: Vec<Option<Vec<f32>>> = vec![None; stretches.len()];
            if !wordy.is_empty() {
                let texts: Vec<String> =
                    wordy.iter().map(|&i| doc_prompt(Some(&title), &stretches[i].text)).collect();
                let out = embedder.embed_texts(&texts)?;
                if out.len() != texts.len() {
                    bail!("embedder returned {} vectors for {} texts", out.len(), texts.len());
                }
                for (&i, v) in wordy.iter().zip(out) {
                    vectors[i] = Some(check(v)?);
                }
            }
            let kind = if cells.is_some() { SegmentKind::Cells } else { SegmentKind::Lines };
            let segments = stretches
                .iter()
                .zip(vectors)
                .map(|(s, v)| NewSegment {
                    kind,
                    modality: Modality::Text,
                    loc: Some(s.start_line as f64),
                    loc_end: Some(s.end_line as f64),
                    excerpt: Some(summary(&s.text)),
                    text: Some(s.text.clone()),
                    vector: v,
                })
                .collect();
            let mut info = BTreeMap::new();
            info.insert("lines".to_string(), lines.to_string());
            if let Some(c) = cells {
                info.insert("cells".to_string(), c.to_string());
            }
            if let Some(r) = repo {
                info.insert("repo".to_string(), r.to_string_lossy().into_owned());
            }
            Ok(Outcome { segments, state: FileState::Done, detail: None, info })
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing::FakeEmbedder;
    use crate::types::Job;
    use digup_extract_audio::AudioWindow;
    use digup_extract_text::code::CodeStretch;
    use digup_extract_text::docs::DocChunk;
    use digup_store::index::{FileRecord, NewFile};
    use std::sync::atomic::{AtomicUsize, Ordering};

    static N: AtomicUsize = AtomicUsize::new(0);

    fn setup(kind: FileKind) -> (IndexStore, FileRecord) {
        let dir = std::env::temp_dir().join(format!(
            "digup-commit-test-{}-{}",
            std::process::id(),
            N.fetch_add(1, Ordering::SeqCst)
        ));
        let _ = std::fs::remove_dir_all(&dir);
        let mut store = IndexStore::open(&dir, "index.sqlite").unwrap();
        let f = NewFile { path: "/x/some file.dat".into(), kind, size: 1, mtime: 1.0, inode: 1, device: 1 };
        store.insert_file(&f, &[]).unwrap();
        let rec = store.pending(None, None).unwrap().remove(0);
        (store, rec)
    }

    fn prepared(rec: &FileRecord, result: Result<Payload, Leave>) -> Prepared {
        Prepared { job: Job { file: rec.clone(), step: None, content_hash: None }, result }
    }

    /// (kind, modality, loc, loc_end, has_vec, has_fts) of every non-name segment.
    type Row = (String, String, Option<f64>, Option<f64>, bool, bool);
    fn rows(store: &IndexStore) -> Vec<Row> {
        let mut st = store
            .connection()
            .prepare(
                "SELECT kind, modality, loc, loc_end, vec IS NOT NULL, \
                 EXISTS(SELECT 1 FROM fts WHERE fts.rowid = segments.id AND body <> '') \
                 FROM segments WHERE kind <> 'name' ORDER BY id",
            )
            .unwrap();
        st.query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?, r.get(5)?)))
            .unwrap()
            .map(|r| r.unwrap())
            .collect()
    }

    fn state(store: &IndexStore, id: i64) -> (String, Option<String>) {
        store
            .connection()
            .query_row("SELECT state, detail FROM files WHERE id = ?1", [id], |r| Ok((r.get(0)?, r.get(1)?)))
            .unwrap()
    }

    fn image(kind: FileKind, ocr: Option<&str>) -> Payload {
        Payload::Image {
            kind,
            width: 8,
            height: 4,
            rgb: vec![7; 8 * 4 * 3],
            orig_width: 4000,
            orig_height: 3000,
            taken: Some("2024-01-02".into()),
            ocr: ocr.map(String::from),
        }
    }

    fn chunks(range: std::ops::Range<usize>) -> Vec<DocChunk> {
        range.map(|i| DocChunk { index: i, text: format!("hello words in chunk number {i}") }).collect()
    }

    #[test]
    fn photo_uses_photo_budget() {
        let (mut store, rec) = setup(FileKind::Image);
        let mut fake = FakeEmbedder::default();
        let opts = IndexOptions::default();
        commit(&mut store, &mut fake, prepared(&rec, Ok(image(FileKind::Image, None))), &opts).unwrap();
        assert_eq!(fake.images, vec![(8, 4, opts.photo_budget)]);
        let r = rows(&store);
        assert_eq!(r.len(), 1);
        assert_eq!((r[0].0.as_str(), r[0].1.as_str(), r[0].4), ("image", "image", true));
        assert_eq!(state(&store, rec.id).0, "done");
        let info = store.info(rec.id).unwrap();
        assert_eq!(info["width"], "4000");
        assert_eq!(info["height"], "3000");
        assert_eq!(info["taken"], "2024-01-02");
        assert!(!info.contains_key("ocr_chars"));
    }

    #[test]
    fn screenshot_with_ocr_adds_keyword_only_segment() {
        let (mut store, rec) = setup(FileKind::Screenshot);
        let mut fake = FakeEmbedder::default();
        let opts = IndexOptions::default();
        let text = format!("error   code\n42 {}", "x".repeat(300));
        commit(&mut store, &mut fake, prepared(&rec, Ok(image(FileKind::Screenshot, Some(&text)))), &opts).unwrap();
        assert_eq!(fake.images[0].2, opts.screenshot_budget);
        let r = rows(&store);
        assert_eq!(r.len(), 2);
        assert_eq!((r[1].0.as_str(), r[1].4, r[1].5), ("ocr", false, true));
        let ex: String = store
            .connection()
            .query_row("SELECT excerpt FROM segments WHERE kind='ocr'", [], |r| r.get(0))
            .unwrap();
        assert!(ex.starts_with("error code 42 xxx"));
        assert!(ex.ends_with('\u{2026}'));
        assert_eq!(ex.chars().count(), 241);
        assert_eq!(store.info(rec.id).unwrap()["ocr_chars"], text.chars().count().to_string());
    }

    #[test]
    fn doc_whole_is_done() {
        let (mut store, rec) = setup(FileKind::Doc);
        let mut fake = FakeEmbedder::default();
        let p = Payload::Doc { title: "T".into(), total: 3, range: 0..3, chars: 99, chunks: chunks(0..3) };
        commit(&mut store, &mut fake, prepared(&rec, Ok(p)), &IndexOptions::default()).unwrap();
        assert_eq!(fake.texts.len(), 3);
        assert_eq!(fake.texts[0], doc_prompt(Some("T"), "hello words in chunk number 0"));
        let r = rows(&store);
        assert_eq!(r.len(), 3);
        assert_eq!(r[2].2, Some(3.0));
        assert!(r.iter().all(|x| x.4 && x.5));
        assert_eq!(state(&store, rec.id), ("done".into(), None));
        let info = store.info(rec.id).unwrap();
        assert_eq!((info["chunks"].as_str(), info["chars"].as_str()), ("3", "99"));
        assert!(!info.contains_key("read_to"));
    }

    #[test]
    fn doc_partial_then_finished() {
        let (mut store, rec) = setup(FileKind::Doc);
        let mut fake = FakeEmbedder::default();
        let opts = IndexOptions::default();
        let p = Payload::Doc { title: "T".into(), total: 45, range: 0..40, chars: 10, chunks: chunks(0..40) };
        commit(&mut store, &mut fake, prepared(&rec, Ok(p)), &opts).unwrap();
        assert_eq!(state(&store, rec.id), ("partial".into(), Some("read to chunk 40 of 45".into())));
        let info = store.info(rec.id).unwrap();
        assert_eq!((info["read_to"].as_str(), info["read_end"].as_str()), ("40", "45"));
        let p = Payload::Doc { title: "T".into(), total: 45, range: 40..45, chars: 10, chunks: chunks(40..45) };
        commit(&mut store, &mut fake, prepared(&rec, Ok(p)), &opts).unwrap();
        assert_eq!(state(&store, rec.id).0, "done");
        let r = rows(&store);
        assert_eq!(r.len(), 45);
        assert_eq!(r[40].2, Some(41.0));
        assert!(!store.info(rec.id).unwrap().contains_key("read_to"));
    }

    #[test]
    fn doc_without_words_has_no_vector_and_empty_doc_is_skipped() {
        let (mut store, rec) = setup(FileKind::Doc);
        let mut fake = FakeEmbedder::default();
        let c = vec![
            DocChunk { index: 0, text: "1234 5678 9012".into() },
            DocChunk { index: 1, text: "some real words here".into() },
        ];
        let p = Payload::Doc { title: "T".into(), total: 2, range: 0..2, chars: 5, chunks: c };
        commit(&mut store, &mut fake, prepared(&rec, Ok(p)), &IndexOptions::default()).unwrap();
        assert_eq!(fake.texts.len(), 1);
        let r = rows(&store);
        assert_eq!((r[0].1.as_str(), r[0].4, r[0].5), ("none", false, true));
        assert_eq!((r[1].1.as_str(), r[1].4), ("text", true));

        let (mut store, rec) = setup(FileKind::Doc);
        let p = Payload::Doc { title: "T".into(), total: 0, range: 0..0, chars: 0, chunks: vec![] };
        commit(&mut store, &mut fake, prepared(&rec, Ok(p)), &IndexOptions::default()).unwrap();
        assert_eq!(state(&store, rec.id), ("skipped".into(), Some("no text".into())));
    }

    fn windows(n: usize) -> Box<dyn Iterator<Item = Result<AudioWindow>> + Send> {
        Box::new((0..n).map(|i| Ok(AudioWindow { start_secs: i as f64 * 25.0, samples: vec![0.1; 16000 * 30] })))
    }

    #[test]
    fn audio_windows_and_silence() {
        let (mut store, rec) = setup(FileKind::Audio);
        let mut fake = FakeEmbedder::default();
        let opts = IndexOptions::default();
        commit(&mut store, &mut fake, prepared(&rec, Ok(Payload::Audio { duration: 55.04, windows: windows(3) })), &opts)
            .unwrap();
        assert_eq!(fake.audio.len(), 3);
        let r = rows(&store);
        assert_eq!(r.len(), 3);
        assert_eq!((r[1].0.as_str(), r[1].1.as_str(), r[1].2, r[1].3), ("audio", "audio", Some(25.0), Some(55.0)));
        assert_eq!(store.info(rec.id).unwrap()["duration"], "80.0");

        let (mut store, rec) = setup(FileKind::Audio);
        commit(&mut store, &mut fake, prepared(&rec, Ok(Payload::Audio { duration: 1.0, windows: windows(0) })), &opts)
            .unwrap();
        assert_eq!(state(&store, rec.id), ("skipped".into(), Some("silent or shorter than 2 s".into())));
    }

    #[test]
    fn code_stretches() {
        let (mut store, rec) = setup(FileKind::Code);
        let mut fake = FakeEmbedder::default();
        let s = vec![
            CodeStretch { start_line: 1, end_line: 9, text: "def retry_with_backoff():\n    pass".into() },
            CodeStretch { start_line: 10, end_line: 12, text: "1 2 3 4 5 6".into() },
        ];
        let p = Payload::Code { title: "r/a.py".into(), repo: Some("/r".into()), lines: 12, cells: None, stretches: s };
        commit(&mut store, &mut fake, prepared(&rec, Ok(p)), &IndexOptions::default()).unwrap();
        assert_eq!(fake.texts.len(), 1);
        let r = rows(&store);
        assert_eq!((r[0].0.as_str(), r[0].2, r[0].3, r[0].4), ("lines", Some(1.0), Some(9.0), true));
        assert!(!r[1].4 && r[1].5);
        let info = store.info(rec.id).unwrap();
        assert_eq!((info["lines"].as_str(), info["repo"].as_str()), ("12", "/r"));
        assert_eq!(state(&store, rec.id).0, "done");
    }

    #[test]
    fn image_embed_failure_fails_only_that_file() {
        let (mut store, rec) = setup(FileKind::Image);
        let mut fake = FakeEmbedder { fail_image_width: Some(8), ..Default::default() };
        commit(&mut store, &mut fake, prepared(&rec, Ok(image(FileKind::Image, None))), &IndexOptions::default())
            .unwrap();
        let (s, d) = state(&store, rec.id);
        assert_eq!(s, "failed");
        assert!(d.unwrap().contains("fake image failure"));
        assert!(rows(&store).is_empty());
    }

    #[test]
    fn leave_values_are_recorded() {
        let (mut store, rec) = setup(FileKind::Doc);
        let mut fake = FakeEmbedder::default();
        let opts = IndexOptions::default();
        commit(&mut store, &mut fake, prepared(&rec, Err(Leave::Skipped("too small".into()))), &opts).unwrap();
        assert_eq!(state(&store, rec.id), ("skipped".into(), Some("too small".into())));
        commit(&mut store, &mut fake, prepared(&rec, Err(Leave::Failed("boom".into()))), &opts).unwrap();
        assert_eq!(state(&store, rec.id), ("failed".into(), Some("boom".into())));
    }
}
