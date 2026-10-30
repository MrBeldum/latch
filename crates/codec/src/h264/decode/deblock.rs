//! In-loop deblocking filter (spec 8.7), frame-picture subset:
//! vertical luma edges left-to-right per 4-sample set, then horizontal,
//! then the chroma edges. Boundary strength per spec 8.7.2.1 with
//! `mixedModeEdgeFlag = 0` (frame pictures only), threshold derivation
//! per 8.7.2.2 (Tables 8-16/8-17), weak filtering per 8.7.2.3 and the
//! strong intra path per 8.7.2.4.
//!
//! The two tuneable knobs come from each MB's own slice (spec: the
//! values of the slice containing sample q0): `disable_deblocking_
//! filter_idc` 1 disables filtering for that MB's controlled edges,
//! and idc 2 additionally skips edges that sit on slice boundaries.

use super::mb::MbState;
use super::tables::{block_index, ALPHA_TABLE, BETA_TABLE, TC0_TABLE};

/// Boundary strengths for one MB: vertical edges at luma x = {0,4,8,12}
/// and horizontal at y = {0,4,8,12}; each entry is the strength of the
/// 4-sample set `k`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct BsMap {
    v: [[u8; 4]; 4],
    h: [[u8; 4]; 4],
}

/// Boundary strengths follow 8.7.2.1 on the luma 4x4 grid. Slice
/// boundaries affect availability only when the controlling slice uses idc 2.
fn compute_bs(cur: &MbState, mb_a: Option<&MbState>, mb_b: Option<&MbState>) -> BsMap {
    if cur.mb_type.is_intra() && cur.disable_deblock_idc != 1 {
        let mut result = BsMap {
            v: [[3; 4]; 4],
            h: [[3; 4]; 4],
        };
        for (edges, neighbour) in [(&mut result.v, mb_a), (&mut result.h, mb_b)] {
            edges[0] = if neighbour
                .is_some_and(|p| cur.disable_deblock_idc != 2 || p.slice_id == cur.slice_id)
            {
                [4; 4]
            } else {
                [0; 4]
            };
        }
        return result;
    }
    compute_bs_general(cur, mb_a, mb_b)
}

fn compute_bs_general(cur: &MbState, mb_a: Option<&MbState>, mb_b: Option<&MbState>) -> BsMap {
    let mut result = BsMap {
        v: [[0; 4]; 4],
        h: [[0; 4]; 4],
    };
    if cur.disable_deblock_idc == 1 {
        return result;
    }
    for dir in 0..2 {
        for edge in 0..4 {
            let other = if edge == 0 {
                if dir == 0 {
                    mb_a
                } else {
                    mb_b
                }
            } else {
                Some(cur)
            };
            let Some(p) = other else {
                continue;
            };
            if edge == 0 && cur.disable_deblock_idc == 2 && p.slice_id != cur.slice_id {
                continue;
            }
            for k in 0..4 {
                let (q_idx, p_idx) = if dir == 0 {
                    (block_index(edge, k), block_index((edge + 3) % 4, k))
                } else {
                    (block_index(k, edge), block_index(k, (edge + 3) % 4))
                };
                let strength = if cur.mb_type.is_intra() || p.mb_type.is_intra() {
                    if edge == 0 {
                        4
                    } else {
                        3
                    }
                } else if cur.nz[q_idx] != 0 || p.nz[p_idx] != 0 {
                    2
                } else if cur.ref_idx[q_idx] != p.ref_idx[p_idx]
                    || (i32::from(cur.mv[q_idx][0]) - i32::from(p.mv[p_idx][0])).abs() >= 4
                    || (i32::from(cur.mv[q_idx][1]) - i32::from(p.mv[p_idx][1])).abs() >= 4
                {
                    1
                } else {
                    0
                };
                if dir == 0 {
                    result.v[edge][k] = strength;
                } else {
                    result.h[edge][k] = strength;
                }
            }
        }
    }
    result
}

/// the filtered p0..p2 / q0..q2 as `(p', q')` with 3 valid entries.
fn clip3(low: i32, high: i32, value: i32) -> i32 {
    value.clamp(low, high)
}

#[inline(always)]
fn filter_set(
    p: &[i32; 4],
    q: &[i32; 4],
    bs: u8,
    index_a: usize,
    alpha: i32,
    beta: i32,
    chroma: bool,
) -> ([i32; 3], [i32; 3]) {
    // Callers pass p = [p0, p1, p2, p3], q = [q0..q3].
    let (p0, p1, p2, p3) = (p[0], p[1], p[2], p[3]);
    let (q0, q1, q2, q3) = (q[0], q[1], q[2], q[3]);
    let mut po = [p0, p1, p2];
    let mut qo = [q0, q1, q2];
    if bs == 0 || (p0 - q0).abs() >= alpha || (p1 - p0).abs() >= beta || (q1 - q0).abs() >= beta {
        return (po, qo);
    }
    let ap = (p2 - p0).abs();
    let aq = (q2 - q0).abs();
    if bs < 4 {
        let tc0 = i32::from(TC0_TABLE[index_a][(bs - 1) as usize]);
        let tc = if chroma {
            tc0 + 1
        } else {
            tc0 + i32::from(ap < beta) + i32::from(aq < beta)
        };
        let delta = clip3(-tc, tc, ((q0 - p0) * 4 + (p1 - q1) + 4) >> 3);
        po[0] = (p0 + delta).clamp(0, 255);
        qo[0] = (q0 - delta).clamp(0, 255);
        if !chroma {
            if ap < beta {
                po[1] = p1 + clip3(-tc0, tc0, (p2 + ((p0 + q0 + 1) >> 1) - 2 * p1) >> 1);
            }
            if aq < beta {
                qo[1] = q1 + clip3(-tc0, tc0, (q2 + ((p0 + q0 + 1) >> 1) - 2 * q1) >> 1);
            }
        }
    } else {
        // bS == 4 (spec 8.7.2.4); chroma edges always take the weak
        // branch of 8-480/8-487 (chromaStyleFilteringFlag = 1).
        if !chroma && ap < beta && (p0 - q0).abs() < (alpha >> 2) + 2 {
            po[0] = (p2 + 2 * p1 + 2 * p0 + 2 * q0 + q1 + 4) >> 3;
            po[1] = (p2 + p1 + p0 + q0 + 2) >> 2;
            po[2] = (2 * p3 + 3 * p2 + p1 + p0 + q0 + 4) >> 3;
        } else {
            po[0] = (2 * p1 + p0 + q1 + 2) >> 2;
        }
        if !chroma && aq < beta && (p0 - q0).abs() < (alpha >> 2) + 2 {
            qo[0] = (p1 + 2 * p0 + 2 * q0 + 2 * q1 + q2 + 4) >> 3;
            qo[1] = (p0 + q0 + q1 + q2 + 2) >> 2;
            qo[2] = (2 * q3 + 3 * q2 + q1 + q0 + p0 + 4) >> 3;
        } else {
            qo[0] = (2 * q1 + q0 + p1 + 2) >> 2;
        }
    }
    (po, qo)
}

/// Filters a vertical edge: 4 sets of samples, each on row `y0+k`,
/// crossing column `x`. `plane` row-major, `stride` bytes.
#[allow(clippy::too_many_arguments)]
fn filter_v_edge(
    plane: &mut [u8],
    stride: usize,
    x: usize,
    y0: usize,
    bs: &[u8; 4],
    qp_p: u8,
    qp_q: u8,
    off_a: i32,
    off_b: i32,
    chroma: bool,
) {
    if bs.iter().all(|&b| b == 0) {
        return;
    }
    let qp_av = (i32::from(qp_p) + i32::from(qp_q) + 1) >> 1;
    let index_a = (qp_av + off_a).clamp(0, 51) as usize;
    let alpha = i32::from(ALPHA_TABLE[index_a]);
    let beta = i32::from(BETA_TABLE[(qp_av + off_b).clamp(0, 51) as usize]);
    if alpha == 0 || beta == 0 {
        return;
    }
    for (k, &b) in bs.iter().enumerate() {
        if b == 0 {
            continue;
        }
        let y = y0 + k;
        let row = &mut plane[y * stride..y * stride + stride];
        if (i32::from(row[x - 1]) - i32::from(row[x])).abs() >= alpha
            || (i32::from(row[x - 2]) - i32::from(row[x - 1])).abs() >= beta
            || (i32::from(row[x + 1]) - i32::from(row[x])).abs() >= beta
        {
            continue;
        }
        let p: [i32; 4] = [
            i32::from(row[x - 1]),
            i32::from(row[x - 2]),
            i32::from(row[x - 3]),
            i32::from(row[x - 4]),
        ];
        let q: [i32; 4] = [
            i32::from(row[x]),
            i32::from(row[x + 1]),
            i32::from(row[x + 2]),
            i32::from(row[x + 3]),
        ];
        // Order for filter_set: [p0..p3] ascending distance.
        let p_ord = [p[0], p[1], p[2], p[3]];
        let (po, qo) = filter_set(&p_ord, &q, b, index_a, alpha, beta, chroma);
        row[x - 1] = po[0].clamp(0, 255) as u8;
        row[x - 2] = po[1].clamp(0, 255) as u8;
        row[x - 3] = po[2].clamp(0, 255) as u8;
        row[x] = qo[0].clamp(0, 255) as u8;
        row[x + 1] = qo[1].clamp(0, 255) as u8;
        row[x + 2] = qo[2].clamp(0, 255) as u8;
    }
}

/// Runs the whole deblocking pass over a reconstructed frame.
///
/// `y`, `cb`, `cr` are the picture planes (row-major, `stride` luma
/// and `stride/2` chroma); `mbs` is the per-MB state array indexed by
/// `mb_y * width_mbs + mb_x`. `qp_chroma` maps a luma QP to the chroma
/// QP the spec uses on chroma edges (Table 8-15 via the per-plane PPS
/// offsets `chroma_off_cb`/`chroma_off_cr`).
#[allow(clippy::too_many_arguments)]
pub(crate) fn filter_frame(
    y: &mut [u8],
    cb: &mut [u8],
    cr: &mut [u8],
    stride: usize,
    mbs: &[MbState],
    width_mbs: usize,
    height_mbs: usize,
    qp_chroma: &[u8; 52],
    chroma_off_cb: i32,
    chroma_off_cr: i32,
) {
    // Precompute the per-plane luma-QP -> chroma-QP tables (52 entries
    // each) so every edge lookup is a single index.
    let mut qpcb = [0u8; 52];
    let mut qpcr = [0u8; 52];
    for (i, v) in qpcb.iter_mut().enumerate() {
        *v = qp_chroma[(i as i32 + chroma_off_cb).clamp(0, 51) as usize];
    }
    for (i, v) in qpcr.iter_mut().enumerate() {
        *v = qp_chroma[(i as i32 + chroma_off_cr).clamp(0, 51) as usize];
    }
    for mb_y in 0..height_mbs {
        for mb_x in 0..width_mbs {
            let idx = mb_y * width_mbs + mb_x;
            let cur = &mbs[idx];
            if cur.disable_deblock_idc == 1 {
                continue;
            }
            let bs = compute_bs(
                cur,
                (mb_x > 0).then(|| &mbs[idx - 1]),
                (mb_y > 0).then(|| &mbs[idx - width_mbs]),
            );
            let off_a = i32::from(cur.filter_offset_a);
            let off_b = i32::from(cur.filter_offset_b);
            let off_ca = off_a; // chroma uses the same slice offsets
            let off_cb = off_b;
            let _ = (off_ca, off_cb);

            // LUMA: per 4-row band interleaved (h264bsd order, proven
            // byte-exact on frames 0/1): for each band e: vertical
            // edges x={0,4,8,12} on that band's 4 rows, then horizontal
            // edge y=4*e on all 16 columns.
            for band in 0..4usize {
                let y0 = mb_y * 16 + band * 4;
                for e in 0..4usize {
                    let x = mb_x * 16 + e * 4;
                    let mut qp_p = [0u8; 4];
                    if e == 0 {
                        if mb_x == 0 {
                            continue;
                        }
                        let nb = &mbs[idx - 1];
                        if nb.slice_id != cur.slice_id && cur.disable_deblock_idc == 2 {
                            continue;
                        }
                        qp_p.fill(nb.qp_y);
                    } else {
                        qp_p.fill(cur.qp_y);
                    }
                    let one = bs.v[e][band];
                    filter_v_edge(
                        y, stride, x, y0, &[one; 4], qp_p[0], cur.qp_y, off_a, off_b, false,
                    );
                }
                let yy = mb_y * 16 + band * 4;
                let mut qp_p = [0u8; 4];
                let mut do_edge = true;
                if band == 0 {
                    if mb_y == 0 {
                        do_edge = false;
                    } else {
                        let nb = &mbs[idx - width_mbs];
                        if nb.slice_id != cur.slice_id && cur.disable_deblock_idc == 2 {
                            do_edge = false;
                        } else {
                            qp_p.fill(nb.qp_y);
                        }
                    }
                } else {
                    qp_p.fill(cur.qp_y);
                }
                if do_edge {
                    let edge = thresholds(bs.h[band], qp_p[0], cur.qp_y, off_a, off_b);
                    crate::h264::loopfilter::luma(y, stride, yy * stride + mb_x * 16, false, &edge);
                }
            }

            // The planes are independent; batch both without changing edge order.
            for vertical in [true, false] {
                for e in 0..2usize {
                    let neighbour = if e != 0 {
                        cur
                    } else if vertical {
                        if mb_x == 0 {
                            continue;
                        }
                        &mbs[idx - 1]
                    } else {
                        if mb_y == 0 {
                            continue;
                        }
                        &mbs[idx - width_mbs]
                    };
                    let strengths = if vertical { bs.v[e * 2] } else { bs.h[e * 2] };
                    let u = thresholds(
                        strengths,
                        qpcb[neighbour.qp_y as usize],
                        qpcb[cur.qp_y as usize],
                        off_a,
                        off_b,
                    );
                    let v = thresholds(
                        strengths,
                        qpcr[neighbour.qp_y as usize],
                        qpcr[cur.qp_y as usize],
                        off_a,
                        off_b,
                    );
                    let x = mb_x * 8 + if vertical { e * 4 } else { 0 };
                    let yy = mb_y * 8 + if vertical { 0 } else { e * 4 };
                    crate::h264::loopfilter::chroma(
                        cb,
                        cr,
                        stride / 2,
                        yy * (stride / 2) + x,
                        vertical,
                        &u,
                        &v,
                    );
                }
            }
        }
    }
}

fn thresholds(bs: [u8; 4], p: u8, q: u8, off_a: i32, off_b: i32) -> crate::h264::loopfilter::Edge {
    let qp = (i32::from(p) + i32::from(q) + 1) >> 1;
    let a = (qp + off_a).clamp(0, 51) as usize;
    crate::h264::loopfilter::Edge {
        alpha: ALPHA_TABLE[a],
        beta: BETA_TABLE[(qp + off_b).clamp(0, 51) as usize],
        bs,
        tc0: bs.map(|b| {
            if b == 0 || b == 4 {
                0
            } else {
                TC0_TABLE[a][b as usize - 1]
            }
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::super::mb::MbType;
    use super::*;

    #[test]
    fn intra_strength_shortcut_preserves_picture_and_slice_boundaries() {
        let mut cur = MbState::new();
        let mut neighbour = MbState::new();
        for kind in [
            MbType::I4x4,
            MbType::IPcm,
            MbType::I16x16 {
                pred: 2,
                cbp_chroma: 2,
                cbp_luma: 15,
            },
        ] {
            cur.mb_type = kind;
            for idc in 0..=2 {
                cur.disable_deblock_idc = idc;
                for slice_id in [0, 1] {
                    neighbour.slice_id = slice_id;
                    for a in [None, Some(&neighbour)] {
                        for b in [None, Some(&neighbour)] {
                            assert_eq!(compute_bs(&cur, a, b), compute_bs_general(&cur, a, b));
                        }
                    }
                }
            }
        }
    }
}
