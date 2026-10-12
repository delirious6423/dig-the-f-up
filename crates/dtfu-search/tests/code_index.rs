//! The code index: names looked up as names, hits that open at a line (ports of CodeTests.swift:231-327, with the
//! stretches written by hand where Swift runs the indexer).

use dtfu_core::{FileKind, Modality, SegmentKind};
use dtfu_search::{SearchOptions, Searcher};
use dtfu_store::index::{IndexStore, NewFile, NewSegment, CODE_FILE};

fn temp_dir(tag: &str) -> std::path::PathBuf {
    let d = std::env::temp_dir().join(format!("dtfu-search-code-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    d
}

fn stretch(loc: f64, end: f64, text: &str) -> NewSegment {
    NewSegment {
        kind: SegmentKind::Lines,
        modality: Modality::None,
        loc: Some(loc),
        loc_end: Some(end),
        excerpt: None,
        text: Some(text.into()),
        vector: None,
    }
}

fn add(store: &mut IndexStore, path: &str, stretches: &[NewSegment]) {
    let file = NewFile { path: path.into(), kind: FileKind::Code, size: 10, mtime: 1.0, inode: 1, device: 1 };
    store.insert_file(&file, stretches).unwrap();
}

fn function(name: &str, body: usize) -> String {
    let mut lines = vec![format!("def {name}(x):")];
    lines.extend((0..body).map(|i| format!("    y{i} = x + {i}")));
    lines.join("\n")
}

#[test]
fn code_index_is_a_file_of_its_own() {
    let dir = temp_dir("own");
    let mut store = IndexStore::open(&dir, CODE_FILE).unwrap();
    assert_eq!(CODE_FILE, "code.sqlite");
    let text = [function("retryWithBackoff", 22), function("circuit", 22), function("other", 22)].join("\n");
    let all: Vec<&str> = text.split('\n').collect();
    let parts = [(1usize, 24usize), (25, 48), (49, all.len())];
    let stretches: Vec<NewSegment> = parts
        .iter()
        .map(|&(a, b)| stretch(a as f64, b as f64, &all[a - 1..b].join("\n")))
        .collect();
    add(&mut store, "/billing/retry.py", &stretches);

    let searcher = Searcher::new_code(store.connection()).unwrap();
    assert!(searcher.is_code());
    let hits = searcher.search(&[], "retryWithBackoff", &SearchOptions::default()).unwrap();
    let first = hits.first().unwrap();
    assert_eq!(first.segment, SegmentKind::Lines);
    assert_eq!((first.loc, first.loc_end), (Some(1.0), Some(24.0)));
    assert_eq!(first.focus, Some(1.0));

    // The main index (index.sqlite) next to it has nothing.
    let main = IndexStore::open(&dir, "index.sqlite").unwrap();
    assert!(!Searcher::new(main.connection()).unwrap().is_code());
    assert!(Searcher::new(main.connection()).unwrap().search(&[], "retryWithBackoff", &SearchOptions::default()).unwrap().is_empty());
    assert!(dir.join("code.sqlite").exists());
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn a_name_looked_up_points_at_its_definition() {
    // The name is called many times near the top, and defined further down in a stretch of its own.
    let calls = format!(
        "func run() {{\n{}\n}}\n",
        (1..=30).map(|i| format!("    refreshAccessToken()  // call {i} of many")).collect::<Vec<_>>().join("\n")
    );
    let definition = "\nfunc refreshAccessToken() -> String {\n    return \"token\"\n}\n";
    let text = format!("{calls}{definition}");
    let all: Vec<&str> = text.split('\n').collect();
    let dir = temp_dir("definition");
    let mut store = IndexStore::open(&dir, CODE_FILE).unwrap();
    add(
        &mut store,
        "/app/Auth.swift",
        &[stretch(1.0, 25.0, &all[..25].join("\n")), stretch(26.0, all.len() as f64, &all[25..].join("\n"))],
    );
    let searcher = Searcher::with_code(store.connection(), true).unwrap();
    let hits = searcher.search(&[], "refreshAccessToken", &SearchOptions::default()).unwrap();
    let hit = hits.first().unwrap();
    assert_eq!(hit.segment, SegmentKind::Lines);
    assert_eq!(hit.focus, Some(34.0)); // the line that defines it, in the stretch that holds it
    assert!(hit.loc.unwrap() <= 34.0 && hit.loc_end.unwrap() >= 34.0);

    // Not the code index: no focus, and no lookup by name.
    let plain = Searcher::new(store.connection()).unwrap();
    let hits = plain.search(&[], "refreshAccessToken", &SearchOptions::default()).unwrap();
    assert!(hits.iter().all(|h| h.focus.is_none()));
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn words_open_at_their_line_and_code_words_weigh_less() {
    let dir = temp_dir("words");
    let mut store = IndexStore::open(&dir, CODE_FILE).unwrap();
    add(&mut store, "/x/a.py", &[stretch(40.0, 44.0, "import time\n\n\ndef retry_with_backoff(attempts=5):\n    pass")]);
    let code = Searcher::new_code(store.connection()).unwrap();
    let hit = code.search(&[], "retry", &SearchOptions::default()).unwrap().remove(0);
    assert_eq!(hit.focus, Some(43.0));
    let plain = Searcher::new(store.connection()).unwrap();
    let weighed = |s: &Searcher, w: Option<f64>| {
        s.search(&[], "retry", &SearchOptions { word_weight: w, ..SearchOptions::default() }).unwrap()[0].score
    };
    let (c, p) = (weighed(&code, None), weighed(&plain, None));
    assert!((p / c - 2.0).abs() < 1e-9, "main {p} vs code {c}");
    // An explicit weight wins on both.
    assert_eq!(weighed(&code, Some(2.0)), p);
    let _ = std::fs::remove_dir_all(&dir);
}
