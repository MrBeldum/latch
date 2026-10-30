//! The look before encoding: each worker measures its own band of the new
//! picture (row hashes for the next frame's scroll search, the band's
//! scroll, how much changed, and a cost per macroblock row that predicts
//! how many bits the band will take), so the frame's quantizer can be
//! chosen before any macroblock is coded.
use super::predict::sad16x8;
use crate::Yuv420;
use std::sync::Arc;

pub(super) struct Job {
    pub source: Arc<Yuv420>,
    /// The previous source picture; `None` codes an IDR.
    pub previous: Option<Arc<Yuv420>>,
    /// Row hashes of `previous`, one per luma row.
    pub previous_hashes: Arc<Vec<u64>>,
    /// The previous frame's motion vector per macroblock (quarter samples).
    pub previous_mvs: Arc<Vec<(i32, i32)>>,
    /// The previous frame's most common nonzero motion vector.
    pub dominant: (i32, i32),
    pub first: usize,
    pub end: usize,
}

pub(super) struct Analysis {
    /// Row hashes of this band's luma rows.
    pub hashes: Vec<u64>,
    /// The band's scroll in quarter samples, `(0, 0)` if none.
    pub global: (i32, i32),
    /// Macroblocks whose centre sample changed.
    pub changed: usize,
    /// Per macroblock row: the intra cost estimate.
    pub intra: Vec<u32>,
    /// Per macroblock row: the cheaper of the intra and inter estimates
    /// (the intra estimate for an IDR).
    pub cost: Vec<u32>,
    /// Per macroblock of a P frame: the displacement that explained it
    /// best (quarter samples), a candidate for the motion search.
    pub mvs: Vec<(i32, i32)>,
}

/// FNV-1a over every 16th sample of a luma row.
pub(super) fn row_hash(p: &Yuv420, row: usize) -> u64 {
    p.y[row * p.y_stride..row * p.y_stride + p.padded_width()]
        .iter()
        .step_by(16)
        .fold(0xcbf29ce484222325u64, |h, &v| {
            (h ^ v as u64).wrapping_mul(0x100000001b3)
        })
}

pub(super) fn run(job: &Job) -> Analysis {
    let src = &*job.source;
    let width = src.padded_width();
    let height = src.padded_height();
    let w = width / 16;
    let hashes: Vec<u64> = (job.first * 16..job.end * 16)
        .map(|y| row_hash(src, y))
        .collect();
    let mut changed = 0;
    let global = match &job.previous {
        Some(old) if job.previous_hashes.len() == height => {
            for my in job.first..job.end {
                let y = my * 16 + 8;
                for mx in 0..w {
                    let x = mx * 16 + 8;
                    changed +=
                        usize::from(src.y[y * src.y_stride + x] != old.y[y * old.y_stride + x]);
                }
            }
            scroll(src, old, &hashes, &job.previous_hashes, job.first, job.end)
        }
        _ => (0, 0),
    };
    let mut intra = Vec::with_capacity(job.end - job.first);
    let mut cost = Vec::with_capacity(job.end - job.first);
    let mut mvs = Vec::with_capacity(if job.previous.is_some() {
        (job.end - job.first) * w
    } else {
        0
    });
    for my in job.first..job.end {
        let (mut row_intra, mut row_cost) = (0, 0);
        let mut left = (0, 0);
        for mx in 0..w {
            let (x, y) = (mx * 16, my * 16);
            let so = y * src.y_stride + x;
            // Neighbour differences on alternate rows: a cheap stand-in for
            // the intra residual. The last column compares with its left
            // neighbour instead of reading past the row.
            let vertical = sad16x8(
                &src.y,
                src.y_stride,
                so,
                &src.y,
                src.y_stride,
                so + src.y_stride,
            );
            let horizontal = if x + 17 <= src.y_stride {
                sad16x8(&src.y, src.y_stride, so, &src.y, src.y_stride, so + 1)
            } else if x > 0 {
                sad16x8(&src.y, src.y_stride, so, &src.y, src.y_stride, so - 1)
            } else {
                vertical
            };
            let gradient = (vertical + horizontal) / 2;
            row_intra += gradient;
            row_cost += match &job.previous {
                None => gradient,
                Some(old) => {
                    // The cheapest of a few likely displacements (whole
                    // samples): none, the band's scroll, what this block,
                    // the picture and the block to the left did.
                    let mv = job
                        .previous_mvs
                        .get(my * w + mx)
                        .copied()
                        .unwrap_or_default();
                    let mut best = (gradient, left);
                    for (dx, dy) in [
                        (0, 0),
                        (global.0 / 4, global.1 / 4),
                        (mv.0 / 4, mv.1 / 4),
                        (job.dominant.0 / 4, job.dominant.1 / 4),
                        left,
                    ] {
                        let (rx, ry) = (x as i32 + dx, y as i32 + dy);
                        if rx < 0 || ry < 0 || rx as usize + 16 > width || ry as usize + 16 > height
                        {
                            continue;
                        }
                        let ro = ry as usize * old.y_stride + rx as usize;
                        let sad = sad16x8(&src.y, src.y_stride, so, &old.y, old.y_stride, ro);
                        if sad < best.0 {
                            best = (sad, (dx, dy));
                            if sad == 0 {
                                break;
                            }
                        }
                    }
                    left = best.1;
                    mvs.push((best.1 .0 * 4, best.1 .1 * 4));
                    best.0
                }
            };
        }
        intra.push(row_intra);
        cost.push(row_cost);
    }
    Analysis {
        hashes,
        global,
        changed,
        intra,
        cost,
        mvs,
    }
}

/// A vertical scroll found by matching row hashes, else a horizontal one
/// found by sampled differences; `(0, 0)` when neither explains the band.
pub(super) fn scroll(
    src: &Yuv420,
    old: &Yuv420,
    hashes: &[u64],
    old_hashes: &[u64],
    first: usize,
    end: usize,
) -> (i32, i32) {
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
    let mut hist = [0u32; 257];
    for y in (start..end).step_by(8) {
        let h = hashes[y - start];
        for dy in -128i32..=128 {
            let yy = y as i32 + dy;
            if yy >= 0 && (yy as usize) < old_hashes.len() && old_hashes[yy as usize] == h {
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
    let columns = 128..width.saturating_sub(128);
    for dx in -128i32..=128 {
        let mut cost = 0u64;
        for y in (start..end).step_by(32) {
            let s = &src.y[y * src.y_stride..];
            let o = &old.y[y * old.y_stride..];
            for x in columns.clone().step_by(16) {
                cost += s[x].abs_diff(o[(x as i32 + dx) as usize]) as u64;
            }
        }
        if dx == 0 {
            zero_cost = cost;
            samples = ((end - start).div_ceil(32) * columns.clone().step_by(16).len()) as u64;
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

#[cfg(test)]
mod tests {
    use super::super::predict;
    use super::*;

    fn hashes(p: &Yuv420) -> Vec<u64> {
        (0..p.padded_height()).map(|y| row_hash(p, y)).collect()
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
        assert_eq!(scroll(&a, &b, &hashes(&a), &hashes(&b), 0, 23), (0, 0));
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
            assert_eq!(
                scroll(&source, &old, &hashes(&source), &hashes(&old), 0, 23),
                (delta.0 * 4, delta.1 * 4)
            );
            // The same answer per band, from band-local hashes.
            let job = Job {
                source: Arc::new(source.clone()),
                previous: Some(Arc::new(old.clone())),
                previous_hashes: Arc::new(hashes(&old)),
                previous_mvs: Arc::new(Vec::new()),
                dominant: (0, 0),
                first: 0,
                end: 23,
            };
            assert_eq!(run(&job).global, (delta.0 * 4, delta.1 * 4));
        }
    }
}
