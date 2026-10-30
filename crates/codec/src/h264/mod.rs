//! H.264 (ITU-T H.264 | ISO/IEC 14496-10), Constrained Baseline profile.
//!
//! [`Encoder`]: made for desktop pictures; see `encode.rs`'s documentation.
//! [`Decoder`]: decodes any Constrained Baseline stream (I and P slices,
//! CAVLC, any number of slices and reference frames, deblocking, frame
//! cropping); other profiles' tools are refused with
//! [`CodecError::Unsupported`], never mis-decoded.

mod decode;
mod encode;
mod loopfilter;
pub mod nal;

pub use decode::Decoder;
pub use encode::{EncodedInfo, Encoder, EncoderConfig};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CodecError {
    /// A valid stream that uses something outside Constrained Baseline.
    Unsupported(&'static str),
    /// Bytes that are not a valid stream.
    Invalid(&'static str),
    /// A P slice refers to a picture the decoder does not have (after loss,
    /// or a stream joined mid-way). Wait for an IDR.
    MissingReference,
    /// The encoder was asked for something it cannot do.
    Config(&'static str),
}

impl std::fmt::Display for CodecError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            CodecError::Unsupported(what) => write!(f, "unsupported H.264 feature: {what}"),
            CodecError::Invalid(what) => write!(f, "invalid H.264 stream: {what}"),
            CodecError::MissingReference => write!(f, "H.264 reference picture missing"),
            CodecError::Config(what) => write!(f, "H.264 encoder setting: {what}"),
        }
    }
}

impl std::error::Error for CodecError {}
