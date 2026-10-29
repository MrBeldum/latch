//! Audio packets and their parity. See `docs/PROTOCOL.md`, "Audio (3)".

use crate::wire::AudioPacket;
use std::time::Instant;

/// Packets per parity group.
pub const GROUP: u32 = 4;

/// Numbers encoded audio frames and sends a parity packet after every
/// [`GROUP`] of them.
pub struct AudioPacketizer {
    session: u32,
}

impl AudioPacketizer {
    pub fn new(session: u32) -> Self {
        Self { session }
    }

    /// Calls `emit` with the packet for `payload` and, when it ends a group,
    /// the group's parity packet after it.
    pub fn packetize(&mut self, pts: u32, payload: &[u8], emit: impl FnMut(&[u8])) {
        let _ = (pts, payload, emit, self.session);
        todo!()
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AudioOut {
    Frame {
        seq: u32,
        pts: u32,
        payload: Vec<u8>,
    },
    /// This packet is gone for good; the player conceals it.
    Missing { seq: u32 },
}

/// Puts audio packets in order and rebuilds a lost one from its group's
/// parity. A missing packet is given up on when its group's parity has
/// arrived without rebuilding it, or when a packet two or more past the
/// end of its group has arrived.
pub struct AudioReceiver {
    session: u32,
}

impl AudioReceiver {
    pub fn new(session: u32) -> Self {
        Self { session }
    }

    pub fn push(&mut self, packet: &AudioPacket<'_>, now: Instant) {
        let _ = (packet, now, self.session);
        todo!()
    }

    /// The next packet in order, or the next one given up on.
    pub fn pop(&mut self) -> Option<AudioOut> {
        todo!()
    }

    /// Counters: packets received, rebuilt from parity, given up on.
    pub fn stats(&self) -> (u64, u64, u64) {
        todo!()
    }
}
