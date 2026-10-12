//! `IndexStore`: the index database (files, segments, the keyword table and meta), a port of DigUp's Swift
//! `IndexStore` (schema v5, file-compatible). Writes run in `BEGIN IMMEDIATE` transactions; `fts.rowid` always equals
//! the `segments.id` it describes; rows are deleted fts-first because there are no triggers.

use anyhow::{bail, Context, Result};
use dtfu_core::{FileKind, FileState, Modality, SegmentKind, DIM};
use rusqlite::types::Value;
use rusqlite::{params, params_from_iter, Connection, OptionalExtension, Params, Row, TransactionBehavior};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

/// File name of the main index inside the index folder.
pub const MAIN_FILE: &str = "index.sqlite";
/// File name of the code index (same schema, separate file).
pub const CODE_FILE: &str = "code.sqlite";

/// `ORDER BY` for indexing order: screenshots first, then images, PDFs, documents, audio, video, then the rest.
const KIND_ORDER: &str = "CASE kind WHEN 'screenshot' THEN 0 WHEN 'image' THEN 1 WHEN 'pdf' THEN 2 WHEN 'doc' THEN 3 \
     WHEN 'audio' THEN 4 WHEN 'video' THEN 5 ELSE 6 END";
const FILE_COLUMNS: &str = "id, path, kind, size, mtime, inode, device, state";
/// Two mtimes closer than this (seconds) count as the same date.
const MTIME_TOLERANCE: f64 = 0.001;

/// A file found on disk, as the crawler reports it.
#[derive(Debug, Clone, PartialEq)]
pub struct NewFile {
    pub path: String,
    pub kind: FileKind,
    pub size: i64,
    /// Seconds since the Unix epoch.
    pub mtime: f64,
    pub inode: u64,
    pub device: u64,
}

/// One extracted piece of a file (a page, a chunk, a frame, the OCR text...).
#[derive(Debug, Clone, PartialEq)]
pub struct NewSegment {
    pub kind: SegmentKind,
    /// Kind of input the vector came from. Stored as `none` when there is no vector.
    pub modality: Modality,
    /// PDF page (1-based), or start second for audio/video, or first line/cell/chunk.
    pub loc: Option<f64>,
    pub loc_end: Option<f64>,
    pub excerpt: Option<String>,
    /// Text for keyword search. Empty or `None` adds no keyword row.
    pub text: Option<String>,
    /// L2-normalized embedding, exactly `DIM` (768) values. Stored as float16.
    pub vector: Option<Vec<f32>>,
}

/// A row of `files`.
#[derive(Debug, Clone, PartialEq)]
pub struct FileRecord {
    pub id: i64,
    pub path: String,
    pub kind: FileKind,
    pub size: i64,
    pub mtime: f64,
    pub inode: u64,
    pub device: u64,
    pub state: FileState,
}

/// What `IndexStore::adopt` does when the stored vector source or fingerprint differs from the given ones.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mismatch {
    /// Return an error; vectors from two models or settings must never mix.
    Refuse,
    /// Delete every vector and queue every file again, then adopt the new values.
    Reset,
}

/// Result of `IndexStore::adopt`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Adoption {
    /// The store had no vector source or fingerprint yet; the given ones were recorded.
    Fresh,
    /// Both already matched.
    Same,
    /// They differed and the store was reset (`Mismatch::Reset`).
    Reset,
}

/// Counts for status displays (`IndexStore::counts`).
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct Counts {
    /// `files` rows per `kind`, then per `state`.
    pub by_kind_and_state: BTreeMap<String, BTreeMap<String, usize>>,
    /// Segments (not counting `name` rows) that have a vector.
    pub vectors: usize,
    /// Segments, not counting `name` rows.
    pub segments: usize,
    /// Long files still being read (state `partial`).
    pub unfinished: usize,
    /// Pages and chunks left to read in those files.
    pub left: usize,
}

/// The index database. Single connection, single thread; writes take `&mut self`.
pub struct IndexStore {
    conn: Connection,
    file: PathBuf,
    scope: Option<Vec<String>>,
}

impl IndexStore {
    /// Opens (creating if needed) `directory/file_name` at schema v5 with WAL, `synchronous=NORMAL`,
    /// `busy_timeout=5000` and foreign keys on. Fails on a newer or unmigrated schema.
    pub fn open(directory: &Path, file_name: &str) -> Result<Self> {
        std::fs::create_dir_all(directory)
            .with_context(|| format!("creating index folder {}", directory.display()))?;
        let file = directory.join(file_name);
        let conn = crate::open(&file)?;
        Ok(Self { conn, file, scope: None })
    }

    /// Gives up the store for its connection (a cached `Searcher` owns one).
    pub fn into_connection(self) -> Connection {
        self.conn
    }

    /// The underlying connection, for read-only use by the searcher.
    pub fn connection(&self) -> &Connection {
        &self.conn
    }

    /// Path of the database file.
    pub fn file(&self) -> &Path {
        &self.file
    }

    /// Bytes on disk: the database plus its write-ahead log.
    pub fn size_on_disk(&self) -> u64 {
        let base = self.file.to_string_lossy().into_owned();
        [base.clone(), format!("{base}-wal")]
            .iter()
            .map(|p| std::fs::metadata(p).map(|m| m.len()).unwrap_or(0))
            .sum()
    }

    /// Limits `pending`, `pending_count`, `unfinished*` and `counts` to files under these folders (`None` = all).
    /// `Some(vec![])` matches nothing. Used when a drive is unplugged so its files neither fail nor count as waiting.
    pub fn set_scope(&mut self, roots: Option<Vec<String>>) {
        self.scope = roots;
    }

    // MARK: meta

    /// The value stored under `key` in `meta`.
    pub fn meta(&self, key: &str) -> Result<Option<String>> {
        Ok(self
            .conn
            .query_row("SELECT value FROM meta WHERE key = ?1", [key], |r| r.get(0))
            .optional()?)
    }

    /// Stores `value` under `key`, or deletes the key when `value` is `None`.
    pub fn set_meta(&self, key: &str, value: Option<&str>) -> Result<()> {
        match value {
            Some(v) => self.conn.prepare_cached(
                "INSERT INTO meta(key, value) VALUES(?1, ?2) ON CONFLICT(key) DO UPDATE SET value = excluded.value",
            )?.execute(params![key, v])?,
            None => self.conn.prepare_cached("DELETE FROM meta WHERE key = ?1")?.execute([key])?,
        };
        Ok(())
    }

    /// Checks that the stored `vector_source` and `fingerprint` (`meta`) are the given ones. A new store records them.
    /// On a difference, `on_mismatch` decides: refuse with an error, or reset (delete vectors, requeue all files).
    pub fn adopt(&mut self, vector_source: &str, fingerprint: &str, on_mismatch: Mismatch) -> Result<Adoption> {
        let stored_source = self.meta("vector_source")?;
        let stored_fingerprint = self.meta("fingerprint")?;
        if stored_source.is_none() && stored_fingerprint.is_none() {
            self.record_identity(vector_source, fingerprint)?;
            return Ok(Adoption::Fresh);
        }
        if stored_source.as_deref() == Some(vector_source) && stored_fingerprint.as_deref() == Some(fingerprint) {
            return Ok(Adoption::Same);
        }
        match on_mismatch {
            Mismatch::Refuse => bail!(
                "index was built with vector source {:?} and fingerprint {:?}, not {:?} / {:?}; \
                 reset it or use the matching model and settings",
                stored_source.unwrap_or_default(),
                stored_fingerprint.unwrap_or_default(),
                vector_source,
                fingerprint
            ),
            Mismatch::Reset => {
                self.reset_all()?;
                self.record_identity(vector_source, fingerprint)?;
                Ok(Adoption::Reset)
            }
        }
    }

    fn record_identity(&self, vector_source: &str, fingerprint: &str) -> Result<()> {
        self.set_meta("vector_source", Some(vector_source))?;
        self.set_meta("fingerprint", Some(fingerprint))
    }

    // MARK: files

    /// Adds a file as `pending`, with its `name` row (keyword search on its name and folders) and the given
    /// segments, all in one transaction. Returns the new file id.
    pub fn insert_file(&mut self, file: &NewFile, segments: &[NewSegment]) -> Result<i64> {
        let tx = self.conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        tx.prepare_cached(
            "INSERT INTO files(path, kind, size, mtime, inode, device) VALUES(?1, ?2, ?3, ?4, ?5, ?6)",
        )?
        .execute(params![
            file.path,
            file.kind.as_str(),
            file.size,
            file.mtime,
            file.inode as i64,
            file.device as i64
        ])?;
        let id = tx.last_insert_rowid();
        insert_name_row(&tx, id, &file.path)?;
        for segment in segments {
            insert_segment(&tx, id, segment)?;
        }
        tx.commit()?;
        Ok(id)
    }

    /// Adds segments to a file in one transaction (one step of extraction). Also bumps the file's `indexed_at`
    /// (strictly increasing), because a cached `dtfu_search::Searcher::refresh` finds changed files by the
    /// fingerprint of their `files` row.
    ///
    /// FINGERPRINT CONTRACT: every writer that inserts, deletes or replaces a file's segments or vectors must also
    /// change that file's row (`state`, `indexed_at` or `info`) in the same transaction, or delete the file.
    /// Segment ids are reused after a delete, so without it a refresh cannot see the change.
    pub fn add_segments(&mut self, file_id: i64, segments: &[NewSegment]) -> Result<()> {
        let tx = self.conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        for segment in segments {
            insert_segment(&tx, file_id, segment)?;
        }
        let now = SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs_f64()).unwrap_or(0.0);
        tx.prepare_cached("UPDATE files SET indexed_at = max(?1, coalesce(indexed_at, 0) + 0.000001) WHERE id = ?2")?
            .execute(params![now, file_id])?;
        tx.commit()?;
        Ok(())
    }

    /// `add_segments` then `finish` in one transaction: a crash between them cannot leave segments on a pending file.
    pub fn add_segments_and_finish(
        &mut self,
        id: i64,
        segments: &[NewSegment],
        state: FileState,
        detail: Option<&str>,
        info: &BTreeMap<String, String>,
    ) -> Result<()> {
        let json = if info.is_empty() { None } else { Some(serde_json::to_string(info)?) };
        let now = SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs_f64()).unwrap_or(0.0);
        let tx = self.conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        for segment in segments {
            insert_segment(&tx, id, segment)?;
        }
        tx.prepare_cached("UPDATE files SET state = ?1, detail = ?2, info = ?3, indexed_at = ?4 WHERE id = ?5")?
            .execute(params![state.as_str(), detail, json, now, id])?;
        tx.commit()?;
        Ok(())
    }

    /// The file changed on disk: drop what was extracted from it (keeping its name row) and queue it again.
    /// With `Some(file)` the stored kind, size, mtime, inode and device are replaced too.
    pub fn requeue(&mut self, id: i64, as_file: Option<&NewFile>) -> Result<()> {
        let tx = self.conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        delete_segments(&tx, id, true)?;
        match as_file {
            Some(f) => tx.prepare_cached(
                "UPDATE files SET kind = ?1, size = ?2, mtime = ?3, inode = ?4, device = ?5, state = 'pending', \
                 detail = NULL, info = NULL, indexed_at = NULL WHERE id = ?6",
            )?
            .execute(params![f.kind.as_str(), f.size, f.mtime, f.inode as i64, f.device as i64, id])?,
            None => tx.prepare_cached(
                "UPDATE files SET state = 'pending', detail = NULL, info = NULL, indexed_at = NULL WHERE id = ?1",
            )?
            .execute([id])?,
        };
        tx.commit()?;
        Ok(())
    }

    /// Same file at a new path (a move or rename): only the path and the name keywords change; nothing is re-embedded.
    pub fn move_file(&mut self, id: i64, new_path: &str) -> Result<()> {
        let tx = self.conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        tx.prepare_cached("UPDATE files SET path = ?1 WHERE id = ?2")?.execute(params![new_path, id])?;
        let (name, body) = name_texts(new_path);
        tx.prepare_cached(
            "UPDATE fts SET name = ?1, body = ?2 WHERE rowid IN (SELECT id FROM segments WHERE file = ?3 AND kind = 'name')",
        )?
        .execute(params![name, body, id])?;
        tx.commit()?;
        Ok(())
    }

    /// Finds a file by identity (device, inode, size, and mtime within 1 ms). Callers use it to detect a move when the
    /// old path is gone. A match is only a candidate: the caller decides.
    pub fn find_by_identity(&self, device: u64, inode: u64, size: i64, mtime: f64) -> Result<Option<FileRecord>> {
        let sql = format!(
            "SELECT {FILE_COLUMNS} FROM files WHERE device = ?1 AND inode = ?2 AND size = ?3 AND ABS(mtime - ?4) < ?5 LIMIT 1"
        );
        let mut rows = query_files(&self.conn, &sql, params![device as i64, inode as i64, size, mtime, MTIME_TOLERANCE])?;
        Ok(rows.pop())
    }

    /// Change detection: true when the stored record still matches what is on disk (size, mtime within 1 ms, kind).
    pub fn is_unchanged(record: &FileRecord, file: &NewFile) -> bool {
        record.size == file.size && (record.mtime - file.mtime).abs() < MTIME_TOLERANCE && record.kind == file.kind
    }

    /// Removes a file: its fts rows first (no triggers), then the `files` row, which cascades to its segments.
    pub fn delete_file(&mut self, id: i64) -> Result<()> {
        let tx = self.conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        delete_file_rows(&tx, id)?;
        tx.commit()?;
        Ok(())
    }

    /// Removes every file not under one of `keep_roots` (folders taken out of the index). Returns how many went.
    pub fn prune(&mut self, keep_roots: &[String]) -> Result<usize> {
        let prefixes: Vec<String> = keep_roots.iter().map(|r| with_slash(r)).collect();
        let gone: Vec<i64> = {
            let mut stmt = self.conn.prepare_cached("SELECT id, path FROM files")?;
            let rows = stmt.query_map([], |r| Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?)))?;
            let mut gone = Vec::new();
            for row in rows {
                let (id, path) = row?;
                if !prefixes.iter().any(|p| path.starts_with(p.as_str())) {
                    gone.push(id);
                }
            }
            gone
        };
        if gone.is_empty() {
            return Ok(0);
        }
        let tx = self.conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        for id in &gone {
            delete_file_rows(&tx, *id)?;
        }
        tx.commit()?;
        Ok(gone.len())
    }

    /// Queues every file again and forgets the fingerprint: vectors are deleted, name rows kept.
    pub fn reset_all(&mut self) -> Result<()> {
        let tx = self.conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        tx.execute_batch(
            "DELETE FROM fts WHERE rowid IN (SELECT id FROM segments WHERE kind != 'name');
             DELETE FROM segments WHERE kind != 'name';
             UPDATE files SET state = 'pending', detail = NULL, info = NULL, indexed_at = NULL;",
        )?;
        tx.prepare_cached("DELETE FROM meta WHERE key = 'fingerprint'")?.execute([])?;
        tx.commit()?;
        Ok(())
    }

    /// True when a file with exactly this path is in the index.
    pub fn has_path(&self, path: &str) -> Result<bool> {
        Ok(self.conn.query_row("SELECT 1 FROM files WHERE path = ?1", params![path], |_| Ok(())).optional()?.is_some())
    }

    /// Every file whose path is under one of `roots` (a root inside another root is not listed twice).
    pub fn files_under(&self, roots: &[String]) -> Result<Vec<FileRecord>> {
        let mut unique: Vec<&String> = Vec::new();
        for r in roots {
            if !unique.contains(&r) {
                unique.push(r);
            }
        }
        let mut out = Vec::new();
        for root in &unique {
            if unique.iter().any(|other| is_inside(root, other)) {
                continue;
            }
            let prefix = with_slash(root);
            let upper = format!("{prefix}\u{10FFFF}");
            out.extend(query_files(
                &self.conn,
                &format!("SELECT {FILE_COLUMNS} FROM files WHERE path >= ?1 AND path < ?2"),
                params![prefix, upper],
            )?);
        }
        Ok(out)
    }

    /// Files waiting to be embedded, in indexing order (`KIND_ORDER`, then newest first). `only` narrows the set to
    /// some ids (`Some(&[])` gives nothing); `limit` caps the count (`None` = all).
    pub fn pending(&self, limit: Option<usize>, only: Option<&[i64]>) -> Result<Vec<FileRecord>> {
        self.queue("state = 'pending'", limit, only, "mtime DESC")
    }

    /// Number of files waiting (state `pending`, within the scope).
    pub fn pending_count(&self) -> Result<usize> {
        self.count_files("state = 'pending'")
    }

    /// Long files whose reading is not finished (state `partial`), in indexing order.
    pub fn unfinished(&self) -> Result<Vec<FileRecord>> {
        self.queue("state = 'partial'", None, None, "mtime DESC")
    }

    /// How many long files are unfinished, and how many of their pages and chunks are left (from `info`'s
    /// `read_end` and `read_to`).
    pub fn unfinished_count(&self) -> Result<(usize, usize)> {
        let (scope_sql, scope_vals) = self.scope_clause();
        let sql = format!(
            "SELECT COUNT(*), COALESCE(SUM(MAX(0, CAST(json_extract(info, '$.read_end') AS INTEGER) \
             - CAST(json_extract(info, '$.read_to') AS INTEGER))), 0) \
             FROM files WHERE state = 'partial'{scope_sql}"
        );
        let (files, left): (i64, i64) =
            self.conn.prepare_cached(&sql)?.query_row(params_from_iter(scope_vals), |r| Ok((r.get(0)?, r.get(1)?)))?;
        Ok((files as usize, left as usize))
    }

    /// Where a long file's reading got to, from its stored segments: the largest `loc` of its page, page_image and
    /// chunk segments (`IndexStore.lastPlace`, IS:355). `None` when nothing is stored for it.
    pub fn last_place(&self, file_id: i64) -> Result<Option<f64>> {
        let last: Option<f64> = self
            .conn
            .prepare_cached(
                "SELECT MAX(loc) FROM segments WHERE file = ?1 AND kind IN ('page', 'page_image', 'chunk')",
            )?
            .query_row([file_id], |r| r.get(0))?;
        Ok(last)
    }

    /// What indexing learned about a file: the `info` JSON object, with string values. Empty when absent or unreadable.
    pub fn info(&self, id: i64) -> Result<BTreeMap<String, String>> {
        let json: Option<String> = self
            .conn
            .prepare_cached("SELECT info FROM files WHERE id = ?1")?
            .query_row([id], |r| r.get(0))
            .optional()?
            .flatten();
        Ok(json.and_then(|j| serde_json::from_str(&j).ok()).unwrap_or_default())
    }

    /// Sets a file's state, `detail` and `info` (replacing both), and stamps `indexed_at` with now.
    /// An empty `info` stores NULL.
    pub fn finish(
        &self,
        id: i64,
        state: FileState,
        detail: Option<&str>,
        info: &BTreeMap<String, String>,
    ) -> Result<()> {
        let json = if info.is_empty() { None } else { Some(serde_json::to_string(info)?) };
        let now = SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs_f64()).unwrap_or(0.0);
        self.conn
            .prepare_cached("UPDATE files SET state = ?1, detail = ?2, info = ?3, indexed_at = ?4 WHERE id = ?5")?
            .execute(params![state.as_str(), detail, json, now, id])?;
        Ok(())
    }

    /// Counts for status: files per kind and state, vectors, segments and unfinished long files.
    pub fn counts(&self) -> Result<Counts> {
        let mut counts = Counts::default();
        let (scope_sql, scope_vals) = self.scope_clause();
        let mut stmt = self.conn.prepare_cached(&format!(
            "SELECT kind, state, COUNT(*) FROM files WHERE 1{scope_sql} GROUP BY kind, state"
        ))?;
        let rows = stmt.query_map(params_from_iter(scope_vals.clone()), |r| {
            Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?, r.get::<_, i64>(2)?))
        })?;
        for row in rows {
            let (kind, state, n) = row?;
            counts.by_kind_and_state.entry(kind).or_default().insert(state, n as usize);
        }
        drop(stmt);
        (counts.unfinished, counts.left) = self.unfinished_count()?;
        let in_scope = if self.scope.is_some() {
            format!(" AND file IN (SELECT id FROM files WHERE 1{scope_sql})")
        } else {
            String::new()
        };
        let (segments, vectors): (i64, i64) = self
            .conn
            .prepare_cached(&format!("SELECT COUNT(*), COUNT(vec) FROM segments WHERE kind != 'name'{in_scope}"))?
            .query_row(params_from_iter(scope_vals), |r| Ok((r.get(0)?, r.get(1)?)))?;
        counts.segments = segments as usize;
        counts.vectors = vectors as usize;
        Ok(counts)
    }

    // MARK: content duplicates

    /// Indexed files (state `done`, not code) whose size is in `sizes`: the only files a new file's bytes can equal.
    /// One scan of the done rows, keeping only the sizes asked for.
    pub fn done_files_with_sizes(&self, sizes: &std::collections::HashSet<i64>) -> Result<Vec<FileRecord>> {
        let mut stmt = self.conn.prepare_cached(&format!(
            "SELECT {FILE_COLUMNS} FROM files WHERE state = 'done' AND kind != 'code' ORDER BY id"
        ))?;
        let mut out = Vec::new();
        for row in stmt.query_map([], file_record)? {
            let file = row?;
            if sizes.contains(&file.size) {
                out.push(file);
            }
        }
        Ok(out)
    }

    /// Sets one string key of a file's `info` object and keeps its other keys.
    pub fn set_info_value(&self, id: i64, key: &str, value: &str) -> Result<()> {
        self.conn
            .prepare_cached("UPDATE files SET info = json_set(COALESCE(info, '{}'), '$.' || ?2, ?3) WHERE id = ?1")?
            .execute(params![id, key, value])?;
        Ok(())
    }

    /// Gives `target` (state `pending`) a copy of the indexed content of `source` (state `done`, same bytes): every
    /// non-name segment with its vector and keyword row, the source's `detail`, and `info` as given. One transaction.
    /// The copies are independent rows, so either file can later be changed or deleted alone; `target` keeps its own
    /// name row. Returns false, changing nothing, when either file is not in the expected state.
    pub fn copy_indexed_content(&mut self, source: i64, target: i64, info: &BTreeMap<String, String>) -> Result<bool> {
        let tx = self.conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let states: Vec<(i64, String)> = {
            let mut stmt = tx.prepare("SELECT id, state FROM files WHERE id IN (?1, ?2)")?;
            let rows = stmt.query_map(params![source, target], |r| Ok((r.get(0)?, r.get(1)?)))?;
            rows.collect::<rusqlite::Result<Vec<_>>>()?
        };
        let state_of = |id: i64| states.iter().find(|(i, _)| *i == id).map(|(_, s)| s.as_str());
        if state_of(source) != Some("done") || state_of(target) != Some("pending") {
            return Ok(false);
        }
        delete_segments(&tx, target, true)?;
        type Row = (String, String, Option<f64>, Option<f64>, Option<String>, Option<Vec<u8>>, Option<String>, Option<String>, Option<String>);
        let copies: Vec<Row> = {
            let mut stmt = tx.prepare(
                "SELECT s.kind, s.modality, s.loc, s.loc_end, s.excerpt, s.vec, f.name, f.body, f.original \
                 FROM segments s LEFT JOIN fts f ON f.rowid = s.id WHERE s.file = ?1 AND s.kind != 'name' ORDER BY s.id",
            )?;
            let rows = stmt.query_map([source], |r| {
                Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?, r.get(5)?, r.get(6)?, r.get(7)?, r.get(8)?))
            })?;
            rows.collect::<rusqlite::Result<Vec<_>>>()?
        };
        for (kind, modality, loc, loc_end, excerpt, vec, name, body, original) in copies {
            tx.prepare_cached(
                "INSERT INTO segments(file, kind, modality, loc, loc_end, excerpt, vec) VALUES(?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            )?
            .execute(params![target, kind, modality, loc, loc_end, excerpt, vec])?;
            let id = tx.last_insert_rowid();
            if let Some(body) = body {
                tx.prepare_cached("INSERT INTO fts(rowid, name, body, original) VALUES(?1, ?2, ?3, ?4)")?
                    .execute(params![id, name, body, original])?;
            }
        }
        let detail: Option<String> = tx.query_row("SELECT detail FROM files WHERE id = ?1", [source], |r| r.get(0))?;
        let json = if info.is_empty() { None } else { Some(serde_json::to_string(info)?) };
        let now = SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs_f64()).unwrap_or(0.0);
        tx.prepare_cached("UPDATE files SET state = 'done', detail = ?1, info = ?2, indexed_at = ?3 WHERE id = ?4")?
            .execute(params![detail, json, now, target])?;
        tx.commit()?;
        Ok(true)
    }

    // MARK: helpers

    fn queue(
        &self,
        state_filter: &str,
        limit: Option<usize>,
        only: Option<&[i64]>,
        order_tail: &str,
    ) -> Result<Vec<FileRecord>> {
        let id_filter = match only {
            Some([]) => return Ok(Vec::new()),
            Some(ids) => format!(
                " AND id IN ({})",
                ids.iter().map(|i| i.to_string()).collect::<Vec<_>>().join(",")
            ),
            None => String::new(),
        };
        let (scope_sql, scope_vals) = self.scope_clause();
        let sql = format!(
            "SELECT {FILE_COLUMNS} FROM files WHERE {state_filter}{id_filter}{scope_sql} \
             ORDER BY {KIND_ORDER}, {order_tail} LIMIT ?"
        );
        let mut values = scope_vals;
        values.push(Value::Integer(limit.map(|n| n as i64).unwrap_or(-1)));
        query_files(&self.conn, &sql, params_from_iter(values))
    }

    fn count_files(&self, state_filter: &str) -> Result<usize> {
        let (scope_sql, scope_vals) = self.scope_clause();
        let n: i64 = self
            .conn
            .prepare_cached(&format!("SELECT COUNT(*) FROM files WHERE {state_filter}{scope_sql}"))?
            .query_row(params_from_iter(scope_vals), |r| r.get(0))?;
        Ok(n as usize)
    }

    /// The scope as SQL (with `?` placeholders) and its bound values. Empty when no scope is set.
    fn scope_clause(&self) -> (String, Vec<Value>) {
        match &self.scope {
            None => (String::new(), Vec::new()),
            Some(roots) if roots.is_empty() => (" AND 0".to_string(), Vec::new()),
            Some(roots) => {
                let parts = vec!["(path >= ? AND path < ?)"; roots.len()].join(" OR ");
                let mut values = Vec::with_capacity(roots.len() * 2);
                for root in roots {
                    let prefix = with_slash(root);
                    values.push(Value::Text(prefix.clone()));
                    values.push(Value::Text(format!("{prefix}\u{10FFFF}")));
                }
                (format!(" AND ({parts})"), values)
            }
        }
    }
}

// MARK: free helpers (run inside a transaction, or on a plain connection)

fn query_files<P: Params>(conn: &Connection, sql: &str, params: P) -> Result<Vec<FileRecord>> {
    let mut stmt = conn.prepare_cached(sql)?;
    let rows = stmt.query_map(params, file_record)?;
    Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
}

fn file_record(r: &Row<'_>) -> rusqlite::Result<FileRecord> {
    let kind: String = r.get(2)?;
    let state: String = r.get(7)?;
    Ok(FileRecord {
        id: r.get(0)?,
        path: r.get(1)?,
        kind: FileKind::parse(&kind).unwrap_or(FileKind::Doc),
        size: r.get(3)?,
        mtime: r.get(4)?,
        inode: r.get::<_, i64>(5)? as u64,
        device: r.get::<_, i64>(6)? as u64,
        state: FileState::parse(&state).unwrap_or(FileState::Pending),
    })
}

fn insert_name_row(c: &Connection, file: i64, path: &str) -> Result<()> {
    c.prepare_cached("INSERT INTO segments(file, kind, modality) VALUES(?1, 'name', 'none')")?.execute([file])?;
    let id = c.last_insert_rowid();
    let (name, body) = name_texts(path);
    c.prepare_cached("INSERT INTO fts(rowid, name, body) VALUES(?1, ?2, ?3)")?.execute(params![id, name, body])?;
    Ok(())
}

/// Adds one segment and, when it has text, its keyword row with `rowid == segments.id`.
fn insert_segment(c: &Connection, file: i64, s: &NewSegment) -> Result<()> {
    let vec_blob = match &s.vector {
        Some(v) => {
            if v.len() != DIM {
                bail!("segment vector has {} values, expected {DIM}", v.len());
            }
            Some(crate::encode_f16(v))
        }
        None => None,
    };
    let modality = if s.vector.is_some() { s.modality } else { Modality::None };
    c.prepare_cached(
        "INSERT INTO segments(file, kind, modality, loc, loc_end, excerpt, vec) VALUES(?1, ?2, ?3, ?4, ?5, ?6, ?7)",
    )?
    .execute(params![file, s.kind.as_str(), modality.as_str(), s.loc, s.loc_end, s.excerpt, vec_blob])?;
    let id = c.last_insert_rowid();
    if let Some(text) = s.text.as_deref().filter(|t| !t.is_empty()) {
        let normalized = normalize(text);
        let original = (normalized != text).then_some(text);
        c.prepare_cached("INSERT INTO fts(rowid, name, body, original) VALUES(?1, '', ?2, ?3)")?
            .execute(params![id, normalized, original])?;
    }
    Ok(())
}

/// Deletes a file's segments (all, or all but its `name` row) and their fts rows, fts first.
fn delete_segments(c: &Connection, file: i64, keep_name: bool) -> Result<()> {
    let filter = if keep_name { " AND kind != 'name'" } else { "" };
    c.prepare_cached(&format!("DELETE FROM fts WHERE rowid IN (SELECT id FROM segments WHERE file = ?1{filter})"))?
        .execute([file])?;
    c.prepare_cached(&format!("DELETE FROM segments WHERE file = ?1{filter}"))?.execute([file])?;
    Ok(())
}

/// fts rows first (they are not covered by the cascade), then the file row, which cascades to its segments.
fn delete_file_rows(c: &Connection, id: i64) -> Result<()> {
    c.prepare_cached("DELETE FROM fts WHERE rowid IN (SELECT id FROM segments WHERE file = ?1)")?.execute([id])?;
    c.prepare_cached("DELETE FROM files WHERE id = ?1")?.execute([id])?;
    Ok(())
}

/// Keyword texts of a name row: the file name, and the folders it sits in (`name`, `body`).
fn name_texts(path: &str) -> (String, String) {
    let name = Path::new(path).file_name().and_then(|s| s.to_str()).unwrap_or("");
    let parent = Path::new(path).parent().map(|p| p.to_string_lossy().into_owned()).unwrap_or_default();
    let home = std::env::var("HOME").unwrap_or_default();
    let folder = match (!home.is_empty()).then(|| parent.strip_prefix(home.as_str())).flatten() {
        Some(rest) => rest,
        None => parent.as_str(),
    };
    let folder_words = folder.split('/').filter(|s| !s.is_empty()).collect::<Vec<_>>().join(" ");
    (normalize(name), normalize(&folder_words))
}

/// `root` with a trailing `/`, so that prefix matching never catches a sibling such as `/ab` for `/a`.
fn with_slash(root: &str) -> String {
    if root.ends_with('/') { root.to_string() } else { format!("{root}/") }
}

/// True when `child` is a folder strictly inside `parent`.
fn is_inside(child: &str, parent: &str) -> bool {
    child != parent && child.starts_with(&with_slash(parent))
}

/// Keyword normalization for `fts`: Swift's `SearchText.normalized` (NFKC, joiners and direction marks dropped, Arabic
/// folded). Case is left to the FTS5 tokenizer, as in Swift.
pub fn normalize(text: &str) -> String {
    dtfu_text::normalized(text)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    /// A temporary folder, removed on drop (no dev-dependency needed).
    struct TempDir(PathBuf);

    impl TempDir {
        fn new() -> Self {
            static COUNTER: AtomicUsize = AtomicUsize::new(0);
            let n = COUNTER.fetch_add(1, Ordering::SeqCst);
            let nanos = SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_nanos()).unwrap_or(0);
            let dir = std::env::temp_dir().join(format!("dtfu-store-test-{}-{n}-{nanos}", std::process::id()));
            std::fs::create_dir_all(&dir).unwrap();
            Self(dir)
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn store(dir: &TempDir) -> IndexStore {
        IndexStore::open(&dir.0, MAIN_FILE).unwrap()
    }

    /// Mirrors the Swift test helper: an image, 10 bytes, mtime 1, device 1.
    fn file(path: &str, inode: u64) -> NewFile {
        NewFile { path: path.into(), kind: FileKind::Image, size: 10, mtime: 1.0, inode, device: 1 }
    }

    fn file_of(path: &str, kind: FileKind, mtime: f64) -> NewFile {
        NewFile { path: path.into(), kind, size: 10, mtime, inode: 1, device: 1 }
    }

    fn text_segment(text: &str) -> NewSegment {
        NewSegment {
            kind: SegmentKind::Chunk,
            modality: Modality::Text,
            loc: Some(1.0),
            loc_end: None,
            excerpt: Some(text.into()),
            text: Some(text.into()),
            vector: None,
        }
    }

    fn vector_segment(seed: f32) -> NewSegment {
        let mut v = vec![0.0f32; DIM];
        v[0] = seed;
        v[1] = 1.0 - seed;
        NewSegment {
            kind: SegmentKind::Image,
            modality: Modality::Image,
            loc: None,
            loc_end: None,
            excerpt: None,
            text: None,
            vector: Some(v),
        }
    }

    #[test]
    fn has_path_is_an_exact_match() {
        let dir = TempDir::new();
        let mut s = store(&dir);
        s.insert_file(&file("/d/a.png", 1), &[]).unwrap();
        assert!(s.has_path("/d/a.png").unwrap());
        assert!(!s.has_path("/d/a.pn").unwrap());
        assert!(!s.has_path("/d").unwrap());
    }

    #[test]
    fn last_place_is_the_largest_stored_page_or_chunk_loc() {
        let dir = TempDir::new();
        let mut s = store(&dir);
        let long = s.insert_file(&file_of("/d/long.pdf", FileKind::Pdf, 1.0), &[]).unwrap();
        let other = s.insert_file(&file_of("/d/other.pdf", FileKind::Pdf, 2.0), &[]).unwrap();
        assert_eq!(s.last_place(long).unwrap(), None);
        let at = |kind: SegmentKind, loc: Option<f64>| NewSegment {
            kind,
            modality: Modality::Text,
            loc,
            loc_end: None,
            excerpt: None,
            text: Some("words".into()),
            vector: None,
        };
        s.add_segments(long, &[at(SegmentKind::Page, Some(3.0)), at(SegmentKind::Page, Some(12.0))]).unwrap();
        s.add_segments(long, &[at(SegmentKind::PageImage, Some(20.0)), at(SegmentKind::Frame, Some(99.0))]).unwrap();
        s.add_segments(long, &[at(SegmentKind::Chunk, Some(7.0)), at(SegmentKind::Ocr, Some(500.0))]).unwrap();
        // Only page, page_image and chunk count (as Swift's query does); the frame and OCR rows above are ignored.
        assert_eq!(s.last_place(long).unwrap(), Some(20.0));
        assert_eq!(s.last_place(other).unwrap(), None);
    }

    fn scalar(s: &IndexStore, sql: &str) -> i64 {
        s.conn.query_row(sql, [], |r| r.get(0)).unwrap()
    }

    fn strings(v: Vec<FileRecord>) -> Vec<String> {
        v.into_iter().map(|f| f.path).collect()
    }

    #[test]
    fn schema_is_v5_with_pragmas() {
        let dir = TempDir::new();
        let s = store(&dir);
        assert_eq!(scalar(&s, "PRAGMA user_version"), 5);
        assert_eq!(scalar(&s, "PRAGMA foreign_keys"), 1);
        assert_eq!(scalar(&s, "PRAGMA busy_timeout"), 5000);
        assert_eq!(scalar(&s, "PRAGMA synchronous"), 1);
        let mode: String = s.conn.query_row("PRAGMA journal_mode", [], |r| r.get(0)).unwrap();
        assert_eq!(mode, "wal");
    }

    #[test]
    fn prune_keeps_only_chosen_folders() {
        let dir = TempDir::new();
        let mut s = store(&dir);
        s.insert_file(&file("/a/x.png", 1), &[text_segment("x")]).unwrap();
        s.insert_file(&file("/a/sub/y.png", 2), &[text_segment("y")]).unwrap();
        s.insert_file(&file("/ab/z.png", 3), &[text_segment("z")]).unwrap(); // a sibling, not inside /a
        s.insert_file(&file("/b/w.png", 4), &[text_segment("w")]).unwrap();
        assert_eq!(scalar(&s, "SELECT COUNT(*) FROM fts"), 8);
        assert_eq!(s.prune(&["/a".to_string()]).unwrap(), 2);
        assert_eq!(s.files_under(&["/a".to_string()]).unwrap().len(), 2);
        assert!(s.files_under(&["/ab".to_string(), "/b".to_string()]).unwrap().is_empty());
        // The pruned files' names (and text) left keyword search too: the two kept files keep 2 rows each.
        assert_eq!(scalar(&s, "SELECT COUNT(*) FROM fts"), 4);
        assert_eq!(s.prune(&[]).unwrap(), 2);
        assert_eq!(scalar(&s, "SELECT COUNT(*) FROM fts"), 0);
    }

    #[test]
    fn scope_limits_what_is_waiting() {
        let dir = TempDir::new();
        let mut s = store(&dir);
        s.insert_file(&file("/a/x.png", 1), &[]).unwrap();
        s.insert_file(&file("/drive/y.png", 2), &[]).unwrap(); // on a drive that is unplugged
        s.set_scope(Some(vec!["/a".into()]));
        assert_eq!(s.pending_count().unwrap(), 1);
        assert_eq!(strings(s.pending(None, None).unwrap()), vec!["/a/x.png".to_string()]);
        assert_eq!(s.counts().unwrap().by_kind_and_state["image"]["pending"], 1);
        s.set_scope(Some(vec![]));
        assert_eq!(s.pending_count().unwrap(), 0);
        s.set_scope(None);
        assert_eq!(s.pending_count().unwrap(), 2);
    }

    #[test]
    fn long_file_counts_read_left_then_finish() {
        let dir = TempDir::new();
        let mut s = store(&dir);
        let id = s.insert_file(&file_of("/a/long.pdf", FileKind::Pdf, 1.0), &[]).unwrap();
        let mut info = BTreeMap::new();
        info.insert("read_to".to_string(), "10".to_string());
        info.insert("read_end".to_string(), "100".to_string());
        s.finish(id, FileState::Partial, None, &info).unwrap();
        assert_eq!(s.unfinished().unwrap().len(), 1);
        assert_eq!(s.unfinished_count().unwrap(), (1, 90));
        assert_eq!(s.counts().unwrap().unfinished, 1);
        assert_eq!(s.info(id).unwrap()["read_end"], "100");
        s.finish(id, FileState::Done, None, &BTreeMap::new()).unwrap();
        assert_eq!(s.unfinished_count().unwrap(), (0, 0));
        assert!(s.info(id).unwrap().is_empty());
    }

    #[test]
    fn pending_orders_by_kind_then_newest() {
        let dir = TempDir::new();
        let mut s = store(&dir);
        s.insert_file(&file_of("/p/doc.txt", FileKind::Doc, 5.0), &[]).unwrap();
        s.insert_file(&file_of("/p/code.rs", FileKind::Code, 9.0), &[]).unwrap();
        s.insert_file(&file_of("/p/vid.mp4", FileKind::Video, 8.0), &[]).unwrap();
        s.insert_file(&file_of("/p/pic-old.png", FileKind::Image, 1.0), &[]).unwrap();
        s.insert_file(&file_of("/p/pic-new.png", FileKind::Image, 2.0), &[]).unwrap();
        s.insert_file(&file_of("/p/shot.png", FileKind::Screenshot, 3.0), &[]).unwrap();
        s.insert_file(&file_of("/p/a.pdf", FileKind::Pdf, 4.0), &[]).unwrap();
        s.insert_file(&file_of("/p/a.mp3", FileKind::Audio, 4.0), &[]).unwrap();
        assert_eq!(
            strings(s.pending(None, None).unwrap()),
            vec![
                "/p/shot.png", "/p/pic-new.png", "/p/pic-old.png", "/p/a.pdf", "/p/doc.txt",
                "/p/a.mp3", "/p/vid.mp4", "/p/code.rs"
            ]
        );
        assert_eq!(s.pending(Some(2), None).unwrap().len(), 2);
        assert!(s.pending(None, Some(&[])).unwrap().is_empty());
    }

    #[test]
    fn insert_keeps_fts_rowid_equal_to_segment_id() {
        let dir = TempDir::new();
        let mut s = store(&dir);
        let id = s
            .insert_file(&file("/a/x.png", 1), &[text_segment("hello world"), vector_segment(0.5), text_segment("")])
            .unwrap();
        // Name row + the one non-empty text segment have keyword rows; the vector and empty-text segments do not.
        assert_eq!(scalar(&s, "SELECT COUNT(*) FROM fts"), 2);
        // Every keyword row points at a segment that exists and belongs to this file.
        assert_eq!(
            scalar(&s, "SELECT COUNT(*) FROM fts f LEFT JOIN segments g ON g.id = f.rowid WHERE g.id IS NULL"),
            0
        );
        let text_seg: i64 = s
            .conn
            .query_row(
                "SELECT id FROM segments WHERE file = ?1 AND kind = 'chunk' AND excerpt = 'hello world'",
                [id],
                |r| r.get(0),
            )
            .unwrap();
        let body: String = s
            .conn
            .query_row("SELECT body FROM fts WHERE rowid = ?1", [text_seg], |r| r.get(0))
            .unwrap();
        assert!(body.contains("hello world"));
        let name_seg: i64 = s
            .conn
            .query_row("SELECT id FROM segments WHERE file = ?1 AND kind = 'name'", [id], |r| r.get(0))
            .unwrap();
        let name: String = s
            .conn
            .query_row("SELECT name FROM fts WHERE rowid = ?1", [name_seg], |r| r.get(0))
            .unwrap();
        assert_eq!(name, "x.png");
        assert_eq!(scalar(&s, "SELECT COUNT(*) FROM segments WHERE vec IS NOT NULL AND length(vec) = 1536"), 1);
    }

    #[test]
    fn delete_removes_fts_rows_then_cascades_to_segments() {
        let dir = TempDir::new();
        let mut s = store(&dir);
        let id = s.insert_file(&file("/a/x.png", 1), &[text_segment("alpha"), vector_segment(0.2)]).unwrap();
        let other = s.insert_file(&file("/a/y.png", 2), &[text_segment("beta")]).unwrap();
        assert_eq!(scalar(&s, "SELECT COUNT(*) FROM fts"), 4);
        s.delete_file(id).unwrap();
        assert_eq!(scalar(&s, &format!("SELECT COUNT(*) FROM files WHERE id = {id}")), 0);
        assert_eq!(scalar(&s, &format!("SELECT COUNT(*) FROM segments WHERE file = {id}")), 0);
        assert_eq!(scalar(&s, "SELECT COUNT(*) FROM fts"), 2);
        assert_eq!(scalar(&s, &format!("SELECT COUNT(*) FROM segments WHERE file = {other}")), 2);
        assert_eq!(s.files_under(&["/a".to_string()]).unwrap().len(), 1);
    }

    #[test]
    fn move_detection_matches_identity_with_bit_cast_inode() {
        let dir = TempDir::new();
        let mut s = store(&dir);
        let big_inode = 0xFFFF_FFFF_FFFF_FFF0u64; // above i64::MAX: stored bit-cast
        let id = s.insert_file(&NewFile { inode: big_inode, ..file("/a/x.png", 0) }, &[]).unwrap();
        let hit = s.find_by_identity(1, big_inode, 10, 1.0005).unwrap().expect("same identity within 1 ms");
        assert_eq!(hit.id, id);
        assert_eq!(hit.inode, big_inode);
        assert!(s.find_by_identity(1, big_inode, 10, 1.01).unwrap().is_none(), "mtime differs by more than 1 ms");
        assert!(s.find_by_identity(2, big_inode, 10, 1.0).unwrap().is_none(), "other device");
        assert!(s.find_by_identity(1, big_inode, 11, 1.0).unwrap().is_none(), "other size");
        assert!(s.find_by_identity(1, 7, 10, 1.0).unwrap().is_none(), "other inode");
    }

    #[test]
    fn moved_file_keeps_its_segments_and_updates_name_keywords() {
        let dir = TempDir::new();
        let mut s = store(&dir);
        let id = s.insert_file(&file("/a/x.png", 1), &[vector_segment(0.3)]).unwrap();
        s.move_file(id, "/b/renamed.png").unwrap();
        assert_eq!(strings(s.files_under(&["/b".to_string()]).unwrap()), vec!["/b/renamed.png".to_string()]);
        assert_eq!(scalar(&s, &format!("SELECT COUNT(*) FROM segments WHERE file = {id} AND vec IS NOT NULL")), 1);
        let name: String = s
            .conn
            .query_row(
                "SELECT f.name FROM fts f JOIN segments g ON g.id = f.rowid WHERE g.file = ?1 AND g.kind = 'name'",
                [id],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(name, "renamed.png");
    }

    #[test]
    fn change_detection_uses_size_mtime_and_kind() {
        let dir = TempDir::new();
        let mut s = store(&dir);
        let id = s.insert_file(&file_of("/a/x.pdf", FileKind::Pdf, 1.0), &[]).unwrap();
        let rec = s.files_under(&["/a".to_string()]).unwrap().remove(0);
        assert_eq!(rec.id, id);
        assert!(IndexStore::is_unchanged(&rec, &file_of("/a/x.pdf", FileKind::Pdf, 1.0005)));
        assert!(!IndexStore::is_unchanged(&rec, &file_of("/a/x.pdf", FileKind::Pdf, 1.01)));
        assert!(!IndexStore::is_unchanged(&rec, &file_of("/a/x.pdf", FileKind::Doc, 1.0)));
        let bigger = NewFile { size: 11, ..file_of("/a/x.pdf", FileKind::Pdf, 1.0) };
        assert!(!IndexStore::is_unchanged(&rec, &bigger));
    }

    #[test]
    fn requeue_drops_segments_but_keeps_name_row() {
        let dir = TempDir::new();
        let mut s = store(&dir);
        let id = s.insert_file(&file("/a/x.png", 1), &[text_segment("hi"), vector_segment(0.4)]).unwrap();
        s.finish(id, FileState::Done, None, &BTreeMap::new()).unwrap();
        s.requeue(id, None).unwrap();
        assert_eq!(strings(s.pending(None, None).unwrap()), vec!["/a/x.png".to_string()]);
        assert_eq!(scalar(&s, &format!("SELECT COUNT(*) FROM segments WHERE file = {id}")), 1);
        assert_eq!(scalar(&s, "SELECT COUNT(*) FROM fts"), 1);
    }

    #[test]
    fn fingerprint_mismatch_is_refused_or_reset() {
        let dir = TempDir::new();
        let mut s = store(&dir);
        assert_eq!(s.adopt("src-a", "fp-1", Mismatch::Refuse).unwrap(), Adoption::Fresh);
        assert_eq!(s.adopt("src-a", "fp-1", Mismatch::Refuse).unwrap(), Adoption::Same);
        let id = s.insert_file(&file("/a/x.png", 1), &[vector_segment(0.6)]).unwrap();
        s.finish(id, FileState::Done, None, &BTreeMap::new()).unwrap();
        assert!(s.adopt("src-a", "fp-2", Mismatch::Refuse).is_err());
        assert!(s.adopt("src-b", "fp-1", Mismatch::Refuse).is_err());
        assert_eq!(s.meta("fingerprint").unwrap().as_deref(), Some("fp-1"), "refused: nothing changed");
        assert_eq!(scalar(&s, "SELECT COUNT(*) FROM segments WHERE vec IS NOT NULL"), 1);
        assert_eq!(s.adopt("src-a", "fp-2", Mismatch::Reset).unwrap(), Adoption::Reset);
        assert_eq!(scalar(&s, "SELECT COUNT(*) FROM segments WHERE vec IS NOT NULL"), 0);
        assert_eq!(strings(s.pending(None, None).unwrap()), vec!["/a/x.png".to_string()]);
        assert_eq!(s.meta("fingerprint").unwrap().as_deref(), Some("fp-2"));
        assert_eq!(s.adopt("src-a", "fp-2", Mismatch::Refuse).unwrap(), Adoption::Same);
    }

    #[test]
    fn meta_round_trips_and_deletes() {
        let dir = TempDir::new();
        let s = store(&dir);
        assert_eq!(s.meta("k").unwrap(), None);
        s.set_meta("k", Some("v1")).unwrap();
        s.set_meta("k", Some("v2")).unwrap();
        assert_eq!(s.meta("k").unwrap().as_deref(), Some("v2"));
        s.set_meta("k", None).unwrap();
        assert_eq!(s.meta("k").unwrap(), None);
    }

    #[test]
    fn wrong_vector_size_is_rejected_and_rolls_back() {
        let dir = TempDir::new();
        let mut s = store(&dir);
        let bad = NewSegment { vector: Some(vec![0.1; 10]), ..vector_segment(0.1) };
        assert!(s.insert_file(&file("/a/x.png", 1), &[bad]).is_err());
        assert_eq!(scalar(&s, "SELECT COUNT(*) FROM files"), 0, "the file row rolled back with the segment");
        assert_eq!(scalar(&s, "SELECT COUNT(*) FROM fts"), 0);
    }

    #[test]
    fn info_is_a_json_object_of_strings() {
        let dir = TempDir::new();
        let s = store(&dir);
        let id = s.conn.execute("INSERT INTO files(path, kind, size, mtime, inode, device) VALUES('/x', 'doc', 1, 1, 1, 1)", []).unwrap();
        assert_eq!(id, 1);
        let mut info = BTreeMap::new();
        info.insert("pages".to_string(), "3".to_string());
        s.finish(1, FileState::Done, Some("ok"), &info).unwrap();
        assert_eq!(s.info(1).unwrap(), info);
        assert_eq!(s.info(99).unwrap(), BTreeMap::new());
    }

    // MARK: content duplicates

    fn sized(path: &str, kind: FileKind, size: i64) -> NewFile {
        NewFile { path: path.into(), kind, size, mtime: 1.0, inode: 1, device: 1 }
    }

    #[test]
    fn done_files_with_sizes_filters_by_state_kind_and_size() {
        let dir = TempDir::new();
        let mut s = store(&dir);
        let done = s.insert_file(&sized("/a/doc", FileKind::Doc, 10), &[text_segment("x")]).unwrap();
        s.finish(done, FileState::Done, None, &BTreeMap::new()).unwrap();
        let other_size = s.insert_file(&sized("/a/big", FileKind::Doc, 20), &[text_segment("y")]).unwrap();
        s.finish(other_size, FileState::Done, None, &BTreeMap::new()).unwrap();
        let code = s.insert_file(&sized("/a/code.rs", FileKind::Code, 10), &[text_segment("z")]).unwrap();
        s.finish(code, FileState::Done, None, &BTreeMap::new()).unwrap();
        s.insert_file(&sized("/a/pending", FileKind::Doc, 10), &[text_segment("w")]).unwrap();

        let sizes: std::collections::HashSet<i64> = [10].into_iter().collect();
        assert_eq!(strings(s.done_files_with_sizes(&sizes).unwrap()), vec!["/a/doc".to_string()]);
        let none: std::collections::HashSet<i64> = std::collections::HashSet::new();
        assert!(s.done_files_with_sizes(&none).unwrap().is_empty());
    }

    #[test]
    fn set_info_value_keeps_other_keys() {
        let dir = TempDir::new();
        let s = store(&dir);
        let id = s.conn.execute("INSERT INTO files(path, kind, size, mtime, inode, device) VALUES('/x', 'doc', 1, 1, 1, 1)", []).unwrap();
        assert_eq!(id, 1);
        s.set_info_value(1, "content_hash", "abc").unwrap();
        assert_eq!(s.info(1).unwrap().get("content_hash").map(String::as_str), Some("abc"));
        let mut info = BTreeMap::new();
        info.insert("pages".to_string(), "3".to_string());
        s.conn.execute("UPDATE files SET info = ?1 WHERE id = 1", [serde_json::to_string(&info).unwrap()]).unwrap();
        s.set_info_value(1, "content_size", "1").unwrap();
        let got = s.info(1).unwrap();
        assert_eq!(got.get("pages").map(String::as_str), Some("3"));
        assert_eq!(got.get("content_size").map(String::as_str), Some("1"));
    }

    #[test]
    fn copy_indexed_content_copies_rows_and_survives_source_delete() {
        let dir = TempDir::new();
        let mut s = store(&dir);
        let src = s.insert_file(&sized("/a/src", FileKind::Image, 10), &[text_segment("needle"), vector_segment(0.3)]).unwrap();
        s.finish(src, FileState::Done, Some("ok"), &BTreeMap::new()).unwrap();
        let dst = s.insert_file(&sized("/a/dst", FileKind::Image, 10), &[]).unwrap();
        s.requeue(dst, None).unwrap();
        let mut info = BTreeMap::new();
        info.insert("content_hash".to_string(), "h".to_string());

        assert!(s.copy_indexed_content(src, dst, &info).unwrap());
        assert_eq!(scalar(&s, &format!("SELECT COUNT(*) FROM segments WHERE file = {dst} AND kind != 'name'")), 2);
        assert_eq!(scalar(&s, &format!("SELECT COUNT(*) FROM segments WHERE file = {dst} AND vec IS NOT NULL")), 1);
        assert_eq!(scalar(&s, &format!("SELECT COUNT(*) FROM segments WHERE file = {dst} AND kind = 'name'")), 1, "the name row stays with the target");
        let matched: i64 = s.conn.query_row(
            &format!("SELECT COUNT(*) FROM fts JOIN segments g ON g.id = fts.rowid WHERE fts MATCH 'needle' AND g.file = {dst}"),
            [],
            |r| r.get(0),
        ).unwrap();
        assert_eq!(matched, 1);
        let state: String = s.conn.query_row("SELECT state FROM files WHERE id = ?1", [dst], |r| r.get(0)).unwrap();
        assert_eq!(state, "done");
        let detail: Option<String> = s.conn.query_row("SELECT detail FROM files WHERE id = ?1", [dst], |r| r.get(0)).unwrap();
        assert_eq!(detail.as_deref(), Some("ok"));
        assert_eq!(s.info(dst).unwrap(), info);

        s.delete_file(src).unwrap();
        assert_eq!(scalar(&s, &format!("SELECT COUNT(*) FROM segments WHERE file = {dst} AND kind != 'name'")), 2, "the copy survives the source");
        assert_eq!(scalar(&s, &format!("SELECT COUNT(*) FROM fts WHERE rowid IN (SELECT id FROM segments WHERE file = {dst})")), 2, "its own name row plus the copied body");
    }

    #[test]
    fn copy_indexed_content_refuses_wrong_states() {
        let dir = TempDir::new();
        let mut s = store(&dir);
        let pending_src = s.insert_file(&sized("/a/p", FileKind::Image, 10), &[text_segment("a")]).unwrap();
        let dst = s.insert_file(&sized("/a/d", FileKind::Image, 10), &[]).unwrap();
        s.requeue(dst, None).unwrap();
        let none = BTreeMap::new();
        assert!(!s.copy_indexed_content(pending_src, dst, &none).unwrap(), "source must be done");
        s.finish(pending_src, FileState::Done, None, &none).unwrap();
        let done_target = s.insert_file(&sized("/a/t", FileKind::Image, 10), &[text_segment("b")]).unwrap();
        s.finish(done_target, FileState::Done, None, &none).unwrap();
        let before = scalar(&s, "SELECT COUNT(*) FROM segments");
        assert!(!s.copy_indexed_content(pending_src, done_target, &none).unwrap(), "target must be pending");
        assert_eq!(scalar(&s, "SELECT COUNT(*) FROM segments"), before, "nothing changed");
        assert!(!s.copy_indexed_content(pending_src, 999, &none).unwrap(), "missing target");
    }
}
