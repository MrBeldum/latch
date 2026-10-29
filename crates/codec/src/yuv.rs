//! The picture type both H.264 sides work on.

/// 8-bit YUV 4:2:0 with planes padded to whole macroblocks: the luma plane
/// is `y_stride × padded_height()` with `y_stride >= padded_width()`, each
/// chroma plane `c_stride × padded_height() / 2`. `width` and `height` are
/// the visible size (even numbers); the padding holds copies of the edge
/// pixels after [`Yuv420::pad_edges`].
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Yuv420 {
    pub width: u32,
    pub height: u32,
    pub y: Vec<u8>,
    pub u: Vec<u8>,
    pub v: Vec<u8>,
    pub y_stride: usize,
    pub c_stride: usize,
}

impl Yuv420 {
    /// A black (Y 16, U/V 128) picture of this visible size.
    pub fn new(width: u32, height: u32) -> Self {
        let pw = (width as usize).div_ceil(16) * 16;
        let ph = (height as usize).div_ceil(16) * 16;
        let cw = pw / 2;
        Self {
            width,
            height,
            y: vec![16; pw * ph],
            u: vec![128; cw * ph / 2],
            v: vec![128; cw * ph / 2],
            y_stride: pw,
            c_stride: cw,
        }
    }

    /// `width` rounded up to a multiple of 16.
    pub fn padded_width(&self) -> usize {
        (self.width as usize).div_ceil(16) * 16
    }

    /// `height` rounded up to a multiple of 16.
    pub fn padded_height(&self) -> usize {
        (self.height as usize).div_ceil(16) * 16
    }

    /// Copies the last visible column and row of each plane into the
    /// padding.
    pub fn pad_edges(&mut self) {
        let (w, h) = (self.width as usize, self.height as usize);
        let (pw, ph) = (self.padded_width(), self.padded_height());
        pad(&mut self.y, self.y_stride, w, h, pw, ph);
        let (cw, ch) = (w.div_ceil(2), h.div_ceil(2));
        pad(&mut self.u, self.c_stride, cw, ch, pw / 2, ph / 2);
        pad(&mut self.v, self.c_stride, cw, ch, pw / 2, ph / 2);
    }
}

/// Replicates the last of `w` columns to `pw` and the last of `h` rows to
/// `ph` in a plane of `stride` bytes per row.
fn pad(plane: &mut [u8], stride: usize, w: usize, h: usize, pw: usize, ph: usize) {
    if w == 0 || h == 0 {
        return;
    }
    if pw > w {
        for row in plane.chunks_mut(stride).take(h) {
            let edge = row[w - 1];
            row[w..pw].fill(edge);
        }
    }
    let (visible, padding) = plane.split_at_mut(h * stride);
    let last = &visible[(h - 1) * stride..(h - 1) * stride + pw];
    for row in padding.chunks_mut(stride).take(ph - h) {
        row[..pw].copy_from_slice(last);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn padding_copies_the_last_visible_column_and_row() {
        let mut p = Yuv420::new(18, 18);
        assert_eq!((p.padded_width(), p.padded_height()), (32, 32));
        assert_eq!((p.y_stride, p.c_stride), (32, 16));
        for (i, v) in p.y.iter_mut().enumerate() {
            *v = (i % 251) as u8;
        }
        let before = p.y.clone();
        p.pad_edges();
        for r in 0..18 {
            assert_eq!(&p.y[r * 32..r * 32 + 18], &before[r * 32..r * 32 + 18]);
            assert!(p.y[r * 32 + 18..r * 32 + 32]
                .iter()
                .all(|&v| v == before[r * 32 + 17]));
        }
        for r in 18..32 {
            assert_eq!(&p.y[r * 32..r * 32 + 32], &p.y[17 * 32..17 * 32 + 32]);
        }
        assert_eq!(p.u.len(), 16 * 16);
    }
}
