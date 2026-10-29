//! Intra prediction (spec 8.3): Intra-4x4's nine modes,
//! Intra-16x16's four, and the 8x8 chroma modes. All functions are
//! pure: the caller gathers the neighbouring samples (with their
//! availability flags) and every function here is a closed-form
//! expression over them.

use super::{Error, Result};

/// The neighbour samples one 4x4 (or 8x8/16x16) block's predictor is
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
        // Diagonal_Down_Right (spec 8.3.1.2.5), cell-for-cell like
        // FFmpeg's pred4x4_down_right.
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
        // cell-for-cell from the spec table, same order as FFmpeg's
        // pred4x4_vertical_right.
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
        // Horizontal_Down (spec 8.3.1.2.8), cell-for-cell like FFmpeg's
        // pred4x4_horizontal_down.
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
        // Vertical_Left (spec 8.3.1.2.7), cell-for-cell like FFmpeg's
        // pred4x4_vertical_left.
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

/// The nine Intra-8x8 modes (spec 8.3.5.2) over 64 output samples in
/// raster order. The reference decoder's `pred8x8l` family: neighbour
/// samples are *low-pass filtered* before use (`(a + 2b + c + 2) >> 2`
/// with edge replication at the ends and `has_topleft`/`has_topright`
/// substituting the nearest real sample). Unavailable inputs make the
/// mode fall back to DC or 128, matching the spec's
/// `check_intra4x4_pred_mode` legalisation.
#[allow(clippy::too_many_lines)]
pub(crate) fn pred8x8(mode: u8, nb: &NbSamples, out: &mut [u8; 64]) {
    // Raw neighbours.
    let l_raw: [i32; 8] = core::array::from_fn(|i| i32::from(nb.left[i]));
    let t_raw: [i32; 8] = core::array::from_fn(|i| i32::from(nb.top[i]));
    let tr_raw: [i32; 8] = nb
        .top_right
        .map(|a| core::array::from_fn(|i| i32::from(a[i])))
        .unwrap_or([i32::from(nb.top[7]); 8]);
    let tl = nb.top_left.map(i32::from);
    let has_tl = tl.is_some();
    let has_l = nb.has_left;
    let has_t = nb.has_top;
    let has_tr = nb.top_right.is_some();

    // Filtered left samples l0..l7 (spec 8.3.5.2.1 `p'[-1, y]`).
    let mut l = [0i32; 8];
    l[0] = (tl.unwrap_or(l_raw[0]) + 2 * l_raw[0] + l_raw[1] + 2) >> 2;
    for i in 1..7 {
        l[i] = (l_raw[i - 1] + 2 * l_raw[i] + l_raw[i + 1] + 2) >> 2;
    }
    l[7] = (l_raw[6] + 3 * l_raw[7] + 2) >> 2;

    // Filtered top samples t0..t15 (`p'[x, -1]`, x = 0..15).
    let mut t = [0i32; 16];
    t[0] = (tl.unwrap_or(t_raw[0]) + 2 * t_raw[0] + t_raw[1] + 2) >> 2;
    for i in 1..7 {
        t[i] = (t_raw[i - 1] + 2 * t_raw[i] + t_raw[i + 1] + 2) >> 2;
    }
    t[7] = (tr_raw[0] + 2 * t_raw[7] + t_raw[6] + 2) >> 2;
    if has_tr {
        for i in 8..15 {
            // PTR(8) reads SRC(7,-1) — the *unfiltered* last top
            // sample — not filtered t[7] (ffmpeg pred8x8l).
            let a = if i == 8 { t_raw[7] } else { tr_raw[i - 9] };
            t[i] = (a + 2 * tr_raw[i - 8] + tr_raw[i - 7] + 2) >> 2;
        }
        t[15] = (tr_raw[6] + 3 * tr_raw[7] + 2) >> 2;
    } else {
        for tv in t.iter_mut().skip(8) {
            *tv = t_raw[7];
        }
    }
    // Filtered top-left corner `p'[-1, -1]`.
    let lt = (l_raw[0] + 2 * tl.unwrap_or_else(|| t_raw[0].max(l_raw[0])) + t_raw[0] + 2) >> 2;

    let set = |out: &mut [u8; 64], x: usize, y: usize, v: i32| {
        out[y * 8 + x] = clip(v);
    };

    if !has_l && !has_t {
        out.fill(128);
        return;
    }
    match mode {
        // Intra_8x8_Vertical.
        0 => {
            if !has_t {
                let dc = ((l.iter().sum::<i32>() + 4) >> 3) as u8;
                out.fill(dc);
                return;
            }
            for y in 0..8 {
                for (x, &tv) in t.iter().enumerate().take(8) {
                    set(out, x, y, tv);
                }
            }
        }
        // Intra_8x8_Horizontal.
        1 => {
            if !has_l {
                let dc = ((t[..8].iter().sum::<i32>() + 4) >> 3) as u8;
                out.fill(dc);
                return;
            }
            for (y, &lv) in l.iter().enumerate().take(8) {
                for x in 0..8 {
                    set(out, x, y, lv);
                }
            }
        }
        // Intra_8x8_DC (spec 8-64: (sum t' + sum l' + 8) >> 4).
        2 => {
            let dc = if has_l && has_t {
                ((l.iter().sum::<i32>() + t[..8].iter().sum::<i32>() + 8) >> 4) as u8
            } else if has_l {
                ((l.iter().sum::<i32>() + 4) >> 3) as u8
            } else {
                ((t[..8].iter().sum::<i32>() + 4) >> 3) as u8
            };
            out.fill(dc);
        }
        // Intra_8x8_Diagonal_Down_Left (spec 8-65): top + top-right,
        // requires `has_t`. The t7..t15 chain extends when top-right
        // is real; with it replicated the formula degenerates to
        // ffmpeg's `SRC(7,-1)` replication.
        3 => {
            if !has_t {
                let dc = ((l.iter().sum::<i32>() + 4) >> 3) as u8;
                out.fill(dc);
                return;
            }
            for y in 0..8 {
                for x in 0..8 {
                    let v = if x + y == 14 {
                        (t[14] + 3 * t[15] + 2) >> 2
                    } else {
                        (t[x + y] + 2 * t[x + y + 1] + t[x + y + 2] + 2) >> 2
                    };
                    set(out, x, y, v);
                }
            }
        }
        // Intra_8x8_Diagonal_Down_Right (spec 8-66): left + top +
        // top-left; needs all three (ffmpeg substitutes 128-DC via
        // the mode-check otherwise — we fall back to DC).
        4 => {
            if !(has_l && has_t && has_tl) {
                dc_fallback(out, &l, &t, has_l, has_t);
                return;
            }
            // Literal port of ffmpeg `pred8x8l_down_right`.
            set(out, 0, 7, (l[7] + 2 * l[6] + l[5] + 2) >> 2);
            set(out, 0, 6, (l[6] + 2 * l[5] + l[4] + 2) >> 2);
            set(out, 1, 7, (l[6] + 2 * l[5] + l[4] + 2) >> 2);
            set(out, 0, 5, (l[5] + 2 * l[4] + l[3] + 2) >> 2);
            set(out, 1, 6, (l[5] + 2 * l[4] + l[3] + 2) >> 2);
            set(out, 2, 7, (l[5] + 2 * l[4] + l[3] + 2) >> 2);
            set(out, 0, 4, (l[4] + 2 * l[3] + l[2] + 2) >> 2);
            set(out, 1, 5, (l[4] + 2 * l[3] + l[2] + 2) >> 2);
            set(out, 2, 6, (l[4] + 2 * l[3] + l[2] + 2) >> 2);
            set(out, 3, 7, (l[4] + 2 * l[3] + l[2] + 2) >> 2);
            set(out, 0, 3, (l[3] + 2 * l[2] + l[1] + 2) >> 2);
            set(out, 1, 4, (l[3] + 2 * l[2] + l[1] + 2) >> 2);
            set(out, 2, 5, (l[3] + 2 * l[2] + l[1] + 2) >> 2);
            set(out, 3, 6, (l[3] + 2 * l[2] + l[1] + 2) >> 2);
            set(out, 4, 7, (l[3] + 2 * l[2] + l[1] + 2) >> 2);
            set(out, 0, 2, (l[2] + 2 * l[1] + l[0] + 2) >> 2);
            set(out, 1, 3, (l[2] + 2 * l[1] + l[0] + 2) >> 2);
            set(out, 2, 4, (l[2] + 2 * l[1] + l[0] + 2) >> 2);
            set(out, 3, 5, (l[2] + 2 * l[1] + l[0] + 2) >> 2);
            set(out, 4, 6, (l[2] + 2 * l[1] + l[0] + 2) >> 2);
            set(out, 5, 7, (l[2] + 2 * l[1] + l[0] + 2) >> 2);
            set(out, 0, 1, (l[1] + 2 * l[0] + lt + 2) >> 2);
            set(out, 1, 2, (l[1] + 2 * l[0] + lt + 2) >> 2);
            set(out, 2, 3, (l[1] + 2 * l[0] + lt + 2) >> 2);
            set(out, 3, 4, (l[1] + 2 * l[0] + lt + 2) >> 2);
            set(out, 4, 5, (l[1] + 2 * l[0] + lt + 2) >> 2);
            set(out, 5, 6, (l[1] + 2 * l[0] + lt + 2) >> 2);
            set(out, 6, 7, (l[1] + 2 * l[0] + lt + 2) >> 2);
            for i in 0..8 {
                set(out, i, i, (l[0] + 2 * lt + t[0] + 2) >> 2);
            }
            set(out, 1, 0, (lt + 2 * t[0] + t[1] + 2) >> 2);
            set(out, 2, 1, (lt + 2 * t[0] + t[1] + 2) >> 2);
            set(out, 3, 2, (lt + 2 * t[0] + t[1] + 2) >> 2);
            set(out, 4, 3, (lt + 2 * t[0] + t[1] + 2) >> 2);
            set(out, 5, 4, (lt + 2 * t[0] + t[1] + 2) >> 2);
            set(out, 6, 5, (lt + 2 * t[0] + t[1] + 2) >> 2);
            set(out, 7, 6, (lt + 2 * t[0] + t[1] + 2) >> 2);
            set(out, 2, 0, (t[0] + 2 * t[1] + t[2] + 2) >> 2);
            set(out, 3, 1, (t[0] + 2 * t[1] + t[2] + 2) >> 2);
            set(out, 4, 2, (t[0] + 2 * t[1] + t[2] + 2) >> 2);
            set(out, 5, 3, (t[0] + 2 * t[1] + t[2] + 2) >> 2);
            set(out, 6, 4, (t[0] + 2 * t[1] + t[2] + 2) >> 2);
            set(out, 7, 5, (t[0] + 2 * t[1] + t[2] + 2) >> 2);
            set(out, 3, 0, (t[1] + 2 * t[2] + t[3] + 2) >> 2);
            set(out, 4, 1, (t[1] + 2 * t[2] + t[3] + 2) >> 2);
            set(out, 5, 2, (t[1] + 2 * t[2] + t[3] + 2) >> 2);
            set(out, 6, 3, (t[1] + 2 * t[2] + t[3] + 2) >> 2);
            set(out, 7, 4, (t[1] + 2 * t[2] + t[3] + 2) >> 2);
            set(out, 4, 0, (t[2] + 2 * t[3] + t[4] + 2) >> 2);
            set(out, 5, 1, (t[2] + 2 * t[3] + t[4] + 2) >> 2);
            set(out, 6, 2, (t[2] + 2 * t[3] + t[4] + 2) >> 2);
            set(out, 7, 3, (t[2] + 2 * t[3] + t[4] + 2) >> 2);
            set(out, 5, 0, (t[3] + 2 * t[4] + t[5] + 2) >> 2);
            set(out, 6, 1, (t[3] + 2 * t[4] + t[5] + 2) >> 2);
            set(out, 7, 2, (t[3] + 2 * t[4] + t[5] + 2) >> 2);
            set(out, 6, 0, (t[4] + 2 * t[5] + t[6] + 2) >> 2);
            set(out, 7, 1, (t[4] + 2 * t[5] + t[6] + 2) >> 2);
            set(out, 7, 0, (t[5] + 2 * t[6] + t[7] + 2) >> 2);
        }
        // Intra_8x8_Vertical_Right (spec 8-67) — literal port of
        // ffmpeg `pred8x8l_vertical_right`.
        5 => {
            if !(has_l && has_t && has_tl) {
                dc_fallback(out, &l, &t, has_l, has_t);
                return;
            }
            set(out, 0, 6, (l[5] + 2 * l[4] + l[3] + 2) >> 2);
            set(out, 0, 7, (l[6] + 2 * l[5] + l[4] + 2) >> 2);
            set(out, 0, 4, (l[3] + 2 * l[2] + l[1] + 2) >> 2);
            set(out, 1, 6, (l[3] + 2 * l[2] + l[1] + 2) >> 2);
            set(out, 0, 5, (l[4] + 2 * l[3] + l[2] + 2) >> 2);
            set(out, 1, 7, (l[4] + 2 * l[3] + l[2] + 2) >> 2);
            set(out, 0, 2, (l[1] + 2 * l[0] + lt + 2) >> 2);
            set(out, 1, 4, (l[1] + 2 * l[0] + lt + 2) >> 2);
            set(out, 2, 6, (l[1] + 2 * l[0] + lt + 2) >> 2);
            set(out, 0, 3, (l[2] + 2 * l[1] + l[0] + 2) >> 2);
            set(out, 1, 5, (l[2] + 2 * l[1] + l[0] + 2) >> 2);
            set(out, 2, 7, (l[2] + 2 * l[1] + l[0] + 2) >> 2);
            set(out, 0, 1, (l[0] + 2 * lt + t[0] + 2) >> 2);
            set(out, 1, 3, (l[0] + 2 * lt + t[0] + 2) >> 2);
            set(out, 2, 5, (l[0] + 2 * lt + t[0] + 2) >> 2);
            set(out, 3, 7, (l[0] + 2 * lt + t[0] + 2) >> 2);
            set(out, 0, 0, (lt + t[0] + 1) >> 1);
            set(out, 1, 2, (lt + t[0] + 1) >> 1);
            set(out, 2, 4, (lt + t[0] + 1) >> 1);
            set(out, 3, 6, (lt + t[0] + 1) >> 1);
            set(out, 1, 1, (lt + 2 * t[0] + t[1] + 2) >> 2);
            set(out, 2, 3, (lt + 2 * t[0] + t[1] + 2) >> 2);
            set(out, 3, 5, (lt + 2 * t[0] + t[1] + 2) >> 2);
            set(out, 4, 7, (lt + 2 * t[0] + t[1] + 2) >> 2);
            set(out, 1, 0, (t[0] + t[1] + 1) >> 1);
            set(out, 2, 2, (t[0] + t[1] + 1) >> 1);
            set(out, 3, 4, (t[0] + t[1] + 1) >> 1);
            set(out, 4, 6, (t[0] + t[1] + 1) >> 1);
            set(out, 2, 1, (t[0] + 2 * t[1] + t[2] + 2) >> 2);
            set(out, 3, 3, (t[0] + 2 * t[1] + t[2] + 2) >> 2);
            set(out, 4, 5, (t[0] + 2 * t[1] + t[2] + 2) >> 2);
            set(out, 5, 7, (t[0] + 2 * t[1] + t[2] + 2) >> 2);
            set(out, 2, 0, (t[1] + t[2] + 1) >> 1);
            set(out, 3, 2, (t[1] + t[2] + 1) >> 1);
            set(out, 4, 4, (t[1] + t[2] + 1) >> 1);
            set(out, 5, 6, (t[1] + t[2] + 1) >> 1);
            set(out, 3, 1, (t[1] + 2 * t[2] + t[3] + 2) >> 2);
            set(out, 4, 3, (t[1] + 2 * t[2] + t[3] + 2) >> 2);
            set(out, 5, 5, (t[1] + 2 * t[2] + t[3] + 2) >> 2);
            set(out, 6, 7, (t[1] + 2 * t[2] + t[3] + 2) >> 2);
            set(out, 3, 0, (t[2] + t[3] + 1) >> 1);
            set(out, 4, 2, (t[2] + t[3] + 1) >> 1);
            set(out, 5, 4, (t[2] + t[3] + 1) >> 1);
            set(out, 6, 6, (t[2] + t[3] + 1) >> 1);
            set(out, 4, 1, (t[2] + 2 * t[3] + t[4] + 2) >> 2);
            set(out, 5, 3, (t[2] + 2 * t[3] + t[4] + 2) >> 2);
            set(out, 6, 5, (t[2] + 2 * t[3] + t[4] + 2) >> 2);
            set(out, 7, 7, (t[2] + 2 * t[3] + t[4] + 2) >> 2);
            set(out, 4, 0, (t[3] + t[4] + 1) >> 1);
            set(out, 5, 2, (t[3] + t[4] + 1) >> 1);
            set(out, 6, 4, (t[3] + t[4] + 1) >> 1);
            set(out, 7, 6, (t[3] + t[4] + 1) >> 1);
            set(out, 5, 1, (t[3] + 2 * t[4] + t[5] + 2) >> 2);
            set(out, 6, 3, (t[3] + 2 * t[4] + t[5] + 2) >> 2);
            set(out, 7, 5, (t[3] + 2 * t[4] + t[5] + 2) >> 2);
            set(out, 5, 0, (t[4] + t[5] + 1) >> 1);
            set(out, 6, 2, (t[4] + t[5] + 1) >> 1);
            set(out, 7, 4, (t[4] + t[5] + 1) >> 1);
            set(out, 6, 1, (t[4] + 2 * t[5] + t[6] + 2) >> 2);
            set(out, 7, 3, (t[4] + 2 * t[5] + t[6] + 2) >> 2);
            set(out, 6, 0, (t[5] + t[6] + 1) >> 1);
            set(out, 7, 2, (t[5] + t[6] + 1) >> 1);
            set(out, 7, 1, (t[5] + 2 * t[6] + t[7] + 2) >> 2);
            set(out, 7, 0, (t[6] + t[7] + 1) >> 1);
        }
        // Intra_8x8_Horizontal_Down (spec 8-68) — literal port of
        // ffmpeg `pred8x8l_horizontal_down`.
        6 => {
            if !(has_l && has_t && has_tl) {
                dc_fallback(out, &l, &t, has_l, has_t);
                return;
            }
            set(out, 0, 7, (l[6] + l[7] + 1) >> 1);
            set(out, 1, 7, (l[5] + 2 * l[6] + l[7] + 2) >> 2);
            set(out, 0, 6, (l[5] + l[6] + 1) >> 1);
            set(out, 2, 7, (l[5] + l[6] + 1) >> 1);
            set(out, 1, 6, (l[4] + 2 * l[5] + l[6] + 2) >> 2);
            set(out, 3, 7, (l[4] + 2 * l[5] + l[6] + 2) >> 2);
            set(out, 0, 5, (l[4] + l[5] + 1) >> 1);
            set(out, 2, 6, (l[4] + l[5] + 1) >> 1);
            set(out, 4, 7, (l[4] + l[5] + 1) >> 1);
            set(out, 1, 5, (l[3] + 2 * l[4] + l[5] + 2) >> 2);
            set(out, 3, 6, (l[3] + 2 * l[4] + l[5] + 2) >> 2);
            set(out, 5, 7, (l[3] + 2 * l[4] + l[5] + 2) >> 2);
            set(out, 0, 4, (l[3] + l[4] + 1) >> 1);
            set(out, 2, 5, (l[3] + l[4] + 1) >> 1);
            set(out, 4, 6, (l[3] + l[4] + 1) >> 1);
            set(out, 6, 7, (l[3] + l[4] + 1) >> 1);
            set(out, 1, 4, (l[2] + 2 * l[3] + l[4] + 2) >> 2);
            set(out, 3, 5, (l[2] + 2 * l[3] + l[4] + 2) >> 2);
            set(out, 5, 6, (l[2] + 2 * l[3] + l[4] + 2) >> 2);
            set(out, 7, 7, (l[2] + 2 * l[3] + l[4] + 2) >> 2);
            set(out, 0, 3, (l[2] + l[3] + 1) >> 1);
            set(out, 2, 4, (l[2] + l[3] + 1) >> 1);
            set(out, 4, 5, (l[2] + l[3] + 1) >> 1);
            set(out, 6, 6, (l[2] + l[3] + 1) >> 1);
            set(out, 1, 3, (l[1] + 2 * l[2] + l[3] + 2) >> 2);
            set(out, 3, 4, (l[1] + 2 * l[2] + l[3] + 2) >> 2);
            set(out, 5, 5, (l[1] + 2 * l[2] + l[3] + 2) >> 2);
            set(out, 7, 6, (l[1] + 2 * l[2] + l[3] + 2) >> 2);
            set(out, 0, 2, (l[1] + l[2] + 1) >> 1);
            set(out, 2, 3, (l[1] + l[2] + 1) >> 1);
            set(out, 4, 4, (l[1] + l[2] + 1) >> 1);
            set(out, 6, 5, (l[1] + l[2] + 1) >> 1);
            set(out, 1, 2, (l[0] + 2 * l[1] + l[2] + 2) >> 2);
            set(out, 3, 3, (l[0] + 2 * l[1] + l[2] + 2) >> 2);
            set(out, 5, 4, (l[0] + 2 * l[1] + l[2] + 2) >> 2);
            set(out, 7, 5, (l[0] + 2 * l[1] + l[2] + 2) >> 2);
            set(out, 0, 1, (l[0] + l[1] + 1) >> 1);
            set(out, 2, 2, (l[0] + l[1] + 1) >> 1);
            set(out, 4, 3, (l[0] + l[1] + 1) >> 1);
            set(out, 6, 4, (l[0] + l[1] + 1) >> 1);
            set(out, 1, 1, (lt + 2 * l[0] + l[1] + 2) >> 2);
            set(out, 3, 2, (lt + 2 * l[0] + l[1] + 2) >> 2);
            set(out, 5, 3, (lt + 2 * l[0] + l[1] + 2) >> 2);
            set(out, 7, 4, (lt + 2 * l[0] + l[1] + 2) >> 2);
            set(out, 0, 0, (lt + l[0] + 1) >> 1);
            set(out, 2, 1, (lt + l[0] + 1) >> 1);
            set(out, 4, 2, (lt + l[0] + 1) >> 1);
            set(out, 6, 3, (lt + l[0] + 1) >> 1);
            set(out, 1, 0, (l[0] + 2 * lt + t[0] + 2) >> 2);
            set(out, 3, 1, (l[0] + 2 * lt + t[0] + 2) >> 2);
            set(out, 5, 2, (l[0] + 2 * lt + t[0] + 2) >> 2);
            set(out, 7, 3, (l[0] + 2 * lt + t[0] + 2) >> 2);
            set(out, 2, 0, (t[1] + 2 * t[0] + lt + 2) >> 2);
            set(out, 4, 1, (t[1] + 2 * t[0] + lt + 2) >> 2);
            set(out, 6, 2, (t[1] + 2 * t[0] + lt + 2) >> 2);
            set(out, 3, 0, (t[2] + 2 * t[1] + t[0] + 2) >> 2);
            set(out, 5, 1, (t[2] + 2 * t[1] + t[0] + 2) >> 2);
            set(out, 7, 2, (t[2] + 2 * t[1] + t[0] + 2) >> 2);
            set(out, 4, 0, (t[3] + 2 * t[2] + t[1] + 2) >> 2);
            set(out, 6, 1, (t[3] + 2 * t[2] + t[1] + 2) >> 2);
            set(out, 5, 0, (t[4] + 2 * t[3] + t[2] + 2) >> 2);
            set(out, 7, 1, (t[4] + 2 * t[3] + t[2] + 2) >> 2);
            set(out, 6, 0, (t[5] + 2 * t[4] + t[3] + 2) >> 2);
            set(out, 7, 0, (t[6] + 2 * t[5] + t[4] + 2) >> 2);
        }
        // Intra_8x8_Vertical_Left (spec 8-69): top + top-right only —
        // literal port of ffmpeg `pred8x8l_vertical_left`.
        7 => {
            if !has_t {
                let dc = ((l.iter().sum::<i32>() + 4) >> 3) as u8;
                out.fill(dc);
                return;
            }
            set(out, 0, 0, (t[0] + t[1] + 1) >> 1);
            set(out, 0, 1, (t[0] + 2 * t[1] + t[2] + 2) >> 2);
            set(out, 0, 2, (t[1] + t[2] + 1) >> 1);
            set(out, 1, 0, (t[1] + t[2] + 1) >> 1);
            set(out, 0, 3, (t[1] + 2 * t[2] + t[3] + 2) >> 2);
            set(out, 1, 1, (t[1] + 2 * t[2] + t[3] + 2) >> 2);
            set(out, 0, 4, (t[2] + t[3] + 1) >> 1);
            set(out, 1, 2, (t[2] + t[3] + 1) >> 1);
            set(out, 2, 0, (t[2] + t[3] + 1) >> 1);
            set(out, 0, 5, (t[2] + 2 * t[3] + t[4] + 2) >> 2);
            set(out, 1, 3, (t[2] + 2 * t[3] + t[4] + 2) >> 2);
            set(out, 2, 1, (t[2] + 2 * t[3] + t[4] + 2) >> 2);
            set(out, 0, 6, (t[3] + t[4] + 1) >> 1);
            set(out, 1, 4, (t[3] + t[4] + 1) >> 1);
            set(out, 2, 2, (t[3] + t[4] + 1) >> 1);
            set(out, 3, 0, (t[3] + t[4] + 1) >> 1);
            set(out, 0, 7, (t[3] + 2 * t[4] + t[5] + 2) >> 2);
            set(out, 1, 5, (t[3] + 2 * t[4] + t[5] + 2) >> 2);
            set(out, 2, 3, (t[3] + 2 * t[4] + t[5] + 2) >> 2);
            set(out, 3, 1, (t[3] + 2 * t[4] + t[5] + 2) >> 2);
            set(out, 1, 6, (t[4] + t[5] + 1) >> 1);
            set(out, 2, 4, (t[4] + t[5] + 1) >> 1);
            set(out, 3, 2, (t[4] + t[5] + 1) >> 1);
            set(out, 4, 0, (t[4] + t[5] + 1) >> 1);
            set(out, 1, 7, (t[4] + 2 * t[5] + t[6] + 2) >> 2);
            set(out, 2, 5, (t[4] + 2 * t[5] + t[6] + 2) >> 2);
            set(out, 3, 3, (t[4] + 2 * t[5] + t[6] + 2) >> 2);
            set(out, 4, 1, (t[4] + 2 * t[5] + t[6] + 2) >> 2);
            set(out, 2, 6, (t[5] + t[6] + 1) >> 1);
            set(out, 3, 4, (t[5] + t[6] + 1) >> 1);
            set(out, 4, 2, (t[5] + t[6] + 1) >> 1);
            set(out, 5, 0, (t[5] + t[6] + 1) >> 1);
            set(out, 2, 7, (t[5] + 2 * t[6] + t[7] + 2) >> 2);
            set(out, 3, 5, (t[5] + 2 * t[6] + t[7] + 2) >> 2);
            set(out, 4, 3, (t[5] + 2 * t[6] + t[7] + 2) >> 2);
            set(out, 5, 1, (t[5] + 2 * t[6] + t[7] + 2) >> 2);
            set(out, 3, 6, (t[6] + t[7] + 1) >> 1);
            set(out, 4, 4, (t[6] + t[7] + 1) >> 1);
            set(out, 5, 2, (t[6] + t[7] + 1) >> 1);
            set(out, 6, 0, (t[6] + t[7] + 1) >> 1);
            set(out, 3, 7, (t[6] + 2 * t[7] + t[8] + 2) >> 2);
            set(out, 4, 5, (t[6] + 2 * t[7] + t[8] + 2) >> 2);
            set(out, 5, 3, (t[6] + 2 * t[7] + t[8] + 2) >> 2);
            set(out, 6, 1, (t[6] + 2 * t[7] + t[8] + 2) >> 2);
            set(out, 4, 6, (t[7] + t[8] + 1) >> 1);
            set(out, 5, 4, (t[7] + t[8] + 1) >> 1);
            set(out, 6, 2, (t[7] + t[8] + 1) >> 1);
            set(out, 7, 0, (t[7] + t[8] + 1) >> 1);
            set(out, 4, 7, (t[7] + 2 * t[8] + t[9] + 2) >> 2);
            set(out, 5, 5, (t[7] + 2 * t[8] + t[9] + 2) >> 2);
            set(out, 6, 3, (t[7] + 2 * t[8] + t[9] + 2) >> 2);
            set(out, 7, 1, (t[7] + 2 * t[8] + t[9] + 2) >> 2);
            set(out, 5, 6, (t[8] + t[9] + 1) >> 1);
            set(out, 6, 4, (t[8] + t[9] + 1) >> 1);
            set(out, 7, 2, (t[8] + t[9] + 1) >> 1);
            set(out, 5, 7, (t[8] + 2 * t[9] + t[10] + 2) >> 2);
            set(out, 6, 5, (t[8] + 2 * t[9] + t[10] + 2) >> 2);
            set(out, 7, 3, (t[8] + 2 * t[9] + t[10] + 2) >> 2);
            set(out, 6, 6, (t[9] + t[10] + 1) >> 1);
            set(out, 7, 4, (t[9] + t[10] + 1) >> 1);
            set(out, 6, 7, (t[9] + 2 * t[10] + t[11] + 2) >> 2);
            set(out, 7, 5, (t[9] + 2 * t[10] + t[11] + 2) >> 2);
            set(out, 7, 6, (t[10] + t[11] + 1) >> 1);
            set(out, 7, 7, (t[10] + 2 * t[11] + t[12] + 2) >> 2);
        }
        // Intra_8x8_Horizontal_Up (spec 8-70): left only — literal
        // port of ffmpeg `pred8x8l_horizontal_up`.
        _ => {
            debug_assert_eq!(mode, 8);
            if !has_l {
                let dc = ((t[..8].iter().sum::<i32>() + 4) >> 3) as u8;
                out.fill(dc);
                return;
            }
            set(out, 0, 0, (l[0] + l[1] + 1) >> 1);
            set(out, 1, 0, (l[0] + 2 * l[1] + l[2] + 2) >> 2);
            set(out, 0, 1, (l[1] + l[2] + 1) >> 1);
            set(out, 2, 0, (l[1] + l[2] + 1) >> 1);
            set(out, 1, 1, (l[1] + 2 * l[2] + l[3] + 2) >> 2);
            set(out, 3, 0, (l[1] + 2 * l[2] + l[3] + 2) >> 2);
            set(out, 0, 2, (l[2] + l[3] + 1) >> 1);
            set(out, 2, 1, (l[2] + l[3] + 1) >> 1);
            set(out, 4, 0, (l[2] + l[3] + 1) >> 1);
            set(out, 1, 2, (l[2] + 2 * l[3] + l[4] + 2) >> 2);
            set(out, 3, 1, (l[2] + 2 * l[3] + l[4] + 2) >> 2);
            set(out, 5, 0, (l[2] + 2 * l[3] + l[4] + 2) >> 2);
            set(out, 0, 3, (l[3] + l[4] + 1) >> 1);
            set(out, 2, 2, (l[3] + l[4] + 1) >> 1);
            set(out, 4, 1, (l[3] + l[4] + 1) >> 1);
            set(out, 6, 0, (l[3] + l[4] + 1) >> 1);
            set(out, 1, 3, (l[3] + 2 * l[4] + l[5] + 2) >> 2);
            set(out, 3, 2, (l[3] + 2 * l[4] + l[5] + 2) >> 2);
            set(out, 5, 1, (l[3] + 2 * l[4] + l[5] + 2) >> 2);
            set(out, 7, 0, (l[3] + 2 * l[4] + l[5] + 2) >> 2);
            set(out, 0, 4, (l[4] + l[5] + 1) >> 1);
            set(out, 2, 3, (l[4] + l[5] + 1) >> 1);
            set(out, 4, 2, (l[4] + l[5] + 1) >> 1);
            set(out, 6, 1, (l[4] + l[5] + 1) >> 1);
            set(out, 1, 4, (l[4] + 2 * l[5] + l[6] + 2) >> 2);
            set(out, 3, 3, (l[4] + 2 * l[5] + l[6] + 2) >> 2);
            set(out, 5, 2, (l[4] + 2 * l[5] + l[6] + 2) >> 2);
            set(out, 7, 1, (l[4] + 2 * l[5] + l[6] + 2) >> 2);
            set(out, 0, 5, (l[5] + l[6] + 1) >> 1);
            set(out, 2, 4, (l[5] + l[6] + 1) >> 1);
            set(out, 4, 3, (l[5] + l[6] + 1) >> 1);
            set(out, 6, 2, (l[5] + l[6] + 1) >> 1);
            set(out, 1, 5, (l[5] + 2 * l[6] + l[7] + 2) >> 2);
            set(out, 3, 4, (l[5] + 2 * l[6] + l[7] + 2) >> 2);
            set(out, 5, 3, (l[5] + 2 * l[6] + l[7] + 2) >> 2);
            set(out, 7, 2, (l[5] + 2 * l[6] + l[7] + 2) >> 2);
            set(out, 0, 6, (l[6] + l[7] + 1) >> 1);
            set(out, 2, 5, (l[6] + l[7] + 1) >> 1);
            set(out, 4, 4, (l[6] + l[7] + 1) >> 1);
            set(out, 6, 3, (l[6] + l[7] + 1) >> 1);
            set(out, 1, 6, (l[6] + 3 * l[7] + 2) >> 2);
            set(out, 3, 5, (l[6] + 3 * l[7] + 2) >> 2);
            set(out, 5, 4, (l[6] + 3 * l[7] + 2) >> 2);
            set(out, 7, 3, (l[6] + 3 * l[7] + 2) >> 2);
            set(out, 0, 7, l[7]);
            set(out, 1, 7, l[7]);
            set(out, 2, 6, l[7]);
            set(out, 2, 7, l[7]);
            set(out, 3, 6, l[7]);
            set(out, 3, 7, l[7]);
            set(out, 4, 5, l[7]);
            set(out, 4, 6, l[7]);
            set(out, 4, 7, l[7]);
            set(out, 5, 5, l[7]);
            set(out, 5, 6, l[7]);
            set(out, 5, 7, l[7]);
            set(out, 6, 4, l[7]);
            set(out, 6, 5, l[7]);
            set(out, 6, 6, l[7]);
            set(out, 6, 7, l[7]);
            set(out, 7, 4, l[7]);
            set(out, 7, 5, l[7]);
            set(out, 7, 6, l[7]);
            set(out, 7, 7, l[7]);
        }
    }
}

/// Shared DC fallback for the tl-dependent modes.
fn dc_fallback(out: &mut [u8; 64], l: &[i32; 8], t: &[i32; 16], has_l: bool, has_t: bool) {
    let dc = if has_l && has_t {
        ((l.iter().sum::<i32>() + t[..8].iter().sum::<i32>() + 8) >> 4) as u8
    } else if has_l {
        ((l.iter().sum::<i32>() + 4) >> 3) as u8
    } else if has_t {
        ((t[..8].iter().sum::<i32>() + 4) >> 3) as u8
    } else {
        128
    };
    out.fill(dc);
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

#[cfg(test)]
mod dc_fallback_tests {
    use super::dc_fallback;

    #[test]
    fn dc_fallback_arms() {
        let l = [8i32; 8];
        let t = [4i32; 16];
        let mut out = [0u8; 64];
        // Both: (sum(l) + sum(t[..8]) + 8) >> 4 = (64 + 32 + 8) >> 4.
        dc_fallback(&mut out, &l, &t, true, true);
        assert_eq!(out[0], ((64 + 32 + 8) >> 4) as u8);
        // Left only: (sum(l) + 4) >> 3 = 68 >> 3 = 8.
        dc_fallback(&mut out, &l, &t, true, false);
        assert_eq!(out[0], ((64 + 4) >> 3) as u8);
        // Top only: (sum(t[..8]) + 4) >> 3 = 36 >> 3 = 4.
        dc_fallback(&mut out, &l, &t, false, true);
        assert_eq!(out[0], ((32 + 4) >> 3) as u8);
        // Neither: constant 128.
        dc_fallback(&mut out, &l, &t, false, false);
        assert_eq!(out[0], 128);
    }
}
