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
use std::ffi::OsStr;
use std::time::Duration;

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

/// Why a Wayland desktop cannot be shared, and what the owner can do.
const WAYLAND: &str = "this is a Wayland session, which Latch cannot share yet: \
    log out, choose an Xorg (X11) session at the login screen, and log in again";

/// Whether `$WAYLAND_DISPLAY` and `$XDG_SESSION_TYPE` describe a Wayland
/// session. Only a hint: a separate Xorg server on `$DISPLAY` still works.
fn wayland_session(display: Option<&OsStr>, session_type: Option<&OsStr>) -> bool {
    display.is_some_and(|d| !d.is_empty())
        || session_type.is_some_and(|t| t.eq_ignore_ascii_case("wayland"))
}

/// Whether these RandR output names are rootless Xwayland's, whose root
/// window holds no desktop: XWAYLAND0, XWAYLAND1, …
fn xwayland_outputs<'a>(mut names: impl Iterator<Item = &'a [u8]>) -> bool {
    names.any(|name| name.starts_with(b"XWAYLAND"))
}

const X11_SILENCE: Duration = Duration::from_secs(2);
mod x11;
pub use x11::run_helper;
#[cfg(test)]
use x11::Bounded;
use x11::{connect, wait_fd, X11};

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn recognises_wayland_sessions_and_xwayland_outputs() {
        let os = |s| Some(OsStr::new(s));
        assert!(wayland_session(os("wayland-0"), None));
        assert!(wayland_session(None, os("wayland")));
        assert!(wayland_session(os(""), os("Wayland")));
        assert!(!wayland_session(None, None));
        assert!(!wayland_session(os(""), os("x11")));
        let names: [&[u8]; 2] = [b"DUMMY0", b"XWAYLAND0"];
        assert!(xwayland_outputs(names.into_iter()));
        assert!(!xwayland_outputs(names[..1].iter().copied()));
        assert!(!xwayland_outputs(std::iter::empty()));
    }
}
