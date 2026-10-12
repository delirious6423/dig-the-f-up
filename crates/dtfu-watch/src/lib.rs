//! Filesystem change watcher that turns OS events into sync requests.
//!
//! Why this crate exists: the index must stay fresh without rescanning everything, but a laptop must not
//! burn battery or disk on every save. This crate coalesces bursts, waits for code projects to settle,
//! honours a power policy and falls back to periodic re-sync when OS events are unavailable.
//!
//! It deliberately does not depend on the engine or index crates. The engine implements `SyncSink`.

use std::time::Duration;

mod clock;
mod coalesce;
mod event_id;
mod power;
mod scheduler;
#[cfg(test)]
mod testing;
mod watcher;

pub use clock::{Clock, SystemClock};
pub use event_id::{EventIdStore, FileEventIdStore};
#[cfg(target_os = "macos")]
pub use power::MacPowerSource;
pub use power::{default_power_source, AlwaysAc, PowerPolicy, PowerSource, PowerState};
pub use scheduler::{DeferReason, WatchConfig};
pub use watcher::{Mode, Watcher};

/// Receives sync requests from the watcher. Implementations must be quick: the event thread calls them.
///
/// The engine implements this for `dtfu_index::Control`, forwarding to `request_sync` and
/// `request_code_sync`. Calls are requests, not guarantees: the sink may coalesce them.
pub trait SyncSink: Send + Sync {
    /// Main index changed (or a periodic or wake re-sync is due).
    fn request_sync(&self);
    /// Code projects changed and have been quiet for the configured window.
    fn request_code_sync(&self) {}
}

/// Default periodic re-sync interval, used by `WatchConfig::default`.
pub const DEFAULT_RESYNC_INTERVAL: Duration = Duration::from_secs(3600);
