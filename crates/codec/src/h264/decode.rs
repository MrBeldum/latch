//! The Constrained Baseline decoder.

use super::CodecError;
use crate::Yuv420;

pub struct Decoder {}

impl Decoder {
    pub fn new() -> Self {
        Self {}
    }

    /// Decodes one access unit (Annex B; may hold AUD, SPS, PPS, SEI and
    /// every slice of one picture). Returns the decoded picture, cropped
    /// size in `width`/`height`, once the access unit held a whole picture.
    /// After an error, the next IDR access unit decodes normally.
    pub fn decode(&mut self, access_unit: &[u8]) -> Result<Option<&Yuv420>, CodecError> {
        let _ = access_unit;
        todo!()
    }

    /// Forgets every picture and parameter set.
    pub fn reset(&mut self) {
        todo!()
    }
}

impl Default for Decoder {
    fn default() -> Self {
        Self::new()
    }
}
