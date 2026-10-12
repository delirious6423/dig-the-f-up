//! UI-free, thread-safe facade over indexing and search for a desktop app. It owns the index folder, the saved
//! folder list, one background indexing thread (`dtfu_index::service::Service`) and query-time search.

use anyhow::{anyhow, bail, Context, Result};
use dtfu_core::{query_prompt, FileKind, SegmentKind};
use dtfu_index::service::{Control, NotReady, Part, Service, ServiceEvent};
use dtfu_index::sync::{forget_volume, RootState};
use dtfu_index::{Embed, IndexOptions, SystemOcr};
use dtfu_search::searcher::OwnedSearcher;
use dtfu_search::filters;
use dtfu_search::{SearchHit, SearchOptions, Searcher};
use dtfu_store::index::{IndexStore, CODE_FILE, MAIN_FILE};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::mpsc::{channel, Receiver, Sender};
use std::sync::{Arc, Mutex, MutexGuard, Weak};
use std::thread::JoinHandle;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

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
    /// `offline`: folders skipped because their drive is unplugged or not answering (their files stay searchable).
    /// `different_drive`: folders where another drive is mounted in place of theirs (see `Engine::use_drive_now`).
    Synced {
        added: usize,
        changed: usize,
        moved: usize,
        removed: usize,
        unchanged: usize,
        offline: Vec<String>,
        different_drive: Vec<String>,
    },
    Progress { done: usize, total: usize, path: String, kind: String, state: String, note: Option<String> },
    /// A step of a long file in the later pass.
    Rest { done: usize, total: usize, path: String, kind: String, state: String, note: Option<String> },
    Waiting,
    Error(String),
    Idle,
    Stopped,
}

#[derive(Serialize, Deserialize)]
struct Settings {
    #[serde(default)]
    folders: Vec<PathBuf>,
    /// Keep the index fresh from filesystem events while indexing is wanted.
    #[serde(default = "yes")]
    watch: bool,
    #[serde(default)]
    watch_policy: WatchPolicy,
    /// Remember which files are opened so they rank and index earlier. Off until the user turns it on; the history
    /// stays in `usage.sqlite` next to the index and never leaves the machine.
    #[serde(default)]
    usage_tracking: bool,
}

fn yes() -> bool {
    true
}

impl Default for Settings {
    fn default() -> Self {
        Self { folders: Vec::new(), watch: true, watch_policy: WatchPolicy::default(), usage_tracking: false }
    }
}

/// When filesystem changes may start a sync. Changes are always recorded; this only gates when the (battery-hungry)
/// embedding runs. The default keeps a laptop on battery quiet.
#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq, Default)]
#[serde(rename_all = "snake_case")]
pub enum WatchPolicy {
    /// Sync as changes settle, on any power source.
    Live,
    /// Sync only on AC power and outside Low Power Mode; changes wait otherwise.
    #[default]
    PluggedIn,
    /// Never sync on its own; the periodic and manual syncs still work.
    Manual,
}

impl WatchPolicy {
    /// The name used in the settings file and by front ends: `live`, `plugged_in` or `manual`.
    pub fn as_str(self) -> &'static str {
        match self {
            WatchPolicy::Live => "live",
            WatchPolicy::PluggedIn => "plugged_in",
            WatchPolicy::Manual => "manual",
        }
    }

    pub fn parse(s: &str) -> Option<WatchPolicy> {
        match s {
            "live" => Some(WatchPolicy::Live),
            "plugged_in" => Some(WatchPolicy::PluggedIn),
            "manual" => Some(WatchPolicy::Manual),
            _ => None,
        }
    }

    fn power_policy(self) -> dtfu_watch::PowerPolicy {
        match self {
            WatchPolicy::Live => dtfu_watch::PowerPolicy::Live,
            WatchPolicy::PluggedIn => dtfu_watch::PowerPolicy::PluggedInOnly,
            WatchPolicy::Manual => dtfu_watch::PowerPolicy::Manual,
        }
    }
}

/// What the watcher is doing, for a status line.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct WatchInfo {
    pub running: bool,
    /// "events", or why only the periodic re-sync runs.
    pub mode: String,
    /// Changes recorded and waiting for conditions to allow a sync.
    pub pending: usize,
    /// Why they are waiting ("on battery", ...), if they are.
    pub deferred: Option<String>,
}

/// Forwards the watcher's sync requests to the indexing thread. Holds the engine weakly: the engine owns the watcher.
struct EngineSink(Weak<Inner>);

impl dtfu_watch::SyncSink for EngineSink {
    fn request_sync(&self) {
        if let Some(inner) = self.0.upgrade() {
            // A running service takes the request; one that went idle and exited is started again.
            if inner.wanted.load(Ordering::SeqCst) && !inner.paused.load(Ordering::SeqCst) {
                if let Err(e) = inner.spawn() {
                    inner.emit(EngineEvent::Error(format!("{e:#}")));
                }
            }
        }
    }
}

struct Inner {
    cfg: EngineConfig,
    settings: Mutex<Settings>,
    factory: Arc<dyn Fn() -> Result<Box<dyn Embed>> + Send + Sync>,
    query_encoder: QueryEncoder,
    /// Searchers kept between queries (main index, code index): loading every vector is the cost of building one,
    /// so it is paid once per index change instead of once per query. See `Inner::with_searcher`.
    main_searcher: Mutex<Option<OwnedSearcher>>,
    code_searcher: Mutex<Option<OwnedSearcher>>,
    /// How many times a cached searcher was loaded in full (first query, or a refresh that fell back); for tests and diagnostics.
    searcher_loads: AtomicUsize,
    /// How many times a cached searcher was brought up to date incrementally (no full load).
    searcher_refreshes: AtomicUsize,
    watcher: Mutex<Option<dtfu_watch::Watcher>>,
    usage: Mutex<Option<dtfu_usage::UsageStore>>,
    status: Mutex<Status>,
    subscribers: Mutex<Vec<Sender<EngineEvent>>>,
    control: Mutex<Control>,
    thread: Mutex<Option<JoinHandle<()>>>,
    paused: AtomicBool,
    wanted: AtomicBool,
    backfill: AtomicBool,
    /// Each folder's state from the last main-index sync, keyed by the folder as the sync saw it.
    folder_states: Mutex<HashMap<String, RootState>>,
}

pub struct Engine {
    inner: Arc<Inner>,
}

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

impl Inner {
    /// Runs `f` with the cached searcher of the main or code index, rebuilding it first when the index changed since
    /// it was loaded (another connection committed: the indexing thread, or another process). The lock is held
    /// during `f`, which serialises searches; a scan is milliseconds, so that is cheaper than a second copy of the
    /// vectors.
    fn with_searcher<R>(&self, code: bool, f: impl FnOnce(&Searcher<'static>) -> Result<R>) -> Result<R> {
        let slot = if code { &self.code_searcher } else { &self.main_searcher };
        let mut guard = lock(slot);
        let mut fresh = match guard.as_ref() {
            Some(s) => s.is_current()?,
            None => false,
        };
        if !fresh {
            if let Some(s) = guard.as_mut() {
                // Update what changed instead of reloading every vector (docs/BENCHMARKS.md R1); a full load is the
                // fallback when that fails.
                match s.refresh() {
                    Ok(stats) => {
                        let counter = if stats.full { &self.searcher_loads } else { &self.searcher_refreshes };
                        counter.fetch_add(1, Ordering::SeqCst);
                        fresh = true;
                    }
                    Err(e) => eprintln!("dtfu: searcher refresh failed ({e}); reloading"),
                }
            }
        }
        if !fresh {
            *guard = None; // drop the old vectors before loading the new ones
            let store = IndexStore::open(&self.cfg.index_dir, if code { CODE_FILE } else { MAIN_FILE })?;
            *guard = Some(Searcher::with_owned_connection(store.into_connection(), code)?);
            self.searcher_loads.fetch_add(1, Ordering::SeqCst);
        }
        f(guard.as_ref().expect("loaded above"))
    }

    fn emit(&self, e: EngineEvent) {
        lock(&self.subscribers).retain(|s| s.send(e.clone()).is_ok());
    }

    fn save(&self, s: &Settings) -> Result<()> {
        if let Some(parent) = self.cfg.settings_file.parent() {
            std::fs::create_dir_all(parent)?;
        }
        dtfu_models::write_atomic(&self.cfg.settings_file, &serde_json::to_vec_pretty(s)?)?;
        Ok(())
    }

    fn handle(&self, ev: ServiceEvent) {
        let ee = match ev {
            ServiceEvent::Pruned { removed, .. } => EngineEvent::Pruned { removed },
            ServiceEvent::Sync { summary, code } => {
                let pick = |want: RootState| -> Vec<String> {
                    summary.roots.iter().filter(|r| r.state == want).map(|r| r.root.clone()).collect()
                };
                if !code {
                    *lock(&self.folder_states) = summary.roots.iter().map(|r| (r.root.clone(), r.state)).collect();
                }
                EngineEvent::Synced {
                    added: summary.added,
                    changed: summary.changed,
                    moved: summary.moved,
                    removed: summary.removed,
                    unchanged: summary.unchanged,
                    offline: pick(RootState::Offline),
                    different_drive: pick(RootState::DifferentDrive),
                }
            }
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
                // The service may have timed out and returned just before the request landed; if so, fall through
                // and start a new one (it begins with a sync) instead of losing the request.
                if !h.is_finished() {
                    return Ok(());
                }
            }
        }
        if let Some(h) = slot.take() {
            let _ = h.join();
        }
        let roots = lock(&self.settings).folders.clone();
        let priority = self.priority();
        let control = Control::new(self.backfill.load(Ordering::SeqCst));
        *lock(&self.control) = control.clone();
        let me = self.clone();
        let dir = self.cfg.index_dir.clone();
        *slot = Some(
            std::thread::Builder::new().name("dtfu-indexing".into()).spawn(move || {
                let result = (|| -> Result<()> {
                    let store = IndexStore::open(&dir, MAIN_FILE)?;
                    let main = Part { store, roots, code: false, keep_only: true };
                    let opts = IndexOptions { priority, ..IndexOptions::default() };
                    let mut service = Service::new(main, None, opts, control);
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

    fn now() -> f64 {
        SystemTime::now().duration_since(UNIX_EPOCH).map_or(0.0, |d| d.as_secs_f64())
    }

    /// Runs `f` on the usage history, opening `usage.sqlite` on first use.
    fn with_usage<R>(&self, f: impl FnOnce(&dtfu_usage::UsageStore) -> Result<R>) -> Result<R> {
        let mut g = lock(&self.usage);
        if g.is_none() {
            *g = Some(dtfu_usage::UsageStore::open(&self.cfg.index_dir.join("usage.sqlite"))?);
        }
        f(g.as_ref().expect("opened above"))
    }

    /// Frecency of the user's most opened files, for indexing them first. Empty unless usage tracking is on.
    fn priority(&self) -> Option<Arc<HashMap<String, f64>>> {
        if !lock(&self.settings).usage_tracking {
            return None;
        }
        let top = self.with_usage(|u| u.top(5000, Self::now())).ok()?;
        (!top.is_empty()).then(|| Arc::new(top.into_iter().collect()))
    }

    fn start_watcher(self: &Arc<Self>) {
        let (enabled, policy, roots) = {
            let s = lock(&self.settings);
            (s.watch, s.watch_policy, s.folders.clone())
        };
        // Stop a running one first, outside the lock: its drop joins a thread that may be inside `request_sync`.
        self.stop_watcher();
        if !enabled || roots.is_empty() {
            return;
        }
        let cfg = dtfu_watch::WatchConfig { policy: policy.power_policy(), ..Default::default() };
        let sink = Arc::new(EngineSink(Arc::downgrade(self)));
        match dtfu_watch::Watcher::start(
            &roots,
            cfg,
            sink,
            dtfu_watch::default_power_source(),
            Arc::new(dtfu_watch::SystemClock::new()),
        ) {
            Ok(w) => *lock(&self.watcher) = Some(w),
            Err(e) => self.emit(EngineEvent::Error(format!("file watcher: {e:#}"))),
        }
    }

    fn stop_watcher(&self) {
        // Dropped outside the lock: the watcher's thread may be inside `request_sync` -> `spawn`.
        let w = lock(&self.watcher).take();
        drop(w);
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
    let (Some(text), Some(mmproj)) = (dtfu_models::locate(&dtfu_models::TEXT), dtfu_models::locate(&dtfu_models::MMPROJ))
    else {
        return Err(NotReady.into());
    };
    let e = dtfu_embed::Embedder::new(&text, Some(&mmproj), &dtfu_embed::Options::default())?;
    Ok(Box::new(e))
}

fn real_query_encoder() -> QueryEncoder {
    let embedder: Mutex<Option<dtfu_embed::Embedder>> = Mutex::new(None);
    Box::new(move |prompt: &str| {
        let mut g = lock(&embedder);
        if g.is_none() {
            let Some(text) = dtfu_models::locate(&dtfu_models::TEXT) else {
                bail!(
                    "the search model is not downloaded yet (looked in {})",
                    dtfu_models::models_dir().display()
                );
            };
            *g = Some(dtfu_embed::Embedder::new(&text, None, &dtfu_embed::Options::default())?);
        }
        let e = g.as_mut().expect("loaded above");
        e.embed_texts(&[prompt.to_string()])?.into_iter().next().ok_or_else(|| anyhow!("the encoder returned nothing"))
    })
}

impl Engine {
    pub fn open(cfg: EngineConfig) -> Result<Engine> {
        Self::open_with(cfg, Box::new(real_factory), real_query_encoder())
    }

    /// `query_encoder` receives the prompt-formatted query (`dtfu_core::query_prompt`).
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
                main_searcher: Mutex::new(None),
                code_searcher: Mutex::new(None),
                searcher_loads: AtomicUsize::new(0),
                searcher_refreshes: AtomicUsize::new(0),
                watcher: Mutex::new(None),
                usage: Mutex::new(None),
                status: Mutex::new(Status::Ready),
                subscribers: Mutex::new(Vec::new()),
                control: Mutex::new(Control::new(true)),
                thread: Mutex::new(None),
                paused: AtomicBool::new(false),
                wanted: AtomicBool::new(false),
                backfill: AtomicBool::new(true),
                folder_states: Mutex::new(HashMap::new()),
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
            self.inner.start_watcher(); // the roots changed
        }
        Ok(())
    }

    pub fn search(&self, query: &str, limit: usize, kinds: Option<Vec<String>>) -> Result<Vec<Hit>> {
        let (text, code) = match code_query_text(query) {
            Some(t) => (t, true),
            None => (query.trim().to_string(), false),
        };
        // `ext:`, `size:` and `mtime:` terms are taken out of the text and applied to candidates before the limit.
        let now = SystemTime::now().duration_since(UNIX_EPOCH).map_or(0.0, |d| d.as_secs_f64());
        let (text, filters) = filters::parse(&text, now);
        let text = text.trim().to_string();
        if text.is_empty() && filters.is_empty() {
            return Ok(Vec::new());
        }
        // A filter-only query has no words, so every indexed file is a candidate and the filters alone decide.
        // Their order is by meaning against the empty query; the gate is open so no file is cut before the filters.
        let z_gate = if text.is_empty() { f64::NEG_INFINITY } else { SearchOptions::default().z_gate };
        let vector = (self.inner.query_encoder)(&query_prompt(&text, code))?;
        let kinds: Option<HashSet<FileKind>> =
            kinds.map(|k| k.iter().filter_map(|s| FileKind::parse(s.trim())).collect());
        // With usage tracking on, files the user opens often get a small bonus. It can only reorder what the search
        // returned, so a few extra hits are fetched for it to choose from; with tracking off nothing changes.
        let tracking = lock(&self.inner.settings).usage_tracking;
        let fetch = if tracking { (limit * 3).min(150).max(limit) } else { limit };
        let opts = SearchOptions { kinds, limit: fetch, filters, z_gate, ..SearchOptions::default() };
        let hits = self.inner.with_searcher(code, |searcher| searcher.search(&vector, &text, &opts))?;
        // A removed folder stays in the index until the next prune; hide its hits meanwhile.
        let roots = self.folders();
        let mut out: Vec<Hit> = hits
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
            .collect();
        if tracking && !out.is_empty() {
            let paths: Vec<&str> = out.iter().map(|h| h.path.as_str()).collect();
            // A history that cannot be read must not break search: fall back to the plain order.
            if let Ok(scores) = self.inner.with_usage(|u| u.scores(&paths, Inner::now())) {
                for h in &mut out {
                    h.score += dtfu_usage::boost(scores.get(&h.path).copied().unwrap_or(0.0));
                }
                out.sort_by(|a, b| b.score.total_cmp(&a.score));
            }
        }
        out.truncate(limit);
        Ok(out)
    }

    /// Records that the user opened `path` from a result, if usage tracking is on (it is off by default).
    pub fn record_open(&self, path: &str) -> Result<()> {
        if !lock(&self.inner.settings).usage_tracking {
            return Ok(());
        }
        self.inner.with_usage(|u| u.record_open(path, Inner::now()))
    }

    pub fn usage_tracking(&self) -> bool {
        lock(&self.inner.settings).usage_tracking
    }

    /// Turns open-history on or off. Turning it off also deletes the history.
    pub fn set_usage_tracking(&self, on: bool) -> Result<()> {
        {
            let mut s = lock(&self.inner.settings);
            if s.usage_tracking == on {
                return Ok(());
            }
            s.usage_tracking = on;
            self.inner.save(&s)?;
        }
        if !on {
            self.clear_usage()?;
        }
        Ok(())
    }

    pub fn clear_usage(&self) -> Result<()> {
        self.inner.with_usage(|u| u.clear())
    }

    /// Whether the file watcher is on and under which power policy.
    pub fn watch_settings(&self) -> (bool, WatchPolicy) {
        let s = lock(&self.inner.settings);
        (s.watch, s.watch_policy)
    }

    /// Changes the watcher settings; a running watcher is restarted with them.
    pub fn set_watch(&self, enabled: bool, policy: WatchPolicy) -> Result<()> {
        {
            let mut s = lock(&self.inner.settings);
            s.watch = enabled;
            s.watch_policy = policy;
            self.inner.save(&s)?;
        }
        if self.inner.wanted.load(Ordering::SeqCst) {
            self.inner.start_watcher();
        }
        Ok(())
    }

    pub fn watch_info(&self) -> WatchInfo {
        match lock(&self.inner.watcher).as_ref() {
            None => WatchInfo::default(),
            Some(w) => WatchInfo {
                running: true,
                mode: match w.mode() {
                    dtfu_watch::Mode::Events => "events".into(),
                    dtfu_watch::Mode::PeriodicOnly { reason } => format!("periodic re-sync only ({reason})"),
                },
                pending: w.pending_count(),
                deferred: w.deferred_reason().map(|r| r.to_string()),
            },
        }
    }

    /// Each folder with its state from the last sync: "online", "offline" (drive unplugged or not answering; its files
    /// stay searchable), "different_drive" (another drive is mounted in its place), or "unknown" (not synced yet).
    pub fn folder_states(&self) -> Vec<(PathBuf, &'static str)> {
        let states = lock(&self.inner.folder_states);
        self.folders()
            .into_iter()
            .map(|f| {
                // Same key as the sync's: the folder as text without a trailing '/' (except "/" itself).
                let text = f.to_string_lossy();
                let key = if text.len() > 1 { text.trim_end_matches('/') } else { &text };
                let state = match states.get(key) {
                    Some(RootState::Online) => "online",
                    Some(RootState::Offline) => "offline",
                    Some(RootState::DifferentDrive) => "different_drive",
                    None => "unknown",
                };
                (f, state)
            })
            .collect()
    }

    /// Accepts the drive now mounted at `folder` in place of the one it was indexed on: the next sync indexes it and
    /// removes the files the old drive had that this one doesn't. Blocking (opens the stores).
    pub fn use_drive_now(&self, folder: &Path) -> Result<()> {
        for file in [MAIN_FILE, CODE_FILE] {
            if self.inner.cfg.index_dir.join(file).exists() {
                forget_volume(&IndexStore::open(&self.inner.cfg.index_dir, file)?, folder)?;
            }
        }
        if self.inner.wanted.load(Ordering::SeqCst) && !self.inner.paused.load(Ordering::SeqCst) {
            self.inner.spawn()?;
        }
        Ok(())
    }

    /// True when `path` is exactly a file of the main or code index. Blocking (opens the stores).
    pub fn is_indexed_file(&self, path: &str) -> Result<bool> {
        for file in [MAIN_FILE, CODE_FILE] {
            // Don't create an index database (e.g. the code index) just to look a path up.
            if !self.inner.cfg.index_dir.join(file).exists() {
                continue;
            }
            if IndexStore::open(&self.inner.cfg.index_dir, file)?.has_path(path)? {
                return Ok(true);
            }
        }
        Ok(false)
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
        self.inner.spawn()?;
        if lock(&self.inner.watcher).is_none() {
            self.inner.start_watcher();
        }
        Ok(())
    }

    /// Stops the thread after its current job and remembers the pause.
    pub fn pause(&self) {
        self.inner.paused.store(true, Ordering::SeqCst);
        self.inner.wanted.store(false, Ordering::SeqCst);
        self.inner.stop_watcher();
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
        self.inner.stop_watcher();
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

    fn indexed_files(e: &Engine, n: usize) -> bool {
        e.counts().map(|c| c.vectors >= n && c.pending == 0).unwrap_or(false)
    }

    /// The three notes plus a Markdown file, so there is a second extension to filter on.
    fn notes_and_md(dir: &Path) {
        notes(dir);
        std::fs::write(dir.join("d.md"), "# Herons\n\nA heron stood motionless in the reeds.").unwrap();
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
    fn extension_filter_returns_only_that_extension() {
        let tmp = tempfile::tempdir().unwrap();
        let docs = tmp.path().join("docs");
        notes_and_md(&docs);
        let e = engine(tmp.path());
        let rx = e.subscribe();
        e.add_folder(docs.clone()).unwrap();
        e.start_indexing().unwrap();
        wait_for("indexing", || indexed_files(&e, 4));
        wait_for("idle", || e.status() == Status::Idle);
        drop(rx);

        let txt = e.search("ext:txt", 10, None).unwrap();
        assert_eq!(txt.len(), 3, "{txt:?}");
        assert!(txt.iter().all(|h| h.path.ends_with(".txt")), "{txt:?}");

        let md = e.search("ext:md", 10, None).unwrap();
        assert_eq!(md.len(), 1, "{md:?}");
        assert!(md[0].path.ends_with("d.md"));

        // Words and a filter together: the word still has to be in the file.
        let both = e.search("heron ext:txt", 10, None).unwrap();
        assert!(both.iter().all(|h| h.path.ends_with(".txt")), "{both:?}");
    }

    #[test]
    fn size_filter_below_one_byte_returns_nothing() {
        let tmp = tempfile::tempdir().unwrap();
        let docs = tmp.path().join("docs");
        notes_and_md(&docs);
        let e = engine(tmp.path());
        e.add_folder(docs.clone()).unwrap();
        e.start_indexing().unwrap();
        wait_for("indexing", || indexed_files(&e, 4));
        wait_for("idle", || e.status() == Status::Idle);

        assert!(e.search("size:<1b", 10, None).unwrap().is_empty());
        assert!(e.search("walrus size:<1b", 10, None).unwrap().is_empty());
        assert_eq!(e.search("size:>1b", 10, None).unwrap().len(), 4);
    }

    #[test]
    fn limit_applies_after_filtering() {
        let tmp = tempfile::tempdir().unwrap();
        let docs = tmp.path().join("docs");
        notes_and_md(&docs);
        let e = engine(tmp.path());
        e.add_folder(docs.clone()).unwrap();
        e.start_indexing().unwrap();
        wait_for("indexing", || indexed_files(&e, 4));
        wait_for("idle", || e.status() == Status::Idle);

        // The only Markdown file is not the first unfiltered hit for a word that appears in the notes,
        // yet with a limit of 1 the filter still finds it.
        let unfiltered = e.search("walrus", 1, None).unwrap();
        assert!(unfiltered[0].path.ends_with("a.txt"), "{unfiltered:?}");
        let md = e.search("ext:md", 1, None).unwrap();
        assert_eq!(md.len(), 1);
        assert!(md[0].path.ends_with("d.md"), "{md:?}");

        let two = e.search("ext:txt", 2, None).unwrap();
        assert_eq!(two.len(), 2, "{two:?}");
        assert!(two.iter().all(|h| h.path.ends_with(".txt")));
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
    fn searcher_is_kept_between_queries_and_refreshed_after_a_change() {
        let tmp = tempfile::tempdir().unwrap();
        let docs = tmp.path().join("docs");
        notes(&docs);
        let e = engine(tmp.path());
        e.add_folder(docs.clone()).unwrap();
        e.start_indexing().unwrap();
        wait_for("indexing", || indexed(&e));
        wait_for("idle", || e.status() == Status::Idle);
        let loads = || e.inner.searcher_loads.load(Ordering::SeqCst);
        e.search("walrus", 3, None).unwrap();
        let first = loads();
        assert_eq!(first, 1, "first query builds the searcher");
        for q in ["ostrich", "zebra", "walrus"] {
            e.search(q, 3, None).unwrap();
        }
        assert_eq!(loads(), first, "unchanged index: the cached searcher is reused");
        std::fs::write(docs.join("d.txt"), "A pelican scooped fish from the harbour.").unwrap();
        e.start_indexing().unwrap();
        wait_for("new file", || e.search("pelican", 3, None).map(|h| h.first().is_some_and(|h| h.path.ends_with("d.txt"))).unwrap_or(false));
        assert_eq!(loads(), first, "a commit refreshes the searcher instead of reloading every vector");
        assert!(e.inner.searcher_refreshes.load(Ordering::SeqCst) >= 1, "the commit led to a refresh");
        e.shutdown();
    }

    #[test]
    fn watcher_picks_up_a_new_file_without_a_manual_sync() {
        let tmp = tempfile::tempdir().unwrap();
        let docs = tmp.path().join("docs");
        notes(&docs);
        let e = engine(tmp.path());
        e.set_watch(true, WatchPolicy::Live).unwrap(); // the default policy waits for AC power; a test box may not be
        e.add_folder(docs.clone()).unwrap();
        e.start_indexing().unwrap();
        wait_for("indexing", || indexed(&e));
        wait_for("watcher", || e.watch_info().running);
        std::fs::write(docs.join("d.txt"), "A pelican scooped fish from the harbour.").unwrap();
        wait_for("watcher sync", || {
            e.search("pelican", 3, None).map(|h| h.first().is_some_and(|h| h.path.ends_with("d.txt"))).unwrap_or(false)
        });
        e.pause();
        assert!(!e.watch_info().running, "pausing stops the watcher");
        e.shutdown();
    }

    #[test]
    fn watch_policy_names_round_trip_and_defaults_are_conservative() {
        for p in [WatchPolicy::Live, WatchPolicy::PluggedIn, WatchPolicy::Manual] {
            assert_eq!(WatchPolicy::parse(p.as_str()), Some(p));
        }
        assert_eq!(WatchPolicy::parse("sometimes"), None);
        let tmp = tempfile::tempdir().unwrap();
        let e = engine(tmp.path());
        assert_eq!(e.watch_settings(), (true, WatchPolicy::PluggedIn), "watch on, battery-friendly by default");
        assert!(!e.usage_tracking(), "open-history is off by default");
        assert_eq!(e.watch_info(), WatchInfo::default(), "nothing runs before indexing is started");
    }

    #[test]
    fn manual_policy_records_but_does_not_sync() {
        let tmp = tempfile::tempdir().unwrap();
        let docs = tmp.path().join("docs");
        notes(&docs);
        let e = engine(tmp.path());
        e.set_watch(true, WatchPolicy::Manual).unwrap();
        e.add_folder(docs.clone()).unwrap();
        e.start_indexing().unwrap();
        wait_for("indexing", || indexed(&e));
        wait_for("watcher", || e.watch_info().running);
        std::fs::write(docs.join("d.txt"), "A pelican scooped fish from the harbour.").unwrap();
        wait_for("change recorded", || e.watch_info().pending > 0);
        // The reason appears once the change has settled (the debounce), a moment after it is recorded.
        wait_for("deferred reason", || e.watch_info().deferred.is_some_and(|d| d.contains("manual")));
        std::thread::sleep(Duration::from_millis(1500));
        assert!(e.search("pelican", 3, None).unwrap().iter().all(|h| !h.path.ends_with("d.txt")), "nothing syncs on its own");
        e.shutdown();
    }

    #[test]
    fn open_history_is_off_by_default_and_boosts_when_on() {
        let tmp = tempfile::tempdir().unwrap();
        let docs = tmp.path().join("docs");
        std::fs::create_dir_all(&docs).unwrap();
        std::fs::write(docs.join("a.txt"), "Harbour boats and fishing nets.").unwrap();
        std::fs::write(docs.join("b.txt"), "Harbour boats and fishing nets.").unwrap(); // same words, tie on meaning
        let e = engine(tmp.path());
        e.add_folder(docs.clone()).unwrap();
        e.start_indexing().unwrap();
        wait_for("indexing", || e.counts().map(|c| c.vectors >= 2 && c.pending == 0).unwrap_or(false));
        let b = docs.join("b.txt").to_string_lossy().into_owned();
        e.record_open(&b).unwrap(); // tracking is off: ignored
        assert!(!tmp.path().join("index/usage.sqlite").exists(), "no history file while tracking is off");
        let plain: Vec<String> = e.search("harbour boats", 5, None).unwrap().into_iter().map(|h| h.path).collect();
        e.set_usage_tracking(true).unwrap();
        for _ in 0..3 {
            e.record_open(&b).unwrap();
        }
        let boosted = e.search("harbour boats", 5, None).unwrap();
        assert_eq!(boosted[0].path, b, "the opened file ranks first");
        assert_eq!(boosted.len(), plain.len());
        e.set_usage_tracking(false).unwrap();
        let after: Vec<String> = e.search("harbour boats", 5, None).unwrap().into_iter().map(|h| h.path).collect();
        assert_eq!(after, plain, "turning it off restores the plain order and drops the history");
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
