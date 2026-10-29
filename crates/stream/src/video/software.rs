//! H.264 Constrained Baseline in Rust (`latch-codec`), for viewers without
//! a platform decoder Latch uses: Linux, and Windows when Media Foundation
//! cannot start.

use super::{Decoder, Frame};
use anyhow::Result;

pub struct Software {
    decoder: latch_codec::h264::Decoder,
}

impl Software {
    pub fn new() -> Self {
        Self {
            decoder: latch_codec::h264::Decoder::new(),
        }
    }
}

impl Decoder for Software {
    fn decode(&mut self, annexb: &[u8], _idr: bool, spare: Option<Frame>) -> Result<Option<Frame>> {
        let Some(picture) = self.decoder.decode(annexb)? else {
            return Ok(None);
        };
        let mut frame = spare.unwrap_or_default();
        let (y_stride, uv_stride) =
            latch_codec::color::yuv420_to_nv12(picture, &mut frame.y, &mut frame.uv);
        frame.width = picture.width;
        frame.height = picture.height;
        frame.y_stride = y_stride;
        frame.uv_stride = uv_stride;
        frame.full_range = false;
        Ok(Some(frame))
    }

    fn name(&self) -> &'static str {
        "Latch H.264 (software)"
    }
}
