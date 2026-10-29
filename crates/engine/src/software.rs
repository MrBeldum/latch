//! The CPU path: any source of BGRA pictures, converted and encoded by
//! `latch-codec`. Linux always takes it; Windows falls back to it.

use crate::input::DisplayRect;
use crate::video::{EncodedVideo, VideoConfig, VideoInfo, VideoPipeline};
use std::time::Duration;

/// A BGRA picture: `stride` bytes per row.
#[derive(Debug, Clone, Default)]
pub struct Bgra {
    pub width: u32,
    pub height: u32,
    pub stride: usize,
    pub data: Vec<u8>,
}

/// A screen the CPU can read.
pub trait CpuCapture: Send {
    /// The screen's size now.
    fn size(&self) -> (u32, u32);

    /// Waits up to `timeout` for the screen to change (a source that cannot
    /// tell waits one frame interval), then copies it into `frame` with the
    /// cursor drawn in. Returns false when it did not change.
    fn grab(&mut self, frame: &mut Bgra, timeout: Duration) -> anyhow::Result<bool>;

    fn display(&self) -> DisplayRect;

    /// For [`VideoInfo::encoder`]'s sake: "X11", "test pattern".
    fn name(&self) -> String;
}

/// [`CpuCapture`] → [`latch_codec::color`] → [`latch_codec::h264::Encoder`].
/// Sends the source's own size (no scaling); repeats the last picture as
/// an all-skip frame when idle; re-creates the encoder if the source's size
/// changes.
pub struct SoftwarePipeline<C: CpuCapture> {
    capture: C,
    config: VideoConfig,
}

impl<C: CpuCapture> SoftwarePipeline<C> {
    pub fn new(capture: C, config: &VideoConfig) -> anyhow::Result<Self> {
        let _ = (&capture, config);
        todo!()
    }
}

impl<C: CpuCapture> VideoPipeline for SoftwarePipeline<C> {
    fn info(&self) -> VideoInfo {
        let _ = (&self.capture, &self.config);
        todo!()
    }

    fn next(&mut self, keyframe: bool, idle: Duration) -> anyhow::Result<EncodedVideo> {
        let _ = (keyframe, idle);
        todo!()
    }

    fn set_bitrate(&mut self, kbps: u32) {
        let _ = kbps;
        todo!()
    }
}
