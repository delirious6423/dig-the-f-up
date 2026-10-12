//! Time source, injected so the scheduler can be driven with a fake clock in tests.

use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

/// Two timelines. `now` drives debounce, quiet windows and timers. `wall` exists only to detect
/// wake-from-sleep: on macOS `Instant` is backed by `mach_absolute_time`, which does not advance while the
/// machine sleeps, so a monotonic gap cannot reveal a sleep. Wall time does advance across sleep.
pub trait Clock: Send + Sync {
    /// Monotonic time since an arbitrary origin.
    fn now(&self) -> Duration;
    /// Wall-clock time since the Unix epoch.
    fn wall(&self) -> Duration;
}

/// Real clock backed by `Instant` and `SystemTime`.
#[derive(Debug)]
pub struct SystemClock {
    origin: Instant,
}

impl SystemClock {
    pub fn new() -> Self {
        Self {
            origin: Instant::now(),
        }
    }
}

impl Default for SystemClock {
    fn default() -> Self {
        Self::new()
    }
}

impl Clock for SystemClock {
    fn now(&self) -> Duration {
        self.origin.elapsed()
    }

    fn wall(&self) -> Duration {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
    }
}
