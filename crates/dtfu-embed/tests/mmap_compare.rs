//! Measures the `Options::use_mmap` switch on the text GGUF: load time, peak RSS and embedding parity.
//! Ignored by default (needs the model files and takes a while). Run with:
//!   cargo test -p dtfu-embed --release --test mmap_compare -- --ignored --nocapture
//! Set DTFU_MODELS to the folder holding the GGUF files (default: ~/Library/Application Support/DigUp/Models).
//!
//! Each mode runs in its own child process (this same test binary, selected by DTFU_MMAP_CHILD), because
//! ru_maxrss is a process-lifetime peak and would otherwise carry over from the first mode.

use dtfu_embed::{cosine, Embedder, Options};
use std::path::PathBuf;
use std::process::Command;
use std::time::Instant;

const TEXTS: [&str; 3] = [
    "How do I restore a deleted file on macOS?",
    "Quarterly budget spreadsheet for 2025",
    "Photos from the beach trip in Kenting",
];

#[repr(C)]
struct Timeval {
    sec: i64,
    usec: i64,
}
#[repr(C)]
struct Rusage {
    utime: Timeval,
    stime: Timeval,
    maxrss: i64, // bytes on macOS
    rest: [i64; 13],
}
unsafe extern "C" {
    fn getrusage(who: i32, usage: *mut Rusage) -> i32;
}
const RUSAGE_SELF: i32 = 0;

fn peak_rss_bytes() -> u64 {
    let mut ru = Rusage { utime: Timeval { sec: 0, usec: 0 }, stime: Timeval { sec: 0, usec: 0 }, maxrss: 0, rest: [0; 13] };
    let rc = unsafe { getrusage(RUSAGE_SELF, &mut ru) };
    assert_eq!(rc, 0, "getrusage failed");
    ru.maxrss as u64
}

fn model_path() -> Option<PathBuf> {
    let dir = std::env::var("DTFU_MODELS").map(PathBuf::from).unwrap_or_else(|_| {
        let home = std::env::var("HOME").unwrap_or_default();
        PathBuf::from(home).join("Library/Application Support/DigUp/Models")
    });
    let p = dir.join("embeddinggemma-2-Q8_0.gguf");
    p.exists().then_some(p)
}

/// Child: loads the text model 3 times in the mode given by DTFU_MMAP_CHILD, writes timings, peak RSS and vectors.
#[test]
#[ignore]
fn mmap_child() {
    let Ok(mode) = std::env::var("DTFU_MMAP_CHILD") else { return };
    let out = PathBuf::from(std::env::var("DTFU_MMAP_OUT").expect("DTFU_MMAP_OUT"));
    let model = model_path().expect("model file not found");
    let opts = Options { use_mmap: mode == "mmap", ..Options::default() };
    let texts: Vec<String> = TEXTS.iter().map(|s| s.to_string()).collect();

    let mut load_s = Vec::new();
    let mut first_embed_s = Vec::new();
    let mut vecs = Vec::new();
    for _ in 0..3 {
        let t0 = Instant::now();
        let mut e = Embedder::new(&model, None, &opts).expect("load");
        let t1 = Instant::now();
        vecs = e.embed_texts(&texts).expect("embed");
        let t2 = Instant::now();
        load_s.push((t1 - t0).as_secs_f64());
        first_embed_s.push((t2 - t0).as_secs_f64());
        drop(e);
    }
    let v = serde_json::json!({
        "load_s": load_s,
        "load_plus_embed_s": first_embed_s,
        "peak_rss_bytes": peak_rss_bytes(),
        "vectors": vecs,
    });
    std::fs::write(out, serde_json::to_vec(&v).unwrap()).unwrap();
}

fn median(v: &[f64]) -> f64 {
    let mut s = v.to_vec();
    s.sort_by(|a, b| a.partial_cmp(b).unwrap());
    s[s.len() / 2]
}

/// Parent: runs the child for each mode, prints the numbers and checks the embeddings agree.
#[test]
#[ignore]
fn mmap_vs_ram() {
    if model_path().is_none() {
        eprintln!("SKIPPED: embeddinggemma-2-Q8_0.gguf not found");
        return;
    }
    let exe = std::env::current_exe().unwrap();
    let dir = std::env::temp_dir();
    let mut results = Vec::new();
    for mode in ["mmap", "ram"] {
        let out = dir.join(format!("dtfu_mmap_{mode}_{}.json", std::process::id()));
        let st = Command::new(&exe)
            .args(["--exact", "mmap_child", "--ignored", "--nocapture", "--test-threads=1"])
            .env("DTFU_MMAP_CHILD", mode)
            .env("DTFU_MMAP_OUT", &out)
            .status()
            .unwrap();
        assert!(st.success(), "child {mode} failed");
        let v: serde_json::Value = serde_json::from_slice(&std::fs::read(&out).unwrap()).unwrap();
        let _ = std::fs::remove_file(&out);
        results.push((mode, v));
    }
    let mut report = String::new();
    for (mode, v) in &results {
        let load: Vec<f64> = v["load_s"].as_array().unwrap().iter().map(|x| x.as_f64().unwrap()).collect();
        let tot: Vec<f64> = v["load_plus_embed_s"].as_array().unwrap().iter().map(|x| x.as_f64().unwrap()).collect();
        report += &format!(
            "{mode:>5}: load median {:.3}s (runs {:?}), load+first embed median {:.3}s, peak RSS {:.1} MiB\n",
            median(&load),
            load.iter().map(|x| (x * 1000.0).round() / 1000.0).collect::<Vec<_>>(),
            median(&tot),
            v["peak_rss_bytes"].as_u64().unwrap() as f64 / (1024.0 * 1024.0),
        );
    }
    let to_vecs = |v: &serde_json::Value| -> Vec<Vec<f32>> {
        v["vectors"].as_array().unwrap().iter().map(|r| r.as_array().unwrap().iter().map(|x| x.as_f64().unwrap() as f32).collect()).collect()
    };
    let (a, b) = (to_vecs(&results[0].1), to_vecs(&results[1].1));
    assert_eq!(a.len(), TEXTS.len());
    assert_eq!(b.len(), TEXTS.len());
    for i in 0..TEXTS.len() {
        let c = cosine(&a[i], &b[i]);
        report += &format!("text {i}: cosine(mmap, ram) = {c:.7}\n");
        assert!(c > 0.99999, "text {i}: cosine {c} <= 0.99999");
    }
    println!("{report}");
}
