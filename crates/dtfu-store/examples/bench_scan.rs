//! Benchmark for the brute-force vector scan, every `ScanMode`.
//! Usage: cargo run --release -p dtfu-store --example bench_scan -- [rows]
//! Shipped config: plain release build. Comparison: RUSTFLAGS="-C target-cpu=native" (label it when reporting).

use dtfu_core::DIM;
use dtfu_store::scan::{best_kernel, Kernel};
use dtfu_store::{top_k, ScanMode, VectorIndex};
use std::time::Instant;

fn main() {
    let n: usize = std::env::args().nth(1).and_then(|s| s.parse().ok()).unwrap_or(100_000);
    let cpu = std::process::Command::new("sysctl")
        .args(["-n", "machdep.cpu.brand_string"])
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .unwrap_or_else(|| "unknown".into());
    let mut idx = VectorIndex::default();
    let mut s: u64 = 0x9E3779B97F4A7C15;
    let mut v = vec![0f32; DIM];
    for i in 0..n {
        for x in v.iter_mut() {
            s ^= s << 13;
            s ^= s >> 7;
            s ^= s << 17;
            *x = (s >> 40) as f32 / (1u64 << 24) as f32 - 0.5;
        }
        let norm = v.iter().map(|x| x * x).sum::<f32>().sqrt();
        v.iter_mut().for_each(|x| *x /= norm);
        idx.push(i as i64, &v);
    }
    let q = v.clone();
    let bytes = (n * DIM * 2) as f64;
    println!(
        "machine: {} {} cpu=\"{}\" cores={} rayon_threads={} simd_kernel={}",
        std::env::consts::ARCH,
        std::env::consts::OS,
        cpu,
        std::thread::available_parallelism().map(|c| c.get()).unwrap_or(0),
        rayon::current_num_threads(),
        best_kernel().name(),
    );
    println!("rows={n} dim={DIM} data={:.2} GB (f16)", bytes / 1e9);
    println!("kernels available: {}", Kernel::ALL.iter().filter(|k| k.available()).map(|k| k.name()).collect::<Vec<_>>().join(", "));

    for mode in [ScanMode::SerialNaive, ScanMode::SerialLanes, ScanMode::ParallelLanes, ScanMode::Simd] {
        let mut best = f64::MAX;
        for _ in 0..5 {
            let t = Instant::now();
            let sc = idx.scores(&q, mode);
            std::hint::black_box(&sc);
            best = best.min(t.elapsed().as_secs_f64());
        }
        println!("{:<14} best {:>8.2} ms   {:>6.2} GB/s", format!("{mode:?}"), best * 1e3, bytes / best / 1e9);
    }

    let sc = idx.scores(&q, ScanMode::Simd);
    let t = Instant::now();
    let top = top_k(&sc, 100);
    println!("top_k(100)     {:>8.2} ms   (best score {:.4})", t.elapsed().as_secs_f64() * 1e3, top[0].1);
}
