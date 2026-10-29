//! Inter prediction for P slices: quarter-pel luma interpolation via
//! the 6-tap filter (spec 8.4.2.2.1, Tables 8-11/8-12), eighth-pel
//! chroma bilinear (8.4.2.2.2), and the weighted-prediction stage
//! (8.4.3).
//!
//! All interpolation is pure over the reference plane plus the motion
//! vector; out-of-picture fetches clamp to the reference border per
//! spec 8.4.2.2.1's `Clip3` on the integer taps.

/// Clamped integer sample fetch from a `width`×`height` plane.
fn px(ref_: &[u8], w: usize, h: usize, x: i32, y: i32) -> i32 {
    let xc = x.clamp(0, w as i32 - 1) as usize;
    let yc = y.clamp(0, h as i32 - 1) as usize;
    i32::from(ref_[yc * w + xc])
}

fn clip8(v: i32) -> u8 {
    v.clamp(0, 255) as u8
}

/// Horizontal 6-tap intermediate (unclipped `b1`-style value) centered
/// at half position (x + ½, y): taps integer columns x−2..x+3.
fn h_tap(ref_: &[u8], w: usize, h: usize, x: i32, y: i32) -> i32 {
    if x >= 2 && x + 3 < w as i32 {
        let yy = y.clamp(0, h as i32 - 1) as usize;
        let start = yy * w + x as usize - 2;
        let p = &ref_[start..start + 6];
        return i32::from(p[0]) - 5 * i32::from(p[1]) + 20 * i32::from(p[2]) + 20 * i32::from(p[3])
            - 5 * i32::from(p[4])
            + i32::from(p[5]);
    }
    px(ref_, w, h, x - 2, y) - 5 * px(ref_, w, h, x - 1, y)
        + 20 * px(ref_, w, h, x, y)
        + 20 * px(ref_, w, h, x + 1, y)
        - 5 * px(ref_, w, h, x + 2, y)
        + px(ref_, w, h, x + 3, y)
}

/// Vertical 6-tap intermediate (unclipped `h1`-style value) centered
/// at half position (x, y + ½): taps integer rows y−2..y+3.
fn v_tap(ref_: &[u8], w: usize, h: usize, x: i32, y: i32) -> i32 {
    if y >= 2 && y + 3 < h as i32 {
        let xx = x.clamp(0, w as i32 - 1) as usize;
        let start = (y as usize - 2) * w + xx;
        return i32::from(ref_[start]) - 5 * i32::from(ref_[start + w])
            + 20 * i32::from(ref_[start + 2 * w])
            + 20 * i32::from(ref_[start + 3 * w])
            - 5 * i32::from(ref_[start + 4 * w])
            + i32::from(ref_[start + 5 * w]);
    }
    px(ref_, w, h, x, y - 2) - 5 * px(ref_, w, h, x, y - 1)
        + 20 * px(ref_, w, h, x, y)
        + 20 * px(ref_, w, h, x, y + 1)
        - 5 * px(ref_, w, h, x, y + 2)
        + px(ref_, w, h, x, y + 3)
}

/// Half-pel b (horizontal half at (x+½, y)).
fn half_b(ref_: &[u8], w: usize, h: usize, x: i32, y: i32) -> i32 {
    i32::from(clip8((h_tap(ref_, w, h, x, y) + 16) >> 5))
}

/// Half-pel h (vertical half at (x, y+½)).
fn half_h(ref_: &[u8], w: usize, h: usize, x: i32, y: i32) -> i32 {
    i32::from(clip8((v_tap(ref_, w, h, x, y) + 16) >> 5))
}

/// Center half-pel j (x+½, y+½): vertical filter over the
/// unclipped horizontal intermediates of the same column (spec 8-246).
fn half_j(ref_: &[u8], w: usize, h: usize, x: i32, y: i32) -> i32 {
    let j1 = h_tap(ref_, w, h, x, y - 2) - 5 * h_tap(ref_, w, h, x, y - 1)
        + 20 * h_tap(ref_, w, h, x, y)
        + 20 * h_tap(ref_, w, h, x, y + 1)
        - 5 * h_tap(ref_, w, h, x, y + 2)
        + h_tap(ref_, w, h, x, y + 3);
    i32::from(clip8((j1 + 512) >> 10))
}

/// One luma prediction sample at integer location `(x, y)` + fraction
/// `(fx, fy)` in quarter-pel units (spec Table 8-12).
fn luma_sample(ref_: &[u8], w: usize, h: usize, x: i32, y: i32, fx: i32, fy: i32) -> u8 {
    let (g, hh, mm) = (
        px(ref_, w, h, x, y),
        px(ref_, w, h, x + 1, y),
        px(ref_, w, h, x, y + 1),
    );

    match (fx, fy) {
        (0, 0) => clip8(g),
        (1, 0) => clip8((g + half_b(ref_, w, h, x, y) + 1) >> 1),
        (2, 0) => clip8(half_b(ref_, w, h, x, y)),
        (3, 0) => clip8((hh + half_b(ref_, w, h, x, y) + 1) >> 1),
        (0, 1) => clip8((g + half_h(ref_, w, h, x, y) + 1) >> 1),
        (0, 2) => clip8(half_h(ref_, w, h, x, y)),
        (0, 3) => clip8((mm + half_h(ref_, w, h, x, y) + 1) >> 1),
        (1, 1) => clip8((half_b(ref_, w, h, x, y) + half_h(ref_, w, h, x, y) + 1) >> 1),
        (1, 2) => clip8((half_h(ref_, w, h, x, y) + half_j(ref_, w, h, x, y) + 1) >> 1),
        (1, 3) => clip8((half_h(ref_, w, h, x, y) + half_b(ref_, w, h, x, y + 1) + 1) >> 1),
        (2, 1) => clip8((half_b(ref_, w, h, x, y) + half_j(ref_, w, h, x, y) + 1) >> 1),
        (2, 2) => clip8(half_j(ref_, w, h, x, y)),
        (2, 3) => clip8((half_j(ref_, w, h, x, y) + half_b(ref_, w, h, x, y + 1) + 1) >> 1),
        (3, 1) => clip8((half_b(ref_, w, h, x, y) + half_h(ref_, w, h, x + 1, y) + 1) >> 1),
        (3, 2) => clip8((half_j(ref_, w, h, x, y) + half_h(ref_, w, h, x + 1, y) + 1) >> 1),
        (3, 3) => clip8((half_h(ref_, w, h, x + 1, y) + half_b(ref_, w, h, x, y + 1) + 1) >> 1),
        _ => unreachable!("quarter-pel fraction out of range"),
    }
}

/// One chroma prediction sample (spec 8.4.2.2.2): bilinear with
/// eighth-pel weights, rounding constant 32 >> 6.
fn chroma_sample(ref_: &[u8], w: usize, h: usize, x: i32, y: i32, fx: i32, fy: i32) -> u8 {
    let a = px(ref_, w, h, x, y);
    let b = px(ref_, w, h, x + 1, y);
    let c = px(ref_, w, h, x, y + 1);
    let d = px(ref_, w, h, x + 1, y + 1);
    let v =
        ((8 - fx) * (8 - fy) * a + fx * (8 - fy) * b + (8 - fx) * fy * c + fx * fy * d + 32) >> 6;
    clip8(v)
}

/// Motion-compensates one luma partition of `pw`×`ph` samples whose
/// top-left corner sits at `(px0, py0)` in picture coordinates and whose
/// quarter-pel motion vector is `mv`. Writes into `dst` (row-major,
/// `pw` stride = `pw`).
#[allow(clippy::too_many_arguments)]
pub(crate) fn mc_luma(
    ref_: &[u8],
    rw: usize,
    rh: usize,
    px0: i32,
    py0: i32,
    mv: (i32, i32),
    pw: usize,
    ph: usize,
    dst: &mut [u8],
) {
    if mv.0 & 3 == 0 && mv.1 & 3 == 0 {
        let x = px0 + (mv.0 >> 2);
        let y = py0 + (mv.1 >> 2);
        if x >= 0 && y >= 0 && x as usize + pw <= rw && y as usize + ph <= rh {
            for row in 0..ph {
                let start = (y as usize + row) * rw + x as usize;
                dst[row * pw..(row + 1) * pw].copy_from_slice(&ref_[start..start + pw]);
            }
            return;
        }
    }
    let fx = mv.0 & 3;
    let fy = mv.1 & 3;
    if (fx == 2 && fy != 0) || (fy == 2 && fx != 0) {
        let ix = px0 + (mv.0 >> 2);
        let iy = py0 + (mv.1 >> 2);
        let mut horizontal = [0i32; 16 * 21];
        for row in 0..ph + 5 {
            for col in 0..pw {
                horizontal[row * 16 + col] =
                    h_tap(ref_, rw, rh, ix + col as i32, iy + row as i32 - 2);
            }
        }
        for row in 0..ph {
            for col in 0..pw {
                let off = row * 16 + col;
                let j = i32::from(clip8(
                    (horizontal[off] - 5 * horizontal[off + 16]
                        + 20 * horizontal[off + 32]
                        + 20 * horizontal[off + 48]
                        - 5 * horizontal[off + 64]
                        + horizontal[off + 80]
                        + 512)
                        >> 10,
                ));
                let (x, y) = (ix + col as i32, iy + row as i32);
                let value = match (fx, fy) {
                    (2, 2) => j,
                    (2, 1) => (j + i32::from(clip8((horizontal[off + 32] + 16) >> 5)) + 1) >> 1,
                    (2, 3) => (j + i32::from(clip8((horizontal[off + 48] + 16) >> 5)) + 1) >> 1,
                    (1, 2) => (j + half_h(ref_, rw, rh, x, y) + 1) >> 1,
                    (3, 2) => (j + half_h(ref_, rw, rh, x + 1, y) + 1) >> 1,
                    _ => j,
                };
                dst[row * pw + col] = value as u8;
            }
        }
        return;
    }
    let mvx = mv.0;
    let mvy = mv.1;
    for y in 0..ph {
        for x in 0..pw {
            // Full-sample location + fraction (spec 8.4.2.2.1):
            // xIntL = xAL + (mvx >> 2), xFracL = mvx & 3.
            let ax = px0 + x as i32;
            let ay = py0 + y as i32;
            let ix = ax + (mvx >> 2);
            let iy = ay + (mvy >> 2);
            let fx = mvx & 3;
            let fy = mvy & 3;
            dst[y * pw + x] = luma_sample(ref_, rw, rh, ix, iy, fx, fy);
        }
    }
}

/// Chroma counterpart of [`mc_luma`]: `mv` is the *luma* vector; the
/// chroma vector divides by 2 for 4:2:0 (spec 8.4.2.1, mvCLX = mvLX/2
/// both components since MvVerticalChromA = mvCLX / 2 truncated — for
/// frame pictures the chroma vector is simply the luma vector halved,
/// then decomposed into eighth-pel units).
#[allow(clippy::too_many_arguments)]
pub(crate) fn mc_chroma(
    ref_: &[u8],
    rw: usize,
    rh: usize,
    px0: i32,
    py0: i32,
    mv: (i32, i32),
    pw: usize,
    ph: usize,
    dst: &mut [u8],
) {
    if mv.0 & 7 == 0 && mv.1 & 7 == 0 {
        let x = px0 + (mv.0 >> 3);
        let y = py0 + (mv.1 >> 3);
        if x >= 0 && y >= 0 && x as usize + pw <= rw && y as usize + ph <= rh {
            for row in 0..ph {
                let start = (y as usize + row) * rw + x as usize;
                dst[row * pw..(row + 1) * pw].copy_from_slice(&ref_[start..start + pw]);
            }
            return;
        }
    }
    // mvCX = mvLX / 2 (spec 8.4.2.1: chroma vectors from luma vectors,
    // xFracC = mvCX & 7 gives eighth-pel fractions).
    for y in 0..ph {
        for x in 0..pw {
            let ax = px0 + x as i32;
            let ay = py0 + y as i32;
            // A luma quarter-pel displacement maps 1:1 onto chroma
            // eighth-pel units (spec 8.4.2.1).
            let ix = ax + (mv.0 >> 3);
            let iy = ay + (mv.1 >> 3);
            let fx = mv.0 & 7;
            let fy = mv.1 & 7;
            dst[y * pw + x] = chroma_sample(ref_, rw, rh, ix, iy, fx, fy);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn cached_motion_matches_per_sample_scalar_at_every_fraction_and_edge() {
        let plane = (0..32 * 24)
            .map(|i| ((i * 37 + i / 11) % 256) as u8)
            .collect::<Vec<_>>();
        for x in [-9, -1, 0, 4, 17, 31] {
            for y in [-8, -1, 0, 3, 15, 23] {
                for fy in 0..4 {
                    for fx in 0..4 {
                        let mut actual = [0; 256];
                        mc_luma(&plane, 32, 24, x, y, (fx - 8, fy - 8), 16, 16, &mut actual);
                        for row in 0..16 {
                            for col in 0..16 {
                                assert_eq!(
                                    actual[row * 16 + col],
                                    luma_sample(
                                        &plane,
                                        32,
                                        24,
                                        x + col as i32 - 2,
                                        y + row as i32 - 2,
                                        fx,
                                        fy
                                    )
                                );
                            }
                        }
                    }
                }
                for fy in 0..8 {
                    for fx in 0..8 {
                        let mut actual = [0; 64];
                        mc_chroma(&plane, 32, 24, x, y, (fx - 16, fy - 16), 8, 8, &mut actual);
                        for row in 0..8 {
                            for col in 0..8 {
                                assert_eq!(
                                    actual[row * 8 + col],
                                    chroma_sample(
                                        &plane,
                                        32,
                                        24,
                                        x + col as i32 - 2,
                                        y + row as i32 - 2,
                                        fx,
                                        fy
                                    )
                                );
                            }
                        }
                    }
                }
            }
        }
    }
}
