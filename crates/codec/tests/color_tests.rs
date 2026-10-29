#![allow(clippy::chunks_exact_to_as_chunks)]

use latch_codec::color::{
    bgra_to_yuv420, bgra_to_yuv420_reference, bgra_to_yuv420_rows, nv12_to_yuv420, yuv420_to_nv12,
};
use latch_codec::yuv::Yuv420;

#[test]
fn test_black_white_primaries() {
    let w = 8u32;
    let h = 8u32;
    let stride = (w as usize) * 4;

    // Black (0, 0, 0)
    let black = vec![0u8; stride * (h as usize)];
    let mut dst = Yuv420::new(w, h);
    bgra_to_yuv420(&black, w, h, stride, &mut dst);
    assert_eq!(dst.y[0], 16);
    assert_eq!(dst.u[0], 128);
    assert_eq!(dst.v[0], 128);

    // White (255, 255, 255)
    let mut white = vec![255u8; stride * (h as usize)];
    for chunk in white.chunks_exact_mut(4) {
        chunk[3] = 255; // alpha
    }
    bgra_to_yuv420(&white, w, h, stride, &mut dst);
    assert_eq!(dst.y[0], 235);
    assert_eq!(dst.u[0], 128);
    assert_eq!(dst.v[0], 128);
}

#[test]
fn test_random_images_within_plus_minus_one() {
    let mut state: u64 = 0xdeadbeef12345678;
    let mut next_u8 = || {
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        state as u8
    };

    let w = 64u32;
    let h = 48u32;
    let stride = (w as usize) * 4 + 16; // Test with extra stride padding

    let mut src = vec![0u8; stride * (h as usize)];
    for y in 0..h as usize {
        for x in 0..w as usize {
            let offset = y * stride + x * 4;
            src[offset] = next_u8(); // B
            src[offset + 1] = next_u8(); // G
            src[offset + 2] = next_u8(); // R
            src[offset + 3] = 255;
        }
    }

    let mut dst_fast = Yuv420::new(w, h);
    let mut dst_ref = Yuv420::new(w, h);

    bgra_to_yuv420(&src, w, h, stride, &mut dst_fast);
    bgra_to_yuv420_reference(&src, w, h, stride, &mut dst_ref);

    // Verify Y within +/- 1 LSB
    for y in 0..h as usize {
        for x in 0..w as usize {
            let fast_val = dst_fast.y[y * dst_fast.y_stride + x] as i32;
            let ref_val = dst_ref.y[y * dst_ref.y_stride + x] as i32;
            assert!(
                (fast_val - ref_val).abs() <= 1,
                "Y mismatch at ({x}, {y}): fast={fast_val}, ref={ref_val}"
            );
        }
    }

    // Verify U within +/- 1 LSB
    let chroma_w = (w as usize).div_ceil(2);
    let chroma_h = (h as usize).div_ceil(2);
    for cy in 0..chroma_h {
        for cx in 0..chroma_w {
            let fast_u = dst_fast.u[cy * dst_fast.c_stride + cx] as i32;
            let ref_u = dst_ref.u[cy * dst_ref.c_stride + cx] as i32;
            assert!(
                (fast_u - ref_u).abs() <= 1,
                "U mismatch at ({cx}, {cy}): fast={fast_u}, ref={ref_u}"
            );

            let fast_v = dst_fast.v[cy * dst_fast.c_stride + cx] as i32;
            let ref_v = dst_ref.v[cy * dst_ref.c_stride + cx] as i32;
            assert!(
                (fast_v - ref_v).abs() <= 1,
                "V mismatch at ({cx}, {cy}): fast={fast_v}, ref={ref_v}"
            );
        }
    }
}

#[test]
fn test_rows_variant_equals_full_conversion() {
    let w = 32u32;
    let h = 32u32;
    let stride = (w as usize) * 4;

    let mut src = vec![0u8; stride * (h as usize)];
    for (i, byte) in src.iter_mut().enumerate() {
        *byte = (i % 256) as u8;
    }

    let mut dst_full = Yuv420::new(w, h);
    bgra_to_yuv420(&src, w, h, stride, &mut dst_full);

    let mut dst_chunked = Yuv420::new(w, h);
    // Convert in chunks of 8 rows
    for r in (0..h).step_by(8) {
        bgra_to_yuv420_rows(&src, w, h, stride, r..r + 8, &mut dst_chunked);
    }
    dst_chunked.pad_edges();

    assert_eq!(dst_full.y, dst_chunked.y);
    assert_eq!(dst_full.u, dst_chunked.u);
    assert_eq!(dst_full.v, dst_chunked.v);
}

#[test]
fn test_odd_strides_and_non_multiple_of_16() {
    let w = 19u32;
    let h = 13u32;
    let stride = (w as usize) * 4 + 7;

    let src = vec![120u8; stride * (h as usize)];
    let mut dst = Yuv420::new(w, h);
    bgra_to_yuv420(&src, w, h, stride, &mut dst);

    assert_eq!(dst.width, 19);
    assert_eq!(dst.height, 13);
    assert_eq!(dst.padded_width(), 32);
    assert_eq!(dst.padded_height(), 16);
}

#[test]
fn test_yuv420_to_nv12_round_trip() {
    let w = 40u32;
    let h = 30u32;
    let stride = (w as usize) * 4;

    let mut src = vec![0u8; stride * (h as usize)];
    for (i, byte) in src.iter_mut().enumerate() {
        *byte = ((i * 37) % 256) as u8;
    }

    let mut yuv_orig = Yuv420::new(w, h);
    bgra_to_yuv420(&src, w, h, stride, &mut yuv_orig);

    let mut nv12_y = Vec::new();
    let mut nv12_uv = Vec::new();
    let (y_stride, uv_stride) = yuv420_to_nv12(&yuv_orig, &mut nv12_y, &mut nv12_uv);

    let mut yuv_recovered = Yuv420::new(w, h);
    nv12_to_yuv420(
        &nv12_y,
        y_stride,
        &nv12_uv,
        uv_stride,
        w,
        h,
        &mut yuv_recovered,
    );

    // Visible pixels should match exactly
    for row in 0..h as usize {
        assert_eq!(
            &yuv_orig.y[row * yuv_orig.y_stride..row * yuv_orig.y_stride + (w as usize)],
            &yuv_recovered.y
                [row * yuv_recovered.y_stride..row * yuv_recovered.y_stride + (w as usize)]
        );
    }
    let chroma_w = (w as usize).div_ceil(2);
    let chroma_h = (h as usize).div_ceil(2);
    for cy in 0..chroma_h {
        assert_eq!(
            &yuv_orig.u[cy * yuv_orig.c_stride..cy * yuv_orig.c_stride + chroma_w],
            &yuv_recovered.u[cy * yuv_recovered.c_stride..cy * yuv_recovered.c_stride + chroma_w]
        );
        assert_eq!(
            &yuv_orig.v[cy * yuv_orig.c_stride..cy * yuv_orig.c_stride + chroma_w],
            &yuv_recovered.v[cy * yuv_recovered.c_stride..cy * yuv_recovered.c_stride + chroma_w]
        );
    }
}
