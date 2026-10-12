//! Decode once, apply EXIF orientation, decide screenshot, thumbnail.
//! Thumbnails use fast_image_resize (Lanczos3); the embedder-input resize is elsewhere (D4).

use std::io::Cursor;
use std::path::Path;

use exif::{In, Reader as ExifReader, Tag, Value};
use fast_image_resize::{images::Image, FilterType, PixelType, ResizeAlg, ResizeOptions, Resizer};
use image::codecs::jpeg::JpegEncoder;
use image::codecs::png::PngEncoder;
use image::{DynamicImage, ExtendedColorType, ImageEncoder, ImageFormat, RgbImage};

use crate::screenshot::is_screenshot;
use crate::{ExtractError, SkipReason};

pub const PHOTO_MAX_SIDE: u32 = 1024;
pub const SCREENSHOT_MAX_SIDE: u32 = 1536;
pub const MIN_IMAGE_SIDE: u32 = 256;
const JPEG_QUALITY: u8 = 90;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ThumbFormat {
    Jpeg,
    Png,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Thumbnail {
    pub width: u32,
    pub height: u32,
    pub format: ThumbFormat,
    pub bytes: Vec<u8>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProcessedImage {
    /// Dimensions after EXIF orientation is applied.
    pub width: u32,
    pub height: u32,
    /// EXIF orientation tag (1..=8), 1 when absent.
    pub orientation: u16,
    /// EXIF DateTimeOriginal as written by the camera.
    pub taken: Option<String>,
    pub is_screenshot: bool,
    pub thumbnail: Thumbnail,
}

/// Reads, decodes once, and produces the thumbnail. Screenshots get PNG at max side 1536,
/// other images JPEG q90 at max side 1024. Images whose longest side is below 256 are `Skipped`.
pub fn process_image(path: &Path) -> Result<ProcessedImage, ExtractError> {
    let bytes = std::fs::read(path)?;
    let format = detect_format(&bytes)?;
    let (orientation, taken) = read_exif(&bytes);
    let decoded = image::load_from_memory_with_format(&bytes, format)
        .map_err(|err| ExtractError::Decode(err.to_string()))?;
    drop(bytes);
    let oriented = apply_orientation(decoded, orientation);
    let (width, height) = (oriented.width(), oriented.height());
    if width.max(height) < MIN_IMAGE_SIDE {
        return Err(ExtractError::Skipped(SkipReason::TooSmall { width, height }));
    }
    let screenshot = is_screenshot(path);
    let (max_side, out_format) = if screenshot {
        (SCREENSHOT_MAX_SIDE, ThumbFormat::Png)
    } else {
        (PHOTO_MAX_SIDE, ThumbFormat::Jpeg)
    };
    let thumbnail = thumbnail(&oriented.to_rgb8(), max_side, out_format)?;
    Ok(ProcessedImage {
        width,
        height,
        orientation,
        taken,
        is_screenshot: screenshot,
        thumbnail,
    })
}

/// Scales so the longest side is `max_side` (aspect kept, never upscaled), then encodes.
pub fn thumbnail(rgb: &RgbImage, max_side: u32, format: ThumbFormat) -> Result<Thumbnail, ExtractError> {
    let (src_w, src_h) = rgb.dimensions();
    let (dst_w, dst_h) = fit_dimensions(src_w, src_h, max_side);
    let pixels = if (dst_w, dst_h) == (src_w, src_h) {
        rgb.as_raw().clone()
    } else {
        resize_rgb(rgb.as_raw(), src_w, src_h, dst_w, dst_h)?
    };
    let mut out = Vec::new();
    let encoded = match format {
        ThumbFormat::Jpeg => JpegEncoder::new_with_quality(&mut out, JPEG_QUALITY).write_image(
            &pixels,
            dst_w,
            dst_h,
            ExtendedColorType::Rgb8,
        ),
        ThumbFormat::Png => PngEncoder::new(&mut out).write_image(&pixels, dst_w, dst_h, ExtendedColorType::Rgb8),
    };
    encoded.map_err(|err| ExtractError::Decode(err.to_string()))?;
    Ok(Thumbnail {
        width: dst_w,
        height: dst_h,
        format,
        bytes: out,
    })
}

/// Output size for a longest side capped at `max_side`. Never upscales; keeps at least 1 pixel per side.
pub fn fit_dimensions(width: u32, height: u32, max_side: u32) -> (u32, u32) {
    let longest = width.max(height);
    if longest <= max_side || longest == 0 {
        return (width, height);
    }
    let scale = f64::from(max_side) / f64::from(longest);
    let w = (f64::from(width) * scale).round().max(1.0) as u32;
    let h = (f64::from(height) * scale).round().max(1.0) as u32;
    (w, h)
}

fn resize_rgb(pixels: &[u8], src_w: u32, src_h: u32, dst_w: u32, dst_h: u32) -> Result<Vec<u8>, ExtractError> {
    let decode_err = |msg: String| ExtractError::Decode(msg);
    let src = Image::from_vec_u8(src_w, src_h, pixels.to_vec(), PixelType::U8x3)
        .map_err(|err| decode_err(err.to_string()))?;
    let mut dst = Image::new(dst_w, dst_h, PixelType::U8x3);
    let options = ResizeOptions::new().resize_alg(ResizeAlg::Convolution(FilterType::Lanczos3));
    Resizer::new()
        .resize(&src, &mut dst, &options)
        .map_err(|err| decode_err(err.to_string()))?;
    Ok(dst.into_vec())
}

/// Rotates/flips per EXIF orientation 1..=8 (anything else is left alone).
pub fn apply_orientation(img: DynamicImage, orientation: u16) -> DynamicImage {
    match orientation {
        2 => img.fliph(),
        3 => img.rotate180(),
        4 => img.flipv(),
        5 => img.rotate90().fliph(),
        6 => img.rotate90(),
        7 => img.rotate270().fliph(),
        8 => img.rotate270(),
        _ => img,
    }
}

/// Maps the bytes to a supported format. HEIC/HEIF and other known-but-unsupported formats are
/// `Unsupported`; unknown bytes are a `Decode` error.
fn detect_format(bytes: &[u8]) -> Result<ImageFormat, ExtractError> {
    if bytes.len() >= 12 && &bytes[4..8] == b"ftyp" {
        let brand = &bytes[8..12];
        if matches!(brand, b"heic" | b"heix" | b"hevc" | b"hevx" | b"mif1" | b"msf1" | b"heim" | b"heis") {
            return Err(ExtractError::Unsupported("HEIC/HEIF".into()));
        }
    }
    let format = image::guess_format(bytes).map_err(|err| ExtractError::Decode(err.to_string()))?;
    match format {
        ImageFormat::Jpeg | ImageFormat::Png | ImageFormat::WebP | ImageFormat::Gif | ImageFormat::Bmp
        | ImageFormat::Tiff => Ok(format),
        other => Err(ExtractError::Unsupported(format!("{other:?}"))),
    }
}

/// (orientation, DateTimeOriginal). Missing or unreadable EXIF gives (1, None).
fn read_exif(bytes: &[u8]) -> (u16, Option<String>) {
    let mut cursor = Cursor::new(bytes);
    let Ok(exif) = ExifReader::new().read_from_container(&mut cursor) else {
        return (1, None);
    };
    let orientation = exif
        .get_field(Tag::Orientation, In::PRIMARY)
        .and_then(|field| field.value.get_uint(0))
        .and_then(|value| u16::try_from(value).ok())
        .filter(|value| (1..=8).contains(value))
        .unwrap_or(1);
    let taken = exif
        .get_field(Tag::DateTimeOriginal, In::PRIMARY)
        .and_then(|field| match &field.value {
            Value::Ascii(parts) => parts.first().and_then(|part| std::str::from_utf8(part).ok()).map(str::to_owned),
            _ => None,
        });
    (orientation, taken)
}
