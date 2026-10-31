//! `LATCH_TEST_PATTERN=still|sweep|noise` makes the engine stream the
//! synthetic desktop of [`crate::pattern`] instead of the screen, and feed
//! it the viewer's input. A host in this mode needs no screen, permission
//! or GPU, so a whole session can be checked anywhere: known pixels arrive,
//! and a key press shows up as a colour change the viewer can time.

use crate::input::{DisplayRect, Event, InputSink};
use crate::pattern::{Motion, Pattern};
use crate::software::{Bgra, CpuCapture, SoftwarePipeline};
use crate::video::{VideoConfig, VideoPipeline};
use parking_lot::Mutex;
use std::sync::Arc;
use std::time::{Duration, Instant};

/// The pattern the running test session draws, shared with its input.
static CURRENT: Mutex<Option<Arc<Mutex<Pattern>>>> = Mutex::new(None);

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

    /// Looks at the pattern once a frame interval, as a screen refreshes;
    /// sleeps outside the lock so input is never held up behind a frame.
    fn grab(&mut self, frame: &mut Bgra, timeout: Duration) -> anyhow::Result<bool> {
        let deadline = Instant::now() + timeout;
        loop {
            let now = Instant::now();
            // Refreshes on a fixed grid, as a display does, whenever it is
            // looked at.
            if now >= self.next {
                self.next += self.interval;
                if self.next <= now {
                    self.next = now + self.interval;
                }
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
