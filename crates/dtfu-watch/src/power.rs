//! Power state and the policy that decides when a sync may fire.
//!
//! Spec: docs/porting/spec-pipeline.md 2.8 (PowerPolicy.swift:54-63). Low Power Mode always counts as "do not
//! start background work", matching the Swift backfill blocker. Thermal state is not read (no stable API
//! on Linux or Windows; the spec calls it optional).

use std::sync::Arc;
#[cfg(target_os = "macos")]
use std::{
    io::Read,
    process::{Command, Stdio},
    time::{Duration, Instant},
};

/// Coarse power state as far as sync scheduling cares.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PowerState {
    OnAc,
    OnBattery,
    /// Low Power Mode is on (regardless of the power source).
    LowPower,
}

/// What the watcher is allowed to do with power state.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum PowerPolicy {
    /// Fire on any power state.
    #[default]
    Live,
    /// Fire only on AC power and outside Low Power Mode.
    PluggedInOnly,
    /// Never fire on its own. Changes are still recorded and kept pending.
    Manual,
}

/// Reports the current power state. Implementations must be cheap or cache internally; the scheduler
/// calls them at most once per `WatchConfig::power_poll`.
pub trait PowerSource: Send + Sync {
    fn state(&self) -> PowerState;
}

/// Power source for platforms without a power implementation. Always reports AC so nothing is deferred.
#[derive(Debug, Default, Clone, Copy)]
pub struct AlwaysAc;

impl PowerSource for AlwaysAc {
    fn state(&self) -> PowerState {
        PowerState::OnAc
    }
}

/// Best-effort power source for the current platform (`MacPowerSource` on macOS, `AlwaysAc` elsewhere).
pub fn default_power_source() -> Arc<dyn PowerSource> {
    #[cfg(target_os = "macos")]
    {
        Arc::new(MacPowerSource::new())
    }
    #[cfg(not(target_os = "macos"))]
    {
        Arc::new(AlwaysAc)
    }
}

/// Parses `pmset -g batt`. Returns `Some(true)` on battery, `Some(false)` on AC, `None` if unrecognised.
#[cfg(any(target_os = "macos", test))]
pub(crate) fn parse_batt_on_battery(out: &str) -> Option<bool> {
    if out.contains("'Battery Power'") {
        Some(true)
    } else if out.contains("'AC Power'") {
        Some(false)
    } else {
        None
    }
}

/// Parses `pmset -g`: true if any section reports `lowpowermode 1`.
#[cfg(any(target_os = "macos", test))]
pub(crate) fn parse_low_power(out: &str) -> bool {
    out.lines().any(|l| {
        let mut it = l.split_whitespace();
        it.next() == Some("lowpowermode") && it.next() == Some("1")
    })
}

/// macOS power source: shells out to `pmset` with a timeout. A failed or slow `pmset` reports AC, so a
/// broken probe never blocks syncing forever.
#[cfg(target_os = "macos")]
#[derive(Debug, Clone, Copy)]
pub struct MacPowerSource {
    timeout: Duration,
}

#[cfg(target_os = "macos")]
impl MacPowerSource {
    pub fn new() -> Self {
        Self {
            timeout: Duration::from_secs(2),
        }
    }
}

#[cfg(target_os = "macos")]
impl Default for MacPowerSource {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(target_os = "macos")]
impl PowerSource for MacPowerSource {
    fn state(&self) -> PowerState {
        if let Some(out) = run_capped("pmset", &["-g"], self.timeout) {
            if parse_low_power(&out) {
                return PowerState::LowPower;
            }
        }
        match run_capped("pmset", &["-g", "batt"], self.timeout)
            .as_deref()
            .and_then(parse_batt_on_battery)
        {
            Some(true) => PowerState::OnBattery,
            _ => PowerState::OnAc,
        }
    }
}

/// Runs a command and returns stdout, or `None` on spawn failure, non-exit within `timeout` (then killed).
#[cfg(target_os = "macos")]
fn run_capped(program: &str, args: &[&str], timeout: Duration) -> Option<String> {
    let mut child = Command::new(program)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .ok()?;
    let deadline = Instant::now() + timeout;
    loop {
        match child.try_wait() {
            Ok(Some(_)) => {
                // pmset output is a few hundred bytes, far below the pipe buffer, so reading after exit is safe.
                let mut s = String::new();
                child.stdout.take()?.read_to_string(&mut s).ok()?;
                return Some(s);
            }
            Ok(None) if Instant::now() < deadline => std::thread::sleep(Duration::from_millis(10)),
            Ok(None) => {
                let _ = child.kill();
                let _ = child.wait();
                return None;
            }
            Err(_) => return None,
        }
    }
}

/// Whether `state` lets a sync fire under `policy`. `Manual` never allows it.
pub(crate) fn allowed(policy: PowerPolicy, state: PowerState) -> bool {
    match policy {
        PowerPolicy::Live => true,
        PowerPolicy::PluggedInOnly => state == PowerState::OnAc,
        PowerPolicy::Manual => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_pmset_batt() {
        assert_eq!(
            parse_batt_on_battery("Now drawing from 'Battery Power'\n -InternalBattery-0"),
            Some(true)
        );
        assert_eq!(
            parse_batt_on_battery("Now drawing from 'AC Power'"),
            Some(false)
        );
        assert_eq!(parse_batt_on_battery("garbage"), None);
    }

    #[test]
    fn parses_low_power_line() {
        assert!(parse_low_power("Battery Power:\n lowpowermode         1\n"));
        assert!(!parse_low_power(" lowpowermode         0\n sleep 1\n"));
        assert!(!parse_low_power("lowpowermode"));
    }

    #[test]
    fn policy_gate() {
        assert!(allowed(PowerPolicy::Live, PowerState::OnBattery));
        assert!(allowed(PowerPolicy::Live, PowerState::LowPower));
        assert!(!allowed(PowerPolicy::PluggedInOnly, PowerState::OnBattery));
        assert!(!allowed(PowerPolicy::PluggedInOnly, PowerState::LowPower));
        assert!(allowed(PowerPolicy::PluggedInOnly, PowerState::OnAc));
        assert!(!allowed(PowerPolicy::Manual, PowerState::OnAc));
    }
}
