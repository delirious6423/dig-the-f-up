//! `Watcher`: owns the OS watch (notify) and one event thread that drives the `Scheduler`.
//!
//! Why one thread and a channel: notify delivers events on its own thread, so every event is funnelled into
//! one `mpsc` queue. The thread alone decides when the sink is called, which keeps ordering simple and lets
//! `Drop` stop it deterministically with a `Stop` message.
//!
//! Why a fallback instead of an error: a laptop can run out of inotify watches or lose a root. The index
//! must still re-sync periodically, so setup failure degrades to `PeriodicOnly` and reports why via `mode()`.

use std::collections::HashMap;
use std::fs;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{self, RecvTimeoutError};
use std::sync::{Arc, Mutex, MutexGuard, TryLockError};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use anyhow::Result;
use notify::{Event, EventKind, RecommendedWatcher, RecursiveMode, Watcher as _};

use crate::clock::Clock;
use crate::power::PowerSource;
use crate::scheduler::{DeferReason, Scheduler, WatchConfig};
use crate::SyncSink;

/// How changes are being observed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Mode {
    /// OS file events are live; periodic and wake re-syncs also run.
    Events,
    /// No OS events; only periodic and wake re-syncs run. `reason` says why.
    PeriodicOnly { reason: String },
}

enum Msg {
    Event(notify::Result<Event>),
    /// Re-run the scheduler tick now (a rescan was requested from another thread).
    Wake,
    Stop,
}

/// One notify watcher per root (keyed by root index, so roots sharing a canonical path never remove each other's), so a hung share can only block its own registration. The lock is held only
/// for map insert/remove, never across `watch` or a watcher drop (both can block on a dead share).
type SharedNotify = Arc<Mutex<HashMap<usize, RecommendedWatcher>>>;

/// Drops watchers on a detached thread: dropping one can block on a dead share.
fn drop_off_thread<T: Send + 'static>(v: T) {
    let _ = std::thread::Builder::new()
        .name("dtfu-watch-drop".into())
        .spawn(move || drop(v));
}
type RootStatus = Arc<Mutex<Vec<(PathBuf, bool)>>>;

enum PollMsg {
    Stop,
    Probe { round: u64, idx: usize, ok: bool },
    /// Volume id and network-ness of an online root; `None` when the query timed out or failed.
    Vol { idx: usize, generation: u64, info: Option<(String, bool)> },
}

pub struct Watcher {
    shared: Arc<Mutex<Scheduler>>,
    mode: Mode,
    last_error: Arc<Mutex<Option<String>>>,
    tx: Option<mpsc::Sender<Msg>>,
    handle: Option<JoinHandle<()>>,
    poll_tx: Option<mpsc::Sender<PollMsg>>,
    poll_handle: Option<JoinHandle<()>>,
    status: RootStatus,
    // Keeps the OS watch alive; shared with the availability poller, which re-adds watches.
    notify: SharedNotify,
}

impl Watcher {
    /// Starts watching `roots` (main index) and `cfg.code_roots` (code projects).
    ///
    /// Setup failures do not return an error; they produce `Mode::PeriodicOnly`. Returns `Err` only if
    /// the event thread cannot be spawned.
    pub fn start(
        roots: &[PathBuf],
        cfg: WatchConfig,
        sink: Arc<dyn SyncSink>,
        power: Arc<dyn PowerSource>,
        clock: Arc<dyn Clock>,
    ) -> Result<Self> {
        // No blocking filesystem calls here: a dead share would hang the caller. Every root starts offline
        // under its given path; the poller's first round canonicalizes and registers it through the bounded
        // `try_recover` path, and `Scheduler::replace_root` swaps in the canonical path (notify event paths
        // are real paths; on macOS /var is a symlink to /private/var).
        let mut fallback: Option<String> = None;
        let main_paths: Vec<PathBuf> = roots.to_vec();
        let mut tracked: Vec<(PathBuf, bool)> = Vec::new();
        for p in roots.iter().chain(cfg.code_roots.iter()) {
            if !tracked.iter().any(|(t, _)| t == p) {
                tracked.push((p.clone(), false));
            }
        }

        let (tx, rx) = mpsc::channel::<Msg>();
        let clock_now = clock.now();
        let shared = Arc::new(Mutex::new(Scheduler::new(
            cfg.clone(),
            main_paths,
            clock_now,
        )));
        let last_error = Arc::new(Mutex::new(None::<String>));

        // Creating a watcher touches no path, so it cannot block; it only decides the mode. Each root gets
        // its own watcher later, on its recovery helper thread.
        {
            match notify::recommended_watcher(|_res| {}) {
                Ok(_probe) => {}
                Err(e) => {
                    fallback = Some(format!(
                        "cannot start file watcher: {e}; periodic re-sync only"
                    ))
                }
            }
        }
        let mode = match fallback {
            None => Mode::Events,
            Some(reason) => Mode::PeriodicOnly { reason },
        };

        let notify: SharedNotify = Arc::new(Mutex::new(HashMap::new()));
        let status: RootStatus = Arc::new(Mutex::new(
            tracked.iter().map(|(p, ok)| (p.clone(), !*ok)).collect(),
        ));
        let (poll_tx, poll_rx) = mpsc::channel::<PollMsg>();
        let (ready_tx, ready_rx) = mpsc::channel::<()>();
        let poller = Poller {
            rx: poll_rx,
            tx: poll_tx.clone(),
            cfg: cfg.clone(),
            roots: tracked
                .iter()
                .map(|(p, ok)| RootState {
                    given: p.clone(),
                    path: p.clone(),
                    epoch: Arc::new(AtomicU64::new(0)),
                    watched: false,
                    next_watch: Instant::now(),
                    online: *ok,
                    inflight: Arc::new(AtomicBool::new(false)),
                    vol_inflight: Arc::new(AtomicBool::new(false)),
                    rec_flag: Arc::new(AtomicBool::new(false)),
                    rec_rx: None,
                    generation: 0,
                    vol_id: None,
                    vol_changed: false,
                    network: None,
                    next_net: Instant::now() + cfg.network_resync_interval,
                })
                .collect(),
            shared: Arc::clone(&shared),
            notify: Arc::clone(&notify),
            status: Arc::clone(&status),
            wake: tx.clone(),
            clock: Arc::clone(&clock),
            last_error: Arc::clone(&last_error),
            events_ok: matches!(mode, Mode::Events),
            ready: Some(ready_tx),
        };

        let thread_shared = Arc::clone(&shared);
        let thread_err = Arc::clone(&last_error);
        let handle = std::thread::Builder::new()
            .name("dtfu-watch".into())
            .spawn(move || {
                event_loop(rx, thread_shared, thread_err, sink, power, clock);
            })?;
        let poll_handle = std::thread::Builder::new()
            .name("dtfu-watch-avail".into())
            .spawn(move || poller.run())?;
        // Let the first availability round register the roots, but never wait longer than one probe timeout.
        let _ = ready_rx.recv_timeout(cfg.probe_timeout);

        Ok(Self {
            shared,
            mode,
            last_error,
            tx: Some(tx),
            handle: Some(handle),
            poll_tx: Some(poll_tx),
            poll_handle: Some(poll_handle),
            status,
            notify,
        })
    }

    pub fn mode(&self) -> &Mode {
        &self.mode
    }

    /// Changes and owed requests not yet delivered to the sink.
    pub fn pending_count(&self) -> usize {
        self.lock().pending_count()
    }

    /// Why pending work is held back, if it is.
    pub fn deferred_reason(&self) -> Option<DeferReason> {
        self.lock().deferred_reason()
    }

    /// Roots (main and code) that were unreachable at the last availability check.
    pub fn offline_roots(&self) -> Vec<PathBuf> {
        self.status
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .iter()
            .filter(|(_, off)| *off)
            .map(|(p, _)| p.clone())
            .collect()
    }

    /// Last runtime error reported by notify (for example a dropped event batch), if any.
    pub fn last_error(&self) -> Option<String> {
        self.last_error
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }

    /// Stops the event thread and releases the OS watch. Same as dropping the watcher.
    pub fn stop(self) {
        drop(self);
    }

    fn lock(&self) -> MutexGuard<'_, Scheduler> {
        self.shared.lock().unwrap_or_else(|e| e.into_inner())
    }
}

impl Drop for Watcher {
    fn drop(&mut self) {
        if let Some(tx) = self.tx.take() {
            let _ = tx.send(Msg::Stop);
        }
        if let Some(tx) = self.poll_tx.take() {
            let _ = tx.send(PollMsg::Stop);
        }
        if let Some(h) = self.poll_handle.take() {
            // Bounded wait: never hang on a poller stuck in a dead share; detach instead.
            let end = Instant::now() + Duration::from_secs(2);
            while !h.is_finished() && Instant::now() < end {
                std::thread::sleep(Duration::from_millis(5));
            }
            if h.is_finished() {
                let _ = h.join();
            }
        }
        // Drop the OS watch before joining so no further callbacks can queue behind Stop. A recovery helper
        // stuck in `watch` may hold the mutex; then skip (the watch dies with the detached helper's Arc).
        if let Some(mut g) = lock_within(&self.notify, Duration::from_secs(1)) {
            let all = std::mem::take(&mut *g);
            drop(g);
            drop_off_thread(all);
        }
        if let Some(h) = self.handle.take() {
            let _ = h.join();
        }
    }
}

fn event_loop(
    rx: mpsc::Receiver<Msg>,
    shared: Arc<Mutex<Scheduler>>,
    last_error: Arc<Mutex<Option<String>>>,
    sink: Arc<dyn SyncSink>,
    power: Arc<dyn PowerSource>,
    clock: Arc<dyn Clock>,
) {
    loop {
        let wait = {
            let s = shared.lock().unwrap_or_else(|e| e.into_inner());
            s.next_wake(clock.now())
        };
        match rx.recv_timeout(wait) {
            Ok(Msg::Stop) | Err(RecvTimeoutError::Disconnected) => return,
            Ok(Msg::Event(res)) => {
                let now = clock.now();
                let mut s = shared.lock().unwrap_or_else(|e| e.into_inner());
                match res {
                    Ok(ev) => handle_event(&mut s, &ev, now),
                    Err(e) => {
                        // The OS dropped information (or the watch died): rescan rather than guess.
                        *last_error.lock().unwrap_or_else(|p| p.into_inner()) = Some(e.to_string());
                        s.force_rescan(now);
                    }
                }
            }
            Ok(Msg::Wake) | Err(RecvTimeoutError::Timeout) => {}
        }
        let now = clock.now();
        let fire = {
            let mut s = shared.lock().unwrap_or_else(|e| e.into_inner());
            s.tick(now, clock.wall(), power.as_ref())
        };
        // Call the sink outside the lock so a slow sink cannot block status queries.
        if fire.sync {
            sink.request_sync();
        }
        if fire.code_sync {
            sink.request_code_sync();
        }
    }
}

fn handle_event(s: &mut Scheduler, ev: &Event, now: std::time::Duration) {
    if matches!(ev.kind, EventKind::Access(_)) {
        return;
    }
    if ev.need_rescan() {
        s.force_rescan(now);
        return;
    }
    for p in &ev.paths {
        s.on_path(p, now);
    }
}

/// Locks `m` but gives up after `d`. Used wherever the poller or Drop touches the notify mutex, because a
/// recovery helper stuck in a dead share's `watch` call may hold it indefinitely.
fn lock_within<T>(m: &Mutex<T>, d: Duration) -> Option<MutexGuard<'_, T>> {
    let end = Instant::now() + d;
    loop {
        match m.try_lock() {
            Ok(g) => return Some(g),
            Err(TryLockError::Poisoned(p)) => return Some(p.into_inner()),
            Err(TryLockError::WouldBlock) => {
                if Instant::now() >= end {
                    return None;
                }
                std::thread::sleep(Duration::from_millis(5));
            }
        }
    }
}

#[cfg(test)]
fn registration_hook(path: &std::path::Path) {
    static PANIC_ONCE: AtomicBool = AtomicBool::new(true);
    if path.to_string_lossy().contains("panic-once-") && PANIC_ONCE.swap(false, Ordering::SeqCst) {
        panic!("test hook: recovery helper panics");
    }
    if path.to_string_lossy().contains("hang-") {
        std::thread::sleep(Duration::from_secs(10));
    }
}
#[cfg(not(test))]
fn registration_hook(_path: &std::path::Path) {}

/// Outcome of a recovery helper: everything that may block on a half-alive share.
struct RecResult {
    given: PathBuf,
    canon: PathBuf,
    vol_id: Option<String>,
    network: Option<bool>,
    /// The OS watch is registered (or events are off, so none is needed).
    watched: bool,
}

/// Resets a flag on drop, so a panicking helper still frees its slot.
struct FlagGuard(Arc<AtomicBool>);
impl Drop for FlagGuard {
    fn drop(&mut self) {
        self.0.store(false, Ordering::SeqCst);
    }
}

struct RootState {
    /// The path the caller passed in; what `offline_roots` reports.
    given: PathBuf,
    /// Current path: canonical once the root has been recovered.
    path: PathBuf,
    /// Bumped on offline/swap; a recovery helper registers its watcher only if this is unchanged.
    epoch: Arc<AtomicU64>,
    /// The OS watch is registered (or events are off). An online root without it gets periodic rescans only.
    watched: bool,
    next_watch: Instant,
    online: bool,
    inflight: Arc<AtomicBool>,
    /// Volume probe in flight (separate so a slow volume query never marks the root offline).
    vol_inflight: Arc<AtomicBool>,
    /// A recovery helper is still running for this root.
    rec_flag: Arc<AtomicBool>,
    rec_rx: Option<mpsc::Receiver<RecResult>>,
    /// Bumped on every online transition so stale volume answers are ignored.
    generation: u64,
    /// Volume id recorded when the root came online; `None` until known.
    vol_id: Option<String>,
    vol_changed: bool,
    /// `None` means unknown (classification timed out); retried on later polls.
    network: Option<bool>,
    next_net: Instant,
}

struct Poller {
    rx: mpsc::Receiver<PollMsg>,
    tx: mpsc::Sender<PollMsg>,
    cfg: WatchConfig,
    roots: Vec<RootState>,
    shared: Arc<Mutex<Scheduler>>,
    notify: SharedNotify,
    status: RootStatus,
    wake: mpsc::Sender<Msg>,
    clock: Arc<dyn Clock>,
    last_error: Arc<Mutex<Option<String>>>,
    events_ok: bool,
    /// Signalled once when the first round has finished.
    ready: Option<mpsc::Sender<()>>,
}

impl Poller {
    fn run(mut self) {
        let mut round = 0u64;
        let mut first = true;
        loop {
            if !first {
                match self.rx.recv_timeout(self.cfg.availability_interval) {
                    Ok(PollMsg::Stop) | Err(RecvTimeoutError::Disconnected) => return,
                    Ok(m) => {
                        self.absorb(m);
                        continue;
                    }
                    Err(RecvTimeoutError::Timeout) => {}
                }
            }
            first = false;
            round += 1;
            let Some(results) = self.probe_round(round) else {
                return;
            };
            let mut changed = false;
            for (i, ok) in results.into_iter().enumerate() {
                if ok && self.roots[i].online && self.roots[i].vol_changed {
                    // Drive swapped between polls: the OS watch is dead. Treat as offline -> online.
                    self.went_offline(i);
                    changed = true;
                }
                if !ok {
                    if self.roots[i].rec_rx.take().is_some() {
                        // Late helper results are ignored, and its watcher must not be registered.
                        self.roots[i].epoch.fetch_add(1, Ordering::SeqCst);
                    }
                    if self.roots[i].online {
                        changed = true;
                        self.went_offline(i);
                    }
                } else if !self.roots[i].online
                    || (self.events_ok
                        && !self.roots[i].watched
                        && Instant::now() >= self.roots[i].next_watch)
                {
                    match self.try_recover(i) {
                        None => return,
                        Some(true) => changed = true,
                        Some(false) => {}
                    }
                } else if self.roots[i].network == Some(true)
                    && Instant::now() >= self.roots[i].next_net
                {
                    changed = true;
                    self.roots[i].next_net = Instant::now() + self.cfg.network_resync_interval;
                }
            }
            if changed {
                self.rescan();
            }
            if let Some(r) = self.ready.take() {
                let _ = r.send(());
            }
        }
    }

    fn absorb(&mut self, m: PollMsg) {
        if let PollMsg::Vol { idx, generation, info } = m {
            let Some(r) = self.roots.get_mut(idx) else { return };
            if !r.online || r.generation != generation {
                return;
            }
            let Some((id, network)) = info else { return }; // unknown: keep and retry next poll
            r.network = Some(network);
            match &r.vol_id {
                None => r.vol_id = Some(id),
                Some(old) => {
                    let (op, np) = (id_kind(old), id_kind(&id));
                    if op == np {
                        if *old != id {
                            r.vol_changed = true;
                        }
                    } else if op == "dev" && np == "uuid" {
                        r.vol_id = Some(id); // a failed UUID lookup earlier, not a swap
                    }
                }
            }
        }
    }

    /// Probes every root concurrently; at most one outstanding probe per root. `None` means stop.
    fn probe_round(&mut self, round: u64) -> Option<Vec<bool>> {
        let mut results = vec![false; self.roots.len()];
        let mut waiting = 0usize;
        for (idx, r) in self.roots.iter().enumerate() {
            if r.online && !r.vol_inflight.swap(true, Ordering::SeqCst) {
                let flag = Arc::clone(&r.vol_inflight);
                let tx = self.tx.clone();
                let path = r.path.clone();
                let generation = r.generation;
                let spawned = std::thread::Builder::new()
                    .name("dtfu-watch-vol".into())
                    .spawn(move || {
                        // Blocking call on purpose: the flag clears only when it really returns, so a
                        // half-alive share cannot pile up one stuck thread per poll.
                        let _guard = FlagGuard(flag);
                        let info = dtfu_crawl::volume::volume_info(&path).map(|v| {
                            (v.id, v.kind == dtfu_crawl::volume::VolumeKind::Network)
                        });
                        let _ = tx.send(PollMsg::Vol { idx, generation, info });
                    });
                if spawned.is_err() {
                    r.vol_inflight.store(false, Ordering::SeqCst);
                }
            }
            if r.inflight.swap(true, Ordering::SeqCst) {
                continue; // previous probe still stuck: offline meanwhile
            }
            let flag = Arc::clone(&r.inflight);
            let tx = self.tx.clone();
            let path = r.path.clone();
            let spawned = std::thread::Builder::new()
                .name("dtfu-watch-probe".into())
                .spawn(move || {
                    let ok = fs::read_dir(&path).is_ok();
                    flag.store(false, Ordering::SeqCst);
                    let _ = tx.send(PollMsg::Probe { round, idx, ok });
                });
            if spawned.is_ok() {
                waiting += 1;
            } else {
                r.inflight.store(false, Ordering::SeqCst);
            }
        }
        let deadline = Instant::now() + self.cfg.probe_timeout;
        while waiting > 0 {
            let left = deadline.saturating_duration_since(Instant::now());
            match self.rx.recv_timeout(left) {
                Ok(PollMsg::Stop) | Err(RecvTimeoutError::Disconnected) => return None,
                Ok(PollMsg::Probe { round: r, idx, ok }) => {
                    if r == round {
                        results[idx] = ok;
                        waiting -= 1;
                    }
                }
                Ok(m) => self.absorb(m),
                Err(RecvTimeoutError::Timeout) => break,
            }
        }
        Some(results)
    }

    /// Canonicalize + volume query + watch registration for a returning root run on a helper thread, because
    /// each can block forever on a half-alive share. Returns `None` to stop, `Some(true)` if the root came
    /// online now, `Some(false)` if it is still pending (retried next poll once the helper has returned).
    fn try_recover(&mut self, i: usize) -> Option<bool> {
        if self.roots[i].rec_rx.is_none() {
            if self.roots[i].rec_flag.swap(true, Ordering::SeqCst) {
                return Some(false); // previous helper still stuck
            }
            let (tx, rx) = mpsc::channel::<RecResult>();
            let flag = Arc::clone(&self.roots[i].rec_flag);
            let given = self.roots[i].path.clone();
            let idx = i;
            let notify = Arc::clone(&self.notify);
            let event_tx = self.wake.clone();
            let epoch = Arc::clone(&self.roots[i].epoch);
            let my_epoch = epoch.load(Ordering::SeqCst);
            let last_error = Arc::clone(&self.last_error);
            let events_ok = self.events_ok;
            let spawned = std::thread::Builder::new()
                .name("dtfu-watch-recover".into())
                .spawn(move || {
                    let _guard = FlagGuard(flag);
                    let canon = fs::canonicalize(&given).unwrap_or_else(|_| given.clone());
                    // Blocking on purpose; the poller's wait is bounded and the flag clears when this returns.
                    let vol = dtfu_crawl::volume::volume_info(&canon);
                    let network = vol
                        .as_ref()
                        .map(|v| v.kind == dtfu_crawl::volume::VolumeKind::Network);
                    let vol_id = vol.map(|v| v.id);
                    let mut watched = !events_ok;
                    if events_ok {
                        let tx_cb = event_tx;
                        match notify::recommended_watcher(move |res| {
                            let _ = tx_cb.send(Msg::Event(res));
                        }) {
                            Ok(mut w) => {
                                registration_hook(&canon);
                                match w.watch(&canon, RecursiveMode::Recursive) {
                                    Ok(()) => {
                                        // Short critical section; epoch is checked under the lock so a
                                        // concurrent offline transition either sees the entry or wins.
                                        let mut leftover = Some(w);
                                        let mut old = None;
                                        if let Some(mut g) = lock_within(&notify, Duration::from_secs(1)) {
                                            if epoch.load(Ordering::SeqCst) == my_epoch {
                                                if let Some(w) = leftover.take() {
                                                    old = g.insert(idx, w);
                                                    watched = true;
                                                }
                                            }
                                        }
                                        if let Some(o) = old {
                                            drop_off_thread(o);
                                        }
                                        if let Some(w) = leftover {
                                            drop_off_thread(w);
                                        }
                                    }
                                    Err(e) => {
                                        *last_error.lock().unwrap_or_else(|p| p.into_inner()) =
                                            Some(describe_watch_error(&e, &canon));
                                        drop_off_thread(w);
                                    }
                                }
                            }
                            Err(e) => {
                                *last_error.lock().unwrap_or_else(|p| p.into_inner()) =
                                    Some(format!("cannot start file watcher: {e}"));
                            }
                        }
                    }
                    let _ = tx.send(RecResult { given, canon, vol_id, network, watched });
                });
            if spawned.is_err() {
                self.roots[i].rec_flag.store(false, Ordering::SeqCst);
                return Some(false);
            }
            self.roots[i].rec_rx = Some(rx);
        }
        let rx = self.roots[i].rec_rx.take()?;
        let deadline = Instant::now() + self.cfg.probe_timeout;
        loop {
            match rx.recv_timeout(Duration::from_millis(20)) {
                Ok(res) => {
                    let was_online = self.roots[i].online;
                    let watched = res.watched;
                    self.finish_recovery(i, res);
                    // A root that is online but still unwatched is not news; no rescan for it.
                    return Some(!was_online || watched);
                }
                Err(RecvTimeoutError::Disconnected) => return Some(false),
                Err(RecvTimeoutError::Timeout) => {}
            }
            loop {
                match self.rx.try_recv() {
                    Ok(PollMsg::Stop) | Err(mpsc::TryRecvError::Disconnected) => return None,
                    Ok(m) => self.absorb(m),
                    Err(mpsc::TryRecvError::Empty) => break,
                }
            }
            if Instant::now() >= deadline {
                self.roots[i].rec_rx = Some(rx); // stays offline; result is picked up on a later poll
                return Some(false);
            }
        }
    }

    fn finish_recovery(&mut self, i: usize, res: RecResult) {
        if res.canon != res.given {
            self.shared
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .replace_root(&res.given, &res.canon);
        }
        let watched = res.watched;
        let r = &mut self.roots[i];
        r.path = res.canon;
        r.network = res.network;
        r.vol_id = res.vol_id;
        r.vol_changed = false;
        r.generation += 1;
        r.next_net = Instant::now() + self.cfg.network_resync_interval;
        r.online = true;
        r.watched = watched;
        r.next_watch = Instant::now() + self.cfg.network_resync_interval;
        self.set_status(i, false);
    }

    fn went_offline(&mut self, i: usize) {
        self.roots[i].online = false;
        self.roots[i].vol_changed = false;
        self.roots[i].generation += 1;
        self.roots[i].watched = false;
        self.roots[i].next_watch = Instant::now();
        self.roots[i].epoch.fetch_add(1, Ordering::SeqCst);
        if let Some(mut g) = lock_within(&self.notify, self.cfg.probe_timeout) {
            let old = g.remove(&i);
            drop(g);
            if let Some(w) = old {
                drop_off_thread(w);
            }
        }
        self.set_status(i, true);
    }

    fn set_status(&self, i: usize, offline: bool) {
        let mut st = self.status.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(e) = st.get_mut(i) {
            *e = (self.roots[i].given.clone(), offline);
        }
    }

    fn rescan(&self) {
        self.shared
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .force_rescan(self.clock.now());
        let _ = self.wake.send(Msg::Wake);
    }
}

/// Kind prefix of a volume id (`dev`, `uuid`, `net`, ...). Ids of different kinds are not comparable.
fn id_kind(id: &str) -> &str {
    id.split(':').next().unwrap_or("")
}

fn describe_watch_error(e: &notify::Error, root: &std::path::Path) -> String {
    if matches!(e.kind, notify::ErrorKind::MaxFilesWatch) {
        let hint = if cfg!(target_os = "linux") {
            " (raise fs.inotify.max_user_watches)"
        } else {
            ""
        };
        format!(
            "watch limit reached for {}{hint}; periodic re-sync only",
            root.display()
        )
    } else {
        format!(
            "cannot watch {}: {e}; periodic re-sync only",
            root.display()
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::clock::SystemClock;
    use crate::power::AlwaysAc;
    use crate::testing::CountingSink;

    use std::sync::atomic::Ordering as O;

    fn fast_cfg() -> WatchConfig {
        WatchConfig {
            availability_interval: Duration::from_millis(50),
            probe_timeout: Duration::from_millis(500),
            debounce: Duration::from_millis(20),
            ..WatchConfig::default()
        }
    }

    fn wait_for(f: impl Fn() -> bool) -> bool {
        for _ in 0..100 {
            if f() {
                return true;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        false
    }

    #[test]
    fn missing_root_is_offline_then_recovers() {
        let tmp = tempfile::tempdir().unwrap();
        let good = tmp.path().join("good");
        let missing = tmp.path().join("missing");
        fs::create_dir(&good).unwrap();
        let sink = Arc::new(CountingSink::default());
        let w = Watcher::start(
            &[good, missing.clone()],
            fast_cfg(),
            sink.clone(),
            Arc::new(AlwaysAc),
            Arc::new(SystemClock::new()),
        )
        .expect("start");
        assert_eq!(*w.mode(), Mode::Events);
        // Registration may finish after `start` returns (start waits at most `probe_timeout` for the first round).
        assert!(wait_for(|| w.offline_roots() == vec![missing.clone()]));
        fs::create_dir(&missing).unwrap();
        assert!(wait_for(|| sink.syncs.load(O::SeqCst) > 0));
        assert!(wait_for(|| w.offline_roots().is_empty()));
        w.stop();
    }

    #[test]
    fn removed_root_goes_offline_and_syncs() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("root");
        fs::create_dir(&root).unwrap();
        let sink = Arc::new(CountingSink::default());
        let w = Watcher::start(
            std::slice::from_ref(&root),
            fast_cfg(),
            sink.clone(),
            Arc::new(AlwaysAc),
            Arc::new(SystemClock::new()),
        )
        .expect("start");
        assert!(wait_for(|| w.offline_roots().is_empty()));
        let before = sink.syncs.load(O::SeqCst);
        fs::remove_dir_all(&root).unwrap();
        assert!(wait_for(|| w.offline_roots().len() == 1));
        assert!(wait_for(|| sink.syncs.load(O::SeqCst) > before));
        w.stop();
    }

    #[test]
    fn drop_does_not_hang_on_blocked_registration() {
        let tmp = tempfile::tempdir().unwrap();
        let good = tmp.path().join("good");
        let hang = tmp.path().join("hang-share");
        fs::create_dir(&good).unwrap();
        let sink = Arc::new(CountingSink::default());
        let w = Watcher::start(
            &[good, hang.clone()],
            fast_cfg(),
            sink,
            Arc::new(AlwaysAc),
            Arc::new(SystemClock::new()),
        )
        .expect("start");
        assert!(wait_for(|| w.offline_roots().len() == 1));
        fs::create_dir(&hang).unwrap();
        std::thread::sleep(Duration::from_millis(400)); // helper is now sleeping inside registration
        let t = Instant::now();
        drop(w);
        assert!(t.elapsed() < Duration::from_secs(3), "drop took {:?}", t.elapsed());
    }

    #[test]
    fn start_with_hanging_root_returns_promptly_and_others_get_events() {
        let tmp = tempfile::tempdir().unwrap();
        let good = tmp.path().join("good");
        let hang = tmp.path().join("hang-share");
        fs::create_dir(&good).unwrap();
        fs::create_dir(&hang).unwrap();
        let sink = Arc::new(CountingSink::default());
        let cfg = fast_cfg();
        let limit = cfg.probe_timeout + Duration::from_secs(1);
        let t = Instant::now();
        let w = Watcher::start(
            &[good.clone(), hang],
            cfg,
            sink.clone(),
            Arc::new(AlwaysAc),
            Arc::new(SystemClock::new()),
        )
        .expect("start");
        assert!(t.elapsed() < limit, "start took {:?}", t.elapsed());
        assert_eq!(*w.mode(), Mode::Events);
        assert!(wait_for(|| w.offline_roots().len() == 1));
        let before = sink.syncs.load(O::SeqCst);
        fs::write(good.join("f.txt"), b"x").unwrap();
        assert!(wait_for(|| sink.syncs.load(O::SeqCst) > before));
        let t = Instant::now();
        drop(w);
        assert!(t.elapsed() < Duration::from_secs(3));
    }

    #[test]
    fn hanging_root_listed_first_does_not_block_good_root() {
        let tmp = tempfile::tempdir().unwrap();
        let good = tmp.path().join("good");
        let hang = tmp.path().join("hang-share");
        fs::create_dir(&good).unwrap();
        fs::create_dir(&hang).unwrap();
        let sink = Arc::new(CountingSink::default());
        let w = Watcher::start(
            &[hang, good.clone()],
            fast_cfg(),
            sink.clone(),
            Arc::new(AlwaysAc),
            Arc::new(SystemClock::new()),
        )
        .expect("start");
        assert_eq!(*w.mode(), Mode::Events);
        // Wait until the good root is registered and the start-up rescans are delivered (the count stays put for 1 s),
        // so the increase below can only come from a file event on the good root.
        assert!(wait_for(|| w.offline_roots().len() == 1 && !w.offline_roots()[0].ends_with("good")));
        let mut before = sink.syncs.load(O::SeqCst);
        loop {
            std::thread::sleep(Duration::from_secs(1));
            let now = sink.syncs.load(O::SeqCst);
            if now == before {
                break;
            }
            before = now;
        }
        fs::write(good.join("f.txt"), b"x").unwrap();
        assert!(wait_for(|| sink.syncs.load(O::SeqCst) > before));
        drop(w);
    }

    #[test]
    fn offline_roots_reports_given_paths() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("root");
        fs::create_dir(&root).unwrap();
        let w = Watcher::start(
            std::slice::from_ref(&root),
            fast_cfg(),
            Arc::new(CountingSink::default()),
            Arc::new(AlwaysAc),
            Arc::new(SystemClock::new()),
        )
        .expect("start");
        fs::remove_dir_all(&root).unwrap();
        assert!(wait_for(|| w.offline_roots() == vec![root.clone()]));
        w.stop();
    }

    #[test]
    fn volume_id_kinds_are_compared_only_within_a_kind() {
        assert_eq!(id_kind("dev:12"), "dev");
        assert_eq!(id_kind("uuid:abc"), "uuid");
        assert_ne!(id_kind("dev:12"), id_kind("uuid:12"));
    }

    #[test]
    fn panicking_recovery_helper_leaves_root_retryable() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("panic-once-root");
        fs::create_dir(&root).unwrap();
        let sink = Arc::new(CountingSink::default());
        let w = Watcher::start(
            &[root],
            fast_cfg(),
            sink,
            Arc::new(AlwaysAc),
            Arc::new(SystemClock::new()),
        )
        .expect("start");
        assert!(wait_for(|| w.offline_roots().is_empty()));
        w.stop();
    }
}
