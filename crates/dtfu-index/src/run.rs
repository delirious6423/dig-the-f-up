//! The pipeline loop: prepare on all cores (rayon, bounded channel) while one thread embeds and commits.
//! Port of Swift `Indexer.run` / `finishLongFiles` / `process` (Indexer.swift:139-260), with more prepare parallelism.

use crate::commit::{commit_unless_stopped, CONTENT_HASH, CONTENT_SIZE};
use crate::options::IndexOptions;
use crate::prepare::prepare;
use crate::sync::MTIME_TOLERANCE;
use crate::types::{Embed, Job, Leave, Ocr, Prepared};
use anyhow::Result;
use dtfu_store::index::{Adoption, FileRecord, IndexStore, Mismatch};
use rayon::prelude::*;
use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::Path;
use std::sync::mpsc::{sync_channel, RecvTimeoutError};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, RwLock};
use std::time::{Duration, Instant, UNIX_EPOCH};

/// One file (or one step of a long file) finished.
#[derive(Debug, Clone)]
pub struct Event {
    pub done: usize,
    pub total: usize,
    pub path: String,
    pub kind: dtfu_core::FileKind,
    /// `done`, `partial`, `skipped`, `failed`.
    pub state: String,
    pub note: Option<String>,
    pub seconds: f64,
}

/// Records or checks the vector source and settings fingerprint (Swift `checkModel`). With `reset`, a mismatch empties
/// the vectors and requeues every file; without, it is an error.
pub fn check_model(store: &mut IndexStore, embedder: &dyn Embed, opts: &IndexOptions, code: bool, reset: bool) -> Result<Adoption> {
    let source = embedder.vector_source();
    let options = if code { opts.code_fingerprint() } else { opts.fingerprint() };
    // Indexer.swift:281 stores "{vector_source} | {options}".
    let fingerprint = format!("{source} | {options}");
    // Earlier builds of this port stored the options part alone: same model and settings, so upgrade it in place.
    if store.meta("vector_source")?.as_deref() == Some(source.as_str()) && store.meta("fingerprint")?.as_deref() == Some(options.as_str()) {
        store.set_meta("fingerprint", Some(&fingerprint))?;
    }
    store.adopt(&source, &fingerprint, if reset { Mismatch::Reset } else { Mismatch::Refuse })
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
    run_until(store, embedder, ocr, opts, limit, only, should_stop, should_stop, on_event)
}

/// [`run`] with two kinds of stop. `should_stop` (a sync request, pause, quit) ends the run between files;
/// `hard_stop` (pause, quit) also abandons a long audio or video file mid-embedding. Keeping the two apart stops
/// frequent sync requests in a busy folder from restarting a long file forever.
#[allow(clippy::too_many_arguments)]
pub fn run_until(
    store: &mut IndexStore,
    embedder: &mut dyn Embed,
    ocr: &dyn Ocr,
    opts: &IndexOptions,
    limit: Option<usize>,
    only: Option<&[i64]>,
    should_stop: &dyn Fn() -> bool,
    hard_stop: &dyn Fn() -> bool,
    on_event: &mut dyn FnMut(&Event),
) -> Result<BTreeMap<String, usize>> {
    let mut pending = store.pending(limit, only)?;
    prioritise(&mut pending, opts);
    let total = pending.len();
    if !opts.dedup_by_content {
        let jobs = pending.into_iter().map(|file| Job { file, step: None, content_hash: None }).collect();
        return process(store, embedder, ocr, opts, jobs, (0, total), should_stop, hard_stop, on_event).map(|(states, _)| states);
    }
    let plan = plan_duplicates(store, opts, pending, should_stop)?;
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
    let (pass, n) = process(store, embedder, ocr, opts, jobs, (done, total), should_stop, hard_stop, on_event)?;
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
        let (pass, _) = process(store, embedder, ocr, opts, rest, (done, total), should_stop, hard_stop, on_event)?;
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
        let size = if file.kind == dtfu_core::FileKind::Pdf { opts.pdf_step } else { opts.doc_step }.max(1);
        // Where it stopped: `read_to`, else the last page or chunk stored (`lastPlace`, Indexer.swift:181), else the start.
        let stored = info.get("read_to").and_then(|v| v.parse::<usize>().ok());
        let mut start = match stored {
            Some(at) => at,
            None => store.last_place(file.id)?.map_or(0, |v| v as usize),
        };
        let end = info.get("read_end").and_then(|v| v.parse::<usize>().ok()).unwrap_or(0).max(start + 1);
        while start < end {
            jobs.push(Job { file: file.clone(), step: Some(start..(start + size).min(end)), content_hash: None });
            start += size;
        }
    }
    let total = jobs.len();
    process(store, embedder, ocr, opts, jobs, (0, total), should_stop, should_stop, on_event).map(|(states, _)| states)
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
fn plan_duplicates(store: &IndexStore, opts: &IndexOptions, pending: Vec<FileRecord>, should_stop: &dyn Fn() -> bool) -> Result<Plan> {
    let eligible = |f: &FileRecord| f.kind != dtfu_core::FileKind::Code && f.size >= 0 && (f.size as u64) <= opts.dedup_max_bytes;
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
        if should_stop() {
            break; // the files not planned yet stay pending
        }
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

/// How long one file may spend in prepare. A share that stops answering blocks reads for minutes or forever and threads
/// cannot be cancelled, so a file that exceeds this is left pending (`Unavailable`) and its helper thread abandoned.
pub const PREPARE_DEADLINE: Duration = Duration::from_secs(120);
/// Time spent in OCR does not count toward `PREPARE_DEADLINE` (OCR reads pixels already in memory, and LightOnOCR
/// serves one image at a time, so a screenshot can wait minutes behind others on a slow CPU), up to this much.
const OCR_ALLOWANCE: Duration = Duration::from_secs(15 * 60);
/// Abandoned (still blocked) helper threads allowed at once; at this many the run stops preparing new files.
const MAX_LEAKED: usize = 4;
/// How often a waiting run checks for a stop (pause, quit, a new sync).
const CANCEL_POLL: Duration = Duration::from_millis(250);

/// The part of the path that names the drive or share: the first two normal components ("/Volumes/X").
fn volume_key(path: &str) -> String {
    #[cfg(test)]
    if let Some(at) = path.find("/vol-") {
        let end = path[at + 1..].find('/').map_or(path.len(), |i| at + 1 + i);
        return path[..end].to_string();
    }
    let parent = Path::new(path).parent().unwrap_or(Path::new(""));
    let mut key = std::path::PathBuf::new();
    let mut normal = 0;
    for c in parent.components() {
        if matches!(c, std::path::Component::Normal(_)) {
            if normal == 2 {
                break;
            }
            normal += 1;
        }
        key.push(c.as_os_str());
    }
    key.to_string_lossy().into_owned()
}

/// How long one helper has spent in OCR (finished calls plus the one in progress). One short-held lock keeps the
/// two fields consistent for the waiter (never held during OCR itself).
#[derive(Default)]
struct OcrClock(Mutex<(Duration, Option<Instant>)>);
impl OcrClock {
    fn enter(&self) {
        self.0.lock().unwrap_or_else(|e| e.into_inner()).1 = Some(Instant::now());
    }
    fn leave(&self) {
        let mut g = self.0.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(since) = g.1.take() {
            g.0 += since.elapsed();
        }
    }
    /// Time in OCR so far, and whether a call is in progress.
    fn spent(&self) -> (Duration, bool) {
        let g = self.0.lock().unwrap_or_else(|e| e.into_inner());
        (g.0 + g.1.map_or(Duration::ZERO, |s| s.elapsed()), g.1.is_some())
    }
}

thread_local! {
    /// The clock of the prepare running on this helper thread (set by `Guard::prepare`).
    static OCR_CLOCK: std::cell::RefCell<Option<Arc<OcrClock>>> = const { std::cell::RefCell::new(None) };
}

/// Waits for the helper's result. Time the helper spends in OCR is not counted, up to `OCR_ALLOWANCE`.
/// Gives up (`None`) at the deadline, or within a quarter second once `cancel` is set.
fn wait_excluding_ocr<T>(
    rx: &std::sync::mpsc::Receiver<T>,
    deadline: Duration,
    clock: &OcrClock,
    cancel: &AtomicBool,
) -> Option<T> {
    let start = Instant::now();
    loop {
        if cancel.load(Ordering::SeqCst) {
            return None;
        }
        let (spent, in_ocr) = clock.spent();
        let used = start.elapsed().saturating_sub(spent.min(OCR_ALLOWANCE));
        let left = deadline.checked_sub(used).filter(|d| !d.is_zero())?;
        // Short steps, so `cancel` is seen; while OCR runs `left` does not shrink, so never spin on a tiny remainder.
        let step = if in_ocr { CANCEL_POLL } else { left.min(CANCEL_POLL) };
        match rx.recv_timeout(step) {
            Ok(v) => return Some(v),
            Err(RecvTimeoutError::Disconnected) => return None,
            Err(RecvTimeoutError::Timeout) => {}
        }
    }
}

/// What helper threads call for OCR. With an owned engine (`Ocr::shared`) the run never waits for them. A borrowed
/// one is lent as a raw `'static` reference: once the run is over `alive` is false and it is never touched again
/// (the write lock waits for calls in flight).
struct OcrGate {
    owned: Option<Arc<dyn Ocr + Send + Sync>>,
    borrowed: Option<&'static dyn Ocr>,
    alive: RwLock<bool>,
}
impl Ocr for OcrGate {
    fn recognize(&self, width: usize, height: usize, rgb: &[u8]) -> Option<String> {
        let clock = OCR_CLOCK.with(|c| c.borrow().clone());
        if let Some(c) = &clock {
            c.enter(); // before the lock: waiting for it is OCR time too
        }
        let out = if let Some(ocr) = &self.owned {
            ocr.recognize(width, height, rgb)
        } else {
            match (self.alive.read(), self.borrowed) {
                (Ok(alive), Some(ocr)) if *alive => ocr.recognize(width, height, rgb),
                _ => None,
            }
        };
        if let Some(c) = &clock {
            c.leave();
        }
        out
    }
}
struct CloseGate(Arc<OcrGate>);
impl Drop for CloseGate {
    fn drop(&mut self) {
        if self.0.borrowed.is_some() {
            *self.0.alive.write().unwrap_or_else(|e| e.into_inner()) = false;
        }
    }
}

/// Process-wide registry of in-flight helper threads. It outlives any one `process()` call because abandoned helpers do.
#[derive(Default)]
struct Registry {
    next: u64,
    /// Helper id -> (volume key, past its deadline and still not returned).
    helpers: HashMap<u64, (String, bool)>,
}
static REGISTRY: Mutex<Option<Registry>> = Mutex::new(None);

fn with_registry<R>(f: impl FnOnce(&mut Registry) -> R) -> R {
    let mut guard = REGISTRY.lock().unwrap_or_else(|e| e.into_inner());
    f(guard.get_or_insert_with(Registry::default))
}

/// Owned by a helper thread; dropping it (also when the helper panics) removes the helper from the registry.
struct Permit(u64);
impl Drop for Permit {
    fn drop(&mut self) {
        let id = self.0;
        with_registry(|r| {
            r.helpers.remove(&id);
        });
    }
}

/// Admits a helper for `key` unless a hung helper already holds that volume or `MAX_LEAKED` helpers are hung.
/// Check and registration happen under one lock.
fn admit(key: &str) -> Option<Permit> {
    with_registry(|r| {
        let hung = r.helpers.values().filter(|(_, h)| *h).count();
        if hung >= MAX_LEAKED || r.helpers.values().any(|(k, h)| *h && k == key) {
            return None;
        }
        r.next += 1;
        let id = r.next;
        r.helpers.insert(id, (key.to_string(), false));
        Some(Permit(id))
    })
}

fn mark_hung(id: u64) {
    with_registry(|r| {
        if let Some(entry) = r.helpers.get_mut(&id) {
            entry.1 = true;
        }
    });
}

fn hung_count() -> usize {
    with_registry(|r| r.helpers.values().filter(|(_, h)| *h).count())
}

/// Prepare with a deadline, and the bookkeeping of abandoned helper threads (see `PREPARE_DEADLINE`).
struct Guard {
    deadline: Duration,
    opts: Arc<IndexOptions>,
    gate: Arc<OcrGate>,
    /// Set when the run stops: files not started are skipped, and waits for files in progress end at once (their
    /// helper threads finish in the background and their results are dropped, never committed).
    cancel: AtomicBool,
}

impl Guard {
    fn prepare(&self, job: &Job) -> Prepared {
        let unavailable = |msg: String| Prepared { job: job.clone(), result: Err(Leave::Unavailable(msg)) };
        let key = volume_key(&job.file.path);
        #[cfg(test)]
        if job.file.path.contains("/hang-late-") {
            // Arrive after the first hang timed out: wait for it, not a fixed time (the pool may start this job first).
            let until = Instant::now() + Duration::from_secs(5);
            while !with_registry(|r| r.helpers.values().any(|(k, h)| *h && *k == key)) && Instant::now() < until {
                std::thread::sleep(Duration::from_millis(20));
            }
        }
        let Some(permit) = admit(&key) else {
            return unavailable("drive not responding".into());
        };
        let id = permit.0;
        let (tx, rx) = std::sync::mpsc::channel();
        let (owned, opts, gate) = (job.clone(), self.opts.clone(), self.gate.clone());
        let clock = Arc::new(OcrClock::default());
        let helper_clock = clock.clone();
        let spawned = std::thread::Builder::new().name("dtfu-prepare".into()).stack_size(8 << 20).spawn(move || {
            let _permit = permit; // released on return and on panic
            OCR_CLOCK.with(|c| *c.borrow_mut() = Some(helper_clock));
            #[cfg(test)]
            test_hang(&owned);
            let prepared = prepare(&owned, &opts, &*gate);
            let _ = tx.send(prepared); // the waiter may be gone: then the result is dropped, never committed
        });
        if spawned.is_err() {
            return unavailable("cannot start a reader thread".into()); // the closure (and its permit) is dropped
        }
        match wait_excluding_ocr(&rx, self.deadline, &clock, &self.cancel) {
            Some(prepared) => prepared,
            None if self.cancel.load(Ordering::SeqCst) => {
                // Abandoned but possibly still running (ffmpeg, PDF rendering): it counts toward `MAX_LEAKED` and holds
                // its volume until it finishes, so a quick Resume does not stack new work on top of it.
                mark_hung(id);
                unavailable("stopped".into())
            }
            None => {
                mark_hung(id); // no-op when the helper already finished
                unavailable(format!("timed out reading {}", job.file.path))
            }
        }
    }
}

#[cfg(test)]
static HANG_STARTED: Mutex<Vec<String>> = Mutex::new(Vec::new());
/// Test hook: a file named `hang-*` blocks its prepare for 3 s, like a dead share.
#[cfg(test)]
fn test_hang(job: &Job) {
    let name = Path::new(&job.file.path).file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
    if name.starts_with("hang-") {
        HANG_STARTED.lock().unwrap().push(job.file.path.clone());
        if !name.starts_with("hang-late-") {
            std::thread::sleep(Duration::from_secs(3));
        }
    }
}

fn deadline_for(_jobs: &[Job]) -> Duration {
    #[cfg(test)]
    if _jobs.iter().any(|j| j.file.path.contains("/hang-")) {
        return Duration::from_millis(200);
    }
    PREPARE_DEADLINE
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
    hard_stop: &dyn Fn() -> bool,
    on_event: &mut dyn FnMut(&Event),
) -> Result<(BTreeMap<String, usize>, usize)> {
    let (mut done, total) = progress;
    let mut states: BTreeMap<String, usize> = BTreeMap::new();
    let mut finished: HashSet<i64> = HashSet::new();
    let batch = (rayon::current_num_threads() * 2).max(4);
    let (tx, rx) = sync_channel::<Prepared>(batch);
    let owned = ocr.shared();
    // SAFETY: the borrowed `ocr` is only reachable through `gate`, which `_close` shuts before this function returns.
    let borrowed: Option<&'static dyn Ocr> =
        if owned.is_some() { None } else { Some(unsafe { std::mem::transmute::<&dyn Ocr, &'static dyn Ocr>(ocr) }) };
    let gate = Arc::new(OcrGate { owned, borrowed, alive: RwLock::new(true) });
    let _close = CloseGate(gate.clone());
    let guard = Guard { deadline: deadline_for(&jobs), opts: Arc::new(opts.clone()), gate, cancel: AtomicBool::new(false) };
    let mut outcome: Result<()> = Ok(());
    std::thread::scope(|scope| {
        let jobs_ref = &jobs;
        let guard = &guard;
        scope.spawn(move || {
            // Chunks keep the order (steps of one file must commit in order); the channel bounds memory.
            for chunk in jobs_ref.chunks(batch) {
                if hung_count() >= MAX_LEAKED {
                    return; // too many drives not answering: end the run, the rest stays pending
                }
                let prepared: Vec<Prepared> = chunk
                    .par_iter()
                    .map(|job| {
                        if guard.cancel.load(Ordering::SeqCst) {
                            Prepared { job: job.clone(), result: Err(Leave::Unavailable("stopped".into())) }
                        } else {
                            guard.prepare(job)
                        }
                    })
                    .collect();
                for p in prepared {
                    if tx.send(p).is_err() {
                        return;
                    }
                }
            }
        });
        // Polls, so a stop is seen while a batch is still being prepared (a long video or OCR can take minutes).
        loop {
            let prepared = match rx.recv_timeout(CANCEL_POLL) {
                Ok(p) => p,
                Err(RecvTimeoutError::Timeout) if !should_stop() => continue,
                Err(_) => break,
            };
            if should_stop() {
                break;
            }
            let file = prepared.job.file.clone();
            if finished.contains(&file.id) && prepared.job.step.is_some() {
                done += 1;
                continue;
            }
            let started = Instant::now();
            let unavailable = matches!(prepared.result, Err(Leave::Unavailable(_)));
            if let Err(e) = commit_unless_stopped(store, embedder, prepared, opts, hard_stop) {
                outcome = Err(e);
                break;
            }
            let (mut state, mut note) = match settled_state(store, file.id) {
                Ok(v) => v,
                Err(e) => {
                    outcome = Err(e);
                    break;
                }
            };
            if state == "pending" && !unavailable && hard_stop() {
                break; // stopped inside a long file: it stays pending, no event
            }
            if state == "pending" || unavailable {
                // Commit left the row alone (folder unavailable; a `partial` row stays `partial`): not settled, so it is
                // retried in a later run. Its remaining steps are skipped for the rest of this run, and it is not
                // counted as done or failed.
                state = "unavailable".into();
                note = Some("folder unavailable, stays pending".into());
                finished.insert(file.id);
                count(&mut states, &state);
            } else if state != "partial" {
                // A file that ends sooner than its steps said (length was a guess, or it went unreadable) skips the rest.
                finished.insert(file.id);
                count(&mut states, &state);
            }
            done += 1;
            on_event(&Event { done, total, path: file.path, kind: file.kind, state, note, seconds: started.elapsed().as_secs_f64() });
        }
        guard.cancel.store(true, Ordering::SeqCst); // the producer stops preparing; nothing more is committed
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

    fn record(id: i64, path: &str, kind: dtfu_core::FileKind) -> FileRecord {
        FileRecord {
            id,
            path: path.into(),
            kind,
            size: 1,
            mtime: 0.0,
            inode: 0,
            device: 0,
            state: dtfu_core::FileState::Pending,
        }
    }

    #[test]
    fn priority_reorders_inside_a_kind_only() {
        use dtfu_core::FileKind::{Doc, Pdf};
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
        let d = std::env::temp_dir().join(format!("dtfu-run-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn fingerprint_is_source_then_options_and_bare_form_upgrades() {
        let dir = temp("fp");
        let mut store = IndexStore::open(&dir, "index.sqlite").unwrap();
        let fake = FakeEmbedder::default();
        let opts = IndexOptions::default();
        let want = format!("{} | {}", fake.vector_source(), opts.fingerprint());
        assert_eq!(check_model(&mut store, &fake, &opts, false, false).unwrap(), Adoption::Fresh);
        assert_eq!(store.meta("fingerprint").unwrap().as_deref(), Some(want.as_str()));
        // An index written by an earlier build holds the options part alone.
        store.set_meta("fingerprint", Some(&opts.fingerprint())).unwrap();
        assert_eq!(check_model(&mut store, &fake, &opts, false, false).unwrap(), Adoption::Same);
        assert_eq!(store.meta("fingerprint").unwrap().as_deref(), Some(want.as_str()));
    }

    #[test]
    fn indexes_pending_then_finishes_long_doc() {
        let root = temp("e2e");
        let paragraphs: Vec<String> = (0..120).map(|i| format!("Paragraph {i}: ") + &"the quick brown fox jumps. ".repeat(40)).collect();
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

    #[test]
    fn file_whose_folder_vanished_stays_pending_and_the_rest_finish() {
        let root = temp("vanish");
        let vol = root.join("usb");
        std::fs::create_dir_all(&vol).unwrap();
        std::fs::write(vol.join("gone.txt"), "A note on the drive that gets unplugged before its turn.").unwrap();
        std::fs::write(root.join("here.txt"), "A note on the internal disk about tomatoes.").unwrap();
        std::fs::write(root.join("also.txt"), "Another note on the internal disk about basil.").unwrap();
        let dir = temp("store-vanish");
        let mut store = IndexStore::open(&dir, "index.sqlite").unwrap();
        let opts = IndexOptions::default();
        sync(&mut store, std::slice::from_ref(&root), &opts, false).unwrap();
        let mut fake = FakeEmbedder::default();
        check_model(&mut store, &fake, &opts, false, false).unwrap();
        // The drive disappears between sync and run.
        std::fs::remove_dir_all(&vol).unwrap();

        let mut events = Vec::new();
        let states = run(&mut store, &mut fake, &NoOcr, &opts, None, None, &|| false, &mut |e| events.push(e.clone())).unwrap();
        assert_eq!(states.get("unavailable").copied(), Some(1), "{states:?}");
        assert_eq!(states.get("failed").copied(), None, "{states:?}");
        assert_eq!(states.get("done").copied(), Some(2), "{states:?}");
        assert_eq!(events.len(), 3, "{events:?}");
        let state_of = |name: &str| -> String {
            store
                .connection()
                .query_row("SELECT state FROM files WHERE path LIKE ?1", [format!("%{}{name}", std::path::MAIN_SEPARATOR)], |r| r.get(0))
                .unwrap()
        };
        assert_eq!(state_of("gone.txt"), "pending");
        assert_eq!(state_of("here.txt"), "done");
        assert_eq!(state_of("also.txt"), "done");
        assert_eq!(store.pending_count().unwrap(), 1);
    }

    #[test]
    fn hung_prepare_leaves_the_file_pending_and_the_run_finishes() {
        let root = temp("hang");
        for v in ["vol-a", "vol-b"] {
            std::fs::create_dir_all(root.join(v)).unwrap();
        }
        std::fs::write(root.join("vol-a/hang-1.txt"), "A note on a share that stopped answering.").unwrap();
        std::fs::write(root.join("vol-a/hang-late-2.txt"), "Another note on the same dead share.").unwrap();
        std::fs::write(root.join("vol-b/x.txt"), "A note on a healthy disk about tomatoes.").unwrap();
        std::fs::write(root.join("vol-b/y.txt"), "Another note on the healthy disk about basil.").unwrap();
        let mut store = IndexStore::open(&temp("store-hang"), "index.sqlite").unwrap();
        let mut fake = FakeEmbedder::default();
        let opts = IndexOptions::default();
        check_model(&mut store, &fake, &opts, false, false).unwrap();
        sync(&mut store, std::slice::from_ref(&root), &opts, false).unwrap();
        let started = Instant::now();
        let mut events = Vec::new();
        let states = run(&mut store, &mut fake, &NoOcr, &opts, None, None, &|| false, &mut |e| events.push(e.clone())).unwrap();
        assert!(started.elapsed() < Duration::from_millis(2500), "run waited for the hung read: {:?}", started.elapsed());
        assert_eq!(states.get("done").copied(), Some(2), "{states:?}");
        assert_eq!(states.get("unavailable").copied(), Some(2), "{states:?}");
        assert_eq!(states.get("failed").copied(), None, "{states:?}");
        let started_here = HANG_STARTED.lock().unwrap().iter().filter(|p| p.starts_with(root.to_str().unwrap())).count();
        assert_eq!(started_here, 1, "second file of the same drive must not spawn a thread");
        let notes: Vec<_> = events.iter().filter_map(|e| e.note.clone()).collect();
        assert!(notes.iter().any(|n| n.contains("stays pending")), "{notes:?}");
        assert_eq!(store.pending_count().unwrap(), 2);
    }

    #[test]
    fn same_key_files_in_one_chunk_leave_one_hung_helper() {
        let root = temp("hang-chunk");
        std::fs::create_dir_all(root.join("vol-d")).unwrap();
        std::fs::write(root.join("vol-d/hang-1.txt"), "A note on a dead share, first.").unwrap();
        for n in 2..5 {
            std::fs::write(root.join(format!("vol-d/hang-late-{n}.txt")), format!("More notes on the dead share {n}.")).unwrap();
        }
        let mut store = IndexStore::open(&temp("store-hang-chunk"), "index.sqlite").unwrap();
        let mut fake = FakeEmbedder::default();
        let opts = IndexOptions::default();
        check_model(&mut store, &fake, &opts, false, false).unwrap();
        sync(&mut store, std::slice::from_ref(&root), &opts, false).unwrap();
        let key = volume_key(root.join("vol-d/x.txt").to_str().unwrap());
        // Enough threads that the late files waiting for the hang cannot occupy the whole pool and keep hang-1 from
        // starting (CI runners have 3 cores).
        let pool = rayon::ThreadPoolBuilder::new().num_threads(8).build().unwrap();
        pool.install(|| run(&mut store, &mut fake, &NoOcr, &opts, None, None, &|| false, &mut |_| {})).unwrap();
        // At most one: on a slow machine the hung helper's (test) sleep may already have ended and released it.
        let hung_for_key = with_registry(|r| r.helpers.values().filter(|(k, h)| *h && *k == key).count());
        assert!(hung_for_key <= 1, "{hung_for_key} hung helpers for one drive");
        let started_here = HANG_STARTED.lock().unwrap().iter().filter(|p| p.starts_with(root.to_str().unwrap())).count();
        assert_eq!(started_here, 1);
        assert_eq!(store.pending_count().unwrap(), 4);
    }

    #[test]
    fn ocr_time_does_not_count_toward_the_deadline() {
        let clock = Arc::new(OcrClock::default());
        let (tx, rx) = std::sync::mpsc::channel();
        let c = clock.clone();
        std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(50));
            c.enter();
            std::thread::sleep(Duration::from_millis(400)); // OCR, longer than the deadline
            c.leave();
            std::thread::sleep(Duration::from_millis(50));
            let _ = tx.send(7);
        });
        assert_eq!(wait_excluding_ocr(&rx, Duration::from_millis(300), &clock, &AtomicBool::new(false)), Some(7));
        assert!(clock.spent().0 >= Duration::from_millis(400), "{:?}", clock.spent());
    }

    #[test]
    fn time_outside_ocr_still_times_out() {
        let clock = OcrClock::default();
        let (_tx, rx) = std::sync::mpsc::channel::<u8>();
        let t = Instant::now();
        assert_eq!(wait_excluding_ocr(&rx, Duration::from_millis(150), &clock, &AtomicBool::new(false)), None);
        assert!(t.elapsed() < Duration::from_millis(1000), "{:?}", t.elapsed());
    }

    #[test]
    fn cancel_ends_the_wait_within_a_poll() {
        let clock = Arc::new(OcrClock::default());
        clock.enter(); // in OCR: the deadline alone would never end this wait
        let (_tx, rx) = std::sync::mpsc::channel::<u8>();
        let cancel = Arc::new(AtomicBool::new(false));
        let c = cancel.clone();
        std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(100));
            c.store(true, Ordering::SeqCst);
        });
        let t = Instant::now();
        assert_eq!(wait_excluding_ocr(&rx, Duration::from_secs(60), &clock, &cancel), None);
        assert!(t.elapsed() < Duration::from_secs(1), "{:?}", t.elapsed());
    }

    #[test]
    fn helper_panic_releases_its_permit() {
        let key = "/vol-panic-test";
        let permit = admit(key).unwrap();
        let id = permit.0;
        let t = std::thread::spawn(move || {
            let _permit = permit;
            panic!("boom");
        });
        assert!(t.join().is_err());
        assert!(with_registry(|r| !r.helpers.contains_key(&id)));
        assert!(admit(key).is_some());
    }

    #[test]
    fn hung_helper_blocks_only_its_own_key() {
        let a = admit("/vol-block-a").unwrap();
        let healthy = admit("/vol-block-a").unwrap(); // in flight but not hung: still admitted
        mark_hung(a.0);
        assert!(admit("/vol-block-a").is_none());
        assert!(admit("/vol-block-b").is_some());
        drop(a);
        assert!(admit("/vol-block-a").is_some());
        drop(healthy);
    }

    #[test]
    fn stop_during_planning_leaves_files_pending() {
        let (root, mut store, mut fake) = dedup_setup("plan-stop");
        let opts = IndexOptions::default();
        for n in ["a.txt", "b.txt", "c.txt"] {
            std::fs::write(root.join(n), NOTE).unwrap();
        }
        sync(&mut store, std::slice::from_ref(&root), &opts, false).unwrap();
        let pending = store.pending(None, None).unwrap();
        let hashed = std::cell::Cell::new(0);
        let plan = plan_duplicates(&store, &opts, pending.clone(), &|| {
            hashed.set(hashed.get() + 1);
            hashed.get() > 1
        })
        .unwrap();
        assert_eq!(plan.jobs.len() + plan.copies.len() + plan.twins.len(), 1);
        let states = run(&mut store, &mut fake, &NoOcr, &opts, None, None, &|| true, &mut |_| {}).unwrap();
        assert!(states.is_empty(), "{states:?}");
        assert_eq!(store.pending_count().unwrap(), pending.len());
    }

    /// An owned OCR engine whose calls block for 3 s.
    struct SlowOcr;
    impl Ocr for SlowOcr {
        fn recognize(&self, _: usize, _: usize, _: &[u8]) -> Option<String> {
            std::thread::sleep(Duration::from_secs(3));
            None
        }
        fn shared(&self) -> Option<Arc<dyn Ocr + Send + Sync>> {
            Some(Arc::new(SlowOcr))
        }
    }

    #[test]
    fn process_returns_promptly_while_ocr_blocks() {
        let dir = temp("slow-ocr");
        let path = dir.join("shot.png");
        image::RgbImage::from_fn(600, 400, |x, y| image::Rgb([(x % 256) as u8, (y % 256) as u8, 128])).save(&path).unwrap();
        let mut store = IndexStore::open(&temp("slow-ocr-store"), "index.sqlite").unwrap();
        let mut fake = FakeEmbedder::default();
        let job = Job { file: record(1, path.to_str().unwrap(), dtfu_core::FileKind::Screenshot), step: None, content_hash: None };
        let t = Instant::now();
        let stop = || t.elapsed() > Duration::from_millis(400);
        process(&mut store, &mut fake, &SlowOcr, &IndexOptions::default(), vec![job], (0, 1), &stop, &stop, &mut |_| {}).unwrap();
        assert!(t.elapsed() < Duration::from_secs(1), "{:?}", t.elapsed());
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
        store.connection().query_row("SELECT id FROM files WHERE path LIKE ?1", [format!("%{}{name}", std::path::MAIN_SEPARATOR)], |r| r.get(0)).unwrap()
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
