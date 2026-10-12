//! `Searcher::refresh` must leave a cached searcher indistinguishable from a fresh one: after random index
//! mutations (modelled on what the indexer does) the hits of 30 queries are bitwise equal.

use std::collections::HashSet;

use digup_core::{FileKind, DIM};
use digup_search::searcher::OwnedSearcher;
use digup_search::{SearchHit, SearchOptions, Searcher};
use rusqlite::{params, Connection};

struct Rng(u64);
impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }
    fn below(&mut self, n: usize) -> usize {
        (self.next() >> 11) as usize % n
    }
    fn unit(&mut self) -> f64 {
        (self.next() >> 11) as f64 / (1u64 << 53) as f64 * 2.0 - 1.0
    }
}

fn random_vec(rng: &mut Rng) -> Vec<f32> {
    let v: Vec<f32> = (0..DIM).map(|_| rng.unit() as f32).collect();
    normalize(v)
}

fn normalize(mut v: Vec<f32>) -> Vec<f32> {
    let n = v.iter().map(|x| x * x).sum::<f32>().sqrt().max(1e-9);
    v.iter_mut().for_each(|x| *x /= n);
    v
}

const WORDS: [&str; 8] = ["alpha", "bravo", "charlie", "delta", "echo", "foxtrot", "golf", "hotel"];

struct World {
    w: Connection,
    clock: f64,
    next_path: usize,
}

impl World {
    fn tick(&mut self) -> f64 {
        self.clock += 1.0;
        self.clock
    }

    fn add_segment(&mut self, rng: &mut Rng, file: i64, kind: &str, modality: &str, loc: f64) {
        let blob = digup_store::encode_f16(&random_vec(rng));
        self.w
            .execute(
                "INSERT INTO segments(file, kind, modality, loc, vec) VALUES(?1, ?2, ?3, ?4, ?5)",
                params![file, kind, modality, loc, blob],
            )
            .unwrap();
        let id = self.w.last_insert_rowid();
        let text = format!("{} {} {}", WORDS[rng.below(8)], WORDS[rng.below(8)], WORDS[rng.below(8)]);
        self.w.execute("INSERT INTO fts(rowid, name, body) VALUES(?1, '', ?2)", params![id, text]).unwrap();
    }

    fn fill(&mut self, rng: &mut Rng, file: i64, kind: &str, count: usize, from: usize) {
        for i in from..from + count {
            match kind {
                "pdf" => self.add_segment(rng, file, "page", "text", (i + 1) as f64),
                "doc" => self.add_segment(rng, file, "chunk", "text", (i * 20 + 1) as f64),
                "audio" => self.add_segment(rng, file, "chunk", "audio", i as f64),
                _ => self.add_segment(rng, file, "image", "image", 0.0),
            }
        }
    }

    fn add_file(&mut self, rng: &mut Rng) -> i64 {
        let kind = ["pdf", "doc", "image", "screenshot", "audio"][rng.below(5)];
        self.next_path += 1;
        let at = self.tick();
        let pages = if kind == "pdf" && rng.below(2) == 0 { 60 + rng.below(30) } else { 5 };
        let info = format!(r#"{{"pages":"{pages}"}}"#);
        self.w
            .execute(
                "INSERT INTO files(path, kind, size, mtime, inode, device, state, info, indexed_at) \
                 VALUES(?1, ?2, 1, 1, ?3, 1, 'done', ?4, ?5)",
                params![format!("/f{}", self.next_path), kind, self.next_path as i64, info, at],
            )
            .unwrap();
        let id = self.w.last_insert_rowid();
        // The name segment has no vector.
        self.w.execute("INSERT INTO segments(file, kind, modality) VALUES(?1, 'name', 'none')", [id]).unwrap();
        let n = match kind {
            "pdf" => 1 + rng.below(if pages > 50 { 40 } else { 6 }),
            "doc" => 1 + rng.below(8),
            "audio" => 1 + rng.below(3),
            _ => 1,
        };
        self.fill(rng, id, kind, n, 0);
        id
    }

    fn files(&self) -> Vec<(i64, String)> {
        let mut st = self.w.prepare("SELECT id, kind FROM files ORDER BY id").unwrap();
        st.query_map([], |r| Ok((r.get(0)?, r.get(1)?))).unwrap().map(Result::unwrap).collect()
    }

    fn delete_segments(&self, file: i64) {
        self.w.execute("DELETE FROM fts WHERE rowid IN (SELECT id FROM segments WHERE file = ?1)", [file]).unwrap();
        self.w.execute("DELETE FROM segments WHERE file = ?1 AND kind != 'name'", [file]).unwrap();
    }

    /// One random mutation; returns its name.
    fn mutate(&mut self, rng: &mut Rng) -> &'static str {
        let files = self.files();
        if files.len() < 5 {
            self.add_file(rng);
            return "add";
        }
        let (id, kind) = files[rng.below(files.len())].clone();
        match rng.below(8) {
            0 | 1 => {
                self.add_file(rng);
                "add"
            }
            2 => {
                self.delete_segments(id);
                self.w.execute("DELETE FROM segments WHERE file = ?1", [id]).unwrap();
                self.w.execute("DELETE FROM files WHERE id = ?1", [id]).unwrap();
                "delete"
            }
            3 => {
                // Re-embed: requeue, then new segments (ids may be reused), then finish with a new stamp.
                self.delete_segments(id);
                let at = self.tick();
                let n = 1 + rng.below(6);
                self.fill(rng, id, &kind, n, 0);
                self.w.execute("UPDATE files SET indexed_at = ?1 WHERE id = ?2", params![at, id]).unwrap();
                "reembed"
            }
            4 => {
                // A later step of a long file: more segments, pages up; sometimes the file row is not touched.
                let target = files.iter().filter(|(_, k)| k == "pdf" || k == "doc").nth(rng.below(3)).cloned();
                if let Some((fid, k)) = target {
                    let have: i64 = self
                        .w
                        .query_row("SELECT count(*) FROM segments WHERE file = ?1 AND kind != 'name'", [fid], |r| r.get(0))
                        .unwrap();
                    let n = 1 + rng.below(12);
                    self.fill(rng, fid, &k, n, have as usize);
                    // `IndexStore::add_segments` bumps `indexed_at` in the same transaction; so does this.
                    let at = self.tick();
                    if rng.below(2) == 0 {
                        let pages = 55 + rng.below(40);
                        self.w
                            .execute(
                                "UPDATE files SET info = ?1, indexed_at = ?2 WHERE id = ?3",
                                params![format!(r#"{{"pages":"{pages}"}}"#), at, fid],
                            )
                            .unwrap();
                    } else {
                        self.w.execute("UPDATE files SET indexed_at = ?1 WHERE id = ?2", params![at, fid]).unwrap();
                    }
                }
                "step"
            }
            5 => {
                // Kind change (re-classified): the file's rows are kept, the kind decides the scale rule.
                let new = ["pdf", "doc", "image"][rng.below(3)];
                let at = self.tick();
                self.w.execute("UPDATE files SET kind = ?1, indexed_at = ?2 WHERE id = ?3", params![new, at, id]).unwrap();
                "kind"
            }
            7 => {
                // The file holding the highest segment ids goes away and rows are added to another file in the
                // same window: SQLite reuses the freed ids, so new rows get ids at or below the last loaded one.
                let top: Option<i64> = self.w.query_row("SELECT file FROM segments ORDER BY id DESC LIMIT 1", [], |r| r.get(0)).ok();
                if let Some(top) = top {
                    self.delete_segments(top);
                    self.w.execute("DELETE FROM segments WHERE file = ?1", [top]).unwrap();
                    self.w.execute("DELETE FROM files WHERE id = ?1", [top]).unwrap();
                    if let Some((fid, k)) = self.files().into_iter().find(|(_, k)| k == "doc" || k == "pdf") {
                        let have: i64 = self
                            .w
                            .query_row("SELECT count(*) FROM segments WHERE file = ?1 AND kind != 'name'", [fid], |r| r.get(0))
                            .unwrap();
                        let count = 1 + rng.below(4);
                        self.fill(rng, fid, &k, count, have as usize);
                        let at = self.tick();
                        self.w.execute("UPDATE files SET indexed_at = ?1 WHERE id = ?2", params![at, fid]).unwrap();
                    }
                }
                "delete-top"
            }
            _ => {
                // Info only (pages), no segment change.
                let at = self.tick();
                let pages = 3 + rng.below(90);
                self.w
                    .execute(
                        "UPDATE files SET info = ?1, indexed_at = ?2 WHERE id = ?3",
                        params![format!(r#"{{"pages":"{pages}"}}"#), at, id],
                    )
                    .unwrap();
                "info"
            }
        }
    }
}

fn same(a: &[SearchHit], b: &[SearchHit], what: &str) -> usize {
    assert_eq!(a.len(), b.len(), "{what}: hit count");
    for (x, y) in a.iter().zip(b) {
        assert_eq!(x.file_id, y.file_id, "{what}: file");
        assert_eq!(x.score.to_bits(), y.score.to_bits(), "{what}: score {} vs {}", x.score, y.score);
        assert_eq!(x.z.map(f64::to_bits), y.z.map(f64::to_bits), "{what}: z");
        assert_eq!(x.cosine.map(f64::to_bits), y.cosine.map(f64::to_bits), "{what}: cosine");
        assert_eq!(x.segment_id, y.segment_id, "{what}: segment");
        assert_eq!(x.keyword_rank, y.keyword_rank, "{what}: keyword rank");
        assert_eq!(x.path, y.path, "{what}: path");
    }
    a.len()
}

#[test]
fn refresh_matches_a_fresh_searcher_over_random_mutations() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("index.sqlite");
    let writer = digup_store::open(&path).unwrap();
    let mut world = World { w: writer, clock: 1000.0, next_path: 0 };
    let mut rng = Rng(0x9E37_79B9_7F4A_7C15);
    for _ in 0..25 {
        world.add_file(&mut rng);
    }

    // Two cached searchers: the default fallback fraction and one that never falls back for size.
    let mut default: OwnedSearcher = Searcher::with_owned_connection(digup_store::open(&path).unwrap(), false).unwrap();
    let mut eager: OwnedSearcher = Searcher::with_owned_connection(digup_store::open(&path).unwrap(), false).unwrap();
    let (mut full_default, mut incr_default, mut incr_eager, mut full_eager) = (0, 0, 0, 0);
    let mut kinds_seen: HashSet<&str> = HashSet::new();
    let mut compared_hits = 0usize;
    let mut queries = 0usize;

    let steps = 240;
    for step in 0..steps {
        for _ in 0..1 + rng.below(3) {
            kinds_seen.insert(world.mutate(&mut rng));
        }
        let s = default.refresh().unwrap();
        if s.full { full_default += 1 } else { incr_default += 1 }
        let s = eager.refresh_with_threshold(10.0).unwrap();
        if s.full { full_eager += 1 } else { incr_eager += 1 }
        assert!(default.is_current().unwrap() && eager.is_current().unwrap());

        let fresh = Searcher::new(&world.w).unwrap();
        assert_eq!(fresh.vector_count(), default.vector_count(), "step {step}");
        assert_eq!(fresh.vector_count(), eager.vector_count(), "step {step}");

        // Query vectors: noisy copies of stored vectors (so results stand out), and random ones.
        let stored: Vec<Vec<u8>> = {
            let mut st = world.w.prepare("SELECT vec FROM segments WHERE vec IS NOT NULL ORDER BY random() LIMIT 20").unwrap();
            st.query_map([], |r| r.get(0)).unwrap().map(Result::unwrap).collect()
        };
        for q in 0..30 {
            let vector: Vec<f32> = if q % 3 == 2 || stored.is_empty() {
                random_vec(&mut rng)
            } else {
                let base = digup_store::decode_f16(&stored[rng.below(stored.len())]).unwrap();
                normalize(base.iter().map(|x| x.to_f32() + 0.02 * rng.unit() as f32).collect())
            };
            let mut opts = SearchOptions { limit: 8, ..SearchOptions::default() };
            let text = if q % 6 == 0 { format!("{} {}", WORDS[rng.below(8)], WORDS[rng.below(8)]) } else { String::new() };
            if q % 5 == 1 {
                opts.kinds = Some(HashSet::from([FileKind::Pdf, FileKind::Image]));
            }
            if q % 10 == 0 {
                opts.z_gate = 0.5;
            }
            let expected = fresh.search(&vector, &text, &opts).unwrap();
            compared_hits += same(&expected, &default.search(&vector, &text, &opts).unwrap(), &format!("default step {step} q {q}"));
            if step % 2 == 0 {
                same(&expected, &eager.search(&vector, &text, &opts).unwrap(), &format!("eager step {step} q {q}"));
            }
            queries += 1;
        }
    }
    eprintln!(
        "steps {steps}, queries {queries} x2 searchers, hits compared {compared_hits}; default: {incr_default} incremental / {full_default} full; eager: {incr_eager} incremental / {full_eager} full; mutations {kinds_seen:?}"
    );
    assert!(compared_hits > 1000, "the queries must find something");
    assert!(incr_eager > steps * 3 / 4, "the incremental path must be what ran: {incr_eager} of {steps}");
    assert!(incr_default > 0 && full_default > 0, "both paths of the default searcher must be exercised");
    assert_eq!(kinds_seen.len(), 7);
}
