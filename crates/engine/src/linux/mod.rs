//! Linux (X11): MIT-SHM capture with the XFixes cursor, RandR sizing,
//! XTest input, PulseAudio/PipeWire monitor capture.

mod audio;
mod capture;
mod display;
mod input;

use crate::audio::AudioCapture;
use crate::input::InputSink;
use crate::video::{VideoConfig, VideoPipeline};
pub use capture::X11Capture;

pub fn open_video(config: &VideoConfig) -> anyhow::Result<Box<dyn VideoPipeline>> {
    Ok(Box::new(crate::software::SoftwarePipeline::new(
        X11Capture::new()?,
        config,
    )?))
}
pub fn open_audio() -> anyhow::Result<Box<dyn AudioCapture>> {
    Ok(Box::new(audio::PulseCapture::new()?))
}
pub fn open_input() -> anyhow::Result<Box<dyn InputSink>> {
    Ok(Box::new(input::X11Input::new()?))
}
pub fn prepare_display(
    width: u32,
    height: u32,
    fps: u32,
) -> anyhow::Result<Option<crate::display::Previous>> {
    display::prepare(width, height, fps)
}
pub fn restore_display(previous: &crate::display::Previous) -> anyhow::Result<()> {
    display::restore(previous)
}

fn wait_fd(fd: std::os::fd::RawFd, timeout: std::time::Duration) -> std::io::Result<()> {
    let deadline = std::time::Instant::now() + timeout;
    loop {
        let mut pollfd = libc::pollfd {
            fd,
            events: libc::POLLIN,
            revents: 0,
        };
        let remaining = deadline.saturating_duration_since(std::time::Instant::now());
        if remaining.is_zero() {
            return Ok(());
        }
        let millis = remaining.as_millis().max(1).min(i32::MAX as u128) as i32;
        // SAFETY: pollfd is a live, initialized object for the duration of poll.
        let result = unsafe { libc::poll(&mut pollfd, 1, millis) };
        if result >= 0 {
            return Ok(());
        }
        let error = std::io::Error::last_os_error();
        if error.kind() != std::io::ErrorKind::Interrupted {
            return Err(error);
        }
    }
}
