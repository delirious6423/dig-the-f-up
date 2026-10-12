//! Schema migrations from v1 to v5 (port of `IndexStore.migrate`, IS:71-192). Version 0 is the `DDL` in `lib.rs`.
//! Every step runs inside one `BEGIN IMMEDIATE` transaction, sets `PRAGMA user_version` as it goes, and nothing is read
//! from disk or embedded again: the migrations work from what is stored.

use crate::index::normalize;
use anyhow::{bail, Result};
use dtfu_core::SCHEMA_VERSION;
use dtfu_text::{has_words, PageTextProblem};
use rusqlite::{params, Connection};
use std::time::Duration;

/// Keyword search's tokenizer (`IndexStore.tokenizer`, IS:28-30): marks count as letters, so they stay inside words.
const TOKENIZER: &str = "porter unicode61 remove_diacritics 2 categories 'L* N* Co M*'";
/// `IndexOptions.maxPages` and `maxChunks` (Model.swift:58-59), the limits that `migrateLongFiles` writes into `info`.
const MAX_PAGES: i64 = 5_000;
const MAX_CHUNKS: i64 = 5_000;

/// Brings an index at schema `from` (1 to 4) up to `SCHEMA_VERSION` in one transaction. A newer schema is refused.
pub fn migrate(conn: &Connection, from: i64) -> Result<()> {
    if from <= 0 {
        bail!("index schema {from} cannot be migrated");
    }
    if from > SCHEMA_VERSION {
        bail!("index schema {from} is newer than this build ({SCHEMA_VERSION})");
    }
    if from == SCHEMA_VERSION {
        return Ok(());
    }
    // Another process may be migrating the same index: wait for it (up to a minute), as Swift does.
    conn.busy_timeout(Duration::from_secs(60))?;
    let result = run(conn);
    conn.busy_timeout(Duration::from_millis(5000))?;
    result
}

fn run(conn: &Connection) -> Result<()> {
    conn.execute_batch("BEGIN IMMEDIATE")?;
    match steps(conn) {
        Ok(()) => {
            if let Err(e) = conn.execute_batch("COMMIT") {
                let _ = conn.execute_batch("ROLLBACK");
                return Err(e.into());
            }
            Ok(())
        }
        Err(e) => {
            let _ = conn.execute_batch("ROLLBACK");
            Err(e)
        }
    }
}

/// Reads the version inside the transaction (another process may have moved it) and runs each step in turn.
fn steps(conn: &Connection) -> Result<()> {
    let mut version = user_version(conn)?;
    while version != SCHEMA_VERSION {
        match version {
            1 => keyword_search(conn)?,
            2 => screenshot_text(conn)?,
            3 => vector_names(conn)?,
            4 => long_files(conn)?,
            v => bail!("index schema {v} cannot be migrated by this build"),
        }
        let next = user_version(conn)?;
        if next != version + 1 {
            bail!("migration from schema {version} stopped at {next}");
        }
        version = next;
    }
    Ok(())
}

fn user_version(conn: &Connection) -> Result<i64> {
    Ok(conn.query_row("PRAGMA user_version", [], |r| r.get(0))?)
}

/// Schema 1 → 2: keyword search with whole Bengali and Arabic words over normalized text. PDFs whose text layer came
/// out garbled (`PageTextProblem`) are queued again, to be read as pictures or by OCR.
fn keyword_search(conn: &Connection) -> Result<()> {
    conn.execute_batch(&format!(
        "CREATE VIRTUAL TABLE fts_new USING fts5(name, body, original UNINDEXED, tokenize = \"{TOKENIZER}\")"
    ))?;
    {
        let mut select = conn.prepare("SELECT rowid, name, body FROM fts")?;
        let rows = select.query_map([], |r| {
            Ok((r.get::<_, i64>(0)?, r.get::<_, Option<String>>(1)?, r.get::<_, Option<String>>(2)?))
        })?;
        let mut insert = conn.prepare("INSERT INTO fts_new(rowid, name, body, original) VALUES(?1, ?2, ?3, ?4)")?;
        for row in rows {
            let (id, name, body) = row?;
            let body = body.unwrap_or_default();
            // `IndexStore.keywordText`: the normalized body, and the text as written when normalizing changed it.
            let normalized = normalize(&body);
            let original = (normalized != body).then_some(body);
            insert.execute(params![id, normalize(&name.unwrap_or_default()), normalized, original])?;
        }
    }
    let mut garbled = Vec::new();
    {
        let mut select = conn.prepare(
            "SELECT DISTINCT s.file, fts.body FROM fts JOIN segments s ON s.id = fts.rowid JOIN files f ON f.id = s.file \
             WHERE f.kind = 'pdf' AND s.kind IN ('page', 'page_image')",
        )?;
        let rows = select.query_map([], |r| Ok((r.get::<_, i64>(0)?, r.get::<_, Option<String>>(1)?)))?;
        for row in rows {
            let (file, body) = row?;
            if PageTextProblem::check(&body.unwrap_or_default()).is_some() && !garbled.contains(&file) {
                garbled.push(file);
            }
        }
    }
    conn.execute_batch("DROP TABLE fts; ALTER TABLE fts_new RENAME TO fts; PRAGMA user_version = 2;")?;
    for file in garbled {
        requeue(conn, file)?;
    }
    Ok(())
}

/// `IndexStore.requeue(_:)`: drops a file's segments except its name row, and queues it again.
fn requeue(conn: &Connection, file: i64) -> Result<()> {
    conn.execute(
        "DELETE FROM fts WHERE rowid IN (SELECT id FROM segments WHERE file = ?1 AND kind != 'name')",
        [file],
    )?;
    conn.execute("DELETE FROM segments WHERE file = ?1 AND kind != 'name'", [file])?;
    conn.execute(
        "UPDATE files SET state = 'pending', detail = NULL, info = NULL, indexed_at = NULL WHERE id = ?1",
        [file],
    )?;
    Ok(())
}

/// Schema 2 → 3: a screenshot's OCR text is for keyword search only; its text vectors go.
fn screenshot_text(conn: &Connection) -> Result<()> {
    conn.execute_batch("UPDATE segments SET vec = NULL, modality = 'none' WHERE kind = 'ocr'; PRAGMA user_version = 3;")?;
    Ok(())
}

/// Schema 3 → 4: vectors are named by their vector version (`vectors1`), not by the llama.cpp build that made them.
fn vector_names(conn: &Connection) -> Result<()> {
    conn.execute_batch(
        "UPDATE meta SET value = replace(value, ' llama.cpp-b11461-metal ', ' llama.cpp-vectors1 ')
         WHERE key IN ('fingerprint', 'vector_source');
         PRAGMA user_version = 4;",
    )?;
    Ok(())
}

/// Schema 4 → 5: text that's mostly digits loses its vectors (`has_words`, as indexing would now). Long files that
/// were cut short by the old limits (PDF at page 30, document at chunk 40) become `partial`, from where they stopped.
fn long_files(conn: &Connection) -> Result<()> {
    let mut digits = Vec::new();
    {
        let mut select = conn.prepare(
            "SELECT s.id, coalesce(fts.original, fts.body) FROM segments s JOIN fts ON fts.rowid = s.id \
             WHERE s.kind IN ('page', 'chunk') AND s.vec IS NOT NULL",
        )?;
        let rows = select.query_map([], |r| Ok((r.get::<_, i64>(0)?, r.get::<_, Option<String>>(1)?)))?;
        for row in rows {
            let (id, text) = row?;
            if !has_words(&text.unwrap_or_default()) {
                digits.push(id);
            }
        }
    }
    for id in digits {
        conn.execute("UPDATE segments SET vec = NULL, modality = 'none' WHERE id = ?1", [id])?;
    }
    conn.execute_batch(&format!(
        "UPDATE files SET detail = 'read to page 30', info = json_set(info, '$.read_to', '30', '$.read_end',
             CAST(MIN(CAST(json_extract(info, '$.pages') AS INTEGER), {MAX_PAGES}) AS TEXT))
         WHERE kind = 'pdf' AND state = 'partial' AND info IS NOT NULL;
         UPDATE files SET state = 'partial', detail = 'read to chunk 40', info = json_set(info, '$.read_to', '40',
             '$.read_end', CAST(MIN(MAX(41, (CAST(json_extract(info, '$.chars') AS INTEGER) + 1599) / 1600),
                                    {MAX_CHUNKS}) AS TEXT))
         WHERE kind = 'doc' AND state = 'done' AND json_extract(info, '$.chunks') = '40';
         PRAGMA user_version = 5;"
    ))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::index::IndexStore;
    use std::path::{Path, PathBuf};

    /// A fresh folder for one test; removed by `Fixture::drop`.
    struct Fixture(PathBuf);

    impl Fixture {
        fn new(name: &str) -> Self {
            let nanos = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0);
            let dir = std::env::temp_dir().join(format!("dtfu-migrate-{name}-{}-{nanos}", std::process::id()));
            std::fs::create_dir_all(&dir).unwrap();
            Self(dir)
        }
        fn path(&self) -> &Path {
            &self.0
        }
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn scalar_i64(c: &Connection, sql: &str) -> i64 {
        c.query_row(sql, [], |r| r.get(0)).unwrap()
    }

    /// Paths of the files whose keyword rows match `query` (normalized the way the search normalizes it).
    fn matching_paths(c: &Connection, query: &str) -> Vec<String> {
        let phrase = format!("\"{}\"", normalize(query));
        let mut st = c
            .prepare(
                "SELECT f.path FROM fts JOIN segments s ON s.id = fts.rowid JOIN files f ON f.id = s.file \
                 WHERE fts MATCH ?1 ORDER BY f.path",
            )
            .unwrap();
        st.query_map([phrase], |r| r.get::<_, String>(0)).unwrap().map(|p| p.unwrap()).collect()
    }

    /// Schema 1 as the Swift app made it (default tokenizer, OCR text with a vector, vectors named by the build).
    const SCHEMA_ONE: &str = r#"
        CREATE TABLE meta(key TEXT PRIMARY KEY, value TEXT NOT NULL);
        CREATE TABLE files(id INTEGER PRIMARY KEY, path TEXT NOT NULL UNIQUE, kind TEXT NOT NULL,
            size INTEGER NOT NULL, mtime REAL NOT NULL, inode INTEGER NOT NULL, device INTEGER NOT NULL,
            state TEXT NOT NULL DEFAULT 'pending', detail TEXT, info TEXT, indexed_at REAL);
        CREATE TABLE segments(id INTEGER PRIMARY KEY, file INTEGER NOT NULL REFERENCES files(id) ON DELETE CASCADE,
            kind TEXT NOT NULL, modality TEXT NOT NULL, loc REAL, loc_end REAL, excerpt TEXT, vec BLOB);
        CREATE VIRTUAL TABLE fts USING fts5(name, body, tokenize = 'porter unicode61 remove_diacritics 2');
        INSERT INTO files VALUES(1, '/d/notes.txt', 'doc', 1, 1, 1, 1, 'done', NULL, NULL, 1);
        INSERT INTO segments VALUES(1, 1, 'chunk', 'text', 1, NULL, NULL, NULL);
        INSERT INTO fts(rowid, name, body) VALUES(1, '', 'قَالَ إِنَّ الْمَكْتَبَةَ تُغْلَقُ');
        INSERT INTO files VALUES(2, '/d/rent.pdf', 'pdf', 1, 1, 2, 1, 'done', NULL, NULL, 1);
        INSERT INTO segments VALUES(2, 2, 'page', 'text', 1, NULL, NULL, NULL);
        INSERT INTO fts(rowid, name, body) VALUES(2, '', 'বািড় ভাড়ার চিপ ধানমি ৭ নর রােডর িতন বডরেমর াট মািসক ২৫,০০০ টাকা ভাড়ায় দওয়া হেলা।');
        INSERT INTO files VALUES(3, '/d/shot.png', 'screenshot', 1, 1, 3, 1, 'done', NULL, NULL, 1);
        INSERT INTO segments VALUES(3, 3, 'ocr', 'text', NULL, NULL, 'Booking K7Q2LM', zeroblob(1536));
        INSERT INTO fts(rowid, name, body) VALUES(3, '', 'Booking K7Q2LM');
        INSERT INTO meta VALUES('fingerprint', 'google/embeddinggemma-2@bfcd298 llama.cpp-b11461-metal text-q8_0+mmproj-q8_0 | photo140 shot280 page280 frame70/3.0s audio30.0/25.0s');
        INSERT INTO meta VALUES('vector_source', 'google/embeddinggemma-2@bfcd298 llama.cpp-b11461-metal text-q8_0+mmproj-q8_0');
        PRAGMA user_version = 1;
    "#;

    #[test]
    fn schema_one_migrates_to_the_current_schema() {
        let folder = Fixture::new("v1");
        {
            let c = Connection::open(folder.path().join("index.sqlite")).unwrap();
            c.execute_batch(SCHEMA_ONE).unwrap();
        }
        let store = IndexStore::open(folder.path(), "index.sqlite").unwrap();
        let c = store.connection();
        assert_eq!(scalar_i64(c, "PRAGMA user_version"), SCHEMA_VERSION);
        // Schema 2: whole Arabic words, normalized; the search finds the note.
        assert_eq!(matching_paths(c, "المكتبة"), ["/d/notes.txt"]);
        // Schema 2: a garbled PDF is queued again, to be read as a picture.
        assert_eq!(store.pending(None, None).unwrap().iter().map(|f| f.path.as_str()).collect::<Vec<_>>(), ["/d/rent.pdf"]);
        // Schema 3: the screenshot's words are for keyword search only.
        assert_eq!(scalar_i64(c, "SELECT COUNT(vec) FROM segments WHERE kind = 'ocr'"), 0);
        assert_eq!(matching_paths(c, "K7Q2LM"), ["/d/shot.png"]);
        // Schema 4: the vectors are named by their version, not by the llama.cpp build that made them.
        let named = "google/embeddinggemma-2@bfcd298 llama.cpp-vectors1 text-q8_0+mmproj-q8_0";
        assert_eq!(store.meta("vector_source").unwrap().as_deref(), Some(named));
        assert!(store.meta("fingerprint").unwrap().unwrap().starts_with(&format!("{named} | ")));
        // Schema 5: short files are left as they were.
        assert_eq!(store.unfinished_count().unwrap(), (0, 0));
    }

    #[test]
    fn schema_four_leaves_a_long_pdf_partial_from_page_thirty() {
        let folder = Fixture::new("v4");
        {
            let c = Connection::open(folder.path().join("index.sqlite")).unwrap();
            c.execute_batch(
                "CREATE TABLE meta(key TEXT PRIMARY KEY, value TEXT NOT NULL);
                 CREATE TABLE files(id INTEGER PRIMARY KEY, path TEXT NOT NULL UNIQUE, kind TEXT NOT NULL,
                    size INTEGER NOT NULL, mtime REAL NOT NULL, inode INTEGER NOT NULL, device INTEGER NOT NULL,
                    state TEXT NOT NULL DEFAULT 'pending', detail TEXT, info TEXT, indexed_at REAL);
                 CREATE TABLE segments(id INTEGER PRIMARY KEY, file INTEGER NOT NULL REFERENCES files(id) ON DELETE CASCADE,
                    kind TEXT NOT NULL, modality TEXT NOT NULL, loc REAL, loc_end REAL, excerpt TEXT, vec BLOB);
                 CREATE VIRTUAL TABLE fts USING fts5(name, body, original UNINDEXED, tokenize = 'porter unicode61 remove_diacritics 2');
                 INSERT INTO files VALUES(1, '/d/long.pdf', 'pdf', 1, 1, 1, 1, 'partial', 'read to page 30', '{\"pages\":\"80\"}', 1);
                 PRAGMA user_version = 4;",
            )
            .unwrap();
        }
        let store = IndexStore::open(folder.path(), "index.sqlite").unwrap();
        let c = store.connection();
        assert_eq!(scalar_i64(c, "PRAGMA user_version"), SCHEMA_VERSION);
        assert_eq!(store.unfinished_count().unwrap(), (1, 50));
        assert_eq!(store.info(1).unwrap().get("read_to").map(String::as_str), Some("30"));
        assert_eq!(store.info(1).unwrap().get("read_end").map(String::as_str), Some("80"));
    }

    #[test]
    fn newer_schema_is_refused() {
        let c = Connection::open_in_memory().unwrap();
        c.execute_batch("PRAGMA user_version = 6").unwrap();
        assert!(migrate(&c, 6).is_err());
    }
}
