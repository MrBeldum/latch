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
use std::io;
use std::os::fd::{AsRawFd, RawFd};
use std::time::{Duration, Instant};
use x11rb::rust_connection::{DefaultStream, PollMode, RustConnection, Stream};
use x11rb::utils::RawFdContainer;

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

/// How long an X server may stay silent while Latch waits on it. x11rb
/// otherwise waits for a reply forever, and a stalled server would hold
/// the video thread, and with it the session's stop and display restore.
const X11_SILENCE: Duration = Duration::from_secs(2);

/// x11rb's own stream, except that a wait for the server gives up after
/// `silence` without a byte. Every round trip waits here, so the request
/// then fails with a connection error instead of hanging.
struct Bounded {
    stream: DefaultStream,
    silence: Duration,
}

impl Stream for Bounded {
    fn poll(&self, mode: PollMode) -> io::Result<()> {
        let mut events = 0;
        if mode.readable() {
            events |= libc::POLLIN;
        }
        if mode.writable() {
            events |= libc::POLLOUT;
        }
        if wait_fd(self.stream.as_raw_fd(), events, self.silence)? {
            Ok(())
        } else {
            Err(io::Error::new(
                io::ErrorKind::TimedOut,
                format!("the X server did not answer for {:?}", self.silence),
            ))
        }
    }
    fn read(&self, buf: &mut [u8], fds: &mut Vec<RawFdContainer>) -> io::Result<usize> {
        self.stream.read(buf, fds)
    }
    fn write(&self, buf: &[u8], fds: &mut Vec<RawFdContainer>) -> io::Result<usize> {
        self.stream.write(buf, fds)
    }
    fn write_vectored(
        &self,
        bufs: &[io::IoSlice<'_>],
        fds: &mut Vec<RawFdContainer>,
    ) -> io::Result<usize> {
        self.stream.write_vectored(bufs, fds)
    }
}

impl AsRawFd for Bounded {
    fn as_raw_fd(&self) -> RawFd {
        self.stream.as_raw_fd()
    }
}

type X11 = RustConnection<Bounded>;

/// `x11rb::connect(None)` (`$DISPLAY`, `$XAUTHORITY`) through [`Bounded`].
fn connect(silence: Duration) -> anyhow::Result<(X11, usize)> {
    use x11rb::reexports::x11rb_protocol::{parse_display::parse_display, xauth::get_auth};
    let display = parse_display(None)?;
    let screen = usize::from(display.screen);
    let mut error = None;
    for address in display.connect_instruction() {
        match DefaultStream::connect(&address) {
            Ok((stream, (family, peer))) => {
                let (name, data) = get_auth(family, &peer, display.display)
                    .ok()
                    .flatten()
                    .unwrap_or_default();
                let stream = Bounded { stream, silence };
                let conn =
                    RustConnection::connect_to_stream_with_auth_info(stream, screen, name, data)?;
                return Ok((conn, screen));
            }
            Err(e) => error = Some(e),
        }
    }
    Err(error.map_or_else(
        || anyhow::anyhow!("no X11 address to connect to"),
        Into::into,
    ))
}

/// Waits up to `timeout` for `events` on `fd`; false when none came.
fn wait_fd(fd: RawFd, events: i16, timeout: Duration) -> io::Result<bool> {
    let deadline = Instant::now() + timeout;
    loop {
        let mut pollfd = libc::pollfd {
            fd,
            events,
            revents: 0,
        };
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return Ok(false);
        }
        let millis = remaining.as_millis().max(1).min(i32::MAX as u128) as i32;
        // SAFETY: pollfd is a live, initialized object for the duration of poll.
        let result = unsafe { libc::poll(&mut pollfd, 1, millis) };
        if result >= 0 {
            return Ok(result > 0);
        }
        let error = io::Error::last_os_error();
        if error.kind() != io::ErrorKind::Interrupted {
            return Err(error);
        }
    }
}

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
