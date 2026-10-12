//! SQLite storage (schema v5, compatible with DigUp's Swift app) and the vector scan.

use anyhow::{bail, Result};
use dtfu_core::{DIM, SCHEMA_VERSION};
use half::f16;
use rayon::prelude::*;
use rusqlite::Connection;
use std::path::Path;
use std::time::Duration;

pub mod index;
pub mod lock;
pub mod migrate;
pub mod scan;

const DDL: &str = r#"
CREATE TABLE meta(key TEXT PRIMARY KEY, value TEXT NOT NULL);
CREATE TABLE files(
    id INTEGER PRIMARY KEY,
    path TEXT NOT NULL UNIQUE,
    kind TEXT NOT NULL,
    size INTEGER NOT NULL,
    mtime REAL NOT NULL,
    inode INTEGER NOT NULL,
    device INTEGER NOT NULL,
    state TEXT NOT NULL DEFAULT 'pending',
    detail TEXT,
    info TEXT,
    indexed_at REAL
);
CREATE INDEX files_inode ON files(device, inode);
CREATE INDEX files_state ON files(state, kind);
CREATE TABLE segments(
    id INTEGER PRIMARY KEY,
    file INTEGER NOT NULL REFERENCES files(id) ON DELETE CASCADE,
    kind TEXT NOT NULL,
    modality TEXT NOT NULL,
    loc REAL,
    loc_end REAL,
    excerpt TEXT,
    vec BLOB
);
CREATE INDEX segments_file ON segments(file);
CREATE VIRTUAL TABLE fts USING fts5(
    name, body, original UNINDEXED,
    tokenize = "porter unicode61 remove_diacritics 2 categories 'L* N* Co M*'"
);
PRAGMA user_version = 5;
"#;

pub fn open(path: &Path) -> Result<Connection> {
    let c = Connection::open(path)?;
    configure(&c)?;
    init(&c)?;
    Ok(c)
}

fn is_corruption(e: &anyhow::Error) -> bool {
    matches!(
        e.downcast_ref::<rusqlite::Error>(),
        Some(rusqlite::Error::SqliteFailure(f, _))
            if matches!(f.code, rusqlite::ErrorCode::DatabaseCorrupt | rusqlite::ErrorCode::NotADatabase)
    )
}

/// `PRAGMA quick_check(1)` on a fresh connection; true when it reports anything but "ok". Only run after an open error.
fn quick_check_fails(path: &Path) -> bool {
    let Ok(c) = Connection::open(path) else { return false };
    match c.query_row("PRAGMA quick_check(1)", [], |r| r.get::<_, String>(0)) {
        Ok(s) => s != "ok",
        Err(rusqlite::Error::SqliteFailure(f, _)) => {
            matches!(f.code, rusqlite::ErrorCode::DatabaseCorrupt | rusqlite::ErrorCode::NotADatabase)
        }
        Err(_) => false,
    }
}

/// Like [`open`], but when opening fails because the file is damaged, moves the database and its `-wal`/`-shm` to
/// `<name>.corrupt-<unix secs>` (never deleted) and creates a fresh one. The flag is true when that happened.
pub fn open_recovering(path: &Path) -> Result<(Connection, bool)> {
    let err = match open(path) {
        Ok(c) => return Ok((c, false)),
        Err(e) => e,
    };
    if !path.exists() || !(is_corruption(&err) || quick_check_fails(path)) {
        return Err(err);
    }
    let mut secs =
        std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0);
    let base = path.to_string_lossy().into_owned();
    // Never overwrite an earlier copy (two recoveries within a second).
    while std::path::Path::new(&format!("{base}.corrupt-{secs}")).exists() {
        secs += 1;
    }
    // The -wal and -shm go first and the database last: a fresh database must never start next to the old WAL. If
    // any move fails, the ones already done are undone and the error is returned (nothing is opened).
    let mut moved: Vec<(std::path::PathBuf, std::path::PathBuf)> = Vec::new();
    for suffix in ["-wal", "-shm", ""] {
        let from = std::path::PathBuf::from(format!("{base}{suffix}"));
        if !from.exists() {
            continue;
        }
        let to = std::path::PathBuf::from(format!("{base}.corrupt-{secs}{suffix}"));
        if let Err(e) = std::fs::rename(&from, &to) {
            for (f, t) in moved.iter().rev() {
                let _ = std::fs::rename(t, f);
            }
            return Err(anyhow::anyhow!("moving damaged index {}: {e}", from.display()));
        }
        moved.push((from, to));
    }
    eprintln!("warning: index {} was damaged ({err:#}); moved to {base}.corrupt-{secs} and starting a fresh one", path.display());
    Ok((open(path)?, true))
}

pub fn open_in_memory() -> Result<Connection> {
    let c = Connection::open_in_memory()?;
    configure(&c)?;
    init(&c)?;
    Ok(c)
}

fn configure(c: &Connection) -> Result<()> {
    c.busy_timeout(Duration::from_millis(5000))?;
    set_wal(c)?;
    c.execute_batch("PRAGMA synchronous = NORMAL; PRAGMA foreign_keys = ON;")?;
    Ok(())
}

/// Switches to WAL. SQLite can return SQLITE_BUSY from this pragma without calling the busy handler when another
/// process is opening the same fresh file, so retry for the busy timeout's span instead of failing the open.
fn set_wal(c: &Connection) -> Result<()> {
    let deadline = std::time::Instant::now() + Duration::from_millis(5000);
    loop {
        match c.query_row("PRAGMA journal_mode = WAL", [], |_| Ok(())) {
            Ok(()) => return Ok(()),
            Err(rusqlite::Error::SqliteFailure(e, _))
                if matches!(e.code, rusqlite::ErrorCode::DatabaseBusy | rusqlite::ErrorCode::DatabaseLocked)
                    && std::time::Instant::now() < deadline =>
            {
                std::thread::sleep(Duration::from_millis(5));
            }
            Err(e) => return Err(e.into()),
        }
    }
}

fn init(c: &Connection) -> Result<()> {
    if user_version(c)? == 0 {
        create(c)?;
    }
    // Re-read: another process may have created the schema (or moved it on) since the check above.
    let v = user_version(c)?;
    match v {
        SCHEMA_VERSION => {}
        v if v > SCHEMA_VERSION => bail!("index schema {v} is newer than this build ({SCHEMA_VERSION})"),
        1..SCHEMA_VERSION => migrate::migrate(c, v)?,
        v => bail!("index schema {v} is not a valid version"),
    }
    Ok(())
}

fn user_version(c: &Connection) -> Result<i64> {
    Ok(c.query_row("PRAGMA user_version", [], |r| r.get(0))?)
}

/// Creates the schema of a fresh index in one `BEGIN IMMEDIATE` transaction, as `migrate.rs` does for v1-4. The version
/// is read again inside the transaction, so a second process that opened the same fresh file waits for the first and
/// then finds the schema there, instead of creating it again.
fn create(c: &Connection) -> Result<()> {
    c.execute_batch("BEGIN IMMEDIATE")?;
    let result = (|| -> Result<()> {
        if user_version(c)? == 0 {
            c.execute_batch(DDL)?;
        }
        Ok(())
    })();
    match result {
        Ok(()) => {
            if let Err(e) = c.execute_batch("COMMIT") {
                let _ = c.execute_batch("ROLLBACK");
                return Err(e.into());
            }
            Ok(())
        }
        Err(e) => {
            let _ = c.execute_batch("ROLLBACK");
            Err(e)
        }
    }
}

/// Encode an L2-normalized vector as 768 little-endian float16 values (1536 bytes).
pub fn encode_f16(v: &[f32]) -> Vec<u8> {
    assert_eq!(v.len(), DIM);
    let mut out = Vec::with_capacity(DIM * 2);
    for &x in v {
        out.extend_from_slice(&f16::from_f32(x).to_le_bytes());
    }
    out
}

/// Decode a `segments.vec` blob. Returns `None` for blobs of the wrong size (Swift skips them too).
pub fn decode_f16(blob: &[u8]) -> Option<Vec<f16>> {
    if blob.len() != DIM * 2 {
        return None;
    }
    Some(blob.as_chunks::<2>().0.iter().map(|b| f16::from_le_bytes(*b)).collect())
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ScanMode {
    /// One accumulator, one thread. The shape of the Swift loop.
    SerialNaive,
    /// Eight accumulators so the compiler can vectorize, one thread.
    SerialLanes,
    /// Same inner loop split across all cores.
    ParallelLanes,
    /// Runtime-dispatched SIMD kernel (AVX-512F / AVX2+FMA+F16C / NEON, else lanes), split across all cores
    /// in blocks of `scan::BLOCK_ROWS` rows. What the searcher uses.
    Simd,
}

/// All vectors in memory, row-major float16, with the segment id of each row.
#[derive(Default)]
pub struct VectorIndex {
    pub ids: Vec<i64>,
    pub data: Vec<f16>,
}

impl VectorIndex {
    pub fn len(&self) -> usize {
        self.ids.len()
    }
    pub fn is_empty(&self) -> bool {
        self.ids.is_empty()
    }

    pub fn push(&mut self, id: i64, v: &[f32]) {
        assert_eq!(v.len(), DIM);
        self.ids.push(id);
        self.data.extend(v.iter().map(|&x| f16::from_f32(x)));
    }

    pub fn load(conn: &Connection) -> Result<Self> {
        let mut idx = VectorIndex::default();
        let mut st = conn.prepare("SELECT id, vec FROM segments WHERE vec IS NOT NULL")?;
        let mut rows = st.query([])?;
        while let Some(r) = rows.next()? {
            let id: i64 = r.get(0)?;
            let blob: Vec<u8> = r.get(1)?;
            if let Some(v) = decode_f16(&blob) {
                idx.ids.push(id);
                idx.data.extend(v);
            }
        }
        Ok(idx)
    }

    /// Dot product of the query with every row.
    pub fn scores(&self, q: &[f32], mode: ScanMode) -> Vec<f32> {
        assert_eq!(q.len(), DIM);
        match mode {
            ScanMode::SerialNaive => self.data.as_chunks::<DIM>().0.iter().map(|row| scan::dot_naive(q, row)).collect(),
            ScanMode::SerialLanes => self.data.as_chunks::<DIM>().0.iter().map(|row| scan::dot_lanes(q, row)).collect(),
            ScanMode::ParallelLanes => self.data.par_chunks_exact(DIM).map(|row| scan::dot_lanes(q, row)).collect(),
            ScanMode::Simd => {
                let mut out = vec![0f32; self.len()];
                scan::dot_all_parallel(scan::best_kernel(), q, &self.data, &mut out);
                out
            }
        }
    }
}

/// Indices and scores of the `k` highest scores, best first.
pub fn top_k(scores: &[f32], k: usize) -> Vec<(usize, f32)> {
    let k = k.min(scores.len());
    if k == 0 {
        return Vec::new();
    }
    let mut idx: Vec<u32> = (0..scores.len() as u32).collect();
    let cmp = |a: &u32, b: &u32| scores[*b as usize].total_cmp(&scores[*a as usize]).then(a.cmp(b));
    if k < idx.len() {
        idx.select_nth_unstable_by(k - 1, cmp);
        idx.truncate(k);
    }
    idx.sort_unstable_by(cmp);
    idx.into_iter().map(|i| (i as usize, scores[i as usize])).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn unit(seed: u32) -> Vec<f32> {
        let mut s = seed.wrapping_mul(2654435761).wrapping_add(1);
        let mut v: Vec<f32> = (0..DIM)
            .map(|_| {
                s ^= s << 13;
                s ^= s >> 17;
                s ^= s << 5;
                (s as f32 / u32::MAX as f32) - 0.5
            })
            .collect();
        let n = v.iter().map(|x| x * x).sum::<f32>().sqrt();
        v.iter_mut().for_each(|x| *x /= n);
        v
    }

    #[test]
    fn schema_is_v5_and_reopen_is_ok() {
        let c = open_in_memory().unwrap();
        let v: i64 = c.query_row("PRAGMA user_version", [], |r| r.get(0)).unwrap();
        assert_eq!(v, 5);
        init(&c).unwrap();
    }

    #[test]
    fn fts_keeps_bengali_and_arabic_words_whole() {
        let c = open_in_memory().unwrap();
        c.execute("INSERT INTO fts(rowid,name,body,original) VALUES (1,'','বাংলা ভাষা',NULL)", []).unwrap();
        c.execute("INSERT INTO fts(rowid,name,body,original) VALUES (2,'','كتاب جميل',NULL)", []).unwrap();
        let hit = |q: &str| -> Vec<i64> {
            let mut st = c.prepare("SELECT rowid FROM fts WHERE fts MATCH ?1 ORDER BY rowid").unwrap();
            st.query_map([q], |r| r.get(0)).unwrap().map(|r| r.unwrap()).collect()
        };
        assert_eq!(hit("\"বাংলা\""), vec![1]);
        assert_eq!(hit("\"ভাষা\""), vec![1]);
        assert_eq!(hit("\"كتاب\""), vec![2]);
    }

    /// A fresh, empty folder for one test's index; removed on drop.
    struct Dir(std::path::PathBuf);
    impl Dir {
        fn new(tag: &str) -> Self {
            let nanos = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0);
            let dir = std::env::temp_dir().join(format!("dtfu-init-{tag}-{}-{nanos}", std::process::id()));
            std::fs::create_dir_all(&dir).unwrap();
            Self(dir)
        }
    }
    impl Drop for Dir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn second_open_of_a_created_fresh_index_is_ok() {
        let dir = Dir::new("seq");
        let path = dir.0.join("index.sqlite");
        drop(open(&path).unwrap());
        // The first open created the tables; a second open must see v5 and not run the DDL again.
        let c = open(&path).unwrap();
        let v: i64 = c.query_row("PRAGMA user_version", [], |r| r.get(0)).unwrap();
        assert_eq!(v, SCHEMA_VERSION);
    }

    #[test]
    fn concurrent_opens_of_a_fresh_index_both_succeed() {
        // Both threads start from a file with version 0; the loser must find the schema, not create it again.
        // Repeated, because the race is timing-dependent.
        for round in 0..20 {
            let dir = Dir::new(&format!("race{round}"));
            let path = dir.0.join("index.sqlite");
            let barrier = std::sync::Arc::new(std::sync::Barrier::new(2));
            let handles: Vec<_> = (0..2)
                .map(|_| {
                    let (path, barrier) = (path.clone(), barrier.clone());
                    std::thread::spawn(move || {
                        barrier.wait();
                        open(&path).map(|c| c.query_row("PRAGMA user_version", [], |r| r.get::<_, i64>(0)).unwrap())
                    })
                })
                .collect();
            for h in handles {
                assert_eq!(h.join().unwrap().unwrap(), SCHEMA_VERSION);
            }
        }
    }

    #[test]
    fn f16_codec_round_trips() {
        let v = unit(7);
        let blob = encode_f16(&v);
        assert_eq!(blob.len(), 1536);
        let back = decode_f16(&blob).unwrap();
        for (a, b) in v.iter().zip(&back) {
            assert!((a - b.to_f32()).abs() < 1e-3);
        }
        assert!(decode_f16(&blob[..100]).is_none());
    }

    #[test]
    fn scan_modes_agree_and_rank_self_first() {
        let mut idx = VectorIndex::default();
        for i in 0..500 {
            idx.push(i, &unit(i as u32));
        }
        let q = unit(123);
        let a = idx.scores(&q, ScanMode::SerialNaive);
        let b = idx.scores(&q, ScanMode::SerialLanes);
        let c = idx.scores(&q, ScanMode::ParallelLanes);
        for i in 0..a.len() {
            assert!((a[i] - b[i]).abs() < 1e-4 && (a[i] - c[i]).abs() < 1e-4);
        }
        let best = top_k(&a, 3);
        assert_eq!(idx.ids[best[0].0], 123);
        assert!(best[0].1 > 0.99);
        assert!(best[0].1 >= best[1].1 && best[1].1 >= best[2].1);
    }

    #[test]
    fn simd_scan_ranks_target_across_blocks() {
        // Swift float16VectorsScoreAcrossBlocks: 2 * blockRows + 300 rows, target at rows - 5.
        let rows = 2 * 1024 + 300;
        let mut idx = VectorIndex::default();
        for i in 0..rows {
            idx.push(i as i64, &unit(i as u32));
        }
        let target = rows - 5;
        let q = unit(target as u32);
        let sc = idx.scores(&q, ScanMode::Simd);
        assert_eq!(sc.len(), rows);
        let best = top_k(&sc, 3);
        assert_eq!(idx.ids[best[0].0], target as i64);
        assert!((best[0].1 - 1.0).abs() < 0.002, "cosine {}", best[0].1);
        let naive = idx.scores(&q, ScanMode::SerialNaive);
        for i in 0..rows {
            assert!((sc[i] - naive[i]).abs() < 1e-4, "row {i}");
        }
    }

    #[test]
    fn top_k_breaks_ties_by_lower_index() {
        let scores = [1.0f32, 2.0, 2.0, 0.5, 2.0, 2.0];
        assert_eq!(top_k(&scores, 3), vec![(1, 2.0), (2, 2.0), (4, 2.0)]);
        assert_eq!(top_k(&[3.0f32; 5], 2), vec![(0, 3.0), (1, 3.0)]);
        assert!(top_k(&scores, 0).is_empty());
    }
}
