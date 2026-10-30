//! Audio packets and their parity. See `docs/PROTOCOL.md`, "Audio (3)".

use crate::wire::AudioPacket;
use std::time::Instant;

/// Packets per parity group.
pub const GROUP: u32 = 4;
/// Largest coded frame, leaving room for the audio header and parity's
/// two-byte payload length.
pub const MAX_PAYLOAD: usize = crate::MAX_DATAGRAM - 16 - 2;

/// Numbers encoded audio frames and sends a parity packet after every
/// [`GROUP`] of them.
pub struct AudioPacketizer {
    session: u32,
    seq: u32,
    parity: Vec<u8>,
}

impl AudioPacketizer {
    pub fn new(session: u32) -> Self {
        Self {
            session,
            seq: 0,
            parity: Vec::new(),
        }
    }

    /// Calls `emit` with the packet for `payload` and, when it ends a group,
    /// the group's parity packet after it. Returns false if `payload`
    /// exceeds [`MAX_PAYLOAD`]; emits nothing and leaves the sequence and
    /// parity unchanged in that case.
    pub fn packetize(&mut self, pts: u32, payload: &[u8], emit: impl FnMut(&[u8])) -> bool {
        if payload.len() > MAX_PAYLOAD {
            return false;
        }
        let mut emit = emit;
        let mut buf = Vec::new();
        crate::wire::write_audio(
            &mut buf,
            self.session,
            &AudioPacket {
                parity: false,
                seq: self.seq,
                pts,
                payload,
            },
        );
        emit(&buf);
        let mut coded = Vec::with_capacity(payload.len() + 2);
        coded.extend((payload.len() as u16).to_le_bytes());
        coded.extend(payload);
        self.parity.resize(self.parity.len().max(coded.len()), 0);
        for (a, b) in self.parity.iter_mut().zip(coded) {
            *a ^= b;
        }
        if self.seq % GROUP == GROUP - 1 {
            crate::wire::write_audio(
                &mut buf,
                self.session,
                &AudioPacket {
                    parity: true,
                    seq: self.seq.wrapping_sub(GROUP - 1),
                    pts,
                    payload: &self.parity,
                },
            );
            emit(&buf);
            self.parity.clear();
        }
        self.seq = self.seq.wrapping_add(1);
        true
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
/// end of its group has arrived. After a gap beyond the 64-packet window,
/// jumps to the arriving packet's group start and clears stale state,
/// rather than playing a long backlog of concealment or stalling forever.
pub struct AudioReceiver {
    session: u32,
    packet_us: u32,
    next: Option<u32>,
    frames: std::collections::HashMap<u32, (u32, Vec<u8>)>,
    parity: std::collections::HashMap<u32, Vec<u8>>,
    latest: Option<u32>,
    stats: (u64, u64, u64),
}

impl AudioReceiver {
    pub fn new(session: u32) -> Self {
        Self::with_format(session, 480, 48_000)
    }

    /// Packet duration from Accept; the legacy constructor retains 10 ms.
    pub fn with_format(session: u32, samples: u32, rate: u32) -> Self {
        Self {
            session,
            packet_us: (u64::from(samples) * 1_000_000 / u64::from(rate.max(1))) as u32,
            next: None,
            frames: Default::default(),
            parity: Default::default(),
            latest: None,
            stats: (0, 0, 0),
        }
    }

    pub fn push(&mut self, packet: &AudioPacket<'_>, now: Instant) {
        let _ = (now, self.session);
        if packet.parity && !packet.seq.is_multiple_of(GROUP) {
            return;
        }
        let mut next = *self.next.get_or_insert(packet.seq);
        if crate::video::before(packet.seq, next.wrapping_sub(GROUP - 1)) {
            return;
        }
        if crate::video::before(next, packet.seq) && packet.seq.wrapping_sub(next) > 64 {
            let group = packet.seq - packet.seq % GROUP;
            self.stats.2 += group.wrapping_sub(next) as u64;
            next = group;
            self.next = Some(next);
            self.frames.clear();
            self.parity.clear();
            self.latest = None;
        }
        if packet.parity {
            self.parity
                .entry(packet.seq)
                .or_insert_with(|| packet.payload.to_vec());
        } else {
            if crate::video::before(packet.seq, next) || packet.seq.wrapping_sub(next) > 64 {
                return;
            }
            if self.frames.contains_key(&packet.seq) {
                return;
            }
            self.frames
                .insert(packet.seq, (packet.pts, packet.payload.to_vec()));
            self.stats.0 += 1;
            if self
                .latest
                .is_none_or(|n| crate::video::before(n, packet.seq))
            {
                self.latest = Some(packet.seq);
            }
        }
        self.rebuild(packet.seq - packet.seq % GROUP);
    }

    fn rebuild(&mut self, group: u32) {
        let Some(parity) = self.parity.get(&group) else {
            return;
        };
        let missing: Vec<_> = (0..GROUP)
            .map(|i| group.wrapping_add(i))
            .filter(|s| !self.frames.contains_key(s))
            .collect();
        if missing.len() != 1 {
            return;
        }
        let mut data = parity.clone();
        for i in 0..GROUP {
            if let Some((_, payload)) = self.frames.get(&group.wrapping_add(i)) {
                let mut coded = Vec::new();
                coded.extend((payload.len() as u16).to_le_bytes());
                coded.extend(payload);
                for (a, b) in data.iter_mut().zip(coded) {
                    *a ^= b;
                }
            }
        }
        if data.len() < 2 {
            return;
        }
        let len = u16::from_le_bytes([data[0], data[1]]) as usize;
        if len > data.len() - 2 {
            return;
        }
        let seq = missing[0];
        let pts = (0..GROUP)
            .find_map(|i| {
                let n = group.wrapping_add(i);
                self.frames
                    .get(&n)
                    .map(|(p, _)| p.wrapping_add(seq.wrapping_sub(n).wrapping_mul(self.packet_us)))
            })
            .unwrap_or(0);
        self.frames.insert(seq, (pts, data[2..2 + len].to_vec()));
        self.stats.1 += 1;
    }
    /// The next packet in order, or the next one given up on.
    pub fn pop(&mut self) -> Option<AudioOut> {
        let seq = self.next?;
        self.frames
            .retain(|&n, _| !crate::video::before(n, seq.wrapping_sub(GROUP)));
        self.parity
            .retain(|&n, _| !crate::video::before(n, seq.wrapping_sub(GROUP)));
        let group = seq - seq % GROUP;
        if let Some((pts, payload)) = self.frames.get(&seq).cloned() {
            self.next = Some(seq.wrapping_add(1));
            return Some(AudioOut::Frame { seq, pts, payload });
        }
        if self.parity.contains_key(&group)
            || self
                .latest
                .is_some_and(|n| !crate::video::before(n, group.wrapping_add(GROUP + 1)))
        {
            self.next = Some(seq.wrapping_add(1));
            self.stats.2 += 1;
            return Some(AudioOut::Missing { seq });
        }
        None
    }

    /// Counters: packets received, rebuilt from parity, given up on.
    pub fn stats(&self) -> (u64, u64, u64) {
        self.stats
    }
}
