//! Usage: crawl_bench <dir> [threads=0] [--code]
//! Prints files/sec for the parallel crawl (threads=1 approximates serial; 0 = all cores).
use digup_crawl::{crawl, CrawlOptions};
use std::time::Instant;

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let Some(dir) = args.first() else {
        eprintln!("usage: crawl_bench <dir> [threads] [--code]");
        std::process::exit(2);
    };
    let threads = args.get(1).and_then(|s| s.parse().ok()).unwrap_or(0);
    let opts = CrawlOptions { threads, code: args.iter().any(|a| a == "--code"), ..Default::default() };
    let t = Instant::now();
    let report = crawl(&[dir.as_str()], &opts);
    let secs = t.elapsed().as_secs_f64();
    let skipped: usize = report.skipped.values().sum();
    let seen = report.candidates.len() + skipped;
    println!(
        "threads={} candidates={} skipped_files={} skipped_folders={} datasets={} in {:.3}s => {:.0} files/sec ({:.0} candidates/sec)",
        if threads == 0 { rayon::current_num_threads() } else { threads },
        report.candidates.len(),
        skipped,
        report.skipped_folders.values().sum::<usize>(),
        report.datasets.len(),
        secs,
        seen as f64 / secs,
        report.candidates.len() as f64 / secs
    );
}
