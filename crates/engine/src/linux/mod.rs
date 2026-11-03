//! Linux (X11): MIT-SHM capture with the XFixes cursor, RandR sizing,
//! XTest input, PulseAudio/PipeWire monitor capture, uinput gamepads.

mod audio;
mod capture;
mod display;
mod hardware;
mod input;
mod nv12;
mod nvenc;
mod uinput;
mod vaapi;

use crate::audio::AudioCapture;
use crate::input::InputSink;
use crate::video::{VideoConfig, VideoPipeline};
pub use capture::X11Capture;
use std::ffi::OsStr;
use std::time::Duration;

/// Query the root size through the same bounded initialization as capture.
pub fn capture_size() -> anyhow::Result<[u32; 2]> {
    use x11rb::connection::Connection;
    let (conn, screen) = connect(X11_SILENCE)?;
    let root = &conn.setup().roots[screen];
    Ok([root.width_in_pixels.into(), root.height_in_pixels.into()])
}

/// What this host encodes: hardware codecs found by [`hardware`], and
/// `latch-codec`'s Constrained Baseline.
pub fn encodable() -> Vec<crate::video::Codec> {
    hardware::encodable()
}

/// What the encoder for `codec` takes: the hardware's limits when it has
/// the codec, else the codec's level.
pub fn limits(codec: crate::video::Codec) -> crate::video::Limits {
    hardware::limits(codec).unwrap_or_else(|| crate::video::Limits::level(codec))
}

/// Whether a session in `codec` is encoded in hardware here.
pub fn hardware_encodes(codec: crate::video::Codec) -> bool {
    hardware::backend(codec).is_some()
}

/// Whether `codec` can stream `width` × `height`, and the desktop as it is
/// now (which the session sends when it does not resize to the viewer).
/// H.264 always can: the software encoder takes what hardware does not.
pub fn takes(codec: crate::video::Codec, width: u32, height: u32) -> bool {
    use crate::video::Codec;
    if matches!(codec, Codec::H264High | Codec::H264Baseline) {
        return true;
    }
    hardware::fits(codec, width, height)
        && capture_size().map_or(true, |[w, h]| hardware::fits(codec, w, h))
}

/// X11 capture into the hardware encoder for `config.codec` when there is
/// one that takes the desktop's size, else into `latch-codec`. H.264 goes
/// to `latch-codec` (Baseline, which every H.264 viewer decodes) when the
/// hardware encoder does not open, then or after a resize. HEVC or AV1,
/// which the viewer has been told, have no such fallback: the error is
/// [`Unencodable`](crate::video::Unencodable), and the next session
/// negotiates another codec.
pub fn open_video(config: &VideoConfig) -> anyhow::Result<Box<dyn VideoPipeline>> {
    use crate::software::{LatchEncoder, PictureEncoder, SoftwarePipeline};
    use crate::video::{Codec, Unencodable};
    let codec = config.codec;
    let h264 = matches!(codec, Codec::H264High | Codec::H264Baseline);
    let Some(backend) = hardware::backend(codec) else {
        anyhow::ensure!(
            h264,
            Unencodable(format!("this host has no {} encoder", codec.name()))
        );
        return Ok(Box::new(SoftwarePipeline::new(X11Capture::new()?, config)?));
    };
    // Opens the hardware encoder for each size the desktop takes; for
    // H.264, the software one where that fails.
    let open =
        move |width, height, config: &VideoConfig| -> anyhow::Result<Box<dyn PictureEncoder>> {
            let failure = if hardware::fits(codec, width, height) {
                match hardware::open(backend, codec, width, height, config) {
                    Ok(encoder) => return Ok(encoder),
                    Err(e) => {
                        hardware::failed(codec, &e);
                        e
                    }
                }
            } else {
                anyhow::anyhow!(
                    "the hardware {} encoder does not take {width} × {height}",
                    codec.name()
                )
            };
            if !h264 {
                return Err(Unencodable(format!("{failure:#}")).into());
            }
            tracing::info!("{failure:#}; encoding H.264 in software");
            Ok(Box::new(LatchEncoder::open(width, height, config)?))
        };
    Ok(Box::new(SoftwarePipeline::with_encoder(
        X11Capture::new()?,
        config,
        Box::new(open),
    )?))
}
pub fn open_audio() -> anyhow::Result<Box<dyn AudioCapture>> {
    Ok(Box::new(audio::PulseCapture::new()?))
}
pub fn open_input() -> anyhow::Result<Box<dyn InputSink>> {
    Ok(Box::new(input::X11Input::new()?))
}
pub fn open_pads(feedback: crate::gamepad::FeedbackFn) -> anyhow::Result<crate::gamepad::Opened> {
    Ok(crate::gamepad::Pads::opened(uinput::open()?, feedback))
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
