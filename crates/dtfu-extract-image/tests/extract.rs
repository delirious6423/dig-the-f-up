use std::io::Cursor;
use std::path::{Path, PathBuf};

use dtfu_extract_image::{
    average_hash_8x8, dedup_frames, hamming, is_screenshot, plan_frame_times, process_image, ExtractError,
    SkipReason, ThumbFormat,
};
use image::{ImageFormat, Rgb, RgbImage};

fn gradient(width: u32, height: u32) -> RgbImage {
    RgbImage::from_fn(width, height, |x, y| {
        Rgb([(x * 255 / width.max(1)) as u8, (y * 255 / height.max(1)) as u8, ((x + y) % 256) as u8])
    })
}

fn encode(img: &RgbImage, format: ImageFormat) -> Vec<u8> {
    let mut buf = Cursor::new(Vec::new());
    img.write_to(&mut buf, format).expect("encode fixture");
    buf.into_inner()
}

/// Inserts an APP1/Exif segment with only Orientation (tag 0x0112) right after SOI.
fn jpeg_with_orientation(jpeg: &[u8], orientation: u16) -> Vec<u8> {
    let mut tiff = Vec::new();
    tiff.extend_from_slice(b"II*\0");
    tiff.extend_from_slice(&8u32.to_le_bytes());
    tiff.extend_from_slice(&1u16.to_le_bytes()); // one IFD entry
    tiff.extend_from_slice(&0x0112u16.to_le_bytes()); // Orientation
    tiff.extend_from_slice(&3u16.to_le_bytes()); // SHORT
    tiff.extend_from_slice(&1u32.to_le_bytes()); // count
    tiff.extend_from_slice(&orientation.to_le_bytes());
    tiff.extend_from_slice(&[0, 0]); // value padding
    tiff.extend_from_slice(&0u32.to_le_bytes()); // next IFD
    let mut payload = b"Exif\0\0".to_vec();
    payload.extend_from_slice(&tiff);
    let len = (payload.len() + 2) as u16;
    let mut out = jpeg[..2].to_vec(); // SOI
    out.extend_from_slice(&[0xFF, 0xE1]);
    out.extend_from_slice(&len.to_be_bytes());
    out.extend_from_slice(&payload);
    out.extend_from_slice(&jpeg[2..]);
    out
}

fn write_temp(dir: &Path, name: &str, bytes: &[u8]) -> PathBuf {
    let path = dir.join(name);
    std::fs::write(&path, bytes).expect("write fixture");
    path
}

#[test]
fn photo_is_thumbnailed_to_1024_max_side_as_jpeg() {
    let dir = tempfile::tempdir().unwrap();
    let bytes = encode(&gradient(3000, 2000), ImageFormat::Png);
    let path = write_temp(dir.path(), "IMG_0001.png", &bytes);
    let out = process_image(&path).expect("photo processes");
    assert_eq!((out.width, out.height), (3000, 2000));
    assert!(!out.is_screenshot);
    assert_eq!(out.thumbnail.format, ThumbFormat::Jpeg);
    assert_eq!((out.thumbnail.width, out.thumbnail.height), (1024, 683));
    let thumb = image::load_from_memory(&out.thumbnail.bytes).unwrap();
    assert_eq!((thumb.width(), thumb.height()), (1024, 683));
    assert_eq!(out.orientation, 1);
    assert_eq!(out.taken, None);
}

#[test]
fn icon_below_256_is_skipped_with_typed_reason() {
    let dir = tempfile::tempdir().unwrap();
    let path = write_temp(dir.path(), "icon.png", &encode(&gradient(128, 128), ImageFormat::Png));
    match process_image(&path) {
        Err(ExtractError::Skipped(SkipReason::TooSmall { width: 128, height: 128 })) => {}
        other => panic!("expected TooSmall skip, got {other:?}"),
    }
}

#[test]
fn screenshot_by_name_with_narrow_no_break_space_uses_1536_png() {
    let dir = tempfile::tempdir().unwrap();
    let name = "Screenshot 2026-10-05 at 9.41.07\u{202F}PM.png";
    let path = write_temp(dir.path(), name, &encode(&gradient(2400, 1200), ImageFormat::Png));
    assert!(is_screenshot(&path));
    let out = process_image(&path).expect("screenshot processes");
    assert!(out.is_screenshot);
    assert_eq!(out.thumbnail.format, ThumbFormat::Png);
    assert_eq!((out.thumbnail.width, out.thumbnail.height), (1536, 768));
    let thumb = image::load_from_memory(&out.thumbnail.bytes).unwrap();
    assert_eq!((thumb.width(), thumb.height()), (1536, 768));
}

#[test]
fn screenshot_name_prefixes_match_swift_rules() {
    for name in ["Screen Shot 2019-01-01 at 1.00.00 PM.png", "CleanShot 2026-01-01 at 10.00.00.png", "SCR-20260101-abc.png"] {
        assert!(is_screenshot(Path::new(&format!("/tmp/none/{name}"))), "{name}");
    }
    assert!(!is_screenshot(Path::new("/tmp/none/IMG_1234.jpg")));
    assert!(!is_screenshot(Path::new("/tmp/none/my screenshot.png")));
}

#[test]
fn exif_orientation_6_rotates_clockwise_and_reports_dimensions() {
    let dir = tempfile::tempdir().unwrap();
    let jpeg = encode(&gradient(300, 200), ImageFormat::Jpeg);
    let bytes = jpeg_with_orientation(&jpeg, 6);
    let path = write_temp(dir.path(), "IMG_rot.jpg", &bytes);
    let out = process_image(&path).expect("oriented jpeg processes");
    assert_eq!(out.orientation, 6);
    assert_eq!((out.width, out.height), (200, 300));
    assert_eq!((out.thumbnail.width, out.thumbnail.height), (200, 300));
}

#[test]
fn exif_orientation_1_leaves_pixels_alone() {
    let dir = tempfile::tempdir().unwrap();
    let jpeg = encode(&gradient(300, 200), ImageFormat::Jpeg);
    let path = write_temp(dir.path(), "IMG_plain.jpg", &jpeg_with_orientation(&jpeg, 1));
    let out = process_image(&path).expect("jpeg processes");
    assert_eq!((out.width, out.height), (300, 200));
}

#[test]
fn unknown_bytes_are_decode_errors_and_heic_is_unsupported() {
    let dir = tempfile::tempdir().unwrap();
    let junk = write_temp(dir.path(), "junk.png", b"not an image at all, just text");
    assert!(matches!(process_image(&junk), Err(ExtractError::Decode(_))));
    let mut heic = vec![0, 0, 0, 24];
    heic.extend_from_slice(b"ftypheic");
    heic.extend_from_slice(&[0; 12]);
    let heic_path = write_temp(dir.path(), "IMG.HEIC", &heic);
    assert!(matches!(process_image(&heic_path), Err(ExtractError::Unsupported(_))));
}

#[test]
fn missing_file_is_io_error() {
    let err = process_image(Path::new("/nonexistent/definitely/missing.jpg")).unwrap_err();
    assert!(matches!(err, ExtractError::Io(_)));
}

#[test]
fn average_hash_is_stable_under_scaling_and_differs_for_different_content() {
    let a = gradient(800, 600);
    let scaled = image::imageops::resize(&a, 400, 300, image::imageops::FilterType::Triangle);
    let inverted = RgbImage::from_fn(800, 600, |x, y| {
        let p = a.get_pixel(x, y).0;
        Rgb([255 - p[0], 255 - p[1], 255 - p[2]])
    });
    let h = average_hash_8x8(&a);
    assert!(hamming(h, average_hash_8x8(&scaled)) <= 4);
    assert!(hamming(h, average_hash_8x8(&inverted)) > 4 || h == 0);
    assert_eq!(hamming(0b1011, 0b0010), 2);
    assert_eq!(average_hash_8x8(&RgbImage::new(0, 0)), 0);
}

#[test]
fn dedup_compares_against_last_kept_frame() {
    // b is 1 bit from a (dropped), c is 3 bits from a (dropped), d is 5 bits from a (kept).
    let a = 0u64;
    let b = 0b1u64;
    let c = 0b111u64;
    let d = 0b1_1111u64; // 5 bits from a
    let kept = dedup_frames(&[a, b, c, d]);
    assert_eq!(kept, vec![0, 3]);
    assert_eq!(dedup_frames(&[]), Vec::<usize>::new());
}

#[test]
fn frame_plan_matches_swift_stride() {
    assert_eq!(plan_frame_times(10.0), vec![0.5, 3.5, 6.5, 9.5]);
    assert!(plan_frame_times(0.0).is_empty());
    assert!(plan_frame_times(f64::NAN).is_empty());
    assert!(plan_frame_times(-3.0).is_empty());
    let long = plan_frame_times(4000.0);
    assert_eq!(long.len(), 1200);
    assert!((long[1] - long[0] - 4000.0 / 1200.0).abs() < 1e-9);
    assert!(long.iter().all(|&t| t < 4000.0));
    let tiny = plan_frame_times(0.4);
    assert_eq!(tiny, vec![0.2]);
}
