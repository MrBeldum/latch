//! The viewer side of a Latch stream (`docs/PROTOCOL.md`): the session
//! with a host ([`session`]), video decoding ([`video`]), audio playback
//! ([`audio`]) and this machine's gamepads ([`gamepad`]). Decoding uses VideoToolbox on a Mac, Media Foundation on
//! Windows, and `latch-codec` wherever neither is there.

pub mod audio;
pub mod gamepad;
pub mod input;
#[path = "../../engine/src/priority.rs"]
mod priority;
pub mod session;
pub mod video;

pub use latch_proto::control::Codec;
pub use session::{Event, Input, LossEvent, RecoveryLog, Server, Session, Settings, Stats};
pub use video::{Frame, FrameSlot};
