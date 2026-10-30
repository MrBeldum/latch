//! Filtering follows H.264 section 8.7, including edges between slices.
use super::{transform::chroma_qp, worker::Mb};
use crate::h264::loopfilter::{self, Edge, ALPHA, BETA, TC0};
use crate::Yuv420;
pub(super) fn filter(p: &mut Yuv420, mbs: &[Mb]) {
    let w = p.padded_width() / 16;
    let h = p.padded_height() / 16;
    for my in 0..h {
        for mx in 0..w {
            let index = my * w + mx;
            let cur = &mbs[index];
            for vertical in [true, false] {
                for edge in 0..4 {
                    if edge == 0 && if vertical { mx == 0 } else { my == 0 } {
                        continue;
                    }
                    let other = if edge > 0 {
                        cur
                    } else {
                        &mbs[index - if vertical { 1 } else { w }]
                    };
                    let qp = (cur.qp as usize + other.qp as usize + 1) >> 1;
                    let cqp = (chroma_qp(cur.qp) as usize + chroma_qp(other.qp) as usize + 1) >> 1;
                    let strengths = std::array::from_fn(|segment| {
                        let (cx, cy) = if vertical {
                            (edge, segment)
                        } else {
                            (segment, edge)
                        };
                        let (ox, oy) = if vertical {
                            (if edge == 0 { 3 } else { edge - 1 }, segment)
                        } else {
                            (segment, if edge == 0 { 3 } else { edge - 1 })
                        };
                        if cur.intra || other.intra {
                            if edge == 0 {
                                4
                            } else {
                                3
                            }
                        } else if cur.nz[cy * 4 + cx] > 0 || other.nz[oy * 4 + ox] > 0 {
                            2
                        } else if (cur.mv.0 - other.mv.0).abs() >= 4
                            || (cur.mv.1 - other.mv.1).abs() >= 4
                        {
                            1
                        } else {
                            0
                        }
                    });
                    let edge_config = |qp: usize| Edge {
                        alpha: ALPHA[qp],
                        beta: BETA[qp],
                        bs: strengths,
                        tc0: strengths.map(|b| {
                            if b == 0 || b == 4 {
                                0
                            } else {
                                TC0[qp][b as usize - 1]
                            }
                        }),
                    };
                    let x = mx * 16 + if vertical { edge * 4 } else { 0 };
                    let y = my * 16 + if vertical { 0 } else { edge * 4 };
                    loopfilter::luma(
                        &mut p.y,
                        p.y_stride,
                        y * p.y_stride + x,
                        vertical,
                        &edge_config(qp),
                    );
                    if edge % 2 == 0 {
                        let chroma = edge_config(cqp);
                        loopfilter::chroma(
                            &mut p.u,
                            &mut p.v,
                            p.c_stride,
                            y / 2 * p.c_stride + x / 2,
                            vertical,
                            &chroma,
                            &chroma,
                        );
                    }
                }
            }
        }
    }
}
