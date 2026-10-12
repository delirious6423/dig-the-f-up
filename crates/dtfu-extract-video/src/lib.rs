//! Video keyframe extraction.
//!
//! STOPGAP: this shells out to the `ffmpeg` / `ffprobe` binaries (one ffmpeg process per file)
//! until libav bindings replace it. Audio is handled by `dtfu-extract-audio`, not here.
//!
//! Frame times follow `plan_frame_times` (first sample at min(0.5, d/2), then every
//! max(interval, d/1200) seconds); frames are fetched with `-ss start -vf fps=1/step`, autorotated,
//! scaled so the longest side is `<= max_side`, then de-duplicated with the 8x8 average hash.
//!
//! Every child runs under a deadline (files may live on network shares or USB drives that vanish):
//! ffprobe has a total deadline; ffmpeg has a no-progress deadline plus a total cap. On a deadline
//! the child's process group is SIGKILLed and reaped, and the error message starts with "timed out".

use dtfu_extract_image::{
    average_hash_8x8, hamming, plan_frame_times, FRAME_DUPLICATE_BITS,
};
use image::RgbImage;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output, Stdio};
use std::sync::mpsc::{self, RecvTimeoutError};
use std::thread;
use std::time::{Duration, Instant};

const MAX_FRAMES: usize = 1200;

#[cfg(not(test))]
const PROBE_TIMEOUT: Duration = Duration::from_secs(30);
#[cfg(not(test))]
const FRAME_NO_PROGRESS_TIMEOUT: Duration = Duration::from_secs(30);
#[cfg(not(test))]
const FRAME_TOTAL_TIMEOUT: Duration = Duration::from_secs(600);

#[cfg(test)]
const PROBE_TIMEOUT: Duration = Duration::from_secs(1);
#[cfg(test)]
const FRAME_NO_PROGRESS_TIMEOUT: Duration = Duration::from_secs(1);
#[cfg(test)]
const FRAME_TOTAL_TIMEOUT: Duration = Duration::from_secs(5);

/// How often the ffprobe wait loop checks the child.
const POLL_INTERVAL: Duration = Duration::from_millis(10);

#[derive(Debug, Clone, PartialEq)]
pub struct Frame {
    pub time_secs: f64,
    pub width: u32,
    pub height: u32,
    pub rgb: Vec<u8>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct VideoInfo {
    pub duration_secs: f64,
    pub has_audio: bool,
    /// Display size (orientation applied); 0x0 when there is no video stream.
    pub width: u32,
    pub height: u32,
}

#[derive(Debug, Clone, PartialEq)]
pub enum VideoError {
    /// ffmpeg / ffprobe not found.
    Missing,
    Unreadable(String),
}

impl std::fmt::Display for VideoError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            VideoError::Missing => write!(f, "ffmpeg not found"),
            VideoError::Unreadable(s) => write!(f, "unreadable video: {s}"),
        }
    }
}
impl std::error::Error for VideoError {}

/// Child-process plumbing: own process group on unix, so a kill takes the whole tree.
mod sys {
    use std::process::Child;

    #[cfg(unix)]
    mod imp {
        use std::process::{Child, Command};

        extern "C" {
            fn kill(pid: i32, sig: i32) -> i32;
        }
        const SIGKILL: i32 = 9;

        pub fn configure(cmd: &mut Command) {
            use std::os::unix::process::CommandExt;
            cmd.process_group(0);
        }

        /// SIGKILL the child's process group (the child is its leader), then the child.
        /// Only call on a child that has not been reaped yet, so the pid cannot be reused.
        pub fn kill_tree(child: &mut Child) {
            let pid = child.id() as i32;
            // SAFETY: plain syscall; a failure (already gone) is ignored.
            unsafe {
                kill(-pid, SIGKILL);
            }
            let _ = child.kill();
        }
    }

    #[cfg(not(unix))]
    mod imp {
        use std::process::{Child, Command};

        pub fn configure(_cmd: &mut Command) {}

        pub fn kill_tree(child: &mut Child) {
            let _ = child.kill();
        }
    }

    pub use imp::{configure, kill_tree};

    /// Kill an unreaped child's tree and reap it.
    pub fn kill_and_reap(child: &mut Child) {
        kill_tree(child);
        let _ = child.wait();
    }
}

/// File names to try for `name` inside each directory: the bare name, plus `<name>.exe` on Windows.
fn tool_candidates(name: &str, windows: bool) -> Vec<String> {
    if windows && !name.to_ascii_lowercase().ends_with(".exe") {
        vec![name.to_string(), format!("{name}.exe")]
    } else {
        vec![name.to_string()]
    }
}

/// Locate a tool: first the app's own LGPL build next to the running executable, named `dtfu-<name>` (see
/// scripts/build-ffmpeg.sh; prefixed so a Linux package never clashes with the distro's ffmpeg), then `<name>` on
/// PATH, then the Homebrew prefixes.
pub fn find_tool(name: &str) -> Option<PathBuf> {
    if let Some(dir) = std::env::current_exe().ok().and_then(|e| e.parent().map(Path::to_path_buf)) {
        let bundled = tool_candidates(&format!("dtfu-{name}"), cfg!(windows)).into_iter().map(|n| dir.join(n)).find(|p| p.is_file());
        if bundled.is_some() {
            return bundled;
        }
    }
    let mut dirs: Vec<PathBuf> = std::env::var_os("PATH").map(|p| std::env::split_paths(&p).collect()).unwrap_or_default();
    dirs.push("/opt/homebrew/bin".into());
    dirs.push("/usr/local/bin".into());
    let names = tool_candidates(name, cfg!(windows));
    dirs.into_iter()
        .flat_map(|d| names.iter().map(move |n| d.join(n)))
        .find(|p| p.is_file())
}

#[cfg(test)]
mod find_tool_tests {
    use super::tool_candidates;

    #[test]
    fn unix_candidates_are_bare_name_only() {
        assert_eq!(tool_candidates("ffprobe", false), vec!["ffprobe".to_string()]);
    }

    #[test]
    fn windows_candidates_add_exe() {
        assert_eq!(
            tool_candidates("ffprobe", true),
            vec!["ffprobe".to_string(), "ffprobe.exe".to_string()]
        );
    }

    #[test]
    fn windows_does_not_double_exe() {
        assert_eq!(tool_candidates("ffmpeg.exe", true), vec!["ffmpeg.exe".to_string()]);
        assert_eq!(tool_candidates("FFMPEG.EXE", true), vec!["FFMPEG.EXE".to_string()]);
    }
}

/// Read a pipe to EOF on a helper thread so the child can never block on a full pipe.
fn drain<R: Read + Send + 'static>(pipe: Option<R>) -> thread::JoinHandle<Vec<u8>> {
    thread::spawn(move || {
        let mut buf = Vec::new();
        if let Some(mut r) = pipe {
            let _ = r.read_to_end(&mut buf);
        }
        buf
    })
}

/// Run ffprobe to completion, or kill it (process group) once `PROBE_TIMEOUT` has passed.
fn run_probe(bin: &Path, path: &Path) -> Result<Output, VideoError> {
    let mut cmd = Command::new(bin);
    cmd.args(["-v", "error", "-print_format", "json", "-show_format", "-show_streams"])
        .arg(path)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    sys::configure(&mut cmd);
    let mut child = cmd.spawn().map_err(|e| VideoError::Unreadable(e.to_string()))?;
    let out_reader = drain(child.stdout.take());
    let err_reader = drain(child.stderr.take());
    let deadline = Instant::now() + PROBE_TIMEOUT;
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) if Instant::now() < deadline => thread::sleep(POLL_INTERVAL),
            Ok(None) => {
                sys::kill_and_reap(&mut child);
                return Err(VideoError::Unreadable(format!(
                    "timed out after {}s waiting for ffprobe",
                    PROBE_TIMEOUT.as_secs()
                )));
            }
            Err(e) => {
                sys::kill_and_reap(&mut child);
                return Err(VideoError::Unreadable(e.to_string()));
            }
        }
    };
    Ok(Output {
        status,
        stdout: out_reader.join().unwrap_or_default(),
        stderr: err_reader.join().unwrap_or_default(),
    })
}

pub fn probe(path: &Path) -> Result<VideoInfo, VideoError> {
    let ffprobe = find_tool("ffprobe").ok_or(VideoError::Missing)?;
    let out = run_probe(&ffprobe, path)?;
    if !out.status.success() {
        return Err(VideoError::Unreadable(String::from_utf8_lossy(&out.stderr).trim().to_string()));
    }
    let v: serde_json::Value =
        serde_json::from_slice(&out.stdout).map_err(|e| VideoError::Unreadable(e.to_string()))?;
    let streams = v["streams"].as_array().cloned().unwrap_or_default();
    let has_audio = streams.iter().any(|s| s["codec_type"] == "audio");
    let video = streams.iter().find(|s| {
        s["codec_type"] == "video" && s["disposition"]["attached_pic"].as_i64().unwrap_or(0) == 0
    });
    let num = |x: &serde_json::Value| -> Option<f64> {
        x.as_f64().or_else(|| x.as_str().and_then(|s| s.parse().ok()))
    };
    let mut duration = num(&v["format"]["duration"])
        .or_else(|| video.and_then(|s| num(&s["duration"])))
        .unwrap_or(0.0);
    if !duration.is_finite() || duration < 0.0 {
        duration = 0.0;
    }
    let (mut width, mut height) = (0u32, 0u32);
    if let Some(s) = video {
        width = s["width"].as_u64().unwrap_or(0) as u32;
        height = s["height"].as_u64().unwrap_or(0) as u32;
        let mut rot = s["tags"]["rotate"].as_str().and_then(|r| r.parse::<f64>().ok());
        if let Some(list) = s["side_data_list"].as_array() {
            for sd in list {
                if let Some(r) = sd["rotation"].as_f64() {
                    rot = Some(r);
                }
            }
        }
        if let Some(r) = rot {
            if (r.abs().round() as i64 / 90) % 2 == 1 {
                std::mem::swap(&mut width, &mut height);
            }
        }
    }
    Ok(VideoInfo { duration_secs: duration, has_audio, width, height })
}

/// Sample times for an arbitrary interval (same formula as `plan_frame_times`).
fn plan(duration: f64, interval: f64) -> Vec<f64> {
    if (interval - 3.0).abs() < 1e-9 {
        return plan_frame_times(duration);
    }
    if !duration.is_finite() || duration <= 0.0 || !interval.is_finite() || interval <= 0.0 {
        return Vec::new();
    }
    let step = interval.max(duration / MAX_FRAMES as f64);
    let start = 0.5f64.min(duration / 2.0);
    let mut out = Vec::new();
    let mut t = start;
    while t < duration && out.len() < MAX_FRAMES {
        out.push(t);
        t = start + step * out.len() as f64;
    }
    out
}

/// Parse one binary PPM (P6, maxval 255) from the stream; `None` at clean EOF.
fn read_ppm(r: &mut impl Read) -> Option<(u32, u32, Vec<u8>)> {
    let mut tokens: Vec<String> = Vec::new();
    let mut cur = String::new();
    let mut b = [0u8; 1];
    while tokens.len() < 4 {
        if r.read_exact(&mut b).is_err() {
            return None;
        }
        if b[0].is_ascii_whitespace() {
            if !cur.is_empty() {
                tokens.push(std::mem::take(&mut cur));
            }
        } else {
            cur.push(b[0] as char);
        }
    }
    if tokens[0] != "P6" {
        return None;
    }
    let w: u32 = tokens[1].parse().ok()?;
    let h: u32 = tokens[2].parse().ok()?;
    let mut data = vec![0u8; w as usize * h as usize * 3];
    r.read_exact(&mut data).ok()?;
    Some((w, h, data))
}

/// Reads PPM frames (one per entry of `times`) and applies the dedup rule as each one arrives:
/// a frame is kept unless it is within `FRAME_DUPLICATE_BITS` of the last KEPT frame's hash.
/// Only kept frames keep their pixel buffer (moved out of the image, never cloned); dropped
/// frames are freed immediately. Returns the kept frames and the peak number of pixel buffers
/// alive at once (kept frames plus the one being hashed).
fn collect_kept(r: &mut impl Read, times: &[f64]) -> (Vec<Frame>, usize) {
    let mut kept: Vec<Frame> = Vec::new();
    let mut last: Option<u64> = None;
    let mut peak = 0usize;
    for &time_secs in times {
        let Some((width, height, rgb)) = read_ppm(r) else { break };
        peak = peak.max(kept.len() + 1);
        // read_ppm sizes the buffer exactly, so this cannot fail; breaking mirrors clean EOF.
        let Some(img) = RgbImage::from_raw(width, height, rgb) else { break };
        let hash = average_hash_8x8(&img);
        let duplicate = last.is_some_and(|prev| hamming(hash, prev) <= FRAME_DUPLICATE_BITS);
        if !duplicate {
            last = Some(hash);
            kept.push(Frame { time_secs, width, height, rgb: img.into_raw() });
        }
    }
    (kept, peak)
}

/// Messages from the ffmpeg output reader thread to the supervising loop.
enum Event {
    /// Some bytes arrived on stdout (resets the no-progress deadline).
    Progress,
    /// Reader finished: kept frames and peak buffer count.
    Done(Vec<Frame>, usize),
}

/// Forwards every successful read as a `Progress` event.
struct ProgressReader<R> {
    inner: R,
    tx: mpsc::Sender<Event>,
}

impl<R: Read> Read for ProgressReader<R> {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        let n = self.inner.read(buf)?;
        if n > 0 {
            let _ = self.tx.send(Event::Progress);
        }
        Ok(n)
    }
}

pub fn frames(path: &Path, interval_secs: f64, max_side: u32) -> Result<Vec<Frame>, VideoError> {
    let ffmpeg = find_tool("ffmpeg").ok_or(VideoError::Missing)?;
    let info = probe(path)?;
    if info.width == 0 || info.height == 0 {
        return Ok(Vec::new());
    }
    let times = plan(info.duration_secs, interval_secs);
    if times.is_empty() {
        return Ok(Vec::new());
    }
    let step = if times.len() > 1 { times[1] - times[0] } else { interval_secs.max(1.0) };
    let m = max_side.max(1);
    let scale = format!(
        "scale=w='max(1,trunc(iw*min(1,{m}/max(iw,ih))))':h='max(1,trunc(ih*min(1,{m}/max(iw,ih))))':flags=area"
    );
    let vf = format!("fps=1/{step:.9}:round=near:eof_action=pass,{scale}");
    let mut cmd = Command::new(&ffmpeg);
    cmd.args(["-v", "error", "-nostdin", "-ss", &format!("{:.6}", times[0]), "-i"])
        .arg(path)
        .args(["-an", "-sn", "-vf", &vf, "-frames:v", &times.len().to_string()])
        .args(["-c:v", "ppm", "-f", "image2pipe", "-"])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    sys::configure(&mut cmd);
    let mut child: Child = cmd.spawn().map_err(|e| VideoError::Unreadable(e.to_string()))?;
    let Some(stdout) = child.stdout.take() else {
        sys::kill_and_reap(&mut child);
        return Err(VideoError::Unreadable("no stdout".into()));
    };
    let (tx, rx) = mpsc::channel::<Event>();
    let reader_times = times.clone();
    thread::spawn(move || {
        let mut progress = ProgressReader { inner: stdout, tx: tx.clone() };
        let (kept, peak) = collect_kept(&mut progress, &reader_times);
        let _ = tx.send(Event::Done(kept, peak));
    });

    let start = Instant::now();
    let mut kept = Vec::new();
    let outcome: Result<(), String> = loop {
        let total_left = FRAME_TOTAL_TIMEOUT.saturating_sub(start.elapsed());
        if total_left.is_zero() {
            break Err(format!("timed out after {}s extracting frames", FRAME_TOTAL_TIMEOUT.as_secs()));
        }
        match rx.recv_timeout(FRAME_NO_PROGRESS_TIMEOUT.min(total_left)) {
            Ok(Event::Progress) => {}
            Ok(Event::Done(k, _peak)) => {
                kept = k;
                break Ok(());
            }
            Err(RecvTimeoutError::Timeout) => {
                break Err(if start.elapsed() >= FRAME_TOTAL_TIMEOUT {
                    format!("timed out after {}s extracting frames", FRAME_TOTAL_TIMEOUT.as_secs())
                } else {
                    format!(
                        "timed out: no frame data from ffmpeg for {}s",
                        FRAME_NO_PROGRESS_TIMEOUT.as_secs()
                    )
                });
            }
            // Reader thread ended without a result (panic): treat as no frames.
            Err(RecvTimeoutError::Disconnected) => break Ok(()),
        }
    };
    // Child is still unreaped here, so its process group cannot have been reused.
    sys::kill_and_reap(&mut child);
    outcome.map_err(VideoError::Unreadable)?;
    if kept.is_empty() {
        return Err(VideoError::Unreadable("ffmpeg produced no frames".into()));
    }
    Ok(kept)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Binary PPM (P6) bytes for a `w`x`h` frame of the given pixel generator.
    fn ppm(w: u32, h: u32, px: impl Fn(u32, u32) -> u8) -> Vec<u8> {
        let mut out = format!("P6\n{w} {h}\n255\n").into_bytes();
        for y in 0..h {
            for x in 0..w {
                let v = px(x, y);
                out.extend_from_slice(&[v, v, v]);
            }
        }
        out
    }

    #[test]
    fn streaming_dedup_keeps_only_changes_and_few_buffers() {
        // 50 flat frames, 50 half-black/half-white frames, 50 flat frames again.
        let flat = ppm(16, 16, |_, _| 0);
        let split = ppm(16, 16, |x, _| if x < 8 { 0 } else { 255 });
        let mut stream = Vec::new();
        for _ in 0..50 {
            stream.extend_from_slice(&flat);
        }
        for _ in 0..50 {
            stream.extend_from_slice(&split);
        }
        for _ in 0..50 {
            stream.extend_from_slice(&flat);
        }
        let times: Vec<f64> = (0..150).map(|i| i as f64).collect();
        let (kept, peak) = collect_kept(&mut stream.as_slice(), &times);
        let kept_times: Vec<f64> = kept.iter().map(|f| f.time_secs).collect();
        assert_eq!(kept_times, vec![0.0, 50.0, 100.0]);
        assert!(kept.iter().all(|f| (f.width, f.height) == (16, 16) && f.rgb.len() == 16 * 16 * 3));
        // Never more than the kept buffers plus the frame being hashed (no per-frame retention).
        assert!(peak <= kept.len() + 1, "peak live buffers {peak}");
    }

    #[test]
    fn long_static_stream_retains_one_buffer() {
        let flat = ppm(32, 24, |_, _| 90);
        let n = 1200;
        let stream: Vec<u8> = flat.iter().copied().cycle().take(flat.len() * n).collect();
        let times: Vec<f64> = (0..n).map(|i| i as f64 * 3.0).collect();
        let (kept, peak) = collect_kept(&mut stream.as_slice(), &times);
        assert_eq!(kept.len(), 1);
        assert_eq!(kept[0].time_secs, 0.0);
        assert!(peak <= 2, "peak live buffers {peak}");
    }
}

/// End-to-end deadline tests through `probe` / `frames`, which locate the tools via PATH.
/// Fake tools are shell scripts placed first on PATH. All PATH changes live in one test so
/// they do not race each other.
#[cfg(all(test, unix))]
mod deadline_tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    extern "C" {
        fn kill(pid: i32, sig: i32) -> i32;
    }

    fn alive(pid: i32) -> bool {
        // SAFETY: signal 0 only probes for existence.
        unsafe { kill(pid, 0) == 0 }
    }

    /// Poll until `pid` is gone (a killed grandchild may linger as a zombie briefly).
    fn gone_soon(pid: i32) -> bool {
        let until = Instant::now() + Duration::from_secs(3);
        while alive(pid) {
            if Instant::now() >= until {
                return false;
            }
            thread::sleep(Duration::from_millis(20));
        }
        true
    }

    /// Writes `dir/name` as an executable `#!/bin/sh` script.
    fn script(dir: &Path, name: &str, body: &str) {
        let p = dir.join(name);
        std::fs::write(&p, format!("#!/bin/sh\n{body}\n")).unwrap();
        std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o755)).unwrap();
    }

    /// Pids written by a hanging fake tool: the shell itself, then a `sleep` grandchild.
    fn hang_body(pidfile: &Path) -> String {
        format!("echo $$ > '{0}'; sleep 100 & echo $! >> '{0}'; wait", pidfile.display())
    }

    fn read_pids(pidfile: &Path) -> Vec<i32> {
        std::fs::read_to_string(pidfile)
            .unwrap()
            .split_whitespace()
            .map(|s| s.parse().unwrap())
            .collect()
    }

    fn with_path_prefix<T>(dir: &Path, f: impl FnOnce() -> T) -> T {
        let old = std::env::var_os("PATH").unwrap_or_default();
        let mut new = dir.as_os_str().to_owned();
        new.push(":");
        new.push(&old);
        std::env::set_var("PATH", &new);
        let out = f();
        std::env::set_var("PATH", &old);
        out
    }

    fn assert_timed_out(err: VideoError) {
        match err {
            VideoError::Unreadable(m) => assert!(m.starts_with("timed out"), "message was: {m}"),
            other => panic!("expected timeout, got {other:?}"),
        }
    }

    #[test]
    fn fake_tools_through_path() {
        let started = Instant::now();

        // 1. ffprobe hangs: probe() times out and kills the whole tree.
        let hang_probe = tempfile::tempdir().unwrap();
        let pf1 = hang_probe.path().join("pids1");
        script(hang_probe.path(), "ffprobe", &hang_body(&pf1));
        let err = with_path_prefix(hang_probe.path(), || probe(Path::new("/nonexistent/x.mp4")));
        assert_timed_out(err.unwrap_err());
        for pid in read_pids(&pf1) {
            assert!(gone_soon(pid), "pid {pid} still alive after ffprobe timeout");
        }

        // 2. ffprobe answers, ffmpeg hangs without output: frames() times out and kills the tree.
        let hang_ffmpeg = tempfile::tempdir().unwrap();
        let pf2 = hang_ffmpeg.path().join("pids2");
        script(
            hang_ffmpeg.path(),
            "ffprobe",
            "printf '%s' '{\"streams\":[{\"codec_type\":\"video\",\"width\":16,\"height\":16}],\"format\":{\"duration\":\"10\"}}'",
        );
        script(hang_ffmpeg.path(), "ffmpeg", &hang_body(&pf2));
        let err = with_path_prefix(hang_ffmpeg.path(), || frames(Path::new("/nonexistent/x.mp4"), 3.0, 480));
        assert_timed_out(err.unwrap_err());
        for pid in read_pids(&pf2) {
            assert!(gone_soon(pid), "pid {pid} still alive after ffmpeg timeout");
        }

        // 3. Normal case still works: ffmpeg emits one flat PPM per planned time and exits.
        let ok_dir = tempfile::tempdir().unwrap();
        script(
            ok_dir.path(),
            "ffprobe",
            "printf '%s' '{\"streams\":[{\"codec_type\":\"video\",\"width\":16,\"height\":16}],\"format\":{\"duration\":\"10\"}}'",
        );
        let n = plan_frame_times(10.0).len();
        script(
            ok_dir.path(),
            "ffmpeg",
            &format!(
                "i=0; while [ $i -lt {n} ]; do printf 'P6\\n16 16\\n255\\n'; head -c 768 /dev/zero; i=$((i+1)); done"
            ),
        );
        let got = with_path_prefix(ok_dir.path(), || frames(Path::new("/nonexistent/x.mp4"), 3.0, 480)).unwrap();
        assert_eq!(got.len(), 1, "flat frames dedup to one");
        assert_eq!((got[0].width, got[0].height), (16, 16));

        assert!(started.elapsed() < Duration::from_secs(60), "deadlines did not fire promptly");
    }
}
