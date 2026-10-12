//! Empty inputs must not fail the batch and must keep their place. Like the Swift app, "" tokenizes to BOS+EOS and
//! gets a real vector (an empty token list, from tokenizer overflow, gets a zero vector instead).
//! Skipped when the model files are absent (see parity.rs).

use dtfu_embed::{Embedder, Options};
use std::path::PathBuf;

#[test]
fn empty_texts_keep_their_place_in_the_batch() {
    let models = std::env::var("DTFU_MODELS").map(PathBuf::from).unwrap_or_else(|_| PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../models"));
    let (text, mmproj) = (models.join("embeddinggemma-2-Q8_0.gguf"), models.join("mmproj-embeddinggemma-2-Q8_0.gguf"));
    if !text.exists() || !mmproj.exists() {
        eprintln!("SKIPPED: model files not found in {}", models.display());
        return;
    }
    let mut e = Embedder::new(&text, Some(&mmproj), &Options::default()).unwrap();
    let texts = ["", "hello world", ""].map(String::from);
    let vs = e.embed_texts(&texts).unwrap();
    assert_eq!(vs.len(), 3);
    assert_eq!(vs[0].len(), dtfu_embed::DIM);
    assert_eq!(vs[2].len(), dtfu_embed::DIM);
    assert!(vs.iter().all(|v| v.iter().all(|x| x.is_finite())));
    assert_eq!(vs[0], vs[2], "the two empty inputs must embed identically");
    assert_ne!(vs[0], vs[1], "an empty input must not take the neighbour's vector");
    let alone = e.embed_texts(&["hello world".to_string()]).unwrap();
    let cos: f32 = alone[0].iter().zip(&vs[1]).map(|(a, b)| a * b).sum();
    assert!(cos > 0.9999, "batching with empty inputs changed the neighbour's vector: cosine {cos}");
}
