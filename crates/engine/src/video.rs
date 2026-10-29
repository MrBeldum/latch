//! Capture plus encode, as one pipeline per platform, so a platform can
//! keep pictures on the GPU from the screen to the encoder.

use crate::input::DisplayRect;
pub use latch_proto::control::Codec;
use std::time::{Duration, Instant};

#[derive(Debug, Clone, PartialEq)]
pub struct VideoConfig {
    /// The picture size to send. A pipeline that cannot scale sends the
    /// desktop's own size and says so in [`VideoInfo`].
    pub width: u32,
    pub height: u32,
    pub fps: u32,
    pub bitrate_kbps: u32,
    pub codec: Codec,
}

/// One encoded frame: an Annex B access unit (SPS/PPS, or VPS/SPS/PPS for
/// HEVC, in band before every keyframe).
#[derive(Debug, Clone)]
pub struct EncodedVideo {
    pub data: Vec<u8>,
    pub keyframe: bool,
    /// When the picture was captured.
    pub captured: Instant,
}

#[derive(Debug, Clone, PartialEq)]
pub struct VideoInfo {
    /// For people: "VideoToolbox HEVC", "Media Foundation H.264 (NVIDIA)",
    /// "Latch software H.264".
    pub encoder: String,
    /// Encoding runs on dedicated hardware.
    pub hardware: bool,
    pub codec: Codec,
    /// The picture size actually sent.
    pub width: u32,
    pub height: u32,
    /// The captured display, where absolute mouse positions land.
    pub display: DisplayRect,
}

pub trait VideoPipeline: Send {
    fn info(&self) -> VideoInfo;

    /// The next encoded frame: as soon as the screen changes (at most `fps`
    /// a second), or a repeat of the last picture once `idle` has passed
    /// without a change. With `keyframe`, the next frame is an IDR and comes
    /// within one frame interval even if nothing changed.
    fn next(&mut self, keyframe: bool, idle: Duration) -> anyhow::Result<EncodedVideo>;

    /// Takes effect from the next frame where the encoder allows.
    fn set_bitrate(&mut self, kbps: u32) {
        let _ = kbps;
    }
}

/// What this machine can encode, best first.
pub fn encodable() -> Vec<Codec> {
    if crate::testmode::motion().is_some() {
        return vec![Codec::H264Baseline];
    }
    #[cfg(target_os = "macos")]
    return crate::mac::encodable();
    #[cfg(windows)]
    return crate::windows::encodable();
    #[cfg(target_os = "linux")]
    return vec![Codec::H264Baseline];
}

/// Opens this platform's pipeline for `config`.
pub fn open(config: &VideoConfig) -> anyhow::Result<Box<dyn VideoPipeline>> {
    if let Some(motion) = crate::testmode::motion() {
        return crate::testmode::open_video(config, motion);
    }
    #[cfg(target_os = "macos")]
    return crate::mac::open_video(config);
    #[cfg(windows)]
    return crate::windows::open_video(config);
    #[cfg(target_os = "linux")]
    return crate::linux::open_video(config);
}
