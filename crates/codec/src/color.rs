//! Colour conversion: BGRA (as screens and capture APIs hand it over) to
//! BT.709 limited-range YUV 4:2:0, which every Latch decoder and the
//! viewer's shader assume, and YUV 4:2:0 to NV12 for the renderer.
//!
//! Fixed-point arithmetic, written so the compiler vectorises it. The
//! result must equal [`bgra_to_yuv420_reference`] (floating point, rounded)
//! within ±1 everywhere.

use crate::Yuv420;

/// Converts a `width × height` BGRA image (`stride` bytes per row) into
/// `dst`, which must have that visible size, and pads `dst`'s edges.
/// Chroma is the average of each 2×2 block.
pub fn bgra_to_yuv420(src: &[u8], width: u32, height: u32, stride: usize, dst: &mut Yuv420) {
    let _ = (src, width, height, stride, dst);
    todo!()
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
    let _ = (src, width, height, stride, rows, dst);
    todo!()
}

/// The slow, obviously correct conversion the fast one is tested against.
pub fn bgra_to_yuv420_reference(
    src: &[u8],
    width: u32,
    height: u32,
    stride: usize,
    dst: &mut Yuv420,
) {
    let _ = (src, width, height, stride, dst);
    todo!()
}

/// Writes `src`'s visible area as NV12: `y` gets `width` bytes per row,
/// `uv` interleaved U,V pairs. Reuses the vectors' allocations. Returns
/// `(y_stride, uv_stride)`.
pub fn yuv420_to_nv12(src: &Yuv420, y: &mut Vec<u8>, uv: &mut Vec<u8>) -> (usize, usize) {
    let _ = (src, y, uv);
    todo!()
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
    let _ = (y, y_stride, uv, uv_stride, width, height, dst);
    todo!()
}
