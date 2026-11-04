//! `LATCH_TEST_PATTERN=still|sweep|noise` makes the engine stream the
//! synthetic desktop of [`crate::pattern`] instead of the screen, and feed
//! it the viewer's input. A host in this mode needs no screen, permission
//! or GPU, so a whole session can be checked anywhere: known pixels arrive,
//! and a key press shows up as a colour change the viewer can time.

use crate::gamepad::{Feedback, FeedbackFn, PadKind, PadState, VirtualBus};
use crate::input::{DisplayRect, Event, InputSink};
use crate::pattern::{Motion, Pattern};
use crate::software::{Bgra, CpuCapture, SoftwarePipeline};
use crate::video::{VideoConfig, VideoPipeline};
use parking_lot::Mutex;
use std::sync::Arc;
use std::time::{Duration, Instant};

/// The pattern the running test session draws, shared with its input.
/// One per process: the session that opened last takes all test-mode
/// input, so tests that stream in parallel must take turns.
static CURRENT: Mutex<Option<Arc<Mutex<Pattern>>>> = Mutex::new(None);

#[cfg(target_os = "macos")]
pub fn native_encoder() -> bool {
    std::env::var_os("LATCH_TEST_VIDEOTOOLBOX").is_some_and(|v| v == "1")
}

/// A permission-free native picture encoder for loss simulations and benchmarks.
#[cfg(target_os = "macos")]
pub fn videotoolbox_encoder(
    config: &VideoConfig,
) -> anyhow::Result<Box<dyn crate::software::PictureEncoder>> {
    Ok(Box::new(crate::mac::synthetic::Synthetic::open(
        config.width,
        config.height,
        config,
    )?))
}

/// The motion asked for in the environment, if test mode is on.
pub fn motion() -> Option<Motion> {
    match std::env::var("LATCH_TEST_PATTERN").ok()?.as_str() {
        "still" => Some(Motion::Still),
        "sweep" => Some(Motion::Sweep),
        "noise" => Some(Motion::Noise),
        _ => None,
    }
}

pub fn open_video(config: &VideoConfig, motion: Motion) -> anyhow::Result<Box<dyn VideoPipeline>> {
    let pattern = Arc::new(Mutex::new(Pattern::new(
        config.width,
        config.height,
        config.fps,
        motion,
    )));
    *CURRENT.lock() = Some(pattern.clone());
    let shared = Shared {
        pattern,
        interval: Duration::from_secs(1) / config.fps.max(1),
        next: Instant::now(),
    };
    #[cfg(target_os = "macos")]
    if native_encoder() {
        return Ok(Box::new(SoftwarePipeline::with_encoder(
            shared,
            config,
            Box::new(|w, h, config| {
                Ok(Box::new(crate::mac::synthetic::Synthetic::open(
                    w, h, config,
                )?))
            }),
        )?));
    }
    Ok(Box::new(SoftwarePipeline::new(shared, config)?))
}

pub fn open_input() -> anyhow::Result<Box<dyn InputSink>> {
    Ok(Box::new(PatternInput))
}

struct Shared {
    pattern: Arc<Mutex<Pattern>>,
    interval: Duration,
    next: Instant,
}

impl CpuCapture for Shared {
    fn size(&self) -> (u32, u32) {
        self.pattern.lock().size()
    }

    fn damage(&self) -> Option<Vec<crate::software::Rect>> {
        self.pattern.lock().damage()
    }

    /// Looks at the pattern once a frame interval, as a screen refreshes;
    /// sleeps outside the lock so input is never held up behind a frame.
    fn grab(&mut self, frame: &mut Bgra, timeout: Duration) -> anyhow::Result<bool> {
        let deadline = Instant::now() + timeout;
        loop {
            let now = Instant::now();
            // Refreshes on a fixed grid, as a display does, whenever it is
            // looked at.
            if now >= self.next {
                // Keep the synthetic display's refresh phase. Re-stamping now
                // adds scheduler wake latency on top of every refresh interval.
                let slots = now.duration_since(self.next).as_nanos() / self.interval.as_nanos() + 1;
                self.next += self.interval * slots.min(u32::MAX as u128) as u32;
                if self.pattern.lock().grab(frame, Duration::ZERO)? {
                    return Ok(true);
                }
            }
            if now >= deadline {
                return Ok(false);
            }
            std::thread::sleep((self.next.min(deadline) - now).min(Duration::from_millis(2)));
        }
    }

    fn display(&self) -> DisplayRect {
        self.pattern.lock().display()
    }

    fn name(&self) -> String {
        "test pattern".into()
    }

    fn set_cursor_drawn(&mut self, drawn: bool) -> bool {
        self.pattern.lock().set_cursor_drawn(drawn)
    }

    fn take_cursor(&mut self) -> Option<crate::video::CursorUpdate> {
        self.pattern.lock().take_cursor()
    }
}

struct PatternInput;

impl InputSink for PatternInput {
    fn apply(&mut self, event: &Event, _display: DisplayRect) -> anyhow::Result<()> {
        if let Some(p) = CURRENT.lock().as_ref() {
            p.lock().input(event);
        }
        Ok(())
    }

    fn release_all(&mut self) {}
}

/// `LATCH_TEST_AUDIO=clicks` (or `clicks:<ms>` for a device period other
/// than 10 ms) replaces the desktop's sound with a 1 ms click on every
/// quarter second of this machine's wall clock, delivered a device period
/// at a time as a loopback device delivers it. A viewer that notes the wall
/// clock when it plays each click (`LATCH_AUDIO_PROBE`) has measured the
/// whole path after capture, without a microphone.
pub fn open_audio() -> Option<anyhow::Result<Box<dyn crate::audio::AudioCapture>>> {
    let spec = std::env::var("LATCH_TEST_AUDIO").ok()?;
    let period_ms = match spec.split_once(':') {
        None if spec == "clicks" => 10,
        Some(("clicks", ms)) => match ms.parse::<u32>() {
            Ok(ms) if (1..=100).contains(&ms) => ms,
            _ => return Some(Err(anyhow::anyhow!("LATCH_TEST_AUDIO: bad period {ms:?}"))),
        },
        _ => return Some(Err(anyhow::anyhow!("LATCH_TEST_AUDIO: unknown {spec:?}"))),
    };
    Some(Ok(Box::new(Clicks {
        start: Instant::now(),
        produced: 0,
        period: crate::audio::RATE as u64 * u64::from(period_ms) / 1000,
        pending: std::collections::VecDeque::new(),
    })))
}

/// Click spacing on the wall clock, and how long each lasts.
pub const CLICK_EVERY_NS: u64 = 250_000_000;
const CLICK_NS: u64 = 1_000_000;

struct Clicks {
    start: Instant,
    /// Samples per channel made so far.
    produced: u64,
    /// Samples per channel per device period.
    period: u64,
    pending: std::collections::VecDeque<i16>,
}

impl crate::audio::AudioCapture for Clicks {
    fn read(
        &mut self,
        out: &mut [i16; crate::audio::FRAME * crate::audio::CHANNELS],
        timeout: Duration,
    ) -> anyhow::Result<bool> {
        use crate::audio::{CHANNELS, RATE};
        let deadline = Instant::now() + timeout;
        while self.pending.len() < out.len() {
            // A period is captured once its last sample has been played.
            let due = self.start
                + Duration::from_nanos((self.produced + self.period) * 1_000_000_000 / RATE as u64);
            let now = Instant::now();
            if due > deadline {
                std::thread::sleep(deadline.saturating_duration_since(now));
                return Ok(false);
            }
            std::thread::sleep(due.saturating_duration_since(now));
            // The wall clock read now, not once at the start: a clock step
            // (NTP) must move the clicks as it moves the viewer's probe.
            let wall = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos() as u64;
            let wall_at_due =
                wall - Instant::now().saturating_duration_since(due).as_nanos() as u64;
            for k in self.produced..self.produced + self.period {
                let before_due = (self.produced + self.period - k) * 1_000_000_000 / RATE as u64;
                let at = wall_at_due - before_due;
                let v = if at % CLICK_EVERY_NS < CLICK_NS {
                    16_000
                } else {
                    0
                };
                self.pending.extend([v; CHANNELS]);
            }
            self.produced += self.period;
        }
        let n = out.len();
        for (o, s) in out.iter_mut().zip(self.pending.drain(..n)) {
            *o = s;
        }
        Ok(true)
    }

    fn describe(&self) -> String {
        "test clicks".into()
    }
}

/// What the test session's virtual pads hold: kind and newest state per slot.
static PADS: Mutex<[Option<(PadKind, PadState)>; 16]> = Mutex::new([None; 16]);

/// The test session's virtual pads, slot by slot.
pub fn pads() -> [Option<(PadKind, PadState)>; 16] {
    *PADS.lock()
}

/// Virtual pads that only remember their state, and rumble back what the
/// triggers say: the large motor follows the right trigger, the small one
/// the left, so a viewer can time a whole round trip.
pub(crate) struct TestBus;

pub(crate) struct TestPad {
    slot: u8,
    feedback: FeedbackFn,
}

impl Drop for TestPad {
    fn drop(&mut self) {
        PADS.lock()[self.slot as usize] = None;
    }
}

impl VirtualBus for TestBus {
    type Pad = TestPad;
    const SLOTS: u8 = 16;

    fn kinds(&self) -> &'static [PadKind] {
        &[PadKind::Xbox, PadKind::PlayStation, PadKind::Generic]
    }

    fn plug(&mut self, slot: u8, kind: PadKind, feedback: FeedbackFn) -> anyhow::Result<TestPad> {
        PADS.lock()[slot as usize] = Some((kind, PadState::default()));
        Ok(TestPad { slot, feedback })
    }

    fn submit(&mut self, pad: &mut TestPad, state: &PadState) -> anyhow::Result<()> {
        if let Some((_, s)) = PADS.lock()[pad.slot as usize].as_mut() {
            *s = *state;
        }
        (pad.feedback)(
            pad.slot,
            Feedback {
                large: state.right_trigger,
                small: state.left_trigger,
                ..Default::default()
            },
        );
        Ok(())
    }
}
