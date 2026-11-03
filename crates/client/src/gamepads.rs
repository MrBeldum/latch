//! This machine's gamepads, for every stream: one watcher for the whole
//! app (`latch_stream::gamepad`), and a [`Link`] per stream that sends the
//! pads to its host and the host's rumble back to the pads.

use latch_stream::gamepad::{PadInfo, Pads};
use latch_stream::{Input, Session};
use std::sync::{Arc, OnceLock};

/// The app's gamepads, watched from first use on.
pub fn pads() -> &'static Arc<Pads> {
    static PADS: OnceLock<Arc<Pads>> = OnceLock::new();
    PADS.get_or_init(|| Arc::new(Pads::start()))
}

/// A stream's hold on the pads: while it lives, their states go to the
/// stream's host and the host's rumble comes back. Dropped, it lets go and
/// stops every rumble, so a pad never buzzes on after a stream.
pub struct Link {
    pads: Arc<Pads>,
    attached: u64,
}

impl Link {
    pub fn new(session: &Session, input: Input) -> Self {
        let pads = pads().clone();
        let to_pads = pads.clone();
        session.on_pad_feedback(move |slot, feedback| to_pads.rumble(slot, feedback));
        let attached = pads.attach(Some(Arc::new(move |slot, pad| match pad {
            Some((kind, state)) => input.pad(slot, kind, state),
            None => input.pad_gone(slot),
        })));
        Self { pads, attached }
    }

    /// The pads connected now.
    pub fn connected(&self) -> Vec<PadInfo> {
        self.pads.connected()
    }
}

impl Drop for Link {
    fn drop(&mut self) {
        // A stream that replaced this one keeps the pads.
        self.pads.detach(self.attached);
        self.pads.quiet();
    }
}
