//! UDP datagram parse and write functions, strict checks per protocol.
//! See `docs/PROTOCOL.md`, "UDP datagram format".

use crate::gamepad::{Ack, FeedbackUpdate, PadUpdate, ACK_LEN, FEEDBACK_LEN};
use crate::input::Event;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WireError {
    Length,
    Field(&'static str),
    UnknownKind(u8),
}

impl std::fmt::Display for WireError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            WireError::Length => write!(f, "bad datagram length"),
            WireError::Field(field) => write!(f, "invalid field: {field}"),
            WireError::UnknownKind(k) => write!(f, "unknown datagram kind: {k}"),
        }
    }
}

impl std::error::Error for WireError {}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct VideoShard<'a> {
    pub keyframe: bool,
    /// A recovery frame: how many frames back the one it predicts from
    /// is; 0 for every other frame. Only to a viewer that negotiated
    /// `reference_invalidation`.
    pub recovery: u16,
    pub host_delay: u16,
    pub frame: u32,
    pub frame_len: u32,
    pub shard: u16,
    pub block: u8,
    pub blocks: u8,
    pub data: u16,
    pub parity: u16,
    pub shard_len: u16,
    pub bytes: &'a [u8],
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AudioPacket<'a> {
    pub parity: bool,
    pub seq: u32,
    pub pts: u32,
    pub payload: &'a [u8],
}

pub struct InputEvents<'a> {
    slice: &'a [u8],
    count: usize,
}

impl<'a> InputEvents<'a> {
    pub fn count(&self) -> usize {
        self.count
    }

    pub fn iter(&self) -> impl Iterator<Item = Event> + 'a {
        let mut rest = self.slice;
        std::iter::from_fn(move || {
            let (event, len) = Event::read(rest)?;
            rest = &rest[len..];
            Some(event)
        })
    }
}

/// The snapshots of a `Pad` datagram, checked when it was parsed.
pub struct PadUpdates<'a> {
    slice: &'a [u8],
    count: usize,
}

impl<'a> PadUpdates<'a> {
    pub fn count(&self) -> usize {
        self.count
    }

    pub fn iter(&self) -> impl Iterator<Item = PadUpdate> + 'a {
        let mut rest = self.slice;
        std::iter::from_fn(move || {
            let (update, len) = PadUpdate::read(rest)?;
            rest = &rest[len..];
            Some(update)
        })
    }
}

/// The fixed-size entries of a `PadAck`, `PadFeedback` or `PadFeedbackAck`.
pub struct Entries<'a, T> {
    slice: &'a [u8],
    len: usize,
    read: fn(&[u8]) -> T,
}

impl<'a, T: 'a> Entries<'a, T> {
    pub fn count(&self) -> usize {
        self.slice.len() / self.len
    }

    pub fn iter(&self) -> impl Iterator<Item = T> + 'a {
        let read = self.read;
        self.slice.chunks_exact(self.len).map(read)
    }
}

fn read_ack(b: &[u8]) -> Ack {
    Ack {
        slot: b[0],
        seq: u32::from_le_bytes([b[1], b[2], b[3], b[4]]),
    }
}

fn read_feedback(b: &[u8]) -> FeedbackUpdate {
    FeedbackUpdate::read(b).expect("checked when parsed")
}

/// `RecoveryRequest` (13): see `docs/PROTOCOL.md`. Frame numbers wrap.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RecoveryRequest {
    /// The last frame the viewer's reassembler handed to its decoder,
    /// as in `KeyframeRequest`.
    pub last_delivered: u32,
    /// The lost frames, inclusive: the first is `last_delivered + 1`.
    pub first_invalid: u32,
    pub last_invalid: u32,
    /// +1 per loss the viewer asks about; a repeat keeps its number.
    pub id: u32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum RecoveryStatus {
    /// Received; the encoder will try.
    Pending = 0,
    /// `frame` is the recovery frame.
    Sent = 1,
    /// The encoder cannot: an IDR follows.
    Refused = 2,
}

/// `RecoveryAck` (14): the host's answer to a `RecoveryRequest`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RecoveryAck {
    pub id: u32,
    pub status: RecoveryStatus,
    /// The recovery frame's number when `Sent`, else 0.
    pub frame: u32,
}

pub enum Datagram<'a> {
    Hello {
        session: u32,
        token: Option<&'a [u8; 16]>,
    },
    Video {
        session: u32,
        shard: VideoShard<'a>,
    },
    Audio {
        session: u32,
        packet: AudioPacket<'a>,
    },
    Input {
        session: u32,
        first: u32,
        events: InputEvents<'a>,
    },
    InputAck {
        session: u32,
        next: u32,
    },
    KeyframeRequest {
        session: u32,
        last: u32,
    },
    /// Frames after `last_delivered` are lost: predict from it instead.
    RecoveryRequest {
        session: u32,
        request: RecoveryRequest,
    },
    RecoveryAck {
        session: u32,
        ack: RecoveryAck,
    },
    Ping {
        session: u32,
        time: u64,
    },
    Pong {
        session: u32,
        time: u64,
    },
    Pad {
        session: u32,
        updates: PadUpdates<'a>,
    },
    PadAck {
        session: u32,
        acks: Entries<'a, Ack>,
    },
    PadFeedback {
        session: u32,
        updates: Entries<'a, FeedbackUpdate>,
    },
    PadFeedbackAck {
        session: u32,
        acks: Entries<'a, Ack>,
    },
}

impl Datagram<'_> {
    pub fn session(&self) -> u32 {
        match self {
            Datagram::Hello { session, .. }
            | Datagram::Video { session, .. }
            | Datagram::Audio { session, .. }
            | Datagram::Input { session, .. }
            | Datagram::InputAck { session, .. }
            | Datagram::KeyframeRequest { session, .. }
            | Datagram::RecoveryRequest { session, .. }
            | Datagram::RecoveryAck { session, .. }
            | Datagram::Ping { session, .. }
            | Datagram::Pong { session, .. }
            | Datagram::Pad { session, .. }
            | Datagram::PadAck { session, .. }
            | Datagram::PadFeedback { session, .. }
            | Datagram::PadFeedbackAck { session, .. } => *session,
        }
    }
}

pub fn parse(buf: &[u8]) -> Result<Datagram<'_>, WireError> {
    if buf.len() < crate::HEADER_LEN || buf.len() > crate::MAX_DATAGRAM {
        return Err(WireError::Length);
    }
    let kind = buf[0];
    let flags = buf[1];
    let aux = u16::from_le_bytes([buf[2], buf[3]]);
    let session = u32::from_le_bytes([buf[4], buf[5], buf[6], buf[7]]);
    let u16at = |i| u16::from_le_bytes([buf[i], buf[i + 1]]);
    let u32at = |i| u32::from_le_bytes([buf[i], buf[i + 1], buf[i + 2], buf[i + 3]]);
    let u64at = |i| {
        u64::from_le_bytes([
            buf[i],
            buf[i + 1],
            buf[i + 2],
            buf[i + 3],
            buf[i + 4],
            buf[i + 5],
            buf[i + 6],
            buf[i + 7],
        ])
    };

    match kind {
        1 => {
            if buf.len() == 8 {
                Ok(Datagram::Hello {
                    session,
                    token: None,
                })
            } else if buf.len() == 24 {
                let token: &[u8; 16] = buf[8..24].try_into().map_err(|_| WireError::Length)?;
                Ok(Datagram::Hello {
                    session,
                    token: Some(token),
                })
            } else {
                Err(WireError::Length)
            }
        }
        2 => {
            if buf.len() < 28 {
                return Err(WireError::Length);
            }
            let shard_len = u16at(24);
            if shard_len == 0
                || shard_len as usize > crate::MAX_SHARD_LEN
                || buf.len() != 28 + shard_len as usize
            {
                return Err(WireError::Length);
            }
            let data = u16at(20);
            let parity = u16at(22);
            let shard = u16at(16);
            let block = buf[18];
            let blocks = buf[19];
            // Bit 1 marks a recovery frame, whose distance back to its
            // reference fills the otherwise reserved field.
            let recovery = u16at(26);
            if data == 0
                || data as u32 + parity as u32 > 255
                || shard as u32 >= data as u32 + parity as u32
                || blocks == 0
                || block >= blocks
                || flags & !3 != 0
                || (flags & 2 != 0) != (recovery != 0)
                || (flags & 3 == 3)
            {
                return Err(WireError::Field("video"));
            }
            Ok(Datagram::Video {
                session,
                shard: VideoShard {
                    keyframe: flags & 1 != 0,
                    recovery,
                    host_delay: aux,
                    frame: u32at(8),
                    frame_len: u32at(12),
                    shard,
                    block,
                    blocks,
                    data,
                    parity,
                    shard_len,
                    bytes: &buf[28..],
                },
            })
        }
        3 => {
            if buf.len() < 16 {
                return Err(WireError::Length);
            }
            Ok(Datagram::Audio {
                session,
                packet: AudioPacket {
                    parity: flags != 0,
                    seq: u32at(8),
                    pts: u32at(12),
                    payload: &buf[16..],
                },
            })
        }
        4 => {
            if buf.len() < 13 {
                return Err(WireError::Length);
            }
            let first = u32at(8);
            let count = buf[12] as usize;
            if count == 0 {
                return Err(WireError::Field("count"));
            }
            let mut rest = &buf[13..];
            for _ in 0..count {
                let (_, len) = Event::read(rest).ok_or(WireError::Field("event"))?;
                rest = &rest[len..];
            }
            if !rest.is_empty() {
                return Err(WireError::Length);
            }
            Ok(Datagram::Input {
                session,
                first,
                events: InputEvents {
                    slice: &buf[13..],
                    count,
                },
            })
        }
        5 => {
            if buf.len() != 12 {
                return Err(WireError::Length);
            }
            Ok(Datagram::InputAck {
                session,
                next: u32at(8),
            })
        }
        6 => {
            if buf.len() != 12 {
                return Err(WireError::Length);
            }
            Ok(Datagram::KeyframeRequest {
                session,
                last: u32at(8),
            })
        }
        13 => {
            if buf.len() != 24 {
                return Err(WireError::Length);
            }
            Ok(Datagram::RecoveryRequest {
                session,
                request: RecoveryRequest {
                    last_delivered: u32at(8),
                    first_invalid: u32at(12),
                    last_invalid: u32at(16),
                    id: u32at(20),
                },
            })
        }
        14 => {
            if buf.len() != 16 {
                return Err(WireError::Length);
            }
            let status = match flags {
                0 => RecoveryStatus::Pending,
                1 => RecoveryStatus::Sent,
                2 => RecoveryStatus::Refused,
                _ => return Err(WireError::Field("status")),
            };
            Ok(Datagram::RecoveryAck {
                session,
                ack: RecoveryAck {
                    id: u32at(8),
                    status,
                    frame: u32at(12),
                },
            })
        }
        7 => {
            if buf.len() != 16 {
                return Err(WireError::Length);
            }
            Ok(Datagram::Ping {
                session,
                time: u64at(8),
            })
        }
        8 => {
            if buf.len() != 16 {
                return Err(WireError::Length);
            }
            Ok(Datagram::Pong {
                session,
                time: u64at(8),
            })
        }
        9 => {
            let count = *buf.get(8).ok_or(WireError::Length)? as usize;
            if count == 0 || count > crate::gamepad::MAX_PADS {
                return Err(WireError::Field("count"));
            }
            let mut rest = &buf[9..];
            for _ in 0..count {
                let (_, len) = PadUpdate::read(rest).ok_or(WireError::Field("pad"))?;
                rest = &rest[len..];
            }
            if !rest.is_empty() {
                return Err(WireError::Length);
            }
            Ok(Datagram::Pad {
                session,
                updates: PadUpdates {
                    slice: &buf[9..],
                    count,
                },
            })
        }
        10 | 12 => {
            let acks = entries(buf, ACK_LEN, read_ack)?;
            if acks
                .iter()
                .any(|a| a.slot as usize >= crate::gamepad::MAX_PADS)
            {
                return Err(WireError::Field("slot"));
            }
            Ok(if kind == 10 {
                Datagram::PadAck { session, acks }
            } else {
                Datagram::PadFeedbackAck { session, acks }
            })
        }
        11 => {
            let updates = entries(buf, FEEDBACK_LEN, read_feedback)?;
            if buf[9..]
                .as_chunks::<FEEDBACK_LEN>()
                .0
                .iter()
                .any(|c| FeedbackUpdate::read(c).is_none())
            {
                return Err(WireError::Field("feedback"));
            }
            Ok(Datagram::PadFeedback { session, updates })
        }
        k => Err(WireError::UnknownKind(k)),
    }
}

/// `u8 count` at offset 8, then exactly `count` entries of `len` bytes.
fn entries<'a, T>(
    buf: &'a [u8],
    len: usize,
    read: fn(&[u8]) -> T,
) -> Result<Entries<'a, T>, WireError> {
    let count = *buf.get(8).ok_or(WireError::Length)? as usize;
    if count == 0 || count > crate::gamepad::MAX_PADS {
        return Err(WireError::Field("count"));
    }
    if buf.len() != 9 + count * len {
        return Err(WireError::Length);
    }
    Ok(Entries {
        slice: &buf[9..],
        len,
        read,
    })
}

fn header(out: &mut Vec<u8>, kind: u8, flags: u8, aux: u16, session: u32) {
    out.clear();
    out.push(kind);
    out.push(flags);
    out.extend(aux.to_le_bytes());
    out.extend(session.to_le_bytes());
}

pub fn write_hello(out: &mut Vec<u8>, session: u32, token: Option<&[u8; 16]>) {
    header(out, 1, 0, 0, session);
    if let Some(token) = token {
        out.extend(token);
    }
}

pub fn write_video(out: &mut Vec<u8>, session: u32, shard: &VideoShard<'_>) {
    out.clear();
    out.reserve(28 + shard.bytes.len());
    let mut hdr = [0u8; 28];
    hdr[0] = 2;
    hdr[1] = shard.keyframe as u8 | if shard.recovery != 0 { 2 } else { 0 };
    hdr[2..4].copy_from_slice(&shard.host_delay.to_le_bytes());
    hdr[4..8].copy_from_slice(&session.to_le_bytes());
    hdr[8..12].copy_from_slice(&shard.frame.to_le_bytes());
    hdr[12..16].copy_from_slice(&shard.frame_len.to_le_bytes());
    hdr[16..18].copy_from_slice(&shard.shard.to_le_bytes());
    hdr[18] = shard.block;
    hdr[19] = shard.blocks;
    hdr[20..22].copy_from_slice(&shard.data.to_le_bytes());
    hdr[22..24].copy_from_slice(&shard.parity.to_le_bytes());
    hdr[24..26].copy_from_slice(&shard.shard_len.to_le_bytes());
    hdr[26..28].copy_from_slice(&shard.recovery.to_le_bytes());
    out.extend_from_slice(&hdr);
    out.extend_from_slice(shard.bytes);
}

pub fn write_audio(out: &mut Vec<u8>, session: u32, p: &AudioPacket<'_>) {
    header(out, 3, p.parity as u8, 0, session);
    out.extend(p.seq.to_le_bytes());
    out.extend(p.pts.to_le_bytes());
    out.extend(p.payload);
}

pub fn write_input(out: &mut Vec<u8>, session: u32, first: u32, events: &[Event]) -> usize {
    header(out, 4, 0, 0, session);
    out.extend(first.to_le_bytes());
    out.push(0);
    let mut count = 0;
    for event in events {
        if count == 255 || out.len() + event.wire_len() > crate::MAX_DATAGRAM {
            break;
        }
        event.write(out);
        count += 1;
    }
    out[12] = count as u8;
    count
}

pub fn write_input_ack(out: &mut Vec<u8>, session: u32, next: u32) {
    header(out, 5, 0, 0, session);
    out.extend(next.to_le_bytes());
}

pub fn write_keyframe_request(out: &mut Vec<u8>, session: u32, last: u32) {
    header(out, 6, 0, 0, session);
    out.extend(last.to_le_bytes());
}

pub fn write_recovery_request(out: &mut Vec<u8>, session: u32, r: &RecoveryRequest) {
    header(out, 13, 0, 0, session);
    for v in [r.last_delivered, r.first_invalid, r.last_invalid, r.id] {
        out.extend(v.to_le_bytes());
    }
}

pub fn write_recovery_ack(out: &mut Vec<u8>, session: u32, a: &RecoveryAck) {
    header(out, 14, a.status as u8, 0, session);
    out.extend(a.id.to_le_bytes());
    out.extend(a.frame.to_le_bytes());
}

pub fn write_ping(out: &mut Vec<u8>, session: u32, time: u64) {
    header(out, 7, 0, 0, session);
    out.extend(time.to_le_bytes());
}

pub fn write_pong(out: &mut Vec<u8>, session: u32, time: u64) {
    header(out, 8, 0, 0, session);
    out.extend(time.to_le_bytes());
}

/// At most [`crate::gamepad::MAX_PADS`] snapshots, one per slot, so they
/// always fit.
pub fn write_pad(out: &mut Vec<u8>, session: u32, updates: &[PadUpdate]) {
    debug_assert!(!updates.is_empty() && updates.len() <= crate::gamepad::MAX_PADS);
    header(out, 9, 0, 0, session);
    out.push(updates.len() as u8);
    for u in updates {
        u.write(out);
    }
}

fn write_acks(out: &mut Vec<u8>, kind: u8, session: u32, acks: &[Ack]) {
    debug_assert!(!acks.is_empty() && acks.len() <= crate::gamepad::MAX_PADS);
    header(out, kind, 0, 0, session);
    out.push(acks.len() as u8);
    for a in acks {
        out.push(a.slot);
        out.extend(a.seq.to_le_bytes());
    }
}

pub fn write_pad_ack(out: &mut Vec<u8>, session: u32, acks: &[Ack]) {
    write_acks(out, 10, session, acks);
}

pub fn write_pad_feedback(out: &mut Vec<u8>, session: u32, updates: &[FeedbackUpdate]) {
    debug_assert!(!updates.is_empty() && updates.len() <= crate::gamepad::MAX_PADS);
    header(out, 11, 0, 0, session);
    out.push(updates.len() as u8);
    for u in updates {
        u.write(out);
    }
}

pub fn write_pad_feedback_ack(out: &mut Vec<u8>, session: u32, acks: &[Ack]) {
    write_acks(out, 12, session, acks);
}
