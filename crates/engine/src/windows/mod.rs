//! Windows: DXGI Desktop Duplication, the Direct3D 11 video processor and
//! Media Foundation's H.264 encoder; WASAPI loopback; SendInput.

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

pub fn prepare_display(
    width: u32,
    height: u32,
    fps: u32,
) -> anyhow::Result<Option<crate::display::Previous>> {
    let _ = (width, height, fps);
    Ok(None)
}

pub fn restore_display(previous: &crate::display::Previous) -> anyhow::Result<()> {
    let _ = previous;
    Ok(())
}
