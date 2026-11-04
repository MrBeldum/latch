//! Latch's codecs, in Rust and nothing else:
//!
//! * [`h264`]: an H.264 Constrained Baseline encoder made for desktops and
//!   a decoder for the Constrained Baseline streams every Latch host sends
//!   a viewer that asks for them;
//! * [`color`]: BGRA to BT.709 limited-range YUV 4:2:0, and YUV to NV12;
//! * [`audio`]: `lpcm-rice`, a lossless code for short frames of 48 kHz
//!   sound, stereo to 7.1.
//!
//! No `unsafe` outside `std::arch` SIMD paths that have a portable twin
//! the tests compare them against, apart from OS thread-priority calls.

#[cfg(not(test))]
#[path = "../../engine/src/priority.rs"]
mod priority;

pub mod audio;
pub mod color;
pub mod h264;
pub mod yuv;

pub use yuv::Yuv420;
