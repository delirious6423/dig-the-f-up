//! UI-free, thread-safe facade over indexing and search for a desktop app. It owns the index folder, the saved
//! folder list, one background indexing thread (`digup_index::service::Service`) and query-time search.

use anyhow::{anyhow, bail, Context, Result};
use digup_core::{query_prompt, FileKind, SegmentKind};
use digup_index::service::{Control, NotReady, Part, Service, ServiceEvent};
use digup_index::{Embed, IndexOptions, SystemOcr};
use digup_search::{SearchHit, SearchOptions, Searcher};
use digup_store::index::{IndexStore, CODE_FILE, MAIN_FILE};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{channel, Receiver, Sender};
use std::sync::{Arc, Mutex, MutexGuard};
use std::thread::JoinHandle;
use std::time::Duration;

pub type EmbedderFactory = Box<dyn Fn() -> Result<Box<dyn Embed>> + Send + Sync>;
pub type QueryEncoder = Box<dyn Fn(&str) -> Result<Vec<f32>> + Send + Sync>;

#[derive(Debug, Clone)]
pub struct EngineConfig {
    pub index_dir: PathBuf,
    pub settings_file: PathBuf,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Status {
    NoModel,
    Ready,
    Indexing { done: usize, total: usize, current: String },
    Idle,
    Paused,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Hit {
    pub path: String,
    pub kind: String,
    pub segment: String,
    pub score: f64,
    pub place: String,
    pub excerpt: Option<String>,
    pub loc: Option<f64>,
    pub focus: Option<f64>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct IndexCounts {
    pub files_by_kind_state: BTreeMap<String, BTreeMap<String, usize>>,
    pub vectors: usize,
    pub pending: usize,
    pub unfinished: usize,
}

/// A plain, cloneable form of `ServiceEvent`.
#[derive(Debug, Clone, PartialEq)]
pub enum EngineEvent {
    Pruned { removed: usize },
    Synced { added: usize, changed: usize, moved: usize, removed: usize, unchanged: usize },
    Progress { done: usize, total: usize, path: String, kind: String, state: String, note: Option<String> },
    /// A step of a long file in the later pass.
    Rest { done: usize, total: usize, path: String, kind: String, state: String, note: Option<String> },
    Waiting,
    Error(String),
    Idle,
    Stopped,
}

#[derive(Serialize, Deserialize, Default)]
struct Settings {
    #[serde(default)]
    folders: Vec<PathBuf>,
}

struct Inner {
    cfg: EngineConfig,
    settings: Mutex<Settings>,
    factory: Arc<dyn Fn() -> Result<Box<dyn Embed>> + Send + Sync>,
    query_encoder: QueryEncoder,
    status: Mutex<Status>,
    subscribers: Mutex<Vec<Sender<EngineEvent>>>,
    control: Mutex<Control>,
    thread: Mutex<Option<JoinHandle<()>>>,
    paused: AtomicBool,
    wanted: AtomicBool,
    backfill: AtomicBool,
}

pub struct Engine {
    inner: Arc<Inner>,
}

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

impl Inner {
    fn emit(&self, e: EngineEvent) {
        lock(&self.subscribers).retain(|s| s.send(e.clone()).is_ok());
    }

    fn save(&self, s: &Settings) -> Result<()> {
        if let Some(parent) = self.cfg.settings_file.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let tmp = self.cfg.settings_file.with_extension("tmp");
        std::fs::write(&tmp, serde_json::to_vec_pretty(s)?)?;
        std::fs::rename(&tmp, &self.cfg.settings_file)?;
        Ok(())
    }

    fn handle(&self, ev: ServiceEvent) {
        let ee = match ev {
            ServiceEvent::Pruned { removed, .. } => EngineEvent::Pruned { removed },
            ServiceEvent::Sync { summary, .. } => EngineEvent::Synced {
                added: summary.added,
                changed: summary.changed,
                moved: summary.moved,
                removed: summary.removed,
                unchanged: summary.unchanged,
            },
            ServiceEvent::Progress(e) | ServiceEvent::Rest(e) => {
                *lock(&self.status) = Status::Indexing { done: e.done, total: e.total, current: e.path.clone() };
                EngineEvent::Progress {
                    done: e.done,
                    total: e.total,
                    path: e.path,
                    kind: e.kind.as_str().into(),
                    state: e.state,
                    note: e.note,
                }
            }
            ServiceEvent::Waiting => {
                *lock(&self.status) = Status::NoModel;
                EngineEvent::Waiting
            }
            ServiceEvent::Error(m) => EngineEvent::Error(m),
            ServiceEvent::Idle => {
                *lock(&self.status) = Status::Idle;
                EngineEvent::Idle
            }
            ServiceEvent::Stopped => EngineEvent::Stopped,
        };
        self.emit(ee);
    }

    fn spawn(self: &Arc<Self>) -> Result<()> {
        let mut slot = lock(&self.thread);
        if let Some(h) = slot.as_ref() {
            if !h.is_finished() {
                lock(&self.control).request_sync();
                return Ok(());
            }
        }
        if let Some(h) = slot.take() {
            let _ = h.join();
        }
        let roots = lock(&self.settings).folders.clone();
        let control = Control::new(self.backfill.load(Ordering::SeqCst));
        *lock(&self.control) = control.clone();
        let me = self.clone();
        let dir = self.cfg.index_dir.clone();
        *slot = Some(
            std::thread::Builder::new().name("digup-indexing".into()).spawn(move || {
                let result = (|| -> Result<()> {
                    let store = IndexStore::open(&dir, MAIN_FILE)?;
                    let main = Part { store, roots, code: false, keep_only: true };
                    let mut service = Service::new(main, None, IndexOptions::default(), control);
                    let factory = me.factory.clone();
                    let mut make = move || factory();
                    let mut on_event = |e: ServiceEvent| me.handle(e);
                    service.run(&mut make, &SystemOcr, Some(Duration::from_secs(60)), &mut on_event)
                })();
                if let Err(e) = result {
                    me.emit(EngineEvent::Error(format!("{e:#}")));
                }
                let mut st = lock(&me.status);
                if matches!(*st, Status::Indexing { .. } | Status::Ready) {
                    *st = Status::Idle;
                }
            })?,
        );
        Ok(())
    }

    fn halt(&self) {
        let mut slot = lock(&self.thread);
        if let Some(h) = slot.take() {
            lock(&self.control).stop();
            let _ = h.join();
        }
    }
}

/// What follows a leading `code:` (any case), trimmed.
fn code_query_text(query: &str) -> Option<String> {
    let t = query.trim_start();
    let head: String = t.chars().take(5).collect();
    (head.to_lowercase() == "code:").then(|| t.chars().skip(5).collect::<String>().trim().to_string())
}

fn clock(s: f64) -> String {
    let t = s as i64;
    format!("{}:{:02}", t / 60, t % 60)
}

/// Where in the file a hit is, as short text ("p. 3", "at 1:05", "lines 10–20").
pub fn describe_location(hit: &SearchHit) -> String {
    let Some(loc) = hit.loc else {
        return if hit.segment == SegmentKind::Ocr { "text in image".into() } else { String::new() };
    };
    match hit.segment {
        SegmentKind::Page | SegmentKind::PageImage => format!("p. {}", loc as i64),
        SegmentKind::Audio => format!("at {} (sound)", clock(loc)),
        SegmentKind::Frame => format!("at {}", clock(loc)),
        SegmentKind::Chunk => format!("part {}", loc as i64),
        SegmentKind::Lines => format!(
            "lines {}–{}{}",
            loc as i64,
            hit.loc_end.unwrap_or(loc) as i64,
            hit.focus.map(|f| format!(", opens at {}", f as i64)).unwrap_or_default()
        ),
        SegmentKind::Cells => {
            if Some(loc) == hit.loc_end {
                format!("cell {}", loc as i64)
            } else {
                format!("cells {}–{}", loc as i64, hit.loc_end.unwrap_or(loc) as i64)
            }
        }
        _ => String::new(),
    }
}

fn real_factory() -> Result<Box<dyn Embed>> {
    let (Some(text), Some(mmproj)) = (digup_models::locate(&digup_models::TEXT), digup_models::locate(&digup_models::MMPROJ))
    else {
        return Err(NotReady.into());
    };
    let e = digup_embed::Embedder::new(&text, Some(&mmproj), &digup_embed::Options::default())?;
    Ok(Box::new(e))
}

fn real_query_encoder() -> QueryEncoder {
    let embedder: Mutex<Option<digup_embed::Embedder>> = Mutex::new(None);
    Box::new(move |prompt: &str| {
        let mut g = lock(&embedder);
        if g.is_none() {
            let Some(text) = digup_models::locate(&digup_models::TEXT) else {
                bail!(
                    "the search model is not downloaded yet (looked in {})",
                    digup_models::models_dir().display()
                );
            };
            *g = Some(digup_embed::Embedder::new(&text, None, &digup_embed::Options::default())?);
        }
        let e = g.as_mut().expect("loaded above");
        e.embed_texts(&[prompt.to_string()])?.into_iter().next().ok_or_else(|| anyhow!("the encoder returned nothing"))
    })
}

impl Engine {
    pub fn open(cfg: EngineConfig) -> Result<Engine> {
        Self::open_with(cfg, Box::new(real_factory), real_query_encoder())
    }

    /// `query_encoder` receives the prompt-formatted query (`digup_core::query_prompt`).
    pub fn open_with(cfg: EngineConfig, factory: EmbedderFactory, query_encoder: QueryEncoder) -> Result<Engine> {
        std::fs::create_dir_all(&cfg.index_dir).with_context(|| format!("creating {}", cfg.index_dir.display()))?;
        IndexStore::open(&cfg.index_dir, MAIN_FILE)?; // creates the store
        let settings = match std::fs::read(&cfg.settings_file) {
            Ok(b) => serde_json::from_slice(&b).unwrap_or_else(|_| {
                let _ = std::fs::rename(&cfg.settings_file, cfg.settings_file.with_extension("json.bad"));
                Settings::default()
            }),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Settings::default(),
            Err(e) => return Err(e.into()),
        };
        Ok(Engine {
            inner: Arc::new(Inner {
                cfg,
                settings: Mutex::new(settings),
                factory: Arc::from(factory),
                query_encoder,
                status: Mutex::new(Status::Ready),
                subscribers: Mutex::new(Vec::new()),
                control: Mutex::new(Control::new(true)),
                thread: Mutex::new(None),
                paused: AtomicBool::new(false),
                wanted: AtomicBool::new(false),
                backfill: AtomicBool::new(true),
            }),
        })
    }

    pub fn folders(&self) -> Vec<PathBuf> {
        lock(&self.inner.settings).folders.clone()
    }

    pub fn add_folder(&self, p: PathBuf) -> Result<()> {
        {
            let mut s = lock(&self.inner.settings);
            if s.folders.contains(&p) {
                return Ok(());
            }
            s.folders.push(p);
            if let Err(e) = self.inner.save(&s) {
                s.folders.pop();
                return Err(e);
            }
        }
        self.folders_changed()
    }

    pub fn remove_folder(&self, p: &Path) -> Result<()> {
        {
            let mut s = lock(&self.inner.settings);
            let before = s.folders.len();
            s.folders.retain(|f| f != p);
            if s.folders.len() == before {
                return Ok(());
            }
            self.inner.save(&s)?;
        }
        self.folders_changed()
    }

    /// The service holds its roots from its start, so a running one is restarted with the new list.
    fn folders_changed(&self) -> Result<()> {
        if self.inner.wanted.load(Ordering::SeqCst) {
            self.inner.halt();
            self.inner.spawn()?;
        }
        Ok(())
    }

    pub fn search(&self, query: &str, limit: usize, kinds: Option<Vec<String>>) -> Result<Vec<Hit>> {
        let (text, code) = match code_query_text(query) {
            Some(t) => (t, true),
            None => (query.trim().to_string(), false),
        };
        if text.is_empty() {
            return Ok(Vec::new());
        }
        let store = IndexStore::open(&self.inner.cfg.index_dir, if code { CODE_FILE } else { MAIN_FILE })?;
        let searcher = Searcher::with_code(store.connection(), code)?;
        let vector = (self.inner.query_encoder)(&query_prompt(&text, code))?;
        let kinds: Option<HashSet<FileKind>> =
            kinds.map(|k| k.iter().filter_map(|s| FileKind::parse(s.trim())).collect());
        let opts = SearchOptions { kinds, limit, ..SearchOptions::default() };
        let hits = searcher.search(&vector, &text, &opts)?;
        // A removed folder stays in the index until the next prune; hide its hits meanwhile.
        let roots = self.folders();
        Ok(hits
            .iter()
            .filter(|h| roots.is_empty() || roots.iter().any(|r| Path::new(&h.path).starts_with(r)))
            .map(|h| Hit {
                path: h.path.clone(),
                kind: h.kind.as_str().into(),
                segment: h.segment.as_str().into(),
                score: h.score,
                place: describe_location(h),
                excerpt: h.excerpt.clone(),
                loc: h.loc,
                focus: h.focus,
            })
            .collect())
    }

    pub fn counts(&self) -> Result<IndexCounts> {
        let store = IndexStore::open(&self.inner.cfg.index_dir, MAIN_FILE)?;
        let c = store.counts()?;
        Ok(IndexCounts {
            files_by_kind_state: c.by_kind_and_state,
            vectors: c.vectors,
            pending: store.pending_count()?,
            unfinished: c.unfinished,
        })
    }

    pub fn status(&self) -> Status {
        if self.inner.paused.load(Ordering::SeqCst) {
            return Status::Paused;
        }
        lock(&self.inner.status).clone()
    }

    pub fn subscribe(&self) -> Receiver<EngineEvent> {
        let (tx, rx) = channel();
        lock(&self.inner.subscribers).push(tx);
        rx
    }

    /// Starts the indexing thread (or asks a running one to sync again). Also clears a pause.
    pub fn start_indexing(&self) -> Result<()> {
        self.inner.paused.store(false, Ordering::SeqCst);
        self.inner.wanted.store(true, Ordering::SeqCst);
        self.inner.spawn()
    }

    /// Stops the thread after its current job and remembers the pause.
    pub fn pause(&self) {
        self.inner.paused.store(true, Ordering::SeqCst);
        self.inner.wanted.store(false, Ordering::SeqCst);
        self.inner.halt();
    }

    pub fn resume(&self) {
        if let Err(e) = self.start_indexing() {
            self.inner.emit(EngineEvent::Error(format!("{e:#}")));
        }
    }

    /// Whether backfill (older pending files, long-file rest) may run; new files always do.
    pub fn set_backfill_allowed(&self, allowed: bool) {
        self.inner.backfill.store(allowed, Ordering::SeqCst);
        lock(&self.inner.control).set_backfill_allowed(allowed);
    }

    /// Stops and joins the thread.
    pub fn shutdown(&self) {
        self.inner.wanted.store(false, Ordering::SeqCst);
        self.inner.halt();
    }
}

impl Drop for Engine {
    fn drop(&mut self) {
        self.shutdown();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Instant;

    fn unit(seed: &[u8]) -> Vec<f32> {
        let mut h: u64 = 0xcbf29ce484222325;
        for b in seed {
            h = (h ^ *b as u64).wrapping_mul(0x100000001b3);
        }
        let mut v = Vec::with_capacity(768);
        for _ in 0..768 {
            h ^= h << 13;
            h ^= h >> 7;
            h ^= h << 17;
            v.push(((h >> 11) as f64 / (1u64 << 53) as f64 - 0.5) as f32);
        }
        let n = v.iter().map(|x| (*x as f64).powi(2)).sum::<f64>().sqrt() as f32;
        v.iter().map(|x| x / n).collect()
    }

    struct FakeEmbed;
    impl Embed for FakeEmbed {
        fn vector_source(&self) -> String {
            "fake".into()
        }
        fn embed_texts(&mut self, texts: &[String]) -> Result<Vec<Vec<f32>>> {
            Ok(texts.iter().map(|t| unit(t.as_bytes())).collect())
        }
        fn embed_image_rgb(&mut self, _: usize, _: usize, rgb: &[u8], _: usize) -> Result<Vec<f32>> {
            Ok(unit(&rgb[..rgb.len().min(64)]))
        }
        fn embed_audio_samples(&mut self, s: &[f32]) -> Result<Vec<f32>> {
            Ok(unit(&s.len().to_le_bytes()))
        }
    }

    fn engine(tmp: &Path) -> Engine {
        let cfg = EngineConfig { index_dir: tmp.join("index"), settings_file: tmp.join("settings.json") };
        Engine::open_with(cfg, Box::new(|| Ok(Box::new(FakeEmbed))), Box::new(|q| Ok(unit(q.as_bytes())))).unwrap()
    }

    fn wait_for(what: &str, mut f: impl FnMut() -> bool) {
        let t = Instant::now();
        while t.elapsed() < Duration::from_secs(5) {
            if f() {
                return;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        panic!("timed out waiting for {what}");
    }

    fn notes(dir: &Path) {
        std::fs::create_dir_all(dir).unwrap();
        std::fs::write(dir.join("a.txt"), "The walrus basked on the ice floe all afternoon.").unwrap();
        std::fs::write(dir.join("b.txt"), "An ostrich buried its head in the sand yesterday.").unwrap();
        std::fs::write(dir.join("c.txt"), "A zebra grazed quietly near the watering hole.").unwrap();
    }

    fn indexed(e: &Engine) -> bool {
        e.counts().map(|c| c.vectors >= 3 && c.pending == 0).unwrap_or(false)
    }

    #[test]
    fn engine_is_send_sync() {
        fn ok<T: Send + Sync>() {}
        ok::<Engine>();
    }

    #[test]
    fn folders_persist_across_reopen() {
        let tmp = tempfile::tempdir().unwrap();
        {
            let e = engine(tmp.path());
            e.add_folder("/a".into()).unwrap();
            e.add_folder("/b".into()).unwrap();
            e.add_folder("/a".into()).unwrap();
            e.remove_folder(Path::new("/b")).unwrap();
        }
        let e = engine(tmp.path());
        assert_eq!(e.folders(), vec![PathBuf::from("/a")]);
    }

    #[test]
    fn indexes_and_searches() {
        let tmp = tempfile::tempdir().unwrap();
        let docs = tmp.path().join("docs");
        notes(&docs);
        let e = engine(tmp.path());
        assert_eq!(e.status(), Status::Ready);
        let rx = e.subscribe();
        e.add_folder(docs.clone()).unwrap();
        e.start_indexing().unwrap();
        wait_for("indexing", || indexed(&e));
        wait_for("idle", || e.status() == Status::Idle);
        let hits = e.search("walrus", 5, None).unwrap();
        assert!(hits[0].path.ends_with("a.txt"), "{hits:?}");
        let hits = e.search("zebra", 5, Some(vec!["doc".into()])).unwrap();
        assert!(hits[0].path.ends_with("c.txt"), "{hits:?}");
        let events: Vec<EngineEvent> = rx.try_iter().collect();
        assert!(events.iter().any(|e| matches!(e, EngineEvent::Progress { .. })));
        assert!(events.contains(&EngineEvent::Idle));
        e.shutdown();
    }

    #[test]
    fn pause_resume_and_shutdown() {
        let tmp = tempfile::tempdir().unwrap();
        let docs = tmp.path().join("docs");
        notes(&docs);
        let e = engine(tmp.path());
        e.add_folder(docs.clone()).unwrap();
        e.pause();
        assert_eq!(e.status(), Status::Paused);
        e.resume();
        assert_ne!(e.status(), Status::Paused);
        wait_for("indexing", || indexed(&e));
        // adding a folder while running restarts the thread with the new roots
        let more = tmp.path().join("more");
        std::fs::create_dir_all(&more).unwrap();
        std::fs::write(more.join("d.txt"), "A pelican scooped fish from the harbour.").unwrap();
        e.add_folder(more).unwrap();
        wait_for("second folder", || e.search("pelican", 3, None).map(|h| h.first().is_some_and(|h| h.path.ends_with("d.txt"))).unwrap_or(false));
        e.pause();
        assert_eq!(e.status(), Status::Paused);
        e.resume();
        e.shutdown();
        assert!(lock(&e.inner.thread).is_none());
    }

    #[test]
    fn missing_model_is_waiting() {
        let tmp = tempfile::tempdir().unwrap();
        let docs = tmp.path().join("docs");
        notes(&docs);
        let cfg = EngineConfig { index_dir: tmp.path().join("i"), settings_file: tmp.path().join("s.json") };
        let e = Engine::open_with(cfg, Box::new(|| Err(NotReady.into())), Box::new(|_| bail!("no model"))).unwrap();
        e.add_folder(docs).unwrap();
        e.start_indexing().unwrap();
        wait_for("no model", || e.status() == Status::NoModel);
        assert!(e.search("walrus", 3, None).is_err());
    }
}
