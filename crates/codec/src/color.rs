#![allow(clippy::chunks_exact_to_as_chunks)]

//! Color conversion between BGRA and YUV 4:2:0 / NV12.
//!
//! All paths use the **ITU-R BT.709** standard with **limited range**
//! (studio swing):
//!
//! - Y: [16, 235]
//! - Cb / Cr: [16, 240]
//!
//! Fixed-point integer arithmetic scales the matrix coefficients by 65536
//! ($2^{16}$) with rounding (+32768) to match the reference BT.709 equations
//! within $\pm 1$ LSB.

use crate::yuv::Yuv420;
mod pool;
mod simd;

// BT.709 limited-range fixed-point coefficients scaled by 65536 (2^16):
// Y  = 16  + (219 / 255) * (0.2126 * R + 0.7152 * G + 0.0722 * B)
// Cb = 128 + (224 / 255) * (-0.2126 * R - 0.7152 * G + (1 - 0.0722) * B) / 1.8556
// Cr = 128 + (224 / 255) * ((1 - 0.2126) * R - 0.7152 * G - 0.0722 * B) / 1.5748

const Y_R: i32 = 11966;
const Y_G: i32 = 40254;
const Y_B: i32 = 4064;

const CB_R: i32 = -6596;
const CB_G: i32 = -22188;
const CB_B: i32 = 28784;

const CR_R: i32 = 28784;
const CR_G: i32 = -26145;
const CR_B: i32 = -2639;

#[inline(always)]
fn rgb_to_y_fast(r: i32, g: i32, b: i32) -> u8 {
    let y = 16 + ((Y_R * r + Y_G * g + Y_B * b + 32768) >> 16);
    y.clamp(16, 235) as u8
}

#[inline(always)]
fn rgb_to_cb_fast(r: i32, g: i32, b: i32) -> u8 {
    let cb = 128 + ((CB_R * r + CB_G * g + CB_B * b + 32768) >> 16);
    cb.clamp(16, 240) as u8
}

#[inline(always)]
fn rgb_to_cr_fast(r: i32, g: i32, b: i32) -> u8 {
    let cr = 128 + ((CR_R * r + CR_G * g + CR_B * b + 32768) >> 16);
    cr.clamp(16, 240) as u8
}

#[inline(always)]
fn convert_y_row(src_row: &[u8], dst_row: &mut [u8]) {
    for (chunk, dst) in src_row.chunks_exact(4).zip(dst_row.iter_mut()) {
        let b = chunk[0] as i32;
        let g = chunk[1] as i32;
        let r = chunk[2] as i32;
        *dst = rgb_to_y_fast(r, g, b);
    }
}

/// Converts a `width × height` BGRA image (`stride` bytes per row) into
/// `dst`, which must have that visible size, and pads `dst`'s edges.
/// Chroma is the average of each 2×2 block.
pub fn bgra_to_yuv420(src: &[u8], width: u32, height: u32, stride: usize, dst: &mut Yuv420) {
    if width == 0 || height == 0 {
        return;
    }
    if dst.width != width || dst.height != height {
        *dst = Yuv420::new(width, height);
    }
    bgra_to_yuv420_rows(src, width, height, stride, 0..height, dst);
    dst.pad_edges();
}

/// Only the rows `rows` (multiples of 2) of the picture, for capture paths
/// that know which rows changed.
pub fn bgra_to_yuv420_rows(
    src: &[u8],
    width: u32,
    height: u32,
    stride: usize,
    rows: std::ops::Range<u32>,
    dst: &mut Yuv420,
) {
    bgra_to_yuv420_rows_impl::<true>(src, width, height, stride, rows, dst);
}

/// A rectangle of pixels: columns `x..x + width`, rows `y..y + height`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Rect {
    pub x: u32,
    pub y: u32,
    pub width: u32,
    pub height: u32,
}

impl Rect {
    pub fn area(&self) -> u64 {
        u64::from(self.width) * u64::from(self.height)
    }
}

/// Converts the pixels of `rects` (widened to even edges and clipped to the
/// picture) with up to `threads` threads, and pads the edges. The rest of
/// `dst` must already hold the picture's conversion; a `dst` of another size
/// is replaced and converted whole. An empty `rects` converts nothing.
pub fn bgra_to_yuv420_rects(
    src: &[u8],
    width: u32,
    height: u32,
    stride: usize,
    rects: &[Rect],
    threads: usize,
    dst: &mut Yuv420,
) {
    let whole = [Rect {
        x: 0,
        y: 0,
        width,
        height,
    }];
    let rects = if dst.width != width || dst.height != height {
        *dst = Yuv420::new(width, height);
        &whole[..]
    } else {
        rects
    };
    let (w, h) = (width as usize, height as usize);
    if w == 0 || h == 0 {
        return;
    }
    // Columns and rows as even ranges inside the picture.
    let spans: Vec<_> = rects
        .iter()
        .filter_map(|r| {
            let x0 = (r.x as usize).min(w) & !1;
            let x1 = ((r.x as usize + r.width as usize + 1) & !1).min(w);
            let y0 = (r.y as usize).min(h) & !1;
            let y1 = (r.y as usize + r.height as usize).min(h);
            (x1 > x0 && y1 > y0).then_some((y0..y1, x0..x1))
        })
        .collect();
    let rows: usize = spans
        .iter()
        .map(|(rows, cols)| rows.len() * cols.len())
        .sum::<usize>()
        / w;
    // Waking a kept thread costs some microseconds: only for big areas.
    let threads = threads.clamp(1, 16).min(rows / 64).max(1);
    let (y_stride, c_stride) = (dst.y_stride, dst.c_stride);
    let pairs = h.div_ceil(2);
    let mut planes = Vec::with_capacity(threads);
    let (mut y_rest, mut u_rest, mut v_rest) = (&mut dst.y[..], &mut dst.u[..], &mut dst.v[..]);
    for i in 0..threads {
        let (first, end) = (i * pairs / threads, (i + 1) * pairs / threads);
        let n = end - first;
        let (yn, cn) = (
            (n * 2 * y_stride).min(y_rest.len()),
            (n * c_stride).min(u_rest.len()).min(v_rest.len()),
        );
        let (y, y_next) = std::mem::take(&mut y_rest).split_at_mut(yn);
        let (u, u_next) = std::mem::take(&mut u_rest).split_at_mut(cn);
        let (v, v_next) = std::mem::take(&mut v_rest).split_at_mut(cn);
        (y_rest, u_rest, v_rest) = (y_next, u_next, v_next);
        planes.push(Planes {
            y,
            u,
            v,
            y_stride,
            c_stride,
            row0: first * 2,
            rows: (first * 2)..(end * 2).min(h),
        });
    }
    let run = |p: &mut Planes| {
        for (rows, cols) in &spans {
            let rows = rows.start.max(p.rows.start)..rows.end.min(p.rows.end);
            if !rows.is_empty() {
                convert::<true>(src, w, h, stride, rows, cols.clone(), p);
            }
        }
    };
    // Each band is its own: the locks are never contended.
    let bands: Vec<_> = planes.into_iter().map(std::sync::Mutex::new).collect();
    pool::run(bands.len(), &|i| {
        run(&mut bands[i].lock().unwrap_or_else(|e| e.into_inner()))
    });
    drop(bands);
    dst.pad_edges();
}

/// Rows `rows` of a picture's planes; `y` starts at luma row `row0`, `u` and
/// `v` at chroma row `row0 / 2`.
struct Planes<'a> {
    y: &'a mut [u8],
    u: &'a mut [u8],
    v: &'a mut [u8],
    y_stride: usize,
    c_stride: usize,
    row0: usize,
    rows: std::ops::Range<usize>,
}

fn bgra_to_yuv420_rows_impl<const SIMD: bool>(
    src: &[u8],
    width: u32,
    height: u32,
    stride: usize,
    rows: std::ops::Range<u32>,
    dst: &mut Yuv420,
) {
    let w = width as usize;
    let h = height as usize;
    if w == 0 || h == 0 {
        return;
    }
    let (y_stride, c_stride) = (dst.y_stride, dst.c_stride);
    let mut planes = Planes {
        y: &mut dst.y,
        u: &mut dst.u,
        v: &mut dst.v,
        y_stride,
        c_stride,
        row0: 0,
        rows: 0..h,
    };
    convert::<SIMD>(
        src,
        w,
        h,
        stride,
        rows.start as usize..rows.end as usize,
        0..w,
        &mut planes,
    );
}

/// Converts columns `cols` (even start) of rows `rows` of a `w × h` picture.
fn convert<const SIMD: bool>(
    src: &[u8],
    w: usize,
    h: usize,
    stride: usize,
    rows: std::ops::Range<usize>,
    cols: std::ops::Range<usize>,
    dst: &mut Planes,
) {
    let start_row = rows.start.min(h) & !1;
    let end_row = rows.end.min(h);
    let x0 = cols.start & !1;
    let cw = cols.end.min(w).saturating_sub(x0);
    if cw == 0 {
        return;
    }
    let row_bytes = cw * 4;

    for y in (start_row..end_row).step_by(2) {
        let src_row0_start = y * stride + x0 * 4;
        let src_row1_start = if y + 1 < h {
            (y + 1) * stride + x0 * 4
        } else {
            src_row0_start
        };

        if src.len() < src_row0_start + row_bytes {
            break;
        }

        let row0 = &src[src_row0_start..src_row0_start + row_bytes];
        let has_row1 = y + 1 < h && src.len() >= src_row1_start + row_bytes;
        let row1 = if has_row1 {
            &src[src_row1_start..src_row1_start + row_bytes]
        } else {
            row0
        };

        let chroma_w = cw.div_ceil(2);
        let ly = y - dst.row0;
        let chroma_y = ly / 2;
        let c0 = chroma_y * dst.c_stride + x0 / 2;
        let u_dst = &mut dst.u[c0..c0 + chroma_w];
        let v_dst = &mut dst.v[c0..c0 + chroma_w];
        let (before, after) = dst.y.split_at_mut((ly + 1) * dst.y_stride);
        let y0 = &mut before[ly * dst.y_stride + x0..ly * dst.y_stride + x0 + cw];
        let y1 = if has_row1 {
            &mut after[x0..x0 + cw]
        } else {
            &mut []
        };
        let done = if SIMD {
            simd::rows(row0, row1, y0, y1, u_dst, v_dst)
        } else {
            0
        };
        convert_y_row(&row0[done * 4..], &mut y0[done..]);
        if has_row1 {
            convert_y_row(&row1[done * 4..], &mut y1[done..]);
        }

        let pairs = cw / 2;
        let mut row0_chunks = row0[done * 4..].chunks_exact(8);
        let mut row1_chunks = row1[done * 4..].chunks_exact(8);

        for cx in done / 2..pairs {
            if let (Some(c0), Some(c1)) = (row0_chunks.next(), row1_chunks.next()) {
                let b00 = c0[0] as i32;
                let g00 = c0[1] as i32;
                let r00 = c0[2] as i32;

                let b01 = c0[4] as i32;
                let g01 = c0[5] as i32;
                let r01 = c0[6] as i32;

                let b10 = c1[0] as i32;
                let g10 = c1[1] as i32;
                let r10 = c1[2] as i32;

                let b11 = c1[4] as i32;
                let g11 = c1[5] as i32;
                let r11 = c1[6] as i32;

                let r_avg = (r00 + r01 + r10 + r11 + 2) >> 2;
                let g_avg = (g00 + g01 + g10 + g11 + 2) >> 2;
                let b_avg = (b00 + b01 + b10 + b11 + 2) >> 2;

                u_dst[cx] = rgb_to_cb_fast(r_avg, g_avg, b_avg);
                v_dst[cx] = rgb_to_cr_fast(r_avg, g_avg, b_avg);
            }
        }

        if !cw.is_multiple_of(2) {
            let x = cw - 1;
            let off = x * 4;
            let b0 = row0[off] as i32;
            let g0 = row0[off + 1] as i32;
            let r0 = row0[off + 2] as i32;

            let (r1, g1, b1) = if has_row1 {
                (row1[off + 2] as i32, row1[off + 1] as i32, row1[off] as i32)
            } else {
                (r0, g0, b0)
            };

            let r_avg = (r0 + r1 + 1) >> 1;
            let g_avg = (g0 + g1 + 1) >> 1;
            let b_avg = (b0 + b1 + 1) >> 1;

            let cx = pairs;
            u_dst[cx] = rgb_to_cb_fast(r_avg, g_avg, b_avg);
            v_dst[cx] = rgb_to_cr_fast(r_avg, g_avg, b_avg);
        }
    }
}

/// Floating-point reference implementation of BT.709 conversion.
pub fn bgra_to_yuv420_reference(
    src: &[u8],
    width: u32,
    height: u32,
    stride: usize,
    dst: &mut Yuv420,
) {
    let w = width as usize;
    let h = height as usize;
    if w == 0 || h == 0 {
        return;
    }
    if dst.width != width || dst.height != height {
        *dst = Yuv420::new(width, height);
    }

    for y in 0..h {
        let row_start = y * stride;
        for x in 0..w {
            let offset = row_start + x * 4;
            let b = src[offset] as f64 / 255.0;
            let g = src[offset + 1] as f64 / 255.0;
            let r = src[offset + 2] as f64 / 255.0;

            let y_prime = 0.2126 * r + 0.7152 * g + 0.0722 * b;
            let y_out = (16.0 + 219.0 * y_prime).round().clamp(16.0, 235.0) as u8;
            dst.y[y * dst.y_stride + x] = y_out;
        }
    }

    let chroma_w = w.div_ceil(2);
    let chroma_h = h.div_ceil(2);

    for cy in 0..chroma_h {
        let y0 = cy * 2;
        let y1 = (y0 + 1).min(h - 1);
        let row0_start = y0 * stride;
        let row1_start = y1 * stride;

        for cx in 0..chroma_w {
            let x0 = cx * 2;
            let x1 = (x0 + 1).min(w - 1);

            let off00 = row0_start + x0 * 4;
            let off01 = row0_start + x1 * 4;
            let off10 = row1_start + x0 * 4;
            let off11 = row1_start + x1 * 4;

            let r_avg = (src[off00 + 2] as f64
                + src[off01 + 2] as f64
                + src[off10 + 2] as f64
                + src[off11 + 2] as f64)
                / (4.0 * 255.0);
            let g_avg = (src[off00 + 1] as f64
                + src[off01 + 1] as f64
                + src[off10 + 1] as f64
                + src[off11 + 1] as f64)
                / (4.0 * 255.0);
            let b_avg =
                (src[off00] as f64 + src[off01] as f64 + src[off10] as f64 + src[off11] as f64)
                    / (4.0 * 255.0);

            let y_prime_avg = 0.2126 * r_avg + 0.7152 * g_avg + 0.0722 * b_avg;
            let cb = (128.0 + 224.0 * (b_avg - y_prime_avg) / 1.8556)
                .round()
                .clamp(16.0, 240.0) as u8;
            let cr = (128.0 + 224.0 * (r_avg - y_prime_avg) / 1.5748)
                .round()
                .clamp(16.0, 240.0) as u8;

            dst.u[cy * dst.c_stride + cx] = cb;
            dst.v[cy * dst.c_stride + cx] = cr;
        }
    }

    dst.pad_edges();
}

/// Writes `src`'s visible area as NV12: `y` gets `width` bytes per row,
/// `uv` interleaved U,V pairs. Reuses the vectors' allocations. Returns
/// `(y_stride, uv_stride)`.
pub fn yuv420_to_nv12(src: &Yuv420, y: &mut Vec<u8>, uv: &mut Vec<u8>) -> (usize, usize) {
    let w = src.width as usize;
    let h = src.height as usize;
    let y_stride = w;
    let uv_stride = w.div_ceil(2) * 2;

    y.resize(y_stride * h, 0);
    for row in 0..h {
        let src_row = &src.y[row * src.y_stride..row * src.y_stride + w];
        let dst_row = &mut y[row * y_stride..row * y_stride + w];
        dst_row.copy_from_slice(src_row);
    }

    let chroma_w = w.div_ceil(2);
    let chroma_h = h.div_ceil(2);
    uv.resize(uv_stride * chroma_h, 0);

    for cy in 0..chroma_h {
        let u_row = &src.u[cy * src.c_stride..cy * src.c_stride + chroma_w];
        let v_row = &src.v[cy * src.c_stride..cy * src.c_stride + chroma_w];
        let dst_row = &mut uv[cy * uv_stride..cy * uv_stride + chroma_w * 2];

        for cx in 0..chroma_w {
            dst_row[cx * 2] = u_row[cx];
            dst_row[cx * 2 + 1] = v_row[cx];
        }
    }

    (y_stride, uv_stride)
}

/// NV12 (as a GPU readback gives it) to YUV 4:2:0, padding the edges.
pub fn nv12_to_yuv420(
    y: &[u8],
    y_stride: usize,
    uv: &[u8],
    uv_stride: usize,
    width: u32,
    height: u32,
    dst: &mut Yuv420,
) {
    let w = width as usize;
    let h = height as usize;
    if w == 0 || h == 0 {
        return;
    }
    if dst.width != width || dst.height != height {
        *dst = Yuv420::new(width, height);
    }

    for row in 0..h {
        let src_row = &y[row * y_stride..row * y_stride + w];
        let dst_row = &mut dst.y[row * dst.y_stride..row * dst.y_stride + w];
        dst_row.copy_from_slice(src_row);
    }

    let chroma_w = w.div_ceil(2);
    let chroma_h = h.div_ceil(2);

    for cy in 0..chroma_h {
        let src_row = &uv[cy * uv_stride..cy * uv_stride + chroma_w * 2];
        let u_dst = &mut dst.u[cy * dst.c_stride..cy * dst.c_stride + chroma_w];
        let v_dst = &mut dst.v[cy * dst.c_stride..cy * dst.c_stride + chroma_w];

        for cx in 0..chroma_w {
            u_dst[cx] = src_row[cx * 2];
            v_dst[cx] = src_row[cx * 2 + 1];
        }
    }

    dst.pad_edges();
}

#[cfg(test)]
mod simd_tests {
    use super::*;
    #[test]
    fn integer_simd_matches_scalar_with_tails_strides_and_missing_rows() {
        let mut rng = 42u32;
        for w in 1..66 {
            for h in [1, 2, 3, 18] {
                let stride = w as usize * 4 + 13;
                let mut src = vec![0; stride * h as usize];
                for x in &mut src {
                    rng ^= rng << 13;
                    rng ^= rng >> 17;
                    rng ^= rng << 5;
                    *x = rng as u8;
                }
                for missing in [false, true] {
                    let input = if missing {
                        &src[..src.len() - stride]
                    } else {
                        &src[..]
                    };
                    let mut actual = Yuv420::new(w, h);
                    let mut scalar = actual.clone();
                    bgra_to_yuv420_rows_impl::<true>(input, w, h, stride, 0..h, &mut actual);
                    bgra_to_yuv420_rows_impl::<false>(input, w, h, stride, 0..h, &mut scalar);
                    assert_eq!(actual, scalar, "{w}x{h} missing={missing}");
                }
            }
        }
    }
}

#[cfg(test)]
mod rect_tests {
    use super::*;
    #[test]
    fn rects_and_threads_match_a_whole_conversion() {
        let (w, h) = (322u32, 270u32);
        let stride = w as usize * 4 + 8;
        let mut rng = 7u32;
        let mut next = || {
            rng ^= rng << 13;
            rng ^= rng >> 17;
            rng ^= rng << 5;
            rng as u8
        };
        let old: Vec<u8> = (0..stride * h as usize).map(|_| next()).collect();
        let mut new = old.clone();
        let rects = [
            Rect {
                x: 3,
                y: 5,
                width: 40,
                height: 33,
            },
            Rect {
                x: 300,
                y: 200,
                width: 100,
                height: 100,
            },
            Rect {
                x: 0,
                y: 129,
                width: 322,
                height: 140,
            },
        ];
        for r in &rects {
            for y in r.y..(r.y + r.height).min(h) {
                for x in r.x..(r.x + r.width).min(w) {
                    new[y as usize * stride + x as usize * 4] = next();
                }
            }
        }
        let mut expected = Yuv420::new(w, h);
        bgra_to_yuv420(&new, w, h, stride, &mut expected);
        for threads in [1, 3, 8] {
            let mut actual = Yuv420::new(w, h);
            bgra_to_yuv420(&old, w, h, stride, &mut actual);
            bgra_to_yuv420_rects(&new, w, h, stride, &rects, threads, &mut actual);
            assert_eq!(actual, expected, "{threads} threads");
        }
        let mut fresh = Yuv420::default();
        bgra_to_yuv420_rects(&new, w, h, stride, &[], 4, &mut fresh);
        assert_eq!(fresh, expected, "a new picture is converted whole");
    }
}

#[cfg(test)]
mod nv12_tests {
    use super::*;

    #[test]
    fn odd_visible_sizes_preserve_the_last_chroma_pair() {
        for (w, h) in [(19, 13), (1, 1), (2, 2), (17, 16)] {
            let mut src = Yuv420::new(w, h);
            for (i, v) in src.y.iter_mut().enumerate() {
                *v = i as u8;
            }
            for (i, v) in src.u.iter_mut().enumerate() {
                *v = i as u8;
            }
            for (i, v) in src.v.iter_mut().enumerate() {
                *v = !(i as u8);
            }
            let (mut y, mut uv) = (vec![], vec![]);
            let (ys, uvs) = yuv420_to_nv12(&src, &mut y, &mut uv);
            assert_eq!(ys, w as usize);
            assert_eq!(uvs, (w as usize).div_ceil(2) * 2);
            for row in 0..h as usize {
                assert_eq!(
                    &y[row * ys..(row + 1) * ys],
                    &src.y[row * src.y_stride..row * src.y_stride + ys]
                );
            }
            for row in 0..(h as usize).div_ceil(2) {
                for x in 0..(w as usize).div_ceil(2) {
                    assert_eq!(uv[row * uvs + x * 2], src.u[row * src.c_stride + x]);
                    assert_eq!(uv[row * uvs + x * 2 + 1], src.v[row * src.c_stride + x]);
                }
            }
        }
    }
}
