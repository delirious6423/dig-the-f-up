//! Frame hashing and keyframe planning for video (no decoding here).
//! Ports MediaExtractors.averageHash, the dedup rule and the step/start rule from spec 3.8.

use image::imageops::FilterType;
use image::RgbImage;

/// A frame is dropped when it is within this many differing bits of the last KEPT frame.
pub const FRAME_DUPLICATE_BITS: u32 = 4;
/// Keyframe interval in seconds (Swift `interval` 3.0).
const FRAME_INTERVAL_SECS: f64 = 3.0;
/// Maximum frames per file (Swift `maxFrames` 1200).
const MAX_FRAMES: usize = 1200;

/// 8x8 grayscale average hash: bit i is set when pixel i is above the integer mean (row-major).
pub fn average_hash_8x8(rgb: &RgbImage) -> u64 {
    if rgb.width() == 0 || rgb.height() == 0 {
        return 0;
    }
    let gray = image::imageops::grayscale(rgb);
    let small = image::imageops::resize(&gray, 8, 8, FilterType::Triangle);
    let pixels = small.as_raw();
    let mean = pixels.iter().map(|&p| u32::from(p)).sum::<u32>() / pixels.len() as u32;
    pixels
        .iter()
        .enumerate()
        .filter(|(_, &p)| u32::from(p) > mean)
        .fold(0u64, |hash, (index, _)| hash | (1u64 << index))
}

/// Number of differing bits.
pub fn hamming(a: u64, b: u64) -> u32 {
    (a ^ b).count_ones()
}

/// Indices of the frames to keep, in order. The first frame is always kept; each later frame is
/// kept only if it differs from the last KEPT frame by more than `FRAME_DUPLICATE_BITS`.
pub fn dedup_frames(hashes: &[u64]) -> Vec<usize> {
    let mut kept = Vec::new();
    let mut last: Option<u64> = None;
    for (index, &hash) in hashes.iter().enumerate() {
        if let Some(previous) = last {
            if hamming(hash, previous) <= FRAME_DUPLICATE_BITS {
                continue;
            }
        }
        last = Some(hash);
        kept.push(index);
    }
    kept
}

/// Sample times in seconds: starts at min(0.5, d/2), then every `max(3, d/1200)` seconds, all below
/// the duration. Non-finite or non-positive durations give no frames. Capped at 1200 entries.
pub fn plan_frame_times(duration_secs: f64) -> Vec<f64> {
    if !duration_secs.is_finite() || duration_secs <= 0.0 {
        return Vec::new();
    }
    let step = FRAME_INTERVAL_SECS.max(duration_secs / MAX_FRAMES as f64);
    let start = 0.5f64.min(duration_secs / 2.0);
    let mut times = Vec::new();
    let mut t = start;
    while t < duration_secs && times.len() < MAX_FRAMES {
        times.push(t);
        t = start + step * times.len() as f64;
    }
    times
}
