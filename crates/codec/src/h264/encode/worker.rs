use super::{
    bits::{signed_bits, Bits},
    cavlc, predict,
    transform::{self, SCAN},
};
use crate::Yuv420;
use std::sync::Arc;

#[derive(Clone, Default)]
pub(super) struct Mb {
    pub intra: bool,
    pub mv: (i32, i32),
    pub nz: [u8; 16],
    pub cnz: [[u8; 4]; 2],
    pub modes: [u8; 16],
    pub qp: u8,
    pub quality: u8,
}
pub(super) struct Job {
    pub source: Arc<Yuv420>,
    pub previous: Option<Arc<Yuv420>>,
    pub reference: Arc<Yuv420>,
    pub half_pel: Option<Arc<predict::HalfPelPlanes>>,
    pub quality: Arc<Vec<u8>>,
    pub first: usize,
    pub end: usize,
    pub qp: u8,
    pub suppression: u8,
    pub subpixel: bool,
    pub is_motion: bool,
    pub global: (i32, i32),
    pub idr: bool,
    pub frame_num: u32,
    pub idr_id: u32,
    pub refresh: usize,
    pub refresh_start: usize,
}
pub(super) struct Band {
    pub first: usize,
    pub picture: Yuv420,
    pub mbs: Vec<Mb>,
    pub nal: Vec<u8>,
    pub skipped: u32,
    pub qp_sum: u32,
}
fn pos(i: usize) -> (usize, usize) {
    (
        ((i / 4) % 2) * 8 + (i % 2) * 4,
        (i / 8) * 8 + ((i / 2) % 2) * 4,
    )
}
fn neighbours(mbs: &[Mb], w: usize, index: usize) -> (Option<&Mb>, Option<&Mb>, Option<&Mb>) {
    let a = if !index.is_multiple_of(w) {
        Some(&mbs[index - 1])
    } else {
        None
    };
    let b = if index >= w {
        Some(&mbs[index - w])
    } else {
        None
    };
    let c = if index >= w && index % w + 1 < w {
        Some(&mbs[index - w + 1])
    } else if index >= w && !index.is_multiple_of(w) {
        Some(&mbs[index - w - 1])
    } else {
        None
    };
    (a, b, c)
}
fn mv_predict(mbs: &[Mb], w: usize, index: usize, skip: bool) -> (i32, i32) {
    let (a, b, c) = neighbours(mbs, w, index);
    if skip
        && (a.is_none()
            || b.is_none()
            || a.is_some_and(|m| !m.intra && m.mv == (0, 0))
            || b.is_some_and(|m| !m.intra && m.mv == (0, 0)))
    {
        return (0, 0);
    }
    let valid = |m: Option<&Mb>| m.filter(|m| !m.intra).map(|m| m.mv);
    let (a, b, c) = (valid(a), valid(b), valid(c));
    if b.is_none() && c.is_none() {
        return a.unwrap_or_default();
    }
    let vals = [a, b, c];
    if vals.iter().flatten().count() == 1 {
        return *vals.iter().flatten().next().unwrap();
    }
    let mut x = vals.map(|v| v.unwrap_or_default().0);
    let mut y = vals.map(|v| v.unwrap_or_default().1);
    x.sort_unstable();
    y.sort_unstable();
    (x[1], y[1])
}
fn equal_mb(a: &Yuv420, b: &Yuv420, x: usize, y: usize) -> bool {
    equal_at(a, b, (x, y), (x, y))
}
fn equal_at(a: &Yuv420, b: &Yuv420, (x, y): (usize, usize), (rx, ry): (usize, usize)) -> bool {
    for (ap, bp, as_, bs, n) in [
        (&a.y, &b.y, a.y_stride, b.y_stride, 16),
        (&a.u, &b.u, a.c_stride, b.c_stride, 8),
        (&a.v, &b.v, a.c_stride, b.c_stride, 8),
    ] {
        let xx = x * n / 16;
        let yy = y * n / 16;
        for r in 0..n {
            if ap[(yy + r) * as_ + xx..(yy + r) * as_ + xx + n]
                != bp
                    [(ry * n / 16 + r) * bs + rx * n / 16..(ry * n / 16 + r) * bs + rx * n / 16 + n]
            {
                return false;
            }
        }
    }
    true
}
fn copy_mc(
    dst: &mut Yuv420,
    reference: &Yuv420,
    hp: Option<&predict::HalfPelPlanes>,
    x: usize,
    y: usize,
    global_y: usize,
    mv: (i32, i32),
) {
    let rx = x as i32 + mv.0.div_euclid(4);
    let ry = global_y as i32 + mv.1.div_euclid(4);
    let pw = reference.padded_width() as i32;
    let ph = reference.padded_height() as i32;

    if mv.0 % 8 == 0 && mv.1 % 8 == 0 && rx >= 0 && ry >= 0 && rx + 16 <= pw && ry + 16 <= ph {
        for (dp, rp, ds, rs, n) in [
            (
                &mut dst.y,
                &reference.y,
                dst.y_stride,
                reference.y_stride,
                16,
            ),
            (
                &mut dst.u,
                &reference.u,
                dst.c_stride,
                reference.c_stride,
                8,
            ),
            (
                &mut dst.v,
                &reference.v,
                dst.c_stride,
                reference.c_stride,
                8,
            ),
        ] {
            for row in 0..n {
                let d = (y * n / 16 + row) * ds + x * n / 16;
                let r = (ry as usize * n / 16 + row) * rs + rx as usize * n / 16;
                dp[d..d + n].copy_from_slice(&rp[r..r + n]);
            }
        }
        return;
    }

    if mv.0 % 4 == 0 && mv.1 % 4 == 0 && rx >= 0 && ry >= 0 && rx + 16 <= pw && ry + 16 <= ph {
        for row in 0..16 {
            let d = (y + row) * dst.y_stride + x;
            let r = (ry as usize + row) * reference.y_stride + rx as usize;
            dst.y[d..d + 16].copy_from_slice(&reference.y[r..r + 16]);
        }
    } else if let Some(hp) = hp.filter(|_| rx >= 0 && ry >= 0 && rx + 17 <= pw && ry + 17 <= ph) {
        let fx = mv.0.rem_euclid(4);
        let fy = mv.1.rem_euclid(4);
        let (p1, ox1, oy1, p2) = predict::subpel_sources(fx, fy);
        let get_p = |p: predict::SubpelPlane| -> &[u8] {
            match p {
                predict::SubpelPlane::Int => &reference.y,
                predict::SubpelPlane::H => &hp.h,
                predict::SubpelPlane::V => &hp.v,
                predict::SubpelPlane::D => &hp.d,
            }
        };
        let plane1 = get_p(p1);
        let ro1 = (ry as usize + oy1) * reference.y_stride + (rx as usize + ox1);
        let plane2 = p2.map(|(p, _, _)| get_p(p));
        let ro2 = p2
            .map(|(_, ox, oy)| (ry as usize + oy) * reference.y_stride + (rx as usize + ox))
            .unwrap_or(0);
        let d = y * dst.y_stride + x;
        predict::copy_subpel_16x16(
            &mut dst.y,
            dst.y_stride,
            d,
            plane1,
            plane2,
            reference.y_stride,
            ro1,
            ro2,
        );
    } else {
        for yy in 0..16 {
            for xx in 0..16 {
                dst.y[(y + yy) * dst.y_stride + x + xx] = predict::luma_mc(
                    &reference.y,
                    reference.y_stride,
                    reference.padded_width(),
                    reference.padded_height(),
                    (x + xx) as i32,
                    (global_y + yy) as i32,
                    mv,
                );
            }
        }
    }

    for (dp, rp) in [(&mut dst.u, &reference.u), (&mut dst.v, &reference.v)] {
        let cx = x as i32 / 2 + mv.0.div_euclid(8);
        let cy = global_y as i32 / 2 + mv.1.div_euclid(8);
        if cx >= 0 && cy >= 0 && cx + 9 <= pw / 2 && cy + 9 <= ph / 2 {
            predict::chroma8x8(
                dp,
                dst.c_stride,
                y / 2 * dst.c_stride + x / 2,
                rp,
                reference.c_stride,
                cy as usize * reference.c_stride + cx as usize,
                mv.0.rem_euclid(8) as u16,
                mv.1.rem_euclid(8) as u16,
            );
            continue;
        }
        for yy in 0..8 {
            for xx in 0..8 {
                dp[(y / 2 + yy) * dst.c_stride + x / 2 + xx] = predict::chroma_mc(
                    rp,
                    reference.c_stride,
                    reference.padded_width() / 2,
                    reference.padded_height() / 2,
                    (x / 2 + xx) as i32,
                    (global_y / 2 + yy) as i32,
                    mv,
                );
            }
        }
    }
}
fn context(mbs: &[Mb], w: usize, index: usize, bx: usize, by: usize, plane: usize) -> i32 {
    let n = if plane == 0 { 4 } else { 2 };
    let get = |m: &Mb, x: usize, y: usize| {
        if plane == 0 {
            m.nz[y * n + x]
        } else {
            m.cnz[plane - 1][y * n + x]
        }
    };
    let a = if bx > 0 {
        Some(get(&mbs[index], bx - 1, by))
    } else if !index.is_multiple_of(w) {
        Some(get(&mbs[index - 1], n - 1, by))
    } else {
        None
    };
    let b = if by > 0 {
        Some(get(&mbs[index], bx, by - 1))
    } else if index >= w {
        Some(get(&mbs[index - w], bx, n - 1))
    } else {
        None
    };
    match (a, b) {
        (Some(a), Some(b)) => (a as i32 + b as i32 + 1) >> 1,
        (Some(v), None) | (None, Some(v)) => v as i32,
        _ => 0,
    }
}
fn mode_predict(mbs: &[Mb], w: usize, index: usize, bx: usize, by: usize) -> u8 {
    let a = if bx > 0 {
        Some(mbs[index].modes[by * 4 + bx - 1])
    } else if !index.is_multiple_of(w) {
        Some(mbs[index - 1].modes[by * 4 + 3])
    } else {
        None
    };
    let b = if by > 0 {
        Some(mbs[index].modes[(by - 1) * 4 + bx])
    } else if index >= w {
        Some(mbs[index - w].modes[12 + bx])
    } else {
        None
    };
    match (a, b) {
        (Some(a), Some(b)) => a.min(b),
        _ => 2,
    }
}
#[allow(clippy::too_many_arguments)]
fn motion_cost(
    src: &Yuv420,
    r: &Yuv420,
    hp: Option<&predict::HalfPelPlanes>,
    x: usize,
    y: usize,
    mv: (i32, i32),
    pred: (i32, i32),
    lambda: u32,
    limit: u32,
) -> u32 {
    let mut cost = lambda * (signed_bits(mv.0 - pred.0) + signed_bits(mv.1 - pred.1));
    let pw = r.padded_width() as i32;
    let ph = r.padded_height() as i32;
    if mv.0 % 4 == 0 && mv.1 % 4 == 0 {
        let rx = x as i32 + mv.0 / 4;
        let ry = y as i32 + mv.1 / 4;
        if rx >= 0 && ry >= 0 && rx + 16 <= pw && ry + 16 <= ph {
            let so = y * src.y_stride + x;
            let ro = ry as usize * r.y_stride + rx as usize;
            cost += predict::sad16x8(&src.y, src.y_stride, so, &r.y, r.y_stride, ro);
            if cost >= limit {
                return cost;
            }
            cost += predict::sad16x8(
                &src.y,
                src.y_stride,
                so + src.y_stride,
                &r.y,
                r.y_stride,
                ro + r.y_stride,
            );
            return cost;
        }
        for yy in 0..16 {
            let sy = (ry + yy as i32).clamp(0, ph - 1) as usize;
            let r_row = sy * r.y_stride;
            let s_row = (y + yy) * src.y_stride + x;
            for xx in 0..16 {
                let sx = (rx + xx as i32).clamp(0, pw - 1) as usize;
                cost += src.y[s_row + xx].abs_diff(r.y[r_row + sx]) as u32;
            }
        }
        return cost;
    }
    if let Some(hp) = hp {
        let rx = x as i32 + mv.0.div_euclid(4);
        let ry = y as i32 + mv.1.div_euclid(4);
        if rx >= 0 && ry >= 0 && rx + 17 <= pw && ry + 17 <= ph {
            let fx = mv.0.rem_euclid(4);
            let fy = mv.1.rem_euclid(4);
            let (p1, ox1, oy1, p2) = predict::subpel_sources(fx, fy);
            let get_p = |p: predict::SubpelPlane| match p {
                predict::SubpelPlane::Int => &r.y,
                predict::SubpelPlane::H => &hp.h,
                predict::SubpelPlane::V => &hp.v,
                predict::SubpelPlane::D => &hp.d,
            };
            let plane1 = get_p(p1);
            let so = y * src.y_stride + x;
            let ro1 = (ry as usize + oy1) * r.y_stride + (rx as usize + ox1);
            if let Some((p2_type, ox2, oy2)) = p2 {
                let plane2 = get_p(p2_type);
                let ro2 = (ry as usize + oy2) * r.y_stride + (rx as usize + ox2);
                cost += predict::sad16x8_avg(
                    &src.y,
                    src.y_stride,
                    so,
                    plane1,
                    plane2,
                    r.y_stride,
                    ro1,
                    ro2,
                );
                if cost >= limit {
                    return cost;
                }
                cost += predict::sad16x8_avg(
                    &src.y,
                    src.y_stride,
                    so + src.y_stride,
                    plane1,
                    plane2,
                    r.y_stride,
                    ro1 + r.y_stride,
                    ro2 + r.y_stride,
                );
            } else {
                cost += predict::sad16x8(&src.y, src.y_stride, so, plane1, r.y_stride, ro1);
                if cost >= limit {
                    return cost;
                }
                cost += predict::sad16x8(
                    &src.y,
                    src.y_stride,
                    so + src.y_stride,
                    plane1,
                    r.y_stride,
                    ro1 + r.y_stride,
                );
            }
            return cost;
        }
    }
    for yy in 0..16 {
        for xx in 0..16 {
            cost += src.y[(y + yy) * src.y_stride + x + xx].abs_diff(predict::luma_mc(
                &r.y,
                r.y_stride,
                r.padded_width(),
                r.padded_height(),
                (x + xx) as i32,
                (y + yy) as i32,
                mv,
            )) as u32;
        }
    }
    cost
}
pub(super) fn scroll(src: &Yuv420, old: &Yuv420, first: usize, end: usize) -> (i32, i32) {
    let width = src.padded_width();
    let start = first * 16;
    let end = end * 16;
    let mut changed = 0;
    let mut samples = 0;
    for y in (start..end).step_by(8) {
        for x in (0..width).step_by(16) {
            samples += 1;
            changed += usize::from(src.y[y * src.y_stride + x] != old.y[y * old.y_stride + x]);
        }
    }
    if changed * 256 < samples {
        return (0, 0);
    }
    let hash = |p: &Yuv420, row: usize| -> u64 {
        p.y[row * p.y_stride..row * p.y_stride + width]
            .iter()
            .step_by(16)
            .fold(0xcbf29ce484222325u64, |h, &v| {
                (h ^ v as u64).wrapping_mul(0x100000001b3)
            })
    };
    let mut hist = [0u32; 257];
    let old_hash: Vec<_> = (0..old.padded_height()).map(|y| hash(old, y)).collect();
    for y in (start..end).step_by(8) {
        let h = hash(src, y);
        for dy in -128i32..=128 {
            let yy = y as i32 + dy;
            if yy >= 0 && (yy as usize) < old_hash.len() && old_hash[yy as usize] == h {
                hist[(dy + 128) as usize] += 1;
            }
        }
    }
    let best = (0..257)
        .max_by_key(|&i| (hist[i], std::cmp::Reverse(i.abs_diff(128))))
        .unwrap();
    if best != 128 && hist[best] > hist[128] + 2 {
        return (0, (best as i32 - 128) * 4);
    }
    let mut best = (0, 0);
    let mut best_cost = u64::MAX;
    let mut zero_cost = 0;
    let mut samples = 0;
    for dx in -128i32..=128 {
        let mut cost = 0u64;
        for y in (start..end).step_by(32) {
            for x in (128..width.saturating_sub(128)).step_by(16) {
                if dx == 0 {
                    samples += 1;
                }
                cost += src.y[y * src.y_stride + x]
                    .abs_diff(old.y[y * old.y_stride + (x as i32 + dx) as usize])
                    as u64;
            }
        }
        if dx == 0 {
            zero_cost = cost;
        }
        if cost < best_cost {
            best_cost = cost;
            best = (dx * 4, 0);
        }
    }
    // A noisy best offset is not evidence of scrolling. Keep the motion path enabled.
    if best_cost <= samples * 2 && best_cost * 4 < zero_cost {
        best
    } else {
        (0, 0)
    }
}
#[allow(clippy::too_many_arguments)]
fn motion(
    job: &Job,
    is_motion: bool,
    mbs: &[Mb],
    w: usize,
    index: usize,
    x: usize,
    y: usize,
    pred: (i32, i32),
    scroll: (i32, i32),
    lambda: u32,
) -> ((i32, i32), u32) {
    let mut candidates = [(0, 0); 6];
    candidates[1] = pred;
    candidates[2] = scroll;
    let mut count = 3;
    let (a, b, c) = neighbours(mbs, w, index);
    for m in [a, b, c].into_iter().flatten() {
        if !m.intra {
            candidates[count] = m.mv;
            count += 1;
        }
    }
    let round = |v: (i32, i32)| ((v.0 / 4) * 4, (v.1 / 4) * 4);
    let cost = |mv, limit| {
        motion_cost(
            &job.source,
            &job.reference,
            job.half_pel.as_deref(),
            x,
            y,
            mv,
            pred,
            lambda,
            limit,
        )
    };
    let mut best = (0, 0);
    let mut score = u32::MAX;
    for (i, &v) in candidates[..count].iter().enumerate() {
        let v = round(v);
        if candidates[..i].iter().any(|&p| round(p) == v) {
            continue;
        }
        let s = cost(v, score);
        if s < score {
            score = s;
            best = v;
        }
    }
    let early_exit_cost = if is_motion {
        256 + lambda * 4
    } else {
        32 + lambda * 2
    };
    if score < early_exit_cost {
        return (best, score);
    }
    let max_iter = if is_motion { 2 } else { 4 };
    let centre = best;
    let steps = if score < 1024 + lambda * 16 {
        [2, 1, 0, 0]
    } else {
        [8, 4, 2, 1]
    };
    for step in steps {
        if step == 0 {
            continue;
        }
        for _ in 0..max_iter {
            let mut next = best;
            for (dx, dy) in [(step * 4, 0), (-step * 4, 0), (0, step * 4), (0, -step * 4)] {
                let v = (best.0 + dx, best.1 + dy);
                if (v.0 - centre.0).abs() > 64 || (v.1 - centre.1).abs() > 64 {
                    continue;
                }
                let s = cost(v, score);
                if s < score {
                    score = s;
                    next = v;
                }
            }
            if next == best || (is_motion && score < early_exit_cost) {
                break;
            }
            best = next;
        }
        if is_motion && score < early_exit_cost {
            break;
        }
    }
    let subpel_allowed = if is_motion {
        score > lambda * 96 && score < 4000
    } else {
        score > lambda * 64
    };
    if job.subpixel && subpel_allowed {
        for step in [2, 1] {
            let centre = best;
            for dy in [-step, 0, step] {
                for dx in [-step, 0, step] {
                    if dx == 0 && dy == 0 {
                        continue;
                    }
                    let v = (centre.0 + dx, centre.1 + dy);
                    let s = cost(v, score);
                    if s < score {
                        score = s;
                        best = v;
                    }
                }
            }
        }
    }
    (best, score)
}

pub(super) fn header(first: usize, idr: bool, frame_num: u32, idr_id: u32, qp: u8) -> Bits {
    let mut b = Bits::default();
    b.ue(first as u32);
    b.ue(if idr { 2 } else { 0 });
    b.ue(0);
    b.put(frame_num, 16);
    if idr {
        b.ue(idr_id);
    } else {
        b.bit(false);
        b.bit(false);
    }
    if idr {
        b.bit(false);
        b.bit(false);
    } else {
        b.bit(false);
    }
    b.se(qp as i32 - 26);
    b.ue(0);
    b.se(0);
    b.se(0);
    b
}

pub(super) fn encode(job: Job) -> Band {
    let w = job.source.padded_width() / 16;
    let rows = job.end - job.first;
    let mut rec = Yuv420::new(job.source.width, rows as u32 * 16);
    let mut mbs = vec![Mb::default(); w * rows];
    let mut b = header(job.first * w, job.idr, job.frame_num, job.idr_id, job.qp);
    let mut skip_run = 0;
    let mut skipped = 0;
    let mut qpy = job.qp;
    let mut qp_sum = 0;
    let global = job.global;
    let lambda = (0.85 * 2f64.powf((job.qp as f64 - 12.) / 6.))
        .sqrt()
        .round() as u32;
    for index in 0..mbs.len() {
        let x = (index % w) * 16;
        let y = (index / w) * 16;
        let gy = y + job.first * 16;
        let gi = job.first * w + index;
        mbs[index].modes = [2; 16];
        let same = !job.idr
            && job
                .previous
                .as_ref()
                .is_some_and(|old| equal_mb(&job.source, old, x, gy));
        let mut reuse = if same {
            Some(((0, 0), job.quality[gi]))
        } else {
            None
        };
        if reuse.is_none()
            && !job.idr
            && global != (0, 0)
            && global.0 % 64 == 0
            && global.1 % 64 == 0
        {
            let rx = x as i32 + global.0 / 4;
            let ry = gy as i32 + global.1 / 4;
            if rx >= 0
                && ry >= 0
                && rx + 16 <= job.source.padded_width() as i32
                && ry + 16 <= job.source.padded_height() as i32
                && job.previous.as_ref().is_some_and(|old| {
                    equal_at(&job.source, old, (x, gy), (rx as usize, ry as usize))
                })
            {
                reuse = Some((
                    global,
                    job.quality[(ry as usize / 16) * w + rx as usize / 16],
                ));
            }
        }
        let previous_quality = reuse.map_or(job.quality[gi], |v| v.1);
        let refine = reuse.is_some()
            && previous_quality > 18
            && (gi + job.quality.len() - job.refresh_start) % job.quality.len() < job.refresh;
        let qp = if refine {
            previous_quality.saturating_sub(4).max(18).min(job.qp)
        } else {
            job.qp
        };
        let pred = mv_predict(&mbs, w, index, false);
        let skip_mv = mv_predict(&mbs, w, index, true);
        if let Some((mv, quality)) = reuse.filter(|_| !refine) {
            copy_mc(
                &mut rec,
                &job.reference,
                job.half_pel.as_deref(),
                x,
                y,
                gy,
                mv,
            );
            mbs[index].mv = mv;
            mbs[index].qp = qpy;
            mbs[index].quality = quality;
            if skip_mv == mv {
                skip_run += 1;
                skipped += 1;
            } else {
                b.ue(skip_run);
                skip_run = 0;
                b.ue(0);
                b.se(mv.0 - pred.0);
                b.se(mv.1 - pred.1);
                b.ue(0);
            }
            qp_sum += qpy as u32;
            continue;
        }
        let is_motion = job.is_motion && global == (0, 0);
        let (mv, inter_cost) = if job.idr {
            ((0, 0), u32::MAX)
        } else {
            motion(&job, is_motion, &mbs, w, index, x, gy, pred, global, lambda)
        };
        // A single DC trial avoids evaluating every intra mode for an obvious temporal match.
        let intra_trial = predict::intra_plane(&rec.y, rec.y_stride, x, y, 16, 2, false);
        let mut intra_cost = 0;
        for yy in 0..16 {
            for xx in 0..16 {
                intra_cost += job.source.y[(gy + yy) * job.source.y_stride + x + xx]
                    .abs_diff(intra_trial[yy * 16 + xx]) as u32;
            }
        }
        let intra = job.idr || intra_cost + lambda * 24 < inter_cost;
        mbs[index].intra = intra;
        mbs[index].mv = if intra { (0, 0) } else { mv };
        mbs[index].quality = if job.suppression == 0 { qp } else { 51 };
        if !intra {
            copy_mc(
                &mut rec,
                &job.reference,
                job.half_pel.as_deref(),
                x,
                y,
                gy,
                mv,
            );
        }
        let luma_quantizer = transform::Quantizer::new(qp, intra, job.suppression);
        let mut coeffs = [[0i32; 16]; 16];
        let mut modes = [(2u8, 2u8); 16];
        let mut cbp = 0u32;
        let mut cost4 = 0;
        let mut best16 = (u32::MAX, 2u8, [0; 256]);
        if intra {
            for mode in 0..4 {
                if (mode == 0 && y == 0)
                    || (mode == 1 && x == 0)
                    || (mode == 3 && (x == 0 || y == 0))
                {
                    continue;
                }
                let p = predict::intra_plane(&rec.y, rec.y_stride, x, y, 16, mode, false);
                let mut cost = lambda * 8;
                for i in 0..16 {
                    if cost >= best16.0 {
                        break;
                    }
                    let (bx, by) = pos(i);
                    let mut block = [0; 16];
                    for yy in 0..4 {
                        block[yy * 4..yy * 4 + 4]
                            .copy_from_slice(&p[(by + yy) * 16 + bx..(by + yy) * 16 + bx + 4]);
                    }
                    cost += transform::satd(
                        &job.source.y,
                        job.source.y_stride,
                        (gy + by) * job.source.y_stride + x + bx,
                        &block,
                    );
                }
                if cost < best16.0 {
                    best16 = (cost, mode, p);
                }
            }
        }
        let skip_intra4 = is_motion && (best16.0 <= lambda * 48 || inter_cost <= lambda * 48);
        let fast16 = intra && (skip_intra4 || best16.0 <= lambda * 16);
        for i in 0..if fast16 { 0 } else { 16 } {
            // All remaining intra4 costs are nonnegative; intra16 has already won.
            if intra && cost4 >= best16.0 {
                break;
            }
            let (bx, by) = pos(i);
            let off = (y + by) * rec.y_stride + x + bx;
            let so = (gy + by) * job.source.y_stride + x + bx;
            let p = if intra {
                let predicted = mode_predict(&mbs, w, index, bx / 4, by / 4);
                let tr =
                    !(bx == 4 && by % 8 == 4 || bx == 12 && by > 0) && x + bx + 8 <= rec.y_stride;
                let (mode, p, cost) = predict::intra4(
                    &job.source.y,
                    job.source.y_stride,
                    so,
                    &rec.y,
                    rec.y_stride,
                    x + bx,
                    y + by,
                    tr,
                    predicted,
                    lambda,
                );
                cost4 += cost;
                mbs[index].modes[by / 4 * 4 + bx / 4] = mode;
                modes[i] = (mode, predicted);
                p
            } else {
                let mut p = [0; 16];
                for yy in 0..4 {
                    p[yy * 4..yy * 4 + 4].copy_from_slice(
                        &rec.y[off + yy * rec.y_stride..off + yy * rec.y_stride + 4],
                    );
                }
                p
            };
            let (c, r) =
                transform::block(&job.source.y, job.source.y_stride, so, &p, &luma_quantizer);
            coeffs[i] = c;
            if c.iter().any(|&v| v != 0) {
                cbp |= 1 << (i / 4);
            }
            for yy in 0..4 {
                rec.y[off + yy * rec.y_stride..off + yy * rec.y_stride + 4]
                    .copy_from_slice(&r[yy * 4..yy * 4 + 4]);
            }
        }
        let intra16 = intra && (fast16 || best16.0 <= cost4);
        let mut ldc = [0i32; 16];
        if intra16 {
            cbp = 0;
            mbs[index].modes = [2; 16];
            if best16.0 == lambda * 8 {
                for yy in 0..16 {
                    rec.y[(y + yy) * rec.y_stride + x..(y + yy) * rec.y_stride + x + 16]
                        .copy_from_slice(&best16.2[yy * 16..yy * 16 + 16]);
                }
            } else {
                let mut transforms = [[0; 16]; 16];
                let mut dc = [0; 16];
                for (i, transformed) in transforms.iter_mut().enumerate() {
                    let (bx, by) = pos(i);
                    let mut residual = [0; 16];
                    for yy in 0..4 {
                        for xx in 0..4 {
                            residual[yy * 4 + xx] = job.source.y
                                [(gy + by + yy) * job.source.y_stride + x + bx + xx]
                                as i32
                                - best16.2[(by + yy) * 16 + bx + xx] as i32;
                        }
                    }
                    *transformed = transform::forward(residual);
                    dc[(by / 4) * 4 + bx / 4] = transformed[0];
                }
                let dc = transform::hadamard(dc).map(|v| {
                    transform::suppress(
                        transform::quant((v + 1) >> 1, qp, 0, true, 1),
                        job.suppression,
                    )
                });
                for i in 0..16 {
                    ldc[i] = dc[SCAN[i]];
                }
                let dci = transform::luma_dc_inverse(dc, qp);
                for i in 0..16 {
                    let (bx, by) = pos(i);
                    let (q, mut dq) = luma_quantizer.apply(transforms[i]);
                    dq[0] = dci[(by / 4) * 4 + bx / 4];
                    coeffs[i] = [0; 16];
                    for (j, &s) in SCAN.iter().enumerate().skip(1) {
                        coeffs[i][j] = q[s];
                    }
                    if coeffs[i][1..].iter().any(|&v| v != 0) {
                        cbp = 15;
                    }
                    let residual = transform::inverse(dq);
                    for yy in 0..4 {
                        for xx in 0..4 {
                            rec.y[(y + by + yy) * rec.y_stride + x + bx + xx] =
                                (best16.2[(by + yy) * 16 + bx + xx] as i32 + residual[yy * 4 + xx])
                                    .clamp(0, 255) as u8;
                        }
                    }
                }
            }
        }
        let mut cc = [[[0i32; 16]; 4]; 2];
        let mut dc = [[0i32; 4]; 2];
        let cqp = transform::chroma_qp(qp);
        let chroma_quantizer = transform::Quantizer::new(cqp, intra, job.suppression);
        let mut cmode = 0;
        if intra {
            let mut best = u32::MAX;
            for mode in 0..4 {
                if (mode == 1 && x == 0)
                    || (mode == 2 && y == 0)
                    || (mode == 3 && (x == 0 || y == 0))
                {
                    continue;
                }
                let mut cost = 0;
                for (dp, sp) in [(&rec.u, &job.source.u), (&rec.v, &job.source.v)] {
                    let p = predict::intra_plane(dp, rec.c_stride, x / 2, y / 2, 8, mode, true);
                    for yy in 0..8 {
                        for xx in 0..8 {
                            cost += sp[(gy / 2 + yy) * job.source.c_stride + x / 2 + xx]
                                .abs_diff(p[yy * 8 + xx])
                                as u32;
                        }
                    }
                }
                if cost < best {
                    best = cost;
                    cmode = mode;
                }
            }
            for dp in [&mut rec.u, &mut rec.v] {
                let p = predict::intra_plane(dp, rec.c_stride, x / 2, y / 2, 8, cmode, true);
                for yy in 0..8 {
                    dp[(y / 2 + yy) * rec.c_stride + x / 2
                        ..(y / 2 + yy) * rec.c_stride + x / 2 + 8]
                        .copy_from_slice(&p[yy * 8..yy * 8 + 8]);
                }
            }
        }
        let mut chroma_cbp = 0;
        for (plane, (dp, sp)) in [(&mut rec.u, &job.source.u), (&mut rec.v, &job.source.v)]
            .into_iter()
            .enumerate()
        {
            if (0..8).all(|row| {
                let d = (y / 2 + row) * rec.c_stride + x / 2;
                let s = (gy / 2 + row) * job.source.c_stride + x / 2;
                dp[d..d + 8] == sp[s..s + 8]
            }) {
                continue;
            }
            let mut transformed = [[0; 16]; 4];
            let mut predictions = [[0; 16]; 4];
            for i in 0..4 {
                let bx = (i % 2) * 4;
                let by = (i / 2) * 4;
                let off = (y / 2 + by) * rec.c_stride + x / 2 + bx;
                predictions[i] = transform::pixels(dp, rec.c_stride, off);
                let input = transform::residual(
                    sp,
                    job.source.c_stride,
                    (gy / 2 + by) * job.source.c_stride + x / 2 + bx,
                    &predictions[i],
                );
                // Constant chroma residuals have only DC; avoid both 4x4 passes.
                transformed[i] = if input.iter().all(|&v| v == input[0]) {
                    let mut dc = [0; 16];
                    dc[0] = input[0] * 16;
                    dc
                } else {
                    transform::forward(input)
                };
            }
            dc[plane] = transform::chroma_dc(transformed.map(|c| c[0])).map(|v| {
                transform::suppress(transform::quant(v, cqp, 0, intra, 1), job.suppression)
            });
            if dc[plane].iter().any(|&v| v != 0) {
                chroma_cbp = chroma_cbp.max(1);
            }
            let dci = transform::chroma_dc_inverse(dc[plane], cqp);
            for i in 0..4 {
                let mut dq = [0; 16];
                if transformed[i][1..].iter().any(|&v| v != 0) {
                    let (q, dequantized) = chroma_quantizer.apply(transformed[i]);
                    dq = dequantized;
                    for (j, &s) in SCAN.iter().enumerate().skip(1) {
                        cc[plane][i][j] = q[s];
                    }
                    if cc[plane][i][1..].iter().any(|&v| v != 0) {
                        chroma_cbp = 2;
                    }
                }
                dq[0] = dci[i];
                let r = transform::inverse(dq);
                let bx = (i % 2) * 4;
                let by = (i / 2) * 4;
                let pixels = transform::reconstruct(&predictions[i], r);
                for yy in 0..4 {
                    let off = (y / 2 + by + yy) * rec.c_stride + x / 2 + bx;
                    dp[off..off + 4].copy_from_slice(&pixels[yy * 4..yy * 4 + 4]);
                }
            }
        }
        cbp |= chroma_cbp * 16;
        if !job.idr && !intra && cbp == 0 && mv == skip_mv {
            skip_run += 1;
            skipped += 1;
            mbs[index].qp = qpy;
            qp_sum += qpy as u32;
            continue;
        }
        if !job.idr {
            b.ue(skip_run);
            skip_run = 0;
        }
        b.ue(if intra {
            let base = if intra16 {
                1 + best16.1 as u32 + 4 * chroma_cbp + if cbp & 15 != 0 { 12 } else { 0 }
            } else {
                0
            };
            base + if job.idr { 0 } else { 5 }
        } else {
            0
        });
        if intra {
            if !intra16 {
                for (mode, predicted) in modes {
                    b.bit(mode == predicted);
                    if mode != predicted {
                        b.put(if mode < predicted { mode } else { mode - 1 } as u32, 3);
                    }
                }
            }
            b.ue(cmode as u32);
        } else {
            b.se(mv.0 - pred.0);
            b.se(mv.1 - pred.1);
        }
        const INTRA: [u8; 48] = [
            47, 31, 15, 0, 23, 27, 29, 30, 7, 11, 13, 14, 39, 43, 45, 46, 16, 3, 5, 10, 12, 19, 21,
            26, 28, 35, 37, 42, 44, 1, 2, 4, 8, 17, 18, 20, 24, 6, 9, 22, 25, 32, 33, 34, 36, 40,
            38, 41,
        ];
        const INTER: [u8; 48] = [
            0, 16, 1, 2, 4, 8, 32, 3, 5, 10, 12, 15, 47, 7, 11, 13, 14, 6, 9, 31, 35, 37, 42, 44,
            33, 34, 36, 40, 39, 43, 45, 46, 17, 18, 20, 24, 19, 21, 26, 28, 23, 27, 29, 30, 22, 25,
            38, 41,
        ];
        let table = if intra { INTRA } else { INTER };
        if !intra16 {
            b.ue(table.iter().position(|&v| v as u32 == cbp).unwrap() as u32);
        }
        if cbp != 0 || intra16 {
            b.se(qp as i32 - qpy as i32);
            qpy = qp;
        }
        mbs[index].qp = qpy;
        qp_sum += qpy as u32;
        if intra16 {
            let nc = context(&mbs, w, index, 0, 0, 0);
            cavlc::write(&mut b, &ldc, nc);
        }
        for (i, c) in coeffs.iter().enumerate() {
            if cbp & (1 << (i / 4)) != 0 {
                let (bx, by) = pos(i);
                let nc = context(&mbs, w, index, bx / 4, by / 4, 0);
                let count = cavlc::write(&mut b, if intra16 { &c[1..] } else { c }, nc);
                mbs[index].nz[by / 4 * 4 + bx / 4] = count;
            }
        }
        if chroma_cbp > 0 {
            for d in &dc {
                cavlc::write(&mut b, d, -1);
            }
        }
        if chroma_cbp == 2 {
            for (plane, coeff) in cc.iter().enumerate() {
                for (i, c) in coeff.iter().enumerate() {
                    let nc = context(&mbs, w, index, i % 2, i / 2, plane + 1);
                    mbs[index].cnz[plane][i] = cavlc::write(&mut b, &c[1..], nc);
                }
            }
        }
    }
    if !job.idr && skip_run > 0 {
        b.ue(skip_run);
    }
    let mut nal = Vec::new();
    b.nal(if job.idr { 0x65 } else { 0x41 }, &mut nal);
    Band {
        first: job.first,
        picture: rec,
        mbs,
        nal,
        skipped,
        qp_sum,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn bounded_motion_cost_preserves_candidate_ordering() {
        let mut seed = 27u32;
        let mut src = Yuv420::new(64, 64);
        let mut reference = src.clone();
        for v in src.y.iter_mut().chain(reference.y.iter_mut()) {
            seed = seed.wrapping_mul(1664525).wrapping_add(1013904223);
            *v = (seed >> 16) as u8;
        }
        let hp = predict::compute_half_pel(&reference.y, 64, 64, 64);
        for x in [0, 16, 48] {
            for y in [0, 16, 48] {
                for mv in [
                    (0, 0),
                    (-16, 12),
                    (16, -12),
                    (-1, -1),
                    (1, 2),
                    (2, 2),
                    (3, 1),
                ] {
                    let expected = 7 * (signed_bits(mv.0 - 4) + signed_bits(mv.1 + 4))
                        + (0..16)
                            .flat_map(|yy| (0..16).map(move |xx| (xx, yy)))
                            .map(|(xx, yy)| {
                                src.y[(y + yy) * 64 + x + xx].abs_diff(predict::luma_mc(
                                    &reference.y,
                                    64,
                                    64,
                                    64,
                                    (x + xx) as i32,
                                    (y + yy) as i32,
                                    mv,
                                )) as u32
                            })
                            .sum::<u32>();
                    for limit in [0, expected / 2, expected, expected + 1, u32::MAX] {
                        let cost =
                            motion_cost(&src, &reference, Some(&hp), x, y, mv, (4, -4), 7, limit);
                        if expected < limit {
                            assert_eq!(cost, expected);
                        } else {
                            assert!(cost >= limit && cost <= expected);
                        }
                    }
                }
            }
        }
    }

    #[test]
    fn unrelated_motion_is_not_a_scroll() {
        let mut a = Yuv420::new(640, 368);
        let mut b = a.clone();
        let mut seed = 17u32;
        for pixel in a.y.iter_mut().chain(b.y.iter_mut()) {
            seed = seed.wrapping_mul(1664525).wrapping_add(1013904223);
            *pixel = (seed >> 16) as u8;
        }
        assert_eq!(scroll(&a, &b, 0, 23), (0, 0));
    }
    #[test]
    fn sixty_four_pixel_scrolls_are_exact_in_both_directions() {
        let mut old = Yuv420::new(640, 368);
        let mut seed = 7u32;
        for v in &mut old.y {
            seed ^= seed << 13;
            seed ^= seed >> 17;
            seed ^= seed << 5;
            *v = seed as u8;
        }
        for delta in [(0, 64), (64, 0), (0, -64), (-64, 0)] {
            let mut source = old.clone();
            for y in 0..368 {
                for x in 0..640 {
                    source.y[y * 640 + x] = predict::sample(
                        &old.y,
                        640,
                        640,
                        368,
                        x as i32 + delta.0,
                        y as i32 + delta.1,
                    ) as u8;
                }
            }
            let job = Job {
                source: Arc::new(source),
                previous: Some(Arc::new(old.clone())),
                reference: Arc::new(old.clone()),
                half_pel: None,
                quality: Arc::new(vec![18; 40 * 23]),
                first: 0,
                end: 23,
                qp: 30,
                suppression: 0,
                subpixel: false,
                is_motion: false,
                global: (0, 0),
                idr: false,
                frame_num: 1,
                idr_id: 0,
                refresh: 0,
                refresh_start: 0,
            };
            assert_eq!(
                scroll(
                    &job.source,
                    job.previous.as_ref().unwrap(),
                    job.first,
                    job.end
                ),
                (delta.0 * 4, delta.1 * 4)
            );
        }
    }
}
