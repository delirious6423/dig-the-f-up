//! Step-1 gate: load LightOnOCR-3 (Q8_0 + mmproj) through llama.cpp/mtmd and print the text of one image.
//! `cargo run --release -p dtfu-ocr --features lighton --example lighton_probe -- <image> [model.gguf mmproj.gguf]`
//! Without model arguments the files are located through dtfu-models (`DTFU_LIGHTON_MODELS` or the data dir).
use dtfu_ocr::lighton::{model_paths, LightOn, Options};
use std::time::Instant;

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let Some(img_path) = args.first() else {
        eprintln!("usage: lighton_probe <image> [model.gguf mmproj.gguf]");
        std::process::exit(2);
    };
    let (model, mmproj) = if args.len() >= 3 {
        (args[1].clone().into(), args[2].clone().into())
    } else {
        model_paths().expect("model files not found; set DTFU_LIGHTON_MODELS")
    };
    let img = image::open(img_path).expect("open image").to_rgb8();
    let (w, h) = (img.width() as usize, img.height() as usize);
    let t = Instant::now();
    let mut engine = LightOn::load(&model, &mmproj, Options::default()).unwrap_or_else(|e| {
        eprintln!("LOAD FAILED: {e}");
        std::process::exit(1);
    });
    eprintln!("loaded in {:.2}s; image {w}x{h}", t.elapsed().as_secs_f64());
    for i in 0..2 {
        let t = Instant::now();
        match engine.recognize(w, h, img.as_raw()) {
            Ok(text) => {
                eprintln!("run {i}: {:.2}s, {} chars", t.elapsed().as_secs_f64(), text.chars().count());
                if i == 0 {
                    println!("{text}");
                }
            }
            Err(e) => {
                eprintln!("RECOGNIZE FAILED: {e}");
                std::process::exit(1);
            }
        }
    }
}
