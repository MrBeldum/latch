//! What Latch nodes share: the control API they speak, the Tailscale CLI,
//! wake packets, config files, the icon.
//!
//! The picture passes through `latch-stream` and `latch-engine`; this
//! crate finds machines, wakes them, controls them, and fetches releases.

pub mod api;
pub mod config;
pub mod dates;
pub mod http;
pub mod icon;
pub mod legacy;
pub mod screens;
pub mod tailscale;
pub mod update;
pub mod wake;

pub const APP_NAME: &str = "Latch";
/// `Status::app` of a Latch node, so a scan can tell one from any other
/// service that answers on the control port.
pub const APP_ID: &str = "latch";
/// `Streamer::kind` of the built-in streaming engine.
pub const STREAMER_KIND: &str = "Latch";
/// TCP port of the host's control API, on loopback and on its Tailscale IP.
pub const CONTROL_PORT: u16 = 47850;
/// TCP port a Mac serves a Latch Host install from, on its Tailscale IP,
/// for a PC whose host is too old to take `/v1/update`.
pub const HANDOVER_PORT: u16 = 47851;
/// Latch 4 compatibility port; retained until the viewer migration lands.
pub const SUNSHINE_PORT: u16 = 47989;
