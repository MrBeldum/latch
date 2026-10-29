//! Datagram layouts: the 8-byte header every datagram starts with, and the
//! body of each kind. See `docs/PROTOCOL.md`, "Datagrams (UDP)".
//!
//! Parsing never panics and never allocates: a malformed datagram is an
//! [`WireError`], which the caller drops silently.

use crate::input::Event;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum Kind {
    Hello = 1,
    Video = 2,
    Audio = 3,
    Input = 4,
    InputAck = 5,
    KeyframeRequest = 6,
    Ping = 7,
    Pong = 8,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WireError {
    /// Shorter than its kind needs, or longer than [`crate::MAX_DATAGRAM`].
    Length,
    UnknownKind(u8),
    /// A field holds a value the protocol does not allow (an input event
    /// kind, a shard index past its block, …).
    Field(&'static str),
}

impl std::fmt::Display for WireError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            WireError::Length => write!(f, "datagram length does not match its kind"),
            WireError::UnknownKind(k) => write!(f, "unknown datagram kind {k}"),
            WireError::Field(what) => write!(f, "bad datagram field: {what}"),
        }
    }
}

impl std::error::Error for WireError {}

/// One video shard as it travels. `bytes` borrows the datagram.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct VideoShard<'a> {
    pub keyframe: bool,
    /// Host capture-to-send time, 0.1 ms units, saturating.
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

/// One audio packet (or parity packet) as it travels.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AudioPacket<'a> {
    pub parity: bool,
    pub seq: u32,
    pub pts: u32,
    pub payload: &'a [u8],
}

/// A parsed datagram. `session` is every kind's header field.
#[derive(Debug, Clone, PartialEq)]
pub enum Datagram<'a> {
    /// `token` is present from a viewer, absent from a host.
    Hello {
        session: u32,
        token: Option<[u8; 16]>,
    },
    Video {
        session: u32,
        shard: VideoShard<'a>,
    },
    Audio {
        session: u32,
        packet: AudioPacket<'a>,
    },
    /// Events `first, first + 1, …` in order.
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
    Ping {
        session: u32,
        time: u64,
    },
    Pong {
        session: u32,
        time: u64,
    },
}

/// The events of an `Input` datagram, validated by [`parse`] and decoded
/// lazily so parsing does not allocate.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct InputEvents<'a> {
    pub count: u8,
    pub bytes: &'a [u8],
}

impl<'a> InputEvents<'a> {
    pub fn iter(&self) -> impl Iterator<Item = Event> + 'a {
        let _ = self;
        std::iter::empty()
    }
}

impl Datagram<'_> {
    pub fn session(&self) -> u32 {
        todo!()
    }
}

/// Parses one datagram. Checks the length against the kind and every field
/// against `docs/PROTOCOL.md`.
pub fn parse(buf: &[u8]) -> Result<Datagram<'_>, WireError> {
    let _ = buf;
    todo!()
}

/// Writers. Each clears `out` and writes one whole datagram into it.
pub fn write_hello(out: &mut Vec<u8>, session: u32, token: Option<&[u8; 16]>) {
    let _ = (out, session, token);
    todo!()
}

pub fn write_video(out: &mut Vec<u8>, session: u32, shard: &VideoShard<'_>) {
    let _ = (out, session, shard);
    todo!()
}

pub fn write_audio(out: &mut Vec<u8>, session: u32, packet: &AudioPacket<'_>) {
    let _ = (out, session, packet);
    todo!()
}

/// Writes as many of `events` (numbered from `first`) as fit in
/// [`crate::MAX_DATAGRAM`]; returns how many it wrote (at least one when
/// `events` is not empty).
pub fn write_input(out: &mut Vec<u8>, session: u32, first: u32, events: &[Event]) -> usize {
    let _ = (out, session, first, events);
    todo!()
}

pub fn write_input_ack(out: &mut Vec<u8>, session: u32, next: u32) {
    let _ = (out, session, next);
    todo!()
}

pub fn write_keyframe_request(out: &mut Vec<u8>, session: u32, last: u32) {
    let _ = (out, session, last);
    todo!()
}

pub fn write_ping(out: &mut Vec<u8>, session: u32, time: u64) {
    let _ = (out, session, time);
    todo!()
}

pub fn write_pong(out: &mut Vec<u8>, session: u32, time: u64) {
    let _ = (out, session, time);
    todo!()
}
