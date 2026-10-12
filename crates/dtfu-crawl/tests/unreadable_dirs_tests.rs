//! A folder that cannot be listed (drive unplugged, no permission) must be reported, not silently treated as empty.
#![cfg(unix)]
use dtfu_crawl::{crawl, CrawlOptions};
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use tempfile::TempDir;

/// Restores the directory's mode on drop so TempDir cleanup works even when an assertion fails.
struct RestoreMode(PathBuf);
impl Drop for RestoreMode {
    fn drop(&mut self) {
        let _ = fs::set_permissions(&self.0, fs::Permissions::from_mode(0o755));
    }
}

fn make_tree(root: &Path) {
    fs::create_dir_all(root.join("a")).unwrap();
    fs::create_dir_all(root.join("b")).unwrap();
    fs::write(root.join("a/x.txt"), b"hello world").unwrap();
    fs::write(root.join("b/y.txt"), b"hidden").unwrap();
}

#[test]
fn unlistable_folder_is_reported_and_siblings_still_crawled() {
    if unsafe { libc::geteuid() } == 0 {
        eprintln!("skipping: running as root, chmod 000 does not block reads");
        return;
    }
    let d = TempDir::new().unwrap();
    let root = d.path().join("root");
    make_tree(&root);
    let b = root.join("b");
    fs::set_permissions(&b, fs::Permissions::from_mode(0o000)).unwrap();
    let _restore = RestoreMode(b.clone());

    let report = crawl(&[&root], &CrawlOptions::default());

    let expected = format!("{}/root/b", d.path().to_string_lossy().replace('\\', "/"));
    assert_eq!(report.unreadable_dirs, vec![expected], "unreadable_dirs");
    assert!(
        report.candidates.iter().any(|c| c.path.ends_with("/a/x.txt")),
        "sibling x.txt must still be crawled: {:?}",
        report.candidates.iter().map(|c| &c.path).collect::<Vec<_>>()
    );
    assert!(!report.candidates.iter().any(|c| c.path.ends_with("y.txt")));
}

#[test]
fn normal_crawl_has_no_unreadable_dirs() {
    let d = TempDir::new().unwrap();
    let root = d.path().join("root");
    make_tree(&root);

    let report = crawl(&[&root], &CrawlOptions::default());

    assert!(report.unreadable_dirs.is_empty(), "{:?}", report.unreadable_dirs);
    assert!(report.candidates.iter().any(|c| c.path.ends_with("/a/x.txt")));
}
