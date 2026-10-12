//! The pipeline loop: prepare on all cores (rayon, bounded channel) while one thread embeds and commits.
//! Port of Swift `Indexer.run` / `finishLongFiles` / `process` (Indexer.swift:139-260), with more prepare parallelism.

use crate::commit::commit;
use crate::options::IndexOptions;
use crate::prepare::prepare;
use crate::types::{Embed, Job, Ocr, Prepared};
use anyhow::Result;
use digup_store::index::{Adoption, IndexStore, Mismatch};
use rayon::prelude::*;
use std::collections::{BTreeMap, HashSet};
use std::sync::mpsc::sync_channel;
use std::time::Instant;

/// One file (or one step of a long file) finished.
#[derive(Debug, Clone)]
pub struct Event {
    pub done: usize,
    pub total: usize,
    pub path: String,
    pub kind: digup_core::FileKind,
    /// `done`, `partial`, `skipped`, `failed`.
    pub state: String,
    pub note: Option<String>,
    pub seconds: f64,
}

/// Records or checks the vector source and settings fingerprint (Swift `checkModel`). With `reset`, a mismatch empties
/// the vectors and requeues every file; without, it is an error.
pub fn check_model(store: &mut IndexStore, embedder: &dyn Embed, opts: &IndexOptions, code: bool, reset: bool) -> Result<Adoption> {
    let fingerprint = if code { opts.code_fingerprint() } else { opts.fingerprint() };
    store.adopt(&embedder.vector_source(), &fingerprint, if reset { Mismatch::Reset } else { Mismatch::Refuse })
}

/// Indexes the pending files, newest first inside each kind. Returns the count per final state.
/// `should_stop` is asked between files only.
#[allow(clippy::too_many_arguments)]
pub fn run(
    store: &mut IndexStore,
    embedder: &mut dyn Embed,
    ocr: &dyn Ocr,
    opts: &IndexOptions,
    limit: Option<usize>,
    only: Option<&[i64]>,
    should_stop: &dyn Fn() -> bool,
    on_event: &mut dyn FnMut(&Event),
) -> Result<BTreeMap<String, usize>> {
    let jobs: Vec<Job> = store.pending(limit, only)?.into_iter().map(|file| Job { file, step: None }).collect();
    process(store, embedder, ocr, opts, jobs, should_stop, on_event)
}

/// The later pass: reads the rest of long documents (`partial` after their first step), a step at a time.
pub fn finish_long_files(
    store: &mut IndexStore,
    embedder: &mut dyn Embed,
    ocr: &dyn Ocr,
    opts: &IndexOptions,
    should_stop: &dyn Fn() -> bool,
    on_event: &mut dyn FnMut(&Event),
) -> Result<BTreeMap<String, usize>> {
    let mut jobs = Vec::new();
    for file in store.unfinished()? {
        let info = store.info(file.id)?;
        let size = if file.kind == digup_core::FileKind::Pdf { opts.pdf_step } else { opts.doc_step }.max(1);
        let mut start = info.get("read_to").and_then(|v| v.parse::<usize>().ok()).unwrap_or(0);
        let end = info.get("read_end").and_then(|v| v.parse::<usize>().ok()).unwrap_or(0).max(start + 1);
        while start < end {
            jobs.push(Job { file: file.clone(), step: Some(start..(start + size).min(end)) });
            start += size;
        }
    }
    process(store, embedder, ocr, opts, jobs, should_stop, on_event)
}

fn process(
    store: &mut IndexStore,
    embedder: &mut dyn Embed,
    ocr: &dyn Ocr,
    opts: &IndexOptions,
    jobs: Vec<Job>,
    should_stop: &dyn Fn() -> bool,
    on_event: &mut dyn FnMut(&Event),
) -> Result<BTreeMap<String, usize>> {
    let total = jobs.len();
    let mut states: BTreeMap<String, usize> = BTreeMap::new();
    let mut finished: HashSet<i64> = HashSet::new();
    let batch = (rayon::current_num_threads() * 2).max(4);
    let (tx, rx) = sync_channel::<Prepared>(batch);
    let mut outcome: Result<()> = Ok(());
    std::thread::scope(|scope| {
        let jobs_ref = &jobs;
        scope.spawn(move || {
            // Chunks keep the order (steps of one file must commit in order); the channel bounds memory.
            for chunk in jobs_ref.chunks(batch) {
                let prepared: Vec<Prepared> = chunk.par_iter().map(|job| prepare(job, opts, ocr)).collect();
                for p in prepared {
                    if tx.send(p).is_err() {
                        return;
                    }
                }
            }
        });
        let mut done = 0;
        for prepared in rx.iter() {
            if should_stop() {
                break;
            }
            let file = prepared.job.file.clone();
            if finished.contains(&file.id) && prepared.job.step.is_some() {
                done += 1;
                continue;
            }
            let started = Instant::now();
            if let Err(e) = commit(store, embedder, prepared, opts) {
                outcome = Err(e);
                break;
            }
            let (state, note) = match settled_state(store, file.id) {
                Ok(v) => v,
                Err(e) => {
                    outcome = Err(e);
                    break;
                }
            };
            // A file that ends sooner than its steps said (length was a guess, or it went unreadable) skips the rest.
            if state != "partial" {
                finished.insert(file.id);
                *states.entry(state.clone()).or_default() += 1;
            }
            done += 1;
            on_event(&Event { done, total, path: file.path, kind: file.kind, state, note, seconds: started.elapsed().as_secs_f64() });
        }
        drop(rx);
    });
    outcome?;
    Ok(states)
}

fn settled_state(store: &IndexStore, id: i64) -> Result<(String, Option<String>)> {
    Ok(store.connection().query_row("SELECT state, detail FROM files WHERE id = ?1", [id], |r| Ok((r.get(0)?, r.get(1)?)))?)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sync::sync;
    use crate::testing::FakeEmbedder;
    use crate::types::NoOcr;

    fn temp(name: &str) -> std::path::PathBuf {
        let d = std::env::temp_dir().join(format!("digup-run-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn indexes_pending_then_finishes_long_doc() {
        let root = temp("e2e");
        let paragraphs: Vec<String> = (0..120).map(|i| format!("Paragraph {i}: ").repeat(1) + &"the quick brown fox jumps. ".repeat(40)).collect();
        std::fs::write(root.join("long.txt"), paragraphs.join("\n\n")).unwrap();
        std::fs::write(root.join("short.txt"), "A short note about penguins and their habitat.").unwrap();
        let img = image::RgbImage::from_fn(600, 400, |x, y| image::Rgb([(x % 256) as u8, (y % 256) as u8, 90]));
        img.save(root.join("photo.jpg")).unwrap();

        let dir = temp("store");
        let mut store = IndexStore::open(&dir, "index.sqlite").unwrap();
        let opts = IndexOptions::default();
        sync(&mut store, std::slice::from_ref(&root), &opts, false).unwrap();
        let mut fake = FakeEmbedder::default();
        check_model(&mut store, &fake, &opts, false, false).unwrap();

        let mut events = Vec::new();
        let states = run(&mut store, &mut fake, &NoOcr, &opts, None, None, &|| false, &mut |e| events.push(e.clone())).unwrap();
        assert_eq!(events.len(), 3, "{events:?}");
        // Partial files are not final, so the long document is not counted yet.
        assert_eq!(events.iter().filter(|e| e.state == "partial").count(), states.values().sum::<usize>().abs_diff(3));
        assert_eq!(store.pending_count().unwrap(), 0);
        assert_eq!(fake.images.len(), 1);

        // The long text file is partial until the later pass reads on.
        let (unfinished, _) = store.unfinished_count().unwrap();
        let steps = finish_long_files(&mut store, &mut fake, &NoOcr, &opts, &|| false, &mut |_| {}).unwrap();
        assert!(unfinished >= 1, "the 120-paragraph file should stop after its first step");
        assert_eq!(steps.get("done").copied(), Some(unfinished), "{steps:?}");
        assert_eq!(store.unfinished_count().unwrap().0, 0);

        // Nothing changed: sync queues nothing, run does nothing.
        let summary = sync(&mut store, std::slice::from_ref(&root), &opts, false).unwrap();
        assert_eq!(summary.queued.len(), 0);
        let again = run(&mut store, &mut fake, &NoOcr, &opts, None, None, &|| false, &mut |_| {}).unwrap();
        assert!(again.is_empty());
    }

    #[test]
    fn stop_between_files_leaves_the_rest_pending() {
        let root = temp("stop");
        for i in 0..4 {
            std::fs::write(root.join(format!("n{i}.txt")), format!("Note number {i} about gardens and tomatoes.")).unwrap();
        }
        let dir = temp("store-stop");
        let mut store = IndexStore::open(&dir, "index.sqlite").unwrap();
        let opts = IndexOptions::default();
        sync(&mut store, &[root], &opts, false).unwrap();
        let mut fake = FakeEmbedder::default();
        let seen = std::cell::Cell::new(0);
        let stop = || seen.get() >= 2;
        run(&mut store, &mut fake, &NoOcr, &opts, None, None, &stop, &mut |_| seen.set(seen.get() + 1)).unwrap();
        assert_eq!(store.pending_count().unwrap(), 2);
    }
}
