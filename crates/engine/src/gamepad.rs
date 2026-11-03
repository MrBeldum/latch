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

/// Opens this platform's virtual pads. `feedback` gets their rumble.
pub fn open(feedback: FeedbackFn) -> anyhow::Result<Opened> {
    if crate::testmode::motion().is_some() {
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

#[cfg(test)]
mod tests {
    use super::*;

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
