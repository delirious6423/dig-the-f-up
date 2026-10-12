//! Test helpers: a deterministic fake model (no llama.cpp) that records what it was asked to embed.

use crate::types::Embed;
use anyhow::{bail, Result};
use dtfu_core::DIM;

#[derive(Default)]
pub struct FakeEmbedder {
    pub texts: Vec<String>,
    pub images: Vec<(usize, usize, usize)>, // width, height, budget
    pub audio: Vec<usize>,                  // sample counts
    /// When set, `embed_image_rgb` fails for images whose width equals this (to test per-file failure).
    pub fail_image_width: Option<usize>,
}

/// A unit vector derived from `seed` bytes: stable across runs, different for different inputs.
pub fn unit_vector(seed: &[u8]) -> Vec<f32> {
    let mut h: u64 = 0xcbf29ce484222325;
    for b in seed {
        h = (h ^ *b as u64).wrapping_mul(0x100000001b3);
    }
    let mut v = Vec::with_capacity(DIM);
    for _ in 0..DIM {
        h ^= h << 13;
        h ^= h >> 7;
        h ^= h << 17;
        v.push(((h >> 11) as f64 / (1u64 << 53) as f64 - 0.5) as f32);
    }
    let n = v.iter().map(|x| (*x as f64).powi(2)).sum::<f64>().sqrt() as f32;
    v.iter().map(|x| x / n).collect()
}

impl Embed for FakeEmbedder {
    fn vector_source(&self) -> String {
        "fake-model".into()
    }
    fn embed_texts(&mut self, texts: &[String]) -> Result<Vec<Vec<f32>>> {
        self.texts.extend(texts.iter().cloned());
        Ok(texts.iter().map(|t| unit_vector(t.as_bytes())).collect())
    }
    fn embed_image_rgb(&mut self, width: usize, height: usize, rgb: &[u8], budget: usize) -> Result<Vec<f32>> {
        if self.fail_image_width == Some(width) {
            bail!("fake image failure");
        }
        self.images.push((width, height, budget));
        Ok(unit_vector(&rgb[..rgb.len().min(4096)]))
    }
    fn embed_audio_samples(&mut self, samples: &[f32]) -> Result<Vec<f32>> {
        self.audio.push(samples.len());
        let bytes: Vec<u8> = samples.iter().take(1024).flat_map(|s| s.to_le_bytes()).collect();
        Ok(unit_vector(&bytes))
    }
}

#[test]
fn fake_vectors_are_unit_and_stable() {
    let a = unit_vector(b"x");
    assert_eq!(a.len(), DIM);
    assert!((a.iter().map(|x| x * x).sum::<f32>() - 1.0).abs() < 1e-4);
    assert_eq!(a, unit_vector(b"x"));
    assert_ne!(a, unit_vector(b"y"));
}
