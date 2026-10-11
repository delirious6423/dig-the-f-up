//! How the reference sizes an image for the vision encoder, so llama.cpp sees the same pixels.
//! Port of Sources/LlamaRuntime/ImageSizing.swift (checked against Pillow bit for bit there and here).

/// transformers' Gemma 4 image processor: scale to fill `budget` soft tokens (each 3x3 patches of 16 px)
/// keeping the aspect ratio, each side rounded down to a multiple of 48 px. Returns (height, width).
pub fn target_size(height: usize, width: usize, budget: usize) -> Option<(usize, usize)> {
    if height == 0 || width == 0 || budget == 0 {
        return None;
    }
    let (patch, pooling) = (16usize, 3usize);
    let max_patches = budget * pooling * pooling;
    let factor = ((max_patches * patch * patch) as f64 / (height * width) as f64).sqrt();
    let side = pooling * patch;
    let mut th = (factor * height as f64 / side as f64).floor() as usize * side;
    let mut tw = (factor * width as f64 / side as f64).floor() as usize * side;
    if th == 0 && tw == 0 {
        return None;
    }
    let max_side = (max_patches / (pooling * pooling)) * side;
    if th == 0 {
        th = side;
        tw = (width / height * side).min(max_side); // integer division is intentional (matches Swift)
    } else if tw == 0 {
        tw = side;
        th = (height / width * side).min(max_side);
    }
    Some((th, tw))
}

const PRECISION: i32 = 22;

fn filter(value: f64) -> f64 {
    let x = value.abs();
    let a = -0.5;
    if x < 1.0 {
        ((a + 2.0) * x - (a + 3.0)) * x * x + 1.0
    } else if x < 2.0 {
        (((x - 5.0) * x + 8.0) * x - 4.0) * a
    } else {
        0.0
    }
}

struct Coeffs {
    bounds: Vec<usize>, // pairs: first, count
    weights: Vec<i32>,
    size: usize,
}

fn coefficients(in_size: usize, out_size: usize) -> Coeffs {
    let scale = in_size as f64 / out_size as f64;
    let filter_scale = scale.max(1.0);
    let support = 2.0 * filter_scale;
    let size = support.ceil() as usize * 2 + 1;
    let mut bounds = vec![0usize; out_size * 2];
    let mut weights = vec![0i32; out_size * size];
    let mut row = vec![0f64; size];
    for out in 0..out_size {
        let center = (out as f64 + 0.5) * scale;
        let first = ((center - support + 0.5) as i64).max(0) as usize;
        let count = (((center + support + 0.5) as i64) as usize).min(in_size) - first;
        let mut total = 0.0;
        for x in 0..size {
            row[x] = if x < count { filter(((x + first) as f64 - center + 0.5) / filter_scale) } else { 0.0 };
            total += row[x];
        }
        for x in 0..size {
            let w = if x < count && total != 0.0 { row[x] / total } else { row[x] };
            weights[out * size + x] = (w * (1u64 << PRECISION) as f64 + if w < 0.0 { -0.5 } else { 0.5 }) as i32;
        }
        bounds[out * 2] = first;
        bounds[out * 2 + 1] = count;
    }
    Coeffs { bounds, weights, size }
}

#[inline]
fn clip(v: i32) -> u8 {
    (v >> PRECISION).clamp(0, 255) as u8
}

fn horizontal(src: &[u8], in_w: usize, rows: usize, out_w: usize) -> Vec<u8> {
    let c = coefficients(in_w, out_w);
    let mut out = vec![0u8; out_w * rows * 3];
    for y in 0..rows {
        for x in 0..out_w {
            let first = c.bounds[x * 2];
            let n = c.bounds[x * 2 + 1];
            let mut acc = [1i32 << (PRECISION - 1); 3];
            let base = (y * in_w + first) * 3;
            for k in 0..n {
                let w = c.weights[x * c.size + k];
                let p = base + k * 3;
                for ch in 0..3 {
                    acc[ch] = acc[ch].wrapping_add((src[p + ch] as i32).wrapping_mul(w));
                }
            }
            let o = (y * out_w + x) * 3;
            for ch in 0..3 {
                out[o + ch] = clip(acc[ch]);
            }
        }
    }
    out
}

fn vertical(src: &[u8], in_h: usize, row_w: usize, out_h: usize) -> Vec<u8> {
    let c = coefficients(in_h, out_h);
    let stride = row_w * 3;
    let mut out = vec![0u8; stride * out_h];
    let mut sums = vec![0i32; stride];
    for y in 0..out_h {
        sums.iter_mut().for_each(|s| *s = 1 << (PRECISION - 1));
        for k in 0..c.bounds[y * 2 + 1] {
            let row = &src[(c.bounds[y * 2] + k) * stride..][..stride];
            let w = c.weights[y * c.size + k];
            for i in 0..stride {
                sums[i] = sums[i].wrapping_add((row[i] as i32).wrapping_mul(w));
            }
        }
        for i in 0..stride {
            out[y * stride + i] = clip(sums[i]);
        }
    }
    out
}

/// Pillow's bicubic resampling of packed RGB8 (a = -0.5, kernel widened when shrinking, 22-bit fixed point).
pub fn resize_bicubic(src: &[u8], width: usize, height: usize, to_w: usize, to_h: usize) -> Vec<u8> {
    assert_eq!(src.len(), width * height * 3);
    match (to_w != width, to_h != height) {
        (true, true) => {
            let pass = horizontal(src, width, height, to_w);
            vertical(&pass, height, to_w, to_h)
        }
        (true, false) => horizontal(src, width, height, to_w),
        (false, true) => vertical(src, height, width, to_h),
        (false, false) => src.to_vec(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    include!("sizing_tests.inc");
}
