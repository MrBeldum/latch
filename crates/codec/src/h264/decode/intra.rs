//! Intra prediction (spec 8.3): Intra-4x4's nine modes,
//! Intra-16x16's four, and the 8x8 chroma modes. All functions are
//! pure: the caller gathers the neighbouring samples (with their
//! availability flags) and every function here is a closed-form
//! expression over them.

use super::{Error, Result};

/// The neighbour samples one 4x4 (or 16x16) block's predictor is
/// built from. `top_right` is only fetched for modes that need it.
#[derive(Clone, Debug)]
pub(crate) struct NbSamples {
    /// `p[-1, 0..n)` left column (n = block size).
    pub left: [u8; 16],
    /// `p[0..n, -1]` top row.
    pub top: [u8; 16],
    /// `p[n..2n, -1]` top-right row continuation, when available.
    pub top_right: Option<[u8; 16]>,
    /// `p[-1,-1]` top-left corner, when available.
    pub top_left: Option<u8>,
    /// Left column available.
    pub has_left: bool,
    /// Top row available.
    pub has_top: bool,
}

fn clip(x: i32) -> u8 {
    x.clamp(0, 255) as u8
}

/// The nine Intra-4x4 modes (spec 8.3.1.2) over 16 output samples in
/// raster order. `mode` must be < 9; `Err` otherwise.
pub(crate) fn pred4x4(mode: u8, nb: &NbSamples, out: &mut [u8; 16]) -> Result<()> {
    let t = &nb.top[..4];
    let l = &nb.left[..4];
    let tl = nb.top_left;
    // Spec-allowed modes carry their required sample arrays; when a
    // stream (non-conformant or fuzzed) selects a mode whose inputs
    // are unavailable, real decoders substitute DC prediction rather
    // than fault — replicate that so decoding never desyncs.
    let dc4 = |nb: &NbSamples| -> u8 {
        match (nb.has_left, nb.has_top) {
            (true, true) => {
                ((t.iter().map(|&s| u32::from(s)).sum::<u32>()
                    + l.iter().map(|&s| u32::from(s)).sum::<u32>()
                    + 4)
                    >> 3) as u8
            }
            (true, false) => ((l.iter().map(|&s| u32::from(s)).sum::<u32>() + 2) >> 2) as u8,
            (false, true) => ((t.iter().map(|&s| u32::from(s)).sum::<u32>() + 2) >> 2) as u8,
            (false, false) => 128,
        }
    };
    match mode {
        // Intra_4x4_Vertical: needs the top row.
        0 => {
            if !nb.has_top {
                out.fill(dc4(nb));
                return Ok(());
            }
            for y in 0..4 {
                out[y * 4..y * 4 + 4].copy_from_slice(t);
            }
        }
        // Horizontal: needs the left column.
        1 => {
            if !nb.has_left {
                out.fill(dc4(nb));
                return Ok(());
            }
            for y in 0..4 {
                for x in 0..4 {
                    out[y * 4 + x] = l[y];
                }
            }
        }
        // DC.
        2 => {
            let v = match (nb.has_left, nb.has_top) {
                (true, true) => {
                    (t.iter().map(|&s| u32::from(s)).sum::<u32>()
                        + l.iter().map(|&s| u32::from(s)).sum::<u32>()
                        + 4)
                        >> 3
                }
                (true, false) => (l.iter().map(|&s| u32::from(s)).sum::<u32>() + 2) >> 2,
                (false, true) => (t.iter().map(|&s| u32::from(s)).sum::<u32>() + 2) >> 2,
                (false, false) => 128,
            } as u8;
            out.fill(v);
        }
        // Diagonal_Down_Left: top + top-right.
        3 => {
            if !nb.has_top {
                out.fill(dc4(nb));
                return Ok(());
            }
            let tr = match &nb.top_right {
                Some(tr) => *tr,
                // When p[4..8) is unavailable every one of them is
                // replaced by p[3,-1] (spec 8.3.1.2.1).
                None => {
                    let mut a = [0u8; 16];
                    a[..4].fill(t[3]);
                    a
                }
            };
            let p: [u8; 8] = [t[0], t[1], t[2], t[3], tr[0], tr[1], tr[2], tr[3]];
            for y in 0..4 {
                for x in 0..4 {
                    let v = if x == 3 && y == 3 {
                        (u32::from(p[6]) + 3 * u32::from(p[7]) + 2) >> 2
                    } else {
                        (u32::from(p[x + y])
                            + 2 * u32::from(p[x + y + 1])
                            + u32::from(p[x + y + 2])
                            + 2)
                            >> 2
                    };
                    out[y * 4 + x] = v as u8;
                }
            }
        }
        // Diagonal_Down_Right (spec 8.3.1.2.5).
        4 => {
            if !(nb.has_left && nb.has_top && tl.is_some()) {
                out.fill(dc4(nb));
                return Ok(());
            }
            let c = tl.unwrap();
            let (l0, l1, l2, l3) = (l[0], l[1], l[2], l[3]);
            let (t0, t1, t2, t3) = (t[0], t[1], t[2], t[3]);
            let mut set = |x: usize, y: usize, v: u32| out[y * 4 + x] = v as u8;
            let a = |p: u8, q: u8, r: u8| (u32::from(p) + 2 * u32::from(q) + u32::from(r) + 2) >> 2;
            set(0, 3, a(l3, l2, l1));
            set(0, 2, a(l2, l1, l0));
            set(1, 3, a(l2, l1, l0));
            set(0, 1, a(l1, l0, c));
            set(1, 2, a(l1, l0, c));
            set(2, 3, a(l1, l0, c));
            set(0, 0, a(l0, c, t0));
            set(1, 1, a(l0, c, t0));
            set(2, 2, a(l0, c, t0));
            set(3, 3, a(l0, c, t0));
            set(1, 0, a(c, t0, t1));
            set(2, 1, a(c, t0, t1));
            set(3, 2, a(c, t0, t1));
            set(2, 0, a(t0, t1, t2));
            set(3, 1, a(t0, t1, t2));
            set(3, 0, a(t1, t2, t3));
        }
        // Vertical_Right (spec 8.3.1.2.6); formulas transcribed
        // from the spec table.
        5 => {
            if !(nb.has_left && nb.has_top && tl.is_some()) {
                out.fill(dc4(nb));
                return Ok(());
            }
            let c = tl.unwrap();
            let (l0, l1, l2) = (l[0], l[1], l[2]);
            let (t0, t1, t2, t3) = (t[0], t[1], t[2], t[3]);
            let mut set = |x: usize, y: usize, v: u32| out[y * 4 + x] = v as u8;
            let a = |_x: usize, _y: usize, p: u8, q: u8, r: u8| {
                (u32::from(p) + 2 * u32::from(q) + u32::from(r) + 2) >> 2
            };
            set(0, 0, (u32::from(c) + u32::from(t0) + 1) >> 1);
            set(1, 2, (u32::from(c) + u32::from(t0) + 1) >> 1);
            set(1, 0, (u32::from(t0) + u32::from(t1) + 1) >> 1);
            set(2, 2, (u32::from(t0) + u32::from(t1) + 1) >> 1);
            set(2, 0, (u32::from(t1) + u32::from(t2) + 1) >> 1);
            set(3, 2, (u32::from(t1) + u32::from(t2) + 1) >> 1);
            set(3, 0, (u32::from(t2) + u32::from(t3) + 1) >> 1);
            set(0, 1, a(0, 1, l0, c, t0));
            set(1, 3, a(1, 3, l0, c, t0));
            set(1, 1, a(1, 1, c, t0, t1));
            set(2, 3, a(2, 3, c, t0, t1));
            set(2, 1, a(2, 1, t0, t1, t2));
            set(3, 3, a(3, 3, t0, t1, t2));
            set(3, 1, a(3, 1, t1, t2, t3));
            set(0, 2, a(0, 2, c, l0, l1));
            set(0, 3, a(0, 3, l0, l1, l2));
        }
        // Horizontal_Down (spec 8.3.1.2.8).
        6 => {
            if !(nb.has_left && nb.has_top && tl.is_some()) {
                out.fill(dc4(nb));
                return Ok(());
            }
            let c = tl.unwrap();
            let (l0, l1, l2, l3) = (l[0], l[1], l[2], l[3]);
            let (t0, t1, t2) = (t[0], t[1], t[2]);
            let mut set = |x: usize, y: usize, v: u32| out[y * 4 + x] = v as u8;
            let a = |p: u8, q: u8, r: u8| (u32::from(p) + 2 * u32::from(q) + u32::from(r) + 2) >> 2;
            set(0, 0, (u32::from(c) + u32::from(l0) + 1) >> 1);
            set(2, 1, (u32::from(c) + u32::from(l0) + 1) >> 1);
            set(1, 0, a(l0, c, t0));
            set(3, 1, a(l0, c, t0));
            set(2, 0, a(c, t0, t1));
            set(3, 0, a(t0, t1, t2));
            set(0, 1, (u32::from(l0) + u32::from(l1) + 1) >> 1);
            set(2, 2, (u32::from(l0) + u32::from(l1) + 1) >> 1);
            set(1, 1, a(c, l0, l1));
            set(3, 2, a(c, l0, l1));
            set(0, 2, (u32::from(l1) + u32::from(l2) + 1) >> 1);
            set(2, 3, (u32::from(l1) + u32::from(l2) + 1) >> 1);
            set(1, 2, a(l0, l1, l2));
            set(3, 3, a(l0, l1, l2));
            set(0, 3, (u32::from(l2) + u32::from(l3) + 1) >> 1);
            set(1, 3, a(l1, l2, l3));
        }
        // Vertical_Left (spec 8.3.1.2.7).
        7 => {
            if !nb.has_top {
                out.fill(dc4(nb));
                return Ok(());
            }
            let tr = match &nb.top_right {
                Some(tr) => *tr,
                None => {
                    let mut a = [0u8; 16];
                    a[..4].fill(t[3]);
                    a
                }
            };
            let p: [u8; 8] = [t[0], t[1], t[2], t[3], tr[0], tr[1], tr[2], tr[3]];
            let mut set = |x: usize, y: usize, v: u32| out[y * 4 + x] = v as u8;
            let a = |i: usize| {
                (u32::from(p[i]) + 2 * u32::from(p[i + 1]) + u32::from(p[i + 2]) + 2) >> 2
            };
            set(0, 0, (u32::from(p[0]) + u32::from(p[1]) + 1) >> 1);
            set(1, 0, (u32::from(p[1]) + u32::from(p[2]) + 1) >> 1);
            set(0, 2, (u32::from(p[1]) + u32::from(p[2]) + 1) >> 1);
            set(2, 0, (u32::from(p[2]) + u32::from(p[3]) + 1) >> 1);
            set(1, 2, (u32::from(p[2]) + u32::from(p[3]) + 1) >> 1);
            set(3, 0, (u32::from(p[3]) + u32::from(p[4]) + 1) >> 1);
            set(2, 2, (u32::from(p[3]) + u32::from(p[4]) + 1) >> 1);
            set(3, 2, (u32::from(p[4]) + u32::from(p[5]) + 1) >> 1);
            set(0, 1, a(0));
            set(1, 1, a(1));
            set(0, 3, a(1));
            set(2, 1, a(2));
            set(1, 3, a(2));
            set(3, 1, a(3));
            set(2, 3, a(3));
            set(3, 3, a(4));
        }
        // Horizontal_Up: left only.
        8 => {
            if !nb.has_left {
                out.fill(dc4(nb));
                return Ok(());
            }
            for y in 0..4usize {
                for x in 0..4usize {
                    let zhu = x + 2 * y;
                    let v = if zhu == 0 || zhu == 2 || zhu == 4 {
                        // (p[-1, y + (x>>1)] + p[-1, y+(x>>1)+1] + 1)>>1
                        let yr = y + (x >> 1);
                        (u32::from(l[yr]) + u32::from(l[yr + 1]) + 1) >> 1
                    } else if zhu == 1 || zhu == 3 {
                        let yr = y + (x >> 1);
                        (u32::from(l[yr]) + 2 * u32::from(l[yr + 1]) + u32::from(l[yr + 2]) + 2)
                            >> 2
                    } else if zhu == 5 {
                        (u32::from(l[2]) + 3 * u32::from(l[3]) + 2) >> 2
                    } else {
                        u32::from(l[3])
                    };
                    out[y * 4 + x] = v as u8;
                }
            }
        }
        _ => return Err(Error::BadValue("intra4x4 mode over 8")),
    }
    Ok(())
}

/// DC mean over the available sides of `nb.top[..n]`/`nb.left[..n]`,
/// used both for mode 0 and as the substitution when a required side
/// is missing (non-conformant streams still decode).
fn dc_mean(top: &[u8], left: &[u8], has_top: bool, has_left: bool) -> u8 {
    let n = top.len().max(left.len()) as u32;
    match (has_left, has_top) {
        (true, true) => {
            ((left.iter().map(|&s| u32::from(s)).sum::<u32>()
                + top.iter().map(|&s| u32::from(s)).sum::<u32>()
                + n)
                >> (2 * n.trailing_zeros())) as u8
        }
        (true, false) => {
            ((left.iter().map(|&s| u32::from(s)).sum::<u32>() + n / 2) >> n.trailing_zeros()) as u8
        }
        (false, true) => {
            ((top.iter().map(|&s| u32::from(s)).sum::<u32>() + n / 2) >> n.trailing_zeros()) as u8
        }
        (false, false) => 128,
    }
}

/// Intra-16x16 prediction (spec 8.3.3, Table 8-3): modes
/// 0 = Vertical, 1 = Horizontal, 2 = DC, 3 = Plane — **not** the
/// chroma order (which puts DC first). A mode whose required side is
/// unavailable (stream edge or unconstrained-intra inter neighbour)
/// falls back to the DC mean of whichever side exists — matching
/// reference-decoder behaviour — rather than faulting the stream.
pub(crate) fn pred16x16(mode: u8, nb: &NbSamples, out: &mut [u8; 256]) -> Result<()> {
    match mode {
        0 => {
            if !nb.has_top {
                out.fill(dc_mean(&nb.top[..16], &nb.left[..16], false, nb.has_left));
                return Ok(());
            }
            for y in 0..16 {
                out[y * 16..y * 16 + 16].copy_from_slice(&nb.top[..16]);
            }
        }
        1 => {
            if !nb.has_left {
                out.fill(dc_mean(&nb.top[..16], &nb.left[..16], nb.has_top, false));
                return Ok(());
            }
            for y in 0..16 {
                out[y * 16..y * 16 + 16].fill(nb.left[y]);
            }
        }
        2 => {
            let v = match (nb.has_left, nb.has_top) {
                (true, true) => {
                    (nb.top[..16].iter().map(|&s| u32::from(s)).sum::<u32>()
                        + nb.left[..16].iter().map(|&s| u32::from(s)).sum::<u32>()
                        + 16)
                        >> 5
                }
                (true, false) => {
                    (nb.left[..16].iter().map(|&s| u32::from(s)).sum::<u32>() + 8) >> 4
                }
                (false, true) => (nb.top[..16].iter().map(|&s| u32::from(s)).sum::<u32>() + 8) >> 4,
                (false, false) => 128,
            } as u8;
            out.fill(v);
        }
        3 => {
            if !(nb.has_left && nb.has_top && nb.top_left.is_some()) {
                out.fill(dc_mean(
                    &nb.top[..16],
                    &nb.left[..16],
                    nb.has_top,
                    nb.has_left,
                ));
                return Ok(());
            }
            let mut h = 0i32;
            let mut v = 0i32;
            for x in 1..=8usize {
                // p[7+x,-1] - p[7-x,-1]; p[-1,-1] for x=8 gives p[-1,-1].
                let hi = if x == 8 {
                    i32::from(nb.top[15])
                } else {
                    i32::from(nb.top[7 + x])
                };
                let lo = if x == 8 {
                    i32::from(nb.top_left.unwrap())
                } else {
                    i32::from(nb.top[7 - x])
                };
                h += x as i32 * (hi - lo);
                let hv = if x == 8 {
                    i32::from(nb.left[15])
                } else {
                    i32::from(nb.left[7 + x])
                };
                let lv = if x == 8 {
                    i32::from(nb.top_left.unwrap())
                } else {
                    i32::from(nb.left[7 - x])
                };
                v += x as i32 * (hv - lv);
            }
            let a = 16 * (i32::from(nb.left[15]) + i32::from(nb.top[15]));
            let b = (5 * h + 32) >> 6;
            let c = (5 * v + 32) >> 6;
            for y in 0..16 {
                for x in 0..16 {
                    out[y * 16 + x] = clip((a + b * (x as i32 - 7) + c * (y as i32 - 7) + 16) >> 5);
                }
            }
        }
        _ => return Err(Error::BadValue("intra16x16 mode over 3")),
    }
    Ok(())
}

/// 8x8 chroma prediction (spec 8.3.4): mode 0 DC (per-quadrant means
/// with per-side fallback), 1 horizontal, 2 vertical, 3 plane.
pub(crate) fn pred_chroma(mode: u8, nb: &NbSamples, out: &mut [u8; 64]) -> Result<()> {
    match mode {
        0 => {
            // Spec 8.3.4.2 — asymmetric quadrants:
            //   Q0 (x<4, y<4): mean of top[0..4] + left[0..4]
            //   Q1 (x>=4, y<4): top[4..8] alone (left side unused)
            //   Q2 (x<4, y>=4): left[4..8] alone (top side unused)
            //   Q3 (x>=4, y>=4): mean of top[4..8] + left[4..8]
            // Unavailable sides fall back per quadrant: Q1/Q3 missing
            // top use the left mean, Q2/Q3 missing left use the top mean,
            // and a quadrant with neither side uses 128.
            let m4 = |s: &[u8]| (s.iter().map(|&v| u32::from(v)).sum::<u32>() + 2) >> 2;
            let tl = &nb.top[..4];
            let tr = &nb.top[4..8];
            let lt = &nb.left[..4];
            let lb = &nb.left[4..8];
            for qy in 0..2 {
                for qx in 0..2 {
                    let v = match (qx, qy) {
                        (0, 0) => match (nb.has_left, nb.has_top) {
                            (true, true) => {
                                ((tl.iter().map(|&s| u32::from(s)).sum::<u32>()
                                    + lt.iter().map(|&s| u32::from(s)).sum::<u32>()
                                    + 4)
                                    >> 3) as u8
                            }
                            (true, false) => m4(lt) as u8,
                            (false, true) => m4(tl) as u8,
                            (false, false) => 128,
                        },
                        (1, 0) => {
                            if nb.has_top {
                                m4(tr) as u8
                            } else if nb.has_left {
                                m4(lt) as u8
                            } else {
                                128
                            }
                        }
                        (0, 1) => {
                            if nb.has_left {
                                m4(lb) as u8
                            } else if nb.has_top {
                                m4(tl) as u8
                            } else {
                                128
                            }
                        }
                        _ => match (nb.has_left, nb.has_top) {
                            (true, true) => {
                                ((tr.iter().map(|&s| u32::from(s)).sum::<u32>()
                                    + lb.iter().map(|&s| u32::from(s)).sum::<u32>()
                                    + 4)
                                    >> 3) as u8
                            }
                            (true, false) => m4(lb) as u8,
                            (false, true) => m4(tr) as u8,
                            (false, false) => 128,
                        },
                    };
                    for y in 0..4 {
                        for x in 0..4 {
                            out[(qy * 4 + y) * 8 + qx * 4 + x] = v;
                        }
                    }
                }
            }
        }
        1 => {
            if !nb.has_left {
                out.fill(dc_mean(&nb.top[..8], &nb.left[..8], nb.has_top, false));
                return Ok(());
            }
            for y in 0..8 {
                for x in 0..8 {
                    out[y * 8 + x] = nb.left[y];
                }
            }
        }
        2 => {
            if !nb.has_top {
                out.fill(dc_mean(&nb.top[..8], &nb.left[..8], false, nb.has_left));
                return Ok(());
            }
            for y in 0..8 {
                out[y * 8..y * 8 + 8].copy_from_slice(&nb.top[..8]);
            }
        }
        3 => {
            if !(nb.has_left && nb.has_top && nb.top_left.is_some()) {
                out.fill(dc_mean(
                    &nb.top[..8],
                    &nb.left[..8],
                    nb.has_top,
                    nb.has_left,
                ));
                return Ok(());
            }
            let mut h = 0i32;
            let mut v = 0i32;
            for x in 1..=4usize {
                let hi = if x == 4 {
                    i32::from(nb.top[7])
                } else {
                    i32::from(nb.top[3 + x])
                };
                let lo = if x == 4 {
                    i32::from(nb.top_left.unwrap())
                } else {
                    i32::from(nb.top[3 - x])
                };
                h += x as i32 * (hi - lo);
                let hv = if x == 4 {
                    i32::from(nb.left[7])
                } else {
                    i32::from(nb.left[3 + x])
                };
                let lv = if x == 4 {
                    i32::from(nb.top_left.unwrap())
                } else {
                    i32::from(nb.left[3 - x])
                };
                v += x as i32 * (hv - lv);
            }
            let a = 16 * (i32::from(nb.left[7]) + i32::from(nb.top[7]));
            let b = (17 * h + 16) >> 5;
            let c = (17 * v + 16) >> 5;
            for y in 0..8 {
                for x in 0..8 {
                    out[y * 8 + x] = clip((a + b * (x as i32 - 3) + c * (y as i32 - 3) + 16) >> 5);
                }
            }
        }
        _ => return Err(Error::BadValue("chroma mode over 3")),
    }
    Ok(())
}

#[cfg(test)]
mod dc_helper_tests {
    use super::*;

    #[test]
    fn dc_mean_arms() {
        let top = [4u8; 16];
        let left = [2u8; 16];
        // Both neighbours: (left+top sums + n) >> log2(2n) — for
        // n = 16 edges that is >> 8.
        let both = dc_mean(&top, &left, true, true);
        assert_eq!(both, ((32u32 + 64 + 16) >> 8) as u8);
        // Top only: plain mean.
        assert_eq!(dc_mean(&top, &left, true, false), 4);
        // Left only.
        assert_eq!(dc_mean(&top, &left, false, true), 2);
        // Neither: the 128 constant.
        assert_eq!(dc_mean(&top, &left, false, false), 128);
    }

    #[test]
    fn mode_rejects_bound_the_tables() {
        // Error arms past the last defined pred mode, both sizes.
        let nb = NbSamples {
            top: [0; 16],
            left: [0; 16],
            top_right: None,
            top_left: None,
            has_left: true,
            has_top: true,
        };
        let mut out4 = [0u8; 16];
        assert!(pred4x4(9, &nb, &mut out4).is_err());
        let mut out16 = [0u8; 256];
        assert!(pred16x16(4, &nb, &mut out16).is_err());
    }
}
