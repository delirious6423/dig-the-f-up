//! Fakes shared by unit tests: a settable clock, a settable power source and a counting sink.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Mutex;

use crate::power::{PowerSource, PowerState};
use crate::SyncSink;

pub struct FakePower {
    state: Mutex<PowerState>,
}

impl FakePower {
    pub fn new(state: PowerState) -> Self {
        Self {
            state: Mutex::new(state),
        }
    }
    pub fn set(&self, state: PowerState) {
        *self.state.lock().unwrap() = state;
    }
}

impl PowerSource for FakePower {
    fn state(&self) -> PowerState {
        *self.state.lock().unwrap()
    }
}

#[derive(Default)]
pub struct CountingSink {
    pub syncs: AtomicUsize,
    pub code_syncs: AtomicUsize,
}

impl SyncSink for CountingSink {
    fn request_sync(&self) {
        self.syncs.fetch_add(1, Ordering::SeqCst);
    }
    fn request_code_sync(&self) {
        self.code_syncs.fetch_add(1, Ordering::SeqCst);
    }
}
