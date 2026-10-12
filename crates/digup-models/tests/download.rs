//! Tests for locate/verify/download against a local HTTP server on 127.0.0.1. No real model files are touched.

use digup_models::{download, locate, verify, ModelFile, MODELS_ENV, REPOSITORY, REVISION};
use sha2::{Digest, Sha256};
use std::fs;
use std::io::{BufRead, BufReader, Write};
use std::net::{TcpListener, TcpStream};
use std::path::Path;
use std::sync::{Arc, Mutex, MutexGuard};
use std::thread;

const NAME: &str = "test-model.gguf";

static ENV_LOCK: Mutex<()> = Mutex::new(());

fn lock_env() -> MutexGuard<'static, ()> {
    ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner())
}

/// Sets environment variables for one test and restores the previous values on drop.
struct EnvGuard(Vec<(&'static str, Option<std::ffi::OsString>)>);

impl EnvGuard {
    fn set(vars: &[(&'static str, Option<&Path>)]) -> Self {
        let mut saved = Vec::new();
        for (key, value) in vars {
            saved.push((*key, std::env::var_os(key)));
            match value {
                Some(v) => std::env::set_var(key, v),
                None => std::env::remove_var(key),
            }
        }
        EnvGuard(saved)
    }
}

impl Drop for EnvGuard {
    fn drop(&mut self) {
        for (key, value) in self.0.drain(..) {
            match value {
                Some(v) => std::env::set_var(key, v),
                None => std::env::remove_var(key),
            }
        }
    }
}

fn content(len: usize) -> Vec<u8> {
    (0..len).map(|i| ((i * 31 + i / 251) % 253) as u8).collect()
}

fn sha_hex(data: &[u8]) -> String {
    Sha256::digest(data)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

fn model_for(data: &[u8]) -> ModelFile {
    let sha: &'static str = Box::leak(sha_hex(data).into_boxed_str());
    ModelFile {
        name: NAME,
        bytes: data.len() as u64,
        sha256: sha,
    }
}

/// What the server saw: the Range header of each request (None when absent).
type RangeLog = Arc<Mutex<Vec<Option<String>>>>;

struct Server {
    base: String,
    ranges: RangeLog,
}

/// Serves `/<name>` with `body`, and `/redirect/<name>` as a 302 to `/<name>`. With `ignore_range` the server
/// answers every request with 200 and the full body.
fn serve(body: Vec<u8>, ignore_range: bool) -> Server {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
    let addr = listener.local_addr().expect("addr");
    let ranges: RangeLog = Arc::new(Mutex::new(Vec::new()));
    let log = ranges.clone();
    thread::spawn(move || {
        for stream in listener.incoming().flatten() {
            handle(stream, &body, ignore_range, &log);
        }
    });
    Server {
        base: format!("http://{addr}"),
        ranges,
    }
}

impl Server {
    fn ranges(&self) -> Vec<Option<String>> {
        self.ranges.lock().unwrap().clone()
    }
}

fn handle(stream: TcpStream, body: &[u8], ignore_range: bool, log: &RangeLog) {
    let mut reader = BufReader::new(stream.try_clone().expect("clone"));
    let mut writer = stream;
    let mut request_line = String::new();
    if reader.read_line(&mut request_line).unwrap_or(0) == 0 {
        return;
    }
    let mut range = None;
    loop {
        let mut line = String::new();
        if reader.read_line(&mut line).unwrap_or(0) == 0 {
            break;
        }
        let line = line.trim_end();
        if line.is_empty() {
            break;
        }
        if let Some((key, value)) = line.split_once(':') {
            if key.eq_ignore_ascii_case("range") {
                range = Some(value.trim().to_string());
            }
        }
    }
    log.lock().unwrap().push(range.clone());

    let path = request_line
        .split_whitespace()
        .nth(1)
        .unwrap_or("/")
        .to_string();
    let path = path.trim_start_matches('/');
    if let Some(target) = path.strip_prefix("redirect/") {
        let location = format!("/{target}");
        respond(&mut writer, "302 Found", &[("Location", location)], &[]);
        return;
    }
    if path != NAME {
        respond(&mut writer, "404 Not Found", &[], b"not found");
        return;
    }
    let total = body.len();
    if ignore_range {
        respond(&mut writer, "200 OK", &[], body);
        return;
    }
    match range
        .as_deref()
        .and_then(|r| r.strip_prefix("bytes="))
        .and_then(|r| r.strip_suffix('-'))
    {
        Some(start) => {
            let start: usize = start.parse().expect("range start");
            if start >= total {
                respond(
                    &mut writer,
                    "416 Range Not Satisfiable",
                    &[("Content-Range", format!("bytes */{total}"))],
                    &[],
                );
            } else {
                respond(
                    &mut writer,
                    "206 Partial Content",
                    &[(
                        "Content-Range",
                        format!("bytes {start}-{}/{total}", total - 1),
                    )],
                    &body[start..],
                );
            }
        }
        None => respond(&mut writer, "200 OK", &[], body),
    }
}

fn respond(writer: &mut TcpStream, status: &str, headers: &[(&str, String)], body: &[u8]) {
    let mut head = format!("HTTP/1.1 {status}\r\n");
    for (k, v) in headers {
        head.push_str(&format!("{k}: {v}\r\n"));
    }
    head.push_str(&format!(
        "Content-Length: {}\r\nConnection: close\r\n\r\n",
        body.len()
    ));
    let _ = writer.write_all(head.as_bytes());
    let _ = writer.write_all(body);
    let _ = writer.flush();
}

#[test]
fn fresh_download_verifies_and_renames() {
    let data = content(2_500_000); // three 1 MiB chunks, last one partial
    let file = model_for(&data);
    let server = serve(data.clone(), false);
    let dir = tempfile::tempdir().unwrap();

    let mut calls = Vec::new();
    let path = download(&file, dir.path(), Some(&server.base), &mut |d, t| {
        calls.push((d, t))
    })
    .unwrap();

    assert_eq!(path, dir.path().join(NAME));
    assert_eq!(fs::read(&path).unwrap(), data);
    assert!(!dir.path().join(format!("{NAME}.part")).exists());
    assert_eq!(server.ranges(), vec![None]);
    assert_eq!(calls.last(), Some(&(data.len() as u64, data.len() as u64)));
    assert!(
        calls.windows(2).all(|w| w[0].0 <= w[1].0),
        "progress is monotonic"
    );
    verify(&path, &file).unwrap();
}

#[test]
fn resumes_from_truncated_part() {
    let data = content(1_200_000);
    let file = model_for(&data);
    let server = serve(data.clone(), false);
    let dir = tempfile::tempdir().unwrap();
    let part = dir.path().join(format!("{NAME}.part"));
    fs::write(&part, &data[..500_000]).unwrap();

    let mut first = None;
    let path = download(&file, dir.path(), Some(&server.base), &mut |d, _| {
        first.get_or_insert(d);
    })
    .unwrap();

    assert_eq!(fs::read(&path).unwrap(), data);
    assert_eq!(server.ranges(), vec![Some("bytes=500000-".to_string())]);
    assert_eq!(first, Some(500_000));
    verify(&path, &file).unwrap();
}

#[test]
fn restarts_when_server_ignores_range() {
    let data = content(900_000);
    let file = model_for(&data);
    let server = serve(data.clone(), true);
    let dir = tempfile::tempdir().unwrap();
    let part = dir.path().join(format!("{NAME}.part"));
    fs::write(&part, &data[..300_000]).unwrap();

    let path = download(&file, dir.path(), Some(&server.base), &mut |_, _| {}).unwrap();

    assert_eq!(fs::read(&path).unwrap(), data);
    assert_eq!(server.ranges(), vec![Some("bytes=300000-".to_string())]);
    verify(&path, &file).unwrap();
}

#[test]
fn oversized_part_is_discarded() {
    let data = content(100_000);
    let file = model_for(&data);
    let server = serve(data.clone(), false);
    let dir = tempfile::tempdir().unwrap();
    fs::write(dir.path().join(format!("{NAME}.part")), vec![7u8; 150_000]).unwrap();

    let path = download(&file, dir.path(), Some(&server.base), &mut |_, _| {}).unwrap();

    assert_eq!(fs::read(&path).unwrap(), data);
    assert_eq!(server.ranges(), vec![None]);
}

#[test]
fn complete_part_goes_to_verification_on_416() {
    let data = content(200_000);
    let file = model_for(&data);
    let server = serve(data.clone(), false);
    let dir = tempfile::tempdir().unwrap();
    let part = dir.path().join(format!("{NAME}.part"));
    fs::write(&part, &data).unwrap();

    let path = download(&file, dir.path(), Some(&server.base), &mut |_, _| {}).unwrap();

    assert_eq!(fs::read(&path).unwrap(), data);
    assert!(!part.exists());
    assert_eq!(server.ranges(), vec![Some("bytes=200000-".to_string())]);
}

#[test]
fn complete_but_corrupt_part_is_deleted_on_416() {
    let data = content(200_000);
    let file = model_for(&data);
    let mut bad = data.clone();
    bad[1234] ^= 0xff;
    let server = serve(data.clone(), false);
    let dir = tempfile::tempdir().unwrap();
    let part = dir.path().join(format!("{NAME}.part"));
    fs::write(&part, &bad).unwrap();

    let err = download(&file, dir.path(), Some(&server.base), &mut |_, _| {}).unwrap_err();

    assert!(format!("{err:#}").contains("checksum"), "{err:#}");
    assert!(!part.exists());
    assert!(!dir.path().join(NAME).exists());
}

#[test]
fn corrupt_content_errors_and_removes_part() {
    let data = content(300_000);
    let file = model_for(&data);
    let mut served = data.clone();
    served[150_000] ^= 0x01; // same size, wrong bytes
    let server = serve(served, false);
    let dir = tempfile::tempdir().unwrap();

    let err = download(&file, dir.path(), Some(&server.base), &mut |_, _| {}).unwrap_err();

    assert!(format!("{err:#}").contains("checksum"), "{err:#}");
    assert!(!dir.path().join(format!("{NAME}.part")).exists());
    assert!(!dir.path().join(NAME).exists());
}

#[test]
fn follows_redirects() {
    let data = content(50_000);
    let file = model_for(&data);
    let server = serve(data.clone(), false);
    let dir = tempfile::tempdir().unwrap();
    let base = format!("{}/redirect", server.base);

    let path = download(&file, dir.path(), Some(&base), &mut |_, _| {}).unwrap();

    assert_eq!(fs::read(&path).unwrap(), data);
}

#[test]
fn http_error_is_reported_and_keeps_nothing() {
    let data = content(10_000);
    let mut file = model_for(&data);
    file.name = "missing.gguf";
    let server = serve(data, false);
    let dir = tempfile::tempdir().unwrap();

    let err = download(&file, dir.path(), Some(&server.base), &mut |_, _| {}).unwrap_err();

    assert!(format!("{err:#}").contains("404"), "{err:#}");
    assert!(!dir.path().join("missing.gguf.part").exists());
}

#[test]
fn verify_checks_sha256() {
    let data = content(4_096);
    let file = model_for(&data);
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join(NAME);
    fs::write(&path, &data).unwrap();
    verify(&path, &file).unwrap();

    fs::write(&path, b"other").unwrap();
    assert!(verify(&path, &file).is_err());
}

#[test]
fn locate_honours_digup_models_and_ignores_wrong_size() {
    let _lock = lock_env();
    let data = content(1_000);
    let file = model_for(&data);
    let right = tempfile::tempdir().unwrap();
    let wrong = tempfile::tempdir().unwrap();
    let hub = tempfile::tempdir().unwrap();
    fs::write(right.path().join(NAME), &data).unwrap();
    fs::write(wrong.path().join(NAME), &data[..10]).unwrap();
    let snapshot = hub
        .path()
        .join(format!("models--{}", REPOSITORY.replace('/', "--")))
        .join("snapshots")
        .join(REVISION);
    fs::create_dir_all(&snapshot).unwrap();
    fs::write(snapshot.join(NAME), &data).unwrap();

    // $DIGUP_MODELS set: only that folder is searched.
    {
        let _env = EnvGuard::set(&[
            (MODELS_ENV, Some(right.path())),
            ("HF_HUB_CACHE", Some(hub.path())),
        ]);
        assert_eq!(locate(&file), Some(right.path().join(NAME)));
    }
    {
        let _env = EnvGuard::set(&[
            (MODELS_ENV, Some(wrong.path())),
            ("HF_HUB_CACHE", Some(hub.path())),
        ]);
        assert_eq!(
            locate(&file),
            None,
            "wrong size and no fallback when DIGUP_MODELS is set"
        );
    }

    // Unset: the HF cache snapshot is used when the size matches, and ignored when it doesn't.
    {
        let _env = EnvGuard::set(&[(MODELS_ENV, None), ("HF_HUB_CACHE", Some(hub.path()))]);
        assert_eq!(locate(&file), Some(snapshot.join(NAME)));
    }
    let other = ModelFile {
        name: NAME,
        bytes: 999,
        sha256: file.sha256,
    };
    {
        let _env = EnvGuard::set(&[(MODELS_ENV, None), ("HF_HUB_CACHE", Some(hub.path()))]);
        assert_eq!(locate(&other), None, "snapshot file has the wrong size");
    }
}

#[test]
fn cancel_before_start_is_reported_and_leaves_no_part() {
    let data = content(300_000);
    let file = model_for(&data);
    let server = serve(data.clone(), false);
    let dir = tempfile::tempdir().unwrap();
    let cancel = std::sync::atomic::AtomicBool::new(true);

    let err = digup_models::download_cancellable(
        &file,
        dir.path(),
        Some(&server.base),
        &cancel,
        &mut |_, _| {},
    )
    .unwrap_err();

    assert!(digup_models::is_cancelled(&err), "{err:#}");
    assert!(
        server.ranges().is_empty(),
        "no request may be made after an early cancel"
    );
    assert!(!dir.path().join(format!("{NAME}.part")).exists());
    assert!(!dir.path().join(NAME).exists());
}

#[test]
fn cancel_mid_download_keeps_part_and_resume_completes() {
    let data = content(3_000_000);
    let file = model_for(&data);
    let server = serve(data.clone(), false);
    let dir = tempfile::tempdir().unwrap();
    let cancel = std::sync::atomic::AtomicBool::new(false);

    // Raise the flag as soon as some bytes arrived but before the file is complete.
    let err = digup_models::download_cancellable(
        &file,
        dir.path(),
        Some(&server.base),
        &cancel,
        &mut |d, total| {
            if d > 0 && d < total {
                cancel.store(true, std::sync::atomic::Ordering::SeqCst);
            }
        },
    )
    .unwrap_err();

    assert!(digup_models::is_cancelled(&err), "{err:#}");
    let part = dir.path().join(format!("{NAME}.part"));
    let kept = fs::metadata(&part).expect("part kept").len();
    assert!(kept > 0 && kept < data.len() as u64, "kept {kept}");
    assert!(!dir.path().join(NAME).exists());

    // A later run without the flag resumes from the kept bytes and verifies.
    let path = download(&file, dir.path(), Some(&server.base), &mut |_, _| {}).unwrap();
    assert_eq!(fs::read(&path).unwrap(), data);
    assert_eq!(
        server.ranges().last().unwrap(),
        &Some(format!("bytes={kept}-"))
    );
    verify(&path, &file).unwrap();
}
