//! The desktop encoder.

use super::CodecError;
use crate::Yuv420;

#[derive(Debug, Clone, PartialEq)]
pub struct EncoderConfig {
    pub width: u32,
    pub height: u32,
    pub fps: u32,
    pub bitrate_kbps: u32,
    /// Worker threads; 0 picks the number of CPUs (at most 8).
    pub threads: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct EncodedInfo {
    pub keyframe: bool,
    /// Average QP of the coded macroblocks.
    pub qp: u8,
    pub skipped_mbs: u32,
    pub total_mbs: u32,
    pub bytes: usize,
}

pub struct Encoder {}

impl Encoder {
    pub fn new(config: EncoderConfig) -> Result<Self, CodecError> {
        let _ = config;
        todo!()
    }

    /// Encodes `picture` (the configured visible size, padded) as one
    /// access unit appended to `out`. The first frame, and any frame with
    /// `keyframe`, is an IDR preceded by SPS and PPS.
    pub fn encode(
        &mut self,
        picture: &Yuv420,
        keyframe: bool,
        out: &mut Vec<u8>,
    ) -> Result<EncodedInfo, CodecError> {
        let _ = (picture, keyframe, out);
        todo!()
    }

    pub fn set_bitrate(&mut self, kbps: u32) {
        let _ = kbps;
        todo!()
    }

    /// The picture a decoder holds after the last frame.
    pub fn reconstruction(&self) -> &Yuv420 {
        todo!()
    }
}
