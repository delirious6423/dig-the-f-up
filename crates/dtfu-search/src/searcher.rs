//! Port of Searcher.swift: hybrid search. Meaning (cosine similarity, put on one scale per query as z-scores) is fused
//! with words (FTS5 over the same index). The layout is dense (one f16 matrix, per-row modality and file-slot arrays,
//! the scan parallel in dtfu-store); the results are those of the Swift algorithm.

use std::collections::{HashMap, HashSet};
use std::time::Instant;

use anyhow::Result;
use dtfu_core::{FileKind, Modality, SegmentKind, DIM};
use dtfu_store::{ScanMode, VectorIndex};
use rusqlite::{Connection, OptionalExtension};
use unicode_segmentation::UnicodeSegmentation;

use crate::filters::Filters;
use crate::{code_lookup, text};

/// Scale rows that don't set the scale (Model.swift: `bigPDFPages`, `pdfStep`, `docStep`): the later pages of a long
/// PDF and the later passages of a long document are scored, but leave the averages and the deviation alone.
const BIG_PDF_PAGES: i64 = 50;
const PDF_STEP: i64 = 30;
const DOC_STEP: i64 = 40;

/// `refresh` reloads everything when more than this fraction of the rows is removed or must be re-evaluated.
const REFRESH_FULL_FRACTION: f64 = 0.25;
/// ... and every this many refreshes, as a guard against drift.
const REFRESH_RELOAD_EVERY: u32 = 500;

/// What `Searcher::refresh` did.
#[derive(Debug, Clone, Copy)]
pub struct RefreshStats {
    /// Everything was reloaded (too much changed, or an out-of-order id); the other counts then describe the diff.
    pub full: bool,
    pub removed: usize,
    /// Rows whose blobs were decoded (all rows, for a full reload).
    pub added: usize,
    /// Kept rows of changed files whose scale flag and modality were recomputed.
    pub reevaluated: usize,
    pub elapsed: std::time::Duration,
}

struct NewRow {
    id: i64,
    file: i64,
    data: Vec<half::f16>,
    modality: u8,
    sets: bool,
}

struct Loaded {
    vectors: VectorIndex,
    row_slot: Vec<u32>,
    row_modality: Vec<u8>,
    row_sets_scale: Vec<bool>,
    row_file: Vec<i64>,
    slot_file: Vec<i64>,
    slot_kind: Vec<Option<FileKind>>,
    slot_of: HashMap<i64, u32>,
    file_kind: HashMap<i64, FileKind>,
    file_fp: HashMap<i64, u64>,
    loaded_version: i64,
}

/// Whether a row sets the scale (see `BIG_PDF_PAGES`).
fn sets_scale(file_kind: &str, kind: &str, loc: Option<f64>, pages: Option<i64>) -> bool {
    let place = loc.map_or(0, |l| l as i64);
    let later = if file_kind == "pdf" {
        kind != "chunk" && pages.unwrap_or(0) > BIG_PDF_PAGES && place > PDF_STEP
    } else {
        kind == "chunk" && place > DOC_STEP
    };
    !later
}

fn fingerprint(kind: &str, rest: [rusqlite::types::ValueRef; 6]) -> u64 {
    use rusqlite::types::ValueRef;
    use std::hash::{Hash, Hasher};
    let mut h = std::collections::hash_map::DefaultHasher::new();
    kind.hash(&mut h);
    for v in rest {
        match v {
            ValueRef::Null => 0u8.hash(&mut h),
            ValueRef::Integer(i) => (1u8, i).hash(&mut h),
            ValueRef::Real(f) => (2u8, f.to_bits()).hash(&mut h),
            ValueRef::Text(t) | ValueRef::Blob(t) => (3u8, t).hash(&mut h),
        }
    }
    h.finish()
}

const FILES_SQL: &str = "SELECT id, kind, state, indexed_at, info, path, size, mtime FROM files";

fn row_fingerprint(r: &rusqlite::Row) -> Result<u64> {
    Ok(fingerprint(
        r.get_ref(1)?.as_str()?,
        [r.get_ref(2)?, r.get_ref(3)?, r.get_ref(4)?, r.get_ref(5)?, r.get_ref(6)?, r.get_ref(7)?],
    ))
}

/// The full load: every file's kind and every vector with its derived per-row state.
fn load_all(conn: &Connection) -> Result<Loaded> {
    let loaded_version = data_version(conn)?;
    // One read transaction: files and segments come from the same snapshot.
    let conn = &conn.unchecked_transaction()?;
    let mut file_kind = HashMap::new();
    let mut file_fp = HashMap::new();
    {
        let mut st = conn.prepare(FILES_SQL)?;
        let mut rows = st.query([])?;
        while let Some(r) = rows.next()? {
            let id: i64 = r.get(0)?;
            let kind: String = r.get(1)?;
            if let Some(k) = FileKind::parse(&kind) {
                file_kind.insert(id, k);
            }
            file_fp.insert(id, row_fingerprint(r)?);
        }
    }
    let mut vectors = VectorIndex::default();
    let mut row_slot = Vec::new();
    let mut row_modality = Vec::new();
    let mut row_sets_scale = Vec::new();
    let mut row_file = Vec::new();
    let mut slot_file: Vec<i64> = Vec::new();
    let mut slot_of: HashMap<i64, u32> = HashMap::new();
    {
        let mut st = conn.prepare(
            "SELECT s.id, s.file, s.modality, s.vec, s.kind, s.loc, f.kind, \
             CAST(json_extract(f.info, '$.pages') AS INTEGER) \
             FROM segments s JOIN files f ON f.id = s.file WHERE s.vec IS NOT NULL ORDER BY s.id",
        )?;
        let mut rows = st.query([])?;
        while let Some(r) = rows.next()? {
            let blob: Vec<u8> = r.get(3)?;
            let Some(v) = dtfu_store::decode_f16(&blob) else { continue };
            let id: i64 = r.get(0)?;
            let file: i64 = r.get(1)?;
            let modality: String = r.get(2)?;
            let kind: String = r.get(4)?;
            let loc: Option<f64> = r.get(5)?;
            let file_kind_text: String = r.get(6)?;
            let pages: Option<i64> = r.get(7)?;
            let slot = *slot_of.entry(file).or_insert_with(|| {
                slot_file.push(file);
                (slot_file.len() - 1) as u32
            });
            vectors.ids.push(id);
            vectors.data.extend(v);
            row_slot.push(slot);
            row_file.push(file);
            row_modality.push(modality_index(Modality::parse(&modality).unwrap_or(Modality::None)));
            row_sets_scale.push(sets_scale(&file_kind_text, &kind, loc, pages));
        }
    }
    let slot_kind = slot_file.iter().map(|f| file_kind.get(f).copied()).collect();
    Ok(Loaded {
        vectors,
        row_slot,
        row_modality,
        row_sets_scale,
        row_file,
        slot_file,
        slot_kind,
        slot_of,
        file_kind,
        file_fp,
        loaded_version,
    })
}

/// Minimum vectors in a modality for its own average (`modalityMeans`); smaller groups take the average of all.
const MIN_MODALITY_ROWS: usize = 20;
/// Floor of the standard deviation (`meaning`).
const DEVIATION_FLOOR: f64 = 0.02;
/// Modalities in dense index order (`modality_index`).
const MODALITIES: usize = 4;
/// Keyword candidates: the first 100 files of the FTS result, and FTS rows read.
const MAX_WORD_FILES: usize = 100;
const MAX_FTS_ROWS: usize = 1000;

/// Word weights (`Searcher.wordWeight`, normal and lookup): meaning counts 1 normally, 0.5 for a lookup, and the
/// words count double for a lookup.
pub const DEFAULT_WORD_WEIGHT: f64 = 2.0;
/// Code's words count for less: they're everywhere in code (`Searcher.codeWordWeight`).
pub const CODE_WORD_WEIGHT: f64 = 1.0;

#[derive(Debug, Clone)]
pub struct SearchOptions {
    /// Kinds to search; `None` searches all.
    pub kinds: Option<HashSet<FileKind>>,
    pub limit: usize,
    /// Meaning counts for a file when its best segment stands this many standard deviations above the scale.
    pub z_gate: f64,
    /// How much finding the words counts against meaning; `None` is 2.0, or 1.0 for the code index.
    pub word_weight: Option<f64>,
    /// Whether the last word is a prefix ("being typed").
    pub prefix_last: bool,
    /// Query-time filters (`ext:`, `size:`, `mtime:`). They only restrict which candidates may be returned; the
    /// scores of the rest are the same as without them.
    pub filters: Filters,
}

impl Default for SearchOptions {
    fn default() -> Self {
        Self { kinds: None, limit: 20, z_gate: 2.0, word_weight: None, prefix_last: false, filters: Filters::default() }
    }
}

#[derive(Debug, Clone)]
pub struct SearchHit {
    pub file_id: i64,
    pub path: String,
    pub kind: FileKind,
    /// Fused rank score, for ordering only.
    pub score: f64,
    /// How far the best segment stands out for this query.
    pub z: Option<f64>,
    pub cosine: Option<f64>,
    /// 1-based rank among keyword matches.
    pub keyword_rank: Option<usize>,
    pub segment: SegmentKind,
    /// The segment the hit points at.
    pub segment_id: i64,
    pub loc: Option<f64>,
    pub loc_end: Option<f64>,
    pub excerpt: Option<String>,
    /// Modification time (seconds since 1970).
    pub modified: f64,
    pub size: i64,
    pub info: HashMap<String, String>,
    /// Code: the line to open at (`None` for other indexes, or when no line is known).
    pub focus: Option<f64>,
}

fn data_version(conn: &Connection) -> Result<i64> {
    Ok(conn.query_row("PRAGMA data_version", [], |r| r.get(0))?)
}

/// The searcher's connection: borrowed for a one-shot search, owned when the searcher is cached across queries
/// (an owned handle makes it `'static`, so the engine can keep it).
enum ConnHandle<'a> {
    Borrowed(&'a Connection),
    Owned(Connection),
}

impl std::ops::Deref for ConnHandle<'_> {
    type Target = Connection;
    fn deref(&self) -> &Connection {
        match self {
            ConnHandle::Borrowed(c) => c,
            ConnHandle::Owned(c) => c,
        }
    }
}

/// A `Searcher` that owns its connection, so it can be kept between queries and moved across threads.
///
/// `Searcher` itself is not `Send`: its borrowed-connection arm holds a `&Connection`, and `Connection` is not
/// `Sync`. This wrapper can only be made by `Searcher::with_owned_connection`, which builds the owned arm, so the
/// borrowed arm never exists inside it.
pub struct OwnedSearcher(Searcher<'static>);

// SAFETY: the only field is a `Searcher<'static>` built with `ConnHandle::Owned` (the constructor is the only way in
// and the field is private). An owned `rusqlite::Connection` is `Send`, and every other field is plain owned data.
unsafe impl Send for OwnedSearcher {}

impl OwnedSearcher {
    /// See `Searcher::refresh`.
    pub fn refresh(&mut self) -> Result<RefreshStats> {
        self.0.refresh()
    }

    #[doc(hidden)]
    pub fn refresh_with_threshold(&mut self, fraction: f64) -> Result<RefreshStats> {
        self.0.refresh_with_threshold(fraction)
    }
}

impl std::ops::Deref for OwnedSearcher {
    type Target = Searcher<'static>;
    fn deref(&self) -> &Searcher<'static> {
        &self.0
    }
}

/// Hybrid searcher over one index connection. Vectors and file kinds are loaded once in `new`.
pub struct Searcher<'a> {
    conn: ConnHandle<'a>,
    /// This is the code index's searcher (`IndexStore::CODE_FILE`): names are looked up as names (`code_lookup`).
    is_code: bool,
    /// One row per vector: `vectors.ids` holds the segment id.
    vectors: VectorIndex,
    row_slot: Vec<u32>,
    row_modality: Vec<u8>,
    row_sets_scale: Vec<bool>,
    /// File id of each row (kept for `refresh`).
    row_file: Vec<i64>,
    /// File id of each slot (a slot is a file that has vectors) and its kind.
    slot_file: Vec<i64>,
    slot_kind: Vec<Option<FileKind>>,
    slot_of: HashMap<i64, u32>,
    file_kind: HashMap<i64, FileKind>,
    /// Per file: hash of what decides its rows' derived state (see `refresh`).
    file_fp: HashMap<i64, u64>,
    refreshes_since_load: u32,
    /// Runs the keyword stage with the original SQL (`keyword_stage_legacy`). For the equivalence test and for
    /// checking the fast path; off in normal use.
    legacy_keyword: bool,
    /// `PRAGMA data_version` read just before the vectors were loaded (see `is_current`).
    loaded_version: i64,
}

/// A keyword match: the file's first FTS row in bm25 order.
struct WordMatch {
    file: i64,
    segment: i64,
    snippet: Option<String>,
}

impl<'a> Searcher<'a> {
    /// Loads every vector of the index and every file's kind (`segments.vec IS NOT NULL`, with the scale flags).
    pub fn new(conn: &'a Connection) -> Result<Self> {
        Self::with_code(conn, false)
    }

    /// The searcher of the code index (`code.sqlite`).
    pub fn new_code(conn: &'a Connection) -> Result<Self> {
        Self::with_code(conn, true)
    }

    /// Keyword stage with the original SQL instead of the fast path. Results must be identical; used by the
    /// equivalence test.
    #[doc(hidden)]
    pub fn with_legacy_keyword(mut self, on: bool) -> Self {
        self.legacy_keyword = on;
        self
    }

    /// Whether this searcher is the code index's.
    pub fn is_code(&self) -> bool {
        self.is_code
    }

    /// `new` for the main index (`is_code` false) or the code index (true).
    pub fn with_code(conn: &'a Connection, is_code: bool) -> Result<Self> {
        Self::build(ConnHandle::Borrowed(conn), is_code)
    }

    /// Like `with_code` but takes the connection, so the searcher can outlive the caller's store and be kept between
    /// queries. Loading the vectors is the expensive part (about 0.09 s at 100k segments, 8-10 s at 1M; docs/BENCHMARKS.md),
    /// so a long-lived caller pays it once and checks `data_version` to learn when the index changed.
    pub fn with_owned_connection(conn: Connection, is_code: bool) -> Result<OwnedSearcher> {
        Ok(OwnedSearcher(Searcher::build(ConnHandle::Owned(conn), is_code)?))
    }

    /// Whether the index is unchanged since this searcher loaded it. SQLite's `data_version` moves whenever another
    /// connection commits, so a cached searcher checks this before each query and is rebuilt when it is false. The
    /// version is read before the load, so a commit that lands mid-load makes the next check false rather than being
    /// missed.
    pub fn is_current(&self) -> Result<bool> {
        Ok(data_version(&self.conn)? == self.loaded_version)
    }

    fn build(conn: ConnHandle<'a>, is_code: bool) -> Result<Self> {
        let l = load_all(&conn)?;
        Ok(Self {
            conn,
            is_code,
            vectors: l.vectors,
            row_slot: l.row_slot,
            row_modality: l.row_modality,
            row_sets_scale: l.row_sets_scale,
            row_file: l.row_file,
            slot_file: l.slot_file,
            slot_kind: l.slot_kind,
            slot_of: l.slot_of,
            file_kind: l.file_kind,
            file_fp: l.file_fp,
            refreshes_since_load: 0,
            legacy_keyword: false,
            loaded_version: l.loaded_version,
        })
    }

    fn install(&mut self, l: Loaded) {
        self.vectors = l.vectors;
        self.row_slot = l.row_slot;
        self.row_modality = l.row_modality;
        self.row_sets_scale = l.row_sets_scale;
        self.row_file = l.row_file;
        self.slot_file = l.slot_file;
        self.slot_kind = l.slot_kind;
        self.slot_of = l.slot_of;
        self.file_kind = l.file_kind;
        self.file_fp = l.file_fp;
        self.loaded_version = l.loaded_version;
        self.refreshes_since_load = 0;
    }

    /// Brings the loaded state up to date with the index without reloading what did not change; afterwards the
    /// searcher is indistinguishable from a fresh one (same rows in the same id order, so the same float sums).
    /// Falls back to a full reload when more than a quarter of the rows are removed or must be re-evaluated.
    /// See `refresh_with_threshold` for how the diff works.
    pub fn refresh(&mut self) -> Result<RefreshStats> {
        if self.refreshes_since_load >= REFRESH_RELOAD_EVERY {
            let t = Instant::now();
            let l = load_all(&self.conn)?;
            let rows = l.vectors.len();
            self.install(l);
            return Ok(RefreshStats { full: true, removed: 0, added: rows, reevaluated: 0, elapsed: t.elapsed() });
        }
        let stats = self.refresh_with_threshold(REFRESH_FULL_FRACTION)?;
        self.refreshes_since_load += 1;
        Ok(stats)
    }

    /// `refresh` with the fallback fraction given (the equivalence test lowers it to reach the incremental path on
    /// small indexes).
    ///
    /// Reads in one read transaction (after `data_version`, as in a full load): every file's fingerprint (kind,
    /// state, `indexed_at`, `info`) and the files that have vectors past the last loaded id (new rows get ids above
    /// every existing row). A file is dirty when its fingerprint changed, it is new or gone, or it has such rows.
    /// Only dirty files are looked at further: their current rows are merged by id with their loaded rows (rows
    /// gone are dropped, new ones decoded, the rest keep their vectors but get modality and scale flag recomputed
    /// from `info.pages`, loc and kind). Scanning the whole segments table instead would read every vector's page
    /// (45 ms at 100k rows), which is what makes a full load slow.
    ///
    /// FINGERPRINT CONTRACT: this relies on every writer that inserts, deletes or replaces a file's segments or
    /// vectors also changing that file's `files` row (`state`, `indexed_at`, `info`; path, size and mtime are in
    /// the fingerprint too) in the same transaction, or deleting the file (`IndexStore`: `add_segments`, `finish`,
    /// `requeue`, copy, `reset_all`). A writer that does not leaves a refreshed searcher stale until the periodic
    /// full reload. Segment ids are reused after a delete (`INTEGER PRIMARY KEY` without AUTOINCREMENT), so for a rewritten
    /// file the kept rows' vectors are compared with the stored bytes. `refresh` also reloads fully every
    /// `REFRESH_RELOAD_EVERY` refreshes, so a writer that breaks these assumptions cannot leave a searcher stale for long.
    #[doc(hidden)]
    pub fn refresh_with_threshold(&mut self, fraction: f64) -> Result<RefreshStats> {
        let t = Instant::now();
        let trace = trace_on();
        let version = data_version(&self.conn)?;
        let conn: &Connection = &self.conn;
        let tx = conn.unchecked_transaction()?;

        // Files: fingerprints (no allocation per row).
        let mut file_kind: HashMap<i64, FileKind> = HashMap::with_capacity(self.file_kind.len());
        let mut file_fp: HashMap<i64, u64> = HashMap::with_capacity(self.file_fp.len());
        {
            let mut st = tx.prepare(FILES_SQL)?;
            let mut rows = st.query([])?;
            while let Some(r) = rows.next()? {
                let kind = r.get_ref(1)?.as_str()?;
                if let Some(k) = FileKind::parse(kind) {
                    file_kind.insert(r.get(0)?, k);
                }
                file_fp.insert(r.get(0)?, row_fingerprint(r)?);
            }
        }
        trace_stage(trace, "refresh files", t);

        // Dirty files: row rewritten (fingerprint changed), new, gone, or with vectors past the last loaded id (new
        // rows get ids above every row that exists, so those are the only way a file gains rows without a rewrite).
        let mut dirty: HashSet<i64> = HashSet::new();
        let mut rewritten: HashSet<i64> = HashSet::new();
        for (id, fp) in &file_fp {
            if self.file_fp.get(id) != Some(fp) {
                dirty.insert(*id);
                rewritten.insert(*id);
            }
        }
        for id in self.file_fp.keys() {
            if !file_fp.contains_key(id) {
                dirty.insert(*id);
            }
        }
        {
            let last = self.vectors.ids.last().copied().unwrap_or(i64::MIN);
            let mut st = tx.prepare("SELECT DISTINCT file FROM segments WHERE id > ?1 AND vec IS NOT NULL")?;
            let mut rows = st.query([last])?;
            while let Some(r) = rows.next()? {
                dirty.insert(r.get(0)?);
            }
        }
        trace_stage(trace, "refresh dirty", t);

        // Loaded rows of the dirty files, by file, in row order.
        let n = self.vectors.len();
        let mut positions: HashMap<i64, Vec<usize>> = dirty.iter().map(|&f| (f, Vec::new())).collect();
        if !dirty.is_empty() {
            let mut watched = vec![false; self.slot_file.len()];
            for f in &dirty {
                if let Some(&s) = self.slot_of.get(f) {
                    watched[s as usize] = true;
                }
            }
            for (row, &slot) in self.row_slot.iter().enumerate() {
                if watched[slot as usize] {
                    positions.get_mut(&self.row_file[row]).expect("watched file").push(row);
                }
            }
        }

        // Per dirty file: merge its loaded rows with its current rows (both id-ascending).
        let mut keep = vec![true; n];
        let mut removed = 0usize;
        // Kept rows: new modality and scale flag; a rewritten file's kept vectors are compared with the stored ones
        // (segment ids are reused after a delete, so a rewrite may have put other vectors under the same ids) and
        // replaced in place.
        let mut reeval: Vec<(usize, u8, bool, Option<Vec<half::f16>>)> = Vec::new();
        let mut new_rows: Vec<NewRow> = Vec::new();
        {
            let mut st = tx.prepare(
                "SELECT s.id, s.modality, s.kind, s.loc, f.kind, CAST(json_extract(f.info, '$.pages') AS INTEGER), s.vec \
                 FROM segments s JOIN files f ON f.id = s.file WHERE s.file = ?1 AND s.vec IS NOT NULL ORDER BY s.id",
            )?;
            for &file in &dirty {
                let loaded = &positions[&file];
                let check = rewritten.contains(&file);
                let mut next = 0usize; // into `loaded`
                let mut rows = st.query([file])?;
                while let Some(r) = rows.next()? {
                    let id: i64 = r.get(0)?;
                    while next < loaded.len() && self.vectors.ids[loaded[next]] < id {
                        keep[loaded[next]] = false;
                        removed += 1;
                        next += 1;
                    }
                    let modality: String = r.get(1)?;
                    let kind: String = r.get(2)?;
                    let sets = sets_scale(r.get_ref(4)?.as_str()?, &kind, r.get(3)?, r.get(5)?);
                    let m = modality_index(Modality::parse(&modality).unwrap_or(Modality::None));
                    if next < loaded.len() && self.vectors.ids[loaded[next]] == id {
                        let pos = loaded[next];
                        next += 1;
                        let mut replacement = None;
                        if check {
                            let at = pos * DIM;
                            match dtfu_store::decode_f16(r.get_ref(6)?.as_blob()?) {
                                Some(v) => {
                                    if v.iter().zip(&self.vectors.data[at..at + DIM]).any(|(a, b)| a.to_bits() != b.to_bits()) {
                                        replacement = Some(v);
                                    }
                                }
                                None => {
                                    // A fresh load skips an undecodable vector.
                                    keep[pos] = false;
                                    removed += 1;
                                    continue;
                                }
                            }
                        }
                        reeval.push((pos, m, sets, replacement));
                    } else if let Some(v) = dtfu_store::decode_f16(r.get_ref(6)?.as_blob()?) {
                        new_rows.push(NewRow { id, file, data: v, modality: m, sets });
                    }
                }
                for &pos in &loaded[next..] {
                    keep[pos] = false;
                    removed += 1;
                }
            }
        }
        new_rows.sort_by_key(|r| r.id);
        trace_stage(trace, "refresh reeval", t);
        let reevaluated = reeval.len();
        let stats = |full: bool, added: usize, t: Instant| RefreshStats {
            full,
            removed,
            added,
            reevaluated,
            elapsed: t.elapsed(),
        };
        // Too much changed, or a new row sorts before a kept one (it can't be appended): a full load is cheaper or
        // the only way to the same order.
        let last_kept = (0..n).rev().find(|&r| keep[r]).map(|r| self.vectors.ids[r]);
        let out_of_order = match (new_rows.first(), last_kept) {
            (Some(a), Some(k)) => a.id < k,
            _ => false,
        };
        if out_of_order || (removed + reevaluated) as f64 > fraction * n as f64 {
            drop(tx);
            let l = load_all(&self.conn)?;
            let rows = l.vectors.len();
            self.install(l);
            return Ok(stats(true, rows, t));
        }
        drop(tx);

        trace_stage(trace, "refresh new rows", t);
        // Apply: re-evaluate in place, compact (order preserved), append.
        for (pos, m, sets, replacement) in reeval {
            self.row_modality[pos] = m;
            self.row_sets_scale[pos] = sets;
            if let Some(v) = replacement {
                self.vectors.data[pos * DIM..(pos + 1) * DIM].copy_from_slice(&v);
            }
        }
        if removed > 0 {
            let mut w = 0usize;
            for (r, &kept) in keep.iter().enumerate() {
                if !kept {
                    continue;
                }
                if w != r {
                    self.vectors.data.copy_within(r * DIM..(r + 1) * DIM, w * DIM);
                    self.vectors.ids[w] = self.vectors.ids[r];
                    self.row_file[w] = self.row_file[r];
                    self.row_modality[w] = self.row_modality[r];
                    self.row_sets_scale[w] = self.row_sets_scale[r];
                }
                w += 1;
            }
            self.vectors.ids.truncate(w);
            self.vectors.data.truncate(w * DIM);
            self.row_file.truncate(w);
            self.row_modality.truncate(w);
            self.row_sets_scale.truncate(w);
        }
        let appended = new_rows.len();
        for r in new_rows {
            self.vectors.ids.push(r.id);
            self.vectors.data.extend(r.data);
            self.row_file.push(r.file);
            self.row_modality.push(r.modality);
            self.row_sets_scale.push(r.sets);
        }
        trace_stage(trace, "refresh apply", t);
        // Slots exactly as a fresh load numbers them (first appearance in row order).
        if removed > 0 || appended > 0 {
            self.slot_file.clear();
            self.slot_of.clear();
            self.row_slot.clear();
            for &file in &self.row_file {
                let slot = *self.slot_of.entry(file).or_insert_with(|| {
                    self.slot_file.push(file);
                    (self.slot_file.len() - 1) as u32
                });
                self.row_slot.push(slot);
            }
        }
        trace_stage(trace, "refresh slots", t);
        self.file_kind = file_kind;
        self.slot_kind = self.slot_file.iter().map(|f| self.file_kind.get(f).copied()).collect();
        self.file_fp = file_fp;
        self.loaded_version = version;
        Ok(stats(false, appended, t))
    }

    /// Number of stored vectors.
    pub fn vector_count(&self) -> usize {
        self.vectors.len()
    }

    /// The indexed text of a segment (`coalesce(original, body)`), as it's written.
    pub fn segment_text(&self, segment_id: i64) -> Result<Option<String>> {
        let text = self
            .conn
            .query_row("SELECT coalesce(original, body) FROM fts WHERE rowid = ?1", [segment_id], |r| r.get(0))
            .optional()?;
        Ok(text)
    }

    /// Searches with a query embedding (`query_vec`, L2-normalized, `DIM` values; empty or of another length means no
    /// meaning search, words only) and the query text (its words).
    pub fn search(&self, query_vec: &[f32], query_text: &str, opts: &SearchOptions) -> Result<Vec<SearchHit>> {
        let trace = trace_on();
        let t = Instant::now();
        let slots = self.slot_kind.len();
        let allowed_slot: Vec<bool> = self
            .slot_kind
            .iter()
            .map(|k| match &opts.kinds {
                None => true,
                Some(set) => k.is_some_and(|k| set.contains(&k)),
            })
            .collect();

        // Meaning: score every vector, put the scores on the query's scale, keep each file's best segment.
        let mut best_z = vec![f64::NEG_INFINITY; slots];
        let mut best_cos = vec![0f32; slots];
        let mut best_row = vec![usize::MAX; slots];
        if query_vec.len() == DIM && !self.vectors.is_empty() {
            let scores = self.vectors.scores(query_vec, ScanMode::Simd);
            let z = self.meaning(&scores);
            for row in 0..scores.len() {
                let slot = self.row_slot[row] as usize;
                if allowed_slot[slot] && z[row] > best_z[slot] {
                    best_z[slot] = z[row];
                    best_cos[slot] = scores[row];
                    best_row[slot] = row;
                }
            }
        }
        trace_stage(trace, "meaning (scan + scale + best per file)", t);

        // Words: FTS5 over names, OCR, page and document text. One pattern per word, OR-ed.
        let terms = terms(query_text);
        let term_refs: Vec<&str> = terms.iter().map(String::as_str).collect();
        let identifier = if self.is_code { code_lookup::identifier(query_text) } else { None };
        let patterns = match &identifier {
            Some(words) => vec![code_lookup::phrase(words)],
            None => patterns(&terms, opts.prefix_last),
        };
        let word_weight = opts.word_weight.unwrap_or(if self.is_code { CODE_WORD_WEIGHT } else { DEFAULT_WORD_WEIGHT });
        let (words, coverage) = if patterns.is_empty() {
            (Vec::new(), HashMap::new())
        } else {
            self.keyword_stage(&patterns, &term_refs, opts, trace)?
        };
        let word_index: HashMap<i64, usize> = words.iter().enumerate().map(|(i, w)| (w.file, i)).collect();

        // A file qualifies by standing out in meaning, or by containing at least half of the query's words.
        let lookup = is_lookup(&terms) || identifier.is_some();
        let (meaning_weight, words_weight) = if lookup { (0.5, word_weight * 2.0) } else { (1.0, word_weight) };
        let hinted = kind_hints(query_text);
        let mut candidates: HashSet<i64> = HashSet::new();
        for (slot, &file) in self.slot_file.iter().enumerate() {
            if best_row[slot] != usize::MAX && best_z[slot] >= opts.z_gate {
                candidates.insert(file);
            }
        }
        for (&file, &share) in &coverage {
            if share >= 0.5 {
                candidates.insert(file);
            }
        }
        // Filters restrict the candidates before scoring and the limit; the scale above is over the whole index.
        if !opts.filters.is_empty() {
            self.retain_matching(&mut candidates, &opts.filters)?;
        }
        let mut scored: Vec<(i64, f64)> = candidates
            .into_iter()
            .map(|file| {
                let z = self
                    .slot_of
                    .get(&file)
                    .filter(|&&s| best_row[s as usize] != usize::MAX)
                    .map_or(0.0, |&s| best_z[s as usize]);
                let share = coverage.get(&file).copied().unwrap_or(0.0);
                let mut score = meaning_weight * z.max(0.0) + words_weight * share.powf(1.5);
                if self.file_kind.get(&file).is_some_and(|k| hinted.contains(k)) {
                    score += 1.0;
                }
                (file, score)
            })
            .collect();
        scored.sort_by(|a, b| b.1.total_cmp(&a.1).then(a.0.cmp(&b.0)));
        trace_stage(trace, "candidates + score + sort", t);
        let t = Instant::now();

        let mut hits = Vec::new();
        for (file, score) in scored.into_iter().take(opts.limit) {
            let semantic = self
                .slot_of
                .get(&file)
                .map(|&s| s as usize)
                .filter(|&s| best_row[s] != usize::MAX)
                .map(|s| (best_z[s], best_cos[s] as f64, best_row[s]));
            let keyword = word_index.get(&file).map(|&i| (i + 1, &words[i]));
            // The best meaning segment if it stands out; else the segment the words matched.
            let use_meaning = semantic.is_some_and(|(z, _, _)| z >= opts.z_gate || keyword.is_none());
            let rank = hits.len();
            let segment_id = if use_meaning {
                match semantic {
                    Some((_, _, row)) => self.vectors.ids[row],
                    None => continue,
                }
            } else {
                match keyword {
                    Some((_, w)) => w.segment,
                    None => continue,
                }
            };

            // Code: where a name is defined is looked for in the first 10 results only (a name used all over a big
            // repo is in hundreds of stretches of every file that has it).
            let mut focus = None;
            let mut segment_id = segment_id;
            if self.is_code {
                let words_here = keyword.is_some_and(|(_, w)| w.segment == segment_id) || identifier.is_some();
                (segment_id, focus) = self.code_place(
                    file,
                    segment_id,
                    if rank < 10 { identifier.as_deref() } else { None },
                    query_text,
                    &terms,
                    words_here,
                )?;
            }

            let file_row = self
                .conn
                .prepare_cached("SELECT path, kind, mtime, size, info FROM files WHERE id = ?1")?
                .query_row([file], |r| {
                    Ok((
                        r.get::<_, String>(0)?,
                        r.get::<_, String>(1)?,
                        r.get::<_, f64>(2)?,
                        r.get::<_, i64>(3)?,
                        r.get::<_, Option<String>>(4)?,
                    ))
                })
                .optional()?;
            let (path, kind, modified, size, info) = match file_row {
                Some((p, k, m, s, i)) => (p, FileKind::parse(&k).unwrap_or(FileKind::Doc), m, s, parse_info(i.as_deref())),
                None => (String::new(), FileKind::Doc, 0.0, 0, HashMap::new()),
            };

            let segment_row = self
                .conn
                .prepare_cached("SELECT kind, loc, loc_end, excerpt FROM segments WHERE id = ?1")?
                .query_row([segment_id], |r| {
                    Ok((
                        r.get::<_, String>(0)?,
                        r.get::<_, Option<f64>>(1)?,
                        r.get::<_, Option<f64>>(2)?,
                        r.get::<_, Option<String>>(3)?,
                    ))
                })
                .optional()?;
            let (segment, loc, loc_end, stored_excerpt) = match segment_row {
                Some((k, l, le, e)) => (SegmentKind::parse(&k).unwrap_or(SegmentKind::Name), l, le, e),
                None => (SegmentKind::Name, None, None, None),
            };

            // The matched words are quoted only when they come from the segment we point at. A segment with no place
            // and no text (a screenshot's image vector) can borrow them from its file's OCR text.
            let mut snippet = keyword
                .filter(|(_, w)| w.segment == segment_id)
                .and_then(|(_, w)| w.snippet.as_deref())
                .filter(|s| s.contains('«'))
                .map(|s| excerpt(s, 400));
            if let Some((_, w)) = keyword {
                if snippet.is_none() && stored_excerpt.is_none() && loc.is_none() {
                    if let Some(s) = w.snippet.as_deref().filter(|s| s.contains('«')) {
                        let matched_kind: Option<String> = self
                            .conn
                            .query_row("SELECT kind FROM segments WHERE id = ?1", [w.segment], |r| r.get(0))
                            .optional()?;
                        if matched_kind.as_deref() != Some(SegmentKind::Name.as_str()) {
                            snippet = Some(excerpt(s, 400));
                        }
                    }
                }
            }

            hits.push(SearchHit {
                file_id: file,
                path,
                kind,
                score,
                z: semantic.map(|(z, _, _)| z),
                cosine: semantic.map(|(_, c, _)| c),
                keyword_rank: keyword.map(|(rank, _)| rank),
                segment,
                segment_id,
                loc,
                loc_end,
                excerpt: snippet.or(stored_excerpt),
                modified,
                size,
                info,
                focus,
            });
        }
        trace_stage(trace, "hits (per-hit file, segment, code place)", t);
        Ok(hits)
    }

    /// Code: the stretch to point at, and the line in it to open at. A name looked up points at the stretch that
    /// defines it, when one does (not the ones that call it). Otherwise the line is the first with the query's words,
    /// when they matched in this stretch; a match by meaning alone opens at the stretch's first line.
    fn code_place(
        &self,
        file: i64,
        segment: i64,
        identifier: Option<&[String]>,
        query: &str,
        terms: &[String],
        words_here: bool,
    ) -> Result<(i64, Option<f64>)> {
        let name = query.trim().replace("()", "");
        if let Some(identifier) = identifier {
            let mut best: Option<(i64, i64, i32)> = None;
            let mut st = self.conn.prepare(
                "SELECT s.id, s.loc, coalesce(fts.original, fts.body) FROM fts JOIN segments s ON s.id = fts.rowid \
                 WHERE fts MATCH ?1 AND s.file = ?2 ORDER BY s.loc LIMIT 300",
            )?;
            let mut rows = st.query(rusqlite::params![code_lookup::phrase(identifier), file])?;
            while let Some(r) = rows.next()? {
                if best.is_some_and(|b| b.2 >= 2) {
                    break;
                }
                let Some(text) = r.get::<_, Option<String>>(2)? else { continue };
                let first = r.get::<_, Option<f64>>(1)?.unwrap_or(0.0) as i64;
                if let Some((line, strength)) = code_lookup::definition(&name, &text, first) {
                    if strength > best.map_or(0, |b| b.2) {
                        best = Some((r.get(0)?, line, strength));
                    }
                }
            }
            if let Some((segment, line, _)) = best {
                return Ok((segment, Some(line as f64)));
            }
        }
        if !(words_here || identifier.is_some()) {
            return Ok((segment, None));
        }
        let Some(text) = self.segment_text(segment)? else { return Ok((segment, None)) };
        let first = self
            .conn
            .query_row("SELECT loc FROM segments WHERE id = ?1", [segment], |r| r.get::<_, Option<f64>>(0))
            .optional()?
            .flatten()
            .map_or(1, |l| l as i64);
        let words = if identifier.is_none() { terms.to_vec() } else { vec![name.to_lowercase()] };
        Ok((segment, code_lookup::first_line(&words, &text, first).map(|l| l as f64)))
    }

    /// The keyword stage: the words' FTS rows in bm25 order (rowid breaks ties), one per file, at most
    /// `MAX_WORD_FILES` files, and each candidate file's share of the query's patterns (`coverage`).
    ///
    /// Fast path: bm25 is computed on `fts` alone and only the top `MAX_FTS_ROWS` rows are read, so the segments
    /// table (whose rows carry vectors) is touched for kept rows only; coverage walks only the segments of the
    /// kept files. Falls back to `keyword_stage_legacy` in the one case where the two can differ (an FTS row without
    /// a segment inside the top rows, which the inner join of the legacy query drops before the limit).
    fn keyword_stage(
        &self,
        patterns: &[String],
        term_refs: &[&str],
        opts: &SearchOptions,
        trace: bool,
    ) -> Result<(Vec<WordMatch>, HashMap<i64, f64>)> {
        if self.legacy_keyword {
            return self.keyword_stage_legacy(patterns, term_refs, opts);
        }
        let expr = patterns.join(" OR ");
        let t = Instant::now();
        let ids: Vec<i64> = {
            let mut st = self.conn.prepare_cached(&format!(
                "SELECT rowid FROM fts WHERE fts MATCH ?1 ORDER BY bm25(fts, 3.0, 1.0), rowid LIMIT {MAX_FTS_ROWS}"
            ))?;
            let v = st.query_map([&expr], |r| r.get(0))?.collect::<rusqlite::Result<Vec<i64>>>()?;
            v
        };
        trace_stage(trace, "fts: top rows by bm25 (no join)", t);

        let t = Instant::now();
        let mut words: Vec<WordMatch> = Vec::new();
        let mut seen = HashSet::new();
        let mut orphans = false;
        {
            let mut st = self.conn.prepare_cached(
                "SELECT s.file, snippet(fts, 1, '«', '»', '…', 12), fts.original FROM fts \
                 JOIN segments s ON s.id = fts.rowid WHERE fts MATCH ?1 AND fts.rowid = ?2",
            )?;
            for &id in &ids {
                if words.len() >= MAX_WORD_FILES {
                    break;
                }
                let mut rows = st.query(rusqlite::params![expr, id])?;
                let Some(r) = rows.next()? else {
                    orphans = true;
                    continue;
                };
                let file: i64 = r.get(0)?;
                if !self.allowed_file(file, opts) || !self.passes_filters(file, &opts.filters)? || !seen.insert(file) {
                    continue;
                }
                let fts_snippet: Option<String> = r.get(1)?;
                let original: Option<String> = r.get(2)?;
                // Text that normalizing changed is quoted as it's written.
                let snippet = original.and_then(|o| text::snippet(&o, term_refs, 12)).or(fts_snippet);
                words.push(WordMatch { file, segment: id, snippet });
            }
        }
        trace_stage(trace, "fts: rows joined to segments + snippets", t);
        if orphans && ids.len() == MAX_FTS_ROWS && words.len() < MAX_WORD_FILES {
            return self.keyword_stage_legacy(patterns, term_refs, opts);
        }

        let t = Instant::now();
        let mut coverage: HashMap<i64, f64> = HashMap::new();
        if !words.is_empty() {
            let files = words.iter().map(|w| w.file.to_string()).collect::<Vec<_>>().join(",");
            // Segments of the kept files (a covering index scan), each checked against the pattern by rowid.
            let sql = format!(
                "SELECT DISTINCT s.file FROM segments s CROSS JOIN fts ON fts.rowid = s.id \
                 WHERE s.file IN ({files}) AND fts MATCH ?1"
            );
            let mut st = self.conn.prepare_cached(&sql)?;
            for pattern in patterns {
                let mut rows = st.query([pattern])?;
                while let Some(r) = rows.next()? {
                    let file: i64 = r.get(0)?;
                    *coverage.entry(file).or_insert(0.0) += 1.0 / patterns.len() as f64;
                }
            }
        }
        trace_stage(trace, "fts: coverage of kept files", t);
        Ok((words, coverage))
    }

    /// The original keyword stage (one query over every match, joined to segments; coverage by `IN` over every
    /// match). Kept as the reference for `keyword_stage`.
    fn keyword_stage_legacy(
        &self,
        patterns: &[String],
        term_refs: &[&str],
        opts: &SearchOptions,
    ) -> Result<(Vec<WordMatch>, HashMap<i64, f64>)> {
        let mut words: Vec<WordMatch> = Vec::new();
        let mut coverage: HashMap<i64, f64> = HashMap::new();
        let mut seen = HashSet::new();
        let mut st = self.conn.prepare(&format!(
            "SELECT s.file, s.id, snippet(fts, 1, '«', '»', '…', 12), fts.original FROM fts \
             JOIN segments s ON s.id = fts.rowid WHERE fts MATCH ?1 \
             ORDER BY bm25(fts, 3.0, 1.0), fts.rowid LIMIT {MAX_FTS_ROWS}"
        ))?;
        let mut rows = st.query([patterns.join(" OR ")])?;
        while let Some(r) = rows.next()? {
            let file: i64 = r.get(0)?;
            if words.len() >= MAX_WORD_FILES || !self.allowed_file(file, opts) || !self.passes_filters(file, &opts.filters)? || !seen.insert(file) {
                continue;
            }
            let segment: i64 = r.get(1)?;
            let fts_snippet: Option<String> = r.get(2)?;
            let original: Option<String> = r.get(3)?;
            // Text that normalizing changed is quoted as it's written.
            let snippet = original.and_then(|o| text::snippet(&o, term_refs, 12)).or(fts_snippet);
            words.push(WordMatch { file, segment, snippet });
        }
        if !words.is_empty() {
            let files = words.iter().map(|w| w.file.to_string()).collect::<Vec<_>>().join(",");
            let sql = format!(
                "SELECT DISTINCT s.file FROM fts JOIN segments s ON s.id = fts.rowid \
                 WHERE fts MATCH ?1 AND s.file IN ({files})"
            );
            let mut st = self.conn.prepare(&sql)?;
            for pattern in patterns {
                let mut rows = st.query([pattern])?;
                while let Some(r) = rows.next()? {
                    let file: i64 = r.get(0)?;
                    *coverage.entry(file).or_insert(0.0) += 1.0 / patterns.len() as f64;
                }
            }
        }
        Ok((words, coverage))
    }

    /// Drops the candidates that the filters rule out. Path, size and mtime come from one lookup per 500 files.
    fn retain_matching(&self, candidates: &mut HashSet<i64>, filters: &Filters) -> Result<()> {
        let ids: Vec<i64> = candidates.iter().copied().collect();
        let mut keep = HashSet::with_capacity(ids.len());
        for chunk in ids.chunks(500) {
            let marks = vec!["?"; chunk.len()].join(",");
            let mut st = self.conn.prepare(&format!("SELECT id, path, size, mtime FROM files WHERE id IN ({marks})"))?;
            let mut rows = st.query(rusqlite::params_from_iter(chunk.iter()))?;
            while let Some(r) = rows.next()? {
                let (id, path, size, mtime): (i64, String, i64, f64) = (r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?);
                if filters.matches(&path, size, mtime) {
                    keep.insert(id);
                }
            }
        }
        candidates.retain(|file| keep.contains(file));
        Ok(())
    }

    /// Whether one file passes the filters. The keyword stage calls this before its caps (`MAX_WORD_FILES`,
    /// `MAX_FTS_ROWS`), so a filtered query's keyword matches are chosen among files that pass: otherwise `report ext:pdf`
    /// would lose a pdf ranked below the first hundred files for "report".
    fn passes_filters(&self, file: i64, filters: &Filters) -> Result<bool> {
        if filters.is_empty() {
            return Ok(true);
        }
        let mut st = self.conn.prepare_cached("SELECT path, size, mtime FROM files WHERE id = ?1")?;
        let row = st
            .query_row([file], |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?, r.get::<_, f64>(2)?)))
            .optional()?;
        Ok(row.is_some_and(|(path, size, mtime)| filters.matches(&path, size, mtime)))
    }

    fn allowed_file(&self, file: i64, opts: &SearchOptions) -> bool {
        match &opts.kinds {
            None => true,
            Some(set) => self.file_kind.get(&file).is_some_and(|k| set.contains(k)),
        }
    }

    /// This query's meaning scores (`Searcher.meaning`): each vector's score minus the average of its modality, in
    /// standard deviations of all of them together. The averages and the deviation come from the rows that set the
    /// scale; the others are scored on it.
    fn meaning(&self, scores: &[f32]) -> Vec<f64> {
        let mut sum = [0f64; MODALITIES];
        let mut count = [0usize; MODALITIES];
        for (row, &s) in scores.iter().enumerate() {
            if self.row_sets_scale[row] {
                let m = self.row_modality[row] as usize;
                sum[m] += s as f64;
                count[m] += 1;
            }
        }
        let total_count: usize = count.iter().sum();
        let all = sum.iter().sum::<f64>() / total_count.max(1) as f64;
        let mut means = [all; MODALITIES];
        for m in 0..MODALITIES {
            if count[m] >= MIN_MODALITY_ROWS {
                means[m] = sum[m] / count[m] as f64;
            }
        }
        let centered: Vec<f64> = scores
            .iter()
            .zip(&self.row_modality)
            .map(|(&s, &m)| s as f64 - means[m as usize])
            .collect();
        let (mut n, mut total) = (0usize, 0f64);
        for (row, &c) in centered.iter().enumerate() {
            if self.row_sets_scale[row] {
                n += 1;
                total += c;
            }
        }
        let mean = total / n.max(1) as f64;
        let mut squares = 0f64;
        for (row, &c) in centered.iter().enumerate() {
            if self.row_sets_scale[row] {
                squares += (c - mean) * (c - mean);
            }
        }
        let deviation = (squares / n.max(1) as f64).sqrt().max(DEVIATION_FLOOR);
        centered.iter().map(|c| (c - mean) / deviation).collect()
    }
}

/// `DTFU_SEARCH_TRACE=1` prints the time of each search stage to stderr.
fn trace_on() -> bool {
    std::env::var_os("DTFU_SEARCH_TRACE").is_some()
}

fn trace_stage(on: bool, stage: &str, since: Instant) {
    if on {
        eprintln!("[dtfu-search] {stage}: {:.3} ms", since.elapsed().as_secs_f64() * 1e3);
    }
}

fn modality_index(m: Modality) -> u8 {
    match m {
        Modality::None => 0,
        Modality::Text => 1,
        Modality::Image => 2,
        Modality::Audio => 3,
    }
}

/// `files.info` as the string map Swift reads; any value that isn't a string makes the whole map empty.
fn parse_info(json: Option<&str>) -> HashMap<String, String> {
    let Some(json) = json else { return HashMap::new() };
    let Ok(map) = serde_json::from_str::<serde_json::Map<String, serde_json::Value>>(json) else {
        return HashMap::new();
    };
    map.into_iter()
        .map(|(k, v)| match v {
            serde_json::Value::String(s) => Some((k, s)),
            _ => None,
        })
        .collect::<Option<HashMap<_, _>>>()
        .unwrap_or_default()
}

/// One line, at most `limit` characters (graphemes), for result lists.
fn excerpt(text: &str, limit: usize) -> String {
    let line = text.split_whitespace().collect::<Vec<_>>().join(" ");
    if line.graphemes(true).count() > limit {
        let mut out: String = line.graphemes(true).take(limit).collect();
        out.push('…');
        out
    } else {
        line
    }
}

/// Length in extended grapheme clusters, as Swift's `String.count`.
fn glen(s: &str) -> usize {
    unicode_segmentation::UnicodeSegmentation::graphemes(s, true).count()
}

/// True when every word has a digit and a letter (or is 4 or more characters): a lookup of codes and IDs.
pub fn is_lookup(terms: &[String]) -> bool {
    !terms.is_empty()
        && terms.iter().all(|t| t.chars().any(char::is_numeric) && (t.chars().any(char::is_alphabetic) || glen(t) >= 4))
}

/// How many of `hits` (best first) score at least `ratio` times the top hit: the strong ones.
pub fn strong_count(hits: &[SearchHit], ratio: f64) -> usize {
    match hits.first() {
        Some(top) if top.score > 0.0 => hits.iter().take_while(|h| h.score >= top.score * ratio).count(),
        _ => hits.len(),
    }
}

const STOPWORDS: &[&str] = &[
    "a", "an", "the", "of", "in", "on", "at", "to", "for", "with", "and", "or", "my", "me", "that", "this", "these",
    "those", "from", "about", "where", "what", "which", "who", "when", "is", "was", "are", "were", "be", "it", "its",
    "some", "any", "find", "show", "file", "files", "photo", "photos", "picture", "image", "images", "screenshot",
    "screenshots", "video", "videos", "pdf", "document", "documents", "recording",
    // Bengali and Arabic (normalized, as `terms` sees them)
    "এবং", "ও", "এই", "সেই", "একটি", "আমার", "থেকে", "জন্য", "সাথে", "মধ্যে", "কোথায়", "কোন", "কী", "কি", "যে", "ছবি",
    "স্ক্রিনশট", "ভিডিও", "ফাইল", "পিডিএফ", "في", "من", "على", "الى", "عن", "مع", "هذا", "هذه", "ذلك", "التي", "الذي",
    "اين", "ما", "صورة", "صور", "لقطة", "شاشة", "فيديو", "ملف", "مستند",
];

const KIND_WORDS: &[(&str, FileKind)] = &[
    ("screenshot", FileKind::Screenshot), ("screenshots", FileKind::Screenshot), ("photo", FileKind::Image),
    ("photos", FileKind::Image), ("picture", FileKind::Image), ("pictures", FileKind::Image), ("image", FileKind::Image),
    ("images", FileKind::Image), ("pdf", FileKind::Pdf), ("pdfs", FileKind::Pdf), ("paper", FileKind::Pdf),
    ("papers", FileKind::Pdf), ("document", FileKind::Doc), ("documents", FileKind::Doc), ("note", FileKind::Doc),
    ("notes", FileKind::Doc), ("video", FileKind::Video), ("videos", FileKind::Video), ("clip", FileKind::Video),
    ("movie", FileKind::Video), ("audio", FileKind::Audio), ("song", FileKind::Audio), ("podcast", FileKind::Audio),
    ("voice", FileKind::Audio), ("sound", FileKind::Audio),
    ("স্ক্রিনশট", FileKind::Screenshot), ("ছবি", FileKind::Image), ("পিডিএফ", FileKind::Pdf), ("নোট", FileKind::Doc),
    ("ভিডিও", FileKind::Video), ("অডিও", FileKind::Audio), ("গান", FileKind::Audio), ("রেকর্ডিং", FileKind::Audio),
    ("لقطة", FileKind::Screenshot), ("صورة", FileKind::Image), ("صور", FileKind::Image), ("مستند", FileKind::Doc),
    ("ملاحظات", FileKind::Doc), ("فيديو", FileKind::Video), ("مقطع", FileKind::Video), ("صوت", FileKind::Audio),
    ("تسجيل", FileKind::Audio), ("اغنية", FileKind::Audio),
];

/// Words of the query: normalized as keyword search keeps its text, lowercased, split on what isn't a letter or digit.
/// Stopwords and kind words are dropped, duplicates removed, words of one character dropped.
pub fn terms(text: &str) -> Vec<String> {
    let normalized = text::normalized(text).to_lowercase();
    let mut seen: HashSet<&str> = HashSet::new();
    normalized
        .split(|c: char| !text::is_word_char(c))
        .filter(|w| w.chars().count() >= 2 && !STOPWORDS.contains(w)) // Swift: unicodeScalars.count
        .filter(|w| seen.insert(w))
        .map(str::to_string)
        .collect()
}

/// FTS5 patterns: quoted, and as prefixes for the last word while it's being typed and for words of languages that
/// attach endings.
fn patterns(terms: &[String], prefix_last: bool) -> Vec<String> {
    let n = terms.len();
    terms
        .iter()
        .enumerate()
        .map(|(i, term)| {
            let prefix = (prefix_last && i + 1 == n) || text::inflects(term);
            format!("\"{term}\"{}", if prefix { "*" } else { "" })
        })
        .collect()
}

/// The query's words as an FTS5 OR-query, or `None` when it has no words.
pub fn fts_query(text: &str, prefix_last: bool) -> Option<String> {
    let words = terms(text);
    if words.is_empty() {
        return None;
    }
    Some(patterns(&words, prefix_last).join(" OR "))
}

/// Kinds the query asks for ("in a video", "ভিডিওতে"): a soft boost, not a filter. Bengali and Arabic attach endings,
/// so a kind word also counts at the start of a longer word.
pub fn kind_hints(text: &str) -> Vec<FileKind> {
    let normalized = text::normalized(text).to_lowercase();
    let mut out: Vec<FileKind> = Vec::new();
    for word in normalized.split(|c: char| !text::is_word_char(c)).filter(|w| !w.is_empty()) {
        let exact = KIND_WORDS.iter().find(|(k, _)| *k == word);
        let kind = exact
            .or_else(|| {
                KIND_WORDS
                    .iter()
                    .find(|(k, _)| text::inflects(k) && k.chars().count() >= 3 && word.starts_with(k))
            })
            .map(|(_, kind)| *kind);
        if let Some(kind) = kind {
            if !out.contains(&kind) {
                out.push(kind);
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn terms_keep_bengali_conjuncts_whole() {
        // virama (U+09CD) is a combining mark, not alphanumeric: must not split the word.
        assert_eq!(terms("ক্ষমা"), vec!["ক্ষমা".to_string()]);
    }

    /// Naive reference of `Searcher.meaning`/`modalityMeans`: per-modality maps, no dense arrays.
    fn naive_meaning(scores: &[f32], modality: &[u8], sets: &[bool]) -> Vec<f64> {
        let set_rows: Vec<usize> = (0..scores.len()).filter(|&i| sets[i]).collect();
        let overall = set_rows.iter().map(|&i| scores[i] as f64).sum::<f64>() / set_rows.len().max(1) as f64;
        let mean_of = |m: u8| {
            let group: Vec<f64> = set_rows.iter().filter(|&&i| modality[i] == m).map(|&i| scores[i] as f64).collect();
            if group.len() >= MIN_MODALITY_ROWS {
                group.iter().sum::<f64>() / group.len() as f64
            } else {
                overall
            }
        };
        let centered: Vec<f64> = (0..scores.len()).map(|i| scores[i] as f64 - mean_of(modality[i])).collect();
        let mean = set_rows.iter().map(|&i| centered[i]).sum::<f64>() / set_rows.len().max(1) as f64;
        let var = set_rows.iter().map(|&i| (centered[i] - mean).powi(2)).sum::<f64>() / set_rows.len().max(1) as f64;
        let dev = var.sqrt().max(DEVIATION_FLOOR);
        centered.iter().map(|c| (c - mean) / dev).collect()
    }

    fn bare(conn: &Connection, modality: Vec<u8>, sets: Vec<bool>) -> Searcher<'_> {
        Searcher {
            conn: ConnHandle::Borrowed(conn),
            is_code: false,
            vectors: VectorIndex::default(),
            row_slot: vec![0; modality.len()],
            row_modality: modality,
            row_sets_scale: sets,
            row_file: Vec::new(),
            file_fp: HashMap::new(),
            refreshes_since_load: 0,
            slot_file: Vec::new(),
            slot_kind: Vec::new(),
            slot_of: HashMap::new(),
            file_kind: HashMap::new(),
            legacy_keyword: false,
            loaded_version: 0,
        }
    }

    /// Small xorshift generator so the random test is deterministic without a dependency.
    fn rng(seed: &mut u64) -> f32 {
        *seed ^= *seed << 13;
        *seed ^= *seed >> 7;
        *seed ^= *seed << 17;
        ((*seed >> 40) as f32) / ((1u64 << 24) as f32) * 2.0 - 1.0
    }

    #[test]
    fn dense_meaning_matches_naive_reference() {
        let conn = dtfu_store::open_in_memory().unwrap();
        let mut seed = 0x9E37_79B9_7F4A_7C15u64;
        for &n in &[7usize, 60, 500, 2000] {
            let modality: Vec<u8> = (0..n).map(|i| [1u8, 2, 3, 0][(i * 7 + i / 3) % 4]).collect();
            let sets: Vec<bool> = (0..n).map(|i| i % 5 != 0).collect();
            let scores: Vec<f32> = (0..n)
                .map(|i| rng(&mut seed) * 0.3 + [0.1f32, 0.4, -0.2, 0.0][modality[i] as usize])
                .collect();
            let s = bare(&conn, modality.clone(), sets.clone());
            let dense = s.meaning(&scores);
            let naive = naive_meaning(&scores, &modality, &sets);
            for (d, r) in dense.iter().zip(&naive) {
                assert!((d - r).abs() < 1e-9, "n={n}: dense {d} vs naive {r}");
            }
        }
    }

    #[test]
    fn kind_hints_follow_the_swift_rules() {
        assert_eq!(kind_hints("photos of cats"), vec![FileKind::Image]);
        assert_eq!(kind_hints("screenshot of the bill"), vec![FileKind::Screenshot]);
        assert_eq!(kind_hints("ভিডিওতে কুমির"), vec![FileKind::Video]);
        assert_eq!(kind_hints("صورة قطة"), vec![FileKind::Image]);
        assert!(kind_hints("tax return 2023").is_empty());
        // Swift counts unicode scalars, not graphemes: "ছবি" and "গান" are 3 scalars but 2 graphemes.
        assert_eq!(kind_hints("ছবিতে কুমির"), vec![FileKind::Image]);
        assert_eq!(kind_hints("গানে"), vec![FileKind::Audio]);
        assert_eq!(terms("মা বই"), vec!["মা".to_string(), "বই".to_string()]);
    }

    #[test]
    fn fts_query_quotes_terms_and_prefixes_inflecting_ones() {
        assert_eq!(fts_query("photos of my cat", false).as_deref(), Some("\"cat\""));
        assert_eq!(fts_query("cat cat dog a", false).as_deref(), Some("\"cat\" OR \"dog\""));
        assert_eq!(fts_query("cat dog", true).as_deref(), Some("\"cat\" OR \"dog\"*"));
        assert_eq!(fts_query("the photos", false), None);
        assert_eq!(terms("a cat cat x"), vec!["cat".to_string()]);
    }

    #[test]
    fn lookup_needs_digits_and_letters_or_length_four() {
        assert!(is_lookup(&["ab12".to_string()]));
        assert!(is_lookup(&["1234".to_string()]));
        assert!(!is_lookup(&["12".to_string()]));
        assert!(!is_lookup(&["cat".to_string()]));
        assert!(!is_lookup(&["abcd".to_string()]));
        assert!(!is_lookup(&[]));
    }

    #[test]
    fn strong_count_uses_ratio_of_top() {
        let hit = |score: f64| SearchHit {
            file_id: 0,
            path: String::new(),
            kind: FileKind::Doc,
            score,
            z: None,
            cosine: None,
            keyword_rank: None,
            segment: SegmentKind::Name,
            segment_id: 0,
            loc: None,
            loc_end: None,
            excerpt: None,
            modified: 0.0,
            size: 0,
            info: HashMap::new(),
            focus: None,
        };
        let hits = vec![hit(4.0), hit(3.0), hit(1.0)];
        assert_eq!(strong_count(&hits, 0.7), 2);
        assert_eq!(strong_count(&[], 0.7), 0);
    }

    fn insert_file(conn: &Connection, id: i64, path: &str, kind: &str) {
        conn.execute(
            "INSERT INTO files(id, path, kind, size, mtime, inode, device) VALUES (?1, ?2, ?3, 100, 1000.0, ?1, 1)",
            rusqlite::params![id, path, kind],
        )
        .unwrap();
    }

    fn insert_segment(conn: &Connection, id: i64, file: i64, kind: &str, modality: &str, vec: &[f32], body: &str) {
        conn.execute(
            "INSERT INTO segments(id, file, kind, modality, loc, loc_end, excerpt, vec) VALUES (?1, ?2, ?3, ?4, NULL, NULL, NULL, ?5)",
            rusqlite::params![id, file, kind, modality, dtfu_store::encode_f16(vec)],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO fts(rowid, name, body, original) VALUES (?1, '', ?2, ?2)",
            rusqlite::params![id, body],
        )
        .unwrap();
    }

    #[test]
    fn fixture_search_keeps_bengali_words_whole_and_points_at_matches() {
        let conn = dtfu_store::open_in_memory().unwrap();
        let mut e0 = vec![0f32; DIM];
        e0[0] = 1.0;
        let mut e1 = vec![0f32; DIM];
        e1[1] = 1.0;
        insert_file(&conn, 1, "/docs/story.txt", "doc");
        insert_file(&conn, 2, "/pics/shot.png", "image");
        insert_segment(&conn, 1, 1, "chunk", "text", &e1, "কুমিরের গল্প এখানে");
        insert_segment(&conn, 2, 2, "image", "image", &e0, "");

        let searcher = Searcher::new(&conn).unwrap();
        assert_eq!(searcher.vector_count(), 2);

        // Words only: an empty embedding.
        let hits = searcher.search(&[], "কুমিরের", &SearchOptions::default()).unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].file_id, 1);
        assert_eq!(hits[0].keyword_rank, Some(1));
        assert!(hits[0].excerpt.as_deref().unwrap_or("").contains("«কুমিরের»"));

        // Kind filter removes the document.
        let only_images = SearchOptions {
            kinds: Some([FileKind::Image].into_iter().collect()),
            ..SearchOptions::default()
        };
        assert!(searcher.search(&[], "কুমিরের", &only_images).unwrap().is_empty());

        // Meaning with a query vector. Two rows cannot stand 2 deviations apart (z is at most 1), so the gate is lowered.
        let gate = SearchOptions { z_gate: 0.5, ..SearchOptions::default() };
        let hits = searcher.search(&e0, "", &gate).unwrap();
        assert!(!hits.is_empty());
        assert_eq!(hits[0].file_id, 2);
        assert!(hits[0].cosine.unwrap() > 0.99);
    }

    fn insert_file_at(conn: &Connection, id: i64, path: &str, size: i64, mtime: f64) {
        conn.execute(
            "INSERT INTO files(id, path, kind, size, mtime, inode, device) VALUES (?1, ?2, 'doc', ?3, ?4, ?1, 1)",
            rusqlite::params![id, path, size, mtime],
        )
        .unwrap();
    }

    #[test]
    fn filters_apply_before_the_limit_and_keep_scores() {
        let conn = dtfu_store::open_in_memory().unwrap();
        // File i (id i + 1) lies a little further from the query than file i - 1, so the unfiltered order is by id.
        for i in 0..12i64 {
            let ext = if i % 2 == 0 { "pdf" } else { "txt" };
            insert_file_at(&conn, i + 1, &format!("/docs/f{i}.{ext}"), i * 1000, 1000.0 + i as f64 * 100.0);
            let theta = 0.1 * i as f32 + 0.05;
            let mut v = vec![0f32; DIM];
            v[0] = theta.cos();
            v[1] = theta.sin();
            insert_segment(&conn, i + 1, i + 1, "chunk", "text", &v, "");
        }
        let searcher = Searcher::new(&conn).unwrap();
        let mut e0 = vec![0f32; DIM];
        e0[0] = 1.0;
        // Every file is a candidate, so the filters are the only thing that can remove one.
        let base = SearchOptions { z_gate: -10.0, ..SearchOptions::default() };
        let all = searcher.search(&e0, "", &SearchOptions { limit: 100, ..base.clone() }).unwrap();
        let ids = |hits: &[SearchHit]| hits.iter().map(|h| h.file_id).collect::<Vec<i64>>();
        assert_eq!(ids(&all), (1..=12).collect::<Vec<i64>>());
        let run = |filters: Filters, limit: usize| {
            searcher.search(&e0, "", &SearchOptions { limit, filters, ..base.clone() }).unwrap()
        };

        // Size >= 6000 bytes is files 7..=12, none of which is in the unfiltered top 3. The limit applies afterwards.
        let big = Filters { size_min: Some(6000), ..Filters::default() };
        let hits = run(big.clone(), 3);
        assert_eq!(ids(&hits), vec![7, 8, 9]);
        // Scores and z of the survivors are the ones they have without the filter.
        for h in &hits {
            let same = all.iter().find(|a| a.file_id == h.file_id).unwrap();
            assert_eq!(h.score, same.score);
            assert_eq!(h.z, same.z);
            assert_eq!(h.cosine, same.cosine);
        }

        // The full filtered list is the unfiltered list with the non-matching files removed, in the same order.
        let pdf_big = Filters { exts: Some(["pdf".to_string()].into_iter().collect()), ..big.clone() };
        let filtered = run(pdf_big.clone(), 100);
        let expected: Vec<&SearchHit> = all.iter().filter(|h| [7, 9, 11].contains(&h.file_id)).collect();
        assert_eq!(filtered.len(), expected.len());
        for (got, want) in filtered.iter().zip(expected) {
            assert_eq!((got.file_id, got.score), (want.file_id, want.score));
        }
        assert_eq!(ids(&run(pdf_big, 2)), vec![7, 9]);

        // Extension alone: the odd files (ids 2, 4, 6, ...) with a limit of 3.
        let txt = Filters { exts: Some(["txt".to_string()].into_iter().collect()), ..Filters::default() };
        assert_eq!(ids(&run(txt, 3)), vec![2, 4, 6]);

        // Modification time: files 9..=12 are the newest.
        let recent = Filters { mtime_min: Some(1800.0), ..Filters::default() };
        assert_eq!(ids(&run(recent, 2)), vec![9, 10]);

        // Nothing matches.
        let none = Filters { size_min: Some(1_000_000), ..Filters::default() };
        assert!(run(none, 5).is_empty());
    }

    #[test]
    fn keyword_matches_are_filtered_before_the_candidate_cap() {
        let conn = dtfu_store::open_in_memory().unwrap();
        let mut v = vec![0f32; DIM];
        v[5] = 1.0;
        // 150 text files match "report" equally well; the only pdf has the highest ids, so without filtering it falls
        // outside the first MAX_WORD_FILES keyword candidates.
        for id in 1..=150i64 {
            insert_file_at(&conn, id, &format!("/docs/r{id}.txt"), 10, 1000.0);
            insert_segment(&conn, id, id, "chunk", "text", &v, "report");
        }
        insert_file_at(&conn, 151, "/docs/annual.pdf", 10, 1000.0);
        insert_segment(&conn, 151, 151, "chunk", "text", &v, "report");
        let searcher = Searcher::new(&conn).unwrap();
        let filters = Filters { exts: Some(["pdf".to_string()].into_iter().collect()), ..Filters::default() };
        let opts = SearchOptions { limit: 5, filters, ..SearchOptions::default() };
        let hits = searcher.search(&[], "report", &opts).unwrap();
        assert_eq!(hits.iter().map(|h| h.file_id).collect::<Vec<_>>(), vec![151]);
    }
}
