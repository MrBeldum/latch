//! Which presenter draws a stream: the native HDR layer (Mac EDR, Windows
//! HDR10) or the wgpu path in egui's own surface.
//!
//! The choice is made when a stream starts and when the window's display
//! changes, never frame by frame. A display that cannot be read for a
//! moment (a space changing, an occluded or minimised window, a mode
//! switch) is no change. A new display counts only once it has held for
//! [`SETTLE`], and the presenter switches for a display at most once per
//! [`SPACING`], so a window on the edge between two screens cannot make
//! the picture alternate between two surfaces, which is a flicker. A
//! native presenter that fails gives way to the wgpu path for the rest of
//! that stream (logged once).
use latch_proto::color::ColorDescription;
use std::collections::VecDeque;
use std::time::{Duration, Instant};

/// How long a new display must hold before the presenter is chosen again.
const SETTLE: Duration = Duration::from_millis(300);
/// The least time between two switches that a display change makes.
const SPACING: Duration = Duration::from_secs(1);

/// A stream, as far as presentation goes: its frame slot (one per session)
/// and its colour, which cannot change within a session.
pub(super) type Stream = (usize, ColorDescription);

pub(super) struct Presenter {
    /// "EDR" or "HDR10", for the log.
    native_name: &'static str,
    stream: Option<Stream>,
    /// The display the choice was made for; None until one was read.
    display: Option<u64>,
    /// Another display, and since when it has been seen without a break.
    moved: Option<(u64, Instant)>,
    native: bool,
    /// The native presenter failed on this stream and display.
    failed: bool,
    /// When the presenter switched, for the spacing and the log's rate.
    last: Option<Instant>,
    switches: VecDeque<Instant>,
    total: u64,
    /// Frames each way since `since`, for a debug line once a second.
    frames: [u32; 2],
    since: Instant,
}

impl Presenter {
    pub fn new(native_name: &'static str) -> Self {
        Self {
            native_name,
            stream: None,
            display: None,
            moved: None,
            native: false,
            failed: false,
            last: None,
            switches: VecDeque::new(),
            total: 0,
            frames: [0; 2],
            since: Instant::now(),
        }
    }

    /// Whether this frame of `stream` goes to the native presenter.
    /// `display` is the window's screen or output as a number, None when
    /// it cannot be read just now. `wanted` (the stream needs the native
    /// presenter and it is there) is consulted only when the stream starts
    /// or a new display has settled.
    pub fn pick(
        &mut self,
        stream: Stream,
        display: Option<u64>,
        wanted: impl FnOnce() -> bool,
    ) -> bool {
        self.pick_at(Instant::now(), stream, display, wanted)
    }

    fn pick_at(
        &mut self,
        now: Instant,
        stream: Stream,
        display: Option<u64>,
        wanted: impl FnOnce() -> bool,
    ) -> bool {
        if self.stream != Some(stream) {
            self.stream = Some(stream);
            self.display = display;
            self.moved = None;
            self.failed = false;
            let native = wanted();
            tracing::info!(
                "video presenter for this stream: {} ({}-bit {})",
                self.name(native),
                stream.1.bit_depth,
                if stream.1.is_hdr() { "HDR" } else { "SDR" }
            );
            self.set(now, native, "stream started");
        } else if let Some(display) = display {
            if Some(display) == self.display {
                // Back where the choice was made before the move settled.
                self.moved = None;
            } else {
                match self.moved {
                    Some((moved, seen)) if moved == display => {
                        let spaced = self.last.is_none_or(|t| now - t >= SPACING);
                        if now - seen >= SETTLE && spaced {
                            self.display = Some(display);
                            self.moved = None;
                            self.failed = false;
                            let native = wanted();
                            if native == self.native {
                                tracing::debug!(
                                    "video display changed; {} still draws",
                                    self.name(native)
                                );
                            }
                            self.set(now, native, "display changed");
                        }
                    }
                    _ => self.moved = Some((display, now)),
                }
            }
        }
        self.frames[usize::from(self.native)] += 1;
        if now.duration_since(self.since) >= Duration::from_secs(1) {
            tracing::debug!(
                "video presenter, last second: {} frames {}, {} wgpu",
                self.frames[1],
                self.native_name,
                self.frames[0]
            );
            self.frames = [0; 2];
            self.since = now;
        }
        self.native
    }

    /// The native presenter could not draw: the wgpu path takes the rest
    /// of this stream, until the display changes. At once, whatever the
    /// spacing: the native presenter has nothing on screen to keep.
    pub fn fail(&mut self, reason: &str) {
        self.fail_at(Instant::now(), reason);
    }

    fn fail_at(&mut self, now: Instant, reason: &str) {
        if !self.failed {
            tracing::warn!(
                "{} presentation failed ({reason}); drawing this stream with wgpu",
                self.native_name
            );
        }
        self.failed = true;
        // This frame goes to wgpu after all.
        if self.native {
            self.frames[1] = self.frames[1].saturating_sub(1);
            self.frames[0] += 1;
        }
        self.set(now, false, "native presentation failed");
    }

    fn name(&self, native: bool) -> &'static str {
        if native {
            self.native_name
        } else {
            "wgpu"
        }
    }

    fn set(&mut self, now: Instant, native: bool, why: &str) {
        let native = native && !self.failed;
        if native == self.native {
            return;
        }
        self.native = native;
        self.last = Some(now);
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

    /// Frames 10 ms apart from `start`, all of stream 1 (HDR) on the
    /// displays given, one per frame; the presenter each one went to.
    fn run(p: &mut Presenter, start: &mut Instant, displays: &[Option<u64>]) -> Vec<bool> {
        displays
            .iter()
            .map(|&d| {
                *start += Duration::from_millis(10);
                p.pick_at(*start, (1, hdr()), d, || true)
            })
            .collect()
    }

    #[test]
    fn a_failure_keeps_the_stream_on_wgpu_until_the_display_changes() {
        let mut p = Presenter::new("EDR");
        let mut t = Instant::now();
        assert!(p.pick_at(t, (1, hdr()), Some(7), || true));
        p.fail_at(t, "no drawable");
        // Every later frame stays on wgpu even though EDR would draw now,
        // including across a moment with no display (a space changing).
        let mut frames = vec![Some(7); 50];
        frames.extend([None; 50]);
        frames.extend([Some(7); 50]);
        assert!(run(&mut p, &mut t, &frames).iter().all(|n| !n));
        assert_eq!(p.switches(), 2);
        // The window moved to another screen and stayed: EDR is tried
        // again once that has held for the settling time.
        let tried = run(&mut p, &mut t, &[Some(8); 50]);
        assert_eq!(tried.iter().position(|n| *n), Some(30));
        assert!(tried[30..].iter().all(|n| *n));
        assert_eq!(p.switches(), 3);
    }

    #[test]
    fn a_display_that_comes_and_goes_never_switches() {
        let mut p = Presenter::new("HDR10");
        let mut t = Instant::now();
        // An HDR output, then a window on the edge of an SDR one: the
        // output it reports alternates, or cannot be read, frame by frame.
        assert!(p.pick_at(t, (1, hdr()), Some(1), || true));
        let frames: Vec<_> = (0..600)
            .map(|i| [Some(1), Some(2), None, Some(2), Some(3)][i % 5])
            .collect();
        let mut asked = 0;
        for &d in &frames {
            t += Duration::from_millis(10);
            assert!(p.pick_at(t, (1, hdr()), d, || {
                asked += 1;
                false
            }));
        }
        assert_eq!((asked, p.switches()), (0, 1));
        // Unreadable for seconds (minimised) is no change either.
        assert!(run(&mut p, &mut t, &[None; 600]).iter().all(|n| *n));
        assert_eq!(p.switches(), 1);
    }

    #[test]
    fn display_switches_are_at_least_a_second_apart() {
        let mut p = Presenter::new("EDR");
        let mut t = Instant::now();
        // Starts on an SDR screen (EDR not wanted), then settles on others
        // that alternate in what they want, each for 400 ms.
        assert!(!p.pick_at(t, (1, hdr()), Some(1), || false));
        let mut at = Vec::new();
        for screen in 2..12u64 {
            for _ in 0..40 {
                t += Duration::from_millis(10);
                let before = p.switches();
                p.pick_at(t, (1, hdr()), Some(screen), || screen % 2 == 0);
                if p.switches() != before {
                    at.push(t);
                }
            }
        }
        assert!(at.len() >= 3, "{at:?}");
        assert!(at.windows(2).all(|w| w[1] - w[0] >= SPACING), "{at:?}");
    }

    #[test]
    fn the_choice_is_made_once_per_stream() {
        let mut p = Presenter::new("EDR");
        let mut asked = 0;
        for _ in 0..10 {
            assert!(!p.pick((1, ColorDescription::default()), Some(7), || {
                asked += 1;
                false
            }));
        }
        assert_eq!(asked, 1);
        assert_eq!(p.switches(), 0);
        // A new session (frame slot) or colour is a new stream, chosen for
        // at once.
        assert!(p.pick((2, hdr()), Some(7), || true));
        assert!(!p.pick((3, ColorDescription::default()), Some(7), || false));
        assert_eq!(p.switches(), 2);
    }
}
