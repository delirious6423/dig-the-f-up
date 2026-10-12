//! Image extraction: decode (EXIF-oriented), thumbnail, screenshot detection,
//! and the frame-hash / frame-planning helpers used by video keyframe dedup.
//! Ported from reference/digup-swift (ImageExtractor, Rules.isScreenshot, keyframes).

use std::fmt;

mod decode;
mod hash;
mod screenshot;

pub use decode::{
    apply_orientation, process_image, thumbnail, ProcessedImage, Thumbnail, ThumbFormat,
    MIN_IMAGE_SIDE, PHOTO_MAX_SIDE, SCREENSHOT_MAX_SIDE,
};
pub use hash::{average_hash_8x8, dedup_frames, hamming, plan_frame_times, FRAME_DUPLICATE_BITS};
pub use screenshot::{is_screenshot, SCREENSHOT_PREFIXES, SCREENSHOT_XATTR};

/// Why an image produced no thumbnail.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SkipReason {
    /// The longest side is below `MIN_IMAGE_SIDE`. Swift detail: "small image WxH".
    TooSmall { width: u32, height: u32 },
}

impl fmt::Display for SkipReason {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            SkipReason::TooSmall { width, height } => write!(f, "small image {width}x{height}"),
        }
    }
}

#[derive(Debug)]
pub enum ExtractError {
    /// Intentionally not indexed (state `skipped`).
    Skipped(SkipReason),
    /// A format we recognise but do not decode (HEIC/HEIF, or a file over the size cap).
    Unsupported(String),
    Io(std::io::Error),
    /// Bytes could not be decoded as an image.
    Decode(String),
}

impl fmt::Display for ExtractError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ExtractError::Skipped(reason) => write!(f, "skipped: {reason}"),
            ExtractError::Unsupported(what) => write!(f, "unsupported format: {what}"),
            ExtractError::Io(err) => write!(f, "io error: {err}"),
            ExtractError::Decode(msg) => write!(f, "decode error: {msg}"),
        }
    }
}

impl std::error::Error for ExtractError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            ExtractError::Io(err) => Some(err),
            _ => None,
        }
    }
}

impl From<std::io::Error> for ExtractError {
    fn from(err: std::io::Error) -> Self {
        ExtractError::Io(err)
    }
}
