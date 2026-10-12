//! Issue #8: sniffing must never open a FIFO or device, because read() on a FIFO with no writer blocks forever.
#![cfg(unix)]
use dtfu_crawl::{classify, Classification, CrawlOptions, FileKind};
use std::process::Command;
use std::sync::mpsc;
use std::time::Duration;
use tempfile::TempDir;

#[test]
fn fifo_named_ts_does_not_block_classify() {
    let d = TempDir::new().unwrap();
    let fifo = d.path().join("clip.ts");
    let status = Command::new("mkfifo").arg(&fifo).status().expect("run mkfifo");
    assert!(status.success(), "mkfifo failed");

    let opts = CrawlOptions { sniff_magic: true, ..Default::default() };
    let (tx, rx) = mpsc::channel();
    let p = fifo.clone();
    // The thread is detached on purpose: if classify blocks, the test fails at the timeout instead of hanging.
    std::thread::spawn(move || {
        let r = classify(&p, &opts);
        let _ = tx.send(r);
    });
    let r = rx.recv_timeout(Duration::from_secs(2)).expect("classify blocked on a FIFO named clip.ts");
    // Not a regular file, so no sniff result: falls back to the extension rule (.ts without a transport stream).
    assert_ne!(r, Classification::Index(FileKind::Video));
}

#[test]
fn fifo_with_unknown_extension_does_not_block_magic_sniff() {
    let d = TempDir::new().unwrap();
    let fifo = d.path().join("blob.xyz");
    let status = Command::new("mkfifo").arg(&fifo).status().expect("run mkfifo");
    assert!(status.success(), "mkfifo failed");

    let opts = CrawlOptions { sniff_magic: true, ..Default::default() };
    let (tx, rx) = mpsc::channel();
    let p = fifo.clone();
    std::thread::spawn(move || {
        let _ = tx.send(classify(&p, &opts));
    });
    let r = rx.recv_timeout(Duration::from_secs(2)).expect("classify blocked on a FIFO with unknown extension");
    assert_eq!(r, Classification::Ignore);
}
