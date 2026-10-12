//! Embedding throughput: `DTFU_MODELS=models cargo run --release -p dtfu-embed --example bench_embed -- [images_dir] [threads]`
//! Measures texts (1800-char chunks, batches of 16), images at the 140 / 280 budgets, and 30 s audio windows.
use dtfu_embed::{Embedder, Options};
use std::path::{Path, PathBuf};
use std::time::Instant;

fn main() -> anyhow::Result<()> {
    let args: Vec<String> = std::env::args().collect();
    let images_dir = PathBuf::from(args.get(1).cloned().unwrap_or_else(|| format!("{}/DigUpTestbed/synthetic/img", std::env::var("HOME").unwrap())));
    let threads = args.get(2).and_then(|t| t.parse().ok());
    let models = PathBuf::from(std::env::var("DTFU_MODELS").unwrap_or_else(|_| "models".into()));
    let t0 = Instant::now();
    let mut e = Embedder::new(
        &models.join("embeddinggemma-2-Q8_0.gguf"),
        Some(&models.join("mmproj-embeddinggemma-2-Q8_0.gguf")),
        &Options { threads, gpu_layers: std::env::var("DTFU_GPU_LAYERS").ok().and_then(|v| v.parse().ok()).unwrap_or(999) },
    )?;
    println!("load + warm-up: {:.2} s (threads {:?})", t0.elapsed().as_secs_f64(), threads);

    let base = "The quick brown fox jumps over the lazy dog while the committee reviews the quarterly report on shipping delays. ";
    let texts: Vec<String> = (0..64).map(|i| format!("title: note {i} | text: {}", base.repeat(16))).collect();
    let t = Instant::now();
    for chunk in texts.chunks(16) {
        e.embed_texts(chunk)?;
    }
    let s = t.elapsed().as_secs_f64();
    println!("text: {} chunks of ~1800 chars in {:.2} s = {:.1} chunks/s", texts.len(), s, texts.len() as f64 / s);

    let mut files: Vec<PathBuf> = std::fs::read_dir(&images_dir)?.filter_map(|d| d.ok()).map(|d| d.path()).collect();
    files.sort();
    let imgs: Vec<(usize, usize, Vec<u8>)> = files
        .iter()
        .filter_map(|p| image::open(p).ok())
        .map(|i| {
            let i = i.thumbnail(1024, 1024).to_rgb8();
            (i.width() as usize, i.height() as usize, i.into_raw())
        })
        .collect();
    for budget in [140usize, 280] {
        let t = Instant::now();
        for (w, h, rgb) in &imgs {
            if let Some((th, tw)) = dtfu_embed::sizing::target_size(*h, *w, budget) {
                let _ = dtfu_embed::sizing::resize_bicubic(rgb, *w, *h, tw, th);
            }
        }
        println!("  (resize alone, budget {budget}: {:.2} s for {} images)", t.elapsed().as_secs_f64(), imgs.len());
        let t = Instant::now();
        for (w, h, rgb) in &imgs {
            e.embed_image_rgb(*w, *h, rgb, budget)?;
        }
        let s = t.elapsed().as_secs_f64();
        println!("image budget {budget}: {} images in {:.2} s = {:.1} images/s", imgs.len(), s, imgs.len() as f64 / s);
    }

    let audio: Vec<f32> = (0..16000 * 30).map(|i| ((i as f32) * 0.01).sin() * 0.2).collect();
    let t = Instant::now();
    for _ in 0..10 {
        e.embed_audio_samples(&audio)?;
    }
    let s = t.elapsed().as_secs_f64();
    println!("audio: 10 windows of 30 s in {:.2} s = {:.1} windows/s", s, 10.0 / s);
    let _ = Path::new("");
    Ok(())
}
