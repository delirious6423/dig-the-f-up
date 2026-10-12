//! Deterministic regression: the file with the highest segment ids is deleted, SQLite reuses the ids, and rows are
//! added to another file with `IndexStore::add_segments` in the same window. `refresh` must equal a fresh load.

use digup_core::{FileKind, Modality, SegmentKind, DIM};
use digup_store::index::{IndexStore, NewFile, NewSegment};

fn vector(seed: usize) -> Vec<f32> {
    let mut v: Vec<f32> = (0..DIM).map(|i| (((i * 31 + seed * 97) % 101) as f32) - 50.0).collect();
    let n = v.iter().map(|x| x * x).sum::<f32>().sqrt();
    v.iter_mut().for_each(|x| *x /= n);
    v
}

fn seg(seed: usize, loc: f64) -> NewSegment {
    NewSegment {
        kind: SegmentKind::Chunk,
        modality: Modality::Text,
        loc: Some(loc),
        loc_end: None,
        excerpt: None,
        text: Some(format!("word{seed}")),
        vector: Some(vector(seed)),
    }
}

fn file(path: &str, n: u64) -> NewFile {
    NewFile { path: path.into(), kind: FileKind::Doc, size: 1, mtime: 1.0, inode: n, device: 1 }
}

#[test]
fn refresh_after_top_file_deleted_and_rows_added_elsewhere() {
    let dir = tempfile::tempdir().unwrap();
    let mut store = IndexStore::open(dir.path(), "index.sqlite").unwrap();
    let a = store.insert_file(&file("/a", 1), &[seg(1, 1.0)]).unwrap();
    let b = store.insert_file(&file("/b", 2), &[seg(2, 1.0), seg(3, 21.0), seg(4, 41.0)]).unwrap();
    let mut cached = digup_search::Searcher::with_owned_connection(
        IndexStore::open(dir.path(), "index.sqlite").unwrap().into_connection(),
        false,
    )
    .unwrap();
    assert_eq!(cached.vector_count(), 4);

    store.delete_file(b).unwrap();
    store.add_segments(a, &[seg(5, 21.0), seg(6, 41.0), seg(7, 61.0), seg(8, 81.0)]).unwrap();

    let stats = cached.refresh_with_threshold(10.0).unwrap();
    let fresh = digup_search::Searcher::new(store.connection()).unwrap();
    assert_eq!(cached.vector_count(), fresh.vector_count(), "{stats:?}");
    let opts = digup_search::SearchOptions { z_gate: -10.0, limit: 10, ..Default::default() };
    for seed in 0..10 {
        let q = vector(seed);
        let x = fresh.search(&q, "word5", &opts).unwrap();
        let y = cached.search(&q, "word5", &opts).unwrap();
        assert_eq!(x.len(), y.len());
        for (p, r) in x.iter().zip(&y) {
            assert_eq!((p.file_id, p.segment_id), (r.file_id, r.segment_id));
            assert_eq!(p.score.to_bits(), r.score.to_bits());
            assert_eq!(p.cosine.map(f64::to_bits), r.cosine.map(f64::to_bits));
        }
    }
}
