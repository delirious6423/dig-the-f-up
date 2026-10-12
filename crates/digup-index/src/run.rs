//! The pipeline loop: prepare on all cores (rayon, bounded channel) while one thread embeds and commits.
//! Port of Swift `Indexer.run` / `finishLongFiles` / `process` (Indexer.swift:139-260), with more prepare parallelism.

use crate::commit::commit;
use crate::options::IndexOptions;
use crate::prepare::prepare;
use crate::types::{Embed, Job, Ocr, Prepared};
use anyhow::Result;
use crate::commit::CONTENT_HASH;
use crate::commit::CONTENT_SIZE;
use digup_store::index::{Adoption, FileRecord, IndexStore, Mismatch};
use rayon::prelude::*;
use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::Path;
use std::sync::mpsc::sync_channel;
use std::time::{Instant, UNIX_EPOCH};

/// Mtime tolerance used by sync (sync.rs `MTIME_TOLERANCE`).
const MTIME_TOLERANCE: f64 = 0.001;

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

/// Moves files the user opens often to the front of their kind. `pending` arrives ordered by kind, then newest first;
/// the sort is stable and only reorders inside a run of one kind, so a screenshot never waits behind a video.
fn prioritise(pending: &mut [FileRecord], opts: &IndexOptions) {
    let Some(scores) = opts.priority.as_ref().filter(|s| !s.is_empty()) else { return };
    let score = |f: &FileRecord| scores.get(&f.path).copied().unwrap_or(0.0);
    let mut start = 0;
    while start < pending.len() {
        let kind = pending[start].kind;
        let end = start + pending[start..].iter().take_while(|f| f.kind == kind).count();
        pending[start..end].sort_by(|a, b| score(b).total_cmp(&score(a)));
        start = end;
    }
}

/// Indexes the pending files, newest first inside each kind. Returns the count per final state.
/// `should_stop` is asked between files only.
///
/// Byte-identical files are not embedded twice (`opts.dedup_by_content`): a pending file whose bytes equal an indexed
/// file of the same kind and size gets that file's rows copied; a pending twin of another pending file waits for it
/// to settle and then copies its rows. Only the first of each identical group reaches prepare and embedding.
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
    let mut pending = store.pending(limit, only)?;
    prioritise(&mut pending, opts);
    let total = pending.len();
    if !opts.dedup_by_content {
        let jobs = pending.into_iter().map(|file| Job { file, step: None, content_hash: None }).collect();
        return process(store, embedder, ocr, opts, jobs, (0, total), should_stop, on_event).map(|(states, _)| states);
    }
    let plan = plan_duplicates(store, opts, pending)?;
    let mut states: BTreeMap<String, usize> = BTreeMap::new();
    let mut done = 0;
    let mut jobs = plan.jobs;
    for copy in plan.copies {
        if should_stop() {
            return Ok(states);
        }
        let started = Instant::now();
        if store.copy_indexed_content(copy.source, copy.target.id, &copy.info)? {
            done += 1;
            count(&mut states, "done");
            on_event(&Event {
                done,
                total,
                path: copy.target.path,
                kind: copy.target.kind,
                state: "done".into(),
                note: Some(format!("same bytes as {}", copy.source_path)),
                seconds: started.elapsed().as_secs_f64(),
            });
        } else {
            // The source changed state since planning: embed as usual.
            jobs.push(Job { file: copy.target, step: None, content_hash: Some(copy.hash) });
        }
    }
    let (pass, n) = process(store, embedder, ocr, opts, jobs, (done, total), should_stop, on_event)?;
    merge_states(&mut states, pass);
    done = n;
    // Twins wait for their first copy to settle; if it did not finish, the twin is embedded itself.
    let mut rest = Vec::new();
    for twin in plan.twins {
        if should_stop() {
            return Ok(states);
        }
        let started = Instant::now();
        let (primary_state, _) = settled_state(store, twin.primary)?;
        let copied = primary_state == "done" && {
            let info = copy_info(store, twin.primary, &twin.hash, twin.target.size)?;
            store.copy_indexed_content(twin.primary, twin.target.id, &info)?
        };
        if copied {
            done += 1;
            count(&mut states, "done");
            on_event(&Event {
                done,
                total,
                path: twin.target.path,
                kind: twin.target.kind,
                state: "done".into(),
                note: Some(format!("same bytes as {}", twin.primary_path)),
                seconds: started.elapsed().as_secs_f64(),
            });
        } else {
            rest.push(Job { file: twin.target, step: None, content_hash: Some(twin.hash) });
        }
    }
    if !rest.is_empty() {
        let (pass, _) = process(store, embedder, ocr, opts, rest, (done, total), should_stop, on_event)?;
        merge_states(&mut states, pass);
    }
    Ok(states)
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
            jobs.push(Job { file: file.clone(), step: Some(start..(start + size).min(end)), content_hash: None });
            start += size;
        }
    }
    let total = jobs.len();
    process(store, embedder, ocr, opts, jobs, (0, total), should_stop, on_event).map(|(states, _)| states)
}

/// A file whose bytes equal an indexed file: rows are copied from `source`.
struct IndexedCopy {
    source: i64,
    source_path: String,
    target: FileRecord,
    hash: String,
    info: BTreeMap<String, String>,
}

/// A file whose bytes equal a pending file that is embedded in this run: rows are copied after that file settles.
struct Twin {
    target: FileRecord,
    primary: i64,
    primary_path: String,
    hash: String,
}

#[derive(Default)]
struct Plan {
    copies: Vec<IndexedCopy>,
    jobs: Vec<Job>,
    twins: Vec<Twin>,
}

/// Sorts the pending files into copies, twins and jobs. Hashing is limited to files that have a same-kind, same-size
/// candidate (an indexed file or a pending peer), so a file with a unique size is never read twice. Code files and files
/// above `dedup_max_bytes` are never hashed.
fn plan_duplicates(store: &IndexStore, opts: &IndexOptions, pending: Vec<FileRecord>) -> Result<Plan> {
    let eligible = |f: &FileRecord| f.kind != digup_core::FileKind::Code && f.size >= 0 && (f.size as u64) <= opts.dedup_max_bytes;
    let mut peers: HashMap<(&'static str, i64), usize> = HashMap::new();
    for f in pending.iter().filter(|f| eligible(f)) {
        *peers.entry((f.kind.as_str(), f.size)).or_default() += 1;
    }
    let sizes: HashSet<i64> = peers.keys().map(|(_, size)| *size).collect();
    let mut indexed: HashMap<(&'static str, i64), Vec<FileRecord>> = HashMap::new();
    if !sizes.is_empty() {
        for f in store.done_files_with_sizes(&sizes)? {
            indexed.entry((f.kind.as_str(), f.size)).or_default().push(f);
        }
    }
    let mut plan = Plan::default();
    let mut primaries: HashMap<String, (i64, String)> = HashMap::new();
    for file in pending {
        let key = (file.kind.as_str(), file.size);
        let candidates = indexed.get(&key);
        let has_peer = peers.get(&key).copied().unwrap_or(0) > 1;
        if !eligible(&file) || (candidates.is_none() && !has_peer) {
            plan.jobs.push(Job { file, step: None, content_hash: None });
            continue;
        }
        let hash = match crate::prepare::content_hash(Path::new(&file.path)) {
            Ok(h) => h,
            Err(_) => {
                // Unreadable now: prepare reports the failure as usual.
                plan.jobs.push(Job { file, step: None, content_hash: None });
                continue;
            }
        };
        let mut source = None;
        for candidate in candidates.into_iter().flatten() {
            if indexed_hash(store, candidate)?.as_deref() == Some(hash.as_str()) {
                source = Some(candidate.clone());
                break;
            }
        }
        if let Some(src) = source {
            let info = copy_info(store, src.id, &hash, file.size)?;
            plan.copies.push(IndexedCopy { source: src.id, source_path: src.path, target: file, hash, info });
        } else if let Some((primary, primary_path)) = primaries.get(&hash) {
            plan.twins.push(Twin { target: file, primary: *primary, primary_path: primary_path.clone(), hash });
        } else {
            primaries.insert(hash.clone(), (file.id, file.path.clone()));
            plan.jobs.push(Job { file, step: None, content_hash: Some(hash) });
        }
    }
    Ok(plan)
}

/// The content hash an indexed file was stored with. Files indexed before this feature have none: their hash is taken
/// now and stored, but only while the file still matches its record (size and mtime), so the hash is of the indexed bytes.
fn indexed_hash(store: &IndexStore, file: &FileRecord) -> Result<Option<String>> {
    let info = store.info(file.id)?;
    if let Some(hash) = info.get(CONTENT_HASH) {
        return Ok(Some(hash.clone()));
    }
    let Ok(meta) = std::fs::metadata(&file.path) else { return Ok(None) };
    let mtime = meta.modified().ok().and_then(|t| t.duration_since(UNIX_EPOCH).ok()).map(|d| d.as_secs_f64());
    if meta.len() != file.size as u64 || mtime.is_none_or(|m| (m - file.mtime).abs() > MTIME_TOLERANCE) {
        return Ok(None);
    }
    let Ok(hash) = crate::prepare::content_hash(Path::new(&file.path)) else { return Ok(None) };
    store.set_info_value(file.id, CONTENT_HASH, &hash)?;
    store.set_info_value(file.id, CONTENT_SIZE, &file.size.to_string())?;
    Ok(Some(hash))
}

/// The info a copy gets: the source's keys (minus reading progress) with this content's hash and size.
fn copy_info(store: &IndexStore, source: i64, hash: &str, size: i64) -> Result<BTreeMap<String, String>> {
    let mut info = store.info(source)?;
    info.remove("read_to");
    info.remove("read_end");
    info.insert(CONTENT_HASH.into(), hash.into());
    info.insert(CONTENT_SIZE.into(), size.to_string());
    Ok(info)
}

fn count(states: &mut BTreeMap<String, usize>, state: &str) {
    *states.entry(state.into()).or_default() += 1;
}

fn merge_states(into: &mut BTreeMap<String, usize>, from: BTreeMap<String, usize>) {
    for (state, n) in from {
        *into.entry(state).or_default() += n;
    }
}

/// Prepares and commits `jobs` in order. `progress` is (events already sent, total events of the whole run).
/// Returns the count per final state and the events sent so far.
#[allow(clippy::too_many_arguments)]
fn process(
    store: &mut IndexStore,
    embedder: &mut dyn Embed,
    ocr: &dyn Ocr,
    opts: &IndexOptions,
    jobs: Vec<Job>,
    progress: (usize, usize),
    should_stop: &dyn Fn() -> bool,
    on_event: &mut dyn FnMut(&Event),
) -> Result<(BTreeMap<String, usize>, usize)> {
    let (mut done, total) = progress;
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
                count(&mut states, &state);
            }
            done += 1;
            on_event(&Event { done, total, path: file.path, kind: file.kind, state, note, seconds: started.elapsed().as_secs_f64() });
        }
        drop(rx);
    });
    outcome?;
    Ok((states, done))
}

fn settled_state(store: &IndexStore, id: i64) -> Result<(String, Option<String>)> {
    Ok(store.connection().query_row("SELECT state, detail FROM files WHERE id = ?1", [id], |r| Ok((r.get(0)?, r.get(1)?)))?)
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::sync::Arc;

    fn record(id: i64, path: &str, kind: digup_core::FileKind) -> FileRecord {
        FileRecord {
            id,
            path: path.into(),
            kind,
            size: 1,
            mtime: 0.0,
            inode: 0,
            device: 0,
            state: digup_core::FileState::Pending,
        }
    }

    #[test]
    fn priority_reorders_inside_a_kind_only() {
        use digup_core::FileKind::{Doc, Pdf};
        let mut pending = vec![
            record(1, "/a.pdf", Pdf),
            record(2, "/b.pdf", Pdf),
            record(3, "/c.txt", Doc),
            record(4, "/d.txt", Doc),
            record(5, "/e.txt", Doc),
        ];
        let mut opts = IndexOptions::default();
        prioritise(&mut pending, &opts);
        assert_eq!(pending.iter().map(|f| f.id).collect::<Vec<_>>(), [1, 2, 3, 4, 5], "no scores: order unchanged");
        opts.priority = Some(Arc::new(HashMap::from([("/b.pdf".to_string(), 2.0), ("/e.txt".to_string(), 3.0), ("/d.txt".to_string(), 1.0)])));
        prioritise(&mut pending, &opts);
        assert_eq!(
            pending.iter().map(|f| f.id).collect::<Vec<_>>(),
            [2, 1, 5, 4, 3],
            "opened files first inside each kind; the kinds keep their order; ties keep newest-first"
        );
    }

    use crate::sync::sync;
    use crate::testing::FakeEmbedder;
    use crate::commit::CONTENT_HASH;
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

    const NOTE: &str = "A short note about penguins and their habitat.";

    fn dedup_setup(name: &str) -> (std::path::PathBuf, IndexStore, FakeEmbedder) {
        let root = temp(&format!("{name}-root"));
        let mut store = IndexStore::open(&temp(&format!("{name}-store")), "index.sqlite").unwrap();
        let fake = FakeEmbedder::default();
        check_model(&mut store, &fake, &IndexOptions::default(), false, false).unwrap();
        (root, store, fake)
    }

    fn index(root: &std::path::Path, store: &mut IndexStore, fake: &mut FakeEmbedder, opts: &IndexOptions) -> BTreeMap<String, usize> {
        sync(store, std::slice::from_ref(&root.to_path_buf()), opts, false).unwrap();
        run(store, fake, &NoOcr, opts, None, None, &|| false, &mut |_| {}).unwrap()
    }

    fn id_of(store: &IndexStore, name: &str) -> i64 {
        store.connection().query_row("SELECT id FROM files WHERE path LIKE ?1", [format!("%/{name}")], |r| r.get(0)).unwrap()
    }

    fn files_matching(store: &IndexStore, term: &str) -> Vec<i64> {
        let mut stmt = store
            .connection()
            .prepare("SELECT DISTINCT s.file FROM fts JOIN segments s ON s.id = fts.rowid WHERE fts MATCH ?1 ORDER BY s.file")
            .unwrap();
        stmt.query_map([term], |r| r.get(0)).unwrap().map(|r| r.unwrap()).collect()
    }

    fn segment_count(store: &IndexStore, id: i64) -> i64 {
        store.connection().query_row("SELECT COUNT(*) FROM segments WHERE file = ?1 AND kind != 'name'", [id], |r| r.get(0)).unwrap()
    }

    #[test]
    fn duplicate_of_indexed_file_is_copied_without_embedding() {
        let (root, mut store, mut fake) = dedup_setup("copy");
        let opts = IndexOptions::default();
        std::fs::write(root.join("a.txt"), NOTE).unwrap();
        index(&root, &mut store, &mut fake, &opts);
        let after_a = fake.texts.len();
        assert!(after_a > 0);

        std::fs::write(root.join("b.txt"), NOTE).unwrap();
        let states = index(&root, &mut store, &mut fake, &opts);
        assert_eq!(fake.texts.len(), after_a, "the duplicate must not reach the embedder");
        assert_eq!(states.get("done"), Some(&1), "{states:?}");
        let (a, b) = (id_of(&store, "a.txt"), id_of(&store, "b.txt"));
        assert_eq!(files_matching(&store, "penguins"), vec![a, b], "both paths are searchable");
        assert_eq!(segment_count(&store, a), segment_count(&store, b));
        let hash_a = store.info(a).unwrap().get(CONTENT_HASH).cloned();
        assert!(hash_a.is_some(), "the indexed twin gets its hash stored on first use");
        assert_eq!(store.info(b).unwrap().get(CONTENT_HASH).cloned(), hash_a);
        assert_eq!(store.pending_count().unwrap(), 0);
    }

    #[test]
    fn identical_pending_pair_embeds_once() {
        let (root, mut store, mut fake) = dedup_setup("pair");
        let opts = IndexOptions::default();
        std::fs::write(root.join("a.txt"), NOTE).unwrap();
        std::fs::write(root.join("b.txt"), NOTE).unwrap();
        let states = index(&root, &mut store, &mut fake, &opts);
        assert_eq!(states.get("done"), Some(&2), "{states:?}");

        let solo = temp("pair-solo-root");
        let mut solo_store = IndexStore::open(&temp("pair-solo-store"), "index.sqlite").unwrap();
        let mut solo_fake = FakeEmbedder::default();
        check_model(&mut solo_store, &solo_fake, &opts, false, false).unwrap();
        std::fs::write(solo.join("a.txt"), NOTE).unwrap();
        index(&solo, &mut solo_store, &mut solo_fake, &opts);
        assert_eq!(fake.texts.len(), solo_fake.texts.len());
        let (a, b) = (id_of(&store, "a.txt"), id_of(&store, "b.txt"));
        assert_eq!(files_matching(&store, "penguins"), vec![a, b]);
    }

    #[test]
    fn editing_one_duplicate_reembeds_only_that_one() {
        let (root, mut store, mut fake) = dedup_setup("edit");
        let opts = IndexOptions::default();
        std::fs::write(root.join("a.txt"), NOTE).unwrap();
        std::fs::write(root.join("b.txt"), NOTE).unwrap();
        index(&root, &mut store, &mut fake, &opts);
        let (a, b) = (id_of(&store, "a.txt"), id_of(&store, "b.txt"));
        let b_segments = segment_count(&store, b);
        let before = fake.texts.len();

        std::fs::write(root.join("a.txt"), "Tomatoes ripen best in the summer garden.").unwrap();
        index(&root, &mut store, &mut fake, &opts);
        assert!(fake.texts.len() > before, "the edited file is embedded");
        assert_eq!(id_of(&store, "a.txt"), a);
        assert_eq!(segment_count(&store, b), b_segments, "the untouched duplicate keeps its rows");
        assert_eq!(files_matching(&store, "penguins"), vec![b]);
        assert_eq!(files_matching(&store, "tomatoes"), vec![a]);
    }

    #[test]
    fn deleting_one_duplicate_keeps_the_other() {
        let (root, mut store, mut fake) = dedup_setup("delete");
        let opts = IndexOptions::default();
        std::fs::write(root.join("a.txt"), NOTE).unwrap();
        index(&root, &mut store, &mut fake, &opts);
        std::fs::write(root.join("b.txt"), NOTE).unwrap();
        index(&root, &mut store, &mut fake, &opts);
        let (a, b) = (id_of(&store, "a.txt"), id_of(&store, "b.txt"));
        let a_segments = segment_count(&store, a);
        assert!(a_segments > 0);

        store.delete_file(b).unwrap();
        assert_eq!(segment_count(&store, a), a_segments);
        assert_eq!(files_matching(&store, "penguins"), vec![a]);
    }

    #[test]
    fn files_over_the_size_cap_are_not_hashed() {
        let (root, mut store, mut fake) = dedup_setup("cap");
        let opts = IndexOptions { dedup_max_bytes: 1, ..IndexOptions::default() };
        std::fs::write(root.join("a.txt"), NOTE).unwrap();
        index(&root, &mut store, &mut fake, &opts);
        let after_a = fake.texts.len();
        std::fs::write(root.join("b.txt"), NOTE).unwrap();
        index(&root, &mut store, &mut fake, &opts);
        assert!(fake.texts.len() > after_a, "over the cap the duplicate is embedded");
        let (a, b) = (id_of(&store, "a.txt"), id_of(&store, "b.txt"));
        assert_eq!(store.info(a).unwrap().get(CONTENT_HASH), None);
        assert_eq!(store.info(b).unwrap().get(CONTENT_HASH), None);
    }

    #[test]
    fn option_off_disables_the_shortcut() {
        let (root, mut store, mut fake) = dedup_setup("off");
        let opts = IndexOptions { dedup_by_content: false, ..IndexOptions::default() };
        std::fs::write(root.join("a.txt"), NOTE).unwrap();
        index(&root, &mut store, &mut fake, &opts);
        let after_a = fake.texts.len();
        std::fs::write(root.join("b.txt"), NOTE).unwrap();
        index(&root, &mut store, &mut fake, &opts);
        assert!(fake.texts.len() > after_a, "with the option off the duplicate is embedded");
        let b = id_of(&store, "b.txt");
        assert_eq!(store.info(b).unwrap().get(CONTENT_HASH), None);
    }

    #[test]
    fn moved_file_is_moved_not_reembedded() {
        let (root, mut store, mut fake) = dedup_setup("move");
        let opts = IndexOptions::default();
        std::fs::write(root.join("a.txt"), NOTE).unwrap();
        index(&root, &mut store, &mut fake, &opts);
        let (a, before) = (id_of(&store, "a.txt"), fake.texts.len());
        std::fs::rename(root.join("a.txt"), root.join("moved.txt")).unwrap();
        index(&root, &mut store, &mut fake, &opts);
        assert_eq!(fake.texts.len(), before, "a move must not embed again");
        assert_eq!(id_of(&store, "moved.txt"), a);
        assert_eq!(files_matching(&store, "penguins"), vec![a]);
    }
}
