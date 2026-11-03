//! Pictures for hardware encoders that read NV12 from memory.

use latch_codec::Yuv420;

/// Writes `picture`'s visible area as NV12: `y` holds `height` rows of
/// `y_pitch` bytes, `uv` `height / 2` rows of `uv_pitch` bytes of U,V pairs.
pub fn write(picture: &Yuv420, y: &mut [u8], y_pitch: usize, uv: &mut [u8], uv_pitch: usize) {
    let (w, h) = (picture.width as usize, picture.height as usize);
    for (row, dst) in y.chunks_mut(y_pitch).take(h).enumerate() {
        let at = row * picture.y_stride;
        dst[..w].copy_from_slice(&picture.y[at..at + w]);
    }
    let cw = w.div_ceil(2);
    for (row, dst) in uv.chunks_mut(uv_pitch).take(h.div_ceil(2)).enumerate() {
        let at = row * picture.c_stride;
        let (u, v) = (&picture.u[at..at + cw], &picture.v[at..at + cw]);
        let pairs = dst[..cw * 2].as_chunks_mut::<2>().0;
        for (pair, (&u, &v)) in pairs.iter_mut().zip(u.iter().zip(v)) {
            pair[0] = u;
            pair[1] = v;
        }
    }
}

/// Bytes an NV12 picture of `height` rows of `pitch` bytes takes.
pub fn size(pitch: usize, height: usize) -> usize {
    pitch * height + pitch * height.div_ceil(2)
}

#[cfg(test)]
mod tests {
    #[test]
    fn interleaves_chroma_and_keeps_pitches() {
        let mut p = latch_codec::Yuv420::new(6, 4);
        for (i, v) in p.y.iter_mut().enumerate() {
            *v = i as u8;
        }
        p.u.fill(1);
        p.v.fill(2);
        let (pitch, height) = (8, 4);
        let mut buffer = vec![0xee; super::size(pitch, height)];
        let (y, uv) = buffer.split_at_mut(pitch * height);
        super::write(&p, y, pitch, uv, pitch);
        assert_eq!(&buffer[..6], &p.y[..6]);
        assert_eq!(&buffer[6..8], &[0xee; 2], "padding is left alone");
        assert_eq!(&buffer[8..14], &p.y[16..22]);
        assert_eq!(&buffer[32..40], &[1, 2, 1, 2, 1, 2, 0xee, 0xee]);
        assert_eq!(&buffer[40..46], &[1, 2, 1, 2, 1, 2]);
    }
}
