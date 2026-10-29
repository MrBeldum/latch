//! Inverse quantisation and the H.264 4x4 integer inverse transform,
//! including the Intra-16x16 luma DC and chroma DC Hadamard transforms
//! (spec 8.5.10 – 8.5.13).
//!
//! Per spec 8.5.12.1 the dequantised coefficient is
//! `d_ij = (c_ij * LevelScale4x4(qP % 6, class(i,j))) << (qP / 6)`.
//! The inverse transform itself (spec 8.5.12.2) is the scaled integer
//! DCT applied horizontally then vertically with a final `(v + 32) >> 6`
//! and a spec-mandated range check on the result (|f| <= 511 used to
//! bound hostile input; legal streams never hit it).
//!
//! All arithmetic uses wrapping ops on the multiply-and-shift path:
//! CAVLC level values come straight from the bitstream, and a hostile
//! stream can produce magnitudes that would overflow i32 in debug
//! builds. Wrapping keeps the behaviour deterministic instead of
//! panicking; the conformance tests pin the legal-input exactness.

use super::tables::{LEVEL_SCALE_4X4, QP_DIV6, QP_MOD6, scale_class};
use super::{Error, Result};

/// 4x4 inverse transform input/output block (`i32` per spec working
/// precision; inputs are post-dequant coefficients).
pub(crate) type Block4 = [i32; 16];

/// Inverse-transforms a dequantised 4x4 block in place
/// (spec 8.5.12.2). `Err` when an output coefficient would exceed the
/// spec's |r| <= 511 + 1 bound on intermediate results — legal streams
/// stay inside it; fuzz input that trips it is corrupt, not a panic.
pub(crate) fn inverse_4x4(b: &mut Block4) -> Result<()> {
    // Horizontal pass.
    for row in 0..4 {
        let p = &mut b[row * 4..row * 4 + 4];
        let t0 = p[0].wrapping_add(p[2]);
        let t1 = p[0].wrapping_sub(p[2]);
        let t2 = (p[1] >> 1).wrapping_sub(p[3]);
        let t3 = p[1].wrapping_add(p[3] >> 1);
        p[0] = t0.wrapping_add(t3);
        p[1] = t1.wrapping_add(t2);
        p[2] = t1.wrapping_sub(t2);
        p[3] = t0.wrapping_sub(t3);
    }
    // Vertical pass with the +32 >> 6 rescale.
    for col in 0..4 {
        let i0 = col;
        let i1 = col + 4;
        let i2 = col + 8;
        let i3 = col + 12;
        let t0 = b[i0].wrapping_add(b[i2]);
        let t1 = b[i0].wrapping_sub(b[i2]);
        let t2 = (b[i1] >> 1).wrapping_sub(b[i3]);
        let t3 = b[i1].wrapping_add(b[i3] >> 1);
        b[i0] = t0.wrapping_add(t3).wrapping_add(32) >> 6;
        b[i1] = t1.wrapping_add(t2).wrapping_add(32) >> 6;
        b[i2] = t1.wrapping_sub(t2).wrapping_add(32) >> 6;
        b[i3] = t0.wrapping_sub(t3).wrapping_add(32) >> 6;
        // Spec 8.5.12.2 constrains every intermediate to
        // [-2^15, 2^15-1]; a conforming stream never exceeds it.
        for &v in &[b[i0], b[i1], b[i2], b[i3]] {
            if !(-32768..=32767).contains(&v) {
                return Err(Error::BadValue(
                    "inverse transform intermediate out of range",
                ));
            }
        }
    }
    Ok(())
}

/// Dequantises one 4x4 coefficient block in place (spec 8.5.12.1,
/// eqs. 8-336/8-337). `coeffs` holds the raw CAVLC levels; products are
/// widened to i64 so hostile magnitudes can't wrap silently — the
/// inverse-transform bound check then rejects over-range blocks.
pub(crate) fn dequant_4x4(coeffs: &mut [i32; 16], qp: u8) {
    debug_assert!(qp < 52);
    let m = LEVEL_SCALE_4X4[QP_MOD6[qp as usize] as usize];
    let q6 = qp as i64 / 6;
    for i in 0..16 {
        let s = i64::from(m[scale_class(i % 4, i / 4)]);
        let c = i64::from(coeffs[i]);
        // Spec 8.5.12.1 (eq. 8-336): d_ij = c_ij * LevelScale << (qP/6)
        // at every qP — the reference implementation (h264bsd
        // levelScale[] << qpDiv, FFmpeg's quant_div6+2 table feeding a
        // >>6 normaliser) has no low-qP branch.
        let v = (c * s) << q6;
        coeffs[i] = v.clamp(i32::MIN as i64, i32::MAX as i64) as i32;
    }
}

/// Dequantises a single coefficient at raster position `(x, y)` —
/// the per-coefficient core of [`dequant_4x4`], exposed for callers
/// (Intra-16x16 AC, chroma AC) whose DC slot is filled from the
/// Hadamard path instead of `LevelScale`.
pub(crate) fn dequant_coeff(c: i32, qp: u8, x: usize, y: usize) -> i32 {
    debug_assert!(qp < 52);
    let m = LEVEL_SCALE_4X4[QP_MOD6[qp as usize] as usize];
    let q6 = qp as i64 / 6;
    let s = i64::from(m[scale_class(x, y)]);
    // Same unconditional scale as [`dequant_4x4`].
    let v = (i64::from(c) * s) << q6;
    v.clamp(i32::MIN as i64, i32::MAX as i64) as i32
}

/// 4x4 Hadamard inverse for the Intra-16x16 luma DC block
/// (spec 8.5.11.1): the DC coefficients first pass through a Hadamard
/// transform, then per spec the scaling is folded in —
/// `(w * LevelScale(qP%6, 0,0)) << (qP/6 - 2)` for `qP >= 12`, else
/// `(w * LevelScale + r) >> (2 - qP/6)` with `r = 1 << (1 - qP/6)`
/// (rounding toward nearest; for `qP/6 == 1` the offset is 1, else 2 —
/// matching the `(1 << (2 - qpDiv)) >> 1` rounding of the reference
/// implementation).
pub(crate) fn inv_luma_dc(block: &mut Block4, qp: u8) {
    debug_assert!(qp < 52);
    // 4x4 Hadamard, horizontal then vertical; the scaling is folded
    // into the vertical pass exactly as the spec's 8.5.11.1 (verified
    // against h264bsdProcessLumaDc).
    for row in 0..4 {
        let p = &mut block[row * 4..row * 4 + 4];
        let a0 = p[0].wrapping_add(p[2]);
        let a1 = p[0].wrapping_sub(p[2]);
        let a2 = p[1].wrapping_sub(p[3]);
        let a3 = p[1].wrapping_add(p[3]);
        p[0] = a0.wrapping_add(a3);
        p[1] = a1.wrapping_add(a2);
        p[2] = a1.wrapping_sub(a2);
        p[3] = a0.wrapping_sub(a3);
    }
    let ls = i64::from(LEVEL_SCALE_4X4[QP_MOD6[qp as usize] as usize][0]);
    let qp_div = i64::from(QP_DIV6[qp as usize]);
    for col in 0..4 {
        let i0 = col;
        let i1 = col + 4;
        let i2 = col + 8;
        let i3 = col + 12;
        let w0 = i64::from(block[i0]).wrapping_add(i64::from(block[i2]));
        let w1 = i64::from(block[i0]).wrapping_sub(i64::from(block[i2]));
        let w2 = i64::from(block[i1]).wrapping_sub(i64::from(block[i3]));
        let w3 = i64::from(block[i1]).wrapping_add(i64::from(block[i3]));
        let w = [
            w0.wrapping_add(w3),
            w1.wrapping_add(w2),
            w1.wrapping_sub(w2),
            w0.wrapping_sub(w3),
        ];
        let idx = [i0, i1, i2, i3];
        for (k, &i) in idx.iter().enumerate() {
            block[i] = if qp >= 12 {
                (w[k] * (ls << (qp_div - 2))).clamp(i32::MIN as i64, i32::MAX as i64) as i32
            } else {
                let r = if qp_div == 1 { 1 } else { 2 };
                ((w[k] * ls + r) >> (2 - qp_div)).clamp(i32::MIN as i64, i32::MAX as i64) as i32
            };
        }
    }
}

/// 2x2 Hadamard inverse for a chroma DC block (spec 8.5.11.2) with the
/// scaling folded in, matching the reference decoder
/// (`h264bsdProcessChromaDc`): for `qP >= 6` each transformed value is
/// `w * (LevelScale(qP%6,0,0) << (qP/6 - 1))`; for `qP < 6` it is
/// `(w * LevelScale) >> 1`. Input and return order are 2x2 raster:
/// `[c00, c01, c10, c11]`.
pub(crate) fn inv_chroma_dc(dc: &[i32; 4], qp: u8) -> [i32; 4] {
    debug_assert!(qp < 52);
    let a0 = i64::from(dc[0]) + i64::from(dc[2]);
    let a1 = i64::from(dc[0]) - i64::from(dc[2]);
    let a2 = i64::from(dc[1]) - i64::from(dc[3]);
    let a3 = i64::from(dc[1]) + i64::from(dc[3]);
    // W2 * C * W2ᵀ in raster order: [f00, f01, f10, f11].
    let f = [a0 + a3, a0 - a3, a1 + a2, a1 - a2];
    let ls = i64::from(LEVEL_SCALE_4X4[QP_MOD6[qp as usize] as usize][0]);
    let qp_div = i64::from(QP_DIV6[qp as usize]);
    let mut out = [0i32; 4];
    for (o, &w) in out.iter_mut().zip(f.iter()) {
        let v = if qp >= 6 {
            w * (ls << (qp_div - 1))
        } else {
            (w * ls) >> 1
        };
        *o = v.clamp(i32::MIN as i64, i32::MAX as i64) as i32;
    }
    out
}

/// Adds a 4x4 residual block to an 8-bit prediction block in place,
/// clipping to [0, 255] (spec 8.5.13).
pub(crate) fn add_residual_4x4(pred: &mut [u8], stride: usize, residual: &Block4) {
    for y in 0..4 {
        for x in 0..4 {
            let v = i32::from(pred[y * stride + x]) + residual[y * 4 + x];
            pred[y * stride + x] = v.clamp(0, 255) as u8;
        }
    }
}

/// The 8x8 inverse quantisation scale matrix classes (spec 8.5.12.1,
/// `v8x8` rows of Table 7-4 for each `qP % 6`). The class index per
/// 8x8 raster position comes from [`scale_class8`].
const LEVEL_SCALE_8X8: [[u16; 6]; 6] = [
    [20, 18, 32, 19, 25, 24],
    [22, 19, 35, 21, 28, 26],
    [26, 23, 42, 24, 33, 31],
    [28, 25, 45, 26, 35, 33],
    [32, 28, 51, 30, 40, 38],
    [36, 32, 58, 34, 46, 43],
];

/// Raster position -> 8x8 `LevelScale8x8` class (0..5), the pattern
/// derived from spec 8.5.12.1's scale-position mapping (matches the
/// reference decoder's `dequant8_coeff_init_scan`).
const SCALE8_CLASS: [u8; 64] = [
    0, 3, 4, 3, 0, 3, 4, 3, 3, 1, 5, 1, 3, 1, 5, 1, 4, 5, 2, 5, 4, 5, 2, 5, 3, 1, 5, 1, 3, 1, 5, 1,
    0, 3, 4, 3, 0, 3, 4, 3, 3, 1, 5, 1, 3, 1, 5, 1, 4, 5, 2, 5, 4, 5, 2, 5, 3, 1, 5, 1, 3, 1, 5, 1,
];

/// Dequantises one 8x8 coefficient block in place (spec 8.5.12.1 with
/// the flat-16 scaling list). The multiplicand is
/// `LevelScale8x8(qP%6, i, j) * 16 << (qP/6)` divided by 64 — the same
/// folded form the reference decoder applies at coefficient-decode
/// time (`(c * qmul + 32) >> 6`). Products go through i64.
pub(crate) fn dequant_8x8(coeffs: &mut [i32; 64], qp: u8) {
    debug_assert!(qp < 52);
    let m = LEVEL_SCALE_8X8[QP_MOD6[qp as usize] as usize];
    let q6 = qp as i64 / 6;
    for i in 0..64 {
        let qmul = (i64::from(m[SCALE8_CLASS[i] as usize]) * 16) << q6;
        let c = i64::from(coeffs[i]);
        // (c * qmul + 32) >> 6 with sign-correct rounding toward -inf,
        // matching the reference decoder's arithmetic exactly.
        let v = (c * qmul + 32) >> 6;
        coeffs[i] = v.clamp(i32::MIN as i64, i32::MAX as i64) as i32;
    }
}

/// 8x4 block of i32 (raster order).
pub(crate) type Block8 = [i32; 64];

/// One 8x8 inverse-transform butterfly pass on the row or column
/// picked by `idx` (spec 8.5.12.3, the reference decoder's exact
/// shift-sharing implementation — the `>>1`/`>>2` sub-terms are
/// mandatory, they are not factorisations).
fn idct8_pass(b: &mut [i32], idx: [usize; 8]) {
    let c = [
        b[idx[0]], b[idx[1]], b[idx[2]], b[idx[3]], b[idx[4]], b[idx[5]], b[idx[6]], b[idx[7]],
    ];
    let a0 = c[0].wrapping_add(c[4]);
    let a2 = c[0].wrapping_sub(c[4]);
    let a4 = (c[2] >> 1).wrapping_sub(c[6]);
    let a6 = (c[6] >> 1).wrapping_add(c[2]);
    let b0 = a0.wrapping_add(a6);
    let b2 = a2.wrapping_add(a4);
    let b4 = a2.wrapping_sub(a4);
    let b6 = a0.wrapping_sub(a6);
    let a1 = c[3]
        .wrapping_neg()
        .wrapping_add(c[5])
        .wrapping_sub(c[7])
        .wrapping_sub(c[7] >> 1);
    let a3 = c[1]
        .wrapping_add(c[7])
        .wrapping_sub(c[3])
        .wrapping_sub(c[3] >> 1);
    let a5 = c[1]
        .wrapping_neg()
        .wrapping_add(c[7])
        .wrapping_add(c[5])
        .wrapping_add(c[5] >> 1);
    let a7 = c[3]
        .wrapping_add(c[5])
        .wrapping_add(c[1])
        .wrapping_add(c[1] >> 1);
    let b1 = (a7 >> 2).wrapping_add(a1);
    let b3 = a3.wrapping_add(a5 >> 2);
    let b5 = (a3 >> 2).wrapping_sub(a5);
    let b7 = a7.wrapping_sub(a1 >> 2);
    b[idx[0]] = b0.wrapping_add(b7);
    b[idx[7]] = b0.wrapping_sub(b7);
    b[idx[1]] = b2.wrapping_add(b5);
    b[idx[6]] = b2.wrapping_sub(b5);
    b[idx[2]] = b4.wrapping_add(b3);
    b[idx[5]] = b4.wrapping_sub(b3);
    b[idx[3]] = b6.wrapping_add(b1);
    b[idx[4]] = b6.wrapping_sub(b1);
}

/// The H.264 8x8 integer inverse transform (spec 8.5.12.3): row pass,
/// then column pass, matching the reference decoder's `idct8_add`
/// exactly — the passes are NOT commutative because each 1-D pass
/// contains rounding shifts (`>>1`/`>>2`), and the reference folds the
/// rounding constant into the DC before the passes.
pub(crate) fn inverse_8x8(b: &mut Block8) {
    // The reference decoder adds the rounding constant into the DC
    // coefficient BEFORE the passes (h264idct_template.c idct8_add:
    // `block[0] += 32;`) and stores with a plain arithmetic `>> 6`.
    // These are NOT interchangeable with a store-time `+32` because the
    // `>>1`/`>>2` shifts inside the butterflies round differently.
    b[0] = b[0].wrapping_add(32);
    // Rows first, columns second: combined with the ZIGZAG_8X8 storage
    // (the transpose of the reference decoder's block layout) this
    // reproduces `idct8_add` bit-exactly (see tables.rs note).
    for i in 0..8 {
        let idx = [
            i * 8,
            i * 8 + 1,
            i * 8 + 2,
            i * 8 + 3,
            i * 8 + 4,
            i * 8 + 5,
            i * 8 + 6,
            i * 8 + 7,
        ];
        idct8_pass(b, idx);
    }
    for i in 0..8 {
        let idx = [i, 8 + i, 16 + i, 24 + i, 32 + i, 40 + i, 48 + i, 56 + i];
        idct8_pass(b, idx);
    }
    // Final normalisation: plain arithmetic shift (the rounding
    // constant was already folded into the DC above).
    for v in b.iter_mut() {
        *v >>= 6;
    }
}

/// Adds an 8x8 residual block to a prediction region (`stride`
/// row-major), clipping to [0, 255].
pub(crate) fn add_residual_8x8(pred: &mut [u8], stride: usize, residual: &Block8) {
    for y in 0..8 {
        for x in 0..8 {
            let v = i32::from(pred[y * stride + x]) + residual[y * 8 + x];
            pred[y * stride + x] = v.clamp(0, 255) as u8;
        }
    }
}
