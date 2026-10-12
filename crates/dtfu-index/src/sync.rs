//! Sync: crawl the roots and bring the store in line (docs/porting/spec-pipeline.md section 2.2).
//! Port of Swift `Indexer.sync` (Indexer.swift:86-129).

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::RecvTimeoutError;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::Result;
use dtfu_core::FileKind;
use dtfu_crawl::volume::{volume_info, VolumeInfo};
use dtfu_crawl::{crawl, Candidate, CrawlOptions, CrawlReport};
use dtfu_store::index::{IndexStore, NewFile};

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
    /// One entry per root, in the order given.
    pub roots: Vec<RootStatus>,
}

/// Whether a root could be synced.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RootState {
    /// Crawled; its index is up to date.
    Online,
    /// Missing, unreadable, unplugged or not answering (a dead network share). Its rows are kept as they are.
    Offline,
    /// A different drive is mounted where this folder's drive used to be. Its rows are kept and the new drive is
    /// not indexed until [`forget_volume`] is called for the root.
    DifferentDrive,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RootStatus {
    pub root: String,
    pub state: RootState,
}

/// How long a volume lookup may take before the root counts as offline (a dead SMB/NFS mount can block for minutes).
const VOLUME_TIMEOUT: Duration = Duration::from_secs(5);

/// `meta` key holding the volume a root was last synced on: `"<volume id>\t<mount point>"`.
fn volume_key(root: &str) -> String {
    format!("volume:{root}")
}

/// Forgets which drive `root` lives on, so the next sync accepts whatever drive is mounted there now (and removes the
/// rows of files that drive doesn't have).
pub fn forget_volume(store: &IndexStore, root: &Path) -> Result<()> {
    store.set_meta(&volume_key(&root_string(root)), Some(ACCEPT))
}

/// `meta` value meaning "take whatever drive is mounted at this root on the next sync" (set by [`forget_volume`]).
const ACCEPT: &str = "accept";

type Probe<'a> = &'a dyn Fn(&Path) -> Option<VolumeInfo>;

/// The root is listable and its volume answers, within [`VOLUME_TIMEOUT`] (a dead SMB/NFS mount can block `read_dir`
/// as long as it blocks a stat, so both run on the helper thread).
/// At most one probe per root is outstanding: while an earlier one is still blocked, the root is offline and no new
/// thread is started, so a hung share costs one thread, not one per sync.
fn real_probe(p: &Path) -> Option<VolumeInfo> {
    static BLOCKED: Mutex<Vec<PathBuf>> = Mutex::new(Vec::new());
    let root = p.to_path_buf();
    {
        let mut blocked = BLOCKED.lock().unwrap_or_else(|e| e.into_inner());
        if blocked.contains(&root) {
            return None;
        }
        blocked.push(root.clone());
    }
    let (tx, rx) = std::sync::mpsc::channel();
    let spawned = std::thread::Builder::new().name("dtfu-root-probe".into()).spawn(move || {
        let ok = std::fs::read_dir(&root).is_ok();
        let info = if ok { volume_info(&root) } else { None };
        BLOCKED.lock().unwrap_or_else(|e| e.into_inner()).retain(|b| b != &root);
        let _ = tx.send(info);
    });
    if spawned.is_err() {
        BLOCKED.lock().unwrap_or_else(|e| e.into_inner()).retain(|b| b != p);
        return None;
    }
    rx.recv_timeout(VOLUME_TIMEOUT).ok().flatten()
}

/// The "finished" flag of a crawl abandoned by the watchdog in [`watched_crawl`]. While that crawl is still blocked,
/// syncs treat every root as offline instead of starting another one, so at most one such thread exists.
static ABANDONED: Mutex<Option<Arc<AtomicBool>>> = Mutex::new(None);

fn crawl_stuck() -> bool {
    ABANDONED.lock().unwrap_or_else(|e| e.into_inner()).as_ref().is_some_and(|done| !done.load(Ordering::SeqCst))
}

/// How often a running crawl checks that its roots still answer.
const CRAWL_WATCH: Duration = Duration::from_secs(5);

/// Longest a crawl may take before it is abandoned even though its roots still answer (a folder inside a root can hang
/// on its own, e.g. a second share mounted inside it). Generous: a large NAS share over SMB lists slowly.
const CRAWL_DEADLINE: Duration = Duration::from_secs(2 * 3600);

/// Crawls on a helper thread. Every [`CRAWL_WATCH`] the online roots are probed again; when one stops answering, is now
/// a different volume, or [`CRAWL_DEADLINE`] passes, the crawl is abandoned (a blocked filesystem call can't be
/// cancelled) and `None` is returned.
fn watched_crawl(roots: &[(PathBuf, VolumeInfo)], copts: &CrawlOptions, probe: Probe) -> Option<CrawlReport> {
    let started = std::time::Instant::now();
    let (tx, rx) = std::sync::mpsc::channel();
    let (r, o) = (roots.iter().map(|(p, _)| p.clone()).collect::<Vec<_>>(), copts.clone());
    let done = Arc::new(AtomicBool::new(false));
    let flag = Arc::clone(&done);
    std::thread::Builder::new()
        .name("dtfu-crawl".into())
        .spawn(move || {
            let report = crawl(&r, &o);
            flag.store(true, Ordering::SeqCst);
            let _ = tx.send(report);
        })
        .ok()?;
    loop {
        match rx.recv_timeout(CRAWL_WATCH) {
            Ok(report) => return Some(report),
            Err(RecvTimeoutError::Disconnected) => return None,
            Err(RecvTimeoutError::Timeout) => {
                // Gone, not answering, or a different volume now (a drive swapped mid-crawl): the listing can't be
                // trusted.
                let changed = roots.iter().any(|(r, v)| probe(r).map(|now| now.id) != Some(v.id.clone()));
                if changed || started.elapsed() > CRAWL_DEADLINE {
                    // The thread stays blocked; remember it until it returns.
                    *ABANDONED.lock().unwrap_or_else(|e| e.into_inner()) = Some(done);
                    return None;
                }
            }
        }
    }
}

/// A root's state before the crawl. `Adopt` is a root whose volume is not recorded yet (every root after an upgrade,
/// or a new folder): it is crawled, but its volume is recorded, and its missing files removed, only once the crawl
/// shows it is the drive the index knows (see `sync_with`).
#[derive(Clone, Copy, PartialEq, Eq)]
enum Check {
    Known(RootState),
    Adopt,
}

fn record(store: &IndexStore, key: &str, vol: &VolumeInfo) -> Result<()> {
    store.set_meta(key, Some(&format!("{}\t{}", vol.id, vol.mount_point)))
}

/// Decides a root's state before the crawl.
fn check_root(store: &IndexStore, root: &Path, probe: Probe) -> Result<(Check, Option<VolumeInfo>)> {
    let Some(vol) = probe(root) else { return Ok((Check::Known(RootState::Offline), None)) };
    let key = volume_key(&root_string(root));
    let Some(rec) = store.meta(&key)? else { return Ok((Check::Adopt, Some(vol))) };
    if rec == ACCEPT {
        record(store, &key, &vol)?;
        return Ok((Check::Known(RootState::Online), Some(vol)));
    }
    let (id, mount) = rec.split_once('\t').unwrap_or((rec.as_str(), ""));
    // A `dev:` id is the fallback when the volume UUID lookup failed: it can't be compared with a `uuid:` one, so a
    // mismatch between the two forms means "unsure", which keeps the rows, never "different drive".
    let one_is_fallback = id.starts_with("dev:") != vol.id.starts_with("dev:");
    if id == vol.id {
        Ok((Check::Known(RootState::Online), Some(vol)))
    } else if vol.mount_point.len() > mount.len() && vol.mount_point.starts_with(mount) {
        // A volume is now mounted closer to the folder than the one recorded: the folder was first seen while its
        // drive was not mounted (the volume underneath was recorded), and the drive has appeared. Adopt it on the
        // same terms as an unrecorded root.
        Ok((Check::Adopt, Some(vol)))
    } else if mount != vol.mount_point || one_is_fallback {
        // The folder now sits on another filesystem: its drive is not mounted and what is left is an empty
        // mount-point folder (macOS and Linux both leave these behind after an unplug without eject).
        Ok((Check::Known(RootState::Offline), None))
    } else {
        Ok((Check::Known(RootState::DifferentDrive), None))
    }
}

/// Two mtimes closer than this (seconds) count as the same date (`dtfu_store::index` uses the same tolerance).
pub(crate) const MTIME_TOLERANCE: f64 = 0.001;

/// `root` followed by the platform's separator, for prefix matching that never catches a sibling ("/ab" for "/a").
fn dir_prefix(root: &str) -> String {
    if root.ends_with(['/', std::path::MAIN_SEPARATOR]) { root.to_string() } else { format!("{root}{}", std::path::MAIN_SEPARATOR) }
}

/// A folder as the index keys it: its text without trailing separators, except a filesystem root ("/", "C:\\")
/// which keeps its own. Shared with the service and the engine so their keys match.
pub fn root_string(p: &Path) -> String {
    let s = p.to_string_lossy().into_owned();
    if p.parent().is_none() {
        return s; // "/" or "C:\\": trimming would change what it names
    }
    let t = s.trim_end_matches(['/', std::path::MAIN_SEPARATOR]);
    if t.is_empty() { s } else { t.to_string() }
}

fn to_new_file(c: &Candidate) -> Result<NewFile> {
    let kind = FileKind::parse(c.kind.as_str())
        .ok_or_else(|| anyhow::anyhow!("unknown file kind {}", c.kind.as_str()))?;
    Ok(NewFile { path: c.path.clone(), kind, size: c.size, mtime: c.mtime, inode: c.inode, device: c.device })
}

/// Crawls `roots` and reconciles the index: new files are queued, changed ones re-queued, moved ones re-pathed
/// (no re-embedding), and files that are gone or newly excluded are removed. Rows under a root that no longer exists
/// on disk, or that can't be read, are kept (and not crawled). Each store call commits on its own (the store API has no outer transaction).
///
/// Drives unplugged without ejecting and dead network shares must never look like "everything was deleted": a root
/// that is unreadable, not answering, on a different volume than last time, or that changes volume or becomes
/// unreadable during the crawl keeps all its rows, and so does every folder the crawl could not list.
pub fn sync(store: &mut IndexStore, roots: &[PathBuf], opts: &IndexOptions, code: bool) -> Result<SyncSummary> {
    sync_with(store, roots, opts, code, &real_probe)
}

fn sync_with(store: &mut IndexStore, roots: &[PathBuf], opts: &IndexOptions, code: bool, probe: Probe) -> Result<SyncSummary> {
    let copts = CrawlOptions {
        code,
        include_code: opts.include_code,
        include_datasets: opts.include_datasets,
        max_code_bytes: opts.max_code_bytes as i64,
        max_notebook_bytes: opts.max_notebook_bytes as i64,
        excluded_folders: opts.excluded_folders.iter().map(|p| root_string(p)).collect(),
        ..CrawlOptions::default()
    };
    // A root that is not online is skipped entirely: it contributes no candidates and its rows are kept (see
    // `vanished` below).
    let mut states = Vec::with_capacity(roots.len());
    let mut adopt = vec![false; roots.len()];
    let mut online: Vec<(PathBuf, VolumeInfo)> = Vec::new();
    for (i, r) in roots.iter().enumerate() {
        let (check, vol) = check_root(store, r, probe)?;
        adopt[i] = check == Check::Adopt;
        states.push(match check {
            Check::Known(s) => s,
            Check::Adopt => RootState::Online,
        });
        if let Some(v) = vol {
            online.push((r.clone(), v));
        }
    }
    let readable: Vec<PathBuf> = online.iter().map(|(r, _)| r.clone()).collect();
    // A crawl abandoned earlier and still blocked means a drive is hanging: don't start a second one.
    let crawled = if readable.is_empty() {
        Some(CrawlReport::default())
    } else if crawl_stuck() {
        None
    } else {
        watched_crawl(&online, &copts, probe)
    };
    let report = crawled.unwrap_or_else(|| {
        // Nothing was (fully) crawled: every root keeps its rows this time.
        states.iter_mut().for_each(|s| *s = RootState::Offline);
        CrawlReport::default()
    });
    // A drive pulled during the crawl: the root is gone, unreadable, or now on another volume. Look again.
    for (i, r) in roots.iter().enumerate() {
        if states[i] != RootState::Online {
            continue;
        }
        let before = online.iter().find(|(o, _)| o == r).map(|(_, v)| v);
        let still = probe(r).as_ref().map(|v| &v.id) == before.map(|v| &v.id);
        if !still {
            states[i] = RootState::Offline;
        }
    }
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
    // An unrecorded root is adopted (its volume recorded) when the index holds nothing under it yet, or when the crawl
    // found at least one file the index already has there. Otherwise it is most likely an empty mount-point folder
    // left by an unplugged drive: it counts as offline and keeps its rows. (A drive whose files were all deleted stays
    // that way until the user picks "Use this drive", which calls `forget_volume`.)
    for (i, r) in roots.iter().enumerate() {
        if !adopt[i] || states[i] != RootState::Online {
            continue;
        }
        let rs = root_string(r);
        let prefix = dir_prefix(&rs);
        let mut under = existing.iter().filter(|row| row.path.starts_with(&prefix)).peekable();
        let known = under.peek().is_none() || under.any(|row| seen.contains(row.path.as_str()));
        if known {
            if let Some((_, v)) = online.iter().find(|(o, _)| o == r) {
                record(store, &volume_key(&rs), v)?;
            }
        } else {
            states[i] = RootState::Offline;
        }
    }
    let mut moved_from: HashSet<&str> = HashSet::new();
    let mut summary = SyncSummary {
        skipped: report.skipped.clone(),
        skipped_folders: report.skipped_folders.clone(),
        datasets: report.datasets.clone(),
        roots: roots.iter().zip(&states).map(|(r, &state)| RootStatus { root: root_string(r), state }).collect(),
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

    // A root that went away (a drive ejected) or can't be read looks empty: keep its rows rather than take that for
    // "deleted". Its prefix is in `vanished`, so rows under it survive even when it is nested in a readable root. The
    // same goes for any folder the crawl could not list.
    let vanished: Vec<String> = roots
        .iter()
        .zip(&states)
        .filter(|(_, &s)| s != RootState::Online)
        .map(|(r, _)| root_string(r))
        .chain(report.unreadable_dirs.iter().cloned())
        .map(|r| dir_prefix(&r))
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
    use dtfu_core::FileState;
    use std::fs;

    struct Tmp(PathBuf);
    impl Tmp {
        fn new(tag: &str) -> Self {
            let nanos = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos();
            let p = std::env::temp_dir().join(format!("dtfu-sync-{tag}-{}-{nanos}", std::process::id()));
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

    fn all(store: &IndexStore, root: &Path) -> Vec<dtfu_store::index::FileRecord> {
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

    /// Restores a folder's permissions on drop, so the temporary folder can be removed even when the test fails.
    #[cfg(unix)]
    struct Restore<'a>(&'a Path);
    #[cfg(unix)]
    impl Drop for Restore<'_> {
        fn drop(&mut self) {
            use std::os::unix::fs::PermissionsExt;
            let _ = fs::set_permissions(self.0, fs::Permissions::from_mode(0o755));
        }
    }

    #[cfg(unix)]
    #[test]
    fn unreadable_root_keeps_rows() {
        use std::os::unix::fs::PermissionsExt;
        let (_d, root, mut store) = setup("unreadable");
        write(&root.0, "a.txt", "aaa");
        run(&mut store, &[&root.0]);
        assert_eq!(all(&store, &root.0).len(), 1);
        let _restore = Restore(&root.0);
        fs::set_permissions(&root.0, fs::Permissions::from_mode(0o000)).unwrap();
        if fs::read_dir(&root.0).is_ok() {
            // Running as root (or a filesystem without permission checks): the root is readable, so there is nothing to test.
            return;
        }
        let s = run(&mut store, &[&root.0]);
        assert_eq!((s.removed, s.added, s.unchanged), (0, 0, 0));
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

    fn vol(id: &str, mount: &str) -> VolumeInfo {
        VolumeInfo {
            mount_point: mount.into(),
            id: id.into(),
            kind: dtfu_crawl::volume::VolumeKind::Removable,
            fs_type: "exfat".into(),
        }
    }

    fn run_probe(store: &mut IndexStore, root: &Path, probe: Probe) -> SyncSummary {
        sync_with(store, &[root.to_path_buf()], &IndexOptions::default(), false, probe).unwrap()
    }

    #[test]
    fn different_drive_at_same_mount_keeps_rows_until_forgotten() {
        let (_d, root, mut store) = setup("swap");
        let a = write(&root.0, "a.txt", "aaa");
        write(&root.0, "b.txt", "bbb");
        let s = run_probe(&mut store, &root.0, &|_| Some(vol("uuid:A", "/Volumes/USB")));
        assert_eq!(s.roots[0].state, RootState::Online);
        fs::remove_file(a).unwrap(); // the other drive doesn't have a.txt
        let other = |_: &Path| Some(vol("uuid:B", "/Volumes/USB"));
        let s = run_probe(&mut store, &root.0, &other);
        assert_eq!(s.roots[0].state, RootState::DifferentDrive);
        assert_eq!((s.removed, s.added, s.unchanged), (0, 0, 0));
        assert_eq!(all(&store, &root.0).len(), 2);
        forget_volume(&store, &root.0).unwrap();
        let s = run_probe(&mut store, &root.0, &other);
        assert_eq!(s.roots[0].state, RootState::Online);
        assert_eq!((s.removed, s.unchanged), (1, 1));
    }

    #[test]
    fn unmounted_drive_leaving_empty_mount_folder_keeps_rows() {
        let (_d, root, mut store) = setup("stale");
        let a = write(&root.0, "a.txt", "aaa");
        run_probe(&mut store, &root.0, &|_| Some(vol("uuid:A", "/Volumes/USB")));
        fs::remove_file(a).unwrap(); // the folder is still there, but empty: it is on the boot disk now
        let s = run_probe(&mut store, &root.0, &|_| Some(vol("uuid:BOOT", "/")));
        assert_eq!(s.roots[0].state, RootState::Offline);
        assert_eq!(s.removed, 0);
        assert_eq!(all(&store, &root.0).len(), 1);
        // Plugged back in: the same drive is synced as usual.
        write(&root.0, "a.txt", "aaa");
        let s = run_probe(&mut store, &root.0, &|_| Some(vol("uuid:A", "/Volumes/USB")));
        assert_eq!((s.roots[0].state, s.removed, s.added), (RootState::Online, 0, 0));
        assert_eq!(all(&store, &root.0).len(), 1);
    }

    #[test]
    fn drive_pulled_during_crawl_keeps_rows() {
        let (_d, root, mut store) = setup("yank");
        let a = write(&root.0, "a.txt", "aaa");
        write(&root.0, "b.txt", "bbb");
        run_probe(&mut store, &root.0, &|_| Some(vol("uuid:A", "/Volumes/USB")));
        fs::remove_file(a).unwrap();
        // Online when the sync starts, gone (lookup fails) when it looks again after the crawl.
        let calls = std::cell::Cell::new(0);
        let probe = |_: &Path| {
            calls.set(calls.get() + 1);
            (calls.get() == 1).then(|| vol("uuid:A", "/Volumes/USB"))
        };
        let s = run_probe(&mut store, &root.0, &probe);
        assert_eq!(s.roots[0].state, RootState::Offline);
        assert_eq!(s.removed, 0);
        assert_eq!(all(&store, &root.0).len(), 2);
    }

    #[test]
    fn unanswering_volume_is_offline() {
        let (_d, root, mut store) = setup("dead");
        write(&root.0, "a.txt", "aaa");
        run_probe(&mut store, &root.0, &|_| Some(vol("net://nas/share", "/Volumes/share")));
        let s = run_probe(&mut store, &root.0, &|_| None);
        assert_eq!((s.roots[0].state, s.removed, s.added), (RootState::Offline, 0, 0));
    }

    #[cfg(unix)]
    #[test]
    fn unlistable_subfolder_keeps_rows() {
        use std::os::unix::fs::PermissionsExt;
        let (_d, root, mut store) = setup("subdir");
        let sub = root.0.join("sub");
        fs::create_dir_all(&sub).unwrap();
        write(&root.0, "a.txt", "aaa");
        write(&sub, "b.txt", "bbb");
        run(&mut store, &[&root.0]);
        assert_eq!(all(&store, &root.0).len(), 2);
        let _restore = Restore(&sub);
        fs::set_permissions(&sub, fs::Permissions::from_mode(0o000)).unwrap();
        if fs::read_dir(&sub).is_ok() {
            return; // running as root
        }
        let s = run(&mut store, &[&root.0]);
        assert_eq!((s.removed, s.unchanged), (0, 1));
        assert_eq!(all(&store, &root.0).len(), 2);
    }

    #[test]
    fn drive_mounted_after_folder_was_first_seen_unmounted_is_accepted() {
        let (_d, root, mut store) = setup("late");
        // First sync while the drive is not mounted: the folder sits on the boot volume.
        let s = run_probe(&mut store, &root.0, &|_| Some(vol("uuid:BOOT", "/")));
        assert_eq!(s.roots[0].state, RootState::Online);
        write(&root.0, "a.txt", "aaa");
        let s = run_probe(&mut store, &root.0, &|_| Some(vol("uuid:A", "/Volumes/USB")));
        assert_eq!((s.roots[0].state, s.added), (RootState::Online, 1));
        // Unplugged again: the boot volume shows through, which is now "offline", not a new drive.
        let s = run_probe(&mut store, &root.0, &|_| Some(vol("uuid:BOOT", "/")));
        assert_eq!((s.roots[0].state, s.removed), (RootState::Offline, 0));
    }

    #[test]
    fn upgrade_with_empty_mount_folder_keeps_rows_and_records_nothing() {
        let (_d, root, mut store) = setup("upgrade");
        let a = write(&root.0, "a.txt", "aaa");
        write(&root.0, "b.txt", "bbb");
        run_probe(&mut store, &root.0, &|_| Some(vol("uuid:A", "/Volumes/USB")));
        // An index from before volumes were recorded: rows, no volume.
        store.set_meta(&volume_key(&root_string(&root.0)), None).unwrap();
        fs::remove_file(a).unwrap();
        fs::remove_file(root.0.join("b.txt")).unwrap(); // empty folder left behind by an unplugged drive
        let s = run_probe(&mut store, &root.0, &|_| Some(vol("uuid:BOOT", "/")));
        assert_eq!((s.roots[0].state, s.removed), (RootState::Offline, 0));
        assert_eq!(all(&store, &root.0).len(), 2);
        assert_eq!(store.meta(&volume_key(&root_string(&root.0))).unwrap(), None);
    }

    #[test]
    fn upgrade_with_drive_present_adopts_it() {
        let (_d, root, mut store) = setup("adopt");
        let a = write(&root.0, "a.txt", "aaa");
        write(&root.0, "b.txt", "bbb");
        run_probe(&mut store, &root.0, &|_| Some(vol("uuid:A", "/Volumes/USB")));
        store.set_meta(&volume_key(&root_string(&root.0)), None).unwrap();
        fs::remove_file(a).unwrap(); // b.txt is still there: this is the drive the index knows
        let s = run_probe(&mut store, &root.0, &|_| Some(vol("uuid:A", "/Volumes/USB")));
        assert_eq!((s.roots[0].state, s.removed, s.unchanged), (RootState::Online, 1, 1));
        assert!(store.meta(&volume_key(&root_string(&root.0))).unwrap().unwrap().starts_with("uuid:A\t"));
    }

    #[test]
    fn use_this_drive_adopts_an_emptied_drive() {
        let (_d, root, mut store) = setup("accept");
        let a = write(&root.0, "a.txt", "aaa");
        run_probe(&mut store, &root.0, &|_| Some(vol("uuid:A", "/Volumes/USB")));
        store.set_meta(&volume_key(&root_string(&root.0)), None).unwrap();
        fs::remove_file(a).unwrap();
        let s = run_probe(&mut store, &root.0, &|_| Some(vol("uuid:A", "/Volumes/USB")));
        assert_eq!(s.roots[0].state, RootState::Offline); // can't tell it from an empty mount folder
        forget_volume(&store, &root.0).unwrap();
        let s = run_probe(&mut store, &root.0, &|_| Some(vol("uuid:A", "/Volumes/USB")));
        assert_eq!((s.roots[0].state, s.removed), (RootState::Online, 1));
    }
}
