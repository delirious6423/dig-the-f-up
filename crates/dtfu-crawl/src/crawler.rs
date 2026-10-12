//! Port of Crawler.swift:27-156. Same semantics, but the walk is parallel (rayon, one task per directory,
//! big directories also split across threads). Metadata only; the one content read is the TS sniff.
//!
//! Deliberate differences from Swift: no iCloud/File Provider ubiquity keys (only SF_DATALESS on macOS), no
//! `isPackage` from the OS (extension list `PACKAGE_EXTENSIONS` instead), no git ls-files filtering in code mode,
//! output sorted by path (Swift's enumerator order is unspecified).

use crate::folder_selection::FolderSelection;
use crate::rules::{self, Classification, DatasetEntry};
use crate::types::FileKind;
use std::collections::{HashMap, HashSet};
use std::fs::Metadata;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

/// Directory extensions treated as macOS packages/bundles (`isPackage`): not descended, counted "app or bundle".
/// Crawler.swift:68-71. A port decision: the OS database is replaced by this list.
pub const PACKAGE_EXTENSIONS: &[&str] = &[
    "app", "appex", "framework", "bundle", "plugin", "kext", "xpc", "photoslibrary", "musiclibrary", "tvlibrary",
    "imovielibrary", "fcpbundle", "xcodeproj", "xcworkspace", "playground", "rtfd", "pages", "numbers", "key",
    "sparsebundle", "dSYM", "xcarchive",
];

/// Subset of Model.swift `IndexOptions` the crawl reads (Model.swift:37-69).
#[derive(Debug, Clone)]
pub struct CrawlOptions {
    /// Code index mode (`IndexOptions.code`).
    pub code: bool,
    /// Model.swift:49
    pub include_code: bool,
    /// Model.swift:50
    pub include_datasets: bool,
    /// Model.swift:46: 1 << 20
    pub max_code_bytes: i64,
    /// Model.swift:48: 20 << 20
    pub max_notebook_bytes: i64,
    pub excluded_folders: Vec<String>,
    /// Lowercased, without the dot.
    pub excluded_extensions: HashSet<String>,
    /// Port addition: open files with an unknown extension and try the magic-byte table (one read each).
    pub sniff_magic: bool,
    /// Port addition: apply `is_secret` in the main index too (Swift does not; spec 2.1).
    pub secrets_everywhere: bool,
    /// Worker threads; 0 = rayon's global pool, 1 = effectively serial.
    pub threads: usize,
    /// For the "~/Library" rule; defaults to $HOME (or %USERPROFILE%).
    pub home: String,
}

impl Default for CrawlOptions {
    fn default() -> Self {
        Self {
            code: false,
            include_code: false,
            include_datasets: false,
            max_code_bytes: 1 << 20,
            max_notebook_bytes: 20 << 20,
            excluded_folders: vec![],
            excluded_extensions: HashSet::new(),
            sniff_magic: false,
            secrets_everywhere: false,
            threads: 0,
            home: unixify(&std::env::var("HOME").or_else(|_| std::env::var("USERPROFILE")).unwrap_or_default()),
        }
    }
}

/// Crawler.swift:5
#[derive(Debug, Clone, PartialEq)]
pub struct Candidate {
    pub path: String,
    pub kind: FileKind,
    pub size: i64,
    /// seconds.nanoseconds
    pub mtime: f64,
    /// unix st_ino; windows: 0 (std exposes the file index only on nightly; `same-file` does not expose the number)
    pub inode: u64,
    /// unix st_dev; windows: 0
    pub device: u64,
}

/// Crawler.swift:14
#[derive(Debug, Default)]
pub struct CrawlReport {
    pub candidates: Vec<Candidate>,
    pub skipped: HashMap<String, usize>,
    pub skipped_folders: HashMap<String, usize>,
    pub datasets: HashMap<String, usize>,
    /// Folders whose listing failed (read_dir error, or an entry error while iterating), as `/`-separated
    /// paths, sorted and deduplicated. Their contents were NOT scanned, so an empty result there is not proof of absence.
    pub unreadable_dirs: Vec<String>,
}

#[derive(Default)]
struct Partial {
    found: Vec<Candidate>,
    skipped: HashMap<String, usize>,
    skipped_folders: HashMap<String, usize>,
    project_roots: Vec<String>,
    unreadable_dirs: Vec<String>,
}

impl Partial {
    fn merge(&mut self, o: Partial) {
        self.found.extend(o.found);
        self.unreadable_dirs.extend(o.unreadable_dirs);
        for (k, v) in o.skipped {
            *self.skipped.entry(k).or_default() += v;
        }
        for (k, v) in o.skipped_folders {
            *self.skipped_folders.entry(k).or_default() += v;
        }
        self.project_roots.extend(o.project_roots);
    }
    fn skip(&mut self, reason: &str) {
        *self.skipped.entry(reason.to_string()).or_default() += 1;
    }
}

/// Lexical `standardizedFileURL.path`: drop `.`/empty components, resolve `..`, no trailing slash.
pub fn standardize(path: &str) -> String {
    let path = &unixify(path);
    let abs = path.starts_with('/');
    let mut parts: Vec<&str> = vec![];
    for c in path.split('/') {
        match c {
            "" | "." => {}
            ".." => {
                if !parts.is_empty() && *parts.last().unwrap() != ".." {
                    parts.pop();
                } else if !abs {
                    parts.push("..");
                }
            }
            c => parts.push(c),
        }
    }
    let joined = parts.join("/");
    if abs { format!("/{joined}") } else { joined }
}

struct Ctx<'a> {
    opts: &'a CrawlOptions,
    skips: Vec<String>,
}

pub fn crawl<P: AsRef<Path>>(roots: &[P], options: &CrawlOptions) -> CrawlReport {
    let roots: Vec<String> = roots.iter().map(|r| unixify(&r.as_ref().to_string_lossy())).collect();
    let roots = &roots[..];
    if options.threads > 0 {
        if let Ok(pool) = rayon::ThreadPoolBuilder::new().num_threads(options.threads).build() { return pool.install(|| crawl_inner(roots, options)) }
    }
    crawl_inner(roots, options)
}

fn crawl_inner(roots: &[String], options: &CrawlOptions) -> CrawlReport {
    let mut total = Partial::default();
    let root_paths: Vec<String> = roots.iter().map(|r| standardize(r)).collect();
    let root_set: HashSet<String> = root_paths.iter().cloned().collect();

    for root in &root_paths {
        // Crawler.swift:40-44. In code mode Swift enters git repos instead; see module docs.
        let root_path = PathBuf::from(root);
        if !options.code && rules::is_project_root(&root_path) {
            total.project_roots.push(root.clone());
        }
        // Crawler.swift:47-50: a skip applies inside the chosen folders that contain it.
        let real = FolderSelection::real_path(root);
        let skips: Vec<String> = options
            .excluded_folders
            .iter()
            .map(|f| unixify(f))
            .filter(|f| FolderSelection::is_inside(f, root) || FolderSelection::is_inside(f, &real))
            .flat_map(|f| {
                let real = FolderSelection::real_path(&f);
                [f, real]
            })
            .collect();
        let ctx = Ctx { opts: options, skips };
        let sink = Mutex::new(Partial::default());
        rayon::scope(|s| walk_dir(&ctx, &root_path, s, &sink));
        total.merge(sink.into_inner().unwrap());
    }

    // Crawler.swift:105 dedup by path (a folder inside another root is walked twice).
    total.found.sort_by(|a, b| a.path.cmp(&b.path));
    total.found.dedup_by(|a, b| a.path == b.path);

    // Crawler.swift:121-137: inside a code project only screenshots count.
    let project_roots: HashSet<String> = total.project_roots.into_iter().collect();
    let mut memo: HashMap<String, bool> = HashMap::new();
    fn in_project(folder: &str, roots: &HashSet<String>, memo: &mut HashMap<String, bool>) -> bool {
        if let Some(k) = memo.get(folder) {
            return *k;
        }
        let parent = FolderSelection::parent(folder);
        // `parent.count > 1` guard from Swift (the "/" root has count 1)
        let answer = roots.contains(folder)
            || (parent != folder && parent.chars().count() > 1 && in_project(&parent, roots, memo));
        memo.insert(folder.to_string(), answer);
        answer
    }
    total.unreadable_dirs.sort();
    total.unreadable_dirs.dedup();
    let mut report = CrawlReport {
        skipped: total.skipped,
        skipped_folders: total.skipped_folders,
        unreadable_dirs: total.unreadable_dirs,
        ..Default::default()
    };
    let mut kept = Vec::with_capacity(total.found.len());
    for file in total.found {
        if !options.code
            && file.kind != FileKind::Screenshot
            && in_project(&FolderSelection::parent(&file.path), &project_roots, &mut memo)
        {
            *report.skipped.entry("inside a code project".into()).or_default() += 1;
        } else {
            kept.push(file);
        }
    }

    // Crawler.swift:139-153
    if !options.include_datasets && !options.code {
        let entries: Vec<DatasetEntry> = kept
            .iter()
            .map(|c| DatasetEntry {
                folder: FolderSelection::parent(&c.path),
                stem: Path::new(&c.path).file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default(),
                kind: c.kind,
            })
            .collect();
        let datasets = rules::dataset_folders(&entries, &root_set, rules::DATASET_MINIMUM);
        if !datasets.is_empty() {
            kept.retain(|f| {
                let drop = datasets.contains_key(&FolderSelection::parent(&f.path))
                    && matches!(f.kind, FileKind::Image | FileKind::Audio | FileKind::Video);
                if drop {
                    *report.skipped.entry("dataset".into()).or_default() += 1;
                }
                !drop
            });
            report.datasets = datasets;
        }
    }
    report.candidates = kept;
    report
}

fn is_package_dir(name: &str) -> bool {
    name.rfind('.').is_some_and(|i| i > 0 && PACKAGE_EXTENSIONS.contains(&&name[i + 1..]))
}

/// Big directories split their file work across threads in chunks of this size.
const CHUNK: usize = 512;

fn walk_dir<'s>(ctx: &'s Ctx<'s>, dir: &Path, scope: &rayon::Scope<'s>, sink: &'s Mutex<Partial>) {
    let mut local = Partial::default();
    let rd = match std::fs::read_dir(dir) {
        Ok(rd) => rd,
        Err(_) => {
            // errorHandler { true }: ignore and continue (Swift parity), but record that this folder was not scanned.
            local.unreadable_dirs.push(path_string(dir));
            sink.lock().unwrap().merge(local);
            return;
        }
    };
    let mut entries: Vec<(String, PathBuf, Option<std::fs::FileType>)> = Vec::new();
    for e in rd {
        match e {
            Ok(e) => entries.push((e.file_name().to_string_lossy().into_owned(), e.path(), e.file_type().ok())),
            // An entry that cannot be read means the listing is incomplete.
            Err(_) => local.unreadable_dirs.push(path_string(dir)),
        }
    }

    // Crawler.swift:56-67: this folder was entered; is it a code project? (same listing, no second readdir)
    if !ctx.opts.code && rules::is_project_listing(entries.iter().map(|e| e.0.as_str())) {
        local.project_roots.push(path_string(dir));
    }

    let mut files: Vec<&(String, PathBuf, Option<std::fs::FileType>)> = vec![];
    for e in &entries {
        let (name, path, ft) = e;
        if name.starts_with('.') {
            continue; // .skipsHiddenFiles
        }
        let is_dir = ft.is_some_and(|t| t.is_dir());
        if !is_dir {
            files.push(e);
            continue;
        }
        if is_package_dir(name) {
            local.skip("app or bundle"); // .skipsPackageDescendants (Crawler.swift:68-71)
            continue;
        }
        let p = path_string(path);
        let reason = rules::skip_reason_for_folder(&p, &ctx.opts.home, &ctx.skips)
            .or_else(|| if ctx.opts.code { rules::code_skip_reason_for_folder(path) } else { None });
        if let Some(r) = reason {
            *local.skipped_folders.entry(r.to_string()).or_default() += 1;
            continue;
        }
        let child = path.clone();
        scope.spawn(move |s| walk_dir(ctx, &child, s, sink));
    }

    if files.len() > CHUNK {
        use rayon::prelude::*;
        let parts: Vec<Partial> = files
            .par_chunks(CHUNK)
            .map(|c| {
                let mut p = Partial::default();
                for (name, path, _) in c.iter().copied() {
                    visit_file(ctx, name, path, &mut p);
                }
                p
            })
            .collect();
        for p in parts {
            local.merge(p);
        }
    } else {
        for (name, path, _) in files {
            visit_file(ctx, name, path, &mut local);
        }
    }
    sink.lock().unwrap().merge(local);
}

fn path_string(p: &Path) -> String {
    unixify(&p.to_string_lossy())
}

/// Windows: `\\?\C:\a\b` becomes `C:/a/b`, so the `/`-based path logic works. Identity elsewhere.
pub fn unixify(p: &str) -> String {
    if cfg!(windows) {
        p.strip_prefix(r"\\?\").unwrap_or(p).replace('\\', "/")
    } else {
        p.to_string()
    }
}

fn visit_file(ctx: &Ctx, name: &str, path: &Path, out: &mut Partial) {
    let o = ctx.opts;
    if !o.code && o.secrets_everywhere && rules::is_secret(name) {
        out.skip("key or password file");
        return;
    }
    let class = if o.code { rules::classify_code(path, o) } else { rules::classify(path, o) };
    let kind = match class {
        Classification::Ignore => return out.skip("other type"),
        Classification::Skip(r) => return out.skip(r),
        Classification::Index(k) => k,
    };
    // Crawler.swift:80-84: lstat, regular files only (symlinks are never followed).
    let md = match std::fs::symlink_metadata(path) {
        Ok(m) if m.file_type().is_file() => m,
        _ => return out.skip("link or special file"),
    };
    if is_dataless(&md) {
        return out.skip("in the cloud, not downloaded"); // Crawler.swift:85-91
    }
    if o.code {
        // Crawler.swift:92-104. (The "ignored by git" step is not ported.)
        let is_nb = path.extension().is_some_and(|e| e.eq_ignore_ascii_case("ipynb"));
        let limit = if is_nb { o.max_notebook_bytes } else { o.max_code_bytes };
        if md.len() as i64 > limit {
            return out.skip("too big to be written by hand");
        }
    }
    let (mtime, inode, device) = stat_fields(&md);
    out.found.push(Candidate { path: path_string(path), kind, size: md.len() as i64, mtime, inode, device });
}

/// Crawler.swift:85-87 SF_DATALESS (0x4000_0000) on macOS; nowhere else.
#[cfg(target_os = "macos")]
fn is_dataless(md: &Metadata) -> bool {
    use std::os::macos::fs::MetadataExt;
    md.st_flags() & 0x4000_0000 != 0
}
#[cfg(not(target_os = "macos"))]
fn is_dataless(_md: &Metadata) -> bool {
    false
}

/// mtime = tv_sec + tv_nsec / 1e9 (Crawler.swift:107), inode, device.
#[cfg(unix)]
fn stat_fields(md: &Metadata) -> (f64, u64, u64) {
    use std::os::unix::fs::MetadataExt;
    (md.mtime() as f64 + md.mtime_nsec() as f64 / 1e9, md.ino(), md.dev())
}
/// Windows fallback: mtime from `modified()`, inode and device 0 (file index is unstable in std). Move detection
/// by inode (spec 2.2) then never fires; a move is seen as removed + added.
#[cfg(not(unix))]
fn stat_fields(md: &Metadata) -> (f64, u64, u64) {
    let m = md
        .modified()
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map_or(0.0, |d| d.as_secs_f64());
    (m, 0, 0)
}
