//! `lpcm-rice`: a lossless code for short frames of 16-bit stereo PCM.
//!
//! Each frame is coded on its own (a lost packet loses nothing else). The
//! format is defined in this module's documentation by the implementer and
//! is part of the protocol: `decode(encode(x)) == x` for every input, and
//! `decode` returns an error (never panics, never allocates more than
//! [`MAX_SAMPLES`] samples) on any other bytes.

/// Samples per channel in Latch's 10 ms frame at 48 kHz.
pub const FRAME_SAMPLES: usize = 480;
pub const CHANNELS: usize = 2;
/// Most samples per channel one frame may hold.
pub const MAX_SAMPLES: usize = 4096;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AudioError {
    Truncated,
    Invalid(&'static str),
}

impl std::fmt::Display for AudioError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            AudioError::Truncated => write!(f, "audio frame is truncated"),
            AudioError::Invalid(what) => write!(f, "audio frame is invalid: {what}"),
        }
    }
}

impl std::error::Error for AudioError {}

/// Codes interleaved stereo `pcm` (an even number of samples, at most
/// `2 * MAX_SAMPLES`), appending to `out`.
pub fn encode(pcm: &[i16], out: &mut Vec<u8>) {
    let _ = (pcm, out);
    todo!()
}

/// Decodes one frame into `pcm` (cleared first), interleaved stereo.
pub fn decode(data: &[u8], pcm: &mut Vec<i16>) -> Result<(), AudioError> {
    let _ = (data, pcm);
    todo!()
}
