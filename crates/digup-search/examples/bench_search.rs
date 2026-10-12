//! End-to-end search benchmark at scale on a synthetic index database.
//! Builds a DB with N segments (70% text chunks with keyword rows, 20% image, 10% audio; about 1.5 segments per
//! file; every segment has a random L2-normalized 768-d float16 vector), then times `Searcher::new` (vector load),
//! `Searcher::search` (vector-only and text+vector) and the raw SIMD scan (`VectorIndex::scores`).
//! Usage: cargo run --release -p digup-search --example bench_search -- [N] [--db PATH]
//! Default N = 100000. Default DB: <temp dir>/digup-bench-search-N.sqlite (any existing file is deleted first).
//! Shipped config: plain release build. Label any RUSTFLAGS="-C target-cpu=native" run when reporting.

use anyhow::Result;
use digup_core::DIM;
use digup_search::{SearchOptions, Searcher};
use digup_store::scan::best_kernel;
use digup_store::{encode_f16, ScanMode, VectorIndex};
use rusqlite::{params, Connection};
use std::fs;
use std::path::PathBuf;
use std::time::Instant;

/// Synthetic vocabulary size. Word draws are skewed (index = 2000 * u^2), so low indices are common, high ones rare.
const VOCAB: usize = 2000;
const QUERY_VECTORS: usize = 20;

struct Rng(u64);

impl Rng {
    fn next_u64(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }
    /// Uniform in [0, 1).
    fn unit(&mut self) -> f64 {
        (self.next_u64() >> 11) as f64 / (1u64 << 53) as f64
    }
    fn below(&mut self, n: usize) -> usize {
        ((self.unit() * n as f64) as usize).min(n - 1)
    }
    /// Skewed word index: common words have low indices.
    fn skewed_word(&mut self) -> usize {
        let u = self.unit();
        ((u * u * VOCAB as f64) as usize).min(VOCAB - 1)
    }
}

fn vocabulary(rng: &mut Rng) -> Vec<String> {
    (0..VOCAB)
        .map(|_| {
            let len = 4 + rng.below(6);
            (0..len).map(|_| (b'a' + rng.below(26) as u8) as char).collect()
        })
        .collect()
}

fn unit_vector(rng: &mut Rng) -> Vec<f32> {
    let mut v: Vec<f32> = (0..DIM).map(|_| rng.unit() as f32 - 0.5).collect();
    let norm = v.iter().map(|x| x * x).sum::<f32>().sqrt();
    v.iter_mut().for_each(|x| *x /= norm);
    v
}

/// Builds the database with raw SQL in one transaction, mirroring `IndexStore::insert_file`:
/// `fts.rowid == segments.id`, text rows carry the body (no `original`), image and audio segments have no keyword row.
/// Returns (build seconds including checkpoint, file count, segment mix [text, image, audio]).
fn build(path: &std::path::Path, n: usize, vocab: &[String]) -> Result<(f64, usize, [usize; 3])> {
    let t = Instant::now();
    let mut conn = digup_store::open(path)?;
    let mut rng = Rng(0x9E37_79B9_7F4A_7C15);
    let mut files = 0usize;
    let mut mix = [0usize; 3];
    let mut seg_total = 0usize;
    {
        let tx = conn.transaction()?;
        let mut file_stmt = tx.prepare_cached(
            "INSERT INTO files(path, kind, size, mtime, inode, device, state) VALUES(?1, ?2, ?3, ?4, ?5, ?6, 'done')",
        )?;
        let mut seg_stmt = tx.prepare_cached(
            "INSERT INTO segments(file, kind, modality, loc, loc_end, excerpt, vec) VALUES(?1, ?2, ?3, ?4, NULL, NULL, ?5)",
        )?;
        let mut fts_stmt = tx.prepare_cached("INSERT INTO fts(rowid, name, body, original) VALUES(?1, '', ?2, NULL)")?;
        while seg_total < n {
            let u = rng.unit();
            let kind_idx = if u < 0.7 { 0 } else if u < 0.9 { 1 } else { 2 };
            let (file_kind, seg_kind, modality) = match kind_idx {
                0 => ("doc", "chunk", "text"),
                1 => ("image", "image", "image"),
                _ => ("audio", "audio", "audio"),
            };
            files += 1;
            let path_text = format!("/bench/{file_kind}/{files:08}.dat");
            let size = 10_000 + rng.below(5_000_000) as i64;
            let mtime = 1.7e9 + files as f64;
            let inode = files as i64;
            file_stmt.execute(params![path_text, file_kind, size, mtime, inode, 1i64])?;
            let file_id = tx.last_insert_rowid();
            // 1 or 2 segments per file, so about 1.5 on average.
            let segs = 1 + (rng.next_u64() & 1) as usize;
            for s in 0..segs {
                if seg_total >= n {
                    break;
                }
                let v: Vec<f32> = unit_vector(&mut rng);
                let blob = encode_f16(&v);
                let loc = if kind_idx == 2 { (s * 30) as f64 } else { (s + 1) as f64 };
                seg_stmt.execute(params![file_id, seg_kind, modality, loc, blob])?;
                let seg_id = tx.last_insert_rowid();
                if kind_idx == 0 {
                    let words = 20 + rng.below(21);
                    let body: Vec<&str> = (0..words).map(|_| vocab[rng.skewed_word()].as_str()).collect();
                    fts_stmt.execute(params![seg_id, body.join(" ")])?;
                }
                mix[kind_idx] += 1;
                seg_total += 1;
            }
        }
        drop(file_stmt);
        drop(seg_stmt);
        drop(fts_stmt);
        tx.commit()?;
    }
    conn.execute_batch("PRAGMA wal_checkpoint(TRUNCATE);")?;
    Ok((t.elapsed().as_secs_f64(), files, mix))
}

fn df(conn: &Connection, word: &str) -> Result<i64> {
    Ok(conn.query_row("SELECT count(*) FROM fts WHERE fts MATCH ?1", [word], |r| r.get(0))?)
}

fn time_ms<T>(f: impl FnOnce() -> Result<T>) -> Result<f64> {
    let t = Instant::now();
    let out = f()?;
    let ms = t.elapsed().as_secs_f64() * 1e3;
    std::hint::black_box(&out);
    Ok(ms)
}

/// (median, p95) by nearest rank.
fn stat(mut v: Vec<f64>) -> (f64, f64) {
    v.sort_by(|a, b| a.partial_cmp(b).unwrap());
    let rank = |p: f64| v[((p * v.len() as f64).ceil() as usize).clamp(1, v.len()) - 1];
    (rank(0.5), rank(0.95))
}

fn sysctl(key: &str) -> String {
    std::process::Command::new("sysctl")
        .args(["-n", key])
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .unwrap_or_else(|| "unknown".into())
}

fn uname_m() -> String {
    std::process::Command::new("uname")
        .arg("-m")
        .output()
        .ok()
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .unwrap_or_else(|| "unknown".into())
}

fn parse_args() -> (usize, PathBuf) {
    let mut n = 100_000usize;
    let mut db = None;
    let mut args = std::env::args().skip(1);
    while let Some(a) = args.next() {
        if a == "--db" {
            db = args.next().map(PathBuf::from);
        } else if let Ok(v) = a.parse::<usize>() {
            n = v;
        } else {
            eprintln!("usage: bench_search [N] [--db PATH]");
            std::process::exit(2);
        }
    }
    let db = db.unwrap_or_else(|| std::env::temp_dir().join(format!("digup-bench-search-{n}.sqlite")));
    (n, db)
}

fn main() -> Result<()> {
    let (n, db) = parse_args();
    println!(
        "machine: uname -m={} os={} cpu=\"{}\" cores={} rayon_threads={} simd_kernel={}",
        uname_m(),
        std::env::consts::OS,
        sysctl("machdep.cpu.brand_string"),
        std::thread::available_parallelism().map(|c| c.get()).unwrap_or(0),
        rayon::current_num_threads(),
        best_kernel().name(),
    );
    println!("N={n} segments, dim={DIM} (f16), db={}", db.display());

    for suffix in ["", "-wal", "-shm", "-journal"] {
        let mut p = db.clone().into_os_string();
        p.push(suffix);
        let _ = fs::remove_file(p);
    }

    let mut vrng = Rng(0x2545_F491_4F6C_DD1D);
    let vocab = vocabulary(&mut vrng);
    let (build_s, files, mix) = build(&db, n, &vocab)?;
    let db_bytes = fs::metadata(&db)?.len();
    println!(
        "build: {build_s:.2} s (vectors + rows + commit + checkpoint); files={files} segments={n} \
         mix text/image/audio={}/{}/{}",
        mix[0], mix[1], mix[2]
    );
    println!("db size: {:.3} GB ({db_bytes} bytes)", db_bytes as f64 / 1e9);

    let conn = digup_store::open(&db)?;
    let common: Vec<String> = (0..3).map(|i| vocab[i].clone()).collect();
    let rare = vocab[VOCAB - 1].clone();
    for w in common.iter().chain(std::iter::once(&rare)) {
        println!("query word {w:?}: fts rows with this word = {}", df(&conn, w)?);
    }

    let t = Instant::now();
    let searcher = Searcher::new(&conn)?;
    let load_s = t.elapsed().as_secs_f64();
    println!("Searcher::new (vector load): {load_s:.3} s, vectors={}", searcher.vector_count());

    let opts = SearchOptions::default();
    let mut qrng = Rng(0xD1B5_4A32_D192_ED03);
    let qvecs: Vec<Vec<f32>> = (0..QUERY_VECTORS).map(|_| unit_vector(&mut qrng)).collect();
    // Untimed warm-up: first query faults in the vector pages and the FTS b-tree pages.
    searcher.search(&qvecs[0], "", &opts)?;

    let mut vec_only = Vec::new();
    let mut common_ms = Vec::new();
    let mut rare_ms = Vec::new();
    for qv in &qvecs {
        // Empty query text: vector-only.
        vec_only.push(time_ms(|| searcher.search(qv, "", &opts))?);
        for w in &common {
            common_ms.push(time_ms(|| searcher.search(qv, w, &opts))?);
        }
        rare_ms.push(time_ms(|| searcher.search(qv, &rare, &opts))?);
    }
    let (m, p) = stat(vec_only);
    println!("search vector-only  (text \"\", n=20):     p50 {m:8.2} ms   p95 {p:8.2} ms");
    let (m, p) = stat(common_ms);
    println!("search text+vec common word (n=60):     p50 {m:8.2} ms   p95 {p:8.2} ms");
    let (m, p) = stat(rare_ms);
    println!("search text+vec rare word   (n=20):     p50 {m:8.2} ms   p95 {p:8.2} ms");

    drop(searcher);
    // Scan alone, on a separately loaded copy of the same vectors.
    let vi = VectorIndex::load(&conn)?;
    std::hint::black_box(vi.scores(&qvecs[0], ScanMode::Simd));
    let mut scan_ms = Vec::new();
    for qv in &qvecs {
        scan_ms.push(time_ms(|| Ok(vi.scores(qv, ScanMode::Simd)))?);
    }
    let (m, p) = stat(scan_ms);
    println!("scan only VectorIndex::scores(Simd) (n=20): p50 {m:8.2} ms   p95 {p:8.2} ms");
    Ok(())
}
