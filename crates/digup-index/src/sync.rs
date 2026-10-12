//! Sync: crawl the roots and bring the store in line (docs/spec-pipeline.md section 2.2).
//! Port of Swift `Indexer.sync` (Indexer.swift:86-129).

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

use anyhow::Result;
use digup_core::FileKind;
use digup_crawl::{crawl, Candidate, CrawlOptions};
use digup_store::index::{IndexStore, NewFile};

use crate::options::IndexOptions;

/// What a sync did.
#[derive(Debug, Default, Clone)]
pub struct SyncSummary {
    pub added: usize,
    pub changed: usize,
    pub moved: usize,
    pub removed: usize,
    pub unchanged: usize,
    /// Ids of added + changed files (the "new files" set).
    pub queued: Vec<i64>,
    pub skipped: HashMap<String, usize>,
    pub skipped_folders: HashMap<String, usize>,
    pub datasets: HashMap<String, usize>,
}

const MTIME_TOLERANCE: f64 = 0.001;

fn root_string(p: &Path) -> String {
    let s = p.to_string_lossy().into_owned();
    if s.len() > 1 && s.ends_with('/') {
        s.trim_end_matches('/').to_string()
    } else {
        s
    }
}

fn to_new_file(c: &Candidate) -> Result<NewFile> {
    let kind = FileKind::parse(c.kind.as_str())
        .ok_or_else(|| anyhow::anyhow!("unknown file kind {}", c.kind.as_str()))?;
    Ok(NewFile { path: c.path.clone(), kind, size: c.size, mtime: c.mtime, inode: c.inode, device: c.device })
}

/// Crawls `roots` and reconciles the index: new files are queued, changed ones re-queued, moved ones re-pathed
/// (no re-embedding), and files that are gone or newly excluded are removed. Rows under a root that no longer exists
/// on disk are kept. Each store call commits on its own (the store API has no outer transaction).
pub fn sync(store: &mut IndexStore, roots: &[PathBuf], opts: &IndexOptions, code: bool) -> Result<SyncSummary> {
    let copts = CrawlOptions {
        code,
        include_code: opts.include_code,
        include_datasets: opts.include_datasets,
        max_code_bytes: opts.max_code_bytes as i64,
        max_notebook_bytes: opts.max_notebook_bytes as i64,
        excluded_folders: opts.excluded_folders.iter().map(|p| root_string(p)).collect(),
        ..CrawlOptions::default()
    };
    let report = crawl(roots, &copts);
    let root_strings: Vec<String> = roots.iter().map(|r| root_string(r)).collect();
    let existing = store.files_under(&root_strings)?;

    let mut by_path: HashMap<&str, usize> = HashMap::new();
    let mut by_inode: HashMap<(u64, u64), usize> = HashMap::new();
    for (i, row) in existing.iter().enumerate() {
        by_path.insert(row.path.as_str(), i);
        if row.inode != 0 {
            by_inode.insert((row.device, row.inode), i);
        }
    }
    let seen: HashSet<&str> = report.candidates.iter().map(|c| c.path.as_str()).collect();
    let mut moved_from: HashSet<&str> = HashSet::new();
    let mut summary = SyncSummary {
        skipped: report.skipped.clone(),
        skipped_folders: report.skipped_folders.clone(),
        datasets: report.datasets.clone(),
        ..Default::default()
    };

    for c in &report.candidates {
        let file = to_new_file(c)?;
        if let Some(&i) = by_path.get(c.path.as_str()) {
            let row = &existing[i];
            if row.size != file.size || (row.mtime - file.mtime).abs() > MTIME_TOLERANCE || row.kind != file.kind {
                store.requeue(row.id, Some(&file))?;
                summary.changed += 1;
                summary.queued.push(row.id);
            } else {
                summary.unchanged += 1;
            }
            continue;
        }
        let moved = if c.inode != 0 {
            by_inode.get(&(c.device, c.inode)).map(|&i| &existing[i]).filter(|row| {
                !seen.contains(row.path.as_str())
                    && !moved_from.contains(row.path.as_str())
                    && row.size == file.size
                    && (row.mtime - file.mtime).abs() < MTIME_TOLERANCE
            })
        } else {
            None
        };
        if let Some(row) = moved {
            store.move_file(row.id, &file.path)?;
            moved_from.insert(row.path.as_str());
            summary.moved += 1;
        } else {
            let id = store.insert_file(&file, &[])?;
            summary.queued.push(id);
            summary.added += 1;
        }
    }

    // A root that went away (a drive ejected) looks empty: keep its rows rather than take that for "deleted".
    let vanished: Vec<String> = root_strings
        .iter()
        .filter(|r| !Path::new(r.as_str()).exists())
        .map(|r| if r.ends_with('/') { r.clone() } else { format!("{r}/") })
        .collect();
    for row in &existing {
        if !seen.contains(row.path.as_str())
            && !moved_from.contains(row.path.as_str())
            && !vanished.iter().any(|v| row.path.starts_with(v.as_str()))
        {
            store.delete_file(row.id)?;
            summary.removed += 1;
        }
    }
    Ok(summary)
}

#[cfg(test)]
mod tests {
    use super::*;
    use digup_core::FileState;
    use std::fs;

    struct Tmp(PathBuf);
    impl Tmp {
        fn new(tag: &str) -> Self {
            let nanos = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos();
            let p = std::env::temp_dir().join(format!("digup-sync-{tag}-{}-{nanos}", std::process::id()));
            fs::create_dir_all(&p).unwrap();
            Tmp(p.canonicalize().unwrap())
        }
    }
    impl Drop for Tmp {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn setup(tag: &str) -> (Tmp, Tmp, IndexStore) {
        let data = Tmp::new(&format!("{tag}-data"));
        let root = Tmp::new(&format!("{tag}-root"));
        let store = IndexStore::open(&data.0, "index.db").unwrap();
        (data, root, store)
    }

    fn write(root: &Path, name: &str, body: &str) -> PathBuf {
        let p = root.join(name);
        fs::write(&p, body).unwrap();
        p
    }

    fn run(store: &mut IndexStore, roots: &[&Path]) -> SyncSummary {
        let roots: Vec<PathBuf> = roots.iter().map(|r| r.to_path_buf()).collect();
        sync(store, &roots, &IndexOptions::default(), false).unwrap()
    }

    fn all(store: &IndexStore, root: &Path) -> Vec<digup_store::index::FileRecord> {
        store.files_under(&[root_string(root)]).unwrap()
    }

    #[test]
    fn first_sync_adds_then_unchanged() {
        let (_d, root, mut store) = setup("add");
        for i in 0..3 {
            write(&root.0, &format!("note{i}.txt"), "hello world");
        }
        let s = run(&mut store, &[&root.0]);
        assert_eq!((s.added, s.changed, s.moved, s.removed, s.unchanged), (3, 0, 0, 0, 0));
        assert_eq!(s.queued.len(), 3);
        let s = run(&mut store, &[&root.0]);
        assert_eq!((s.added, s.changed, s.moved, s.removed, s.unchanged), (0, 0, 0, 0, 3));
        assert!(s.queued.is_empty());
    }

    #[test]
    fn modified_file_is_requeued() {
        let (_d, root, mut store) = setup("mod");
        let p = write(&root.0, "a.txt", "short");
        run(&mut store, &[&root.0]);
        let id = all(&store, &root.0)[0].id;
        store.finish(id, FileState::Done, None, &Default::default()).unwrap();
        assert_eq!(all(&store, &root.0)[0].state, FileState::Done);
        fs::write(&p, "a much longer body than before").unwrap();
        let s = run(&mut store, &[&root.0]);
        assert_eq!((s.added, s.changed, s.unchanged), (0, 1, 0));
        assert_eq!(s.queued, vec![id]);
        let rows = all(&store, &root.0);
        assert_eq!(rows[0].state, FileState::Pending);
        assert_eq!(rows[0].size, 30);
    }

    #[cfg(unix)]
    #[test]
    fn renamed_file_is_moved_not_requeued() {
        let (_d, root, mut store) = setup("mv");
        let p = write(&root.0, "old.txt", "same content");
        run(&mut store, &[&root.0]);
        let id = all(&store, &root.0)[0].id;
        store.finish(id, FileState::Done, None, &Default::default()).unwrap();
        let new = root.0.join("new.txt");
        fs::rename(&p, &new).unwrap();
        let s = run(&mut store, &[&root.0]);
        assert_eq!((s.added, s.changed, s.moved, s.removed), (0, 0, 1, 0));
        assert!(s.queued.is_empty());
        let rows = all(&store, &root.0);
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].id, id);
        assert_eq!(rows[0].path, new.to_string_lossy());
        assert_eq!(rows[0].state, FileState::Done);
    }

    #[test]
    fn deleted_file_is_removed() {
        let (_d, root, mut store) = setup("del");
        let a = write(&root.0, "a.txt", "aaa");
        write(&root.0, "b.txt", "bbb");
        run(&mut store, &[&root.0]);
        fs::remove_file(a).unwrap();
        let s = run(&mut store, &[&root.0]);
        assert_eq!((s.removed, s.unchanged), (1, 1));
        assert_eq!(all(&store, &root.0).len(), 1);
    }

    #[test]
    fn vanished_root_keeps_rows() {
        let (_d, root, mut store) = setup("gone");
        write(&root.0, "a.txt", "aaa");
        run(&mut store, &[&root.0]);
        fs::remove_dir_all(&root.0).unwrap();
        let s = run(&mut store, &[&root.0]);
        assert_eq!((s.removed, s.added), (0, 0));
        assert_eq!(all(&store, &root.0).len(), 1);
    }

    #[test]
    fn nested_roots_count_once() {
        let (_d, root, mut store) = setup("nest");
        let sub = root.0.join("sub");
        fs::create_dir_all(&sub).unwrap();
        write(&root.0, "a.txt", "aaa");
        write(&sub, "b.txt", "bbb");
        let s = run(&mut store, &[&root.0, &sub]);
        assert_eq!(s.added, 2);
        let s = run(&mut store, &[&root.0, &sub]);
        assert_eq!((s.unchanged, s.added, s.removed), (2, 0, 0));
    }
}
