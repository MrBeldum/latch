//! Gamepads into the desktop: one virtual pad per viewer pad, rumble and
//! lights back. Windows plugs pads into the ViGEmBus driver
//! (`windows/vigem.rs`), Linux makes them with uinput (`linux/uinput.rs`);
//! a Mac has no way to add a gamepad without a driver, so a Mac host takes
//! none.

use latch_proto::gamepad::{Ack, FeedbackSender, MAX_PADS};
pub use latch_proto::gamepad::{Feedback, PadKind, PadState};
use parking_lot::{Condvar, Mutex};
use std::sync::Arc;
use std::time::{Duration, Instant};

/// Where a pad's rumble and lights go: its slot and the newest feedback.
pub type FeedbackFn = Arc<dyn Fn(u8, Feedback) + Send + Sync>;

pub trait PadSink: Send {
    /// Plugs in, updates, or (`kind` None) unplugs the pad in `slot`.
    fn update(&mut self, slot: u8, kind: Option<PadKind>, state: &PadState) -> anyhow::Result<()>;

    /// Unplugs every pad (the viewer left).
    fn unplug_all(&mut self);
}

/// An opened sink and what it can do, for the `Accept`.
pub struct Opened {
    pub sink: Box<dyn PadSink>,
    pub slots: u8,
    pub kinds: Vec<PadKind>,
}

/// Opens this platform's virtual pads. `feedback` gets their rumble. A
/// test-pattern host gets pads that only remember, unless
/// `LATCH_TEST_GAMEPADS=system` asks for the platform's own.
pub fn open(feedback: FeedbackFn) -> anyhow::Result<Opened> {
    if crate::testmode::motion().is_some()
        && std::env::var("LATCH_TEST_GAMEPADS").as_deref() != Ok("system")
    {
        return Ok(Pads::opened(crate::testmode::TestBus, feedback));
    }
    #[cfg(windows)]
    return crate::windows::open_pads(feedback);
    #[cfg(target_os = "linux")]
    return crate::linux::open_pads(feedback);
    #[cfg(target_os = "macos")]
    {
        let _ = feedback;
        anyhow::bail!("a Mac cannot add a virtual gamepad without a driver")
    }
}

/// One platform's way of making virtual pads. A `Pad` unplugs when dropped.
pub(crate) trait VirtualBus: Send {
    type Pad: Send;
    /// Pads at once.
    const SLOTS: u8;
    /// The kinds this bus emulates as themselves; others become Xbox pads.
    fn kinds(&self) -> &'static [PadKind];
    fn plug(&mut self, slot: u8, kind: PadKind, feedback: FeedbackFn) -> anyhow::Result<Self::Pad>;
    fn submit(&mut self, pad: &mut Self::Pad, state: &PadState) -> anyhow::Result<()>;
}

/// A [`VirtualBus`] as a [`PadSink`]: plugs a pad in on its first state,
/// in again as another kind when the viewer's pad in that slot changes kind,
/// and out when it goes.
pub(crate) struct Pads<B: VirtualBus> {
    bus: B,
    pads: Vec<Option<(PadKind, B::Pad)>>,
    feedback: FeedbackFn,
}

impl<B: VirtualBus + 'static> Pads<B> {
    pub(crate) fn opened(bus: B, feedback: FeedbackFn) -> Opened {
        let kinds = bus.kinds().to_vec();
        Opened {
            sink: Box::new(Self {
                bus,
                pads: (0..B::SLOTS).map(|_| None).collect(),
                feedback,
            }),
            slots: B::SLOTS,
            kinds,
        }
    }
}

impl<B: VirtualBus> PadSink for Pads<B> {
    fn update(&mut self, slot: u8, kind: Option<PadKind>, state: &PadState) -> anyhow::Result<()> {
        let Some(entry) = self.pads.get_mut(slot as usize) else {
            anyhow::bail!("no gamepad slot {slot}");
        };
        let Some(kind) = kind else {
            *entry = None;
            return Ok(());
        };
        let kind = if self.bus.kinds().contains(&kind) {
            kind
        } else {
            PadKind::Xbox
        };
        if entry.as_ref().is_some_and(|(k, _)| *k != kind) {
            *entry = None;
        }
        if entry.is_none() {
            *entry = Some((kind, self.bus.plug(slot, kind, self.feedback.clone())?));
        }
        let (_, pad) = entry.as_mut().expect("plugged above");
        self.bus.submit(pad, state)
    }

    fn unplug_all(&mut self) {
        for pad in &mut self.pads {
            *pad = None;
        }
    }
}

/// Rumble and lights from the sink's threads to the viewer: keeps each
/// slot's newest feedback and sends it until acknowledged (`docs/PROTOCOL.md`,
/// "Gamepads").
#[derive(Default)]
pub(crate) struct FeedbackHub {
    state: Mutex<HubState>,
    cv: Condvar,
}

#[derive(Default)]
struct HubState {
    sender: Option<FeedbackSender>,
    stop: bool,
}

impl FeedbackHub {
    pub(crate) fn feedback_fn(self: &Arc<Self>) -> FeedbackFn {
        let hub = self.clone();
        Arc::new(move |slot, feedback| hub.set(slot, feedback))
    }

    fn set(&self, slot: u8, feedback: Feedback) {
        if let Some(sender) = self.state.lock().sender.as_mut() {
            if sender.set(slot, feedback) {
                self.cv.notify_one();
            }
        }
    }

    pub(crate) fn ack(&self, ack: Ack) {
        if let Some(sender) = self.state.lock().sender.as_mut() {
            sender.ack(ack);
        }
    }

    /// Feedback from now on is for this session.
    pub(crate) fn start(&self, session: u32) {
        self.state.lock().sender = Some(FeedbackSender::new(session));
    }

    pub(crate) fn stop(&self) {
        self.state.lock().stop = true;
        self.cv.notify_all();
    }

    /// Sends each due `PadFeedback` datagram through `send` until
    /// [`FeedbackHub::stop`].
    pub(crate) fn run(&self, mut send: impl FnMut(&[u8])) {
        let mut out = Vec::with_capacity(latch_proto::MAX_DATAGRAM);
        let mut state = self.state.lock();
        while !state.stop {
            let now = Instant::now();
            let Some(sender) = state.sender.as_mut() else {
                self.cv.wait_for(&mut state, Duration::from_millis(100));
                continue;
            };
            if sender.poll(now, &mut out) {
                drop(state);
                send(&out);
                state = self.state.lock();
                continue;
            }
            let deadline = sender
                .next_deadline()
                .unwrap_or(now + Duration::from_secs(1));
            self.cv.wait_until(&mut state, deadline.max(now));
        }
    }
}

/// What the server keeps per session for its pads.
pub(crate) struct SessionPads {
    pub receiver: latch_proto::gamepad::LatestReceiver,
    pub sink: Option<Box<dyn PadSink>>,
    pub slots: u8,
    /// Slots whose last update failed, so a failure is logged once.
    pub failing: [bool; MAX_PADS],
}

/// What each platform's virtual pads are sent, from a [`PadState`]. Pure,
/// so every mapping is tested on every platform.
pub(crate) mod map {
    use latch_proto::gamepad::{button, PadState};

    /// A d-pad's direction, from −1/0/1 per axis; opposite presses cancel.
    #[cfg_attr(not(any(windows, target_os = "linux", test)), allow(dead_code))]
    fn dpad(buttons: u32) -> (i32, i32) {
        let axis = |neg, pos| (buttons & pos != 0) as i32 - (buttons & neg != 0) as i32;
        (
            axis(button::DPAD_LEFT, button::DPAD_RIGHT),
            axis(button::DPAD_UP, button::DPAD_DOWN),
        )
    }

    /// An Xbox 360 pad's `XUSB_REPORT`: buttons, triggers, sticks (up
    /// positive, as XInput has them).
    #[cfg_attr(not(any(windows, test)), allow(dead_code))]
    pub fn xusb(s: &PadState) -> [u8; 12] {
        const TABLE: [(u32, u16); 16] = [
            (button::DPAD_UP, 0x0001),
            (button::DPAD_DOWN, 0x0002),
            (button::DPAD_LEFT, 0x0004),
            (button::DPAD_RIGHT, 0x0008),
            (button::START, 0x0010),
            (button::BACK, 0x0020),
            // A PlayStation pad's touchpad click is its other select button.
            (button::TOUCHPAD, 0x0020),
            (button::LEFT_STICK, 0x0040),
            (button::RIGHT_STICK, 0x0080),
            (button::LEFT_SHOULDER, 0x0100),
            (button::RIGHT_SHOULDER, 0x0200),
            (button::GUIDE, 0x0400),
            (button::SOUTH, 0x1000),
            (button::EAST, 0x2000),
            (button::WEST, 0x4000),
            (button::NORTH, 0x8000),
        ];
        let buttons = TABLE
            .iter()
            .filter(|(b, _)| s.buttons & b != 0)
            .fold(0u16, |n, (_, x)| n | x);
        let mut out = [0u8; 12];
        out[0..2].copy_from_slice(&buttons.to_le_bytes());
        out[2] = (s.left_trigger >> 8) as u8;
        out[3] = (s.right_trigger >> 8) as u8;
        for (i, v) in [s.left_x, s.left_y, s.right_x, s.right_y]
            .iter()
            .enumerate()
        {
            out[4 + 2 * i..6 + 2 * i].copy_from_slice(&v.to_le_bytes());
        }
        out
    }

    /// A DualShock 4's `DS4_REPORT` (with C layout's padding: 10 bytes):
    /// sticks 0–255 with 128 centred and down positive, buttons with the
    /// d-pad as a hat in the low nibble, the PS and touchpad buttons, and
    /// triggers.
    #[cfg_attr(not(any(windows, test)), allow(dead_code))]
    pub fn ds4(s: &PadState) -> [u8; 10] {
        let x = |v: i16| ((v as i32 + 32768) >> 8) as u8;
        let y = |v: i16| x(v.saturating_neg());
        const TABLE: [(u32, u16); 10] = [
            (button::WEST, 1 << 4),
            (button::SOUTH, 1 << 5),
            (button::EAST, 1 << 6),
            (button::NORTH, 1 << 7),
            (button::LEFT_SHOULDER, 1 << 8),
            (button::RIGHT_SHOULDER, 1 << 9),
            (button::BACK, 1 << 12),
            (button::START, 1 << 13),
            (button::LEFT_STICK, 1 << 14),
            (button::RIGHT_STICK, 1 << 15),
        ];
        let mut buttons = TABLE
            .iter()
            .filter(|(b, _)| s.buttons & b != 0)
            .fold(0u16, |n, (_, x)| n | x);
        // The digital L2 and R2 a real pad reports past the trigger's slack.
        const PULLED: u16 = 0x0400;
        if s.left_trigger > PULLED {
            buttons |= 1 << 10;
        }
        if s.right_trigger > PULLED {
            buttons |= 1 << 11;
        }
        buttons |= match dpad(s.buttons) {
            (0, -1) => 0,
            (1, -1) => 1,
            (1, 0) => 2,
            (1, 1) => 3,
            (0, 1) => 4,
            (-1, 1) => 5,
            (-1, 0) => 6,
            (-1, -1) => 7,
            _ => 8,
        };
        let special = (s.buttons & button::GUIDE != 0) as u8
            | ((s.buttons & button::TOUCHPAD != 0) as u8) << 1;
        let b = buttons.to_le_bytes();
        [
            x(s.left_x),
            y(s.left_y),
            x(s.right_x),
            y(s.right_y),
            b[0],
            b[1],
            special,
            (s.left_trigger >> 8) as u8,
            (s.right_trigger >> 8) as u8,
            0,
        ]
    }

    /// Linux key codes (`input-event-codes.h`) of an Xbox-layout pad, as the
    /// kernel's `xpad` driver reports them.
    #[cfg_attr(not(target_os = "linux"), allow(dead_code))]
    pub const EV_KEYS: [(u32, u16); 11] = [
        (button::SOUTH, 0x130),          // BTN_SOUTH
        (button::EAST, 0x131),           // BTN_EAST
        (button::NORTH, 0x133),          // BTN_NORTH
        (button::WEST, 0x134),           // BTN_WEST
        (button::LEFT_SHOULDER, 0x136),  // BTN_TL
        (button::RIGHT_SHOULDER, 0x137), // BTN_TR
        (button::BACK, 0x13a),           // BTN_SELECT
        (button::START, 0x13b),          // BTN_START
        (button::GUIDE, 0x13c),          // BTN_MODE
        (button::LEFT_STICK, 0x13d),     // BTN_THUMBL
        (button::RIGHT_STICK, 0x13e),    // BTN_THUMBR
    ];

    /// The axes an `xpad` pad reports, in `ABS_*` codes, with evdev's down
    /// positive: X, Y, RX, RY (−32768 to 32767), Z and RZ (triggers, 0 to
    /// 1023 as an Xbox One pad has them), HAT0X and HAT0Y (−1 to 1).
    #[cfg_attr(not(any(target_os = "linux", test)), allow(dead_code))]
    pub fn ev_axes(s: &PadState) -> [(u16, i32); 8] {
        let down = |v: i16| -(v as i32) - 1;
        let (hx, hy) = dpad(s.buttons);
        [
            (0x00, s.left_x as i32),
            (0x01, down(s.left_y)),
            (0x03, s.right_x as i32),
            (0x04, down(s.right_y)),
            (0x02, (s.left_trigger >> 6) as i32),
            (0x05, (s.right_trigger >> 6) as i32),
            (0x10, hx),
            (0x11, hy),
        ]
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn xbox_reports_carry_buttons_triggers_and_sticks_as_xinput_has_them() {
        use latch_proto::gamepad::button;
        let r = map::xusb(&PadState {
            buttons: button::SOUTH | button::GUIDE | button::DPAD_RIGHT | button::TOUCHPAD,
            left_x: -32768,
            left_y: 32767,
            right_x: 1,
            right_y: -1,
            left_trigger: 65535,
            right_trigger: 0x80ff,
            ..Default::default()
        });
        assert_eq!(
            u16::from_le_bytes([r[0], r[1]]),
            0x1000 | 0x0400 | 0x0008 | 0x0020
        );
        assert_eq!((r[2], r[3]), (255, 0x80));
        assert_eq!(i16::from_le_bytes([r[4], r[5]]), -32768);
        assert_eq!(i16::from_le_bytes([r[6], r[7]]), 32767);
        assert_eq!(i16::from_le_bytes([r[10], r[11]]), -1);
    }

    #[test]
    fn dualshock_reports_centre_sticks_flip_y_and_use_a_hat() {
        use latch_proto::gamepad::button;
        let neutral = map::ds4(&PadState::default());
        assert_eq!(neutral, [128, 128, 128, 128, 8, 0, 0, 0, 0, 0]);
        let r = map::ds4(&PadState {
            buttons: button::SOUTH | button::DPAD_UP | button::DPAD_LEFT | button::GUIDE,
            left_y: 32767,
            right_y: -32768,
            left_x: -32768,
            right_trigger: 65535,
            ..Default::default()
        });
        assert_eq!((r[0], r[1], r[3]), (0, 0, 255));
        let buttons = u16::from_le_bytes([r[4], r[5]]);
        assert_eq!(buttons & 0xf, 7, "north-west");
        assert_ne!(buttons & 1 << 5, 0, "cross");
        assert_ne!(buttons & 1 << 11, 0, "digital R2");
        assert_eq!(buttons & 1 << 10, 0, "no L2");
        assert_eq!((r[6], r[8]), (1, 255));
        // Opposite directions cancel.
        let r = map::ds4(&PadState {
            buttons: button::DPAD_UP | button::DPAD_DOWN,
            ..Default::default()
        });
        assert_eq!(r[4] & 0xf, 8);
    }

    #[test]
    fn linux_axes_point_down_and_triggers_span_ten_bits() {
        use latch_proto::gamepad::button;
        let a = map::ev_axes(&PadState {
            buttons: button::DPAD_DOWN | button::DPAD_LEFT,
            left_y: 32767,
            right_y: -32768,
            left_trigger: 65535,
            ..Default::default()
        });
        assert_eq!(a[1], (0x01, -32768));
        assert_eq!(a[3], (0x04, 32767));
        assert_eq!(a[4], (0x02, 1023));
        assert_eq!((a[6].1, a[7].1), (-1, 1));
    }

    struct Recorder(Arc<Mutex<Vec<String>>>);
    struct RecordedPad(Arc<Mutex<Vec<String>>>, u8);
    impl Drop for RecordedPad {
        fn drop(&mut self) {
            self.0.lock().push(format!("unplug {}", self.1));
        }
    }

    impl VirtualBus for Recorder {
        type Pad = RecordedPad;
        const SLOTS: u8 = 4;
        fn kinds(&self) -> &'static [PadKind] {
            &[PadKind::Xbox, PadKind::PlayStation]
        }
        fn plug(&mut self, slot: u8, kind: PadKind, _: FeedbackFn) -> anyhow::Result<RecordedPad> {
            self.0.lock().push(format!("plug {slot} {kind:?}"));
            Ok(RecordedPad(self.0.clone(), slot))
        }
        fn submit(&mut self, pad: &mut RecordedPad, state: &PadState) -> anyhow::Result<()> {
            self.0
                .lock()
                .push(format!("submit {} {}", pad.1, state.buttons));
            Ok(())
        }
    }

    #[test]
    fn pads_plug_on_first_state_replug_on_a_new_kind_and_unplug_when_gone() {
        let log = Arc::new(Mutex::new(Vec::new()));
        let mut opened = Pads::opened(Recorder(log.clone()), Arc::new(|_, _| {}));
        assert_eq!(opened.slots, 4);
        let s = |buttons| PadState {
            buttons,
            ..Default::default()
        };
        let sink = &mut opened.sink;
        sink.update(1, Some(PadKind::PlayStation), &s(1)).unwrap();
        sink.update(1, Some(PadKind::PlayStation), &s(2)).unwrap();
        // Generic is not emulated here: it becomes an Xbox pad.
        sink.update(1, Some(PadKind::Generic), &s(3)).unwrap();
        sink.update(1, Some(PadKind::Xbox), &s(4)).unwrap();
        sink.update(1, None, &s(0)).unwrap();
        assert!(sink.update(4, Some(PadKind::Xbox), &s(0)).is_err());
        sink.update(0, Some(PadKind::Xbox), &s(5)).unwrap();
        sink.unplug_all();
        assert_eq!(
            *log.lock(),
            [
                "plug 1 PlayStation",
                "submit 1 1",
                "submit 1 2",
                "unplug 1",
                "plug 1 Xbox",
                "submit 1 3",
                "submit 1 4",
                "unplug 1",
                "plug 0 Xbox",
                "submit 0 5",
                "unplug 0",
            ]
        );
    }

    #[test]
    fn the_hub_sends_feedback_until_acknowledged() {
        let hub = Arc::new(FeedbackHub::default());
        let feedback = hub.feedback_fn();
        // Before a session starts, nothing is kept.
        feedback(0, Feedback::default());
        hub.start(7);
        let (tx, rx) = std::sync::mpsc::channel();
        let runner = {
            let hub = hub.clone();
            std::thread::spawn(move || hub.run(|d| tx.send(d.to_vec()).unwrap()))
        };
        feedback(
            2,
            Feedback {
                large: 100,
                ..Default::default()
            },
        );
        let first = rx.recv_timeout(Duration::from_secs(1)).unwrap();
        let again = rx.recv_timeout(Duration::from_secs(1)).unwrap();
        assert_eq!(first, again);
        let Ok(latch_proto::wire::Datagram::PadFeedback { updates, .. }) =
            latch_proto::wire::parse(&first)
        else {
            panic!("not feedback");
        };
        let u = updates.iter().next().unwrap();
        assert_eq!((u.slot, u.feedback.large), (2, 100));
        hub.ack(Ack {
            slot: 2,
            seq: u.seq,
        });
        while rx.recv_timeout(Duration::from_millis(50)).is_ok() {}
        assert!(rx.recv_timeout(latch_proto::gamepad::RESEND * 3).is_err());
        hub.stop();
        runner.join().unwrap();
    }
}
