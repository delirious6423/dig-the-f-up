//! Local usage log for Dig-the-f-up: which indexed files the user opens, and a frecency score per file.
//!
//! The data lives in its own SQLite file (the caller picks the path, e.g. `<index_dir>/usage.sqlite`), never in the
//! index DBs, so the index schema is untouched. Nothing leaves the machine.
//!
//! Frecency model: each open adds 1 to a score that decays exponentially with a half-life of [`HALF_LIFE_SECS`]
//! (14 days). The stored pair `(score, last_update)` is the score as of `last_update`, so a read decays it to `now`
//! in O(1). Every time-dependent function takes `now` (unix seconds, `f64`) explicitly, so tests are deterministic.

use std::collections::HashMap;
use std::path::Path;
use std::time::Duration;

use anyhow::{ensure, Context, Result};
use rusqlite::{params, params_from_iter, Connection, OptionalExtension};

/// Half-life of a file's frecency score, in seconds (14 days).
pub const HALF_LIFE_SECS: f64 = 14.0 * 86_400.0;

/// Largest bonus [`boost`] can return. It is on the same scale as the `1.0` kind boost that
/// `dtfu-search`'s `Searcher::search` adds to a fused score for files whose kind the query names (`kind_hints`):
/// a frequently opened file gets at most half of a kind match.
pub const MAX_BOOST: f64 = 0.5;

/// Frecency score at which [`boost`] reaches about 63% of [`MAX_BOOST`] (`1 - exp(-1)`).
const BOOST_SCALE: f64 = 3.0;

/// Width of one frecency tier in [`priority_key`], in natural-log units of `1 + score`.
const TIER_STEP: f64 = 0.25;

/// Age, in seconds, at which the recency term of [`priority_key`] has halved (7 days).
const RECENCY_SCALE_SECS: f64 = 7.0 * 86_400.0;

/// Schema version stored in `PRAGMA user_version`.
const SCHEMA_VERSION: i64 = 1;

/// Largest number of bound parameters per `IN (...)` list (SQLite's old default limit is 999).
const CHUNK: usize = 900;

const BUSY_TIMEOUT: Duration = Duration::from_secs(5);

/// SQLite-backed usage log. Each `UsageStore` owns one connection; several handles may share a file.
pub struct UsageStore {
    conn: Connection,
}

/// Decays `score` (as of `from`) to `to`. A `to` before `from` (clock skew) gives no decay.
fn decay(score: f64, from: f64, to: f64) -> f64 {
    if to <= from {
        score
    } else {
        score * 0.5f64.powf((to - from) / HALF_LIFE_SECS)
    }
}

impl UsageStore {
    /// Opens or creates the usage file at `path`, enables WAL and a busy timeout, and creates the schema if needed.
    pub fn open(path: &Path) -> Result<Self> {
        let conn = Connection::open(path).with_context(|| format!("opening usage store {}", path.display()))?;
        conn.busy_timeout(BUSY_TIMEOUT)?;
        let _mode: String = conn.query_row("PRAGMA journal_mode = WAL", [], |r| r.get(0))?;
        conn.pragma_update(None, "synchronous", "NORMAL")?;
        let store = UsageStore { conn };
        store.immediate(|c| {
            let version: i64 = c.query_row("PRAGMA user_version", [], |r| r.get(0))?;
            ensure!(version <= SCHEMA_VERSION, "usage store schema {version} is newer than {SCHEMA_VERSION}");
            if version < SCHEMA_VERSION {
                c.execute_batch(
                    "CREATE TABLE IF NOT EXISTS usage (
                        path        TEXT PRIMARY KEY,
                        opens       INTEGER NOT NULL,
                        last_open   REAL NOT NULL,
                        score       REAL NOT NULL,
                        last_update REAL NOT NULL
                    );",
                )?;
                c.pragma_update(None, "user_version", SCHEMA_VERSION)?;
            }
            Ok(())
        })?;
        Ok(store)
    }

    /// Records one open of `path` at `now`: opens += 1, last_open = max(last_open, now), score = decayed + 1.
    pub fn record_open(&self, path: &str, now: f64) -> Result<()> {
        ensure!(now.is_finite(), "record_open: non-finite time");
        self.immediate(|c| {
            let cur = read_row(c, path)?;
            let row = match cur {
                None => Row { path: path.to_string(), opens: 1, last_open: now, score: 1.0, last_update: now },
                Some(r) => {
                    let at = now.max(r.last_update);
                    Row {
                        path: path.to_string(),
                        opens: r.opens + 1,
                        last_open: r.last_open.max(now),
                        score: decay(r.score, r.last_update, at) + 1.0,
                        last_update: at,
                    }
                }
            };
            write_row(c, &row)
        })
    }

    /// Frecency score of `path` decayed to `now`; 0.0 if the path has never been opened.
    pub fn score(&self, path: &str, now: f64) -> Result<f64> {
        let row = read_row(&self.conn, path)?;
        Ok(row.map_or(0.0, |r| decay(r.score, r.last_update, now)))
    }

    /// Frecency scores (decayed to `now`) of the given paths. Unknown paths are absent from the map.
    pub fn scores(&self, paths: &[&str], now: f64) -> Result<HashMap<String, f64>> {
        let mut out = HashMap::with_capacity(paths.len());
        for chunk in paths.chunks(CHUNK) {
            if chunk.is_empty() {
                continue;
            }
            let marks = vec!["?"; chunk.len()].join(",");
            let sql = format!("SELECT path, score, last_update FROM usage WHERE path IN ({marks})");
            let mut stmt = self.conn.prepare_cached(&sql)?;
            let rows = stmt.query_map(params_from_iter(chunk.iter()), |r| {
                Ok((r.get::<_, String>(0)?, r.get::<_, f64>(1)?, r.get::<_, f64>(2)?))
            })?;
            for row in rows {
                let (path, score, last) = row?;
                out.insert(path, decay(score, last, now));
            }
        }
        Ok(out)
    }

    /// The `limit` highest frecency scores at `now`, best first (ties broken by path).
    pub fn top(&self, limit: usize, now: f64) -> Result<Vec<(String, f64)>> {
        let mut stmt = self.conn.prepare("SELECT path, score, last_update FROM usage")?;
        let rows = stmt.query_map([], |r| {
            Ok((r.get::<_, String>(0)?, r.get::<_, f64>(1)?, r.get::<_, f64>(2)?))
        })?;
        let mut all = Vec::new();
        for row in rows {
            let (path, score, last) = row?;
            all.push((path, decay(score, last, now)));
        }
        all.sort_by(|a, b| b.1.total_cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
        all.truncate(limit);
        Ok(all)
    }

    /// Removes the usage record of `path`, if any.
    pub fn forget(&self, path: &str) -> Result<()> {
        self.conn.execute("DELETE FROM usage WHERE path = ?1", [path])?;
        Ok(())
    }

    /// Removes every usage record.
    pub fn clear(&self) -> Result<()> {
        self.conn.execute("DELETE FROM usage", [])?;
        Ok(())
    }

    /// Moves the record of a moved file from `from` to `to`. If `to` already has a record, the two are merged
    /// (opens summed, scores summed after decay to the later update). A missing `from` is a no-op.
    pub fn rename(&self, from: &str, to: &str) -> Result<()> {
        if from == to {
            return Ok(());
        }
        self.immediate(|c| {
            let Some(src) = read_row(c, from)? else {
                return Ok(());
            };
            c.execute("DELETE FROM usage WHERE path = ?1", [from])?;
            let merged = match read_row(c, to)? {
                None => Row { path: to.to_string(), ..src },
                Some(dst) => {
                    let at = src.last_update.max(dst.last_update);
                    Row {
                        path: to.to_string(),
                        opens: src.opens + dst.opens,
                        last_open: src.last_open.max(dst.last_open),
                        score: decay(src.score, src.last_update, at) + decay(dst.score, dst.last_update, at),
                        last_update: at,
                    }
                }
            };
            write_row(c, &merged)
        })
    }

    /// Deletes records whose last open is before `cutoff_secs`.
    pub fn prune_older_than(&self, cutoff_secs: f64) -> Result<()> {
        self.conn.execute("DELETE FROM usage WHERE last_open < ?1", [cutoff_secs])?;
        Ok(())
    }

    /// Number of files with a usage record.
    pub fn count(&self) -> Result<u64> {
        let n: i64 = self.conn.query_row("SELECT COUNT(*) FROM usage", [], |r| r.get(0))?;
        Ok(n as u64)
    }

    /// Runs `f` inside `BEGIN IMMEDIATE ... COMMIT` (write lock taken up front, so busy_timeout applies cleanly).
    fn immediate<T>(&self, f: impl FnOnce(&Connection) -> Result<T>) -> Result<T> {
        self.conn.execute_batch("BEGIN IMMEDIATE")?;
        match f(&self.conn) {
            Ok(v) => {
                if let Err(e) = self.conn.execute_batch("COMMIT") {
                    let _ = self.conn.execute_batch("ROLLBACK");
                    return Err(e.into());
                }
                Ok(v)
            }
            Err(e) => {
                let _ = self.conn.execute_batch("ROLLBACK");
                Err(e)
            }
        }
    }
}

struct Row {
    path: String,
    opens: i64,
    last_open: f64,
    score: f64,
    last_update: f64,
}

fn read_row(c: &Connection, path: &str) -> Result<Option<Row>> {
    let row = c
        .query_row(
            "SELECT opens, last_open, score, last_update FROM usage WHERE path = ?1",
            [path],
            |r| Ok(Row { path: path.to_string(), opens: r.get(0)?, last_open: r.get(1)?, score: r.get(2)?, last_update: r.get(3)? }),
        )
        .optional()?;
    Ok(row)
}

fn write_row(c: &Connection, r: &Row) -> Result<()> {
    c.execute(
        "INSERT INTO usage (path, opens, last_open, score, last_update) VALUES (?1, ?2, ?3, ?4, ?5)
         ON CONFLICT(path) DO UPDATE SET opens = excluded.opens, last_open = excluded.last_open,
             score = excluded.score, last_update = excluded.last_update",
        params![r.path, r.opens, r.last_open, r.score, r.last_update],
    )?;
    Ok(())
}

/// Maps a raw frecency score to a fusion bonus in `[0, MAX_BOOST)`:
/// `MAX_BOOST * (1 - exp(-score / 3))`. Monotone non-decreasing, zero for zero, negative or NaN input,
/// with diminishing returns. Add it to the fused score (see [`MAX_BOOST`]).
pub fn boost(score: f64) -> f64 {
    if score.is_nan() || score <= 0.0 {
        return 0.0;
    }
    MAX_BOOST * (1.0 - (-score / BOOST_SCALE).exp())
}

/// Indexing priority of a pending file; larger means index sooner. Frecency first, then recency.
///
/// Let `tier = floor(ln(1 + max(score, 0)) / 0.25)` (frecency bucket, 0 for never-opened files) and
/// `recency = 1 / (1 + max(now - mtime, 0) / 7 days)` in `(0, 1]` (newer mtime gives a higher value). Then
/// `key = tier + 0.5 * recency`. Because the recency term is below 0.5 and tiers are 1 apart, a higher tier always
/// wins, and within a tier the newer file wins. Non-finite inputs are treated as 0.
pub fn priority_key(score: f64, mtime: f64, now: f64) -> f64 {
    let score = if score.is_finite() { score.max(0.0) } else { 0.0 };
    let tier = ((1.0 + score).ln() / TIER_STEP).floor();
    let age = if mtime.is_finite() && now.is_finite() { (now - mtime).max(0.0) } else { 0.0 };
    let recency = 1.0 / (1.0 + age / RECENCY_SCALE_SECS);
    tier + 0.5 * recency
}

#[cfg(test)]
mod tests {
    use super::*;

    const DAY: f64 = 86_400.0;
    const T0: f64 = 1_800_000_000.0;

    fn store() -> (tempfile::TempDir, UsageStore) {
        let dir = tempfile::tempdir().unwrap();
        let s = UsageStore::open(&dir.path().join("usage.sqlite")).unwrap();
        (dir, s)
    }

    fn close(a: f64, b: f64) -> bool {
        (a - b).abs() < 1e-9
    }

    #[test]
    fn decay_halves_after_14_days() {
        let (_d, s) = store();
        s.record_open("/a", T0).unwrap();
        assert!(close(s.score("/a", T0).unwrap(), 1.0));
        assert!(close(s.score("/a", T0 + 14.0 * DAY).unwrap(), 0.5));
        assert!(close(s.score("/a", T0 + 28.0 * DAY).unwrap(), 0.25));
        // Reads never go backwards in time.
        assert!(close(s.score("/a", T0 - DAY).unwrap(), 1.0));
    }

    #[test]
    fn upsert_accumulates() {
        let (_d, s) = store();
        s.record_open("/a", T0).unwrap();
        s.record_open("/a", T0).unwrap();
        s.record_open("/a", T0).unwrap();
        assert!(close(s.score("/a", T0).unwrap(), 3.0));
        assert_eq!(s.count().unwrap(), 1);
        let opens: i64 = s.conn.query_row("SELECT opens FROM usage WHERE path='/a'", [], |r| r.get(0)).unwrap();
        assert_eq!(opens, 3);
        // One half-life later the old score (3) has halved to 1.5; the new open adds 1 on top.
        assert!(close(s.score("/a", T0 + 14.0 * DAY).unwrap(), 1.5));
        s.record_open("/a", T0 + 14.0 * DAY).unwrap();
        assert!(close(s.score("/a", T0 + 14.0 * DAY).unwrap(), 2.5));
    }

    #[test]
    fn unknown_path_scores_zero() {
        let (_d, s) = store();
        assert_eq!(s.score("/nope", T0).unwrap(), 0.0);
    }

    #[test]
    fn batched_scores_including_more_than_900_paths() {
        let (_d, s) = store();
        let paths: Vec<String> = (0..1000).map(|i| format!("/f/{i}")).collect();
        for p in &paths {
            s.record_open(p, T0).unwrap();
        }
        let mut query: Vec<&str> = paths.iter().map(String::as_str).collect();
        query.push("/missing");
        let got = s.scores(&query, T0 + 14.0 * DAY).unwrap();
        assert_eq!(got.len(), 1000);
        assert!(!got.contains_key("/missing"));
        for p in &paths {
            assert!(close(got[p], 0.5), "{p}");
        }
        assert!(s.scores(&[], T0).unwrap().is_empty());
    }

    #[test]
    fn top_orders_by_decayed_score() {
        let (_d, s) = store();
        s.record_open("/old-but-many", T0).unwrap();
        s.record_open("/old-but-many", T0).unwrap();
        s.record_open("/fresh", T0 + 60.0 * DAY).unwrap();
        let now = T0 + 60.0 * DAY;
        let top = s.top(10, now).unwrap();
        assert_eq!(top[0].0, "/fresh");
        assert!(close(top[0].1, 1.0));
        assert_eq!(top[1].0, "/old-but-many");
        assert!(close(top[1].1, 2.0 * 0.5f64.powf(60.0 / 14.0)));
        assert_eq!(s.top(1, now).unwrap().len(), 1);
    }

    #[test]
    fn rename_moves_and_merges() {
        let (_d, s) = store();
        s.record_open("/a", T0).unwrap();
        s.rename("/a", "/b").unwrap();
        assert_eq!(s.score("/a", T0).unwrap(), 0.0);
        assert!(close(s.score("/b", T0).unwrap(), 1.0));
        // Merge into an existing record.
        s.record_open("/c", T0).unwrap();
        s.record_open("/c", T0).unwrap();
        s.rename("/b", "/c").unwrap();
        assert!(close(s.score("/c", T0).unwrap(), 2.0 + 1.0));
        assert_eq!(s.count().unwrap(), 1);
        // Missing source and identical names are no-ops.
        s.rename("/gone", "/x").unwrap();
        s.rename("/c", "/c").unwrap();
        assert_eq!(s.count().unwrap(), 1);
    }

    #[test]
    fn prune_removes_only_old_records() {
        let (_d, s) = store();
        s.record_open("/old", T0).unwrap();
        s.record_open("/new", T0 + 10.0 * DAY).unwrap();
        s.prune_older_than(T0 + 5.0 * DAY).unwrap();
        assert_eq!(s.count().unwrap(), 1);
        assert!(s.score("/old", T0).unwrap() == 0.0);
        assert!(close(s.score("/new", T0 + 10.0 * DAY).unwrap(), 1.0));
    }

    #[test]
    fn forget_and_clear() {
        let (_d, s) = store();
        s.record_open("/a", T0).unwrap();
        s.record_open("/b", T0).unwrap();
        s.forget("/a").unwrap();
        assert_eq!(s.count().unwrap(), 1);
        s.clear().unwrap();
        assert_eq!(s.count().unwrap(), 0);
    }

    #[test]
    fn reopening_persists_and_sets_schema_version() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("usage.sqlite");
        {
            let s = UsageStore::open(&path).unwrap();
            s.record_open("/a", T0).unwrap();
        }
        let s = UsageStore::open(&path).unwrap();
        assert!(close(s.score("/a", T0).unwrap(), 1.0));
        let v: i64 = s.conn.query_row("PRAGMA user_version", [], |r| r.get(0)).unwrap();
        assert_eq!(v, SCHEMA_VERSION);
    }

    #[test]
    fn newer_schema_is_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("usage.sqlite");
        {
            let c = Connection::open(&path).unwrap();
            c.pragma_update(None, "user_version", 99).unwrap();
        }
        assert!(UsageStore::open(&path).is_err());
    }

    #[test]
    fn concurrent_writers_on_two_handles_do_not_error() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("usage.sqlite");
        let a = UsageStore::open(&path).unwrap();
        std::thread::scope(|scope| {
            for _ in 0..2 {
                let path = path.clone();
                scope.spawn(move || {
                    let store = UsageStore::open(&path).unwrap();
                    for _ in 0..200 {
                        store.record_open("/shared", T0).unwrap();
                    }
                });
            }
        });
        assert!(close(a.score("/shared", T0).unwrap(), 400.0));
        let opens: i64 = a.conn.query_row("SELECT opens FROM usage WHERE path='/shared'", [], |r| r.get(0)).unwrap();
        assert_eq!(opens, 400);
    }

    #[test]
    fn boost_is_monotone_and_bounded() {
        assert_eq!(boost(0.0), 0.0);
        assert_eq!(boost(-1.0), 0.0);
        assert_eq!(boost(f64::NAN), 0.0);
        let mut prev = 0.0;
        for i in 1..=200 {
            let b = boost(i as f64 * 0.25);
            assert!(b >= prev, "not monotone at {i}");
            assert!((0.0..=MAX_BOOST).contains(&b));
            prev = b;
        }
        assert!(boost(1e12) <= MAX_BOOST);
        assert!(boost(1e12) > MAX_BOOST * 0.999);
        assert!(close(boost(3.0), MAX_BOOST * (1.0 - (-1.0f64).exp())));
    }

    #[test]
    fn priority_key_orders_frecency_first_then_recency() {
        let now = T0;
        let fresh = now;
        let old = now - 365.0 * DAY;
        // Heavily used but ancient beats never-used but brand new.
        assert!(priority_key(5.0, old, now) > priority_key(0.0, fresh, now));
        // A single open beats never opened, regardless of mtime.
        assert!(priority_key(1.0, old, now) > priority_key(0.0, fresh, now));
        // Same frecency: newer mtime first.
        assert!(priority_key(2.0, fresh, now) > priority_key(2.0, old, now));
        // Higher frecency wins over a newer file with lower frecency.
        assert!(priority_key(3.0, old, now) > priority_key(2.0, fresh, now));
        // Never-opened files are ordered by recency among themselves.
        assert!(priority_key(0.0, now - DAY, now) > priority_key(0.0, now - 30.0 * DAY, now));
        // Future mtime is treated as now; non-finite input does not panic or produce NaN.
        assert!(priority_key(0.0, now + DAY, now) >= priority_key(0.0, now, now) - 1e-12);
        assert!(priority_key(f64::NAN, f64::NAN, now).is_finite());
    }
}
