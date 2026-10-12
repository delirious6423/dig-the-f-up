//! Pure event coalescer: per-path entries, debounce, quiet window, hard cap and the storm fallback.
//!
//! No threads and no clock of its own: every call takes `now`, so the whole state machine is unit-testable.
//! Rules follow docs/spec-pipeline.md 2.8 (Engine.swift:401-447):
//! - Main lane: a sync is due `debounce` (750 ms) after the last event, or `max_wait` after the first.
//! - Code lane: a path is settled after `quiet` (60 s) without events, or `max_wait` (600 s) after its first event.
//! - Storm: more than `storm_cap` pending paths drop the per-path entries and become one `RescanRoots`.

use std::collections::HashMap;
use std::path::PathBuf;
use std::time::Duration;

/// Which kind of root produced an event. Decides the window that applies.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Lane {
    Main,
    Code,
}

/// A request the coalescer emits once a window has closed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Request {
    /// Main index sync (covers all main-lane paths).
    Sync,
    /// Code index sync (at least one code-lane path settled).
    CodeSync,
    /// Storm: the per-path list was dropped; re-crawl every root.
    RescanRoots,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CoalesceConfig {
    pub debounce: Duration,
    pub quiet: Duration,
    pub max_wait: Duration,
    pub storm_cap: usize,
}

#[derive(Debug, Clone, Copy)]
struct Span {
    first: Duration,
    last: Duration,
}

#[derive(Debug, Clone, Copy)]
struct Entry {
    lane: Lane,
    span: Span,
}

#[derive(Debug)]
pub struct Coalescer {
    cfg: CoalesceConfig,
    entries: HashMap<PathBuf, Entry>,
    /// Set while a storm is waiting to settle. Per-path events are ignored (the rescan covers them).
    storm: Option<Span>,
}

impl Coalescer {
    pub fn new(cfg: CoalesceConfig) -> Self {
        Self {
            cfg,
            entries: HashMap::new(),
            storm: None,
        }
    }

    /// Records one changed path. A path already pending keeps one entry; only its `last` time moves.
    pub fn record(&mut self, path: PathBuf, lane: Lane, now: Duration) {
        if let Some(s) = self.storm.as_mut() {
            s.last = now;
            return;
        }
        self.entries
            .entry(path)
            .and_modify(|e| e.span.last = now)
            .or_insert(Entry {
                lane,
                span: Span {
                    first: now,
                    last: now,
                },
            });
        if self.entries.len() > self.cfg.storm_cap {
            self.start_storm(now);
        }
    }

    /// Forces a full rescan, e.g. when the OS reports that it dropped events (`Event::need_rescan`).
    pub fn request_rescan(&mut self, now: Duration) {
        self.start_storm(now);
    }

    fn start_storm(&mut self, now: Duration) {
        self.entries.clear();
        match self.storm.as_mut() {
            Some(s) => s.last = now,
            None => {
                self.storm = Some(Span {
                    first: now,
                    last: now,
                })
            }
        }
    }

    /// Returns the requests whose windows have closed at `now`. Each call consumes what it returns.
    pub fn poll(&mut self, now: Duration) -> Vec<Request> {
        if let Some(s) = self.storm {
            if due(s, self.cfg.debounce, now, self.cfg.max_wait) {
                self.storm = None;
                self.entries.clear();
                return vec![Request::RescanRoots];
            }
            return Vec::new();
        }
        let mut out = Vec::new();
        let main_due = self
            .main_span()
            .is_some_and(|s| due(s, self.cfg.debounce, now, self.cfg.max_wait));
        if main_due {
            // A full main sync re-crawls every main root, so it covers all pending main-lane paths.
            self.entries.retain(|_, e| e.lane != Lane::Main);
            out.push(Request::Sync);
        }
        let quiet = self.cfg.quiet;
        let before = self.entries.len();
        self.entries
            .retain(|_, e| e.lane != Lane::Code || !due(e.span, quiet, now, self.cfg.max_wait));
        if self.entries.len() != before {
            out.push(Request::CodeSync);
        }
        out
    }

    /// Earliest absolute time at which `poll` could return something. `None` when nothing is pending.
    pub fn next_deadline(&self) -> Option<Duration> {
        if let Some(s) = self.storm {
            return Some(deadline(s, self.cfg.debounce, self.cfg.max_wait));
        }
        self.entries
            .values()
            .map(|e| match e.lane {
                Lane::Main => deadline(e.span, self.cfg.debounce, self.cfg.max_wait),
                Lane::Code => deadline(e.span, self.cfg.quiet, self.cfg.max_wait),
            })
            .chain(
                self.main_span()
                    .map(|s| deadline(s, self.cfg.debounce, self.cfg.max_wait)),
            )
            .min()
    }

    /// One span over all main-lane entries. The main lane syncs as a whole, so its debounce must wait for
    /// the last main event of the burst, not for each path separately.
    fn main_span(&self) -> Option<Span> {
        self.entries
            .values()
            .filter(|e| e.lane == Lane::Main)
            .map(|e| e.span)
            .reduce(|a, b| Span {
                first: a.first.min(b.first),
                last: a.last.max(b.last),
            })
    }

    /// Number of paths pending (the storm flag is not counted; see `rescan_pending`).
    pub fn pending_paths(&self) -> usize {
        self.entries.len()
    }

    pub fn rescan_pending(&self) -> bool {
        self.storm.is_some()
    }
}

/// `window` is the quiet period for the entry's lane; the hard cap applies to every lane.
fn due(s: Span, window: Duration, now: Duration, max_wait: Duration) -> bool {
    now.saturating_sub(s.last) >= window || now.saturating_sub(s.first) >= max_wait
}

fn deadline(s: Span, window: Duration, max_wait: Duration) -> Duration {
    (s.last + window).min(s.first + max_wait)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ms(n: u64) -> Duration {
        Duration::from_millis(n)
    }
    fn secs(n: u64) -> Duration {
        Duration::from_secs(n)
    }
    fn cfg() -> CoalesceConfig {
        CoalesceConfig {
            debounce: ms(750),
            quiet: secs(60),
            max_wait: secs(600),
            storm_cap: 5000,
        }
    }

    #[test]
    fn same_path_keeps_one_entry() {
        let mut c = Coalescer::new(cfg());
        for i in 0..50 {
            c.record(PathBuf::from("/m/a.txt"), Lane::Main, ms(i));
        }
        assert_eq!(c.pending_paths(), 1);
    }

    #[test]
    fn burst_collapses_into_one_sync() {
        let mut c = Coalescer::new(cfg());
        for i in 0..100u64 {
            c.record(PathBuf::from(format!("/m/{i}.txt")), Lane::Main, ms(i * 5));
        }
        // Last event at 495 ms; nothing is due before 495 + 750 ms.
        assert!(c.poll(ms(1200)).is_empty());
        assert_eq!(c.poll(ms(1245)), vec![Request::Sync]);
        assert!(c.poll(ms(9999)).is_empty(), "one sync per burst");
        assert_eq!(c.pending_paths(), 0);
    }

    #[test]
    fn debounce_resets_on_each_event() {
        let mut c = Coalescer::new(cfg());
        c.record(PathBuf::from("/m/a"), Lane::Main, ms(0));
        c.record(PathBuf::from("/m/b"), Lane::Main, ms(500));
        c.record(PathBuf::from("/m/c"), Lane::Main, ms(1000));
        assert!(c.poll(ms(1250)).is_empty());
        assert_eq!(c.poll(ms(1750)), vec![Request::Sync]);
    }

    #[test]
    fn main_lane_ignores_the_quiet_window() {
        let mut c = Coalescer::new(cfg());
        c.record(PathBuf::from("/m/a"), Lane::Main, ms(0));
        assert_eq!(c.poll(ms(750)), vec![Request::Sync]);
    }

    #[test]
    fn code_lane_waits_for_quiet_window() {
        let mut c = Coalescer::new(cfg());
        c.record(PathBuf::from("/c/x.rs"), Lane::Code, ms(0));
        assert!(c.poll(ms(750)).is_empty());
        assert!(c.poll(ms(59_999)).is_empty());
        assert_eq!(c.poll(secs(60)), vec![Request::CodeSync]);
    }

    #[test]
    fn code_lane_hard_cap_600s_since_first_event() {
        let mut c = Coalescer::new(cfg());
        // Keeps changing every 30 s, so the quiet window never closes; only the cap can settle it.
        for i in 0..20u64 {
            c.record(PathBuf::from("/c/x.rs"), Lane::Code, secs(i * 30));
        }
        // Last event at 570 s, first at 0 s.
        assert!(c.poll(secs(599)).is_empty());
        assert_eq!(c.poll(secs(600)), vec![Request::CodeSync]);
        assert_eq!(c.pending_paths(), 0);
    }

    #[test]
    fn storm_cap_becomes_single_rescan() {
        let mut c = Coalescer::new(CoalesceConfig {
            storm_cap: 3,
            ..cfg()
        });
        for i in 0..4u64 {
            c.record(PathBuf::from(format!("/m/{i}")), Lane::Main, ms(i));
        }
        assert_eq!(c.pending_paths(), 0, "per-path entries dropped");
        assert!(c.rescan_pending());
        // Events during the storm only extend the quiet timer.
        c.record(PathBuf::from("/m/late"), Lane::Main, ms(900));
        assert_eq!(c.pending_paths(), 0);
        assert!(c.poll(ms(1500)).is_empty());
        assert_eq!(c.poll(ms(1650)), vec![Request::RescanRoots]);
        assert!(c.poll(ms(9000)).is_empty());
        assert!(!c.rescan_pending());
    }

    #[test]
    fn need_rescan_request_is_a_storm() {
        let mut c = Coalescer::new(cfg());
        c.request_rescan(ms(0));
        assert_eq!(c.poll(ms(750)), vec![Request::RescanRoots]);
    }

    #[test]
    fn next_deadline_tracks_earliest_window() {
        let mut c = Coalescer::new(cfg());
        assert_eq!(c.next_deadline(), None);
        c.record(PathBuf::from("/c/x"), Lane::Code, ms(0));
        c.record(PathBuf::from("/m/y"), Lane::Main, ms(100));
        assert_eq!(c.next_deadline(), Some(ms(850)));
    }
}
