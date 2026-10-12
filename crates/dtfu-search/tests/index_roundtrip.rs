//! Names written by `IndexStore` must be found by `Searcher`: both sides share `dtfu_text::normalized`.

use dtfu_core::{FileKind, Modality, SegmentKind};
use dtfu_search::{SearchOptions, Searcher};
use dtfu_store::index::{IndexStore, NewFile, NewSegment};

fn temp_dir(tag: &str) -> std::path::PathBuf {
    let d = std::env::temp_dir().join(format!("dtfu-search-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    d
}

fn add(store: &mut IndexStore, path: &str, text: Option<&str>) {
    let file = NewFile { path: path.into(), kind: FileKind::Doc, size: 10, mtime: 1.0, inode: 1, device: 1 };
    let segs: Vec<NewSegment> = text
        .map(|t| NewSegment {
            kind: SegmentKind::Chunk,
            modality: Modality::None,
            loc: Some(1.0),
            loc_end: None,
            excerpt: None,
            text: Some(t.into()),
            vector: None,
        })
        .into_iter()
        .collect();
    store.insert_file(&file, &segs).unwrap();
}

fn hits(store: &IndexStore, q: &str) -> Vec<String> {
    let searcher = Searcher::new(store.connection()).unwrap();
    searcher.search(&[], q, &SearchOptions::default()).unwrap().into_iter().map(|h| h.path).collect()
}

#[test]
fn arabic_and_bengali_names_are_found_after_indexing() {
    let dir = temp_dir("names");
    let mut store = IndexStore::open(&dir, "index.sqlite").unwrap();
    // Arabic with tatweel and harakat in the file name; a plain query must still match.
    add(&mut store, "/docs/مَدرســة/تقرير.pdf", None);
    // Bengali word with a zero-width joiner inside the folder name.
    add(&mut store, "/docs/কু\u{200D}মির/note.txt", None);
    add(&mut store, "/docs/other/plain.txt", Some("nothing relevant here"));
    assert_eq!(hits(&store, "مدرسة"), vec!["/docs/مَدرســة/تقرير.pdf"]);
    assert_eq!(hits(&store, "কুমির"), vec!["/docs/কু\u{200D}মির/note.txt"]);
    let _ = std::fs::remove_dir_all(&dir);
}
