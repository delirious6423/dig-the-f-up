//! Scheduler: classifies paths, runs the coalescer, adds the periodic and wake-from-sleep re-syncs, and applies
//! the power gate. Pure state machine (no threads); `Watcher` drives it from its event thread.
//!
//! Core guarantee: the watcher always records. A request that the power gate blocks is kept as an `owed` flag
//! and fires on a later `tick` once the gate opens, so deferral never drops a change.

use std::fmt;
use std::path::{Path, PathBuf};
use std::time::Duration;

use crate::coalesce::{CoalesceConfig, Coalescer, Lane, Request};
use crate::power::{allowed, PowerPolicy, PowerSource, PowerState};

/// Developer folders the main index never crawls (the crawler's `SKIPPED_FOLDER_NAMES`).
pub(crate) const MAIN_SKIP_FOLDERS: &[&str] = dtfu_crawl::rules::SKIPPED_FOLDER_NAMES;

/// Extra folders skipped for code projects (docs/porting/spec-pipeline.md 2.8, Engine.swift codeNoise).
pub(crate) const CODE_NOISE_FOLDERS: &[&str] = &[
    "node_modules",
    "bower_components",
    "venv",
    "site-packages",
    "__pycache__",
    "DerivedData",
    "Pods",
    "Carthage",
    "vendor",
    "Vendor",
    "third_party",
    "build",
    "Build",
    "dist",
    "out",
    "target",
    "obj",
    "coverage",
    "wandb",
    "mlruns",
    "lightning_logs",
];

#[derive(Debug, Clone)]
pub struct WatchConfig {
    pub policy: PowerPolicy,
    /// Main lane quiet time before a sync is requested (spec: 0.75 s).
    pub debounce: Duration,
    /// Code lane quiet time before a path settles (spec: 60 s).
    pub quiet_window: Duration,
    /// Hard cap since the first event, both lanes (spec: 600 s).
    pub max_wait: Duration,
    /// Pending paths above this become one rescan (default 5000).
    pub storm_cap: usize,
    /// Periodic safety re-sync interval (default 1 h). Also the wake-from-sleep threshold is twice this.
    pub resync_interval: Duration,
    /// How often power is re-read while a request is owed, and the longest the event thread sleeps.
    pub power_poll: Duration,
    /// Roots indexed as code projects. Paths under these use the code lane and its noise list.
    pub code_roots: Vec<PathBuf>,
    /// Paths whose events are dropped, with everything under them (the index folder, app data, OS folders).
    pub ignored: Vec<PathBuf>,
    /// How often root availability is probed (default 10 s).
    pub availability_interval: Duration,
    /// Longest a single availability or volume probe may take before the root counts as offline (default 3 s).
    pub probe_timeout: Duration,
    /// Forced rescan interval for network roots, which get no events for remote changes (default 15 min).
    pub network_resync_interval: Duration,
}

impl Default for WatchConfig {
    fn default() -> Self {
        Self {
            policy: PowerPolicy::Live,
            debounce: Duration::from_millis(750),
            quiet_window: Duration::from_secs(60),
            max_wait: Duration::from_secs(600),
            storm_cap: 5000,
            resync_interval: crate::DEFAULT_RESYNC_INTERVAL,
            power_poll: Duration::from_secs(30),
            code_roots: Vec::new(),
            ignored: Vec::new(),
            availability_interval: Duration::from_secs(10),
            probe_timeout: Duration::from_secs(3),
            network_resync_interval: Duration::from_secs(900),
        }
    }
}

/// Why owed work is not being delivered right now.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeferReason {
    /// Policy is `Manual`; nothing fires on its own.
    Manual,
    /// Policy is `PluggedInOnly` and the machine is on battery.
    OnBattery,
    /// Policy is `PluggedInOnly` and Low Power Mode is on.
    LowPowerMode,
}

impl fmt::Display for DeferReason {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            DeferReason::Manual => "manual policy (changes kept pending)",
            DeferReason::OnBattery => "on battery (changes kept pending)",
            DeferReason::LowPowerMode => "Low Power Mode (changes kept pending)",
        })
    }
}

/// Requests to forward to the sink from one `tick`.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct Fire {
    pub sync: bool,
    pub code_sync: bool,
}

impl Fire {
    #[cfg(test)]
    pub fn any(self) -> bool {
        self.sync || self.code_sync
    }
}

#[derive(Debug)]
pub struct Scheduler {
    cfg: WatchConfig,
    roots: Vec<PathBuf>,
    coalescer: Coalescer,
    owed_sync: bool,
    owed_code: bool,
    next_resync: Duration,
    last_wall: Option<Duration>,
    power_cache: Option<(Duration, PowerState)>,
    deferred: Option<DeferReason>,
    fired: u64,
}

impl Scheduler {
    /// `roots` are the main roots; `cfg.code_roots` are the code roots. Both must already be canonical.
    pub fn new(cfg: WatchConfig, roots: Vec<PathBuf>, now: Duration) -> Self {
        let coalescer = Coalescer::new(CoalesceConfig {
            debounce: cfg.debounce,
            quiet: cfg.quiet_window,
            max_wait: cfg.max_wait,
            storm_cap: cfg.storm_cap,
        });
        let next_resync = now + cfg.resync_interval;
        Self {
            cfg,
            roots,
            coalescer,
            owed_sync: false,
            owed_code: false,
            next_resync,
            last_wall: None,
            power_cache: None,
            deferred: None,
            fired: 0,
        }
    }

    /// Records one changed path. Paths outside every root, and noise paths, are dropped here.
    pub fn on_path(&mut self, path: &Path, now: Duration) {
        if let Some(lane) = classify(path, &self.roots, &self.cfg.code_roots, &self.cfg.ignored) {
            self.coalescer.record(path.to_path_buf(), lane, now);
        }
    }

    /// Swaps a root's path (for example given path -> canonical path once an offline root appears).
    pub fn replace_root(&mut self, old: &Path, new: &Path) {
        for r in self.roots.iter_mut().chain(self.cfg.code_roots.iter_mut()) {
            if r == old {
                *r = new.to_path_buf();
            }
        }
    }

    /// The OS reported dropped events: schedule a full rescan.
    pub fn force_rescan(&mut self, now: Duration) {
        self.coalescer.request_rescan(now);
    }

    /// Advances the scheduler. Call on every event and at least every `power_poll`.
    ///
    /// `wall` is the wall clock, used only to detect a sleep (a jump larger than twice `resync_interval`).
    pub fn tick(&mut self, now: Duration, wall: Duration, power: &dyn PowerSource) -> Fire {
        let interval = self.cfg.resync_interval;
        if let Some(prev) = self.last_wall {
            if wall.saturating_sub(prev) > interval * 2 {
                // Woke from sleep: whatever changed while asleep may have been missed.
                self.owed_sync = true;
                self.next_resync = now + interval;
            }
        }
        self.last_wall = Some(wall);

        for req in self.coalescer.poll(now) {
            match req {
                Request::Sync => self.owed_sync = true,
                Request::CodeSync => self.owed_code = true,
                Request::RescanRoots => {
                    self.owed_sync = true;
                    self.owed_code = true;
                }
            }
        }
        if now >= self.next_resync {
            self.owed_sync = true;
            self.next_resync = now + interval;
        }

        if !self.owed_sync && !self.owed_code {
            self.deferred = None;
            return Fire::default();
        }
        match self.gate(now, power) {
            Ok(()) => {
                let fire = Fire {
                    sync: self.owed_sync,
                    code_sync: self.owed_code,
                };
                self.owed_sync = false;
                self.owed_code = false;
                self.deferred = None;
                self.fired += 1;
                fire
            }
            Err(reason) => {
                self.deferred = Some(reason);
                Fire::default()
            }
        }
    }

    fn gate(&mut self, now: Duration, power: &dyn PowerSource) -> Result<(), DeferReason> {
        match self.cfg.policy {
            PowerPolicy::Live => Ok(()),
            PowerPolicy::Manual => Err(DeferReason::Manual),
            PowerPolicy::PluggedInOnly => {
                let state = self.power_state(now, power);
                if allowed(PowerPolicy::PluggedInOnly, state) {
                    Ok(())
                } else if state == PowerState::LowPower {
                    Err(DeferReason::LowPowerMode)
                } else {
                    Err(DeferReason::OnBattery)
                }
            }
        }
    }

    /// Re-reads power at most once per `power_poll`, so a busy event stream does not shell out per event.
    fn power_state(&mut self, now: Duration, power: &dyn PowerSource) -> PowerState {
        if let Some((at, st)) = self.power_cache {
            if now.saturating_sub(at) < self.cfg.power_poll {
                return st;
            }
        }
        let st = power.state();
        self.power_cache = Some((now, st));
        st
    }

    /// Number of changes and owed requests not yet delivered to the sink.
    pub fn pending_count(&self) -> usize {
        self.coalescer.pending_paths()
            + usize::from(self.coalescer.rescan_pending())
            + usize::from(self.owed_sync)
            + usize::from(self.owed_code)
    }

    /// Why owed work is held back, or `None` when nothing is held back.
    pub fn deferred_reason(&self) -> Option<DeferReason> {
        if self.pending_count() == 0 {
            None
        } else {
            self.deferred
        }
    }

    /// Number of times the sink was called (for status display).
    #[cfg(test)]
    pub fn fired_count(&self) -> u64 {
        self.fired
    }

    /// How long the event thread may sleep from `now` before it must call `tick` again.
    pub fn next_wake(&self, now: Duration) -> Duration {
        let mut wake = self.next_resync.min(now + self.cfg.power_poll);
        if let Some(d) = self.coalescer.next_deadline() {
            wake = wake.min(d);
        }
        wake.saturating_sub(now).max(Duration::from_millis(1))
    }
}

/// Which lane a path belongs to, or `None` if it should be ignored. Code roots win over main roots.
pub(crate) fn classify(path: &Path, roots: &[PathBuf], code_roots: &[PathBuf], ignored: &[PathBuf]) -> Option<Lane> {
    if ignored.iter().any(|p| path.starts_with(p)) {
        return None;
    }
    if let Some(rel) = relative_under(path, code_roots) {
        return (!is_noise(rel, CODE_NOISE_FOLDERS)).then_some(Lane::Code);
    }
    let rel = relative_under(path, roots)?;
    (!is_noise(rel, MAIN_SKIP_FOLDERS)).then_some(Lane::Main)
}

fn relative_under<'a>(path: &'a Path, roots: &[PathBuf]) -> Option<&'a Path> {
    roots.iter().find_map(|r| path.strip_prefix(r).ok())
}

/// True if any component below the root is hidden (starts with `.`) or is in `names`.
fn is_noise(rel: &Path, names: &[&str]) -> bool {
    rel.components().any(|c| {
        let s = c.as_os_str().to_string_lossy();
        s.starts_with('.') || names.contains(&s.as_ref())
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing::FakePower;

    fn ms(n: u64) -> Duration {
        Duration::from_millis(n)
    }
    fn secs(n: u64) -> Duration {
        Duration::from_secs(n)
    }
    fn roots() -> (Vec<PathBuf>, Vec<PathBuf>) {
        (
            vec![PathBuf::from("/w/main")],
            vec![PathBuf::from("/w/code")],
        )
    }
    fn sched(policy: PowerPolicy) -> Scheduler {
        let (r, c) = roots();
        Scheduler::new(
            WatchConfig {
                policy,
                code_roots: c,
                ..WatchConfig::default()
            },
            r,
            Duration::ZERO,
        )
    }

    #[test]
    fn noise_folders_and_hidden_paths_are_ignored() {
        let (r, c) = roots();
        let none: &[PathBuf] = &[];
        assert_eq!(
            classify(Path::new("/w/main/node_modules/x.js"), &r, &c, none),
            None
        );
        assert_eq!(classify(Path::new("/w/main/.git/config"), &r, &c, none), None);
        assert_eq!(classify(Path::new("/w/main/.DS_Store"), &r, &c, none), None);
        assert_eq!(classify(Path::new("/w/code/target/debug/a"), &r, &c, none), None);
        assert_eq!(classify(Path::new("/w/code/vendor/lib.go"), &r, &c, none), None);
        assert_eq!(classify(Path::new("/elsewhere/a.txt"), &r, &c, none), None);
        // Main index does index build/ and target/ documents; only code projects skip them.
        assert_eq!(
            classify(Path::new("/w/main/build/report.pdf"), &r, &c, none),
            Some(Lane::Main)
        );
        assert_eq!(
            classify(Path::new("/w/main/notes/a.md"), &r, &c, none),
            Some(Lane::Main)
        );
        assert_eq!(
            classify(Path::new("/w/code/src/main.rs"), &r, &c, none),
            Some(Lane::Code)
        );
    }

    #[test]
    fn events_under_ignored_prefixes_are_dropped() {
        // Non-hidden names, so only the ignore list can drop these events.
        let (r, c) = (vec![PathBuf::from("/home/u")], Vec::new());
        let ign = vec![PathBuf::from("/home/u/DigUp/index"), PathBuf::from("/home/u/Library")];
        let idx = Path::new("/home/u/DigUp/index/main.sqlite-wal");
        assert_eq!(classify(idx, &r, &c, &[]), Some(Lane::Main), "control: not dropped without the list");
        assert_eq!(classify(idx, &r, &c, &ign), None);
        assert_eq!(classify(Path::new("/home/u/Library/Application Support/x"), &r, &c, &ign), None);
        // A sibling that only shares a string prefix is not under the ignored folder.
        assert_eq!(classify(Path::new("/home/u/DigUp/index2/a.txt"), &r, &c, &ign), Some(Lane::Main));
        assert_eq!(classify(Path::new("/home/u/LibraryNotes/a.md"), &r, &c, &ign), Some(Lane::Main));
        assert_eq!(classify(Path::new("/home/u/docs/a.md"), &r, &c, &ign), Some(Lane::Main));
    }

    #[test]
    fn normal_file_under_root_is_main_lane() {
        let (r, c) = (vec![PathBuf::from("/home/u")], Vec::new());
        let ign = vec![PathBuf::from("/home/u/.dtfu")];
        assert_eq!(classify(Path::new("/home/u/notes/a.md"), &r, &c, &ign), Some(Lane::Main));
    }

    #[test]
    fn noise_paths_never_reach_the_coalescer() {
        let mut s = sched(PowerPolicy::Live);
        s.on_path(Path::new("/w/main/node_modules/a.js"), Duration::ZERO);
        s.on_path(Path::new("/w/code/target/x"), Duration::ZERO);
        assert_eq!(s.pending_count(), 0);
    }

    #[test]
    fn burst_fires_one_sync_on_live_policy() {
        let mut s = sched(PowerPolicy::Live);
        let p = FakePower::new(PowerState::OnAc);
        for i in 0..20u64 {
            s.on_path(Path::new(&format!("/w/main/{i}.txt")), ms(i * 10));
        }
        assert_eq!(s.tick(ms(500), ms(500), &p), Fire::default());
        assert_eq!(
            s.tick(ms(190 + 750), ms(940), &p),
            Fire {
                sync: true,
                code_sync: false
            }
        );
        assert_eq!(s.pending_count(), 0);
        assert_eq!(s.deferred_reason(), None);
    }

    #[test]
    fn battery_defers_then_fires_on_ac_without_losing_changes() {
        let mut s = sched(PowerPolicy::PluggedInOnly);
        let p = FakePower::new(PowerState::OnBattery);
        s.on_path(Path::new("/w/main/a.txt"), Duration::ZERO);
        assert_eq!(s.tick(ms(750), ms(750), &p), Fire::default());
        assert_eq!(s.deferred_reason(), Some(DeferReason::OnBattery));
        assert_eq!(s.pending_count(), 1);

        // More changes arrive while on battery; they must join the owed request, not vanish.
        s.on_path(Path::new("/w/main/b.txt"), secs(5));
        assert_eq!(s.tick(secs(10), secs(10), &p), Fire::default());

        // Plug in. Power is cached for 30 s, so the change is seen only after the next poll.
        p.set(PowerState::OnAc);
        assert_eq!(
            s.tick(secs(20), secs(20), &p),
            Fire::default(),
            "cached battery reading"
        );
        assert_eq!(
            s.tick(secs(31), secs(31), &p),
            Fire {
                sync: true,
                code_sync: false
            }
        );
        assert_eq!(s.deferred_reason(), None);
        assert_eq!(s.pending_count(), 0);
        assert_eq!(s.fired_count(), 1, "both changes delivered in one sync");
    }

    #[test]
    fn low_power_mode_defers_plugged_in_policy() {
        let mut s = sched(PowerPolicy::PluggedInOnly);
        let p = FakePower::new(PowerState::LowPower);
        s.on_path(Path::new("/w/main/a.txt"), Duration::ZERO);
        assert_eq!(s.tick(ms(750), ms(750), &p), Fire::default());
        assert_eq!(s.deferred_reason(), Some(DeferReason::LowPowerMode));
    }

    #[test]
    fn live_policy_fires_on_battery() {
        let mut s = sched(PowerPolicy::Live);
        let p = FakePower::new(PowerState::OnBattery);
        s.on_path(Path::new("/w/main/a.txt"), Duration::ZERO);
        assert_eq!(
            s.tick(ms(750), ms(750), &p),
            Fire {
                sync: true,
                code_sync: false
            }
        );
    }

    #[test]
    fn manual_policy_never_fires() {
        let mut s = sched(PowerPolicy::Manual);
        let p = FakePower::new(PowerState::OnAc);
        s.on_path(Path::new("/w/main/a.txt"), Duration::ZERO);
        s.on_path(Path::new("/w/code/src/a.rs"), Duration::ZERO);
        for t in 1..=10_000u64 {
            assert!(!s.tick(secs(t), secs(t), &p).any(), "fired at {t}s");
        }
        assert_eq!(s.deferred_reason(), Some(DeferReason::Manual));
        assert!(s.pending_count() >= 2, "changes stay pending");
    }

    #[test]
    fn periodic_resync_fires_every_interval() {
        let mut s = sched(PowerPolicy::Live);
        let p = FakePower::new(PowerState::OnAc);
        assert!(!s.tick(secs(3599), secs(3599), &p).any());
        assert_eq!(
            s.tick(secs(3600), secs(3600), &p),
            Fire {
                sync: true,
                code_sync: false
            }
        );
        assert!(!s.tick(secs(7199), secs(7199), &p).any());
        assert_eq!(
            s.tick(secs(7200), secs(7200), &p),
            Fire {
                sync: true,
                code_sync: false
            }
        );
    }

    #[test]
    fn periodic_resync_obeys_power_gate() {
        let mut s = sched(PowerPolicy::PluggedInOnly);
        let p = FakePower::new(PowerState::OnBattery);
        assert!(!s.tick(secs(3600), secs(3600), &p).any());
        assert_eq!(s.deferred_reason(), Some(DeferReason::OnBattery));
        p.set(PowerState::OnAc);
        assert_eq!(
            s.tick(secs(3630), secs(3630), &p),
            Fire {
                sync: true,
                code_sync: false
            }
        );
    }

    #[test]
    fn wake_from_sleep_triggers_resync() {
        let mut s = sched(PowerPolicy::Live);
        let p = FakePower::new(PowerState::OnAc);
        assert!(!s.tick(secs(60), secs(60), &p).any());
        // Monotonic time barely moved (macOS does not count sleep) but the wall clock jumped 3 h.
        assert_eq!(
            s.tick(secs(90), secs(60 + 3 * 3600), &p),
            Fire {
                sync: true,
                code_sync: false
            }
        );
    }

    #[test]
    fn code_changes_fire_code_sync_after_quiet_window() {
        let mut s = sched(PowerPolicy::Live);
        let p = FakePower::new(PowerState::OnAc);
        s.on_path(Path::new("/w/code/src/a.rs"), Duration::ZERO);
        assert!(!s.tick(secs(59), secs(59), &p).any());
        assert_eq!(
            s.tick(secs(60), secs(60), &p),
            Fire {
                sync: false,
                code_sync: true
            }
        );
    }

    #[test]
    fn next_wake_is_bounded_by_power_poll() {
        let s = sched(PowerPolicy::Live);
        assert_eq!(s.next_wake(Duration::ZERO), secs(30));
    }
}
