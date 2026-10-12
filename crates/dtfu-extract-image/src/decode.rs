//! Decode once, apply EXIF orientation, decide screenshot, thumbnail.
//! Thumbnails use fast_image_resize (Lanczos3); the embedder-input resize is elsewhere (D4).

use std::io::{Cursor, Read};
use std::path::Path;

use exif::{In, Reader as ExifReader, Tag, Value};
use fast_image_resize::{images::Image, FilterType, PixelType, ResizeAlg, ResizeOptions, Resizer};
use image::codecs::jpeg::JpegEncoder;
use image::codecs::png::PngEncoder;
use image::{DynamicImage, ExtendedColorType, ImageEncoder, ImageFormat, ImageReader, RgbImage};

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
    let bytes = read_capped(path)?;
    let format = detect_format(&bytes)?;
    let (orientation, taken) = read_exif(&bytes);
    let decoded = decode_limited(&bytes, format)?;
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

/// Largest source file we will read (bytes).
pub const MAX_IMAGE_FILE_BYTES: u64 = 256 * 1024 * 1024;
/// Largest accepted width or height (pixels), checked from the header before any pixel allocation.
pub const MAX_IMAGE_SIDE: u32 = 16384;
/// Largest decoder allocation (bytes).
pub const MAX_DECODE_ALLOC: u64 = 256 * 1024 * 1024;

/// Reads the whole file, refusing anything larger than [`MAX_IMAGE_FILE_BYTES`].
fn read_capped(path: &Path) -> Result<Vec<u8>, ExtractError> {
    let file = std::fs::File::open(path)?;
    if file.metadata()?.len() > MAX_IMAGE_FILE_BYTES {
        return Err(ExtractError::Unsupported(format!(
            "file larger than {} MiB",
            MAX_IMAGE_FILE_BYTES / (1024 * 1024)
        )));
    }
    let mut bytes = Vec::new();
    // Bounded read: the file may have grown since the metadata check.
    file.take(MAX_IMAGE_FILE_BYTES + 1).read_to_end(&mut bytes)?;
    if bytes.len() as u64 > MAX_IMAGE_FILE_BYTES {
        return Err(ExtractError::Unsupported(format!(
            "file larger than {} MiB",
            MAX_IMAGE_FILE_BYTES / (1024 * 1024)
        )));
    }
    Ok(bytes)
}

/// Decodes with the format already detected, under the dimension and allocation limits.
fn decode_limited(bytes: &[u8], format: ImageFormat) -> Result<DynamicImage, ExtractError> {
    // `Limits` is non-exhaustive, so fields are set on the default rather than via a struct literal.
    let mut limits = image::Limits::default();
    limits.max_image_width = Some(MAX_IMAGE_SIDE);
    limits.max_image_height = Some(MAX_IMAGE_SIDE);
    limits.max_alloc = Some(MAX_DECODE_ALLOC);
    let mut reader = ImageReader::with_format(Cursor::new(bytes), format);
    reader.limits(limits);
    reader.decode().map_err(|err| ExtractError::Decode(err.to_string()))
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

#[cfg(test)]
mod tests {
    use super::*;

    fn crc32(data: &[u8]) -> u32 {
        let mut crc = 0xFFFF_FFFFu32;
        for &b in data {
            crc ^= u32::from(b);
            for _ in 0..8 {
                crc = if crc & 1 != 0 { (crc >> 1) ^ 0xEDB8_8320 } else { crc >> 1 };
            }
        }
        !crc
    }

    fn encode_png(width: u32, height: u32) -> Vec<u8> {
        let pixels = vec![0u8; (width * height * 3) as usize];
        let mut out = Vec::new();
        PngEncoder::new(&mut out)
            .write_image(&pixels, width, height, ExtendedColorType::Rgb8)
            .unwrap();
        out
    }

    /// A 1x1 PNG whose IHDR is rewritten to claim `width` x `height` (valid IHDR CRC, no real pixel data).
    fn png_claiming(width: u32, height: u32) -> Vec<u8> {
        let mut png = encode_png(1, 1);
        // Layout: 8-byte signature, 4-byte length, "IHDR" at 12..16, 13 data bytes at 16..29, CRC at 29..33.
        png[16..20].copy_from_slice(&width.to_be_bytes());
        png[20..24].copy_from_slice(&height.to_be_bytes());
        let crc = crc32(&png[12..29]);
        png[29..33].copy_from_slice(&crc.to_be_bytes());
        png
    }

    #[test]
    fn oversized_png_header_is_rejected_before_allocation() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("huge.png");
        std::fs::write(&path, png_claiming(40_000, 40_000)).unwrap();
        let err = process_image(&path).unwrap_err();
        assert!(matches!(err, ExtractError::Decode(_)), "got {err:?}");
    }

    #[test]
    fn normal_png_still_produces_a_thumbnail() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("photo.png");
        std::fs::write(&path, encode_png(300, 200)).unwrap();
        let img = process_image(&path).unwrap();
        assert_eq!((img.width, img.height), (300, 200));
        assert_eq!(img.thumbnail.format, ThumbFormat::Jpeg);
        assert_eq!((img.thumbnail.width, img.thumbnail.height), (300, 200));
    }
}
