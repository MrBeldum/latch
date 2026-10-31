//! Fused integer matrix: identical coefficients, RGB averaging and rounding to the scalar path.
use super::*;
pub(super) fn rows(
    a: &[u8],
    b: &[u8],
    y0: &mut [u8],
    y1: &mut [u8],
    u: &mut [u8],
    v: &mut [u8],
) -> usize {
    #[cfg(target_arch = "aarch64")]
    {
        unsafe { neon(a, b, y0, y1, u, v) }
    }
    #[cfg(target_arch = "x86_64")]
    {
        if std::is_x86_feature_detected!("sse4.1") && std::is_x86_feature_detected!("ssse3") {
            return unsafe { x86(a, b, y0, y1, u, v) };
        }
        0
    }
    #[cfg(not(any(target_arch = "aarch64", target_arch = "x86_64")))]
    {
        let _ = (a, b, y0, y1, u, v);
        0
    }
}
#[cfg(target_arch = "aarch64")]
unsafe fn neon(
    a: &[u8],
    b: &[u8],
    y0: &mut [u8],
    y1: &mut [u8],
    u: &mut [u8],
    v: &mut [u8],
) -> usize {
    use std::arch::aarch64::*;
    unsafe fn matrix(
        r: uint16x4_t,
        g: uint16x4_t,
        b: uint16x4_t,
        c: [i32; 3],
        base: i32,
    ) -> uint16x4_t {
        let sum = vmlaq_n_s32(
            vmlaq_n_s32(
                vmulq_n_s32(vreinterpretq_s32_u32(vmovl_u16(r)), c[0]),
                vreinterpretq_s32_u32(vmovl_u16(g)),
                c[1],
            ),
            vreinterpretq_s32_u32(vmovl_u16(b)),
            c[2],
        );
        vqmovun_s32(vaddq_s32(
            vshrq_n_s32(vaddq_s32(sum, vdupq_n_s32(32768)), 16),
            vdupq_n_s32(base),
        ))
    }
    unsafe fn luma(p: uint8x8x4_t) -> uint8x8_t {
        let (r, g, b) = (vmovl_u8(p.2), vmovl_u8(p.1), vmovl_u8(p.0));
        vqmovn_u16(vcombine_u16(
            matrix(
                vget_low_u16(r),
                vget_low_u16(g),
                vget_low_u16(b),
                [Y_R, Y_G, Y_B],
                16,
            ),
            matrix(
                vget_high_u16(r),
                vget_high_u16(g),
                vget_high_u16(b),
                [Y_R, Y_G, Y_B],
                16,
            ),
        ))
    }
    unsafe fn average(a: uint8x8_t, b: uint8x8_t) -> uint16x4_t {
        let sum = vaddl_u8(a, b);
        vshrn_n_u32(vaddq_u32(vpaddlq_u16(sum), vdupq_n_u32(2)), 2)
    }
    let done = y0.len() / 8 * 8;
    for x in (0..done).step_by(8) {
        let p = vld4_u8(a.as_ptr().add(x * 4));
        let q = vld4_u8(b.as_ptr().add(x * 4));
        vst1_u8(y0.as_mut_ptr().add(x), luma(p));
        if !y1.is_empty() {
            vst1_u8(y1.as_mut_ptr().add(x), luma(q));
        }
        let (r, g, b) = (average(p.2, q.2), average(p.1, q.1), average(p.0, q.0));
        for (dst, c) in [(&mut *u, [CB_R, CB_G, CB_B]), (&mut *v, [CR_R, CR_G, CR_B])] {
            let value = matrix(r, g, b, c, 128);
            let packed = vqmovn_u16(vcombine_u16(value, value));
            dst[x / 2..x / 2 + 4]
                .copy_from_slice(&vget_lane_u32(vreinterpret_u32_u8(packed), 0).to_ne_bytes());
        }
    }
    done
}
#[cfg(target_arch = "x86_64")]
#[target_feature(enable = "ssse3,sse4.1")]
unsafe fn x86(
    a: &[u8],
    b: &[u8],
    y0: &mut [u8],
    y1: &mut [u8],
    u: &mut [u8],
    v: &mut [u8],
) -> usize {
    use std::arch::x86_64::*;
    #[target_feature(enable = "ssse3,sse4.1")]
    unsafe fn channels(p: __m128i) -> [__m128i; 3] {
        std::array::from_fn(|i| {
            _mm_cvtepu8_epi32(_mm_shuffle_epi8(
                p,
                _mm_setr_epi8(
                    i as i8,
                    (i + 4) as i8,
                    (i + 8) as i8,
                    (i + 12) as i8,
                    -1,
                    -1,
                    -1,
                    -1,
                    -1,
                    -1,
                    -1,
                    -1,
                    -1,
                    -1,
                    -1,
                    -1,
                ),
            ))
        })
    }
    #[target_feature(enable = "ssse3,sse4.1")]
    unsafe fn matrix(p: [__m128i; 3], c: [i32; 3], base: i32) -> i32 {
        let sum = _mm_add_epi32(
            _mm_add_epi32(
                _mm_mullo_epi32(p[2], _mm_set1_epi32(c[0])),
                _mm_mullo_epi32(p[1], _mm_set1_epi32(c[1])),
            ),
            _mm_mullo_epi32(p[0], _mm_set1_epi32(c[2])),
        );
        let value = _mm_add_epi32(
            _mm_srai_epi32(_mm_add_epi32(sum, _mm_set1_epi32(32768)), 16),
            _mm_set1_epi32(base),
        );
        let value = _mm_packus_epi32(value, value);
        _mm_cvtsi128_si32(_mm_packus_epi16(value, value))
    }
    let done = y0.len() / 4 * 4;
    for x in (0..done).step_by(4) {
        let p = channels(_mm_loadu_si128(a.as_ptr().add(x * 4).cast()));
        let q = channels(_mm_loadu_si128(b.as_ptr().add(x * 4).cast()));
        y0[x..x + 4].copy_from_slice(&matrix(p, [Y_R, Y_G, Y_B], 16).to_ne_bytes());
        if !y1.is_empty() {
            y1[x..x + 4].copy_from_slice(&matrix(q, [Y_R, Y_G, Y_B], 16).to_ne_bytes());
        }
        let avg = std::array::from_fn(|i| {
            let s = _mm_add_epi32(p[i], q[i]);
            _mm_srli_epi32(_mm_add_epi32(_mm_hadd_epi32(s, s), _mm_set1_epi32(2)), 2)
        });
        u[x / 2..x / 2 + 2]
            .copy_from_slice(&matrix(avg, [CB_R, CB_G, CB_B], 128).to_ne_bytes()[..2]);
        v[x / 2..x / 2 + 2]
            .copy_from_slice(&matrix(avg, [CR_R, CR_G, CR_B], 128).to_ne_bytes()[..2]);
    }
    done
}
