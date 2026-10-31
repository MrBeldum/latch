//! Latch's stream protocol, version 1 (`docs/PROTOCOL.md`), without I/O:
//! datagram layouts ([`wire`]), Reed–Solomon erasure coding ([`fec`]),
//! cutting frames into shards and putting them back together ([`video`]),
//! audio packets and their parity ([`audio`]), reliable input ([`input`]),
//! the TCP control messages ([`control`]), round-trip estimation ([`rtt`]),
//! adaptive bitrate ([`rate`])
//! and a network impairment simulator for tests ([`sim`]).
//!
//! Nothing here reads a clock: every function that needs the time takes
//! `now`, so tests run on simulated time.

pub mod audio;
pub mod base64;
pub mod control;
pub mod fec;
pub mod input;
pub mod rate;
pub mod rtt;
pub mod sim;
pub mod video;
pub mod wire;

/// The protocol version this crate speaks.
pub const PROTOCOL: u32 = 1;
/// Largest datagram either side sends.
pub const MAX_DATAGRAM: usize = 1200;
/// Bytes before every datagram's payload.
pub const HEADER_LEN: usize = 8;
/// Bytes before a video shard's data.
pub const VIDEO_HEADER_LEN: usize = 28;
/// Largest shard: what fits in [`MAX_DATAGRAM`] after the video header.
pub const MAX_SHARD_LEN: usize = MAX_DATAGRAM - VIDEO_HEADER_LEN;
/// Data shards per FEC block at most.
pub const MAX_BLOCK_DATA: usize = 64;
