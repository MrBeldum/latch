//! macOS: ScreenCaptureKit capture into VideoToolbox, ScreenCaptureKit
//! audio, CoreGraphics input.

use crate::audio::AudioCapture;
use crate::input::InputSink;
use crate::video::{VideoConfig, VideoPipeline};

pub fn open_video(config: &VideoConfig) -> anyhow::Result<Box<dyn VideoPipeline>> {
    let _ = config;
    anyhow::bail!("not implemented yet")
}

pub fn open_audio() -> anyhow::Result<Box<dyn AudioCapture>> {
    anyhow::bail!("not implemented yet")
}

pub fn open_input() -> anyhow::Result<Box<dyn InputSink>> {
    anyhow::bail!("not implemented yet")
}

pub fn encodable() -> Vec<crate::video::Codec> {
    Vec::new()
}
