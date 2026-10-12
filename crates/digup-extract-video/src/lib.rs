//! Video keyframe extraction.
//!
//! STOPGAP: this shells out to the `ffmpeg` / `ffprobe` binaries (one ffmpeg process per file)
//! until libav bindings replace it. Audio is handled by `digup-extract-audio`, not here.
//!
//! Frame times follow `plan_frame_times` (first sample at min(0.5, d/2), then every
//! max(interval, d/1200) seconds); frames are fetched with `-ss start -vf fps=1/step`, autorotated,
//! scaled so the longest side is `<= max_side`, then de-duplicated with the 8x8 average hash.

use digup_extract_image::{
    average_hash_8x8, hamming, plan_frame_times, FRAME_DUPLICATE_BITS,
};
use image::RgbImage;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

const MAX_FRAMES: usize = 1200;

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

/// Locate a tool via PATH, then the Homebrew prefixes.
pub fn find_tool(name: &str) -> Option<PathBuf> {
    let mut dirs: Vec<PathBuf> = std::env::var_os("PATH")
        .map(|p| std::env::split_paths(&p).collect())
        .unwrap_or_default();
    dirs.push("/opt/homebrew/bin".into());
    dirs.push("/usr/local/bin".into());
    dirs.into_iter().map(|d| d.join(name)).find(|p| p.is_file())
}

pub fn probe(path: &Path) -> Result<VideoInfo, VideoError> {
    let ffprobe = find_tool("ffprobe").ok_or(VideoError::Missing)?;
    let out = Command::new(ffprobe)
        .args(["-v", "error", "-print_format", "json", "-show_format", "-show_streams"])
        .arg(path)
        .stdin(Stdio::null())
        .output()
        .map_err(|e| VideoError::Unreadable(e.to_string()))?;
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
    let mut child = Command::new(ffmpeg)
        .args(["-v", "error", "-nostdin", "-ss", &format!("{:.6}", times[0]), "-i"])
        .arg(path)
        .args(["-an", "-sn", "-vf", &vf, "-frames:v", &times.len().to_string()])
        .args(["-c:v", "ppm", "-f", "image2pipe", "-"])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|e| VideoError::Unreadable(e.to_string()))?;
    let mut stdout = child.stdout.take().ok_or_else(|| VideoError::Unreadable("no stdout".into()))?;
    let (kept, _peak) = collect_kept(&mut stdout, &times);
    drop(stdout);
    let _ = child.kill();
    let _ = child.wait();
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
