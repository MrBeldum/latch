//! Input events and their reliability. See `docs/PROTOCOL.md`, "Input (4)".

use std::collections::{BTreeMap, VecDeque};
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
/// Backlog threshold retained for callers monitoring input congestion.
/// Motion only merges with an adjacent, unsent event, regardless of backlog.
pub const MAX_UNACKED: usize = 1024;
/// Receiver reorder window. The sender retains input until acknowledged,
/// including backlogs larger than this window: it sends oldest first, so it
/// never needs the receiver to buffer the entire backlog at once.
pub const HARD_MAX_UNACKED: usize = 65_536;

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
        match self {
            Event::MouseMove { .. } | Event::Scroll { .. } | Event::Key { .. } => 5,
            Event::MousePosition { .. } => 9,
            Event::MouseButton { .. } => 3,
            Event::Text(s) => 2 + s.len(),
            Event::ReleaseAll => 1,
        }
    }

    /// Appends the event's wire form.
    pub fn write(&self, out: &mut Vec<u8>) {
        match self {
            Event::MouseMove { dx, dy } => {
                out.push(1);
                out.extend(dx.to_le_bytes());
                out.extend(dy.to_le_bytes());
            }
            Event::MousePosition {
                x,
                y,
                width,
                height,
            } => {
                out.push(2);
                for n in [x, y, width, height] {
                    out.extend(n.to_le_bytes());
                }
            }
            Event::MouseButton { button, down } => out.extend([3, *button, *down as u8]),
            Event::Key {
                vk,
                down,
                modifiers,
            } => {
                out.push(4);
                out.extend(vk.to_le_bytes());
                out.extend([*down as u8, *modifiers]);
            }
            Event::Scroll {
                vertical,
                horizontal,
            } => {
                out.push(5);
                out.extend(vertical.to_le_bytes());
                out.extend(horizontal.to_le_bytes());
            }
            Event::Text(text) => {
                out.extend([6, text.len() as u8]);
                out.extend(text.as_bytes());
            }
            Event::ReleaseAll => out.push(7),
        }
    }

    /// Reads one event from the front of `bytes`; returns it and its length.
    pub fn read(bytes: &[u8]) -> Option<(Event, usize)> {
        let kind = *bytes.first()?;
        let u16at = |i| Some(u16::from_le_bytes([*bytes.get(i)?, *bytes.get(i + 1)?]));
        let boolean = |i| match bytes.get(i)? {
            0 => Some(false),
            1 => Some(true),
            _ => None,
        };
        Some(match kind {
            1 => (
                Event::MouseMove {
                    dx: u16at(1)? as i16,
                    dy: u16at(3)? as i16,
                },
                5,
            ),
            2 => (
                Event::MousePosition {
                    x: u16at(1)?,
                    y: u16at(3)?,
                    width: u16at(5)?,
                    height: u16at(7)?,
                },
                9,
            ),
            3 => {
                let button = *bytes.get(1)?;
                if !(1..=5).contains(&button) {
                    return None;
                }
                (
                    Event::MouseButton {
                        button,
                        down: boolean(2)?,
                    },
                    3,
                )
            }
            4 => (
                Event::Key {
                    vk: u16at(1)?,
                    down: boolean(3)?,
                    modifiers: *bytes.get(4)?,
                },
                5,
            ),
            5 => (
                Event::Scroll {
                    vertical: u16at(1)? as i16,
                    horizontal: u16at(3)? as i16,
                },
                5,
            ),
            6 => {
                let len = *bytes.get(1)? as usize;
                if !(1..=MAX_TEXT).contains(&len) {
                    return None;
                }
                (
                    Event::Text(std::str::from_utf8(bytes.get(2..2 + len)?).ok()?.to_owned()),
                    2 + len,
                )
            }
            7 => (Event::ReleaseAll, 1),
            _ => return None,
        })
    }
}

/// The viewer's side: numbers events and sends every unacknowledged one
/// until the host acknowledges it.
pub struct InputSender {
    session: u32,
    next: u32,
    queue: VecDeque<(u32, Event, bool)>,
    last_send: Option<Instant>,
    changed: bool,
}

impl InputSender {
    pub fn new(session: u32) -> Self {
        Self {
            session,
            next: 0,
            queue: Default::default(),
            last_send: None,
            changed: false,
        }
    }

    /// Queues an event. Text longer than [`MAX_TEXT`] is split at character
    /// boundaries. A relative move right after another not yet sent is
    /// merged into it if the sum fits. Sent payloads remain immutable.
    pub fn push(&mut self, event: Event) {
        if let Event::Text(text) = &event {
            if text.is_empty() {
                return;
            }
            if text.len() > MAX_TEXT {
                let mut start = 0;
                while start < text.len() {
                    let mut end = (start + MAX_TEXT).min(text.len());
                    while !text.is_char_boundary(end) {
                        end -= 1;
                    }
                    self.push(Event::Text(text[start..end].to_owned()));
                    start = end;
                }
                return;
            }
        }
        if let Event::MouseMove { dx, dy } = event {
            if let Some((_, Event::MouseMove { dx: x, dy: y }, false)) = self.queue.back_mut() {
                if let (Some(a), Some(b)) = (x.checked_add(dx), y.checked_add(dy)) {
                    *x = a;
                    *y = b;
                    self.changed = true;
                    return;
                }
            }
            self.enqueue(Event::MouseMove { dx, dy });
        } else {
            self.enqueue(event);
        }
    }

    fn enqueue(&mut self, event: Event) {
        self.queue.push_back((self.next, event, false));
        self.next = self.next.wrapping_add(1);
        self.changed = true;
    }

    /// From an `InputAck`: every event before `next` arrived.
    pub fn ack(&mut self, next: u32) {
        if crate::video::before(self.next, next) {
            return;
        }
        while self
            .queue
            .front()
            .is_some_and(|(seq, _, _)| crate::video::before(*seq, next))
        {
            self.queue.pop_front();
            self.changed = true;
        }
    }

    /// Writes an `Input` datagram into `out` and returns true when one is
    /// due: the queue changed, or unacknowledged events were last sent
    /// [`RESEND`] ago or more. Always starts at the oldest unacknowledged
    /// event; an acknowledgement makes room for the next batch at once.
    pub fn poll(&mut self, now: Instant, out: &mut Vec<u8>) -> bool {
        if self.queue.is_empty() {
            return false;
        }
        if !self.changed
            && self
                .last_send
                .is_some_and(|t| now.saturating_duration_since(t) < RESEND)
        {
            return false;
        }
        let events: Vec<Event> = self.queue.iter().take(255).map(|x| x.1.clone()).collect();
        let count = crate::wire::write_input(out, self.session, self.queue[0].0, &events);
        for entry in self.queue.iter_mut().take(count) {
            entry.2 = true;
        }
        self.last_send = Some(now);
        // An unsent tail beyond this datagram must not make `while poll()`
        // spin on the same oldest batch while waiting for its acknowledgement.
        self.changed = false;
        count > 0
    }

    /// When `poll` will next have something to send, if nothing new is
    /// pushed.
    pub fn next_deadline(&self) -> Option<Instant> {
        if self.queue.is_empty() {
            None
        } else if self.changed {
            self.last_send
        } else {
            self.last_send.map(|t| t + RESEND)
        }
    }

    pub fn unacked(&self) -> usize {
        self.queue.len()
    }
}

/// The host's side: applies events once each, in order.
pub struct InputReceiver {
    next: u32,
    pending: BTreeMap<u32, Event>,
    /// The event at `next` that failed, and since when.
    failing: Option<(u32, Instant)>,
}

/// How long a failing event is retried before it is dropped. A transient
/// refusal (Windows' secure desktop, a busy display server) clears well
/// within it; an event the host can never inject must not hold every
/// later move, key-up and release behind it for the rest of the session.
pub const GIVE_UP_AFTER: Duration = Duration::from_secs(2);

impl InputReceiver {
    pub fn new() -> Self {
        Self {
            next: 0,
            pending: Default::default(),
            failing: None,
        }
    }

    /// Takes the events of one `Input` datagram (numbered from `first`),
    /// calls `apply` for each event now in order, and returns the `next`
    /// to acknowledge. Events after a gap are kept (at most
    /// [`HARD_MAX_UNACKED`]) until the gap is filled.
    pub fn receive(
        &mut self,
        first: u32,
        events: impl IntoIterator<Item = Event>,
        mut apply: impl FnMut(Event),
    ) -> u32 {
        self.try_receive(first, events, |event| {
            apply(event);
            Ok::<_, ()>(())
        })
    }

    /// Failed injection stays pending and is not acknowledged, for up to
    /// [`GIVE_UP_AFTER`]; then it is dropped and what follows goes on.
    pub fn try_receive<E>(
        &mut self,
        first: u32,
        events: impl IntoIterator<Item = Event>,
        apply: impl FnMut(Event) -> Result<(), E>,
    ) -> u32 {
        self.try_receive_at(Instant::now(), first, events, apply)
    }

    fn try_receive_at<E>(
        &mut self,
        now: Instant,
        first: u32,
        events: impl IntoIterator<Item = Event>,
        mut apply: impl FnMut(Event) -> Result<(), E>,
    ) -> u32 {
        for (i, event) in events.into_iter().enumerate() {
            let seq = first.wrapping_add(i as u32);
            let offset = seq.wrapping_sub(self.next);
            if offset < HARD_MAX_UNACKED as u32 {
                self.pending.entry(seq).or_insert(event);
            }
        }
        while let Some(event) = self.pending.remove(&self.next) {
            if apply(event.clone()).is_err() {
                let since = match self.failing {
                    Some((seq, since)) if seq == self.next => since,
                    _ => now,
                };
                if now.duration_since(since) < GIVE_UP_AFTER {
                    self.failing = Some((self.next, since));
                    self.pending.insert(self.next, event);
                    break;
                }
            }
            self.failing = None;
            self.next = self.next.wrapping_add(1);
        }
        self.next
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

#[cfg(test)]
mod fallible_tests {
    use super::*;
    #[test]
    fn failed_release_is_retried_before_acknowledging() {
        let mut receiver = InputReceiver::new();
        assert_eq!(receiver.try_receive(0, [Event::ReleaseAll], |_| Err(())), 0);
        let mut applied = 0;
        assert_eq!(
            receiver.try_receive(0, [Event::ReleaseAll], |_| {
                applied += 1;
                Ok::<_, ()>(())
            }),
            1
        );
        assert_eq!(applied, 1);
    }

    #[test]
    fn an_event_that_never_injects_is_dropped_and_the_rest_go_on() {
        let start = Instant::now();
        let mut receiver = InputReceiver::new();
        let stuck = Event::Text("x".into());
        let mut applied = Vec::new();
        let mut apply = |e: Event| {
            if e == stuck {
                Err(())
            } else {
                applied.push(e);
                Ok(())
            }
        };
        let events = [stuck.clone(), Event::MouseMove { dx: 1, dy: 1 }];
        assert_eq!(
            receiver.try_receive_at(start, 0, events.clone(), &mut apply),
            0
        );
        // Still within the grace period: held, not acknowledged.
        let later = start + GIVE_UP_AFTER / 2;
        assert_eq!(
            receiver.try_receive_at(later, 0, events.clone(), &mut apply),
            0
        );
        // Past it: dropped, and the move behind it goes through.
        let past = start + GIVE_UP_AFTER;
        assert_eq!(receiver.try_receive_at(past, 0, events, &mut apply), 2);
        assert_eq!(applied, [Event::MouseMove { dx: 1, dy: 1 }]);
    }
}
