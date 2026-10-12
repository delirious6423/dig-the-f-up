//! Ports of RulesTests.swift (the tests that touch Rules/Crawler; Searcher, DocExtractor, Vectors tests belong to
//! other crates) plus extra coverage of the Rust-side tables.
use dtfu_crawl::rules::*;
use dtfu_crawl::{classify, classify_code, crawl, Classification, CrawlOptions, FileKind};
use std::collections::HashSet;
use std::fs;
use tempfile::TempDir;

fn names(r: &dtfu_crawl::CrawlReport, root: &std::path::Path) -> Vec<String> {
    // Candidate paths use '/' on every platform (see `unixify` in crawler.rs).
    let prefix = format!("{}/", root.display()).replace('\\', "/");
    r.candidates.iter().map(|c| c.path.strip_prefix(&prefix).unwrap().to_string()).collect()
}

// RulesTests.swift:14 typeScriptIsNotVideo
#[test]
fn type_script_is_not_video() {
    let d = TempDir::new().unwrap();
    let f = d.path().join("app.ts");
    fs::write(&f, "export const x: number = 1;\n").unwrap();
    assert_eq!(classify(&f, &CrawlOptions::default()), Classification::Skip("code or data file"));
}

// RulesTests.swift:21 realTransportStreamIsVideo
#[test]
fn real_transport_stream_is_video() {
    let d = TempDir::new().unwrap();
    let mut bytes = vec![0xFFu8; 188 * 4];
    for p in 0..4 {
        bytes[p * 188] = 0x47;
    }
    let f = d.path().join("clip.ts");
    fs::write(&f, bytes).unwrap();
    assert_eq!(classify(&f, &CrawlOptions::default()), Classification::Index(FileKind::Video));
}

// RulesTests.swift:29 codeCanBeIncluded
#[test]
fn code_can_be_included() {
    let d = TempDir::new().unwrap();
    let o = CrawlOptions { include_code: true, ..Default::default() };
    assert_eq!(classify(&d.path().join("main.py"), &o), Classification::Index(FileKind::Doc));
}

// RulesTests.swift:36 machineNames
#[test]
fn machine_names() {
    assert!(looks_machine_named("clip_000123"));
    assert!(looks_machine_named("frame-0042"));
    assert!(looks_machine_named("3f9a1c0e-12ab-4cde"));
    assert!(looks_machine_named("000123"));
    assert!(!looks_machine_named("IMG_1234"));
    assert!(!looks_machine_named("PXL_20260101_123456789"));
    assert!(!looks_machine_named("Screenshot 2026-10-01 at 9.41.07 PM"));
    assert!(!looks_machine_named("Tokyo itinerary"));
}

// RulesTests.swift:47 datasetNeedsManyMachineNamedFiles
#[test]
fn dataset_needs_many_machine_named_files() {
    let e = |f: &str, s: String, k| DatasetEntry { folder: f.into(), stem: s, kind: k };
    let mut files = vec![];
    files.extend((0..400).map(|i| e("/x/clips", format!("clip_{i:06}"), FileKind::Audio)));
    files.extend((0..400).map(|i| e("/x/camera", format!("IMG_{}", 1000 + i), FileKind::Image)));
    files.extend((0..50).map(|i| e("/x/few", format!("clip_{i}"), FileKind::Audio)));
    let d = dataset_folders(&files, &HashSet::new(), DATASET_MINIMUM);
    assert_eq!(d.len(), 1);
    assert_eq!(d["/x/clips"], 400);
}

// RulesTests.swift:132 nestedRootsAreCrawledOnce
#[test]
fn nested_roots_are_crawled_once() {
    let d = TempDir::new().unwrap();
    let inner = d.path().join("inner");
    fs::create_dir_all(&inner).unwrap();
    fs::write(inner.join("trip.txt"), "notes about the trip").unwrap();
    let r = crawl(&[d.path(), inner.as_path()], &CrawlOptions::default());
    let n: Vec<_> = r.candidates.iter().map(|c| c.path.rsplit('/').next().unwrap().to_string()).collect();
    assert_eq!(n, vec!["trip.txt"]);
}

// ---- extra coverage ----

#[test]
fn extension_and_magic_tables() {
    let o = CrawlOptions::default();
    let p = |n: &str| std::path::PathBuf::from(format!("/nonexistent/{n}"));
    assert_eq!(classify(&p("a.JPG"), &o), Classification::Index(FileKind::Image));
    assert_eq!(classify(&p("a.pdf"), &o), Classification::Index(FileKind::Pdf));
    assert_eq!(classify(&p("a.flac"), &o), Classification::Index(FileKind::Audio));
    assert_eq!(classify(&p("a.mkv"), &o), Classification::Index(FileKind::Video));
    assert_eq!(classify(&p("a.svg"), &o), Classification::Ignore);
    assert_eq!(classify(&p("Screenshot 1.png"), &o), Classification::Index(FileKind::Screenshot));
    assert_eq!(classify(&p("screenshot 1.png"), &o), Classification::Index(FileKind::Image)); // case-sensitive
    assert_eq!(classify(&p("x.m2ts"), &o), Classification::Skip("code or data file")); // unreadable => not TS
    let o2 = CrawlOptions { excluded_extensions: ["pdf".to_string()].into(), ..Default::default() };
    assert_eq!(classify(&p("a.pdf"), &o2), Classification::Skip("excluded type"));
    assert_eq!(media_by_magic(b"%PDF-1.7 xxxxxxxxxxxx"), Some(Media::Pdf));
    assert_eq!(media_by_magic(&[0xFF, 0xD8, 0xFF, 0xE0, 0, 0, 0, 0, 0, 0, 0, 0]), Some(Media::Image));
    assert_eq!(media_by_magic(b"\0\0\0\x18ftypheic\0\0\0\0"), Some(Media::Image));
    assert_eq!(media_by_magic(b"\0\0\0\x18ftypM4A \0\0\0\0"), Some(Media::Audio));
    assert_eq!(media_by_magic(b"\0\0\0\x18ftypisom\0\0\0\0"), Some(Media::Video));
    assert_eq!(media_by_magic(b"RIFF\x24\0\0\0WAVEfmt "), Some(Media::Audio));
    assert_eq!(media_by_magic(b"hello world, plain"), None);
}

#[test]
fn ts_sniff_boundaries() {
    assert!(!is_mpeg_ts_bytes(&vec![0x47; 399])); // need >= 400
    let mut m2ts = vec![0u8; 400];
    for o in [4, 196, 388] {
        m2ts[o] = 0x47;
    }
    assert!(is_mpeg_ts_bytes(&m2ts));
    assert!(!is_mpeg_ts_bytes(&vec![0u8; 600]));
}

#[test]
fn magic_sniff_is_opt_in() {
    let d = TempDir::new().unwrap();
    let f = d.path().join("noext");
    fs::write(&f, b"%PDF-1.4 and some more bytes to read").unwrap();
    assert_eq!(classify(&f, &CrawlOptions::default()), Classification::Ignore);
    let o = CrawlOptions { sniff_magic: true, ..Default::default() };
    assert_eq!(classify(&f, &o), Classification::Index(FileKind::Pdf));
}

#[test]
fn secrets_and_code_classification() {
    for n in ["server.PEM", "id_rsa", "id_rsa.pub", ".env.local", "Secrets.txt", "credentials.json", ".npmrc", "x.mobileprovision"] {
        assert!(is_secret(n), "{n}");
    }
    assert!(!is_secret("main.rs"));
    let o = CrawlOptions { code: true, ..Default::default() };
    let p = |n: &str| std::path::PathBuf::from(format!("/nonexistent/{n}"));
    assert_eq!(classify_code(&p("id_rsa"), &o), Classification::Skip("key or password file"));
    assert_eq!(classify_code(&p("app.min.js"), &o), Classification::Skip("minified"));
    assert_eq!(classify_code(&p("LICENSE.md"), &o), Classification::Skip("license"));
    assert_eq!(classify_code(&p("main.rs"), &o), Classification::Index(FileKind::Code));
    assert_eq!(classify_code(&p("Dockerfile.dev"), &o), Classification::Index(FileKind::Code));
    assert_eq!(classify_code(&p("data.json"), &o), Classification::Skip("data file"));
    assert_eq!(classify_code(&p("photo.jpg"), &o), Classification::Ignore);
    assert_eq!(classify_code(&p("notebook.ipynb"), &o), Classification::Index(FileKind::Code));
}

#[test]
fn skip_rules_hidden_bundles_dev_folders() {
    let d = TempDir::new().unwrap();
    let r = d.path();
    for (p, t) in [
        ("keep/a.txt", "a"),
        (".hidden/b.txt", "b"),
        ("node_modules/c.txt", "c"),
        ("x.noindex/d.txt", "d"),
        ("My.app/e.txt", "e"),
        ("keep/.dot.txt", "f"),
        ("keep/unknown.zzz", "g"),
        ("keep/main.py", "h"),
    ] {
        let f = r.join(p);
        fs::create_dir_all(f.parent().unwrap()).unwrap();
        fs::write(f, t).unwrap();
    }
    let rep = crawl(&[r], &CrawlOptions::default());
    assert_eq!(names(&rep, r), vec!["keep/a.txt"]);
    assert_eq!(rep.skipped_folders["developer folder"], 1);
    assert_eq!(rep.skipped_folders[".noindex folder"], 1);
    assert_eq!(rep.skipped["app or bundle"], 1);
    assert_eq!(rep.skipped["other type"], 1);
    assert_eq!(rep.skipped["code or data file"], 1);
}

#[test]
fn project_folders_index_only_screenshots() {
    let d = TempDir::new().unwrap();
    let r = d.path();
    for p in ["proj/Cargo.toml", "proj/notes.txt", "proj/docs/guide.md", "proj/shots/Screenshot 1.png", "proj/pic.jpg", "free/notes.txt"] {
        let f = r.join(p);
        fs::create_dir_all(f.parent().unwrap()).unwrap();
        fs::write(f, "x").unwrap();
    }
    let rep = crawl(&[r], &CrawlOptions::default());
    assert_eq!(names(&rep, r), vec!["free/notes.txt", "proj/shots/Screenshot 1.png"]);
    assert_eq!(rep.skipped["inside a code project"], 3);
    // A project chosen as the root itself is also a project.
    let rep = crawl(&[r.join("proj")], &CrawlOptions::default());
    assert_eq!(rep.candidates.len(), 1);
}

#[test]
fn datasets_are_dropped_unless_included_or_root() {
    let d = TempDir::new().unwrap();
    let r = d.path();
    fs::create_dir_all(r.join("clips")).unwrap();
    for i in 0..300 {
        fs::write(r.join(format!("clips/clip_{i:06}.wav")), "x").unwrap();
    }
    fs::write(r.join("clips/readme.txt"), "x").unwrap();
    let rep = crawl(&[r], &CrawlOptions::default());
    assert_eq!(names(&rep, r), vec!["clips/readme.txt"]);
    assert_eq!(rep.skipped["dataset"], 300);
    assert_eq!(rep.datasets[&format!("{}/clips", r.display())], 300);
    let rep = crawl(&[r], &CrawlOptions { include_datasets: true, ..Default::default() });
    assert_eq!(rep.candidates.len(), 301);
    let rep = crawl(&[r.join("clips")], &CrawlOptions::default()); // chosen roots are never datasets
    assert_eq!(rep.candidates.len(), 301);
}

#[cfg(unix)]
#[test]
fn symlinks_are_not_followed_and_stat_fields_filled() {
    use std::os::unix::fs::{symlink, MetadataExt};
    let d = TempDir::new().unwrap();
    let r = d.path();
    fs::write(r.join("real.txt"), "hello").unwrap();
    symlink(r.join("real.txt"), r.join("link.txt")).unwrap();
    let rep = crawl(&[r], &CrawlOptions::default());
    assert_eq!(names(&rep, r), vec!["real.txt"]);
    assert_eq!(rep.skipped["link or special file"], 1);
    let md = fs::metadata(r.join("real.txt")).unwrap();
    let c = &rep.candidates[0];
    assert_eq!((c.size, c.inode, c.device), (5, md.ino(), md.dev()));
    assert!((c.mtime - (md.mtime() as f64 + md.mtime_nsec() as f64 / 1e9)).abs() < 1e-6);
}

#[test]
fn code_mode_crawl() {
    let d = TempDir::new().unwrap();
    let r = d.path();
    for (p, t) in [
        ("app/main.py", "print(1)"),
        ("app/README.md", "hi"),
        ("app/vendor/x.py", "x"),
        ("app/server.pem", "k"),
        ("app/data.csv", "1,2"),
        ("app/.git/config", "x"),
        ("app/sub/.git", "gitdir: ../x"),
        ("app/sub/y.py", "y"),
    ] {
        let f = r.join(p);
        fs::create_dir_all(f.parent().unwrap()).unwrap();
        fs::write(f, t).unwrap();
    }
    fs::write(r.join("app/big.py"), vec![b'a'; (1 << 20) + 1]).unwrap();
    let o = CrawlOptions { code: true, ..Default::default() };
    let rep = crawl(&[r], &o);
    assert_eq!(names(&rep, r), vec!["app/README.md", "app/main.py"]);
    assert_eq!(rep.skipped["key or password file"], 1);
    assert_eq!(rep.skipped["data file"], 1);
    assert_eq!(rep.skipped["too big to be written by hand"], 1);
    assert_eq!(rep.skipped_folders["vendored or built code"], 1);
    assert_eq!(rep.skipped_folders["submodule or worktree"], 1);
}

#[test]
fn parallel_matches_serial_on_wide_tree() {
    let d = TempDir::new().unwrap();
    let r = d.path();
    for i in 0..30 {
        let dir = r.join(format!("d{i}/e"));
        fs::create_dir_all(&dir).unwrap();
        for j in 0..40 {
            fs::write(dir.join(format!("f{j}.txt")), "x").unwrap();
        }
    }
    for j in 0..1200 {
        fs::write(r.join(format!("top{j}.md")), "x").unwrap(); // exceeds the per-directory chunk size
    }
    let a = crawl(&[r], &CrawlOptions { threads: 1, ..Default::default() });
    let b = crawl(&[r], &CrawlOptions { threads: 4, ..Default::default() });
    assert_eq!(a.candidates.len(), 30 * 40 + 1200);
    assert_eq!(a.candidates, b.candidates);
}
