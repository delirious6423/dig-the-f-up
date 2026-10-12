//! `Watcher`: owns the OS watch (notify) and one event thread that drives the `Scheduler`.
//!
//! Why one thread and a channel: notify delivers events on its own thread, so every event is funnelled into
//! one `mpsc` queue. The thread alone decides when the sink is called, which keeps ordering simple and lets
//! `Drop` stop it deterministically with a `Stop` message.
//!
//! Why a fallback instead of an error: a laptop can run out of inotify watches or lose a root. The index
//! must still re-sync periodically, so setup failure degrades to `PeriodicOnly` and reports why via `mode()`.

use std::fs;
use std::path::PathBuf;
use std::sync::mpsc::{self, RecvTimeoutError};
use std::sync::{Arc, Mutex, MutexGuard};
use std::thread::JoinHandle;

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
    Stop,
}

pub struct Watcher {
    shared: Arc<Mutex<Scheduler>>,
    mode: Mode,
    last_error: Arc<Mutex<Option<String>>>,
    tx: Option<mpsc::Sender<Msg>>,
    handle: Option<JoinHandle<()>>,
    // Held only to keep the OS watch alive; dropped after the thread is told to stop.
    _notify: Option<RecommendedWatcher>,
}

impl Watcher {
    /// Starts watching `roots` (main index) and `cfg.code_roots` (code projects).
    ///
    /// Setup failures do not return an error; they produce `Mode::PeriodicOnly`. Returns `Err` only if
    /// the event thread cannot be spawned.
    pub fn start(
        roots: &[PathBuf],
        mut cfg: WatchConfig,
        sink: Arc<dyn SyncSink>,
        power: Arc<dyn PowerSource>,
        clock: Arc<dyn Clock>,
    ) -> Result<Self> {
        // Canonicalize so notify's event paths (which are real paths) match by prefix. On macOS /var is a
        // symlink to /private/var, and a raw /var root would silently match nothing.
        let mut fallback: Option<String> = None;
        let main_roots = canonical_all(roots, &mut fallback);
        cfg.code_roots = canonical_all(&cfg.code_roots.clone(), &mut fallback);
        let mut watch_list = main_roots.clone();
        for c in &cfg.code_roots {
            if !watch_list.contains(c) {
                watch_list.push(c.clone());
            }
        }

        let (tx, rx) = mpsc::channel::<Msg>();
        let clock_now = clock.now();
        let shared = Arc::new(Mutex::new(Scheduler::new(
            cfg.clone(),
            main_roots,
            clock_now,
        )));
        let last_error = Arc::new(Mutex::new(None::<String>));

        let mut notify_watcher = None;
        if fallback.is_none() {
            let tx_cb = tx.clone();
            match notify::recommended_watcher(move |res| {
                let _ = tx_cb.send(Msg::Event(res));
            }) {
                Ok(mut w) => {
                    for root in &watch_list {
                        if let Err(e) = w.watch(root, RecursiveMode::Recursive) {
                            fallback = Some(describe_watch_error(&e, root));
                            break;
                        }
                    }
                    if fallback.is_none() {
                        notify_watcher = Some(w);
                    }
                }
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

        let thread_shared = Arc::clone(&shared);
        let thread_err = Arc::clone(&last_error);
        let handle = std::thread::Builder::new()
            .name("digup-watch".into())
            .spawn(move || {
                event_loop(rx, thread_shared, thread_err, sink, power, clock);
            })?;

        Ok(Self {
            shared,
            mode,
            last_error,
            tx: Some(tx),
            handle: Some(handle),
            _notify: notify_watcher,
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
        // Drop the OS watch before joining so no further callbacks can queue behind Stop.
        self._notify.take();
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
            Err(RecvTimeoutError::Timeout) => {}
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

fn canonical_all(paths: &[PathBuf], fallback: &mut Option<String>) -> Vec<PathBuf> {
    paths
        .iter()
        .map(|p| match fs::canonicalize(p) {
            Ok(c) => c,
            Err(e) => {
                if fallback.is_none() {
                    *fallback = Some(format!(
                        "cannot open root {}: {e}; periodic re-sync only",
                        p.display()
                    ));
                }
                p.clone()
            }
        })
        .collect()
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

    #[test]
    fn missing_root_falls_back_to_periodic_only() {
        let sink = Arc::new(CountingSink::default());
        let w = Watcher::start(
            &[PathBuf::from("/definitely/not/a/real/root/digup")],
            WatchConfig::default(),
            sink,
            Arc::new(AlwaysAc),
            Arc::new(SystemClock::new()),
        )
        .expect("start");
        match w.mode() {
            Mode::PeriodicOnly { reason } => assert!(reason.contains("not/a/real"), "{reason}"),
            Mode::Events => panic!("expected fallback"),
        }
        assert_eq!(w.pending_count(), 0);
        w.stop();
    }
}
