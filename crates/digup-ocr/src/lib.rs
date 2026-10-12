//! Screenshot OCR. Engines: Apple Vision (macOS default), `ocrs` (default elsewhere) and, with feature `lighton`,
//! LightOnOCR-3-0.8B; pick one with `$DIGUP_OCR` or [`recognize_rgb_with`]. On macOS: Apple Vision. Elsewhere: the pure-Rust `ocrs` engine
//! (see `ocrs_backend`; models are not bundled, see that module's docs). On macOS the
//! `ocrs` backend is available with `--features ocrs` through `recognize_rgb_ocrs`.

#[cfg(any(not(target_os = "macos"), feature = "ocrs"))]
mod ocrs_backend;

#[cfg(feature = "lighton")]
mod llama_backend;
/// LightOnOCR-3-0.8B engine (feature `lighton`): model loading, options and the shared instance.
#[cfg(feature = "lighton")]
pub mod lighton {
    pub use crate::llama_backend::*;
}

/// An OCR engine. All variants exist on every platform; [`Engine::is_compiled`] says whether this build has it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Engine {
    /// Apple Vision (macOS only).
    Vision,
    /// The pure-Rust `ocrs` engine (off macOS, or with feature `ocrs`).
    Ocrs,
    /// LightOnOCR-3-0.8B through llama.cpp (feature `lighton`; model files needed, see `digup-models`).
    Lighton,
}

/// Environment variable that selects the engine used by [`recognize_rgb`]: `vision`, `ocrs` or `lighton`.
pub const ENGINE_ENV: &str = "DIGUP_OCR";

impl Engine {
    /// Vision on macOS, `ocrs` elsewhere.
    pub fn platform_default() -> Engine {
        if cfg!(target_os = "macos") {
            Engine::Vision
        } else {
            Engine::Ocrs
        }
    }

    /// Parses `vision` | `ocrs` | `lighton` (case-insensitive; `lightonocr` and `apple` are accepted aliases).
    pub fn parse(name: &str) -> Option<Engine> {
        match name.trim().to_ascii_lowercase().as_str() {
            "vision" | "apple" => Some(Engine::Vision),
            "ocrs" => Some(Engine::Ocrs),
            "lighton" | "lightonocr" => Some(Engine::Lighton),
            _ => None,
        }
    }

    /// The engine named by `$DIGUP_OCR`, or the platform default when unset, empty or unknown.
    pub fn from_env() -> Engine {
        match std::env::var(ENGINE_ENV) {
            Ok(v) if !v.trim().is_empty() => Engine::parse(&v).unwrap_or_else(|| {
                eprintln!("digup-ocr: unknown {ENGINE_ENV}={v:?} (use vision, ocrs or lighton); using the default");
                Engine::platform_default()
            }),
            _ => Engine::platform_default(),
        }
    }

    /// Whether this build contains the engine (it may still lack its models at run time).
    pub fn is_compiled(self) -> bool {
        match self {
            Engine::Vision => cfg!(target_os = "macos"),
            Engine::Ocrs => cfg!(any(not(target_os = "macos"), feature = "ocrs")),
            Engine::Lighton => cfg!(feature = "lighton"),
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            Engine::Vision => "vision",
            Engine::Ocrs => "ocrs",
            Engine::Lighton => "lighton",
        }
    }
}

/// Recognize text in a packed RGB8 image (`rgb.len() >= width*height*3`).
/// Returns recognized lines joined by "\n", or `None` if nothing was found.
/// Uses the engine named by `$DIGUP_OCR` ([`Engine::from_env`]); by default Apple Vision on macOS and `ocrs`
/// elsewhere. If the selected engine is not compiled in, the platform default is used instead.
pub fn recognize_rgb(width: usize, height: usize, rgb: &[u8]) -> Option<String> {
    let engine = Engine::from_env();
    let engine = if engine.is_compiled() { engine } else { Engine::platform_default() };
    recognize_rgb_with(engine, width, height, rgb)
}

/// Same contract as [`recognize_rgb`] with an explicit engine. `None` for invalid images, an engine that is not
/// compiled in or lacks its models, and images with no text.
pub fn recognize_rgb_with(engine: Engine, width: usize, height: usize, rgb: &[u8]) -> Option<String> {
    if !valid_image(width, height, rgb) {
        return None;
    }
    match engine {
        #[cfg(target_os = "macos")]
        Engine::Vision => mac::recognize(width, height, rgb),
        #[cfg(any(not(target_os = "macos"), feature = "ocrs"))]
        Engine::Ocrs => ocrs_backend::recognize(width, height, rgb),
        #[cfg(feature = "lighton")]
        Engine::Lighton => llama_backend::recognize(width, height, rgb),
        #[allow(unreachable_patterns)]
        _ => None,
    }
}

/// Same contract as [`recognize_rgb`], but always uses the `ocrs` engine.
/// Only available off macOS or with the `ocrs` feature.
#[cfg(any(not(target_os = "macos"), feature = "ocrs"))]
pub fn recognize_rgb_ocrs(width: usize, height: usize, rgb: &[u8]) -> Option<String> {
    if !valid_image(width, height, rgb) {
        return None;
    }
    ocrs_backend::recognize(width, height, rgb)
}

fn valid_image(width: usize, height: usize, rgb: &[u8]) -> bool {
    width != 0
        && height != 0
        && width.checked_mul(height).and_then(|n| n.checked_mul(3)).is_some_and(|need| rgb.len() >= need)
}

/// Apple Vision OCR engine (stateless; all objects are created per call).
#[cfg(target_os = "macos")]
#[derive(Debug, Default, Clone, Copy)]
pub struct VisionOcr;

#[cfg(target_os = "macos")]
impl VisionOcr {
    pub fn new() -> Self {
        VisionOcr
    }
    pub fn recognize(&self, width: usize, height: usize, rgb: &[u8]) -> Option<String> {
        recognize_rgb_with(Engine::Vision, width, height, rgb)
    }
}

#[cfg(target_os = "macos")]
mod mac {
    use objc2::rc::autoreleasepool;
    use objc2::runtime::AnyObject;
    use objc2::AnyThread;
    use objc2_core_graphics::{
        CGColorRenderingIntent, CGColorSpace, CGDataProvider, CGImage, CGBitmapInfo,
        CGImageAlphaInfo,
    };
    use objc2_foundation::{NSArray, NSDictionary, NSObjectProtocol};
    use objc2_vision::{
        VNImageRequestHandler, VNRecognizeTextRequest, VNRecognizedTextObservation, VNRequest,
        VNRequestTextRecognitionLevel,
    };
    use std::ffi::c_void;

    pub fn recognize(width: usize, height: usize, rgb: &[u8]) -> Option<String> {
        // Expand to RGBA (alpha skipped) so the layout is a universally supported one.
        let n = width * height;
        let mut rgba = Vec::with_capacity(n * 4);
        #[allow(unknown_lints, clippy::chunks_exact_to_as_chunks)]
        for px in rgb[..n * 3].chunks_exact(3) {
            rgba.extend_from_slice(&[px[0], px[1], px[2], 255]);
        }
        autoreleasepool(|_| unsafe {
            let provider = CGDataProvider::with_data(
                std::ptr::null_mut(),
                rgba.as_ptr() as *const c_void,
                rgba.len(),
                None,
            )?;
            let space = CGColorSpace::new_device_rgb()?;
            let image = CGImage::new(
                width,
                height,
                8,
                32,
                width * 4,
                Some(&space),
                CGBitmapInfo(CGImageAlphaInfo::NoneSkipLast.0),
                Some(&provider),
                std::ptr::null(),
                false,
                CGColorRenderingIntent::RenderingIntentDefault,
            )?;
            let options = NSDictionary::<objc2_foundation::NSString, AnyObject>::new();
            let handler =
                VNImageRequestHandler::initWithCGImage_options(VNImageRequestHandler::alloc(), &image, &options);
            let request = VNRecognizeTextRequest::new();
            request.setRecognitionLevel(VNRequestTextRecognitionLevel::Accurate);
            request.setUsesLanguageCorrection(true);
            if request.respondsToSelector(objc2::sel!(setAutomaticallyDetectsLanguage:)) {
                request.setAutomaticallyDetectsLanguage(true);
            }
            let req: &VNRequest = &request;
            let arr = NSArray::from_slice(&[req]);
            handler.performRequests_error(&arr).ok()?;
            let results = request.results()?;
            let mut lines: Vec<String> = Vec::new();
            for obs in results.iter() {
                let obs: &VNRecognizedTextObservation = obs.downcast_ref()?;
                let cands = obs.topCandidates(1);
                if let Some(c) = cands.firstObject() {
                    lines.push(c.string().to_string());
                }
            }
            let text = lines.join("\n");
            if text.trim().is_empty() {
                None
            } else {
                Some(text)
            }
        })
    }
}

#[cfg(all(test, target_os = "macos"))]
mod tests {
    use super::*;
    use crate::test_util::{load_font, render};

    #[test]
    fn hello_and_blank() {
        let blank = vec![255u8; 300 * 200 * 3];
        let r = recognize_rgb(300, 200, &blank);
        assert!(r.is_none_or(|s| s.trim().is_empty()));

        let font = load_font();
        let Some(font) = font else {
            eprintln!("NOTE: no loadable font; skipping HELLO test");
            return;
        };
        let (w, h, img) = render("HELLO", &font);
        let out = VisionOcr::new().recognize(w, h, &img).expect("some text");
        assert!(out.to_uppercase().contains("HELLO"), "got {out:?}");
    }
}

#[cfg(test)]
mod test_util {
    use ab_glyph::{Font, FontVec, PxScale, ScaleFont};

    pub fn load_font() -> Option<FontVec> {
        [
            "/System/Library/Fonts/Helvetica.ttc",
            "/System/Library/Fonts/Supplemental/Arial.ttf",
            "/usr/share/fonts/truetype/dejavu/DejaVuSans.ttf",
            "/usr/share/fonts/TTF/DejaVuSans.ttf",
            "/usr/share/fonts/dejavu/DejaVuSans.ttf",
            "C:\\Windows\\Fonts\\arial.ttf",
        ]
        .iter()
        .find_map(|p| std::fs::read(p).ok().and_then(|d| FontVec::try_from_vec(d).ok()))
    }

pub fn render(text: &str, font: &FontVec) -> (usize, usize, Vec<u8>) {
    let (w, h) = (700usize, 240usize);
    let mut img = vec![255u8; w * h * 3];
    let scale = PxScale::from(140.0);
    let sf = font.as_scaled(scale);
    let mut x = 30.0f32;
    let base = 170.0f32;
    for ch in text.chars() {
        let id = font.glyph_id(ch);
        let g = id.with_scale_and_position(scale, ab_glyph::point(x, base));
        x += sf.h_advance(id);
        if let Some(o) = font.outline_glyph(g) {
            let b = o.px_bounds();
            o.draw(|gx, gy, c| {
                let px = b.min.x as i32 + gx as i32;
                let py = b.min.y as i32 + gy as i32;
                if px >= 0 && py >= 0 && (px as usize) < w && (py as usize) < h {
                    let i = (py as usize * w + px as usize) * 3;
                    let v = (255.0 * (1.0 - c)) as u8;
                    img[i] = img[i].min(v);
                    img[i + 1] = img[i + 1].min(v);
                    img[i + 2] = img[i + 2].min(v);
                }
            });
        }
    }
    (w, h, img)
}

}

#[cfg(all(test, any(not(target_os = "macos"), feature = "ocrs")))]
mod ocrs_tests {
    use super::*;
    use crate::test_util::{load_font, render};

    #[test]
    fn ocrs_text_image_contains_words() {
        if !ocrs_backend::models_available() {
            eprintln!("NOTE: ocrs models not found (DIGUP_OCR_MODELS / models/ocr / data dir); skipping");
            return;
        }
        let Some(font) = load_font() else {
            eprintln!("NOTE: no loadable font; skipping ocrs text test");
            return;
        };
        let (w, h, img) = render("HELLO WORLD", &font);
        let out = recognize_rgb_ocrs(w, h, &img).expect("some text");
        let up = out.to_uppercase();
        assert!(up.contains("HELLO") && up.contains("WORLD"), "got {out:?}");
    }

    #[test]
    fn blank_image_is_none() {
        let blank = vec![255u8; 300 * 200 * 3];
        assert_eq!(recognize_rgb_ocrs(300, 200, &blank), None);
        assert_eq!(recognize_rgb_ocrs(0, 0, &[]), None);
    }
}

#[cfg(test)]
mod engine_tests {
    use super::*;

    #[test]
    fn parse_names() {
        assert_eq!(Engine::parse("Vision"), Some(Engine::Vision));
        assert_eq!(Engine::parse(" ocrs "), Some(Engine::Ocrs));
        assert_eq!(Engine::parse("lighton"), Some(Engine::Lighton));
        assert_eq!(Engine::parse("nope"), None);
        assert!(Engine::platform_default().is_compiled());
    }

    #[test]
    fn invalid_image_is_none_for_every_engine() {
        for e in [Engine::Vision, Engine::Ocrs, Engine::Lighton] {
            assert_eq!(recognize_rgb_with(e, 0, 0, &[]), None);
            assert_eq!(recognize_rgb_with(e, 10, 10, &[0; 5]), None);
        }
    }
}

#[cfg(all(test, feature = "lighton"))]
mod lighton_tests {
    use super::*;
    use crate::test_util::{load_font, render};

    #[test]
    fn lighton_reads_hello_world() {
        if !lighton::available() {
            eprintln!("NOTE: LightOnOCR model files not found (DIGUP_LIGHTON_MODELS); skipping");
            return;
        }
        let Some(font) = load_font() else { return };
        let (w, h, img) = render("HELLO WORLD", &font);
        let out = recognize_rgb_with(Engine::Lighton, w, h, &img).expect("some text");
        // The test renderer's 700 px canvas clips "WORLD", so only the first word is checked.
        assert!(out.to_uppercase().contains("HELLO"), "got {out:?}");
    }
}
