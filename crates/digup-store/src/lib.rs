//! SQLite storage (schema v5, compatible with DigUp's Swift app) and the vector scan.

use anyhow::{bail, Result};
use digup_core::{DIM, SCHEMA_VERSION};
use half::f16;
use rayon::prelude::*;
use rusqlite::Connection;
use std::path::Path;
use std::time::Duration;

const DDL: &str = r#"
BEGIN IMMEDIATE;
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
COMMIT;
"#;

pub fn open(path: &Path) -> Result<Connection> {
    let c = Connection::open(path)?;
    configure(&c)?;
    init(&c)?;
    Ok(c)
}

pub fn open_in_memory() -> Result<Connection> {
    let c = Connection::open_in_memory()?;
    configure(&c)?;
    init(&c)?;
    Ok(c)
}

fn configure(c: &Connection) -> Result<()> {
    c.busy_timeout(Duration::from_millis(5000))?;
    c.execute_batch("PRAGMA journal_mode = WAL; PRAGMA synchronous = NORMAL; PRAGMA foreign_keys = ON;")?;
    Ok(())
}

fn init(c: &Connection) -> Result<()> {
    let v: i64 = c.query_row("PRAGMA user_version", [], |r| r.get(0))?;
    match v {
        0 => c.execute_batch(DDL)?,
        SCHEMA_VERSION => {}
        v if v > SCHEMA_VERSION => bail!("index schema {v} is newer than this build ({SCHEMA_VERSION})"),
        v => bail!("index schema {v} needs a migration this build does not have yet"),
    }
    Ok(())
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
    Some(blob.chunks_exact(2).map(|b| f16::from_le_bytes([b[0], b[1]])).collect())
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ScanMode {
    /// One accumulator, one thread. The shape of the Swift loop.
    SerialNaive,
    /// Eight accumulators so the compiler can vectorize, one thread.
    SerialLanes,
    /// Same inner loop split across all cores.
    ParallelLanes,
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
            ScanMode::SerialNaive => self.data.chunks_exact(DIM).map(|row| dot_naive(q, row)).collect(),
            ScanMode::SerialLanes => self.data.chunks_exact(DIM).map(|row| dot_lanes(q, row)).collect(),
            ScanMode::ParallelLanes => self.data.par_chunks_exact(DIM).map(|row| dot_lanes(q, row)).collect(),
        }
    }
}

fn dot_naive(q: &[f32], row: &[f16]) -> f32 {
    q.iter().zip(row).map(|(a, b)| a * b.to_f32()).sum()
}

fn dot_lanes(q: &[f32], row: &[f16]) -> f32 {
    let mut acc = [0f32; 8];
    for (qc, rc) in q.chunks_exact(8).zip(row.chunks_exact(8)) {
        for i in 0..8 {
            acc[i] += qc[i] * rc[i].to_f32();
        }
    }
    acc.iter().sum()
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
}
