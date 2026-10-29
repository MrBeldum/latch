//! Input events and their reliability. See `docs/PROTOCOL.md`, "Input (4)".

use std::time::{Duration, Instant};

pub const BUTTON_LEFT: u8 = 1;
pub const BUTTON_MIDDLE: u8 = 2;
pub const BUTTON_RIGHT: u8 = 3;
pub const BUTTON_BACK: u8 = 4;
pub const BUTTON_FORWARD: u8 = 5;

pub const MOD_SHIFT: u8 = 1;
pub const MOD_CTRL: u8 = 2;
pub const MOD_ALT: u8 = 4;
pub const MOD_META: u8 = 8;

/// Longest text event, in bytes.
pub const MAX_TEXT: usize = 64;
/// How often unacknowledged events are sent again.
pub const RESEND: Duration = Duration::from_millis(20);
/// Unacknowledged events kept at most; beyond this, new relative mouse
/// moves are dropped first, then the oldest events.
pub const MAX_UNACKED: usize = 1024;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Event {
    MouseMove {
        dx: i16,
        dy: i16,
    },
    /// A point in a `width × height` picture.
    MousePosition {
        x: u16,
        y: u16,
        width: u16,
        height: u16,
    },
    MouseButton {
        button: u8,
        down: bool,
    },
    /// `vk` is a Windows virtual-key code; `modifiers` the `MOD_*` bits held.
    Key {
        vk: u16,
        down: bool,
        modifiers: u8,
    },
    /// 120 per notch; up and right positive.
    Scroll {
        vertical: i16,
        horizontal: i16,
    },
    /// 1 to [`MAX_TEXT`] bytes of UTF-8.
    Text(String),
    ReleaseAll,
}

impl Event {
    /// Bytes this event takes in an `Input` datagram.
    pub fn wire_len(&self) -> usize {
        todo!()
    }

    /// Appends the event's wire form.
    pub fn write(&self, out: &mut Vec<u8>) {
        let _ = out;
        todo!()
    }

    /// Reads one event from the front of `bytes`; returns it and its length.
    pub fn read(bytes: &[u8]) -> Option<(Event, usize)> {
        let _ = bytes;
        todo!()
    }
}

/// The viewer's side: numbers events and sends every unacknowledged one
/// until the host acknowledges it.
pub struct InputSender {
    session: u32,
}

impl InputSender {
    pub fn new(session: u32) -> Self {
        Self { session }
    }

    /// Queues an event. Text longer than [`MAX_TEXT`] is split at character
    /// boundaries. A relative move right after another not yet sent is
    /// merged into it (saturating).
    pub fn push(&mut self, event: Event) {
        let _ = (event, self.session);
        todo!()
    }

    /// From an `InputAck`: every event before `next` arrived.
    pub fn ack(&mut self, next: u32) {
        let _ = next;
        todo!()
    }

    /// Writes an `Input` datagram into `out` and returns true when one is
    /// due: there are events never sent, or unacknowledged ones last sent
    /// [`RESEND`] ago or more.
    pub fn poll(&mut self, now: Instant, out: &mut Vec<u8>) -> bool {
        let _ = (now, out);
        todo!()
    }

    /// When `poll` will next have something to send, if nothing new is
    /// pushed.
    pub fn next_deadline(&self) -> Option<Instant> {
        todo!()
    }

    pub fn unacked(&self) -> usize {
        todo!()
    }
}

/// The host's side: applies events once each, in order.
pub struct InputReceiver {
    next: u32,
}

impl InputReceiver {
    pub fn new() -> Self {
        Self { next: 0 }
    }

    /// Takes the events of one `Input` datagram (numbered from `first`),
    /// calls `apply` for each event now in order, and returns the `next`
    /// to acknowledge. Events after a gap are kept (at most
    /// [`MAX_UNACKED`]) until the gap is filled.
    pub fn receive(
        &mut self,
        first: u32,
        events: impl IntoIterator<Item = Event>,
        apply: impl FnMut(Event),
    ) -> u32 {
        let _ = (first, events, apply);
        todo!()
    }

    pub fn next(&self) -> u32 {
        self.next
    }
}

impl Default for InputReceiver {
    fn default() -> Self {
        Self::new()
    }
}
