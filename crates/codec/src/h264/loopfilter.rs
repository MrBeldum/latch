//! The in-loop deblocking filter's edge kernels (H.264 8.7.2.3 and
//! 8.7.2.4), shared by the encoder and the decoder: one call filters a
//! whole luma edge of 16 samples, or the same chroma edge of 8 samples
//! in both chroma planes.
//!
//! The scalar functions are the specification; NEON (aarch64) and SSE2
//! (always present on x86_64) run the same integer arithmetic on 16
//! lanes, and the tests compare them on random pictures for every
//! threshold and strength.

/// `alpha` indexed by `indexA = clip(0, 51, qP + filter_offset_a)`
/// (spec Table 8-16).
pub(crate) const ALPHA: [u8; 52] = [
    0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, //
    4, 4, 5, 6, 7, 8, 9, 10, 12, 13, 15, 17, 20, 22, 25, 28, //
    32, 36, 40, 45, 50, 56, 63, 71, 80, 90, 101, 113, 127, 144, 162, 182, //
    203, 226, 255, 255,
];

/// `beta` indexed by `indexB = clip(0, 51, qP + filter_offset_b)`
/// (spec Table 8-16).
pub(crate) const BETA: [u8; 52] = [
    0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, //
    2, 2, 2, 3, 3, 3, 3, 4, 4, 4, 6, 6, 7, 7, 8, 8, //
    9, 9, 10, 10, 11, 11, 12, 12, 13, 13, 14, 14, 15, 15, 16, 16, //
    17, 17, 18, 18,
];

/// `tc0` indexed by `indexA` and `bS - 1` (spec Table 8-17); bS is 1..=3
/// when this table applies (bS == 4 uses the strong intra filter).
pub(crate) const TC0: [[u8; 3]; 52] = [
    [0, 0, 0],
    [0, 0, 0],
    [0, 0, 0],
    [0, 0, 0],
    [0, 0, 0],
    [0, 0, 0],
    [0, 0, 0],
    [0, 0, 0],
    [0, 0, 0],
    [0, 0, 0],
    [0, 0, 0],
    [0, 0, 0],
    [0, 0, 0],
    [0, 0, 0],
    [0, 0, 0],
    [0, 0, 0],
    [0, 0, 0],
    [0, 0, 1],
    [0, 0, 1],
    [0, 0, 1],
    [0, 0, 1],
    [0, 1, 1],
    [0, 1, 1],
    [1, 1, 1],
    [1, 1, 1],
    [1, 1, 1],
    [1, 1, 1],
    [1, 1, 2],
    [1, 1, 2],
    [1, 1, 2],
    [1, 1, 2],
    [1, 2, 3],
    [1, 2, 3],
    [2, 2, 3],
    [2, 2, 4],
    [2, 3, 4],
    [2, 3, 4],
    [3, 3, 5],
    [3, 4, 6],
    [3, 4, 6],
    [4, 5, 7],
    [4, 5, 8],
    [4, 6, 9],
    [5, 7, 10],
    [6, 8, 11],
    [6, 8, 13],
    [7, 10, 14],
    [8, 11, 16],
    [9, 12, 18],
    [10, 13, 20],
    [11, 15, 23],
    [13, 17, 25],
];

/// One edge's thresholds (8.7.2.2) and, per group of samples along it
/// (four luma samples, two chroma samples), the boundary strength and
/// tC0 for that strength (Table 8-17).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Edge {
    pub alpha: u8,
    pub beta: u8,
    pub bs: [u8; 4],
    pub tc0: [u8; 4],
}

impl Edge {
    fn idle(&self) -> bool {
        self.alpha == 0 || self.beta == 0 || self.bs == [0; 4]
    }
}

/// Filters the luma edge whose first q0 sample is `q0`: a vertical edge
/// (samples across it are horizontal neighbours, rows run down) when
/// `vertical`, else a horizontal one.
pub(crate) fn luma(plane: &mut [u8], stride: usize, q0: usize, vertical: bool, edge: &Edge) {
    if edge.idle() {
        return;
    }
    let (across, along) = if vertical { (1, stride) } else { (stride, 1) };
    // Every sample either kernel touches: p3 of the first to q3 of the last.
    assert!(q0 >= 4 * across && q0 + 3 * across + 15 * along < plane.len());
    let strong = edge.bs == [4; 4];
    if strong || edge.bs.iter().all(|&b| b < 4) {
        #[cfg(target_arch = "aarch64")]
        // SAFETY: the assertion above bounds every load and store.
        unsafe {
            return neon::luma(plane, stride, q0, vertical, edge, strong);
        }
        #[cfg(target_arch = "x86_64")]
        // SAFETY: as above; SSE2 is part of x86_64.
        unsafe {
            return sse2::luma(plane, stride, q0, vertical, edge, strong);
        }
    }
    #[allow(unreachable_code)]
    luma_scalar(plane, stride, q0, vertical, edge);
}

/// Filters the same chroma edge, of 8 samples, in `u` and `v`; `v_edge`
/// holds the second plane's thresholds when its QP differs.
pub(crate) fn chroma(
    u: &mut [u8],
    v: &mut [u8],
    stride: usize,
    q0: usize,
    vertical: bool,
    u_edge: &Edge,
    v_edge: &Edge,
) {
    let (across, along) = if vertical { (1, stride) } else { (stride, 1) };
    assert!(q0 >= 4 * across && q0 + 3 * across + 7 * along < u.len().min(v.len()));
    if u_edge.idle() && v_edge.idle() {
        return;
    }
    let strong = u_edge.bs == [4; 4];
    if u_edge == v_edge && (strong || u_edge.bs.iter().all(|&b| b < 4)) {
        #[cfg(target_arch = "aarch64")]
        // SAFETY: the assertion above bounds every load and store.
        unsafe {
            return neon::chroma(u, v, stride, q0, vertical, u_edge, strong);
        }
        #[cfg(target_arch = "x86_64")]
        // SAFETY: as above; SSE2 is part of x86_64.
        unsafe {
            return sse2::chroma(u, v, stride, q0, vertical, u_edge, strong);
        }
    }
    #[allow(unreachable_code)]
    {
        chroma_scalar(u, stride, q0, vertical, u_edge);
        chroma_scalar(v, stride, q0, vertical, v_edge);
    }
}

pub(crate) fn luma_scalar(plane: &mut [u8], stride: usize, q0: usize, vertical: bool, e: &Edge) {
    let (across, along) = if vertical { (1, stride) } else { (stride, 1) };
    let (alpha, beta) = (e.alpha as i32, e.beta as i32);
    for i in 0..16 {
        let bs = e.bs[i / 4];
        if bs == 0 {
            continue;
        }
        let q = q0 + i * along;
        let at = |plane: &[u8], k: isize| plane[(q as isize + k * across as isize) as usize] as i32;
        let (p0, q0_) = (at(plane, -1), at(plane, 0));
        let (p1, q1) = (at(plane, -2), at(plane, 1));
        if (p0 - q0_).abs() >= alpha || (p1 - p0).abs() >= beta || (q1 - q0_).abs() >= beta {
            continue;
        }
        let (p2, q2) = (at(plane, -3), at(plane, 2));
        let (p3, q3) = (at(plane, -4), at(plane, 3));
        let ap = (p2 - p0).abs() < beta;
        let aq = (q2 - q0_).abs() < beta;
        let mut set = |k: isize, v: i32| {
            plane[(q as isize + k * across as isize) as usize] = v.clamp(0, 255) as u8;
        };
        if bs == 4 {
            let small = (p0 - q0_).abs() < (alpha >> 2) + 2;
            if ap && small {
                set(-1, (p2 + 2 * p1 + 2 * p0 + 2 * q0_ + q1 + 4) >> 3);
                set(-2, (p2 + p1 + p0 + q0_ + 2) >> 2);
                set(-3, (2 * p3 + 3 * p2 + p1 + p0 + q0_ + 4) >> 3);
            } else {
                set(-1, (2 * p1 + p0 + q1 + 2) >> 2);
            }
            if aq && small {
                set(0, (p1 + 2 * p0 + 2 * q0_ + 2 * q1 + q2 + 4) >> 3);
                set(1, (p0 + q0_ + q1 + q2 + 2) >> 2);
                set(2, (2 * q3 + 3 * q2 + q1 + q0_ + p0 + 4) >> 3);
            } else {
                set(0, (2 * q1 + q0_ + p1 + 2) >> 2);
            }
        } else {
            let tc0 = e.tc0[i / 4] as i32;
            let tc = tc0 + i32::from(ap) + i32::from(aq);
            let delta = (((q0_ - p0) * 4 + (p1 - q1) + 4) >> 3).clamp(-tc, tc);
            set(-1, p0 + delta);
            set(0, q0_ - delta);
            if ap {
                set(
                    -2,
                    p1 + ((p2 + ((p0 + q0_ + 1) >> 1) - 2 * p1) >> 1).clamp(-tc0, tc0),
                );
            }
            if aq {
                set(
                    1,
                    q1 + ((q2 + ((p0 + q0_ + 1) >> 1) - 2 * q1) >> 1).clamp(-tc0, tc0),
                );
            }
        }
    }
}

pub(crate) fn chroma_scalar(plane: &mut [u8], stride: usize, q0: usize, vertical: bool, e: &Edge) {
    if e.idle() {
        return;
    }
    let (across, along) = if vertical { (1, stride) } else { (stride, 1) };
    let (alpha, beta) = (e.alpha as i32, e.beta as i32);
    for i in 0..8 {
        let bs = e.bs[i / 2];
        if bs == 0 {
            continue;
        }
        let q = q0 + i * along;
        let at = |plane: &[u8], k: isize| plane[(q as isize + k * across as isize) as usize] as i32;
        let (p0, q0_) = (at(plane, -1), at(plane, 0));
        let (p1, q1) = (at(plane, -2), at(plane, 1));
        if (p0 - q0_).abs() >= alpha || (p1 - p0).abs() >= beta || (q1 - q0_).abs() >= beta {
            continue;
        }
        let (np, nq) = if bs == 4 {
            ((2 * p1 + p0 + q1 + 2) >> 2, (2 * q1 + q0_ + p1 + 2) >> 2)
        } else {
            let tc = e.tc0[i / 2] as i32 + 1;
            let delta = (((q0_ - p0) * 4 + (p1 - q1) + 4) >> 3).clamp(-tc, tc);
            (p0 + delta, q0_ - delta)
        };
        plane[q - across] = np.clamp(0, 255) as u8;
        plane[q] = nq.clamp(0, 255) as u8;
    }
}

/// Per-lane bS and tC0 for 16 lanes: luma groups of four, or chroma
/// groups of two repeated for the second plane.
fn lanes(e: &Edge, chroma: bool) -> ([u8; 16], [u8; 16]) {
    let group = |i: usize| if chroma { (i % 8) / 2 } else { i / 4 };
    (
        std::array::from_fn(|i| e.bs[group(i)]),
        std::array::from_fn(|i| e.tc0[group(i)]),
    )
}

#[cfg(target_arch = "aarch64")]
mod neon {
    use super::{lanes, Edge};
    use std::arch::aarch64::*;

    /// Transposes two 8x8 byte blocks at once (low and high halves).
    #[inline(always)]
    unsafe fn transpose(r: [uint8x16_t; 8]) -> [uint8x16_t; 8] {
        let t0 = vtrnq_u8(r[0], r[1]);
        let t1 = vtrnq_u8(r[2], r[3]);
        let t2 = vtrnq_u8(r[4], r[5]);
        let t3 = vtrnq_u8(r[6], r[7]);
        let u0 = vtrnq_u16(vreinterpretq_u16_u8(t0.0), vreinterpretq_u16_u8(t1.0));
        let u1 = vtrnq_u16(vreinterpretq_u16_u8(t0.1), vreinterpretq_u16_u8(t1.1));
        let u2 = vtrnq_u16(vreinterpretq_u16_u8(t2.0), vreinterpretq_u16_u8(t3.0));
        let u3 = vtrnq_u16(vreinterpretq_u16_u8(t2.1), vreinterpretq_u16_u8(t3.1));
        let v0 = vtrnq_u32(vreinterpretq_u32_u16(u0.0), vreinterpretq_u32_u16(u2.0));
        let v1 = vtrnq_u32(vreinterpretq_u32_u16(u1.0), vreinterpretq_u32_u16(u3.0));
        let v2 = vtrnq_u32(vreinterpretq_u32_u16(u0.1), vreinterpretq_u32_u16(u2.1));
        let v3 = vtrnq_u32(vreinterpretq_u32_u16(u1.1), vreinterpretq_u32_u16(u3.1));
        [
            vreinterpretq_u8_u32(v0.0),
            vreinterpretq_u8_u32(v1.0),
            vreinterpretq_u8_u32(v2.0),
            vreinterpretq_u8_u32(v3.0),
            vreinterpretq_u8_u32(v0.1),
            vreinterpretq_u8_u32(v1.1),
            vreinterpretq_u8_u32(v2.1),
            vreinterpretq_u8_u32(v3.1),
        ]
    }

    /// Loads p3..q3 for 16 positions: rows `first` and `second` (8 each)
    /// for a vertical edge, else whole rows of 16.
    #[inline(always)]
    unsafe fn load(
        first: *const u8,
        second: *const u8,
        stride: usize,
        vertical: bool,
    ) -> [uint8x16_t; 8] {
        if vertical {
            transpose(std::array::from_fn(|i| {
                vcombine_u8(
                    vld1_u8(first.add(i * stride).sub(4)),
                    vld1_u8(second.add(i * stride).sub(4)),
                )
            }))
        } else {
            std::array::from_fn(|i| {
                let row = (i as isize - 4) * stride as isize;
                vcombine_u8(vld1_u8(first.offset(row)), vld1_u8(second.offset(row)))
            })
        }
    }

    #[inline(always)]
    unsafe fn store(
        first: *mut u8,
        second: *mut u8,
        stride: usize,
        vertical: bool,
        s: [uint8x16_t; 8],
    ) {
        if vertical {
            let rows = transpose(s);
            for (i, row) in rows.into_iter().enumerate() {
                vst1_u8(first.add(i * stride).sub(4), vget_low_u8(row));
                vst1_u8(second.add(i * stride).sub(4), vget_high_u8(row));
            }
        } else {
            for (i, row) in s.into_iter().enumerate().take(7).skip(1) {
                let at = (i as isize - 4) * stride as isize;
                vst1_u8(first.offset(at), vget_low_u8(row));
                vst1_u8(second.offset(at), vget_high_u8(row));
            }
        }
    }

    #[inline(always)]
    unsafe fn select(mask: uint8x16_t, a: uint8x16_t, b: uint8x16_t) -> uint8x16_t {
        vbslq_u8(mask, a, b)
    }

    /// `((q0 - p0) * 4 + (p1 - q1) + 4) >> 3` clipped to `±tc`, on 8 lanes.
    #[inline(always)]
    unsafe fn delta(
        p1: uint8x8_t,
        p0: uint8x8_t,
        q0: uint8x8_t,
        q1: uint8x8_t,
        tc: uint8x8_t,
    ) -> int16x8_t {
        let d = vaddq_s16(
            vshlq_n_s16::<2>(vreinterpretq_s16_u16(vsubl_u8(q0, p0))),
            vreinterpretq_s16_u16(vsubl_u8(p1, q1)),
        );
        let d = vshrq_n_s16::<3>(vaddq_s16(d, vdupq_n_s16(4)));
        let tc = vreinterpretq_s16_u16(vmovl_u8(tc));
        vminq_s16(vmaxq_s16(d, vnegq_s16(tc)), tc)
    }

    /// `x + d` and `x - d`, saturated to bytes, for 16 lanes split in two.
    #[inline(always)]
    unsafe fn apply(x: uint8x16_t, lo: int16x8_t, hi: int16x8_t, sign: bool) -> uint8x16_t {
        let w = |x: uint8x8_t, d: int16x8_t| {
            let x = vreinterpretq_s16_u16(vmovl_u8(x));
            vqmovun_s16(if sign {
                vaddq_s16(x, d)
            } else {
                vsubq_s16(x, d)
            })
        };
        vcombine_u8(w(vget_low_u8(x), lo), w(vget_high_u8(x), hi))
    }

    /// Normal-filter p1 (or q1) correction on 16 lanes:
    /// `p1 + clip(±tc0, (p2 + ((p0 + q0 + 1) >> 1) - 2 * p1) >> 1)`.
    #[inline(always)]
    unsafe fn side(p2: uint8x16_t, p1: uint8x16_t, avg: uint8x16_t, tc0: uint8x16_t) -> uint8x16_t {
        let half = |p2: uint8x8_t, p1: uint8x8_t, avg: uint8x8_t, tc0: uint8x8_t| {
            let x = vsubq_s16(
                vreinterpretq_s16_u16(vaddl_u8(p2, avg)),
                vreinterpretq_s16_u16(vshll_n_u8::<1>(p1)),
            );
            let tc = vreinterpretq_s16_u16(vmovl_u8(tc0));
            let x = vminq_s16(vmaxq_s16(vshrq_n_s16::<1>(x), vnegq_s16(tc)), tc);
            vqmovun_s16(vaddq_s16(vreinterpretq_s16_u16(vmovl_u8(p1)), x))
        };
        vcombine_u8(
            half(
                vget_low_u8(p2),
                vget_low_u8(p1),
                vget_low_u8(avg),
                vget_low_u8(tc0),
            ),
            half(
                vget_high_u8(p2),
                vget_high_u8(p1),
                vget_high_u8(avg),
                vget_high_u8(tc0),
            ),
        )
    }

    /// `(a + b + c + d + e + round) >> shift` style sums for the strong
    /// filter, on 16 lanes, from widened pairs.
    #[inline(always)]
    unsafe fn narrow<const SHIFT: i32>(lo: uint16x8_t, hi: uint16x8_t) -> uint8x16_t {
        vcombine_u8(vrshrn_n_u16::<SHIFT>(lo), vrshrn_n_u16::<SHIFT>(hi))
    }

    /// The bS 4 luma filter for one side, from (p3, p2, p1, p0, q0, q1)
    /// on 8 lanes, before rounding: p0', p1', p2' sums.
    #[inline(always)]
    unsafe fn strong_sums(s: [uint8x8_t; 6]) -> [uint16x8_t; 3] {
        let [p3, p2, p1, p0, q0, q1] = s;
        // p0' = (p2 + 2p1 + 2p0 + 2q0 + q1 + 4) >> 3
        let n0 = vaddq_u16(
            vaddl_u8(p2, q1),
            vshlq_n_u16::<1>(vaddq_u16(vaddl_u8(p1, p0), vmovl_u8(q0))),
        );
        // p1' = (p2 + p1 + p0 + q0 + 2) >> 2
        let n1 = vaddq_u16(vaddl_u8(p2, p1), vaddl_u8(p0, q0));
        // p2' = (2p3 + 3p2 + p1 + p0 + q0 + 4) >> 3
        let n2 = vaddq_u16(
            vaddq_u16(vshll_n_u8::<1>(p3), vmull_u8(p2, vdup_n_u8(3))),
            vaddq_u16(vaddl_u8(p1, p0), vmovl_u8(q0)),
        );
        [n0, n1, n2]
    }

    /// The bS 4 luma filter for one side on 16 lanes.
    #[inline(always)]
    unsafe fn strong_side(s: [uint8x16_t; 6]) -> [uint8x16_t; 3] {
        let lo = strong_sums(s.map(|v| vget_low_u8(v)));
        let hi = strong_sums(s.map(|v| vget_high_u8(v)));
        [
            narrow::<3>(lo[0], hi[0]),
            narrow::<2>(lo[1], hi[1]),
            narrow::<3>(lo[2], hi[2]),
        ]
    }

    /// `(2 * p1 + p0 + q1 + 2) >> 2` on 16 lanes.
    #[inline(always)]
    unsafe fn weak_p0(p1: uint8x16_t, p0: uint8x16_t, q1: uint8x16_t) -> uint8x16_t {
        let lo = vaddq_u16(
            vshll_n_u8::<1>(vget_low_u8(p1)),
            vaddl_u8(vget_low_u8(p0), vget_low_u8(q1)),
        );
        let hi = vaddq_u16(
            vshll_n_u8::<1>(vget_high_u8(p1)),
            vaddl_u8(vget_high_u8(p0), vget_high_u8(q1)),
        );
        narrow::<2>(lo, hi)
    }

    /// The sample masks every filter starts with.
    #[inline(always)]
    unsafe fn gate(s: &[uint8x16_t; 8], e: &Edge, bs: uint8x16_t) -> uint8x16_t {
        let alpha = vdupq_n_u8(e.alpha);
        let beta = vdupq_n_u8(e.beta);
        let m = vandq_u8(
            vcltq_u8(vabdq_u8(s[3], s[4]), alpha),
            vandq_u8(
                vcltq_u8(vabdq_u8(s[2], s[3]), beta),
                vcltq_u8(vabdq_u8(s[5], s[4]), beta),
            ),
        );
        vandq_u8(m, vtstq_u8(bs, bs))
    }

    pub(super) unsafe fn luma(
        plane: &mut [u8],
        stride: usize,
        q0: usize,
        vertical: bool,
        e: &Edge,
        strong: bool,
    ) {
        let base = plane.as_mut_ptr().add(q0);
        let (first, second) = if vertical {
            (base, base.add(8 * stride))
        } else {
            (base, base.add(8))
        };
        let mut s = load(first, second, stride, vertical);
        let (bs, tc0) = lanes(e, false);
        let bs = vld1q_u8(bs.as_ptr());
        let m = gate(&s, e, bs);
        let beta = vdupq_n_u8(e.beta);
        let ap = vandq_u8(m, vcltq_u8(vabdq_u8(s[1], s[3]), beta));
        let aq = vandq_u8(m, vcltq_u8(vabdq_u8(s[6], s[4]), beta));
        let [p3, p2, p1, p0, q0v, q1, q2, q3] = s;
        if strong {
            let small = vcltq_u8(vabdq_u8(p0, q0v), vdupq_n_u8((e.alpha >> 2) + 2));
            let sp = vandq_u8(ap, small);
            let sq = vandq_u8(aq, small);
            let [np0, np1, np2] = strong_side([p3, p2, p1, p0, q0v, q1]);
            let [nq0, nq1, nq2] = strong_side([q3, q2, q1, q0v, p0, p1]);
            s[3] = select(sp, np0, select(m, weak_p0(p1, p0, q1), p0));
            s[2] = select(sp, np1, p1);
            s[1] = select(sp, np2, p2);
            s[4] = select(sq, nq0, select(m, weak_p0(q1, q0v, p1), q0v));
            s[5] = select(sq, nq1, q1);
            s[6] = select(sq, nq2, q2);
        } else {
            let tc0 = vld1q_u8(tc0.as_ptr());
            let one = vdupq_n_u8(1);
            let tc = vaddq_u8(tc0, vaddq_u8(vandq_u8(ap, one), vandq_u8(aq, one)));
            let lo = delta(
                vget_low_u8(p1),
                vget_low_u8(p0),
                vget_low_u8(q0v),
                vget_low_u8(q1),
                vget_low_u8(tc),
            );
            let hi = delta(
                vget_high_u8(p1),
                vget_high_u8(p0),
                vget_high_u8(q0v),
                vget_high_u8(q1),
                vget_high_u8(tc),
            );
            let avg = vrhaddq_u8(p0, q0v);
            s[2] = select(ap, side(p2, p1, avg, tc0), p1);
            s[5] = select(aq, side(q2, q1, avg, tc0), q1);
            s[3] = select(m, apply(p0, lo, hi, true), p0);
            s[4] = select(m, apply(q0v, lo, hi, false), q0v);
        }
        store(first, second, stride, vertical, s);
    }

    pub(super) unsafe fn chroma(
        u: &mut [u8],
        v: &mut [u8],
        stride: usize,
        q0: usize,
        vertical: bool,
        e: &Edge,
        strong: bool,
    ) {
        let (first, second) = (u.as_mut_ptr().add(q0), v.as_mut_ptr().add(q0));
        let mut s = load(first, second, stride, vertical);
        let (bs, tc0) = lanes(e, true);
        let m = gate(&s, e, vld1q_u8(bs.as_ptr()));
        let [_, _, p1, p0, q0v, q1, _, _] = s;
        if strong {
            s[3] = select(m, weak_p0(p1, p0, q1), p0);
            s[4] = select(m, weak_p0(q1, q0v, p1), q0v);
        } else {
            let tc = vaddq_u8(vld1q_u8(tc0.as_ptr()), vdupq_n_u8(1));
            let lo = delta(
                vget_low_u8(p1),
                vget_low_u8(p0),
                vget_low_u8(q0v),
                vget_low_u8(q1),
                vget_low_u8(tc),
            );
            let hi = delta(
                vget_high_u8(p1),
                vget_high_u8(p0),
                vget_high_u8(q0v),
                vget_high_u8(q1),
                vget_high_u8(tc),
            );
            s[3] = select(m, apply(p0, lo, hi, true), p0);
            s[4] = select(m, apply(q0v, lo, hi, false), q0v);
        }
        store(first, second, stride, vertical, s);
    }
}

#[cfg(target_arch = "x86_64")]
mod sse2 {
    use super::{lanes, Edge};
    use std::arch::x86_64::*;

    /// Transposes two 8x8 byte blocks: rows 0..8 hold block A in their low
    /// halves and block B in their high halves; so do the results.
    #[inline(always)]
    unsafe fn transpose(r: [__m128i; 8]) -> [__m128i; 8] {
        // Interleave A and B rows so each step works on both blocks.
        let a: [__m128i; 8] = std::array::from_fn(|i| r[i]);
        // Bytes: pair rows (0,1), (2,3), ...
        let b0 = _mm_unpacklo_epi8(a[0], a[1]);
        let b1 = _mm_unpackhi_epi8(a[0], a[1]);
        let b2 = _mm_unpacklo_epi8(a[2], a[3]);
        let b3 = _mm_unpackhi_epi8(a[2], a[3]);
        let b4 = _mm_unpacklo_epi8(a[4], a[5]);
        let b5 = _mm_unpackhi_epi8(a[4], a[5]);
        let b6 = _mm_unpacklo_epi8(a[6], a[7]);
        let b7 = _mm_unpackhi_epi8(a[6], a[7]);
        // 16-bit pairs: rows 0-3 and 4-7 (A from b0/b2/b4/b6, B from the odd ones).
        let c0 = _mm_unpacklo_epi16(b0, b2);
        let c1 = _mm_unpackhi_epi16(b0, b2);
        let c2 = _mm_unpacklo_epi16(b4, b6);
        let c3 = _mm_unpackhi_epi16(b4, b6);
        let c4 = _mm_unpacklo_epi16(b1, b3);
        let c5 = _mm_unpackhi_epi16(b1, b3);
        let c6 = _mm_unpacklo_epi16(b5, b7);
        let c7 = _mm_unpackhi_epi16(b5, b7);
        // 32-bit: columns of A in d0..d3 (two per register), of B in d4..d7.
        let d0 = _mm_unpacklo_epi32(c0, c2);
        let d1 = _mm_unpackhi_epi32(c0, c2);
        let d2 = _mm_unpacklo_epi32(c1, c3);
        let d3 = _mm_unpackhi_epi32(c1, c3);
        let d4 = _mm_unpacklo_epi32(c4, c6);
        let d5 = _mm_unpackhi_epi32(c4, c6);
        let d6 = _mm_unpacklo_epi32(c5, c7);
        let d7 = _mm_unpackhi_epi32(c5, c7);
        // Column k of A sits in the low or high 8 bytes of d[k / 2]; pair it with B's.
        [
            _mm_unpacklo_epi64(d0, d4),
            _mm_unpackhi_epi64(d0, d4),
            _mm_unpacklo_epi64(d1, d5),
            _mm_unpackhi_epi64(d1, d5),
            _mm_unpacklo_epi64(d2, d6),
            _mm_unpackhi_epi64(d2, d6),
            _mm_unpacklo_epi64(d3, d7),
            _mm_unpackhi_epi64(d3, d7),
        ]
    }

    #[inline(always)]
    unsafe fn pair(lo: *const u8, hi: *const u8) -> __m128i {
        _mm_unpacklo_epi64(_mm_loadl_epi64(lo.cast()), _mm_loadl_epi64(hi.cast()))
    }

    #[inline(always)]
    unsafe fn load(
        first: *const u8,
        second: *const u8,
        stride: usize,
        vertical: bool,
    ) -> [__m128i; 8] {
        if vertical {
            transpose(std::array::from_fn(|i| {
                pair(first.add(i * stride).sub(4), second.add(i * stride).sub(4))
            }))
        } else {
            std::array::from_fn(|i| {
                let row = (i as isize - 4) * stride as isize;
                pair(first.offset(row), second.offset(row))
            })
        }
    }

    #[inline(always)]
    unsafe fn store(
        first: *mut u8,
        second: *mut u8,
        stride: usize,
        vertical: bool,
        s: [__m128i; 8],
    ) {
        let put = |lo: *mut u8, hi: *mut u8, v: __m128i| {
            _mm_storel_epi64(lo.cast(), v);
            _mm_storel_epi64(hi.cast(), _mm_unpackhi_epi64(v, v));
        };
        if vertical {
            for (i, row) in transpose(s).into_iter().enumerate() {
                put(
                    first.add(i * stride).sub(4),
                    second.add(i * stride).sub(4),
                    row,
                );
            }
        } else {
            for (i, row) in s.into_iter().enumerate().take(7).skip(1) {
                let at = (i as isize - 4) * stride as isize;
                put(first.offset(at), second.offset(at), row);
            }
        }
    }

    #[inline(always)]
    unsafe fn select(mask: __m128i, a: __m128i, b: __m128i) -> __m128i {
        _mm_or_si128(_mm_and_si128(mask, a), _mm_andnot_si128(mask, b))
    }

    #[inline(always)]
    unsafe fn abd(a: __m128i, b: __m128i) -> __m128i {
        _mm_or_si128(_mm_subs_epu8(a, b), _mm_subs_epu8(b, a))
    }

    /// Lanes where `a < t` for unsigned bytes and `t >= 1`.
    #[inline(always)]
    unsafe fn below(a: __m128i, t: u8) -> __m128i {
        _mm_cmpeq_epi8(
            _mm_subs_epu8(a, _mm_set1_epi8((t - 1) as i8)),
            _mm_setzero_si128(),
        )
    }

    #[inline(always)]
    unsafe fn widen(a: __m128i) -> (__m128i, __m128i) {
        let z = _mm_setzero_si128();
        (_mm_unpacklo_epi8(a, z), _mm_unpackhi_epi8(a, z))
    }

    #[inline(always)]
    unsafe fn clip(x: __m128i, tc: __m128i) -> __m128i {
        _mm_min_epi16(_mm_max_epi16(x, _mm_sub_epi16(_mm_setzero_si128(), tc)), tc)
    }

    /// `((q0 - p0) * 4 + (p1 - q1) + 4) >> 3` clipped to `±tc` (two halves).
    #[inline(always)]
    unsafe fn delta(
        p1: __m128i,
        p0: __m128i,
        q0: __m128i,
        q1: __m128i,
        tc: __m128i,
    ) -> (__m128i, __m128i) {
        let (p1l, p1h) = widen(p1);
        let (p0l, p0h) = widen(p0);
        let (q0l, q0h) = widen(q0);
        let (q1l, q1h) = widen(q1);
        let (tcl, tch) = widen(tc);
        let four = _mm_set1_epi16(4);
        let d = |p1, p0, q0, q1, tc| {
            let d = _mm_add_epi16(
                _mm_slli_epi16(_mm_sub_epi16(q0, p0), 2),
                _mm_sub_epi16(p1, q1),
            );
            clip(_mm_srai_epi16(_mm_add_epi16(d, four), 3), tc)
        };
        (d(p1l, p0l, q0l, q1l, tcl), d(p1h, p0h, q0h, q1h, tch))
    }

    #[inline(always)]
    unsafe fn apply(x: __m128i, d: (__m128i, __m128i), add: bool) -> __m128i {
        let (xl, xh) = widen(x);
        let f = |x, d| {
            if add {
                _mm_add_epi16(x, d)
            } else {
                _mm_sub_epi16(x, d)
            }
        };
        _mm_packus_epi16(f(xl, d.0), f(xh, d.1))
    }

    #[inline(always)]
    unsafe fn side(p2: __m128i, p1: __m128i, avg: __m128i, tc0: __m128i) -> __m128i {
        let (p2l, p2h) = widen(p2);
        let (p1l, p1h) = widen(p1);
        let (al, ah) = widen(avg);
        let (tl, th) = widen(tc0);
        let f = |p2, p1, a, t| {
            let x = _mm_sub_epi16(_mm_add_epi16(p2, a), _mm_slli_epi16(p1, 1));
            _mm_add_epi16(p1, clip(_mm_srai_epi16(x, 1), t))
        };
        _mm_packus_epi16(f(p2l, p1l, al, tl), f(p2h, p1h, ah, th))
    }

    /// The bS 4 luma filter for one side, from (p3, p2, p1, p0, q0, q1)
    /// widened to 16 bits: p0', p1', p2'.
    #[inline(always)]
    unsafe fn strong_half(s: [__m128i; 6]) -> [__m128i; 3] {
        let [p3, p2, p1, p0, q0, q1] = s;
        let round = |sum: __m128i, half: i16, shift: i32| {
            let sum = _mm_add_epi16(sum, _mm_set1_epi16(half));
            if shift == 3 {
                _mm_srli_epi16(sum, 3)
            } else {
                _mm_srli_epi16(sum, 2)
            }
        };
        let n0 = _mm_add_epi16(
            _mm_add_epi16(p2, q1),
            _mm_slli_epi16(_mm_add_epi16(_mm_add_epi16(p1, p0), q0), 1),
        );
        let n1 = _mm_add_epi16(_mm_add_epi16(p2, p1), _mm_add_epi16(p0, q0));
        let n2 = _mm_add_epi16(
            _mm_add_epi16(
                _mm_slli_epi16(p3, 1),
                _mm_add_epi16(_mm_slli_epi16(p2, 1), p2),
            ),
            _mm_add_epi16(_mm_add_epi16(p1, p0), q0),
        );
        [round(n0, 4, 3), round(n1, 2, 2), round(n2, 4, 3)]
    }

    #[inline(always)]
    unsafe fn strong_side(s: [__m128i; 6]) -> [__m128i; 3] {
        let wide = s.map(widen);
        let lo = strong_half(wide.map(|w| w.0));
        let hi = strong_half(wide.map(|w| w.1));
        [
            _mm_packus_epi16(lo[0], hi[0]),
            _mm_packus_epi16(lo[1], hi[1]),
            _mm_packus_epi16(lo[2], hi[2]),
        ]
    }

    #[inline(always)]
    unsafe fn weak_p0(p1: __m128i, p0: __m128i, q1: __m128i) -> __m128i {
        let (p1l, p1h) = widen(p1);
        let (p0l, p0h) = widen(p0);
        let (q1l, q1h) = widen(q1);
        let f = |p1, p0, q1| {
            let sum = _mm_add_epi16(_mm_slli_epi16(p1, 1), _mm_add_epi16(p0, q1));
            _mm_srli_epi16(_mm_add_epi16(sum, _mm_set1_epi16(2)), 2)
        };
        _mm_packus_epi16(f(p1l, p0l, q1l), f(p1h, p0h, q1h))
    }

    #[inline(always)]
    unsafe fn gate(s: &[__m128i; 8], e: &Edge, bs: __m128i) -> __m128i {
        let m = _mm_and_si128(
            below(abd(s[3], s[4]), e.alpha),
            _mm_and_si128(
                below(abd(s[2], s[3]), e.beta),
                below(abd(s[5], s[4]), e.beta),
            ),
        );
        _mm_andnot_si128(_mm_cmpeq_epi8(bs, _mm_setzero_si128()), m)
    }

    pub(super) unsafe fn luma(
        plane: &mut [u8],
        stride: usize,
        q0: usize,
        vertical: bool,
        e: &Edge,
        strong: bool,
    ) {
        let base = plane.as_mut_ptr().add(q0);
        let (first, second) = if vertical {
            (base, base.add(8 * stride))
        } else {
            (base, base.add(8))
        };
        let mut s = load(first, second, stride, vertical);
        let (bs, tc0) = lanes(e, false);
        let m = gate(&s, e, _mm_loadu_si128(bs.as_ptr().cast()));
        let ap = _mm_and_si128(m, below(abd(s[1], s[3]), e.beta));
        let aq = _mm_and_si128(m, below(abd(s[6], s[4]), e.beta));
        let [p3, p2, p1, p0, q0v, q1, q2, q3] = s;
        if strong {
            let small = below(abd(p0, q0v), (e.alpha >> 2) + 2);
            let sp = _mm_and_si128(ap, small);
            let sq = _mm_and_si128(aq, small);
            let [np0, np1, np2] = strong_side([p3, p2, p1, p0, q0v, q1]);
            let [nq0, nq1, nq2] = strong_side([q3, q2, q1, q0v, p0, p1]);
            s[3] = select(sp, np0, select(m, weak_p0(p1, p0, q1), p0));
            s[2] = select(sp, np1, p1);
            s[1] = select(sp, np2, p2);
            s[4] = select(sq, nq0, select(m, weak_p0(q1, q0v, p1), q0v));
            s[5] = select(sq, nq1, q1);
            s[6] = select(sq, nq2, q2);
        } else {
            let tc0 = _mm_loadu_si128(tc0.as_ptr().cast());
            let one = _mm_set1_epi8(1);
            let tc = _mm_add_epi8(
                tc0,
                _mm_add_epi8(_mm_and_si128(ap, one), _mm_and_si128(aq, one)),
            );
            let d = delta(p1, p0, q0v, q1, tc);
            let avg = _mm_avg_epu8(p0, q0v);
            s[2] = select(ap, side(p2, p1, avg, tc0), p1);
            s[5] = select(aq, side(q2, q1, avg, tc0), q1);
            s[3] = select(m, apply(p0, d, true), p0);
            s[4] = select(m, apply(q0v, d, false), q0v);
        }
        store(first, second, stride, vertical, s);
    }

    pub(super) unsafe fn chroma(
        u: &mut [u8],
        v: &mut [u8],
        stride: usize,
        q0: usize,
        vertical: bool,
        e: &Edge,
        strong: bool,
    ) {
        let (first, second) = (u.as_mut_ptr().add(q0), v.as_mut_ptr().add(q0));
        let mut s = load(first, second, stride, vertical);
        let (bs, tc0) = lanes(e, true);
        let m = gate(&s, e, _mm_loadu_si128(bs.as_ptr().cast()));
        let [_, _, p1, p0, q0v, q1, _, _] = s;
        if strong {
            s[3] = select(m, weak_p0(p1, p0, q1), p0);
            s[4] = select(m, weak_p0(q1, q0v, p1), q0v);
        } else {
            let tc = _mm_add_epi8(_mm_loadu_si128(tc0.as_ptr().cast()), _mm_set1_epi8(1));
            let d = delta(p1, p0, q0v, q1, tc);
            s[3] = select(m, apply(p0, d, true), p0);
            s[4] = select(m, apply(q0v, d, false), q0v);
        }
        store(first, second, stride, vertical, s);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Table 8-16 and 8-17: (alpha, beta, tC0 for bS 1..3) per indexA.
    fn thresholds(index: usize) -> (u8, u8, [u8; 3]) {
        (ALPHA[index], BETA[index], TC0[index])
    }

    fn random(seed: &mut u32) -> u32 {
        *seed ^= *seed << 13;
        *seed ^= *seed >> 17;
        *seed ^= *seed << 5;
        *seed
    }

    /// A picture whose samples sit near a level, so that every threshold
    /// passes for some edges and fails for others.
    fn picture(seed: &mut u32, len: usize, spread: u32) -> Vec<u8> {
        let level = (random(seed) % 256) as i32;
        (0..len)
            .map(|_| {
                let r = random(seed);
                if r.is_multiple_of(17) {
                    r as u8 >> (r % 3)
                } else {
                    (level + (r >> 8) as i32 % (spread as i32 * 2 + 1) - spread as i32)
                        .clamp(0, 255) as u8
                }
            })
            .collect()
    }

    fn edges(seed: &mut u32) -> impl Iterator<Item = Edge> + '_ {
        (16..52)
            .flat_map(move |index| {
                let (alpha, beta, tc0) = thresholds(index);
                let mut out = vec![Edge {
                    alpha,
                    beta,
                    bs: [4; 4],
                    tc0: [0; 4],
                }];
                for _ in 0..6 {
                    let bs: [u8; 4] = std::array::from_fn(|_| (random(seed) % 4) as u8);
                    out.push(Edge {
                        alpha,
                        beta,
                        bs,
                        tc0: bs.map(|b| if b == 0 { 0 } else { tc0[b as usize - 1] }),
                    });
                }
                out.push(Edge {
                    alpha,
                    beta,
                    bs: [3, 4, 3, 4],
                    tc0: [tc0[2], 0, tc0[2], 0],
                });
                out
            })
            .collect::<Vec<_>>()
            .into_iter()
    }

    #[test]
    fn luma_edges_match_the_scalar_filter() {
        let mut seed = 0x9e3779b9;
        let stride = 48;
        let mut changed = 0;
        for spread in [1, 3, 8, 20, 128] {
            for edge in edges(&mut seed.clone()).collect::<Vec<_>>() {
                for vertical in [false, true] {
                    let original = picture(&mut seed, stride * 40, spread);
                    for q0 in [4 * stride + 4, 8 * stride + 16, 9 * stride + 7] {
                        let mut simd = original.clone();
                        let mut scalar = original.clone();
                        luma(&mut simd, stride, q0, vertical, &edge);
                        luma_scalar(&mut scalar, stride, q0, vertical, &edge);
                        assert_eq!(simd, scalar, "{edge:?} vertical={vertical} spread={spread}");
                        changed += usize::from(simd != original);
                    }
                }
            }
        }
        // Most edges filtered something: the comparison is not vacuous.
        assert!(changed > 2000, "{changed}");
    }

    #[test]
    fn chroma_edges_match_the_scalar_filter() {
        let mut seed = 0x2545f491;
        let stride = 24;
        let mut changed = 0;
        for spread in [1, 3, 8, 20, 128] {
            for edge in edges(&mut seed.clone()).collect::<Vec<_>>() {
                for vertical in [false, true] {
                    let (u0, v0) = (
                        picture(&mut seed, stride * 20, spread),
                        picture(&mut seed, stride * 20, spread),
                    );
                    for q0 in [4 * stride + 4, 8 * stride + 8, 5 * stride + 11] {
                        let (mut u, mut v) = (u0.clone(), v0.clone());
                        let (mut su, mut sv) = (u0.clone(), v0.clone());
                        chroma(&mut u, &mut v, stride, q0, vertical, &edge, &edge);
                        chroma_scalar(&mut su, stride, q0, vertical, &edge);
                        chroma_scalar(&mut sv, stride, q0, vertical, &edge);
                        changed += usize::from(u != u0 || v != v0);
                        assert_eq!(
                            (u, v),
                            (su, sv),
                            "{edge:?} vertical={vertical} spread={spread}"
                        );
                    }
                }
            }
        }
        assert!(changed > 1000, "{changed}");
    }
}
