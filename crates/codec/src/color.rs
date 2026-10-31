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

    let start_row = (rows.start as usize).min(h) & !1;
    let end_row = (rows.end as usize).min(h);
    let row_bytes = w * 4;

    for y in (start_row..end_row).step_by(2) {
        let src_row0_start = y * stride;
        let src_row1_start = if y + 1 < h {
            (y + 1) * stride
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

        let (top, bottom) = dst.y.split_at_mut((y + 1) * dst.y_stride);
        let y0 = &mut top[y * dst.y_stride..y * dst.y_stride + w];
        let y1 = if has_row1 { &mut bottom[..w] } else { &mut [] };
        let chroma_w = w.div_ceil(2);
        let chroma_y = y / 2;
        let u_dst = &mut dst.u[chroma_y * dst.c_stride..chroma_y * dst.c_stride + chroma_w];
        let v_dst = &mut dst.v[chroma_y * dst.c_stride..chroma_y * dst.c_stride + chroma_w];
        #[allow(unused_mut)]
        let mut done = 0;
        #[cfg(target_arch = "aarch64")]
        if SIMD {
            // All input/output slices are checked above; the kernel handles whole 8-pixel groups.
            done = unsafe { neon::rows(row0, row1, y0, y1, u_dst, v_dst) };
        }
        convert_y_row(&row0[done * 4..], &mut y0[done..]);
        if has_row1 {
            convert_y_row(&row1[done * 4..], &mut y1[done..]);
        }

        let pairs = w / 2;
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

        if !w.is_multiple_of(2) {
            let x = w - 1;
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
    let uv_stride = w;

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

// AArch64's baseline NEON processes both luma rows and their shared chroma in one read.
#[cfg(target_arch = "aarch64")]
mod neon {
    use super::*;
    use std::arch::aarch64::*;

    #[inline]
    unsafe fn matrix(
        r: int32x4_t,
        g: int32x4_t,
        b: int32x4_t,
        c: [i32; 3],
        offset: i32,
    ) -> int32x4_t {
        let sum = vmlaq_n_s32(vmlaq_n_s32(vmulq_n_s32(r, c[0]), g, c[1]), b, c[2]);
        vaddq_s32(vrshrq_n_s32::<16>(sum), vdupq_n_s32(offset))
    }

    #[inline]
    unsafe fn luma(p: uint8x8x4_t) -> uint8x8_t {
        let r = vreinterpretq_s16_u16(vmovl_u8(p.2));
        let g = vreinterpretq_s16_u16(vmovl_u8(p.1));
        let b = vreinterpretq_s16_u16(vmovl_u8(p.0));
        let lo = matrix(
            vmovl_s16(vget_low_s16(r)),
            vmovl_s16(vget_low_s16(g)),
            vmovl_s16(vget_low_s16(b)),
            [Y_R, Y_G, Y_B],
            16,
        );
        let hi = matrix(
            vmovl_s16(vget_high_s16(r)),
            vmovl_s16(vget_high_s16(g)),
            vmovl_s16(vget_high_s16(b)),
            [Y_R, Y_G, Y_B],
            16,
        );
        vqmovn_u16(vcombine_u16(vqmovun_s32(lo), vqmovun_s32(hi)))
    }

    #[inline]
    unsafe fn average(a: uint8x8_t, b: uint8x8_t) -> int32x4_t {
        let sum = vaddl_u8(a, b);
        let pairs = vpadd_u16(vget_low_u16(sum), vget_high_u16(sum));
        vreinterpretq_s32_u32(vmovl_u16(vrshr_n_u16::<2>(pairs)))
    }

    pub(super) unsafe fn rows(
        row0: &[u8],
        row1: &[u8],
        y0: &mut [u8],
        y1: &mut [u8],
        u: &mut [u8],
        v: &mut [u8],
    ) -> usize {
        let mut x = 0;
        while x + 8 <= y0.len() {
            let a = vld4_u8(row0.as_ptr().add(x * 4));
            let b = vld4_u8(row1.as_ptr().add(x * 4));
            vst1_u8(y0.as_mut_ptr().add(x), luma(a));
            if !y1.is_empty() {
                vst1_u8(y1.as_mut_ptr().add(x), luma(b));
            }
            let r = average(a.2, b.2);
            let g = average(a.1, b.1);
            let blue = average(a.0, b.0);
            let cb = matrix(r, g, blue, [CB_R, CB_G, CB_B], 128);
            let cr = matrix(r, g, blue, [CR_R, CR_G, CR_B], 128);
            let cbcr = vqmovn_u16(vcombine_u16(vqmovun_s32(cb), vqmovun_s32(cr)));
            let bytes = vget_lane_u64::<0>(vreinterpret_u64_u8(cbcr)).to_le_bytes();
            u[x / 2..x / 2 + 4].copy_from_slice(&bytes[..4]);
            v[x / 2..x / 2 + 4].copy_from_slice(&bytes[4..]);
            x += 8;
        }
        x
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn simd_matches_scalar_with_tails_strides_and_missing_rows() {
        let mut seed = 93u32;
        for w in [1, 2, 7, 8, 9, 15, 16, 17, 34, 130] {
            for h in [1, 2, 3, 18] {
                let stride = w as usize * 4 + 7;
                let src: Vec<_> = (0..stride * h as usize)
                    .map(|_| {
                        seed = seed.wrapping_mul(1664525).wrapping_add(1013904223);
                        (seed >> 16) as u8
                    })
                    .collect();
                for len in [src.len(), src.len() - stride + w as usize * 4 - 1] {
                    for rows in [0..h, 1..h, 0..1] {
                        let mut a = Yuv420::new(w, h);
                        let mut b = a.clone();
                        bgra_to_yuv420_rows_impl::<true>(
                            &src[..len],
                            w,
                            h,
                            stride,
                            rows.clone(),
                            &mut a,
                        );
                        bgra_to_yuv420_rows_impl::<false>(&src[..len], w, h, stride, rows, &mut b);
                        assert_eq!(a, b, "{w}x{h}, input bytes {len}");
                    }
                }
            }
        }
    }
}
