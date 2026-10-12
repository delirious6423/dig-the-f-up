//! Dot-product kernels for the brute-force vector scan.
//!
//! Rows are float16 (`DIM` per row, little-endian in the blob, `half::f16` in memory); the query is
//! float32. Every kernel converts the row to f32 on the fly and accumulates in f32, so all kernels
//! agree with `dot_naive` up to float summation order.

use dtfu_core::DIM;
use half::f16;
use rayon::prelude::*;
use std::sync::OnceLock;

/// Rows per rayon task. Big enough that scheduling cost is negligible, small enough to load-balance.
pub const BLOCK_ROWS: usize = 4096;

/// One dot-product implementation. `Lanes` is portable; the others need a CPU feature.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kernel {
    /// x86_64: AVX-512F with `_mm512_cvtph_ps`, two accumulators of 16 lanes.
    Avx512,
    /// x86_64: AVX2 + FMA + F16C, four accumulators of 8 lanes.
    Avx2,
    /// aarch64: NEON `vcvt_f32_f16` / `vfmaq_f32`, four accumulators of 4 lanes.
    Neon,
    /// Portable: eight scalar accumulators that the compiler may vectorize.
    Lanes,
}

impl Kernel {
    pub const ALL: [Kernel; 4] = [Kernel::Avx512, Kernel::Avx2, Kernel::Neon, Kernel::Lanes];

    pub fn name(self) -> &'static str {
        match self {
            Kernel::Avx512 => "avx512f",
            Kernel::Avx2 => "avx2+fma+f16c",
            Kernel::Neon => "neon",
            Kernel::Lanes => "lanes",
        }
    }

    /// True if this CPU can run the kernel. Cheap after the first call (std caches the detection).
    pub fn available(self) -> bool {
        detect(self)
    }
}

#[cfg(target_arch = "x86_64")]
fn detect(k: Kernel) -> bool {
    match k {
        Kernel::Avx512 => {
            std::arch::is_x86_feature_detected!("avx512f")
                && std::arch::is_x86_feature_detected!("avx2")
                && std::arch::is_x86_feature_detected!("fma")
                && std::arch::is_x86_feature_detected!("f16c")
        }
        Kernel::Avx2 => {
            std::arch::is_x86_feature_detected!("avx2")
                && std::arch::is_x86_feature_detected!("fma")
                && std::arch::is_x86_feature_detected!("f16c")
        }
        Kernel::Neon => false,
        Kernel::Lanes => true,
    }
}

#[cfg(target_arch = "aarch64")]
fn detect(k: Kernel) -> bool {
    // NEON (Advanced SIMD) and its half-precision conversions are part of the aarch64 baseline.
    matches!(k, Kernel::Neon | Kernel::Lanes)
}

#[cfg(not(any(target_arch = "x86_64", target_arch = "aarch64")))]
fn detect(k: Kernel) -> bool {
    k == Kernel::Lanes
}

/// The fastest kernel this CPU supports. Detected once per process.
pub fn best_kernel() -> Kernel {
    static BEST: OnceLock<Kernel> = OnceLock::new();
    *BEST.get_or_init(|| {
        [Kernel::Avx512, Kernel::Avx2, Kernel::Neon]
            .into_iter()
            .find(|k| k.available())
            .unwrap_or(Kernel::Lanes)
    })
}

/// Dot product of `q` with one row using kernel `k`. Panics if `k` is not available on this CPU.
pub fn dot_row(k: Kernel, q: &[f32], row: &[f16]) -> f32 {
    assert_eq!(q.len(), row.len());
    assert!(k.available(), "scan kernel {} is not supported on this CPU", k.name());
    match k {
        Kernel::Lanes => dot_lanes(q, row),
        #[cfg(target_arch = "x86_64")]
        // SAFETY: availability was checked above; the row length was asserted equal to the query's.
        Kernel::Avx2 => unsafe { dot_avx2(q, row) },
        #[cfg(target_arch = "x86_64")]
        // SAFETY: availability was checked above; the row length was asserted equal to the query's.
        Kernel::Avx512 => unsafe { dot_avx512(q, row) },
        #[cfg(target_arch = "aarch64")]
        // SAFETY: availability was checked above; the row length was asserted equal to the query's.
        Kernel::Neon => unsafe { dot_neon(q, row) },
        #[allow(unreachable_patterns)]
        _ => unreachable!("kernel {} is not available on this architecture", k.name()),
    }
}

/// Scores of `q` against every row of `rows` (row-major, `DIM` per row) into `out`, one thread.
pub fn dot_block(k: Kernel, q: &[f32], rows: &[f16], out: &mut [f32]) {
    assert_eq!(q.len(), DIM);
    assert_eq!(rows.len(), out.len() * DIM);
    for (o, row) in out.iter_mut().zip(rows.as_chunks::<DIM>().0) {
        *o = dot_row(k, q, row);
    }
}

/// Scores of `q` against every row, split into `BLOCK_ROWS`-row blocks across rayon's pool.
pub fn dot_all_parallel(k: Kernel, q: &[f32], rows: &[f16], out: &mut [f32]) {
    assert_eq!(q.len(), DIM);
    assert_eq!(rows.len(), out.len() * DIM);
    assert!(k.available(), "scan kernel {} is not supported on this CPU", k.name());
    out.par_chunks_mut(BLOCK_ROWS)
        .zip(rows.par_chunks(BLOCK_ROWS * DIM))
        .for_each(|(o, r)| dot_block(k, q, r, o));
}

/// Reference: one accumulator, the shape of the Swift loop.
pub fn dot_naive(q: &[f32], row: &[f16]) -> f32 {
    q.iter().zip(row).map(|(a, b)| a * b.to_f32()).sum()
}

/// Portable: eight accumulators so the compiler can vectorize.
pub fn dot_lanes(q: &[f32], row: &[f16]) -> f32 {
    let mut acc = [0f32; 8];
    for (qc, rc) in q.as_chunks::<8>().0.iter().zip(row.as_chunks::<8>().0.iter()) {
        for i in 0..8 {
            acc[i] += qc[i] * rc[i].to_f32();
        }
    }
    let mut total: f32 = acc.iter().sum();
    let tail = q.len() - q.len() % 8;
    for i in tail..q.len() {
        total += q[i] * row[i].to_f32();
    }
    total
}

#[cfg(target_arch = "x86_64")]
#[target_feature(enable = "avx2,fma,f16c")]
unsafe fn dot_avx2(q: &[f32], row: &[f16]) -> f32 {
    use std::arch::x86_64::*;
    assert_eq!(q.len(), row.len());
    let n = q.len();
    let (qp, rp) = (q.as_ptr(), row.as_ptr() as *const u16);
    // SAFETY: every load reads 8 or 32 elements starting at index `i` with `i + 32 <= n` (or `i < n` for
    // the tail), and both slices have length `n`, so all reads are in bounds. AVX2/FMA/F16C are enabled
    // by the target_feature attribute and the caller checked availability.
    unsafe {
        let (mut a0, mut a1, mut a2, mut a3) = (_mm256_setzero_ps(), _mm256_setzero_ps(), _mm256_setzero_ps(), _mm256_setzero_ps());
        let mut i = 0;
        while i + 32 <= n {
            let h0 = _mm256_cvtph_ps(_mm_loadu_si128(rp.add(i) as *const __m128i));
            let h1 = _mm256_cvtph_ps(_mm_loadu_si128(rp.add(i + 8) as *const __m128i));
            let h2 = _mm256_cvtph_ps(_mm_loadu_si128(rp.add(i + 16) as *const __m128i));
            let h3 = _mm256_cvtph_ps(_mm_loadu_si128(rp.add(i + 24) as *const __m128i));
            a0 = _mm256_fmadd_ps(_mm256_loadu_ps(qp.add(i)), h0, a0);
            a1 = _mm256_fmadd_ps(_mm256_loadu_ps(qp.add(i + 8)), h1, a1);
            a2 = _mm256_fmadd_ps(_mm256_loadu_ps(qp.add(i + 16)), h2, a2);
            a3 = _mm256_fmadd_ps(_mm256_loadu_ps(qp.add(i + 24)), h3, a3);
            i += 32;
        }
        let acc = _mm256_add_ps(_mm256_add_ps(a0, a1), _mm256_add_ps(a2, a3));
        let s = _mm_add_ps(_mm256_castps256_ps128(acc), _mm256_extractf128_ps::<1>(acc));
        let s = _mm_add_ps(s, _mm_movehl_ps(s, s));
        let s = _mm_add_ss(s, _mm_shuffle_ps::<0x55>(s, s));
        let mut total = _mm_cvtss_f32(s);
        while i < n {
            total += *qp.add(i) * f16::from_bits(*rp.add(i)).to_f32();
            i += 1;
        }
        total
    }
}

#[cfg(target_arch = "x86_64")]
#[target_feature(enable = "avx512f,avx2,fma,f16c")]
unsafe fn dot_avx512(q: &[f32], row: &[f16]) -> f32 {
    use std::arch::x86_64::*;
    assert_eq!(q.len(), row.len());
    let n = q.len();
    let (qp, rp) = (q.as_ptr(), row.as_ptr() as *const u16);
    // SAFETY: loads cover `[i, i + 32)` with `i + 32 <= n` or single elements with `i < n`; both slices
    // have length `n`. AVX-512F and F16C are enabled by the attribute and availability was checked.
    unsafe {
        let (mut a0, mut a1) = (_mm512_setzero_ps(), _mm512_setzero_ps());
        let mut i = 0;
        while i + 32 <= n {
            let h0 = _mm512_cvtph_ps(_mm256_loadu_si256(rp.add(i) as *const __m256i));
            let h1 = _mm512_cvtph_ps(_mm256_loadu_si256(rp.add(i + 16) as *const __m256i));
            a0 = _mm512_fmadd_ps(_mm512_loadu_ps(qp.add(i)), h0, a0);
            a1 = _mm512_fmadd_ps(_mm512_loadu_ps(qp.add(i + 16)), h1, a1);
            i += 32;
        }
        let mut total = _mm512_reduce_add_ps(_mm512_add_ps(a0, a1));
        while i < n {
            total += *qp.add(i) * f16::from_bits(*rp.add(i)).to_f32();
            i += 1;
        }
        total
    }
}

#[cfg(target_arch = "aarch64")]
unsafe fn dot_neon(q: &[f32], row: &[f16]) -> f32 {
    use std::arch::aarch64::*;
    assert_eq!(q.len(), row.len());
    let n = q.len();
    let (qp, rp) = (q.as_ptr(), row.as_ptr() as *const u16);
    // SAFETY: loads cover `[i, i + 16)` with `i + 16 <= n` or single elements with `i < n`; both slices
    // have length `n`. NEON is part of the aarch64 baseline.
    unsafe {
        let (mut a0, mut a1, mut a2, mut a3) = (vdupq_n_f32(0.0), vdupq_n_f32(0.0), vdupq_n_f32(0.0), vdupq_n_f32(0.0));
        let mut i = 0;
        while i + 16 <= n {
            let r0 = vreinterpretq_f16_u16(vld1q_u16(rp.add(i)));
            let r1 = vreinterpretq_f16_u16(vld1q_u16(rp.add(i + 8)));
            let f0 = vcvt_f32_f16(vget_low_f16(r0));
            let f1 = vcvt_high_f32_f16(r0);
            let f2 = vcvt_f32_f16(vget_low_f16(r1));
            let f3 = vcvt_high_f32_f16(r1);
            a0 = vfmaq_f32(a0, vld1q_f32(qp.add(i)), f0);
            a1 = vfmaq_f32(a1, vld1q_f32(qp.add(i + 4)), f1);
            a2 = vfmaq_f32(a2, vld1q_f32(qp.add(i + 8)), f2);
            a3 = vfmaq_f32(a3, vld1q_f32(qp.add(i + 12)), f3);
            i += 16;
        }
        let acc = vaddq_f32(vaddq_f32(a0, a1), vaddq_f32(a2, a3));
        let mut total = vaddvq_f32(acc);
        while i < n {
            total += *qp.add(i) * f16::from_bits(*rp.add(i)).to_f32();
            i += 1;
        }
        total
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Deterministic unit vector.
    fn unit(seed: u32) -> Vec<f32> {
        let mut s = seed.wrapping_mul(2654435761).wrapping_add(1);
        let mut v: Vec<f32> = (0..DIM)
            .map(|_| {
                s ^= s << 13;
                s ^= s >> 17;
                s ^= s << 5;
                (s as f32 / u32::MAX as f32) - 0.5
            })
            .collect();
        let n = v.iter().map(|x| x * x).sum::<f32>().sqrt();
        v.iter_mut().for_each(|x| *x /= n);
        v
    }

    fn to_f16(v: &[f32]) -> Vec<f16> {
        v.iter().map(|&x| f16::from_f32(x)).collect()
    }

    #[test]
    fn every_available_kernel_agrees_with_naive() {
        let rows: Vec<Vec<f16>> = (0..300u32).map(|i| to_f16(&unit(i))).collect();
        let flat: Vec<f16> = rows.concat();
        let q = unit(7777);
        let expect: Vec<f32> = rows.iter().map(|r| dot_naive(&q, r)).collect();
        let mut ran = 0;
        for k in Kernel::ALL.into_iter().filter(|k| k.available()) {
            ran += 1;
            let mut out = vec![0f32; rows.len()];
            dot_block(k, &q, &flat, &mut out);
            for (i, (a, b)) in expect.iter().zip(&out).enumerate() {
                assert!((a - b).abs() < 1e-4, "{} row {i}: naive {a} vs {b}", k.name());
            }
            let mut par = vec![0f32; rows.len()];
            dot_all_parallel(k, &q, &flat, &mut par);
            assert_eq!(out, par, "{} parallel differs from serial", k.name());
        }
        assert!(ran >= 1);
        assert!(best_kernel().available());
        let names: Vec<&str> = Kernel::ALL.into_iter().filter(|k| k.available()).map(|k| k.name()).collect();
        eprintln!("scan kernels checked against naive: {names:?}; best: {}", best_kernel().name());
    }

    #[test]
    fn kernels_handle_tail_lengths() {
        // DIM is a multiple of 32, so exercise the scalar tails with other lengths.
        for n in 1..=70usize {
            let q: Vec<f32> = (0..n).map(|i| (i as f32 * 0.37).sin()).collect();
            let row = to_f16(&(0..n).map(|i| (i as f32 * 0.11).cos()).collect::<Vec<_>>());
            let expect = dot_naive(&q, &row);
            for k in Kernel::ALL.into_iter().filter(|k| k.available()) {
                let got = match k {
                    Kernel::Lanes => dot_lanes(&q, &row),
                    _ => {
                        // Row kernels take any length; dot_row asserts equal lengths.
                        dot_row(k, &q, &row)
                    }
                };
                assert!((expect - got).abs() < 1e-4, "{} len {n}: {expect} vs {got}", k.name());
            }
        }
    }

    #[test]
    fn parallel_scan_covers_partial_and_multiple_blocks() {
        // 2 * 1024 + 300 rows, as in the Swift test; plus a size spanning several BLOCK_ROWS tasks.
        for rows in [2 * 1024 + 300, 2 * BLOCK_ROWS + 300] {
            let flat: Vec<f16> = (0..rows as u32).flat_map(|i| to_f16(&unit(i))).collect();
            let q = unit(9_999_999);
            let mut out = vec![0f32; rows];
            dot_all_parallel(best_kernel(), &q, &flat, &mut out);
            let expect: Vec<f32> = flat.as_chunks::<DIM>().0.iter().map(|r| dot_naive(&q, r)).collect();
            for i in 0..rows {
                assert!((out[i] - expect[i]).abs() < 1e-4, "row {i} of {rows}");
            }
        }
    }
}
