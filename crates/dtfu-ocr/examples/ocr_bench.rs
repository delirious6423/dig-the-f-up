//! OCR engine comparison on a corpus of PNGs with ground-truth text (`<name>.png` + `<name>.txt`, see
//! `evals/ocr/gen_corpus.swift`). Per engine: median / p95 seconds per image, characters recognized, and the
//! character error rate (CER) against the ground truth with all whitespace removed.
//!
//! `cargo run --release -p dtfu-ocr --features lighton --example ocr_bench -- <corpus-dir> <reps> <engine,...> [--out dir]`
//! Timing only starts when the 1-minute load average is below 4 (waits up to 25 min); the load observed during the
//! timed runs is printed per engine. `--out dir` also writes each engine's recognized text to `<dir>/<engine>/<name>.txt`.
use dtfu_ocr::{recognize_rgb_with, Engine};
use std::path::PathBuf;
use std::time::{Duration, Instant};

#[cfg(unix)]
extern "C" {
    fn getloadavg(avg: *mut f64, n: i32) -> i32;
}

#[cfg(unix)]
fn load1() -> f64 {
    let mut a = [0f64; 3];
    if unsafe { getloadavg(a.as_mut_ptr(), 3) } < 1 {
        return f64::NAN;
    }
    a[0]
}

/// Windows has no load average: report 0 so the idle gate never waits there.
#[cfg(not(unix))]
fn load1() -> f64 {
    0.0
}

fn wait_for_idle() -> bool {
    if std::env::var_os("OCR_BENCH_NOWAIT").is_some() {
        return load1() < 4.0; // quality-only pass: timings are then flagged as not final
    }
    let start = Instant::now();
    while load1() >= 4.0 {
        if start.elapsed() > Duration::from_secs(25 * 60) {
            return false;
        }
        std::thread::sleep(Duration::from_secs(15));
    }
    true
}

fn levenshtein(a: &[char], b: &[char]) -> usize {
    let mut prev: Vec<usize> = (0..=b.len()).collect();
    for (i, ca) in a.iter().enumerate() {
        let mut cur = vec![i + 1];
        for (j, cb) in b.iter().enumerate() {
            cur.push((prev[j] + usize::from(ca != cb)).min(prev[j + 1] + 1).min(cur[j] + 1));
        }
        prev = cur;
    }
    prev[b.len()]
}

fn squash(s: &str) -> Vec<char> {
    s.chars().filter(|c| !c.is_whitespace()).collect()
}

fn pct(sorted: &[f64], p: f64) -> f64 {
    if sorted.is_empty() {
        return f64::NAN;
    }
    sorted[(((sorted.len() - 1) as f64) * p).round() as usize]
}

type Item = (String, usize, usize, Vec<u8>, Vec<char>); // name, width, height, rgb, ground truth

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.len() < 3 {
        eprintln!("usage: ocr_bench <corpus-dir> <reps> <engine,...> [--out dir]");
        std::process::exit(2);
    }
    let dir = PathBuf::from(&args[0]);
    let reps: usize = args[1].parse().expect("reps");
    let engines: Vec<Engine> = args[2].split(',').map(|n| Engine::parse(n).unwrap_or_else(|| panic!("engine {n}"))).collect();
    let out_dir = args.iter().position(|a| a == "--out").map(|i| PathBuf::from(&args[i + 1]));

    let mut names: Vec<String> = std::fs::read_dir(&dir)
        .expect("corpus dir")
        .filter_map(|e| e.ok()?.path().file_stem()?.to_str().map(String::from))
        .collect();
    names.sort();
    names.dedup();
    names.retain(|n| dir.join(format!("{n}.png")).exists() && dir.join(format!("{n}.txt")).exists());
    let images: Vec<Item> = names
        .iter()
        .map(|n| {
            let img = image::open(dir.join(format!("{n}.png"))).expect("png").to_rgb8();
            let truth = squash(&std::fs::read_to_string(dir.join(format!("{n}.txt"))).expect("txt"));
            (n.clone(), img.width() as usize, img.height() as usize, img.into_raw(), truth)
        })
        .collect();
    println!("corpus {} images, reps {reps}", images.len());

    for engine in engines {
        if !engine.is_compiled() {
            println!("## {}: not compiled in", engine.name());
            continue;
        }
        let t = Instant::now();
        // First call loads models (excluded from the per-image times) and shows whether the engine works at all.
        let warm = recognize_rgb_with(engine, images[0].1, images[0].2, &images[0].3);
        let warm_s = t.elapsed().as_secs_f64();
        if warm.is_none() && engine != Engine::Vision {
            // None can also mean "no text"; the corpus's first image has text, so treat it as unavailable.
            println!("## {}: unavailable (no output on the warm-up image; models missing?)", engine.name());
            continue;
        }
        let idle = wait_for_idle();
        let mut times = Vec::new();
        let (mut max_load, mut total_chars, mut total_err, mut total_truth) = (0f64, 0usize, 0usize, 0usize);
        println!("## {} (first call incl. model load {warm_s:.2}s; started at load {:.2}{})", engine.name(), load1(), if idle { "" } else { " NOT IDLE" });
        println!("| image | chars | CER | median s |");
        println!("|---|---|---|---|");
        for (name, w, h, rgb, truth) in &images {
            let mut text = None;
            let mut per = Vec::new();
            for _ in 0..reps {
                let t = Instant::now();
                text = recognize_rgb_with(engine, *w, *h, rgb);
                per.push(t.elapsed().as_secs_f64());
                max_load = max_load.max(load1());
            }
            let text = text.unwrap_or_default();
            let got = squash(&text);
            let err = levenshtein(&got, truth);
            total_chars += text.chars().count();
            total_err += err;
            total_truth += truth.len();
            per.sort_by(|a, b| a.total_cmp(b));
            println!("| {name} | {} | {:.3} | {:.2} |", text.chars().count(), err as f64 / truth.len().max(1) as f64, pct(&per, 0.5));
            times.extend(per);
            if let Some(o) = &out_dir {
                let d = o.join(engine.name());
                std::fs::create_dir_all(&d).ok();
                std::fs::write(d.join(format!("{name}.txt")), &text).ok();
            }
        }
        times.sort_by(|a, b| a.total_cmp(b));
        println!(
            "TOTAL {}: n={} timed runs, median {:.3}s, p95 {:.3}s, chars {total_chars}, micro-CER {:.4}, max 1-min load during timing {max_load:.2}{}\n",
            engine.name(),
            times.len(),
            pct(&times, 0.5),
            pct(&times, 0.95),
            total_err as f64 / total_truth.max(1) as f64,
            if idle && max_load < 4.0 { "" } else { "  ** UNDER LOAD: not final **" }
        );
    }
    #[cfg(feature = "lighton")]
    dtfu_ocr::lighton::unload();
}
