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
    let mut candidates = Vec::with_capacity(9);
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
        candidates.push((sad, mode, p, rate));
    }
    candidates.sort_unstable_by_key(|v| v.0);
    candidates
        .into_iter()
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
) -> Vec<u8> {
    let mut t = vec![128i32; n + 1];
    let mut l = t.clone();
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
            (t[1..].iter().sum::<i32>() + l[1..].iter().sum::<i32>() + n as i32) / (2 * n) as i32
        }
        (true, false) => (l[1..].iter().sum::<i32>() + (n / 2) as i32) / n as i32,
        (false, true) => (t[1..].iter().sum::<i32>() + (n / 2) as i32) / n as i32,
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
    let mut p = vec![0; n * n];
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
