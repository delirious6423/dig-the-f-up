//! The keyword fast path (`Searcher::search`) must return exactly what the original SQL returns: same hits, same
//! order (bm25 with rowid tie-break), same segments, same scores (1e-9), same keyword ranks.
//! The reference is the same searcher with `with_legacy_keyword(true)`, which runs the original queries verbatim
//! (plus the rowid tie-break). The databases are seeded or built from fixed text, so the tests are deterministic.
//! Run with `--release` (building 20k vectors is slow in debug).

use digup_core::{FileKind, Modality, SegmentKind, DIM};
use digup_search::{fts_query, SearchHit, SearchOptions, Searcher};
use digup_store::encode_f16;
use digup_store::index::{IndexStore, NewFile, NewSegment};
use rusqlite::{params, Connection};
use std::path::PathBuf;

const SEGMENTS: usize = 20_000;
/// Small vocabulary, so words repeat a lot: many FTS rows share a bm25 score and the tie-break matters.
const VOCAB: usize = 300;

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
    /// Skewed index: low indices are common words.
    fn skewed(&mut self) -> usize {
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

/// A fresh, empty directory per test (tests run in parallel, so the tag keeps them apart).
fn fresh_dir(tag: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("digup-fts-eq-{}-{tag}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    d
}

/// Same layout as `IndexStore::insert_file` (schema v5): `fts.rowid == segments.id`. Some text segments get
/// `original` text, which the snippet path reads, and some segments have no keyword row.
fn build(path: &std::path::Path, vocab: &[String]) -> Connection {
    let mut conn = digup_store::open(path).unwrap();
    let mut rng = Rng(0x9E37_79B9_7F4A_7C15);
    let tx = conn.transaction().unwrap();
    {
        let mut file_stmt = tx
            .prepare(
                "INSERT INTO files(path, kind, size, mtime, inode, device, state) VALUES(?1, ?2, ?3, ?4, ?5, ?6, 'done')",
            )
            .unwrap();
        let mut seg_stmt = tx
            .prepare("INSERT INTO segments(file, kind, modality, loc, loc_end, excerpt, vec) VALUES(?1, ?2, ?3, ?4, NULL, NULL, ?5)")
            .unwrap();
        let mut fts_stmt = tx
            .prepare("INSERT INTO fts(rowid, name, body, original) VALUES(?1, '', ?2, ?3)")
            .unwrap();
        let mut seg_total = 0usize;
        let mut files = 0usize;
        while seg_total < SEGMENTS {
            let u = rng.unit();
            let (file_kind, seg_kind, modality) = if u < 0.7 {
                ("doc", "chunk", "text")
            } else if u < 0.9 {
                ("image", "image", "image")
            } else {
                ("audio", "audio", "audio")
            };
            files += 1;
            let path_text = format!("/fts/{file_kind}/{files:06}.dat");
            let size = 10_000 + rng.below(5_000_000) as i64;
            file_stmt
                .execute(params![path_text, file_kind, size, 1.7e9 + files as f64, files as i64, 1i64])
                .unwrap();
            let file_id = tx.last_insert_rowid();
            let segs = 1 + (rng.next_u64() & 1) as usize;
            for s in 0..segs {
                if seg_total >= SEGMENTS {
                    break;
                }
                let blob = encode_f16(&unit_vector(&mut rng));
                let loc = (s + 1) as f64;
                seg_stmt.execute(params![file_id, seg_kind, modality, loc, blob]).unwrap();
                let seg_id = tx.last_insert_rowid();
                if file_kind == "doc" {
                    let words = 15 + rng.below(25);
                    let body: Vec<&str> = (0..words).map(|_| vocab[rng.skewed()].as_str()).collect();
                    let body = body.join(" ");
                    // Every 7th row keeps the original text (with a capital), like a normalized document would.
                    let original: Option<String> = (seg_id % 7 == 0).then(|| body.to_uppercase());
                    fts_stmt.execute(params![seg_id, body, original]).unwrap();
                }
                seg_total += 1;
            }
        }
    }
    tx.commit().unwrap();
    conn.execute_batch("PRAGMA wal_checkpoint(TRUNCATE);").unwrap();
    conn
}

/// Queries: single common and rare words, multi-word, prefix-typed, kind-filtered, empty text, and unknown words.
fn queries(vocab: &[String]) -> Vec<(String, bool, Option<FileKind>)> {
    let mut q: Vec<(String, bool, Option<FileKind>)> = Vec::new();
    for i in [0usize, 1, 2, 3, 5, 8, 13, 21, 34, 55, 89, 144, 233, 299] {
        q.push((vocab[i].clone(), false, None));
    }
    for (a, b) in [(0usize, 1usize), (2, 7), (4, 40), (10, 150), (0, 299)] {
        q.push((format!("{} {}", vocab[a], vocab[b]), false, None));
    }
    for i in [0usize, 4, 30, 200] {
        let w = &vocab[i];
        q.push((w[..3].to_string(), true, None));
    }
    for i in [0usize, 6, 60] {
        q.push((vocab[i].clone(), false, Some(FileKind::Doc)));
        q.push((vocab[i].clone(), false, Some(FileKind::Image)));
    }
    q.push(("".into(), false, None));
    q.push(("zzqxwv".into(), false, None));
    q.push((format!("{} zzqxwv", vocab[1]), false, None));
    q.push((vocab[7].to_uppercase(), false, None));
    q.push((format!("{} {} {}", vocab[0], vocab[2], vocab[9]), false, None));
    q
}

fn same(a: &[SearchHit], b: &[SearchHit], label: &str) {
    assert_eq!(a.len(), b.len(), "{label}: hit count differs");
    for (i, (x, y)) in a.iter().zip(b).enumerate() {
        assert_eq!(x.path, y.path, "{label}: path at rank {i}");
        assert_eq!(x.file_id, y.file_id, "{label}: file_id at rank {i}");
        assert_eq!(x.segment_id, y.segment_id, "{label}: segment_id at rank {i}");
        assert_eq!(x.keyword_rank, y.keyword_rank, "{label}: keyword_rank at rank {i}");
        assert_eq!(x.excerpt, y.excerpt, "{label}: excerpt at rank {i}");
        assert!((x.score - y.score).abs() <= 1e-9, "{label}: score at rank {i}: {} vs {}", x.score, y.score);
    }
}

/// Runs one query on the fast and the legacy searcher, asserts they agree, and returns the fast hits.
fn check(conn: &Connection, text: &str, prefix_last: bool, kind: Option<FileKind>, limit: usize) -> Vec<SearchHit> {
    let fast = Searcher::new(conn).unwrap();
    let legacy = Searcher::new(conn).unwrap().with_legacy_keyword(true);
    let mut qrng = Rng(0xD1B5_4A32_D192_ED03);
    let qv = unit_vector(&mut qrng);
    let opts = SearchOptions { kinds: kind.map(|k| [k].into_iter().collect()), limit, prefix_last, ..SearchOptions::default() };
    let label = format!("query {text:?} prefix={prefix_last} kind={kind:?} limit={limit}");
    let a = fast.search(&qv, text, &opts).unwrap();
    let b = legacy.search(&qv, text, &opts).unwrap();
    same(&a, &b, &label);
    a
}

fn keyword_hits(hits: &[SearchHit]) -> usize {
    hits.iter().filter(|h| h.keyword_rank.is_some()).count()
}

#[test]
fn fts_fast_path_matches_legacy_sql() {
    let mut vrng = Rng(0x2545_F491_4F6C_DD1D);
    let vocab = vocabulary(&mut vrng);
    let dir = fresh_dir("main");
    let conn = build(&dir.join("index.sqlite"), &vocab);

    let mut nonempty = 0usize;
    let mut total = 0usize;
    for (text, prefix_last, kind) in queries(&vocab) {
        for limit in [20usize, 200] {
            let hits = check(&conn, &text, prefix_last, kind, limit);
            total += 1;
            if !hits.is_empty() {
                nonempty += 1;
            }
        }
    }
    // The comparison is only meaningful if most queries return hits.
    assert!(nonempty * 2 > total, "too few non-empty results: {nonempty}/{total}");
    drop(conn);
    let _ = std::fs::remove_dir_all(&dir);
}

/// Orphan FTS rows (keyword row whose segment is gone) inside the top 1000 rows. Legacy's inner join drops them
/// before its LIMIT; the fast path must notice and fall back, so results still match.
#[test]
fn orphan_fts_rows_match_legacy() {
    let mut vrng = Rng(0x2545_F491_4F6C_DD1D);
    let vocab = vocabulary(&mut vrng);

    // (a1) Every third matching row of a common word is orphaned: orphans sit inside the top rows, but
    // plenty of real rows remain, so the fast path stays on its own branch.
    let dir = fresh_dir("orphan-third");
    let conn = build(&dir.join("index.sqlite"), &vocab);
    let rowids: Vec<i64> = conn
        .prepare("SELECT rowid FROM fts WHERE fts MATCH ?1 ORDER BY rowid")
        .unwrap()
        .query_map([&vocab[1]], |r| r.get(0))
        .unwrap()
        .collect::<rusqlite::Result<_>>()
        .unwrap();
    assert!(rowids.len() > 1000, "need more than 1000 matching rows, got {}", rowids.len());
    for id in rowids.iter().step_by(3) {
        conn.execute("DELETE FROM segments WHERE id = ?1", [id]).unwrap();
    }
    for limit in [20usize, 200] {
        check(&conn, &vocab[1], false, None, limit);
        check(&conn, &format!("{} {}", vocab[1], vocab[2]), false, None, limit);
    }
    drop(conn);
    let _ = std::fs::remove_dir_all(&dir);

    // (a2) Almost every matching row of a very common word is orphaned, so the top 1000 rows yield fewer than
    // 100 files and the legacy fallback runs. Only 60 real rows remain for the word.
    let dir = fresh_dir("orphan-most");
    let conn = build(&dir.join("index.sqlite"), &vocab);
    let rowids: Vec<i64> = conn
        .prepare("SELECT rowid FROM fts WHERE fts MATCH ?1 ORDER BY rowid")
        .unwrap()
        .query_map([&vocab[0]], |r| r.get(0))
        .unwrap()
        .collect::<rusqlite::Result<_>>()
        .unwrap();
    assert!(rowids.len() > 1000, "need more than 1000 matching rows, got {}", rowids.len());
    for id in rowids.iter().skip(60) {
        conn.execute("DELETE FROM segments WHERE id = ?1", [id]).unwrap();
    }
    for limit in [20usize, 200] {
        check(&conn, &vocab[0], false, None, limit);
    }
    drop(conn);
    let _ = std::fs::remove_dir_all(&dir);
}

fn add_text_file(store: &mut IndexStore, n: usize, path: &str, kind: FileKind, texts: &[&str]) {
    let file = NewFile { path: path.into(), kind, size: 10, mtime: 1.0 + n as f64, inode: n as u64 + 1, device: 1 };
    let segs: Vec<NewSegment> = texts
        .iter()
        .enumerate()
        .map(|(i, t)| NewSegment {
            kind: SegmentKind::Chunk,
            modality: Modality::None,
            loc: Some(i as f64 + 1.0),
            loc_end: None,
            excerpt: None,
            text: Some((*t).into()),
            vector: None,
        })
        .collect();
    store.insert_file(&file, &segs).unwrap();
}

/// Both caps: 200 files, each matching the word once, so the 100-file stop binds (exactly 100 keyword files).
/// Then 40 files with 30 matching segments each (1200 rows, under 100 files), so the 1000-row cap binds.
#[test]
fn file_and_row_caps_match_legacy() {
    let dir = fresh_dir("caps-files");
    let mut store = IndexStore::open(&dir, "index.sqlite").unwrap();
    for i in 0..200 {
        let body = format!("alpha filler{} {}", i, "beta ".repeat(i % 5 + 1));
        add_text_file(&mut store, i, &format!("/caps/many/{i:04}.txt"), FileKind::Doc, &[&body]);
    }
    drop(store);
    let conn = digup_store::open(&dir.join("index.sqlite")).unwrap();
    let hits = check(&conn, "alpha", false, None, 200);
    let ranked = hits.iter().filter_map(|h| h.keyword_rank).max();
    assert_eq!(ranked, Some(100), "the 100-file stop should bind: {ranked:?}");
    drop(conn);
    let _ = std::fs::remove_dir_all(&dir);

    let dir = fresh_dir("caps-rows");
    let mut store = IndexStore::open(&dir, "index.sqlite").unwrap();
    for i in 0..40 {
        let texts: Vec<String> = (0..30).map(|s| format!("alpha gamma{} delta{}", s, (i * 30 + s) % 17)).collect();
        let refs: Vec<&str> = texts.iter().map(|t| t.as_str()).collect();
        add_text_file(&mut store, i, &format!("/caps/dense/{i:04}.txt"), FileKind::Doc, &refs);
    }
    drop(store);
    let conn = digup_store::open(&dir.join("index.sqlite")).unwrap();
    let hits = check(&conn, "alpha", false, None, 200);
    let files: std::collections::HashSet<i64> = hits.iter().filter(|h| h.keyword_rank.is_some()).map(|h| h.file_id).collect();
    assert!(files.len() <= 40 && !files.is_empty(), "dense files: {}", files.len());
    drop(conn);
    let _ = std::fs::remove_dir_all(&dir);
}

/// Non-ASCII multi-term queries (Arabic and Bengali) and inflecting-prefix queries (English).
#[test]
fn non_ascii_and_inflecting_prefix_match_legacy() {
    let dir = fresh_dir("scripts");
    let mut store = IndexStore::open(&dir, "index.sqlite").unwrap();
    let docs: [(&str, &[&str]); 6] = [
        ("/s/ar1.txt", &["كتاب الطالب في المكتبة الكبيرة"]),
        ("/s/ar2.txt", &["المكتبة مفتوحة للكتب والمجلات", "مدرسة الطالب"]),
        ("/s/bn1.txt", &["বাংলা ভাষা আমার গর্ব", "বাংলা সাহিত্য অনেক সমৃদ্ধ"]),
        ("/s/bn2.txt", &["ভাষা শেখা সহজ নয়"]),
        ("/s/en1.txt", &["walking home after he walked slowly", "the walks were long"]),
        ("/s/en2.txt", &["walk the dog", "running and jumping"]),
    ];
    for (i, (path, texts)) in docs.iter().enumerate() {
        add_text_file(&mut store, i, path, FileKind::Doc, texts);
    }
    drop(store);
    let conn = digup_store::open(&dir.join("index.sqlite")).unwrap();

    // The inflecting-prefix queries must actually take the prefix route, or the case proves nothing.
    assert!(fts_query("walk", true).unwrap().contains('*'), "prefix query for an inflecting word");
    let cases: [(&str, bool); 9] = [
        ("كتاب", false),
        ("كتاب المكتبة", false),
        ("الطالب مدرسة", false),
        ("বাংলা ভাষা", false),
        ("বাংলা", true),
        ("ভাষা শেখা", false),
        ("walk", true),
        ("walk", false),
        ("walked walking", false),
    ];
    for (text, prefix) in cases {
        let hits = check(&conn, text, prefix, None, 20);
        assert!(keyword_hits(&hits) > 0, "no keyword hits for {text:?} prefix={prefix}");
    }
    drop(conn);
    let _ = std::fs::remove_dir_all(&dir);
}
