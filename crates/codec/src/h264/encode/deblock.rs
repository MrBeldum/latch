//! Filtering follows H.264 section 8.7, including edges between slices.
use super::{transform::chroma_qp, worker::Mb};
use crate::Yuv420;
const ALPHA: [i32; 52] = [
    0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 4, 4, 5, 6, 7, 8, 9, 10, 12, 13, 15, 17, 20,
    22, 25, 28, 32, 36, 40, 45, 50, 56, 63, 71, 80, 90, 101, 113, 127, 144, 162, 182, 203, 226,
    255, 255,
];
const BETA: [i32; 52] = [
    0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 2, 2, 2, 3, 3, 3, 3, 4, 4, 4, 6, 6, 7, 7, 8, 8,
    9, 9, 10, 10, 11, 11, 12, 12, 13, 13, 14, 14, 15, 15, 16, 16, 17, 17, 18, 18,
];
const TC: [[i32; 4]; 52] = [
    [-1, 0, 0, 0],
    [-1, 0, 0, 0],
    [-1, 0, 0, 0],
    [-1, 0, 0, 0],
    [-1, 0, 0, 0],
    [-1, 0, 0, 0],
    [-1, 0, 0, 0],
    [-1, 0, 0, 0],
    [-1, 0, 0, 0],
    [-1, 0, 0, 0],
    [-1, 0, 0, 0],
    [-1, 0, 0, 0],
    [-1, 0, 0, 0],
    [-1, 0, 0, 0],
    [-1, 0, 0, 0],
    [-1, 0, 0, 0],
    [-1, 0, 0, 0],
    [-1, 0, 0, 1],
    [-1, 0, 0, 1],
    [-1, 0, 0, 1],
    [-1, 0, 0, 1],
    [-1, 0, 1, 1],
    [-1, 0, 1, 1],
    [-1, 1, 1, 1],
    [-1, 1, 1, 1],
    [-1, 1, 1, 1],
    [-1, 1, 1, 1],
    [-1, 1, 1, 2],
    [-1, 1, 1, 2],
    [-1, 1, 1, 2],
    [-1, 1, 1, 2],
    [-1, 1, 2, 3],
    [-1, 1, 2, 3],
    [-1, 2, 2, 3],
    [-1, 2, 2, 4],
    [-1, 2, 3, 4],
    [-1, 2, 3, 4],
    [-1, 3, 3, 5],
    [-1, 3, 4, 6],
    [-1, 3, 4, 6],
    [-1, 4, 5, 7],
    [-1, 4, 5, 8],
    [-1, 4, 6, 9],
    [-1, 5, 7, 10],
    [-1, 6, 8, 11],
    [-1, 6, 8, 13],
    [-1, 7, 10, 14],
    [-1, 8, 11, 16],
    [-1, 9, 12, 18],
    [-1, 10, 13, 20],
    [-1, 11, 15, 23],
    [-1, 13, 17, 25],
];
fn pixel(p: &mut [u8], q: usize, step: usize, bs: usize, qp: usize, chroma: bool) {
    let at = |i: isize| p[(q as isize + i * step as isize) as usize] as i32;
    let (a, b) = (ALPHA[qp], BETA[qp]);
    let (p0, q0) = (at(-1), at(0));
    if (p0 - q0).abs() >= a {
        return;
    }
    let (p1, q1) = (at(-2), at(1));
    if (p1 - p0).abs() >= b || (q1 - q0).abs() >= b {
        return;
    }
    let (p2, q2) = (at(-3), at(2));
    let strong = bs == 4 && !chroma && (p0 - q0).abs() < (a >> 2) + 2;
    let p3 = if strong && (p2 - p0).abs() < b {
        at(-4)
    } else {
        0
    };
    let q3 = if strong && (q2 - q0).abs() < b {
        at(3)
    } else {
        0
    };
    let mut set =
        |i: isize, v: i32| p[(q as isize + i * step as isize) as usize] = v.clamp(0, 255) as u8;
    if bs == 4 {
        if strong && (p2 - p0).abs() < b {
            set(-1, (p2 + 2 * p1 + 2 * p0 + 2 * q0 + q1 + 4) >> 3);
            set(-2, (p2 + p1 + p0 + q0 + 2) >> 2);
            set(-3, (2 * p3 + 3 * p2 + p1 + p0 + q0 + 4) >> 3);
        } else {
            set(-1, (2 * p1 + p0 + q1 + 2) >> 2);
        }
        if strong && (q2 - q0).abs() < b {
            set(0, (p1 + 2 * p0 + 2 * q0 + 2 * q1 + q2 + 4) >> 3);
            set(1, (p0 + q0 + q1 + q2 + 2) >> 2);
            set(2, (2 * q3 + 3 * q2 + q1 + q0 + p0 + 4) >> 3);
        } else {
            set(0, (2 * q1 + q0 + p1 + 2) >> 2);
        }
    } else {
        let tc0 = TC[qp][bs];
        let ap = !chroma && (p2 - p0).abs() < b;
        let aq = !chroma && (q2 - q0).abs() < b;
        let tc = tc0
            + if chroma {
                1
            } else {
                i32::from(ap) + i32::from(aq)
            };
        let delta = (((q0 - p0) * 4 + (p1 - q1) + 4) >> 3).clamp(-tc, tc);
        set(-1, p0 + delta);
        set(0, q0 - delta);
        if ap {
            set(
                -2,
                p1 + ((p2 + ((p0 + q0 + 1) >> 1) - 2 * p1) >> 1).clamp(-tc0, tc0),
            );
        }
        if aq {
            set(
                1,
                q1 + ((q2 + ((p0 + q0 + 1) >> 1) - 2 * q1) >> 1).clamp(-tc0, tc0),
            );
        }
    }
}
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
                    for segment in 0..4 {
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
                        let bs = if cur.intra || other.intra {
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
                        };
                        if bs == 0 {
                            continue;
                        }
                        let x = mx * 16 + if vertical { edge * 4 } else { segment * 4 };
                        let y = my * 16 + if vertical { segment * 4 } else { edge * 4 };
                        for i in 0..4 {
                            let q = (y + if vertical { i } else { 0 }) * p.y_stride
                                + x
                                + if vertical { 0 } else { i };
                            pixel(
                                &mut p.y,
                                q,
                                if vertical { 1 } else { p.y_stride },
                                bs,
                                qp,
                                false,
                            );
                        }
                        if edge % 2 == 0 {
                            for plane in [&mut p.u, &mut p.v] {
                                for i in 0..2 {
                                    let q = (y / 2 + if vertical { i } else { 0 }) * p.c_stride
                                        + x / 2
                                        + if vertical { 0 } else { i };
                                    pixel(
                                        plane,
                                        q,
                                        if vertical { 1 } else { p.c_stride },
                                        bs,
                                        cqp,
                                        true,
                                    );
                                }
                            }
                        }
                    }
                }
            }
        }
    }
}
