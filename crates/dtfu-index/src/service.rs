//! The long-running scheduler: sync, new files first, backfill when allowed, code second, long files last.
//! Port of Swift `IndexService.run` (docs/porting/spec-pipeline.md section 2.7). A library: the app or CLI drives it with a
//! `Control` handle from another thread and receives `ServiceEvent`s through a callback.

use crate::options::IndexOptions;
use crate::run::{self, check_model, finish_long_files};
use crate::sync::{sync, SyncSummary};
use crate::types::{Embed, Ocr};
use anyhow::Result;
use dtfu_core::{FileKind, SegmentKind};
use dtfu_store::index::IndexStore;
use std::path::PathBuf;
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

/// One index (the main one, or the code one) with the folders it covers.
pub struct Part {
    pub store: IndexStore,
    pub roots: Vec<PathBuf>,
    pub code: bool,
    /// At startup, delete rows outside `roots`.
    pub keep_only: bool,
}

/// Returned (as an `anyhow` error) by the embedder factory when the model is not downloaded or loaded yet.
#[derive(Debug)]
pub struct NotReady;

/// `meta` key: [`OCR_MISSED`] after a run found OCR unable to read, [`OCR_DONE`] once screenshots lacking OCR text
/// were queued again with OCR working.
const OCR_BACKFILL_KEY: &str = "ocr_backfill";
const OCR_MISSED: &str = "missed";
const OCR_DONE: &str = "done";
/// `meta` key: [`OCR_DONE`] once screenshots were queued again after LightOnOCR's models were installed.
const OCR_LIGHTON_KEY: &str = "ocr_lighton";
/// Once LightOnOCR can run, OCR screenshots again, once per index. Off macOS every finished screenshot (`ocrs` turns
/// non-Latin text into wrong Latin text). On macOS only those without OCR text (Vision found nothing); Vision's
/// low-confidence misreads cannot be told apart afterwards and stay until the file changes (owner's choice: cost).
fn lighton_backfill(store: &mut IndexStore, ocr: &dyn Ocr) -> Result<()> {
    if ocr.best_ready() && store.meta(OCR_LIGHTON_KEY)?.as_deref() != Some(OCR_DONE) {
        if cfg!(target_os = "macos") {
            store.requeue_lacking(FileKind::Screenshot, SegmentKind::Ocr)?;
        } else {
            store.requeue_kind(FileKind::Screenshot)?;
        }
        store.set_meta(OCR_LIGHTON_KEY, Some(OCR_DONE))?;
    }
    Ok(())
}

impl std::fmt::Display for NotReady {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "the model is not ready")
    }
}
impl std::error::Error for NotReady {}

#[derive(Debug)]
pub enum ServiceEvent {
    /// `keep_only` removed this many rows outside the roots.
    Pruned { code: bool, removed: usize },
    Sync { code: bool, summary: SyncSummary },
    /// A file finished in a normal pass (main or code index).
    Progress(run::Event),
    /// A step of a long file finished in the later pass.
    Rest(run::Event),
    /// The model is not ready; file names are searchable, the service returns.
    Waiting,
    /// A sync failed (an unreadable root, a full disk); the service goes on.
    Error(String),
    Idle,
    Stopped,
}

#[derive(Default)]
struct Flags {
    sync: bool,
    code_sync: bool,
    stop: bool,
    backfill: bool,
    woken: bool,
}

/// Thread-safe handle to steer a running `Service`.
#[derive(Clone)]
pub struct Control {
    inner: Arc<(Mutex<Flags>, Condvar)>,
}

impl Default for Control {
    fn default() -> Self {
        Self::new(false)
    }
}

impl Control {
    /// Both syncs are requested initially.
    pub fn new(backfill_allowed: bool) -> Self {
        let flags = Flags { sync: true, code_sync: true, backfill: backfill_allowed, ..Flags::default() };
        Self { inner: Arc::new((Mutex::new(flags), Condvar::new())) }
    }
    fn with<R>(&self, f: impl FnOnce(&mut Flags) -> R) -> R {
        let (lock, cv) = &*self.inner;
        let mut g = lock.lock().unwrap_or_else(|e| e.into_inner());
        let r = f(&mut g);
        cv.notify_all();
        r
    }
    fn read<R>(&self, f: impl FnOnce(&Flags) -> R) -> R {
        let g = self.inner.0.lock().unwrap_or_else(|e| e.into_inner());
        f(&g)
    }
    pub fn request_sync(&self) {
        self.with(|f| f.sync = true);
    }
    pub fn request_code_sync(&self) {
        self.with(|f| f.code_sync = true);
    }
    /// The running pass stops after its current job; the service returns.
    pub fn stop(&self) {
        self.with(|f| f.stop = true);
    }
    pub fn set_backfill_allowed(&self, allowed: bool) {
        self.with(|f| {
            if f.backfill != allowed {
                f.backfill = allowed;
                f.woken = true;
            }
        });
    }
    /// Ends an idle wait (the loop re-checks everything).
    pub fn wake(&self) {
        self.with(|f| f.woken = true);
    }
    fn take_sync(&self) -> bool {
        self.with(|f| std::mem::take(&mut f.sync))
    }
    fn take_code_sync(&self) -> bool {
        self.with(|f| std::mem::take(&mut f.code_sync))
    }
    fn stopped(&self) -> bool {
        self.read(|f| f.stop)
    }
    fn backfill_allowed(&self) -> bool {
        self.read(|f| f.backfill)
    }
    fn any_request(&self, with_code: bool) -> bool {
        self.read(|f| f.sync || f.stop || (with_code && f.code_sync))
    }
    /// Waits for a request, a wake, or `linger`. Returns false on timeout.
    fn wait(&self, linger: Duration, with_code: bool) -> bool {
        let (lock, cv) = &*self.inner;
        let deadline = Instant::now() + linger;
        let mut g = lock.lock().unwrap_or_else(|e| e.into_inner());
        loop {
            if g.sync || (with_code && g.code_sync) || g.stop || g.woken {
                g.woken = false;
                return true;
            }
            let now = Instant::now();
            if now >= deadline {
                return false;
            }
            g = cv.wait_timeout(g, deadline - now).unwrap_or_else(|e| e.into_inner()).0;
        }
    }
}

pub struct Service {
    main: Part,
    code: Option<Part>,
    opts: IndexOptions,
    control: Control,
}

fn root_strings(roots: &[PathBuf]) -> Vec<String> {
    roots.iter().map(|r| crate::sync::root_string(r)).collect()
}

/// Roots that exist now (unplugged drives drop out); the store scope follows them.
fn available_roots(part: &mut Part) -> Vec<PathBuf> {
    let available: Vec<PathBuf> = part.roots.iter().filter(|r| r.is_dir()).cloned().collect();
    part.store.set_scope(Some(root_strings(&available)));
    available
}

impl Service {
    pub fn new(main: Part, code: Option<Part>, opts: IndexOptions, control: Control) -> Self {
        Self { main, code, opts, control }
    }

    pub fn main(&self) -> &Part {
        &self.main
    }
    pub fn code(&self) -> Option<&Part> {
        self.code.as_ref()
    }

    /// Runs until `stop()`, until the model is not ready, or (after `Idle`) until `linger` passes without work
    /// (`None`: returns at the first idle). `make_embedder` is called the first time work needs the model; an error
    /// whose cause is `NotReady` emits `Waiting` and returns `Ok`.
    pub fn run(
        &mut self,
        make_embedder: &mut dyn FnMut() -> Result<Box<dyn Embed>>,
        ocr: &dyn Ocr,
        linger: Option<Duration>,
        on_event: &mut dyn FnMut(ServiceEvent),
    ) -> Result<()> {
        let control = self.control.clone();
        let has_code = self.code.is_some();
        let opts = self.opts.clone();
        let limit = opts.new_files_limit;
        let mut embedder: Option<Box<dyn Embed>> = None;
        let mut new_files: Vec<i64> = Vec::new();
        let mut new_code: Vec<i64> = Vec::new();

        for part in std::iter::once(&mut self.main).chain(self.code.as_mut()) {
            available_roots(part);
            if part.keep_only {
                let removed = part.store.prune(&root_strings(&part.roots))?;
                on_event(ServiceEvent::Pruned { code: part.code, removed });
            }
        }
        // Screenshots indexed while OCR could not read (Windows and Linux had no OCR models before they were
        // downloaded automatically) have no OCR text: once OCR works, queue them again, once per index. On macOS
        // Vision always worked, so screenshots without OCR text there simply have none and are left alone.
        let store = &mut self.main.store;
        if !ocr.ready() {
            store.set_meta(OCR_BACKFILL_KEY, Some(OCR_MISSED))?;
        } else {
            match store.meta(OCR_BACKFILL_KEY)?.as_deref() {
                Some(OCR_DONE) => {}
                missed => {
                    if missed == Some(OCR_MISSED) || !cfg!(target_os = "macos") {
                        store.requeue_lacking(FileKind::Screenshot, SegmentKind::Ocr)?;
                    }
                    store.set_meta(OCR_BACKFILL_KEY, Some(OCR_DONE))?;
                }
            }
        }
        lighton_backfill(store, ocr)?;

        loop {
            if control.take_sync() {
                let roots = available_roots(&mut self.main);
                let summary = match sync(&mut self.main.store, &roots, &opts, false) {
                    Ok(s) => s,
                    Err(e) => {
                        on_event(ServiceEvent::Error(format!("sync failed: {e:#}")));
                        continue;
                    }
                };
                if summary.queued.len() <= limit {
                    for id in &summary.queued {
                        if !new_files.contains(id) {
                            new_files.push(*id);
                        }
                    }
                }
                on_event(ServiceEvent::Sync { code: false, summary });
            }
            if control.stopped() {
                break;
            }
            if embedder.is_none() {
                match make_embedder() {
                    Ok(e) => {
                        for part in std::iter::once(&mut self.main).chain(self.code.as_mut()) {
                            // Like the Swift index service (main.swift:409 resetOnModelChange): a new model or settings reindexes.
                            check_model(&mut part.store, e.as_ref(), &opts, part.code, true)?;
                        }
                        embedder = Some(e);
                    }
                    Err(e) if e.downcast_ref::<NotReady>().is_some() => {
                        on_event(ServiceEvent::Waiting);
                        return Ok(());
                    }
                    Err(e) => return Err(e),
                }
            }
            let emb: &mut dyn Embed = embedder.as_mut().expect("embedder set above").as_mut();

            if !new_files.is_empty() {
                let c = &control;
                run::run(&mut self.main.store, emb, ocr, &opts, None, Some(&new_files), &|| c.any_request(false), &mut |e| {
                    on_event(ServiceEvent::Progress(e.clone()))
                })?;
                // Leftovers stay "new" only when the run was interrupted; otherwise they wait for the backfill gate.
                new_files = if c.any_request(false) { self.main.store.pending(None, Some(&new_files))?.into_iter().map(|f| f.id).collect() } else { Vec::new() };
                continue;
            }
            // LightOn's models may arrive (downloaded in the background) while the service runs.
            lighton_backfill(&mut self.main.store, ocr)?;
            if self.main.store.pending_count()? > 0 && control.backfill_allowed() {
                let c = &control;
                run::run(&mut self.main.store, emb, ocr, &opts, None, None, &|| c.any_request(false) || !c.backfill_allowed(), &mut |e| {
                    on_event(ServiceEvent::Progress(e.clone()))
                })?;
                if control.any_request(false) {
                    continue;
                }
            }
            if let Some(code) = self.code.as_mut() {
                if self.main.store.pending_count()? == 0 || !control.backfill_allowed() {
                    if control.take_code_sync() {
                        let roots = available_roots(code);
                        let summary = match sync(&mut code.store, &roots, &opts, true) {
                            Ok(s) => s,
                            Err(e) => {
                                on_event(ServiceEvent::Error(format!("code sync failed: {e:#}")));
                                continue;
                            }
                        };
                        if summary.queued.len() <= limit {
                            for id in &summary.queued {
                                if !new_code.contains(id) {
                                    new_code.push(*id);
                                }
                            }
                        }
                        on_event(ServiceEvent::Sync { code: true, summary });
                    }
                    let c = &control;
                    if !new_code.is_empty() {
                        run::run(&mut code.store, emb, ocr, &opts, None, Some(&new_code), &|| c.any_request(has_code), &mut |e| {
                            on_event(ServiceEvent::Progress(e.clone()))
                        })?;
                        new_code = if c.any_request(has_code) { code.store.pending(None, Some(&new_code))?.into_iter().map(|f| f.id).collect() } else { Vec::new() };
                        continue;
                    } else if code.store.pending_count()? > 0 && control.backfill_allowed() {
                        run::run(&mut code.store, emb, ocr, &opts, None, None, &|| c.any_request(has_code) || !c.backfill_allowed(), &mut |e| {
                            on_event(ServiceEvent::Progress(e.clone()))
                        })?;
                        if control.any_request(has_code) {
                            continue;
                        }
                    }
                }
            }
            let code_pending = match self.code.as_ref() {
                Some(c) => c.store.pending_count()?,
                None => 0,
            };
            if control.backfill_allowed()
                && self.main.store.pending_count()? == 0
                && code_pending == 0
                && self.main.store.unfinished_count()?.0 > 0
            {
                let c = &control;
                finish_long_files(&mut self.main.store, emb, ocr, &opts, &|| c.any_request(has_code) || !c.backfill_allowed(), &mut |e| {
                    on_event(ServiceEvent::Rest(e.clone()))
                })?;
                if control.any_request(has_code) {
                    continue;
                }
            }
            if self.code.is_some() && control.read(|f| f.code_sync) {
                continue;
            }
            on_event(ServiceEvent::Idle);
            match linger {
                None => return Ok(()),
                Some(l) => {
                    if !control.wait(l, self.code.is_some()) {
                        return Ok(());
                    }
                }
            }
        }
        on_event(ServiceEvent::Stopped);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing::FakeEmbedder;
    use crate::types::NoOcr;
    use std::sync::mpsc;

    struct BestOcr(bool);
    impl Ocr for BestOcr {
        fn recognize(&self, _: usize, _: usize, _: &[u8]) -> Option<String> {
            None
        }
        fn best_ready(&self) -> bool {
            self.0
        }
    }

    #[test]
    fn lighton_backfill_requeues_screenshots_once_when_the_best_engine_is_ready() {
        let mut store = IndexStore::open(&temp("lighton-backfill"), "index.sqlite").unwrap();
        let file = |p: &str, kind| dtfu_store::index::NewFile { path: p.into(), kind, size: 10, mtime: 1.0, inode: 1, device: 1 };
        let shot = store.insert_file(&file("/d/a.png", FileKind::Screenshot), &[]).unwrap();
        let photo = store.insert_file(&file("/d/p.jpg", FileKind::Image), &[]).unwrap();
        for id in [shot, photo] {
            store.finish(id, dtfu_core::FileState::Done, None, &Default::default()).unwrap();
        }
        lighton_backfill(&mut store, &BestOcr(false)).unwrap();
        assert_eq!(store.pending_count().unwrap(), 0, "not ready: nothing queued");
        assert_eq!(store.meta(OCR_LIGHTON_KEY).unwrap(), None);
        lighton_backfill(&mut store, &BestOcr(true)).unwrap();
        assert_eq!(store.pending_count().unwrap(), 1, "the screenshot only");
        store.finish(shot, dtfu_core::FileState::Done, None, &Default::default()).unwrap();
        lighton_backfill(&mut store, &BestOcr(true)).unwrap();
        assert_eq!(store.pending_count().unwrap(), 0, "once per index");
    }

    fn temp(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("dtfu-svc-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    fn notes(dir: &std::path::Path, prefix: &str, n: usize) {
        for i in 0..n {
            std::fs::write(dir.join(format!("{prefix}{i}.txt")), format!("Note {prefix}{i} about penguins number {i}.")).unwrap();
        }
    }

    fn service(name: &str, backfill: bool, opts: IndexOptions) -> (Service, Control, PathBuf) {
        let root = temp(&format!("{name}-root"));
        let store = IndexStore::open(&temp(&format!("{name}-store")), "index.sqlite").unwrap();
        let control = Control::new(backfill);
        let part = Part { store, roots: vec![root.clone()], code: false, keep_only: false };
        (Service::new(part, None, opts, control.clone()), control, root)
    }

    fn fake() -> Result<Box<dyn Embed>> {
        Ok(Box::new(FakeEmbedder::default()))
    }

    fn progress(events: &[ServiceEvent]) -> usize {
        events.iter().filter(|e| matches!(e, ServiceEvent::Progress(_))).count()
    }

    #[test]
    fn initial_sync_and_index() {
        let (mut svc, _c, root) = service("initial", true, IndexOptions::default());
        notes(&root, "a", 3);
        let mut events = Vec::new();
        svc.run(&mut fake, &NoOcr, None, &mut |e| events.push(e)).unwrap();
        assert_eq!(progress(&events), 3, "{events:?}");
        assert!(matches!(events.last(), Some(ServiceEvent::Idle)));
        assert_eq!(svc.main().store.pending_count().unwrap(), 0);
    }

    #[test]
    fn new_small_file_is_indexed_without_backfill_but_large_batch_waits() {
        let opts = IndexOptions { new_files_limit: 2, ..IndexOptions::default() };
        let (mut svc, control, root) = service("backfill", false, opts);
        notes(&root, "big", 4); // 4 > limit: waits for backfill
        let (tx, rx) = mpsc::channel();
        let c2 = control.clone();
        let handle = std::thread::spawn(move || {
            let r = svc.run(&mut fake, &NoOcr, Some(Duration::from_secs(10)), &mut |e| {
                let _ = tx.send(e);
            });
            (svc, r)
        });
        let wait_idle = |count: &mut usize| loop {
            match rx.recv_timeout(Duration::from_secs(10)).unwrap() {
                ServiceEvent::Progress(_) => *count += 1,
                ServiceEvent::Idle => return,
                _ => {}
            }
        };
        let mut done = 0;
        wait_idle(&mut done);
        assert_eq!(done, 0, "a big first sync must wait for backfill");

        notes(&root, "new", 1);
        c2.request_sync();
        wait_idle(&mut done);
        assert_eq!(done, 1, "the new file is indexed while backfill is off");

        c2.set_backfill_allowed(true);
        wait_idle(&mut done);
        assert_eq!(done, 5);
        c2.stop();
        let (svc, r) = handle.join().unwrap();
        r.unwrap();
        assert_eq!(svc.main().store.pending_count().unwrap(), 0);
    }

    #[test]
    fn stop_mid_run_leaves_the_rest_pending() {
        let (mut svc, control, root) = service("stop", true, IndexOptions::default());
        notes(&root, "s", 8);
        let mut events = Vec::new();
        svc.run(&mut fake, &NoOcr, None, &mut |e| {
            if matches!(e, ServiceEvent::Progress(_)) {
                control.stop();
            }
            events.push(e);
        })
        .unwrap();
        assert!(matches!(events.last(), Some(ServiceEvent::Stopped)), "{events:?}");
        let left = svc.main().store.pending_count().unwrap();
        assert!(left > 0 && left < 8, "left {left}");
    }

    #[test]
    fn model_not_ready_waits_with_names_searchable() {
        let (mut svc, _c, root) = service("waiting", true, IndexOptions::default());
        notes(&root, "w", 3);
        let mut events = Vec::new();
        svc.run(&mut || Err(NotReady.into()), &NoOcr, None, &mut |e| events.push(e)).unwrap();
        assert!(matches!(events.last(), Some(ServiceEvent::Waiting)), "{events:?}");
        assert_eq!(progress(&events), 0);
        let store = &svc.main().store;
        assert_eq!(store.pending_count().unwrap(), 3);
        let names: i64 = store.connection().query_row("SELECT COUNT(*) FROM segments WHERE kind = 'name'", [], |r| r.get(0)).unwrap();
        assert_eq!(names, 3);
        let vectors: i64 =
            store.connection().query_row("SELECT COUNT(*) FROM segments WHERE kind != 'name'", [], |r| r.get(0)).unwrap();
        assert_eq!(vectors, 0);
    }
}
