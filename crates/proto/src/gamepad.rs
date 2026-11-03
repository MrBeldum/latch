//! Gamepads: each pad's whole state from viewer to host, and rumble and
//! lights back. See `docs/PROTOCOL.md`, "Gamepads (9–12)".
//!
//! Unlike keyboard and mouse events, a pad is a state, not a stream of
//! changes: the newest snapshot replaces every older one. Each side numbers
//! its snapshots per slot, sends a new one at once, and sends the newest
//! unacknowledged snapshot of every slot again every [`RESEND`] until the
//! other side acknowledges it. A receiver applies a snapshot only when it is
//! newer than the last one it applied, so a late or repeated datagram can
//! never move a stick back.

use serde::{Deserialize, Serialize};
use std::time::{Duration, Instant};

/// Slots on the wire; a host announces how many it can emulate.
pub const MAX_PADS: usize = 16;
/// How often unacknowledged snapshots are sent again. Snapshots are small
/// (at most 46 bytes), so a short interval costs nothing worth saving and
/// bounds what one lost datagram costs a stick or a rumble.
pub const RESEND: Duration = Duration::from_millis(10);

/// What a pad looks like to games, as the viewer saw it. A host emulates
/// the kinds it announces and falls back to an Xbox pad for the others.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum PadKind {
    #[serde(rename = "xbox")]
    Xbox,
    #[serde(rename = "playstation")]
    PlayStation,
    #[serde(rename = "generic")]
    Generic,
}

impl PadKind {
    fn wire(self) -> u8 {
        match self {
            PadKind::Xbox => 1,
            PadKind::PlayStation => 2,
            PadKind::Generic => 3,
        }
    }

    fn from_wire(b: u8) -> Option<Option<PadKind>> {
        Some(match b {
            0 => None,
            1 => Some(PadKind::Xbox),
            2 => Some(PadKind::PlayStation),
            3 => Some(PadKind::Generic),
            _ => return None,
        })
    }

    pub fn name(self) -> &'static str {
        match self {
            PadKind::Xbox => "Xbox",
            PadKind::PlayStation => "PlayStation",
            PadKind::Generic => "gamepad",
        }
    }
}

/// Button bits, by position on an Xbox-layout pad: `SOUTH` is A on an Xbox
/// pad and Cross on a PlayStation one.
pub mod button {
    pub const SOUTH: u32 = 1 << 0;
    pub const EAST: u32 = 1 << 1;
    pub const WEST: u32 = 1 << 2;
    pub const NORTH: u32 = 1 << 3;
    pub const LEFT_SHOULDER: u32 = 1 << 4;
    pub const RIGHT_SHOULDER: u32 = 1 << 5;
    /// View, Back, Share (DualShock 4), Create (DualSense), Select.
    pub const BACK: u32 = 1 << 6;
    /// Menu, Start, Options.
    pub const START: u32 = 1 << 7;
    /// Xbox, PS, Home.
    pub const GUIDE: u32 = 1 << 8;
    pub const LEFT_STICK: u32 = 1 << 9;
    pub const RIGHT_STICK: u32 = 1 << 10;
    pub const DPAD_UP: u32 = 1 << 11;
    pub const DPAD_DOWN: u32 = 1 << 12;
    pub const DPAD_LEFT: u32 = 1 << 13;
    pub const DPAD_RIGHT: u32 = 1 << 14;
    /// Share (Xbox Series), Mute (DualSense), Capture (Switch).
    pub const MISC: u32 = 1 << 15;
    pub const TOUCHPAD: u32 = 1 << 16;
    pub const PADDLE1: u32 = 1 << 17;
    pub const PADDLE2: u32 = 1 << 18;
    pub const PADDLE3: u32 = 1 << 19;
    pub const PADDLE4: u32 = 1 << 20;
}

/// Gyroscope in 1/16 °/s and accelerometer in 1/8192 g, x right, y up,
/// z towards the player: a DualShock 4's own units.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Motion {
    pub gyro: [i16; 3],
    pub accel: [i16; 3],
}

/// One finger on a touchpad; `x` and `y` span the pad, 0 to 65535 from the
/// top left.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Touch {
    pub down: bool,
    /// Tracks one finger from touch to lift, 0 to 127.
    pub id: u8,
    pub x: u16,
    pub y: u16,
}

/// Everything a pad reports at one moment.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct PadState {
    /// [`button`] bits held.
    pub buttons: u32,
    /// Sticks, −32768 to 32767, right and up positive.
    pub left_x: i16,
    pub left_y: i16,
    pub right_x: i16,
    pub right_y: i16,
    /// Triggers, 0 (released) to 65535 (fully pulled).
    pub left_trigger: u16,
    pub right_trigger: u16,
    pub motion: Option<Motion>,
    pub touch: Option<[Touch; 2]>,
}

/// A slot's snapshot: the pad in it, or `kind: None` once it is gone.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PadUpdate {
    pub slot: u8,
    pub seq: u32,
    pub kind: Option<PadKind>,
    pub state: PadState,
}

const STATE_LEN: usize = 24;
const MOTION_LEN: usize = 12;
const TOUCH_LEN: usize = 10;

impl PadUpdate {
    pub fn wire_len(&self) -> usize {
        STATE_LEN
            + self.state.motion.map_or(0, |_| MOTION_LEN)
            + self.state.touch.map_or(0, |_| TOUCH_LEN)
    }

    pub fn write(&self, out: &mut Vec<u8>) {
        let s = &self.state;
        let flags = s.motion.is_some() as u8 | (s.touch.is_some() as u8) << 1;
        out.extend([self.slot, self.kind.map_or(0, PadKind::wire), flags, 0]);
        out.extend(self.seq.to_le_bytes());
        out.extend(s.buttons.to_le_bytes());
        for v in [s.left_x, s.left_y, s.right_x, s.right_y] {
            out.extend(v.to_le_bytes());
        }
        out.extend(s.left_trigger.to_le_bytes());
        out.extend(s.right_trigger.to_le_bytes());
        if let Some(m) = s.motion {
            for v in m.gyro.iter().chain(&m.accel) {
                out.extend(v.to_le_bytes());
            }
        }
        if let Some(touch) = s.touch {
            for t in touch {
                out.push((t.down as u8) << 7 | (t.id & 0x7f));
                out.extend(t.x.to_le_bytes());
                out.extend(t.y.to_le_bytes());
            }
        }
    }

    /// Reads one snapshot from the front of `bytes`; returns it and its
    /// length.
    pub fn read(bytes: &[u8]) -> Option<(PadUpdate, usize)> {
        let head = bytes.get(..STATE_LEN)?;
        let slot = head[0];
        if slot as usize >= MAX_PADS || head[3] != 0 || head[2] & !3 != 0 {
            return None;
        }
        let kind = PadKind::from_wire(head[1])?;
        let u16at = |b: &[u8], i: usize| u16::from_le_bytes([b[i], b[i + 1]]);
        let u32at = |i: usize| u32::from_le_bytes([head[i], head[i + 1], head[i + 2], head[i + 3]]);
        let mut state = PadState {
            buttons: u32at(8),
            left_x: u16at(head, 12) as i16,
            left_y: u16at(head, 14) as i16,
            right_x: u16at(head, 16) as i16,
            right_y: u16at(head, 18) as i16,
            left_trigger: u16at(head, 20),
            right_trigger: u16at(head, 22),
            motion: None,
            touch: None,
        };
        let mut len = STATE_LEN;
        if head[2] & 1 != 0 {
            let m = bytes.get(len..len + MOTION_LEN)?;
            let v = |i: usize| u16at(m, 2 * i) as i16;
            state.motion = Some(Motion {
                gyro: [v(0), v(1), v(2)],
                accel: [v(3), v(4), v(5)],
            });
            len += MOTION_LEN;
        }
        if head[2] & 2 != 0 {
            let t = bytes.get(len..len + TOUCH_LEN)?;
            let finger = |o: usize| Touch {
                down: t[o] & 0x80 != 0,
                id: t[o] & 0x7f,
                x: u16at(t, o + 1),
                y: u16at(t, o + 3),
            };
            state.touch = Some([finger(0), finger(5)]);
            len += TOUCH_LEN;
        }
        Some((
            PadUpdate {
                slot,
                seq: u32at(4),
                kind,
                state,
            },
            len,
        ))
    }
}

/// What the host's games ask of a pad: motors, 0 (off) to 65535, and a
/// light. Xbox pads have both trigger motors; most others have neither.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Feedback {
    /// The heavy, low-frequency motor (left on an Xbox pad).
    pub large: u16,
    /// The light, high-frequency motor.
    pub small: u16,
    pub left_trigger: u16,
    pub right_trigger: u16,
    /// A light bar's colour, when the game set one.
    pub led: Option<[u8; 3]>,
    /// The player number the host's system gave the pad, 1 to 8; 0 unknown.
    pub player: u8,
}

/// A slot's feedback snapshot.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FeedbackUpdate {
    pub slot: u8,
    pub seq: u32,
    pub feedback: Feedback,
}

pub const FEEDBACK_LEN: usize = 20;

impl FeedbackUpdate {
    pub fn write(&self, out: &mut Vec<u8>) {
        let f = &self.feedback;
        out.extend([self.slot, f.led.is_some() as u8, f.player, 0]);
        out.extend(self.seq.to_le_bytes());
        for v in [f.large, f.small, f.left_trigger, f.right_trigger] {
            out.extend(v.to_le_bytes());
        }
        out.extend(f.led.unwrap_or_default());
        out.push(0);
    }

    pub fn read(bytes: &[u8]) -> Option<FeedbackUpdate> {
        let b = bytes.get(..FEEDBACK_LEN)?;
        if b[0] as usize >= MAX_PADS || b[1] & !1 != 0 || b[3] != 0 || b[19] != 0 {
            return None;
        }
        let u16at = |i: usize| u16::from_le_bytes([b[i], b[i + 1]]);
        Some(FeedbackUpdate {
            slot: b[0],
            seq: u32::from_le_bytes([b[4], b[5], b[6], b[7]]),
            feedback: Feedback {
                large: u16at(8),
                small: u16at(10),
                left_trigger: u16at(12),
                right_trigger: u16at(14),
                led: (b[1] & 1 != 0).then(|| [b[16], b[17], b[18]]),
                player: b[2],
            },
        })
    }
}

/// An acknowledgement: every snapshot of `slot` up to `seq` is settled.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Ack {
    pub slot: u8,
    pub seq: u32,
}

pub const ACK_LEN: usize = 5;

#[derive(Debug, Clone)]
struct Pending<T> {
    seq: u32,
    value: T,
    acked: bool,
}

/// Each slot's newest value, kept until the other side acknowledges it.
#[derive(Debug, Clone)]
pub struct LatestSender<T> {
    slots: Vec<Option<Pending<T>>>,
    changed: bool,
    last_send: Option<Instant>,
}

impl<T: Copy + PartialEq> Default for LatestSender<T> {
    fn default() -> Self {
        Self::new()
    }
}

impl<T: Copy + PartialEq> LatestSender<T> {
    pub fn new() -> Self {
        Self {
            slots: vec![None; MAX_PADS],
            changed: false,
            last_send: None,
        }
    }

    /// Makes `value` the slot's newest; returns false if it already was.
    pub fn set(&mut self, slot: u8, value: T) -> bool {
        let Some(entry) = self.slots.get_mut(slot as usize) else {
            return false;
        };
        match entry {
            Some(p) if p.value == value => false,
            Some(p) => {
                p.seq = p.seq.wrapping_add(1);
                p.value = value;
                p.acked = false;
                self.changed = true;
                true
            }
            None => {
                *entry = Some(Pending {
                    seq: 1,
                    value,
                    acked: false,
                });
                self.changed = true;
                true
            }
        }
    }

    pub fn get(&self, slot: u8) -> Option<&T> {
        self.slots.get(slot as usize)?.as_ref().map(|p| &p.value)
    }

    pub fn ack(&mut self, ack: Ack) {
        if let Some(Some(p)) = self.slots.get_mut(ack.slot as usize) {
            if !crate::video::before(ack.seq, p.seq) {
                p.acked = true;
            }
        }
    }

    /// Whether a datagram is due: something changed, or something is
    /// unacknowledged and was last sent [`RESEND`] ago.
    pub fn due(&self, now: Instant) -> bool {
        self.changed
            || (self.unacked().next().is_some()
                && self
                    .last_send
                    .is_none_or(|t| now.saturating_duration_since(t) >= RESEND))
    }

    /// Every slot whose newest value is unacknowledged: slot, seq, value.
    pub fn unacked(&self) -> impl Iterator<Item = (u8, u32, &T)> {
        self.slots.iter().enumerate().filter_map(|(i, p)| {
            p.as_ref()
                .filter(|p| !p.acked)
                .map(|p| (i as u8, p.seq, &p.value))
        })
    }

    pub fn sent(&mut self, now: Instant) {
        self.changed = false;
        self.last_send = Some(now);
    }

    /// When [`LatestSender::due`] next turns true, if nothing changes.
    pub fn next_deadline(&self) -> Option<Instant> {
        if self.unacked().next().is_none() {
            None
        } else if self.changed {
            Some(self.last_send.unwrap_or_else(Instant::now))
        } else {
            self.last_send.map(|t| t + RESEND)
        }
    }
}

/// Applies each slot's snapshots newest-only.
#[derive(Debug, Clone)]
pub struct LatestReceiver {
    applied: Vec<Option<u32>>,
}

impl Default for LatestReceiver {
    fn default() -> Self {
        Self::new()
    }
}

impl LatestReceiver {
    pub fn new() -> Self {
        Self {
            applied: vec![None; MAX_PADS],
        }
    }

    /// Whether `seq` is newer than the last snapshot applied to `slot`;
    /// if so it becomes the last.
    pub fn accept(&mut self, slot: u8, seq: u32) -> bool {
        let Some(last) = self.applied.get_mut(slot as usize) else {
            return false;
        };
        if last.is_some_and(|l| !crate::video::before(l, seq)) {
            return false;
        }
        *last = Some(seq);
        true
    }

    /// The acknowledgement for `slot`: the newest snapshot applied.
    pub fn ack(&self, slot: u8) -> Option<Ack> {
        let seq = (*self.applied.get(slot as usize)?)?;
        Some(Ack { slot, seq })
    }
}

/// The viewer's side: each pad's newest snapshot, until the host has it.
pub struct PadSender {
    session: u32,
    pads: LatestSender<(Option<PadKind>, PadState)>,
}

impl PadSender {
    pub fn new(session: u32) -> Self {
        Self {
            session,
            pads: LatestSender::new(),
        }
    }

    /// The pad in `slot` (now) looks like this.
    pub fn update(&mut self, slot: u8, kind: PadKind, state: PadState) {
        self.pads.set(slot, (Some(kind), state));
    }

    /// The pad in `slot` went away. A slot never used is left alone.
    pub fn remove(&mut self, slot: u8) {
        if self.pads.get(slot).is_some_and(|(k, _)| k.is_some()) {
            self.pads.set(slot, (None, PadState::default()));
        }
    }

    pub fn ack(&mut self, ack: Ack) {
        self.pads.ack(ack);
    }

    /// Writes a `Pad` datagram into `out` and returns true when one is due.
    pub fn poll(&mut self, now: Instant, out: &mut Vec<u8>) -> bool {
        if !self.pads.due(now) {
            return false;
        }
        let updates: Vec<PadUpdate> = self
            .pads
            .unacked()
            .map(|(slot, seq, (kind, state))| PadUpdate {
                slot,
                seq,
                kind: *kind,
                state: *state,
            })
            .collect();
        self.pads.sent(now);
        if updates.is_empty() {
            return false;
        }
        crate::wire::write_pad(out, self.session, &updates);
        true
    }

    pub fn next_deadline(&self) -> Option<Instant> {
        self.pads.next_deadline()
    }
}

/// The host's side of rumble and lights: each slot's newest feedback, until
/// the viewer has it.
pub struct FeedbackSender {
    session: u32,
    slots: LatestSender<Feedback>,
}

impl FeedbackSender {
    pub fn new(session: u32) -> Self {
        Self {
            session,
            slots: LatestSender::new(),
        }
    }

    /// Returns false when the slot already had this feedback.
    pub fn set(&mut self, slot: u8, feedback: Feedback) -> bool {
        self.slots.set(slot, feedback)
    }

    pub fn ack(&mut self, ack: Ack) {
        self.slots.ack(ack);
    }

    /// Writes a `PadFeedback` datagram into `out` and returns true when one
    /// is due.
    pub fn poll(&mut self, now: Instant, out: &mut Vec<u8>) -> bool {
        if !self.slots.due(now) {
            return false;
        }
        let updates: Vec<FeedbackUpdate> = self
            .slots
            .unacked()
            .map(|(slot, seq, feedback)| FeedbackUpdate {
                slot,
                seq,
                feedback: *feedback,
            })
            .collect();
        self.slots.sent(now);
        if updates.is_empty() {
            return false;
        }
        crate::wire::write_pad_feedback(out, self.session, &updates);
        true
    }

    pub fn next_deadline(&self) -> Option<Instant> {
        self.slots.next_deadline()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn full_state() -> PadState {
        PadState {
            buttons: button::SOUTH | button::GUIDE | button::PADDLE4,
            left_x: -32768,
            left_y: 32767,
            right_x: -1,
            right_y: 1234,
            left_trigger: 65535,
            right_trigger: 7,
            motion: Some(Motion {
                gyro: [1, -2, 3],
                accel: [-8192, 0, 8192],
            }),
            touch: Some([
                Touch {
                    down: true,
                    id: 5,
                    x: 100,
                    y: 65535,
                },
                Touch::default(),
            ]),
        }
    }

    #[test]
    fn snapshots_round_trip_with_and_without_extras() {
        for state in [
            PadState::default(),
            full_state(),
            PadState {
                touch: None,
                ..full_state()
            },
            PadState {
                motion: None,
                ..full_state()
            },
        ] {
            for kind in [
                None,
                Some(PadKind::Xbox),
                Some(PadKind::PlayStation),
                Some(PadKind::Generic),
            ] {
                let u = PadUpdate {
                    slot: 15,
                    seq: u32::MAX,
                    kind,
                    state,
                };
                let mut out = Vec::new();
                u.write(&mut out);
                assert_eq!(out.len(), u.wire_len());
                assert_eq!(PadUpdate::read(&out), Some((u, out.len())));
                assert_eq!(PadUpdate::read(&out[..out.len() - 1]), None);
            }
        }
    }

    #[test]
    fn malformed_snapshots_are_refused() {
        let mut out = Vec::new();
        PadUpdate {
            slot: 0,
            seq: 1,
            kind: Some(PadKind::Xbox),
            state: PadState::default(),
        }
        .write(&mut out);
        for (at, value) in [(0, MAX_PADS as u8), (1, 4), (2, 4), (3, 1)] {
            let mut bad = out.clone();
            bad[at] = value;
            assert_eq!(PadUpdate::read(&bad), None, "byte {at} = {value}");
        }
    }

    #[test]
    fn feedback_round_trips() {
        for led in [None, Some([1, 2, 3])] {
            let u = FeedbackUpdate {
                slot: 3,
                seq: 9,
                feedback: Feedback {
                    large: 65535,
                    small: 1,
                    left_trigger: 2,
                    right_trigger: 3,
                    led,
                    player: 2,
                },
            };
            let mut out = Vec::new();
            u.write(&mut out);
            assert_eq!(out.len(), FEEDBACK_LEN);
            assert_eq!(FeedbackUpdate::read(&out), Some(u));
        }
    }

    #[test]
    fn ack_and_feedback_datagrams_parse_strictly() {
        use crate::wire::{self, Datagram};
        let mut out = Vec::new();
        let acks = [Ack { slot: 1, seq: 7 }, Ack { slot: 15, seq: 0 }];
        wire::write_pad_ack(&mut out, 9, &acks);
        let Ok(Datagram::PadAck {
            session: 9,
            acks: got,
        }) = wire::parse(&out)
        else {
            panic!("not a pad ack");
        };
        assert_eq!(got.iter().collect::<Vec<_>>(), acks);
        wire::write_pad_feedback_ack(&mut out, 9, &acks);
        assert!(matches!(
            wire::parse(&out),
            Ok(Datagram::PadFeedbackAck { .. })
        ));
        let mut bad = out.clone();
        bad[9 + ACK_LEN] = MAX_PADS as u8;
        assert!(wire::parse(&bad).is_err());
        assert!(wire::parse(&out[..out.len() - 1]).is_err());
        out.push(0);
        assert!(wire::parse(&out).is_err());

        let f = FeedbackUpdate {
            slot: 2,
            seq: 3,
            feedback: Feedback {
                large: 1,
                ..Default::default()
            },
        };
        let mut s = FeedbackSender::new(9);
        assert!(s.set(2, f.feedback));
        assert!(!s.set(2, f.feedback));
        assert!(s.poll(Instant::now(), &mut out));
        let Ok(Datagram::PadFeedback { updates, .. }) = wire::parse(&out) else {
            panic!("not feedback");
        };
        assert_eq!(updates.iter().next().map(|u| u.feedback), Some(f.feedback));
        let mut bad = out.clone();
        bad[9 + 19] = 1;
        assert!(wire::parse(&bad).is_err());
        // An empty pad datagram is malformed.
        wire::write_pad_ack(&mut out, 9, &acks[..1]);
        out[0] = 9;
        out.truncate(9);
        out[8] = 0;
        assert!(wire::parse(&out).is_err());
    }

    #[test]
    fn a_sender_resends_the_newest_until_acknowledged() {
        let t = Instant::now();
        let mut s = PadSender::new(7);
        let mut out = Vec::new();
        assert!(!s.poll(t, &mut out));
        let a = PadState {
            buttons: button::SOUTH,
            ..Default::default()
        };
        s.update(0, PadKind::Xbox, a);
        assert!(s.poll(t, &mut out));
        // Nothing new and the resend interval not up: quiet.
        assert!(!s.poll(t + RESEND / 2, &mut out));
        assert!(s.poll(t + RESEND, &mut out));
        // A newer state replaces the old one in the next datagram.
        let b = PadState::default();
        s.update(0, PadKind::Xbox, b);
        assert!(s.poll(t + RESEND, &mut out));
        let crate::wire::Datagram::Pad { updates, .. } = crate::wire::parse(&out).unwrap() else {
            panic!("not a pad datagram");
        };
        let sent: Vec<_> = updates.iter().collect();
        assert_eq!(sent.len(), 1);
        assert_eq!((sent[0].seq, sent[0].state), (2, b));
        // An acknowledgement of the older one settles nothing.
        s.ack(Ack { slot: 0, seq: 1 });
        assert!(s.poll(t + RESEND * 2, &mut out));
        s.ack(Ack { slot: 0, seq: 2 });
        assert!(!s.poll(t + RESEND * 4, &mut out));
        assert_eq!(s.next_deadline(), None);
        // The same state again is not a change.
        s.update(0, PadKind::Xbox, b);
        assert!(!s.poll(t + RESEND * 5, &mut out));
    }

    #[test]
    fn every_unacknowledged_pad_rides_in_each_datagram() {
        let t = Instant::now();
        let mut s = PadSender::new(1);
        let mut out = Vec::new();
        for slot in 0..MAX_PADS as u8 {
            s.update(slot, PadKind::PlayStation, full_state());
        }
        assert!(s.poll(t, &mut out));
        assert!(out.len() <= crate::MAX_DATAGRAM);
        let crate::wire::Datagram::Pad { updates, .. } = crate::wire::parse(&out).unwrap() else {
            panic!("not a pad datagram");
        };
        assert_eq!(updates.count(), MAX_PADS);
        s.ack(Ack { slot: 3, seq: 1 });
        s.update(0, PadKind::PlayStation, PadState::default());
        assert!(s.poll(t, &mut out));
        let crate::wire::Datagram::Pad { updates, .. } = crate::wire::parse(&out).unwrap() else {
            panic!("not a pad datagram");
        };
        assert_eq!(updates.count(), MAX_PADS - 1);
    }

    #[test]
    fn a_removed_pad_is_sent_as_gone_once_and_unused_slots_stay_silent() {
        let t = Instant::now();
        let mut s = PadSender::new(1);
        let mut out = Vec::new();
        s.remove(4);
        assert!(!s.poll(t, &mut out));
        s.update(4, PadKind::Generic, PadState::default());
        assert!(s.poll(t, &mut out));
        s.remove(4);
        assert!(s.poll(t, &mut out));
        let crate::wire::Datagram::Pad { updates, .. } = crate::wire::parse(&out).unwrap() else {
            panic!("not a pad datagram");
        };
        let u = updates.iter().next().unwrap();
        assert_eq!((u.slot, u.seq, u.kind), (4, 2, None));
        s.ack(Ack { slot: 4, seq: 2 });
        s.remove(4);
        assert!(!s.poll(t + RESEND, &mut out));
    }

    #[test]
    fn a_receiver_drops_stale_and_repeated_snapshots() {
        let mut r = LatestReceiver::new();
        assert_eq!(r.ack(0), None);
        assert!(r.accept(0, 5));
        assert!(!r.accept(0, 5));
        assert!(!r.accept(0, 4));
        assert!(r.accept(0, 9));
        assert_eq!(r.ack(0), Some(Ack { slot: 0, seq: 9 }));
        // Wraps.
        assert!(r.accept(1, u32::MAX));
        assert!(r.accept(1, 0));
        assert!(!r.accept(1, u32::MAX));
        assert!(!r.accept(MAX_PADS as u8, 1));
    }

    #[test]
    fn a_lossy_reordering_link_ends_on_the_newest_state() {
        // Ten seconds of a pad changing every millisecond over a link that
        // drops a third of datagrams and delivers the rest out of order.
        let t0 = Instant::now();
        let mut s = PadSender::new(1);
        let mut r = LatestReceiver::new();
        let mut host = PadState::default();
        let mut in_flight: Vec<Vec<u8>> = Vec::new();
        let mut out = Vec::new();
        let mut rng = 0x9e3779b9u32;
        let mut next = || {
            rng ^= rng << 13;
            rng ^= rng >> 17;
            rng ^= rng << 5;
            rng
        };
        let mut last = PadState::default();
        for ms in 0..10_000u32 {
            let now = t0 + Duration::from_millis(ms as u64);
            if ms < 9_000 {
                last = PadState {
                    left_x: (ms % 30_000) as i16,
                    ..Default::default()
                };
                s.update(0, PadKind::Xbox, last);
            }
            if s.poll(now, &mut out) {
                in_flight.push(out.clone());
            }
            while in_flight.len() > 3 || (!in_flight.is_empty() && next() % 2 == 0) {
                let i = next() as usize % in_flight.len();
                let d = in_flight.swap_remove(i);
                if next() % 3 == 0 {
                    continue;
                }
                let crate::wire::Datagram::Pad { updates, .. } = crate::wire::parse(&d).unwrap()
                else {
                    panic!()
                };
                for u in updates.iter() {
                    if r.accept(u.slot, u.seq) {
                        host = u.state;
                    }
                    if next() % 3 != 0 {
                        s.ack(r.ack(u.slot).unwrap());
                    }
                }
            }
        }
        assert_eq!(host, last);
        assert_eq!(s.next_deadline(), None);
    }
}
