//! The host side of a Latch stream. [`server`] takes a session from the
//! control service and runs it: [`video`] (capture and encode),
//! [`audio`] (desktop sound), [`input`] (keyboard and mouse into the
//! desktop) and [`display`] (the desktop's size), each with one
//! implementation per platform in `mac/`, `windows/` and `linux/`.
//! [`software`] is the CPU path (any BGRA source through `latch-codec`),
//! [`pattern`] a synthetic desktop for tests and measurements.

pub mod audio;
#[cfg(any(windows, test))]
mod bitstream;
pub mod display;
pub mod input;
pub mod pattern;
pub mod priority;
pub mod server;
pub mod sockets;
pub mod software;
pub mod testmode;
pub mod video;

#[cfg(target_os = "linux")]
pub mod linux;
#[cfg(target_os = "macos")]
pub mod mac;
#[cfg(windows)]
pub mod windows;
