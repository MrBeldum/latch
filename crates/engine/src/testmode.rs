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
use std::time::Duration;

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
    Ok(Box::new(SoftwarePipeline::new(Shared(pattern), config)?))
}

pub fn open_input() -> anyhow::Result<Box<dyn InputSink>> {
    Ok(Box::new(PatternInput))
}

struct Shared(Arc<Mutex<Pattern>>);

impl CpuCapture for Shared {
    fn size(&self) -> (u32, u32) {
        self.0.lock().size()
    }

    fn grab(&mut self, frame: &mut Bgra, timeout: Duration) -> anyhow::Result<bool> {
        // The pattern paces itself by sleeping; do that outside the lock so
        // input is never held up behind a frame interval.
        let deadline = std::time::Instant::now() + timeout;
        loop {
            let changed = self.0.lock().grab(frame, Duration::ZERO)?;
            let now = std::time::Instant::now();
            if changed || now >= deadline {
                return Ok(changed);
            }
            std::thread::sleep((deadline - now).min(Duration::from_millis(2)));
        }
    }

    fn display(&self) -> DisplayRect {
        self.0.lock().display()
    }

    fn name(&self) -> String {
        "test pattern".into()
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
