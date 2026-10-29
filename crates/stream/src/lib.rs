//! The viewer side of a Latch stream (`docs/PROTOCOL.md`): the session
//! with a host ([`session`]), video decoding ([`video`]) and audio playback
//! ([`audio`]). Decoding uses VideoToolbox on a Mac, Media Foundation on
//! Windows, and `latch-codec` wherever neither is there.

pub mod audio;
pub mod input;
pub mod session;
pub mod video;

pub use latch_proto::control::Codec;
pub use session::{Event, Input, Server, Session, Settings, Stats};
pub use video::{Frame, FrameSlot};
