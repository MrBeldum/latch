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
// Four independent samples of a weak luma edge. Strong intra edges use the scalar oracle.
#[cfg(target_arch = "aarch64")]
fn weak4(p: &mut [u8], q: usize, step: usize, along: usize, bs: usize, qp: usize) {
    use std::arch::aarch64::*;
    assert!(q >= 3 * step && q + 2 * step + 3 * along < p.len());
    unsafe {
        let load = |delta: isize| {
            let base = (q as isize + delta * step as isize) as usize;
            let v: [i16; 4] = std::array::from_fn(|i| p[base + i * along] as i16);
            vld1_s16(v.as_ptr())
        };
        let (p2, p1, p0, q0, q1, q2) = (load(-3), load(-2), load(-1), load(0), load(1), load(2));
        let beta = vdup_n_s16(BETA[qp] as i16);
        let mask = vand_u16(
            vclt_s16(vabd_s16(p0, q0), vdup_n_s16(ALPHA[qp] as i16)),
            vand_u16(
                vclt_s16(vabd_s16(p1, p0), beta),
                vclt_s16(vabd_s16(q1, q0), beta),
            ),
        );
        if vget_lane_u64::<0>(vreinterpret_u64_u16(mask)) == 0 {
            return;
        }
        let ap = vclt_s16(vabd_s16(p2, p0), beta);
        let aq = vclt_s16(vabd_s16(q2, q0), beta);
        let tc0 = vdup_n_s16(TC[qp][bs] as i16);
        let tc = vsub_s16(
            vsub_s16(tc0, vreinterpret_s16_u16(ap)),
            vreinterpret_s16_u16(aq),
        );
        let clip = |v, t| vmin_s16(vmax_s16(v, vneg_s16(t)), t);
        let delta = clip(
            vshr_n_s16::<3>(vadd_s16(
                vadd_s16(vshl_n_s16::<2>(vsub_s16(q0, p0)), vsub_s16(p1, q1)),
                vdup_n_s16(4),
            )),
            tc,
        );
        let avg = vrhadd_s16(p0, q0);
        let pp1 = vadd_s16(
            p1,
            clip(
                vshr_n_s16::<1>(vsub_s16(vadd_s16(p2, avg), vshl_n_s16::<1>(p1))),
                tc0,
            ),
        );
        let qq1 = vadd_s16(
            q1,
            clip(
                vshr_n_s16::<1>(vsub_s16(vadd_s16(q2, avg), vshl_n_s16::<1>(q1))),
                tc0,
            ),
        );
        let values = [
            vbsl_s16(vand_u16(mask, ap), pp1, p1),
            vbsl_s16(mask, vadd_s16(p0, delta), p0),
            vbsl_s16(mask, vsub_s16(q0, delta), q0),
            vbsl_s16(vand_u16(mask, aq), qq1, q1),
        ];
        for (i, value) in values.into_iter().enumerate() {
            let bytes = vqmovun_s16(vcombine_s16(value, value));
            let bytes = vget_lane_u32::<0>(vreinterpret_u32_u8(bytes)).to_le_bytes();
            let base = (q as isize + (i as isize - 2) * step as isize) as usize;
            for j in 0..4 {
                p[base + j * along] = bytes[j];
            }
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
            let empty = !cur.intra && cur.nz == [0; 16];
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
                    // Every segment has bS=0 on an empty internal edge, or between
                    // empty inter MBs with matching motion. This is the desktop skip path.
                    if empty
                        && (edge > 0
                            || (!other.intra
                                && other.nz == [0; 16]
                                && (cur.mv.0 - other.mv.0).abs() < 4
                                && (cur.mv.1 - other.mv.1).abs() < 4))
                    {
                        continue;
                    }
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
                        #[cfg(target_arch = "aarch64")]
                        let simd = bs < 4;
                        #[cfg(not(target_arch = "aarch64"))]
                        let simd = false;
                        #[cfg(target_arch = "aarch64")]
                        if simd {
                            weak4(
                                &mut p.y,
                                y * p.y_stride + x,
                                if vertical { 1 } else { p.y_stride },
                                if vertical { p.y_stride } else { 1 },
                                bs,
                                qp,
                            );
                        }
                        if !simd {
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

#[cfg(all(test, target_arch = "aarch64"))]
mod tests {
    use super::*;
    #[test]
    fn weak_edges_match_scalar() {
        let mut seed = 17u32;
        for qp in 0..52 {
            for bs in 1..4 {
                for vertical in [false, true] {
                    for range in [8, 32, 256] {
                        for _ in 0..20 {
                            let mut a: Vec<_> = (0..16 * 16)
                                .map(|_| {
                                    seed = seed.wrapping_mul(1664525).wrapping_add(1013904223);
                                    (128 + (seed >> 16) % range - range / 2) as u8
                                })
                                .collect();
                            let mut b = a.clone();
                            let (step, along) = if vertical { (1, 16) } else { (16, 1) };
                            let q = 4 * 16 + 4;
                            weak4(&mut a, q, step, along, bs, qp);
                            for i in 0..4 {
                                pixel(&mut b, q + i * along, step, bs, qp, false);
                            }
                            assert_eq!(a, b, "qp={qp}, bs={bs}, vertical={vertical}");
                        }
                    }
                }
            }
        }
    }
}
