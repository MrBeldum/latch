//! A synthetic desktop for tests and measurements: known pixels a decoded
//! frame can be checked against, motion when asked, and a square that
//! changes colour when a key is pressed (the viewer times that change to
//! measure input to picture).

use crate::input::{DisplayRect, Event};
use crate::software::{Bgra, CpuCapture};
use std::time::Duration;

/// What the pattern draws.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Motion {
    /// Never changes, except for the input square.
    Still,
    /// A bar sweeps across every frame.
    Sweep,
    /// Everything changes every frame (noise): the worst case for an
    /// encoder.
    Noise,
}

pub struct Pattern {
    pub width: u32,
    pub height: u32,
    pub fps: u32,
    pub motion: Motion,
}

impl Pattern {
    pub fn new(width: u32, height: u32, fps: u32, motion: Motion) -> Self {
        Self {
            width,
            height,
            fps,
            motion,
        }
    }

    /// Feeds an input event in (the square at the top left flips between
    /// black and white on every key press).
    pub fn input(&mut self, event: &Event) {
        let _ = event;
        todo!()
    }

    /// The colour the 64×64 square at (0, 0) has right now (BGRA).
    pub fn square(&self) -> [u8; 4] {
        todo!()
    }
}

impl CpuCapture for Pattern {
    fn size(&self) -> (u32, u32) {
        (self.width, self.height)
    }

    fn grab(&mut self, frame: &mut Bgra, timeout: Duration) -> anyhow::Result<bool> {
        let _ = (frame, timeout);
        todo!()
    }

    fn display(&self) -> DisplayRect {
        DisplayRect {
            x: 0,
            y: 0,
            width: self.width,
            height: self.height,
        }
    }

    fn name(&self) -> String {
        "test pattern".into()
    }
}
