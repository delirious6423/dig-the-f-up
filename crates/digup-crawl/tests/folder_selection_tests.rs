//! Ports of FolderSelectionTests.swift. Estimator / IndexStore tests (estimatesComeBySubfolder,
//! filesUnderNestedFoldersComeOnce) belong to other crates; the crawl half of subfoldersAreNamedAsTheFolderWasGiven
//! is ported as `excluded_subfolder_leaves_nothing`.
use digup_crawl::{crawl, CrawlOptions, FolderSelection as FS, Mark};
use std::fs;
use tempfile::TempDir;

fn s(v: &[&str]) -> Vec<String> {
    v.iter().map(|x| x.to_string()).collect()
}

#[test]
fn unchecking_a_subfolder_skips_it() {
    let mut sel = FS::new(s(&["/D"]), vec![], vec![]);
    sel.set_subfolder("/D/Courses", false);
    assert_eq!(sel.skipped, s(&["/D/Courses"]));
    assert_eq!(sel.mark("/D"), Mark::Mixed);
    assert!(!sel.is_searched("/D/Courses/week1.mp4"));
    assert!(sel.is_searched("/D/Books/a.pdf"));
    sel.set_folder("/D", true);
    assert!(sel.skipped.is_empty());
    assert_eq!(sel.mark("/D"), Mark::On);
}

#[test]
fn checking_a_subfolder_chooses_it_alone() {
    let mut sel = FS::default();
    sel.set_subfolder("/D/Receipts", true);
    assert_eq!(sel.chosen, s(&["/D/Receipts"]));
    assert_eq!(sel.mark("/D"), Mark::Mixed);
    assert!(sel.is_searched("/D/Receipts/may.pdf"));
    assert!(!sel.is_searched("/D/other.pdf"));
    assert_eq!(sel.folders(), s(&["/D/Receipts"]));
    sel.set_folder("/D", true);
    assert_eq!(sel.chosen, s(&["/D"]));
    sel.set_folder("/D", false);
    assert!(sel.chosen.is_empty());
    assert_eq!(sel.mark("/D"), Mark::Off);
}

#[test]
fn unchecking_a_folder_drops_its_subfolder_choices_but_not_deeper_skips() {
    let mut sel = FS::new(s(&["/D"]), s(&["/D/A", "/D/B/old"]), vec![]);
    sel.set_folder("/D", false);
    assert!(sel.chosen.is_empty());
    assert_eq!(sel.skipped, s(&["/D/B/old"]));
    assert!(sel.effective_skips().is_empty());
}

#[test]
fn a_chosen_folder_inside_a_skipped_one_is_searched_whole() {
    let mut sel = FS::new(s(&["/D"]), s(&["/D/Media"]), vec![]);
    sel.set_subfolder("/D/Media/Courses", true);
    assert_eq!(sel.chosen, s(&["/D", "/D/Media/Courses"]));
    assert!(sel.is_searched("/D/Media/Courses/week1/a.mp4"));
    assert!(!sel.is_searched("/D/Media/film.mp4"));
    assert_eq!(sel.folders(), s(&["/D", "/D/Media/Courses"]));
    sel.set_subfolder("/D/Media/Courses/old", false);
    assert!(!sel.is_searched("/D/Media/Courses/old/b.mp4"));
}

#[test]
fn folders_inside_another_chosen_one_are_covered() {
    let sel = FS::new(s(&["/D/A", "/D", "/E"]), vec![], vec![]);
    assert_eq!(sel.folders(), s(&["/D", "/E"]));
    assert_eq!(sel.searched_as("/D/A/x.png").as_deref(), Some("/D/A"));
}

#[test]
fn kept_folders_stay_whatever_is_clicked() {
    let mut sel = FS::new(vec![], vec![], s(&["/D/Receipts"]));
    assert_eq!(sel.mark("/D"), Mark::Mixed);
    sel.set_folder("/D", true);
    assert_eq!(sel.folders(), s(&["/D"]));
    sel.set_folder("/D", false);
    assert_eq!(sel.folders(), s(&["/D/Receipts"]));
    assert_eq!(sel.mark("/D"), Mark::Mixed);
}

#[test]
fn checking_a_skipped_folder_takes_the_skip_back() {
    let mut sel = FS::new(vec![], s(&["/H/Movies"]), s(&["/H"]));
    assert!(!sel.is_searched("/H/Movies"));
    sel.set_folder("/H/Movies", true);
    assert!(sel.skipped.is_empty());
    assert!(sel.chosen.is_empty());
    sel.set_folder("/H/Movies", false);
    assert_eq!(sel.skipped, s(&["/H/Movies"]));
}

#[test]
fn paths_are_compared_by_whole_names() {
    assert!(!FS::is_inside("/a/bc", "/a/b"));
    assert!(FS::is_inside("/a/b/c", "/a/b"));
    assert!(!FS::new(s(&["/a/b"]), vec![], vec![]).is_searched("/a/bc/x"));
}

fn write(root: &std::path::Path, rel: &str, text: &str) {
    let f = root.join(rel);
    fs::create_dir_all(f.parent().unwrap()).unwrap();
    fs::write(f, text).unwrap();
}

#[test]
fn crawls_leave_skips_out_but_walk_chosen_folders_inside_them() {
    let d = TempDir::new().unwrap();
    let root = fs::canonicalize(d.path()).unwrap();
    for (p, t) in [
        ("D/top.txt", "top"),
        ("D/Media/film.txt", "film"),
        ("D/Media/Courses/week1.txt", "week one"),
        ("D/Media/Courses/Old/old.txt", "old"),
    ] {
        write(&root, p, t);
    }
    let downloads = root.join("D");
    let courses = downloads.join("Media/Courses");
    let o = CrawlOptions {
        excluded_folders: vec![
            downloads.join("Media").to_string_lossy().into(),
            courses.join("Old").to_string_lossy().into(),
        ],
        ..Default::default()
    };
    let mut names: Vec<String> = crawl(&[downloads.as_path(), courses.as_path()], &o)
        .candidates
        .iter()
        .map(|c| c.path.rsplit('/').next().unwrap().to_string())
        .collect();
    names.sort();
    assert_eq!(names, vec!["top.txt", "week1.txt"]);
}

#[test]
fn excluded_subfolder_leaves_nothing() {
    let d = TempDir::new().unwrap();
    let given = fs::canonicalize(d.path()).unwrap().join("F");
    write(&given, "A/a.txt", "notes");
    assert_eq!(crawl(&[given.as_path()], &CrawlOptions::default()).candidates.len(), 1);
    let o = CrawlOptions { excluded_folders: vec![given.join("A").to_string_lossy().into()], ..Default::default() };
    assert!(crawl(&[given.as_path()], &o).candidates.is_empty());
}
