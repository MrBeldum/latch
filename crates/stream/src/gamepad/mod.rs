//! The viewer's own gamepads: which are connected, each one's state the
//! moment it changes, and rumble and lights from the host back out to them.
//! The GameController framework on a Mac (`mac.rs`), Windows.Gaming.Input
//! polled at 1 kHz on Windows (`windows.rs`), evdev on Linux (`linux.rs`).
//!
//! Pads get the lowest free slot when they connect and keep it until they
//! go. A stream [`Pads::attach`]es to send states to its host; the pads
//! keep being tracked between streams, and a new stream gets every
//! connected pad's current state at once, so the host plugs them in before
//! anyone touches a stick.

use latch_proto::gamepad::MAX_PADS;
pub use latch_proto::gamepad::{Feedback, PadKind, PadState};
use parking_lot::Mutex;
use std::sync::Arc;

#[cfg(target_os = "linux")]
mod linux;
#[cfg(target_os = "macos")]
mod mac;
#[cfg(windows)]
mod windows;

/// Where pad states go: the slot, and the pad's kind and state, or None
/// when it went away.
pub type PadFn = Arc<dyn Fn(u8, Option<(PadKind, PadState)>) + Send + Sync>;

/// A connected pad, for the toolbar.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PadInfo {
    pub slot: u8,
    pub kind: PadKind,
    /// As the system names it ("Xbox Wireless Controller", "DualSense
    /// Wireless Controller").
    pub name: String,
}

/// What a platform's capture can be asked to do.
trait Backend: Send + Sync {
    fn rumble(&self, slot: u8, feedback: Feedback);
}

/// Every connected pad and where their states go, shared with the
/// platform's capture threads.
#[derive(Default)]
pub(crate) struct Shared {
    pads: Mutex<Vec<Option<(PadInfo, PadState)>>>,
    /// Where states go, and which [`Pads::attach`] put it there.
    sink: Mutex<Option<PadFn>>,
    attached: std::sync::atomic::AtomicU64,
    /// Called (on a capture thread) when a pad connects or goes, so a
    /// window can redraw its list.
    changed: Mutex<Option<Arc<dyn Fn() + Send + Sync>>>,
}

impl Shared {
    /// A pad connected: its slot, or None when all are taken.
    pub(crate) fn connect(&self, kind: PadKind, name: String) -> Option<u8> {
        let slot = {
            let mut pads = self.pads.lock();
            if pads.is_empty() {
                pads.resize(MAX_PADS, None);
            }
            let slot = pads.iter().position(Option::is_none)? as u8;
            pads[slot as usize] = Some((PadInfo { slot, kind, name }, PadState::default()));
            slot
        };
        tracing::info!("gamepad {} connected: {kind:?}", slot + 1);
        self.send(slot, Some((kind, PadState::default())));
        self.notify();
        Some(slot)
    }

    /// The pad in `slot` reads `state` now.
    pub(crate) fn update(&self, slot: u8, state: PadState) {
        let kind = {
            let mut pads = self.pads.lock();
            let Some(Some((info, last))) = pads.get_mut(slot as usize) else {
                return;
            };
            if *last == state {
                return;
            }
            *last = state;
            info.kind
        };
        self.send(slot, Some((kind, state)));
    }

    pub(crate) fn disconnect(&self, slot: u8) {
        if let Some(entry) = self.pads.lock().get_mut(slot as usize) {
            if entry.take().is_none() {
                return;
            }
        }
        tracing::info!("gamepad {} disconnected", slot + 1);
        self.send(slot, None);
        self.notify();
    }

    fn send(&self, slot: u8, pad: Option<(PadKind, PadState)>) {
        let sink = self.sink.lock().clone();
        if let Some(sink) = sink {
            sink(slot, pad);
        }
    }

    fn notify(&self) {
        let changed = self.changed.lock().clone();
        if let Some(f) = changed {
            f();
        }
    }
}

/// This machine's gamepads.
pub struct Pads {
    shared: Arc<Shared>,
    backend: Option<Box<dyn Backend>>,
}

impl Pads {
    /// Starts watching for pads. Where the platform has no way to read
    /// them, the list just stays empty.
    pub fn start() -> Self {
        let shared = Arc::new(Shared::default());
        #[cfg(target_os = "linux")]
        let backend = linux::start(shared.clone());
        #[cfg(target_os = "macos")]
        let backend = mac::start(shared.clone());
        #[cfg(windows)]
        let backend = windows::start(shared.clone());
        #[cfg(not(any(target_os = "linux", target_os = "macos", windows)))]
        let backend: Option<Box<dyn Backend>> = None;
        Self { shared, backend }
    }

    /// The pads connected now, by slot.
    pub fn connected(&self) -> Vec<PadInfo> {
        self.shared
            .pads
            .lock()
            .iter()
            .flatten()
            .map(|(info, _)| info.clone())
            .collect()
    }

    /// Sends every pad's state to `sink` from now on, starting with each
    /// connected pad's current state; `None` stops sending. Returns what
    /// [`Pads::detach`] takes.
    pub fn attach(&self, sink: Option<PadFn>) -> u64 {
        let id = self
            .shared
            .attached
            .fetch_add(1, std::sync::atomic::Ordering::AcqRel)
            + 1;
        *self.shared.sink.lock() = sink.clone();
        if let Some(sink) = sink {
            let now: Vec<_> = self
                .shared
                .pads
                .lock()
                .iter()
                .flatten()
                .map(|(info, state)| (info.slot, info.kind, *state))
                .collect();
            for (slot, kind, state) in now {
                sink(slot, Some((kind, state)));
            }
        }
        id
    }

    /// Stops sending, unless something attached since `id`.
    pub fn detach(&self, id: u64) {
        let mut sink = self.shared.sink.lock();
        if self
            .shared
            .attached
            .load(std::sync::atomic::Ordering::Acquire)
            == id
        {
            *sink = None;
        }
    }

    /// Called whenever a pad connects or goes.
    pub fn on_change(&self, f: impl Fn() + Send + Sync + 'static) {
        *self.shared.changed.lock() = Some(Arc::new(f));
    }

    /// Rumbles (and lights) the pad in `slot`, as far as it can.
    pub fn rumble(&self, slot: u8, feedback: Feedback) {
        if let Some(b) = &self.backend {
            b.rumble(slot, feedback);
        }
    }

    /// Stops every pad's rumble (a stream ended).
    pub fn quiet(&self) {
        for pad in self.connected() {
            self.rumble(pad.slot, Feedback::default());
        }
    }
}

/// −1.0…1.0 (up and right positive) as a stick value.
#[cfg_attr(target_os = "linux", allow(dead_code))]
pub(crate) fn stick(v: f64) -> i16 {
    (v.clamp(-1.0, 1.0) * 32767.0).round() as i16
}

/// 0.0…1.0 as a trigger value.
#[cfg_attr(target_os = "linux", allow(dead_code))]
pub(crate) fn trigger(v: f64) -> u16 {
    (v.clamp(0.0, 1.0) * 65535.0).round() as u16
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pads_take_the_lowest_free_slot_and_a_new_stream_gets_their_state() {
        let shared = Arc::new(Shared::default());
        let pads = Pads {
            shared: shared.clone(),
            backend: None,
        };
        let a = shared.connect(PadKind::Xbox, "a".into()).unwrap();
        let b = shared.connect(PadKind::PlayStation, "b".into()).unwrap();
        assert_eq!((a, b), (0, 1));
        let pressed = PadState {
            buttons: 1,
            ..Default::default()
        };
        shared.update(b, pressed);
        shared.disconnect(a);
        assert_eq!(shared.connect(PadKind::Generic, "c".into()), Some(0));
        let seen = Arc::new(Mutex::new(Vec::new()));
        let log = seen.clone();
        pads.attach(Some(Arc::new(move |slot, pad| {
            log.lock().push((slot, pad))
        })));
        assert_eq!(
            *seen.lock(),
            [
                (0, Some((PadKind::Generic, PadState::default()))),
                (1, Some((PadKind::PlayStation, pressed))),
            ]
        );
        // An unchanged state is not sent again; a change and a removal are.
        shared.update(1, pressed);
        shared.update(1, PadState::default());
        shared.disconnect(1);
        shared.disconnect(1);
        assert_eq!(seen.lock().len(), 4);
        assert_eq!(seen.lock()[3], (1, None));
        assert_eq!(pads.connected().len(), 1);
        // A later attach survives an earlier one's detach.
        let first = pads.attach(None);
        let second = pads.attach(Some(Arc::new(|_, _| {})));
        pads.detach(first);
        assert!(shared.sink.lock().is_some());
        pads.detach(second);
        assert!(shared.sink.lock().is_none());
        for _ in 1..MAX_PADS {
            shared.connect(PadKind::Xbox, "x".into()).unwrap();
        }
        assert_eq!(shared.connect(PadKind::Xbox, "one too many".into()), None);
    }

    #[test]
    fn sticks_and_triggers_scale_to_the_wire() {
        assert_eq!((stick(1.0), stick(-1.0), stick(0.0)), (32767, -32767, 0));
        assert_eq!(stick(2.0), 32767);
        assert_eq!(
            (trigger(1.0), trigger(0.5), trigger(-1.0)),
            (65535, 32768, 0)
        );
    }
}
