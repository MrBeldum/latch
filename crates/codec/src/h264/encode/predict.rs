use super::transform::satd;

#[allow(clippy::too_many_arguments)]
pub(super) fn intra4(
    src: &[u8],
    ss: usize,
    so: usize,
    rec: &[u8],
    stride: usize,
    x: usize,
    y: usize,
    top_right: bool,
    predicted: u8,
    lambda: u32,
) -> (u8, [u8; 16], u32) {
    let mut t = [128i32; 9];
    let mut l = [128i32; 5];
    if y > 0 {
        for i in 0..4 {
            t[i + 1] = rec[(y - 1) * stride + x + i] as i32;
        }
        for i in 4..8 {
            t[i + 1] = if top_right && x + i < stride {
                rec[(y - 1) * stride + x + i] as i32
            } else {
                t[4]
            };
        }
    }
    if x > 0 {
        for i in 0..4 {
            l[i + 1] = rec[(y + i) * stride + x - 1] as i32;
        }
    }
    if x > 0 && y > 0 {
        t[0] = rec[(y - 1) * stride + x - 1] as i32;
        l[0] = t[0];
    }
    let mut candidates = [(0u32, 2u8, [0u8; 16], 0u32); 9];
    let mut count = 0;
    for mode in 0..9 {
        if (matches!(mode, 0 | 3 | 7) && y == 0)
            || (matches!(mode, 1 | 8) && x == 0)
            || (matches!(mode, 4..=6) && (x == 0 || y == 0))
        {
            continue;
        }
        let mut p = [0u8; 16];
        let dc = match (x > 0, y > 0) {
            (true, true) => (l[1..5].iter().sum::<i32>() + t[1..5].iter().sum::<i32>() + 4) >> 3,
            (true, false) => (l[1..5].iter().sum::<i32>() + 2) >> 2,
            (false, true) => (t[1..5].iter().sum::<i32>() + 2) >> 2,
            _ => 128,
        };
        let vr = |x: usize, y: usize, t: &[i32; 9], l: &[i32; 5]| -> i32 {
            let z = 2 * x as i32 - y as i32;
            if z >= 0 {
                let i = x - (y >> 1);
                if z & 1 == 0 {
                    (t[i] + t[i + 1] + 1) >> 1
                } else {
                    (t[i - 1] + 2 * t[i] + t[i + 1] + 2) >> 2
                }
            } else if z == -1 {
                (l[1] + 2 * t[0] + t[1] + 2) >> 2
            } else {
                (l[y] + 2 * l[y - 1] + l[y - 2] + 2) >> 2
            }
        };
        for yy in 0..4 {
            for xx in 0..4 {
                let v = match mode {
                    0 => t[xx + 1],
                    1 => l[yy + 1],
                    2 => dc,
                    3 => {
                        let i = xx + yy + 1;
                        (t[i] + 2 * t[i + 1] + t[(i + 2).min(8)] + 2) >> 2
                    }
                    4 => {
                        if xx > yy {
                            let i = xx - yy;
                            (t[i - 1] + 2 * t[i] + t[i + 1] + 2) >> 2
                        } else if yy > xx {
                            let i = yy - xx;
                            (l[i - 1] + 2 * l[i] + l[i + 1] + 2) >> 2
                        } else {
                            (t[1] + 2 * t[0] + l[1] + 2) >> 2
                        }
                    }
                    5 => vr(xx, yy, &t, &l),
                    6 => {
                        let mut tl = [0; 9];
                        tl[..5].copy_from_slice(&l);
                        let mut lt = [0; 5];
                        lt.copy_from_slice(&t[..5]);
                        vr(yy, xx, &tl, &lt)
                    }
                    7 => {
                        let i = xx + (yy >> 1) + 1;
                        if yy & 1 == 0 {
                            (t[i] + t[i + 1] + 1) >> 1
                        } else {
                            (t[i] + 2 * t[i + 1] + t[i + 2] + 2) >> 2
                        }
                    }
                    _ => {
                        let z = xx + 2 * yy;
                        let i = yy + (xx >> 1) + 1;
                        if z <= 4 && z & 1 == 0 {
                            (l[i] + l[i + 1] + 1) >> 1
                        } else if z < 5 {
                            (l[i] + 2 * l[i + 1] + l[i + 2] + 2) >> 2
                        } else if z == 5 {
                            (l[3] + 3 * l[4] + 2) >> 2
                        } else {
                            l[4]
                        }
                    }
                };
                p[yy * 4 + xx] = v as u8;
            }
        }
        let rate = lambda * if mode == predicted { 1 } else { 4 };
        let sad = (0..4)
            .flat_map(|yy| (0..4).map(move |xx| (yy, xx)))
            .step_by(2)
            .map(|(yy, xx)| src[so + yy * ss + xx].abs_diff(p[yy * 4 + xx]) as u32)
            .sum::<u32>()
            * 2
            + rate;
        candidates[count] = (sad, mode, p, rate);
        count += 1;
    }
    candidates[..count].sort_unstable_by_key(|v| v.0);
    candidates[..count]
        .iter()
        .copied()
        .take(3)
        .map(|(_, m, p, rate)| {
            let cost = satd(src, ss, so, &p) + rate;
            (m, p, cost)
        })
        .min_by_key(|v| v.2)
        .unwrap()
}

pub(super) fn intra_plane(
    rec: &[u8],
    stride: usize,
    x: usize,
    y: usize,
    n: usize,
    mode: u8,
    chroma: bool,
) -> [u8; 256] {
    let mut t = [128i32; 17];
    let mut l = t;
    if y > 0 {
        for i in 0..n {
            t[i + 1] = rec[(y - 1) * stride + x + i] as i32;
        }
    }
    if x > 0 {
        for i in 0..n {
            l[i + 1] = rec[(y + i) * stride + x - 1] as i32;
        }
    }
    if x > 0 && y > 0 {
        t[0] = rec[(y - 1) * stride + x - 1] as i32;
        l[0] = t[0];
    }
    let dc = match (x > 0, y > 0) {
        (true, true) => {
            (t[1..=n].iter().sum::<i32>() + l[1..=n].iter().sum::<i32>() + n as i32)
                / (2 * n) as i32
        }
        (true, false) => (l[1..=n].iter().sum::<i32>() + (n / 2) as i32) / n as i32,
        (false, true) => (t[1..=n].iter().sum::<i32>() + (n / 2) as i32) / n as i32,
        _ => 128,
    };
    let mut h = 0;
    let mut v = 0;
    for i in 1..=n / 2 {
        h += i as i32 * (t[n / 2 + i] - t[n / 2 - i]);
        v += i as i32 * (l[n / 2 + i] - l[n / 2 - i]);
    }
    let b = if n == 16 {
        (5 * h + 32) >> 6
    } else {
        (17 * h + 16) >> 5
    };
    let c = if n == 16 {
        (5 * v + 32) >> 6
    } else {
        (17 * v + 16) >> 5
    };
    let a = 16 * (t[n] + l[n]);
    let mut p = [0; 256];
    for yy in 0..n {
        for xx in 0..n {
            let val = match mode {
                0 if !chroma => t[xx + 1],
                1 if !chroma => l[yy + 1],
                1 if chroma => l[yy + 1],
                2 if chroma => t[xx + 1],
                3 => {
                    (a + b * (xx as i32 - (n / 2 - 1) as i32)
                        + c * (yy as i32 - (n / 2 - 1) as i32)
                        + 16)
                        >> 5
                }
                _ if chroma => {
                    let bx = xx / 4;
                    let by = yy / 4;
                    let ts = t[bx * 4 + 1..bx * 4 + 5].iter().sum::<i32>();
                    let ls = l[by * 4 + 1..by * 4 + 5].iter().sum::<i32>();
                    match (x > 0, y > 0) {
                        (true, true) if bx == by => (ts + ls + 4) >> 3,
                        (true, true) if by == 0 => (ts + 2) >> 2,
                        (true, true) => (ls + 2) >> 2,
                        (true, false) => (ls + 2) >> 2,
                        (false, true) => (ts + 2) >> 2,
                        _ => 128,
                    }
                }
                _ => dc,
            };
            p[yy * n + xx] = val.clamp(0, 255) as u8;
        }
    }
    p
}

pub(super) fn sample(
    plane: &[u8],
    stride: usize,
    width: usize,
    height: usize,
    x: i32,
    y: i32,
) -> i32 {
    plane[y.clamp(0, height as i32 - 1) as usize * stride + x.clamp(0, width as i32 - 1) as usize]
        as i32
}
pub(super) fn chroma_mc(
    plane: &[u8],
    stride: usize,
    width: usize,
    height: usize,
    x: i32,
    y: i32,
    mv: (i32, i32),
) -> u8 {
    let sx = x + mv.0.div_euclid(8);
    let sy = y + mv.1.div_euclid(8);
    let dx = mv.0.rem_euclid(8);
    let dy = mv.1.rem_euclid(8);
    let a = sample(plane, stride, width, height, sx, sy);
    let b = sample(plane, stride, width, height, sx + 1, sy);
    let c = sample(plane, stride, width, height, sx, sy + 1);
    let d = sample(plane, stride, width, height, sx + 1, sy + 1);
    (((8 - dx) * (8 - dy) * a + dx * (8 - dy) * b + (8 - dx) * dy * c + dx * dy * d + 32) >> 6)
        as u8
}

#[allow(clippy::too_many_arguments)]
pub(super) fn chroma8x8(
    dst: &mut [u8],
    ds: usize,
    offset: usize,
    src: &[u8],
    ss: usize,
    so: usize,
    dx: u16,
    dy: u16,
) {
    assert!(offset + 7 * ds + 8 <= dst.len() && so + 8 * ss + 9 <= src.len());
    #[cfg(target_arch = "aarch64")]
    unsafe {
        use std::arch::aarch64::*;
        let weights = [(8 - dx) * (8 - dy), dx * (8 - dy), (8 - dx) * dy, dx * dy];
        for y in 0..8 {
            let p = src.as_ptr().add(so + y * ss);
            let a = vmovl_u8(vld1_u8(p));
            let b = vmovl_u8(vld1_u8(p.add(1)));
            let c = vmovl_u8(vld1_u8(p.add(ss)));
            let d = vmovl_u8(vld1_u8(p.add(ss + 1)));
            let sum = vmlaq_n_u16(
                vmlaq_n_u16(
                    vmlaq_n_u16(vmulq_n_u16(a, weights[0]), b, weights[1]),
                    c,
                    weights[2],
                ),
                d,
                weights[3],
            );
            vst1_u8(
                dst.as_mut_ptr().add(offset + y * ds),
                vrshrn_n_u16::<6>(sum),
            );
        }
    }
    #[cfg(not(target_arch = "aarch64"))]
    chroma8x8_scalar(dst, ds, offset, src, ss, so, dx, dy);
}
#[allow(clippy::too_many_arguments)]
#[cfg_attr(not(test), allow(dead_code))]
fn chroma8x8_scalar(
    dst: &mut [u8],
    ds: usize,
    offset: usize,
    src: &[u8],
    ss: usize,
    so: usize,
    dx: u16,
    dy: u16,
) {
    let weights = [(8 - dx) * (8 - dy), dx * (8 - dy), (8 - dx) * dy, dx * dy];
    for y in 0..8 {
        for x in 0..8 {
            let i = so + y * ss + x;
            dst[offset + y * ds + x] = ((src[i] as u16 * weights[0]
                + src[i + 1] as u16 * weights[1]
                + src[i + ss] as u16 * weights[2]
                + src[i + ss + 1] as u16 * weights[3]
                + 32)
                >> 6) as u8;
        }
    }
}

/// Six-tap half samples retain their precision until the diagonal pass.
#[allow(clippy::too_many_arguments)]
pub(super) fn luma_mc(
    plane: &[u8],
    stride: usize,
    width: usize,
    height: usize,
    x: i32,
    y: i32,
    mv: (i32, i32),
) -> u8 {
    let x = x + mv.0.div_euclid(4);
    let y = y + mv.1.div_euclid(4);
    let fx = mv.0.rem_euclid(4);
    let fy = mv.1.rem_euclid(4);
    let s = |x, y| sample(plane, stride, width, height, x, y);
    if fx == 0 && fy == 0 {
        return s(x, y) as u8;
    }
    let tap = |v: [i32; 6]| v[0] - 5 * v[1] + 20 * v[2] + 20 * v[3] - 5 * v[4] + v[5];
    let raw_h = |x, y| tap(std::array::from_fn(|i| s(x + i as i32 - 2, y)));
    let h = |x, y| ((raw_h(x, y) + 16) >> 5).clamp(0, 255);
    let v = |x, y| ((tap(std::array::from_fn(|i| s(x, y + i as i32 - 2))) + 16) >> 5).clamp(0, 255);
    let d =
        || ((tap(std::array::from_fn(|i| raw_h(x, y + i as i32 - 2))) + 512) >> 10).clamp(0, 255);
    let avg = |a, b| (a + b + 1) >> 1;
    (match (fx, fy) {
        (2, 0) => h(x, y),
        (0, 2) => v(x, y),
        (2, 2) => d(),
        (1, 0) => avg(s(x, y), h(x, y)),
        (3, 0) => avg(h(x, y), s(x + 1, y)),
        (0, 1) => avg(s(x, y), v(x, y)),
        (0, 3) => avg(v(x, y), s(x, y + 1)),
        (2, 1) => avg(h(x, y), d()),
        (2, 3) => avg(d(), h(x, y + 1)),
        (1, 2) => avg(v(x, y), d()),
        (3, 2) => avg(d(), v(x + 1, y)),
        _ => avg(h(x, y + fy / 2), v(x + fx / 2, y)),
    }) as u8
}

#[inline(always)]
pub(super) fn sad16x8(
    src: &[u8],
    src_stride: usize,
    src_offset: usize,
    ref_plane: &[u8],
    ref_stride: usize,
    ref_offset: usize,
) -> u32 {
    #[cfg(target_arch = "aarch64")]
    {
        use std::arch::aarch64::*;
        unsafe {
            let mut acc = vdupq_n_u16(0);
            for i in 0..8 {
                let so = src_offset + (i * 2) * src_stride;
                let ro = ref_offset + (i * 2) * ref_stride;
                let va = vld1q_u8(src.as_ptr().add(so));
                let vb = vld1q_u8(ref_plane.as_ptr().add(ro));
                let diff_l = vabdl_u8(vget_low_u8(va), vget_low_u8(vb));
                let diff_h = vabdl_high_u8(va, vb);
                acc = vaddq_u16(acc, vaddq_u16(diff_l, diff_h));
            }
            vaddlvq_u16(acc) as u32
        }
    }
    #[cfg(target_arch = "x86_64")]
    {
        use std::arch::x86_64::*;
        unsafe {
            let mut acc = _mm_setzero_si128();
            for i in 0..8 {
                let so = src_offset + (i * 2) * src_stride;
                let ro = ref_offset + (i * 2) * ref_stride;
                let va = _mm_loadu_si128(src.as_ptr().add(so) as *const __m128i);
                let vb = _mm_loadu_si128(ref_plane.as_ptr().add(ro) as *const __m128i);
                acc = _mm_add_epi64(acc, _mm_sad_epu8(va, vb));
            }
            let lo = _mm_cvtsi128_si32(acc) as u32;
            let hi = _mm_cvtsi128_si32(_mm_srli_si128(acc, 8)) as u32;
            lo + hi
        }
    }
    #[cfg(not(any(target_arch = "aarch64", target_arch = "x86_64")))]
    {
        sad16x8_scalar(
            src, src_stride, src_offset, ref_plane, ref_stride, ref_offset,
        )
    }
}

#[inline(always)]
#[allow(dead_code)]
pub(super) fn sad16x8_scalar(
    src: &[u8],
    src_stride: usize,
    src_offset: usize,
    ref_plane: &[u8],
    ref_stride: usize,
    ref_offset: usize,
) -> u32 {
    let mut sum = 0u32;
    for i in 0..8 {
        let so = src_offset + (i * 2) * src_stride;
        let ro = ref_offset + (i * 2) * ref_stride;
        for j in 0..16 {
            sum += src[so + j].abs_diff(ref_plane[ro + j]) as u32;
        }
    }
    sum
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HalfPelPlanes {
    pub h: Vec<u8>,
    pub v: Vec<u8>,
    pub d: Vec<u8>,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(super) enum SubpelPlane {
    Int,
    H,
    V,
    D,
}

pub(super) fn subpel_sources(
    fx: i32,
    fy: i32,
) -> (
    SubpelPlane,
    usize,
    usize,
    Option<(SubpelPlane, usize, usize)>,
) {
    match (fx, fy) {
        (0, 0) => (SubpelPlane::Int, 0, 0, None),
        (2, 0) => (SubpelPlane::H, 0, 0, None),
        (0, 2) => (SubpelPlane::V, 0, 0, None),
        (2, 2) => (SubpelPlane::D, 0, 0, None),
        (1, 0) => (SubpelPlane::Int, 0, 0, Some((SubpelPlane::H, 0, 0))),
        (3, 0) => (SubpelPlane::H, 0, 0, Some((SubpelPlane::Int, 1, 0))),
        (0, 1) => (SubpelPlane::Int, 0, 0, Some((SubpelPlane::V, 0, 0))),
        (0, 3) => (SubpelPlane::V, 0, 0, Some((SubpelPlane::Int, 0, 1))),
        (2, 1) => (SubpelPlane::H, 0, 0, Some((SubpelPlane::D, 0, 0))),
        (2, 3) => (SubpelPlane::D, 0, 0, Some((SubpelPlane::H, 0, 1))),
        (1, 2) => (SubpelPlane::V, 0, 0, Some((SubpelPlane::D, 0, 0))),
        (3, 2) => (SubpelPlane::D, 0, 0, Some((SubpelPlane::V, 1, 0))),
        (1, 1) => (SubpelPlane::H, 0, 0, Some((SubpelPlane::V, 0, 0))),
        (3, 1) => (SubpelPlane::H, 0, 0, Some((SubpelPlane::V, 1, 0))),
        (1, 3) => (SubpelPlane::H, 0, 1, Some((SubpelPlane::V, 0, 0))),
        (3, 3) => (SubpelPlane::H, 0, 1, Some((SubpelPlane::V, 1, 0))),
        _ => unreachable!(),
    }
}

pub fn compute_half_pel(plane: &[u8], stride: usize, width: usize, height: usize) -> HalfPelPlanes {
    compute_half_pel_impl::<true>(plane, stride, width, height)
}
fn compute_half_pel_impl<const SIMD: bool>(
    plane: &[u8],
    stride: usize,
    width: usize,
    height: usize,
) -> HalfPelPlanes {
    let mut h = vec![0u8; stride * height];
    let mut v = vec![0u8; stride * height];
    let mut d = vec![0u8; stride * height];
    let mut raw_h = vec![0i16; stride * height];

    let tap = |v0: i32, v1: i32, v2: i32, v3: i32, v4: i32, v5: i32| {
        v0 - 5 * v1 + 20 * v2 + 20 * v3 - 5 * v4 + v5
    };

    for y in 0..height {
        let y_off = y * stride;
        for x in 0..2.min(width) {
            let s = |dx: i32| -> i32 {
                let cx = (x as i32 + dx).clamp(0, width as i32 - 1) as usize;
                plane[y_off + cx] as i32
            };
            let rh = tap(s(-2), s(-1), s(0), s(1), s(2), s(3));
            raw_h[y_off + x] = rh as i16;
            h[y_off + x] = ((rh + 16) >> 5).clamp(0, 255) as u8;
        }
        let row = &plane[y_off..y_off + width];
        let mid_end = width.saturating_sub(3);
        #[allow(unused_mut)] // Mutable only in the NEON path.
        let mut middle = 2;
        #[cfg(target_arch = "aarch64")]
        if SIMD {
            while middle + 8 <= mid_end {
                // Six eight-byte loads stay inside this row (including the final +3 tap).
                unsafe {
                    use std::arch::aarch64::*;
                    let rh = half_tap_neon(std::array::from_fn(|i| {
                        vreinterpretq_s16_u16(vmovl_u8(vld1_u8(row.as_ptr().add(middle + i - 2))))
                    }));
                    vst1q_s16(raw_h.as_mut_ptr().add(y_off + middle), rh);
                    vst1_u8(h.as_mut_ptr().add(y_off + middle), vqrshrun_n_s16::<5>(rh));
                }
                middle += 8;
            }
        }
        for x in middle..mid_end {
            let rh = tap(
                row[x - 2] as i32,
                row[x - 1] as i32,
                row[x] as i32,
                row[x + 1] as i32,
                row[x + 2] as i32,
                row[x + 3] as i32,
            );
            raw_h[y_off + x] = rh as i16;
            h[y_off + x] = ((rh + 16) >> 5).clamp(0, 255) as u8;
        }
        for x in mid_end.max(2)..width {
            let s = |dx: i32| -> i32 {
                let cx = (x as i32 + dx).clamp(0, width as i32 - 1) as usize;
                plane[y_off + cx] as i32
            };
            let rh = tap(s(-2), s(-1), s(0), s(1), s(2), s(3));
            raw_h[y_off + x] = rh as i16;
            h[y_off + x] = ((rh + 16) >> 5).clamp(0, 255) as u8;
        }
    }

    for y in 0..height {
        let y_off = y * stride;
        let y0 = (y as i32 - 2).clamp(0, height as i32 - 1) as usize * stride;
        let y1 = (y as i32 - 1).clamp(0, height as i32 - 1) as usize * stride;
        let y2 = y * stride;
        let y3 = (y as i32 + 1).clamp(0, height as i32 - 1) as usize * stride;
        let y4 = (y as i32 + 2).clamp(0, height as i32 - 1) as usize * stride;
        let y5 = (y as i32 + 3).clamp(0, height as i32 - 1) as usize * stride;

        let r0 = &plane[y0..y0 + width];
        let r1 = &plane[y1..y1 + width];
        let r2 = &plane[y2..y2 + width];
        let r3 = &plane[y3..y3 + width];
        let r4 = &plane[y4..y4 + width];
        let r5 = &plane[y5..y5 + width];

        let rh0 = &raw_h[y0..y0 + width];
        let rh1 = &raw_h[y1..y1 + width];
        let rh2 = &raw_h[y2..y2 + width];
        let rh3 = &raw_h[y3..y3 + width];
        let rh4 = &raw_h[y4..y4 + width];
        let rh5 = &raw_h[y5..y5 + width];

        let v_row = &mut v[y_off..y_off + width];
        let d_row = &mut d[y_off..y_off + width];

        #[allow(unused_mut)] // Mutable only in the NEON path.
        let mut middle = 0;
        #[cfg(target_arch = "aarch64")]
        if SIMD {
            while middle + 8 <= width {
                unsafe {
                    use std::arch::aarch64::*;
                    let rv =
                        half_tap_neon([r0, r1, r2, r3, r4, r5].map(|r| {
                            vreinterpretq_s16_u16(vmovl_u8(vld1_u8(r.as_ptr().add(middle))))
                        }));
                    vst1_u8(v_row.as_mut_ptr().add(middle), vqrshrun_n_s16::<5>(rv));
                    let rows =
                        [rh0, rh1, rh2, rh3, rh4, rh5].map(|r| vld1q_s16(r.as_ptr().add(middle)));
                    let lo = diagonal_tap_neon(rows.map(|r| vget_low_s16(r)));
                    let hi = diagonal_tap_neon(rows.map(|r| vget_high_s16(r)));
                    let result = vqmovn_u16(vcombine_u16(
                        vqrshrun_n_s32::<10>(lo),
                        vqrshrun_n_s32::<10>(hi),
                    ));
                    vst1_u8(d_row.as_mut_ptr().add(middle), result);
                }
                middle += 8;
            }
        }
        for x in middle..width {
            let rv = tap(
                r0[x] as i32,
                r1[x] as i32,
                r2[x] as i32,
                r3[x] as i32,
                r4[x] as i32,
                r5[x] as i32,
            );
            v_row[x] = ((rv + 16) >> 5).clamp(0, 255) as u8;

            let rd = tap(
                rh0[x] as i32,
                rh1[x] as i32,
                rh2[x] as i32,
                rh3[x] as i32,
                rh4[x] as i32,
                rh5[x] as i32,
            );
            d_row[x] = ((rd + 512) >> 10).clamp(0, 255) as u8;
        }
    }

    HalfPelPlanes { h, v, d }
}

#[cfg(target_arch = "aarch64")]
unsafe fn half_tap_neon(v: [std::arch::aarch64::int16x8_t; 6]) -> std::arch::aarch64::int16x8_t {
    use std::arch::aarch64::*;
    vaddq_s16(
        vsubq_s16(
            vmulq_n_s16(vaddq_s16(v[2], v[3]), 20),
            vmulq_n_s16(vaddq_s16(v[1], v[4]), 5),
        ),
        vaddq_s16(v[0], v[5]),
    )
}
#[cfg(target_arch = "aarch64")]
unsafe fn diagonal_tap_neon(
    v: [std::arch::aarch64::int16x4_t; 6],
) -> std::arch::aarch64::int32x4_t {
    use std::arch::aarch64::*;
    let a = vaddq_s32(vmovl_s16(v[0]), vmovl_s16(v[5]));
    let b = vmlal_n_s16(vmlal_n_s16(a, v[1], -5), v[4], -5);
    vmlal_n_s16(vmlal_n_s16(b, v[2], 20), v[3], 20)
}

#[inline(always)]
#[allow(clippy::too_many_arguments)]
pub(super) fn sad16x8_avg(
    src: &[u8],
    src_stride: usize,
    src_offset: usize,
    p1: &[u8],
    p2: &[u8],
    ref_stride: usize,
    ro1: usize,
    ro2: usize,
) -> u32 {
    #[cfg(target_arch = "aarch64")]
    {
        use std::arch::aarch64::*;
        unsafe {
            let mut acc = vdupq_n_u16(0);
            for i in 0..8 {
                let so = src_offset + (i * 2) * src_stride;
                let o1 = ro1 + (i * 2) * ref_stride;
                let o2 = ro2 + (i * 2) * ref_stride;
                let va = vld1q_u8(p1.as_ptr().add(o1));
                let vb = vld1q_u8(p2.as_ptr().add(o2));
                let avg = vrhaddq_u8(va, vb);
                let vsrc = vld1q_u8(src.as_ptr().add(so));
                let diff_l = vabdl_u8(vget_low_u8(vsrc), vget_low_u8(avg));
                let diff_h = vabdl_high_u8(vsrc, avg);
                acc = vaddq_u16(acc, vaddq_u16(diff_l, diff_h));
            }
            vaddlvq_u16(acc) as u32
        }
    }
    #[cfg(target_arch = "x86_64")]
    {
        use std::arch::x86_64::*;
        unsafe {
            let mut acc = _mm_setzero_si128();
            for i in 0..8 {
                let so = src_offset + (i * 2) * src_stride;
                let o1 = ro1 + (i * 2) * ref_stride;
                let o2 = ro2 + (i * 2) * ref_stride;
                let va = _mm_loadu_si128(p1.as_ptr().add(o1) as *const __m128i);
                let vb = _mm_loadu_si128(p2.as_ptr().add(o2) as *const __m128i);
                let avg = _mm_avg_epu8(va, vb);
                let vsrc = _mm_loadu_si128(src.as_ptr().add(so) as *const __m128i);
                acc = _mm_add_epi64(acc, _mm_sad_epu8(vsrc, avg));
            }
            let lo = _mm_cvtsi128_si32(acc) as u32;
            let hi = _mm_cvtsi128_si32(_mm_srli_si128(acc, 8)) as u32;
            lo + hi
        }
    }
    #[cfg(not(any(target_arch = "aarch64", target_arch = "x86_64")))]
    {
        sad16x8_avg_scalar(src, src_stride, src_offset, p1, p2, ref_stride, ro1, ro2)
    }
}

#[allow(dead_code)]
#[allow(clippy::too_many_arguments)]
pub(super) fn sad16x8_avg_scalar(
    src: &[u8],
    src_stride: usize,
    src_offset: usize,
    p1: &[u8],
    p2: &[u8],
    ref_stride: usize,
    ro1: usize,
    ro2: usize,
) -> u32 {
    let mut sum = 0u32;
    for i in 0..8 {
        let so = src_offset + (i * 2) * src_stride;
        let o1 = ro1 + (i * 2) * ref_stride;
        let o2 = ro2 + (i * 2) * ref_stride;
        for j in 0..16 {
            let avg = ((p1[o1 + j] as u16 + p2[o2 + j] as u16 + 1) >> 1) as u8;
            sum += src[so + j].abs_diff(avg) as u32;
        }
    }
    sum
}

#[inline(always)]
#[allow(clippy::too_many_arguments)]
pub(super) fn copy_subpel_16x16(
    dst: &mut [u8],
    dst_stride: usize,
    dst_offset: usize,
    p1: &[u8],
    p2: Option<&[u8]>,
    ref_stride: usize,
    ro1: usize,
    ro2: usize,
) {
    if let Some(p2) = p2 {
        #[cfg(target_arch = "aarch64")]
        {
            use std::arch::aarch64::*;
            unsafe {
                for i in 0..16 {
                    let d = dst_offset + i * dst_stride;
                    let o1 = ro1 + i * ref_stride;
                    let o2 = ro2 + i * ref_stride;
                    let va = vld1q_u8(p1.as_ptr().add(o1));
                    let vb = vld1q_u8(p2.as_ptr().add(o2));
                    let avg = vrhaddq_u8(va, vb);
                    vst1q_u8(dst.as_mut_ptr().add(d), avg);
                }
            }
        }
        #[cfg(target_arch = "x86_64")]
        {
            use std::arch::x86_64::*;
            unsafe {
                for i in 0..16 {
                    let d = dst_offset + i * dst_stride;
                    let o1 = ro1 + i * ref_stride;
                    let o2 = ro2 + i * ref_stride;
                    let va = _mm_loadu_si128(p1.as_ptr().add(o1) as *const __m128i);
                    let vb = _mm_loadu_si128(p2.as_ptr().add(o2) as *const __m128i);
                    let avg = _mm_avg_epu8(va, vb);
                    _mm_storeu_si128(dst.as_mut_ptr().add(d) as *mut __m128i, avg);
                }
            }
        }
        #[cfg(not(any(target_arch = "aarch64", target_arch = "x86_64")))]
        {
            for i in 0..16 {
                let d = dst_offset + i * dst_stride;
                let o1 = ro1 + i * ref_stride;
                let o2 = ro2 + i * ref_stride;
                for j in 0..16 {
                    dst[d + j] = ((p1[o1 + j] as u16 + p2[o2 + j] as u16 + 1) >> 1) as u8;
                }
            }
        }
    } else {
        for i in 0..16 {
            let d = dst_offset + i * dst_stride;
            let o1 = ro1 + i * ref_stride;
            dst[d..d + 16].copy_from_slice(&p1[o1..o1 + 16]);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sad16x8_matches_scalar() {
        let mut a = vec![0u8; 16 * 16];
        let mut b = vec![0u8; 16 * 16];
        let mut seed = 0x12345678u32;
        for _ in 0..20 {
            for v in a.iter_mut().chain(b.iter_mut()) {
                seed = seed.wrapping_mul(1664525).wrapping_add(1013904223);
                *v = (seed >> 16) as u8;
            }
            let sim = sad16x8(&a, 16, 0, &b, 16, 0);
            let sca = sad16x8_scalar(&a, 16, 0, &b, 16, 0);
            assert_eq!(sim, sca);
        }
    }

    #[test]
    fn chroma_simd_matches_scalar() {
        let mut seed = 93u32;
        let src: Vec<_> = (0..16 * 16)
            .map(|_| {
                seed = seed.wrapping_mul(1664525).wrapping_add(1013904223);
                (seed >> 16) as u8
            })
            .collect();
        for dy in 0..8 {
            for dx in 0..8 {
                let mut a = [0; 16 * 16];
                let mut b = a;
                chroma8x8(&mut a, 16, 17, &src, 16, 18, dx, dy);
                chroma8x8_scalar(&mut b, 16, 17, &src, 16, 18, dx, dy);
                assert_eq!(a, b);
            }
        }
    }
    #[test]
    fn half_pel_simd_matches_scalar() {
        let mut seed = 91u32;
        for (width, height, stride) in [(16, 16, 16), (34, 19, 48), (130, 98, 144)] {
            let plane: Vec<_> = (0..stride * height)
                .map(|_| {
                    seed = seed.wrapping_mul(1664525).wrapping_add(1013904223);
                    (seed >> 16) as u8
                })
                .collect();
            let simd = compute_half_pel(&plane, stride, width, height);
            let scalar = compute_half_pel_impl::<false>(&plane, stride, width, height);
            assert_eq!(simd.h, scalar.h);
            assert_eq!(simd.v, scalar.v);
            assert_eq!(simd.d, scalar.d);
        }
    }
    #[test]
    fn half_pel_and_subpel_match_luma_mc() {
        let width = 32;
        let height = 32;
        let stride = 32;
        let mut plane = vec![0u8; stride * height];
        let mut seed = 0xabcdef01u32;
        for v in &mut plane {
            seed = seed.wrapping_mul(1664525).wrapping_add(1013904223);
            *v = (seed >> 16) as u8;
        }
        let hp = compute_half_pel(&plane, stride, width, height);

        for y in 0..height as i32 {
            for x in 0..width as i32 {
                let h_mc = luma_mc(&plane, stride, width, height, x, y, (2, 0));
                assert_eq!(
                    hp.h[y as usize * stride + x as usize],
                    h_mc,
                    "h mismatch at {},{}",
                    x,
                    y
                );
                let v_mc = luma_mc(&plane, stride, width, height, x, y, (0, 2));
                assert_eq!(
                    hp.v[y as usize * stride + x as usize],
                    v_mc,
                    "v mismatch at {},{}",
                    x,
                    y
                );
                let d_mc = luma_mc(&plane, stride, width, height, x, y, (2, 2));
                assert_eq!(
                    hp.d[y as usize * stride + x as usize],
                    d_mc,
                    "d mismatch at {},{}",
                    x,
                    y
                );
            }
        }

        // Test all 16 subpel positions against luma_mc
        for fy in 0..4 {
            for fx in 0..4 {
                let (p1, ox1, oy1, p2) = subpel_sources(fx, fy);
                let get_p = |p: SubpelPlane| match p {
                    SubpelPlane::Int => &plane,
                    SubpelPlane::H => &hp.h,
                    SubpelPlane::V => &hp.v,
                    SubpelPlane::D => &hp.d,
                };
                let plane1 = get_p(p1);
                let plane2 = p2.map(|(p, _, _)| get_p(p));
                let (ox2, oy2) = p2.map(|(_, ox, oy)| (ox, oy)).unwrap_or((0, 0));

                for y in 0..(height - 2) {
                    for x in 0..(width - 2) {
                        let sample1 = plane1[(y + oy1) * stride + (x + ox1)];
                        let actual = if let Some(p2) = plane2 {
                            let sample2 = p2[(y + oy2) * stride + (x + ox2)];
                            ((sample1 as u16 + sample2 as u16 + 1) >> 1) as u8
                        } else {
                            sample1
                        };
                        let expected =
                            luma_mc(&plane, stride, width, height, x as i32, y as i32, (fx, fy));
                        assert_eq!(
                            actual, expected,
                            "subpel ({},{}) mismatch at {},{}",
                            fx, fy, x, y
                        );
                    }
                }
            }
        }
    }
}
