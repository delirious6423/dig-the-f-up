//! Resident (cached) searcher benchmark on a synthetic index database.
//!
//! Compares the old behaviour (a new `Searcher` per query: open the index, load every vector, search) with the
//! resident behaviour that `dtfu-engine` now uses (one `OwnedSearcher` kept between queries, `is_current()` checked
//! before each query, rebuilt when another connection commits). Phases:
//!   1. fresh per query: open + `Searcher::new` + search, FRESH_COUNT queries;
//!   2. cached: first query, then QUERY_COUNT queries on the cached searcher (median / p95), and the `is_current()`
//!      cost on its own (IS_CURRENT_COUNT calls);
//!   3. identity: IDENTITY_COUNT queries, cached vs a freshly loaded searcher, compared by their `Debug` output;
//!   4. rebuild after a commit by another connection: REBUILD_COUNT commits, each followed by `is_current` (false),
//!      the rebuild (open + load) and one query;
//!   5. churn: a writer thread (its own connection) commits one segment per second for CHURN_SECS seconds while the
//!      reader queries every 100 ms through the same `is_current` + rebuild path the engine uses.
//!
//!   7. refresh vs rebuild (`--refresh`, alone with `--only-refresh`): another connection commits K new segments
//!      (K = 1000, 10000, one tx), then `OwnedSearcher::refresh` is timed against a full rebuild (open + load) of the same
//!      state, and the refreshed searcher is compared with the fresh one on IDENTITY_COUNT queries.
//!
//! Every timed phase first waits until the 1-minute load average is below LOAD_LIMIT (polling `vm.loadavg` every
//! 10 s, up to LOAD_WAIT). It prints the load before and after the phase, and marks the phase LOADED if the load was
//! still at or above the limit when the wait ran out or when the phase ended. LOADED timings are not final.
//!
//! Usage: cargo run --release -p dtfu-search --example resident_bench -- [N] [--db PATH] [--churn SECS] [--only-refresh]
//! Default N = 100000. Default DB: <temp dir>/dtfu-resident-N.sqlite (any existing file is deleted first).
//! Opens the index with `dtfu_store::open` (the same pragmas as the store), not the engine's `IndexStore::open`.

use anyhow::Result;
use dtfu_core::DIM;
use dtfu_search::{SearchOptions, Searcher};
use dtfu_store::{encode_f16, open};
use rusqlite::{params, Connection};
use std::fs;
use std::path::PathBuf;
use std::process::Command;
use std::thread;
use std::time::{Duration, Instant};

const VOCAB: usize = 2000;
const QUERY_VECTORS: usize = 20;
const FRESH_COUNT: usize = 20;
const QUERY_COUNT: usize = 200;
const IS_CURRENT_COUNT: usize = 1000;
const IDENTITY_COUNT: usize = 50;
const REBUILD_COUNT: usize = 5;
const LOAD_LIMIT: f64 = 4.0;
const LOAD_WAIT: Duration = Duration::from_secs(25 * 60);

struct Rng(u64);

impl Rng {
    fn next_u64(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }
    fn unit(&mut self) -> f64 {
        (self.next_u64() >> 11) as f64 / (1u64 << 53) as f64
    }
    fn below(&mut self, n: usize) -> usize {
        ((self.unit() * n as f64) as usize).min(n - 1)
    }
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

/// Same layout as `dtfu-search`'s `bench_search`: 70% text (with FTS rows), 20% image, 10% audio.
fn build(path: &std::path::Path, n: usize, vocab: &[String]) -> Result<f64> {
    let t = Instant::now();
    let mut conn = open(path)?;
    let mut rng = Rng(0x9E37_79B9_7F4A_7C15);
    let mut files = 0usize;
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
            file_stmt.execute(params![path_text, file_kind, size, mtime, files as i64, 1i64])?;
            let file_id = tx.last_insert_rowid();
            let segs = 1 + (rng.next_u64() & 1) as usize;
            for s in 0..segs {
                if seg_total >= n {
                    break;
                }
                let v = unit_vector(&mut rng);
                let loc = if kind_idx == 2 { (s * 30) as f64 } else { (s + 1) as f64 };
                seg_stmt.execute(params![file_id, seg_kind, modality, loc, encode_f16(&v)])?;
                let seg_id = tx.last_insert_rowid();
                if kind_idx == 0 {
                    let words = 20 + rng.below(21);
                    let body: Vec<&str> = (0..words).map(|_| vocab[rng.skewed_word()].as_str()).collect();
                    fts_stmt.execute(params![seg_id, body.join(" ")])?;
                }
                seg_total += 1;
            }
        }
        drop(file_stmt);
        drop(seg_stmt);
        drop(fts_stmt);
        tx.commit()?;
    }
    conn.execute_batch("PRAGMA wal_checkpoint(TRUNCATE);")?;
    Ok(t.elapsed().as_secs_f64())
}

/// One new segment with its own file, committed on `conn` (a connection other than the reader's).
fn insert_one(conn: &mut Connection, seq: u64, rng: &mut Rng) -> Result<()> {
    let tx = conn.transaction()?;
    let path_text = format!("/bench/churn/{seq:08}.dat");
    tx.execute(
        "INSERT INTO files(path, kind, size, mtime, inode, device, state) VALUES(?1, 'image', 1000, 1.7e9, ?2, 1, 'done')",
        params![path_text, (1u64 << 40) as i64 + seq as i64],
    )?;
    let fid = tx.last_insert_rowid();
    tx.execute(
        "INSERT INTO segments(file, kind, modality, loc, loc_end, excerpt, vec) VALUES(?1, 'image', 'image', 1.0, NULL, NULL, ?2)",
        params![fid, encode_f16(&unit_vector(rng))],
    )?;
    tx.commit()?;
    Ok(())
}

/// (median, p95) by nearest rank.
fn stat(mut v: Vec<f64>) -> (f64, f64) {
    if v.is_empty() {
        return (f64::NAN, f64::NAN);
    }
    v.sort_by(|a, b| a.partial_cmp(b).unwrap());
    let rank = |p: f64| v[((p * v.len() as f64).ceil() as usize).clamp(1, v.len()) - 1];
    (rank(0.5), rank(0.95))
}

fn fmt_stat((m, p): (f64, f64), unit: &str) -> String {
    format!("p50 {m:.2} {unit} p95 {p:.2} {unit}")
}

fn ms(t: Instant) -> f64 {
    t.elapsed().as_secs_f64() * 1e3
}

fn sysctl(key: &str) -> String {
    Command::new("sysctl")
        .args(["-n", key])
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .unwrap_or_else(|| "unknown".into())
}

/// 1-minute load average from `vm.loadavg` (format "{ 1.00 2.00 3.00 }").
fn load1() -> f64 {
    sysctl("vm.loadavg").split_whitespace().nth(1).and_then(|s| s.parse().ok()).unwrap_or(f64::NAN)
}

struct Gate {
    before: f64,
    loaded: bool,
}

/// Waits for the load to drop below LOAD_LIMIT, then prints the load at the start of the phase.
fn gate(label: &str) -> Gate {
    let start = Instant::now();
    let mut loaded = false;
    loop {
        let l = load1();
        if l < LOAD_LIMIT {
            break;
        }
        if start.elapsed() >= LOAD_WAIT {
            loaded = true;
            break;
        }
        println!("[{label}] waiting: load1={l:.2} >= {LOAD_LIMIT}, waited {:.0} s", start.elapsed().as_secs_f64());
        thread::sleep(Duration::from_secs(10));
    }
    let before = load1();
    println!(
        "[{label}] START load1={before:.2} waited={:.0} s{}",
        start.elapsed().as_secs_f64(),
        if loaded { "  LOADED: timings below are NOT final" } else { "" }
    );
    Gate { before, loaded: loaded || before >= LOAD_LIMIT }
}

fn end(g: Gate, label: &str) {
    let after = load1();
    let loaded = g.loaded || after >= LOAD_LIMIT;
    println!(
        "[{label}] END load1={after:.2} (start {:.2}){}",
        g.before,
        if loaded { "  LOADED: timings are NOT final" } else { "  ok" }
    );
}

/// `count` new files with one image segment each, one transaction.
fn insert_batch(conn: &mut Connection, first_seq: u64, count: usize, rng: &mut Rng) -> Result<()> {
    let tx = conn.transaction()?;
    {
        let mut f = tx.prepare_cached(
            "INSERT INTO files(path, kind, size, mtime, inode, device, state) VALUES(?1, 'image', 1000, 1.7e9, ?2, 1, 'done')",
        )?;
        let mut s = tx.prepare_cached(
            "INSERT INTO segments(file, kind, modality, loc, loc_end, excerpt, vec) VALUES(?1, 'image', 'image', 1.0, NULL, NULL, ?2)",
        )?;
        for i in 0..count as u64 {
            let seq = first_seq + i;
            f.execute(params![format!("/bench/batch/{seq:08}.dat"), (1u64 << 41) as i64 + seq as i64])?;
            s.execute(params![tx.last_insert_rowid(), encode_f16(&unit_vector(rng))])?;
        }
    }
    tx.commit()?;
    Ok(())
}

struct Args {
    only_refresh: bool,
    n: usize,
    db: PathBuf,
    churn_secs: u64,
}

fn parse_args() -> Args {
    let mut n = 100_000usize;
    let mut db = None;
    let mut churn_secs = 30u64;
    let mut only_refresh = false;
    let mut args = std::env::args().skip(1);
    while let Some(a) = args.next() {
        if a == "--db" {
            db = args.next().map(PathBuf::from);
        } else if a == "--only-refresh" {
            only_refresh = true;
        } else if a == "--churn" {
            churn_secs = args.next().and_then(|s| s.parse().ok()).unwrap_or(30);
        } else if let Ok(v) = a.parse::<usize>() {
            n = v;
        } else {
            eprintln!("usage: resident_bench [N] [--db PATH] [--churn SECS]");
            std::process::exit(2);
        }
    }
    let db = db.unwrap_or_else(|| std::env::temp_dir().join(format!("dtfu-resident-{n}.sqlite")));
    Args { only_refresh, n, db, churn_secs }
}

fn main() -> Result<()> {
    let args = parse_args();
    let db = args.db.clone();
    println!(
        "machine: hw.model={} chip=\"{}\" mem_bytes={} cores={} os={} rayon_threads={}",
        sysctl("hw.model"),
        sysctl("machdep.cpu.brand_string"),
        sysctl("hw.memsize"),
        std::thread::available_parallelism().map(|c| c.get()).unwrap_or(0),
        std::env::consts::OS,
        rayon::current_num_threads(),
    );
    println!("N={} segments, dim={DIM} (f16), db={}, churn={} s", args.n, db.display(), args.churn_secs);

    for suffix in ["", "-wal", "-shm", "-journal"] {
        let mut p = db.clone().into_os_string();
        p.push(suffix);
        let _ = fs::remove_file(p);
    }
    let mut vrng = Rng(0x9E37_79B9_7F4A_7C15 ^ 0x1234);
    let vocab = vocabulary(&mut vrng);
    let g = gate("build");
    let build_s = build(&db, args.n, &vocab)?;
    end(g, "build");
    println!("build: {build_s:.2} s, db size {:.3} GB", fs::metadata(&db)?.len() as f64 / 1e9);

    let reader = open(&db)?;
    let mut qrng = Rng(0xD1B5_4A32_D192_ED03);
    let qvecs: Vec<Vec<f32>> = (0..QUERY_VECTORS).map(|_| unit_vector(&mut qrng)).collect();
    // Two common words, one mid-frequency, one rare: each query uses one of them with a vector.
    let words: Vec<String> = vec![vocab[0].clone(), vocab[1].clone(), vocab[20].clone(), vocab[VOCAB - 1].clone()];
    let query = |i: usize| (qvecs[i % QUERY_VECTORS].as_slice(), words[i % words.len()].as_str());
    let opts = SearchOptions::default();
    let vectors = Searcher::new(&reader)?.vector_count();
    println!("vectors={vectors}");

    let refresh_phase = || -> Result<()> {
        let mut owned = Searcher::with_owned_connection(open(&db)?, false)?;
        let mut writer = open(&db)?;
        let mut wrng = Rng(0x7777_1111_2222_3333);
        let mut seq = 5_000_000u64;
        for k in [1_000usize, 10_000] {
            let label = format!("7 refresh vs rebuild, {k} new segments");
            insert_batch(&mut writer, seq, k, &mut wrng)?;
            seq += k as u64;
            assert!(!owned.is_current()?);
            let g = gate(&label);
            let t = Instant::now();
            let stats = owned.refresh()?;
            let refresh_ms = ms(t);
            let t = Instant::now();
            let fresh = Searcher::with_owned_connection(open(&db)?, false)?;
            let rebuild_ms = ms(t);
            end(g, &label);
            let mut mismatches = 0usize;
            for i in 0..IDENTITY_COUNT {
                let (qv, text) = query(i);
                if format!("{:?}", fresh.search(qv, text, &opts)?) != format!("{:?}", owned.search(qv, text, &opts)?) {
                    mismatches += 1;
                }
            }
            println!(
                "RESULT refresh k={k} on {vectors} vectors: refresh {refresh_ms:.1} ms (full={} removed={} added={} reevaluated={}), rebuild {rebuild_ms:.1} ms, speedup x{:.1}, identity mismatches={mismatches}/{IDENTITY_COUNT}",
                stats.full, stats.removed, stats.added, stats.reevaluated, rebuild_ms / refresh_ms
            );
            assert!(owned.is_current()?);
        }
        Ok(())
    };
    if args.only_refresh {
        return refresh_phase();
    }

    // 1. Old behaviour: open, load every vector, search, per query.
    let g = gate("1 fresh per query");
    let (mut build_v, mut search_v, mut total_v) = (Vec::new(), Vec::new(), Vec::new());
    for i in 0..FRESH_COUNT {
        let (qv, text) = query(i);
        let t = Instant::now();
        let conn = open(&db)?;
        let s = Searcher::new(&conn)?;
        let b = ms(t);
        let t2 = Instant::now();
        let hits = s.search(qv, text, &opts)?;
        let sm = ms(t2);
        std::hint::black_box(&hits);
        build_v.push(b);
        search_v.push(sm);
        total_v.push(b + sm);
    }
    end(g, "1 fresh per query");
    let (bm, bp) = stat(build_v);
    let (sm, sp) = stat(search_v);
    let (tm, tp) = stat(total_v);
    println!("RESULT fresh_per_query n={FRESH_COUNT}: open+load p50 {bm:.1} ms p95 {bp:.1} | search p50 {sm:.2} ms p95 {sp:.2} | total p50 {tm:.1} ms p95 {tp:.1}");

    // 2. Cached: one load, then queries on the resident searcher.
    let g = gate("2 cached load");
    let t = Instant::now();
    let mut owned = Searcher::with_owned_connection(open(&db)?, false)?;
    let cached_load_ms = ms(t);
    end(g, "2 cached load");
    println!("RESULT cached_initial_load: {cached_load_ms:.1} ms (open + Searcher load, once)");

    let g = gate("2 cached queries");
    let (qv, text) = query(0);
    let t = Instant::now();
    std::hint::black_box(owned.search(qv, text, &opts)?);
    let first_ms = ms(t);
    let mut cached_search = Vec::new();
    let mut cached_total = Vec::new();
    for i in 0..QUERY_COUNT {
        let (qv, text) = query(i);
        let t = Instant::now();
        let current = owned.is_current()?;
        let check = ms(t);
        let t2 = Instant::now();
        std::hint::black_box(owned.search(qv, text, &opts)?);
        let sm = ms(t2);
        assert!(current, "index changed during the cached phase");
        cached_search.push(sm);
        cached_total.push(check + sm);
    }
    end(g, "2 cached queries");
    println!("RESULT cached_first_query: {first_ms:.2} ms");
    let (m, p) = stat(cached_search);
    println!("RESULT cached_search n={QUERY_COUNT}: p50 {m:.2} ms p95 {p:.2} ms");
    let (m, p) = stat(cached_total);
    println!("RESULT cached_query_incl_is_current n={QUERY_COUNT}: p50 {m:.2} ms p95 {p:.2} ms");

    // 3. is_current on its own.
    let g = gate("3 is_current");
    let mut ic = Vec::with_capacity(IS_CURRENT_COUNT);
    for _ in 0..IS_CURRENT_COUNT {
        let t = Instant::now();
        std::hint::black_box(owned.is_current()?);
        ic.push(ms(t) * 1e3);
    }
    end(g, "3 is_current");
    let (m, p) = stat(ic);
    println!("RESULT is_current n={IS_CURRENT_COUNT}: p50 {m:.1} us p95 {p:.1} us");

    // 4. Identity: cached vs a freshly loaded searcher, same index, IDENTITY_COUNT queries.
    let g = gate("4 identity");
    let fresh = Searcher::new(&reader)?;
    let mut mismatches = 0usize;
    let mut nonempty = 0usize;
    for i in 0..IDENTITY_COUNT {
        let (qv, text) = query(i);
        let a = format!("{:?}", fresh.search(qv, text, &opts)?);
        let b = format!("{:?}", owned.search(qv, text, &opts)?);
        if a != b {
            mismatches += 1;
        }
        if a != "[]" {
            nonempty += 1;
        }
    }
    end(g, "4 identity");
    println!("RESULT identity cached vs fresh: {IDENTITY_COUNT} queries, mismatches={mismatches}, non-empty={nonempty}");

    // 5. Rebuild after a commit by another connection (data_version changes).
    let g = gate("5 rebuild after commit");
    let mut writer = open(&db)?;
    let mut wrng = Rng(0xABCD_EF01_2345_6789);
    let (mut rb_check, mut rb_build, mut rb_query) = (Vec::new(), Vec::new(), Vec::new());
    for r in 0..REBUILD_COUNT {
        insert_one(&mut writer, r as u64, &mut wrng)?;
        let t = Instant::now();
        let current = owned.is_current()?;
        rb_check.push(ms(t));
        assert!(!current, "commit by another connection was not seen by data_version");
        let t = Instant::now();
        owned = Searcher::with_owned_connection(open(&db)?, false)?;
        rb_build.push(ms(t));
        let (qv, text) = query(r);
        let t = Instant::now();
        std::hint::black_box(owned.search(qv, text, &opts)?);
        rb_query.push(ms(t));
    }
    end(g, "5 rebuild after commit");
    let (m, p) = stat(rb_check);
    println!("RESULT rebuild_detect (is_current false) n={REBUILD_COUNT}: p50 {m:.1} us p95 {p:.1} us");
    let (m, p) = stat(rb_build);
    println!("RESULT rebuild (open + load after commit) n={REBUILD_COUNT}: p50 {m:.1} ms p95 {p:.1} ms");
    let (m, p) = stat(rb_query);
    println!("RESULT query right after rebuild n={REBUILD_COUNT}: p50 {m:.2} ms p95 {p:.2} ms");

    // 6. Churn: one commit per second from another connection; reader queries every 100 ms via is_current + rebuild.
    let g = gate("6 churn");
    let churn_dur = Duration::from_secs(args.churn_secs);
    let (mut lat_hold, mut lat_rebuild, mut rebuild_ms) = (Vec::new(), Vec::new(), Vec::new());
    let mut queries = 0usize;
    let commits = thread::scope(|s| -> Result<u64> {
        let writer_handle = s.spawn(|| -> Result<u64> {
            let mut c = open(&db)?;
            let mut rng = Rng(0x5555_AAAA_1234_5678);
            let start = Instant::now();
            let mut seq = 1_000u64;
            let mut next = start;
            while start.elapsed() < churn_dur {
                insert_one(&mut c, seq, &mut rng)?;
                seq += 1;
                next += Duration::from_secs(1);
                if let Some(d) = next.checked_duration_since(Instant::now()) {
                    thread::sleep(d);
                }
            }
            Ok(seq - 1_000)
        });
        let end_at = Instant::now() + churn_dur;
        let mut i = 0usize;
        while Instant::now() < end_at {
            let t = Instant::now();
            let mut rebuilt = false;
            if !owned.is_current()? {
                let rt = Instant::now();
                owned = Searcher::with_owned_connection(open(&db)?, false)?;
                rebuild_ms.push(ms(rt));
                rebuilt = true;
            }
            let (qv, text) = query(i);
            std::hint::black_box(owned.search(qv, text, &opts)?);
            let total = ms(t);
            if rebuilt {
                lat_rebuild.push(total);
            } else {
                lat_hold.push(total);
            }
            queries += 1;
            i += 1;
            thread::sleep(Duration::from_millis(100));
        }
        let commits = writer_handle.join().expect("writer thread panicked")?;
        Ok(commits)
    })?;
    end(g, "6 churn");
    println!(
        "RESULT churn {}s, commits={commits}, queries={queries}, rebuilds={}",
        args.churn_secs,
        rebuild_ms.len()
    );
    println!("RESULT churn query, no rebuild n={}: {}", lat_hold.len(), fmt_stat(stat(lat_hold), "ms"));
    println!("RESULT churn query incl. rebuild n={}: {}", lat_rebuild.len(), fmt_stat(stat(lat_rebuild), "ms"));
    let (m, p) = stat(rebuild_ms);
    println!("RESULT churn rebuild only: p50 {m:.1} ms p95 {p:.1} ms");
    refresh_phase()
}
