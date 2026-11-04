//! Which presenter draws a stream: the native HDR layer (Mac EDR, Windows
//! HDR10) or the wgpu path in egui's own surface.
//!
//! The choice is made when a stream starts and when the window's display
//! changes, never frame by frame. A native presenter that fails gives way
//! to the wgpu path for the rest of that stream (logged once), so the
//! picture can never alternate between two surfaces, which is a flicker.
use latch_proto::color::ColorDescription;
use std::collections::VecDeque;
use std::time::{Duration, Instant};

/// A stream, as far as presentation goes: its frame slot (one per session)
/// and its colour, which cannot change within a session.
pub(super) type Stream = (usize, ColorDescription);

pub(super) struct Presenter {
    /// "EDR" or "HDR10", for the log.
    native_name: &'static str,
    stream: Option<Stream>,
    display: u64,
    native: bool,
    /// The native presenter failed on this stream and display.
    failed: bool,
    /// When the presenter last switched, for the log's rate.
    switches: VecDeque<Instant>,
    total: u64,
}

impl Presenter {
    pub fn new(native_name: &'static str) -> Self {
        Self {
            native_name,
            stream: None,
            display: 0,
            native: false,
            failed: false,
            switches: VecDeque::new(),
            total: 0,
        }
    }

    /// Whether this frame of `stream` goes to the native presenter.
    /// `wanted` (the stream needs it and it is there) is consulted only
    /// when the stream starts or `display` (window and screen) changes.
    pub fn pick(&mut self, stream: Stream, display: u64, wanted: impl FnOnce() -> bool) -> bool {
        if self.stream != Some(stream) {
            self.stream = Some(stream);
            self.display = display;
            self.failed = false;
            let native = wanted();
            tracing::info!(
                "video presenter for this stream: {} ({}-bit {})",
                self.name(native),
                stream.1.bit_depth,
                if stream.1.is_hdr() { "HDR" } else { "SDR" }
            );
            self.set(native, "stream started");
        } else if self.display != display {
            self.display = display;
            self.failed = false;
            let native = wanted();
            self.set(native, "display changed");
        }
        self.native
    }

    /// The native presenter could not draw: the wgpu path takes the rest
    /// of this stream, until the display changes.
    pub fn fail(&mut self, reason: &str) {
        if !self.failed {
            tracing::warn!(
                "{} presentation failed ({reason}); drawing this stream with wgpu",
                self.native_name
            );
        }
        self.failed = true;
        self.set(false, "native presentation failed");
    }

    fn name(&self, native: bool) -> &'static str {
        if native {
            self.native_name
        } else {
            "wgpu"
        }
    }

    fn set(&mut self, native: bool, why: &str) {
        let native = native && !self.failed;
        if native == self.native {
            return;
        }
        self.native = native;
        let now = Instant::now();
        self.switches.push_back(now);
        while self
            .switches
            .front()
            .is_some_and(|t| now.duration_since(*t) > Duration::from_secs(1))
        {
            self.switches.pop_front();
        }
        self.total += 1;
        let recent = self.switches.len();
        if recent > 2 {
            tracing::warn!(
                "video presenter -> {} ({why}): {recent} switches in the last second",
                self.name(native)
            );
        } else {
            tracing::info!(
                "video presenter -> {} ({why}); switch {} this run, {recent} in the last second",
                self.name(native),
                self.total
            );
        }
    }

    /// Presenter switches since the window opened.
    #[cfg(test)]
    pub fn switches(&self) -> u64 {
        self.total
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use latch_proto::color::{ColorCapabilities, ColorDescription};

    fn hdr() -> ColorDescription {
        ColorDescription::negotiate(
            ColorCapabilities {
                ten_bit: true,
                hdr: true,
                peak_nits: 1000,
            },
            true,
            true,
        )
    }

    #[test]
    fn a_failure_keeps_the_stream_on_wgpu_until_the_display_changes() {
        let mut p = Presenter::new("EDR");
        assert!(p.pick((1, hdr()), 7, || true));
        p.fail("no drawable");
        // Every later frame stays on wgpu even though EDR would draw now.
        for _ in 0..100 {
            assert!(!p.pick((1, hdr()), 7, || true));
        }
        assert_eq!(p.switches(), 2);
        // The window moved to another screen: EDR is tried again.
        assert!(p.pick((1, hdr()), 8, || true));
        assert_eq!(p.switches(), 3);
    }

    #[test]
    fn the_choice_is_made_once_per_stream() {
        let mut p = Presenter::new("EDR");
        let mut asked = 0;
        for _ in 0..10 {
            assert!(!p.pick((1, ColorDescription::default()), 7, || {
                asked += 1;
                false
            }));
        }
        assert_eq!(asked, 1);
        assert_eq!(p.switches(), 0);
        // A new session (frame slot) or colour is a new stream.
        assert!(p.pick((2, hdr()), 7, || true));
        assert!(!p.pick((3, ColorDescription::default()), 7, || false));
        assert_eq!(p.switches(), 2);
    }
}
