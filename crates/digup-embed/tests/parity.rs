//! Numerical parity with DigUp's Swift app: the same nine inputs must give vectors with cosine >= 0.999
//! against the reference vectors the Swift project ships (Tests/LlamaRuntimeTests/ReferenceVectorTests.swift).
//! Skipped when the model files are absent. Set DIGUP_MODELS to the folder holding the two GGUF files.

use digup_embed::{cosine, Embedder, Options};
use serde::Deserialize;
use std::collections::BTreeMap;
use std::path::PathBuf;

#[derive(Deserialize)]
struct Reference {
    vector_version: u32,
    vectors: BTreeMap<String, Vec<f32>>,
}

fn query(t: &str) -> String {
    digup_core::query_prompt(t, false)
}

#[test]
fn matches_swift_reference_vectors() {
    let models = std::env::var("DIGUP_MODELS").map(PathBuf::from).unwrap_or_else(|_| PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../models"));
    let (text, mmproj) = (models.join("embeddinggemma-2-Q8_0.gguf"), models.join("mmproj-embeddinggemma-2-Q8_0.gguf"));
    if !text.exists() || !mmproj.exists() {
        eprintln!("SKIPPED: model files not found in {}", models.display());
        return;
    }
    let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/reference");
    let reference: Reference = serde_json::from_slice(&std::fs::read(dir.join("vectors.json")).unwrap()).unwrap();
    assert_eq!(reference.vector_version, 1);

    let t0 = std::time::Instant::now();
    let mut e = Embedder::new(&text, Some(&mmproj), &Options::default()).unwrap();
    eprintln!("load + warm-up: {:.1}s", t0.elapsed().as_secs_f64());

    let long: String = (1..=60)
        .map(|i| format!("Note {i}: the review moved to room {} because the projector in the main hall failed again, and the team agreed to send the slides the night before.", i % 7 + 2))
        .collect::<Vec<_>>()
        .join(" ");
    let texts: Vec<(&str, String)> = vec![
        ("query", query("payment declined error")),
        ("query-bn", query("ইলিশ মাছ রান্না")),
        ("query-ar", query("متى تغلق المكتبة")),
        ("document", digup_core::doc_prompt(Some("Tokyo itinerary"), "Flight to Tokyo Haneda on November 3. Then the train to Kamakura to see the Great Buddha, and back for dinner in Shibuya.")),
        ("document-long", digup_core::doc_prompt(None, &long)),
    ];

    let mut got: BTreeMap<String, Vec<f32>> = BTreeMap::new();
    let t = std::time::Instant::now();
    let vs = e.embed_texts(&texts.iter().map(|(_, s)| s.clone()).collect::<Vec<_>>()).unwrap();
    eprintln!("5 texts in one call: {:.2}s", t.elapsed().as_secs_f64());
    for ((name, _), v) in texts.iter().zip(vs) {
        got.insert(name.to_string(), v);
    }
    for (name, file, budget) in [("screen-280", "screen.png", 280), ("photo-140", "photo.jpg", 140), ("frame-70", "photo.jpg", 70)] {
        let t = std::time::Instant::now();
        got.insert(name.into(), e.embed_image_file(&dir.join(file), budget).unwrap());
        eprintln!("{name}: {:.2}s", t.elapsed().as_secs_f64());
    }
    let t = std::time::Instant::now();
    got.insert("speech".into(), e.embed_audio_file(&dir.join("speech.wav")).unwrap());
    eprintln!("speech: {:.2}s", t.elapsed().as_secs_f64());

    assert_eq!(got.keys().collect::<Vec<_>>(), reference.vectors.keys().collect::<Vec<_>>());
    let mut worst = (1.0f32, "");
    let mut failures = vec![];
    for (name, v) in &got {
        let c = cosine(v, &reference.vectors[name]);
        eprintln!("{name:<14} cosine {c:.6}");
        if c < worst.0 {
            worst = (c, name);
        }
        if c < 0.999 {
            failures.push(format!("{name}={c:.5}"));
        }
    }
    assert!(failures.is_empty(), "below 0.999: {failures:?}");
}
