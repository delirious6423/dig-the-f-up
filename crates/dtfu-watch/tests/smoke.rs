//! Real-filesystem smoke test. Skipped (with a message) when the OS watcher cannot start, for example in a
//! sandbox without file-event support.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use dtfu_watch::{AlwaysAc, Mode, SyncSink, SystemClock, WatchConfig, Watcher};

#[derive(Default)]
struct Counting(AtomicUsize);

impl SyncSink for Counting {
    fn request_sync(&self) {
        self.0.fetch_add(1, Ordering::SeqCst);
    }
}

#[test]
fn real_file_change_requests_a_sync() {
    let dir = tempfile::tempdir().unwrap();
    let sink = Arc::new(Counting::default());
    let watcher = Watcher::start(
        &[dir.path().to_path_buf()],
        WatchConfig::default(),
        sink.clone(),
        Arc::new(AlwaysAc),
        Arc::new(SystemClock::new()),
    )
    .expect("start watcher");

    if !matches!(watcher.mode(), Mode::Events) {
        eprintln!("skipping: OS events unavailable: {:?}", watcher.mode());
        return;
    }

    std::fs::write(dir.path().join("hello.txt"), b"hi").unwrap();

    // Generous: FSEvents can take a second or two to deliver, plus the 750 ms debounce.
    let deadline = Instant::now() + Duration::from_secs(20);
    while sink.0.load(Ordering::SeqCst) == 0 && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(100));
    }
    assert!(
        sink.0.load(Ordering::SeqCst) >= 1,
        "no sync requested within 20 s"
    );
    watcher.stop();
}
