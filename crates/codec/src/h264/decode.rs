//! A bounded, single-threaded Constrained Baseline decoder.
//!
//! Scalar reconstruction kernels are adapted from pith-h264 0.1.1;
//! see `decode/SOURCE.md` and `decode/LICENSE.pith-h264`.

#![forbid(unsafe_code)]

mod cavlc;
mod deblock;
mod dpb;
mod engine;
pub(super) mod golomb;
mod inter;
mod intra;
mod mb;
pub(super) mod nal;
pub(super) mod pps;
pub(super) mod slice;
pub(super) mod sps;
mod tables;
mod transform;

use super::CodecError;
use crate::Yuv420;

type Result<T> = std::result::Result<T, Error>;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum Error {
    BadValue(&'static str),
    Unsupported(&'static str),
    MissingReference,
}

impl Error {
    fn truncated(what: &'static str, _needed: usize, _remaining: usize) -> Self {
        Self::BadValue(what)
    }
    fn too_large(what: &'static str, _limit: usize) -> Self {
        Self::BadValue(what)
    }
}

impl From<Error> for CodecError {
    fn from(value: Error) -> Self {
        match value {
            Error::BadValue(s) => Self::Invalid(s),
            Error::Unsupported(s) => Self::Unsupported(s),
            Error::MissingReference => Self::MissingReference,
        }
    }
}

#[derive(Clone, Debug)]
struct Frame { picture: Yuv420, poc: i64, epoch: u64 }

#[derive(Clone, Copy)]
struct Limits {
    max_input: usize,
    max_frames: usize,
    max_refs: u32,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            max_input: 64 * 1024 * 1024,
            max_frames: 1,
            max_refs: 16,
        }
    }
}

pub struct Decoder {
    engine: engine::Decoder,
    output: Option<Yuv420>,
    poc: Option<(u64, i64)>,
}

impl Decoder {
    pub fn new() -> Self {
        Self { engine: engine::Decoder::new(Limits::default()), output: None, poc: None }
    }

    /// Decodes every slice in one Annex B access unit, then publishes the
    /// complete visible picture. An error invalidates references so loss
    /// cannot silently propagate into later P pictures.
    pub fn decode(&mut self, access_unit: &[u8]) -> std::result::Result<Option<&Yuv420>, CodecError> {
        if access_unit.len() > self.engine.max_input() {
            self.engine.invalidate();
            return Err(CodecError::Invalid("access unit exceeds 64 MiB"));
        }
        let frames = match self.engine.push_stream(access_unit) {
            Ok(frames) => frames,
            Err(error) => {
                self.engine.invalidate();
                return Err(error.into());
            }
        };
        self.output = frames.into_iter().next().map(|f| {
            self.poc = Some((f.epoch, f.poc));
            f.picture
        });
        Ok(self.output.as_ref())
    }

    /// POC of the last returned picture, for callers that elect to display
    /// reordered P-only test streams. Latch publishes pictures immediately.
    pub fn picture_order(&self) -> Option<(u64, i64)> { self.poc }

    /// Forgets parameter sets and reference pictures when switching streams.
    pub fn reset(&mut self) {
        *self = Self::new();
    }
}

impl Default for Decoder {
    fn default() -> Self { Self::new() }
}
