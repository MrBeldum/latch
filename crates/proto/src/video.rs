//! Video frames to shards on the host ([`Packetizer`]) and back to frames
//! on the viewer ([`Reassembler`]). See `docs/PROTOCOL.md`, "Video (2)".

use crate::wire::VideoShard;
use std::time::{Duration, Instant};

/// A later frame must have been complete this long before an earlier,
/// still incomplete frame counts as lost: shards that crossed on the way
/// get a moment to arrive.
pub const REORDER_GRACE: Duration = Duration::from_millis(3);
/// A frame whose first shard arrived this long ago and that is still
/// incomplete is lost.
pub const FRAME_TIMEOUT: Duration = Duration::from_millis(100);
/// Largest frame the reassembler accepts.
pub const MAX_FRAME_LEN: usize = 32 << 20;
/// Incomplete frames kept at once; the oldest is lost beyond this.
pub const MAX_PENDING_FRAMES: usize = 32;

/// Default and bounds for the parity share, in percent of data shards.
pub const DEFAULT_FEC_PERCENT: u8 = 10;
pub const MIN_FEC_PERCENT: u8 = 5;
pub const MAX_FEC_PERCENT: u8 = 100;

/// The parity share for a viewer that reports `loss_pct` percent of video
/// datagrams lost: enough to rebuild that loss with room for bursts.
pub fn fec_percent_for_loss(loss_pct: f32) -> u8 {
    let _ = loss_pct;
    todo!()
}

/// One encoded frame for the packetizer.
#[derive(Debug, Clone, Copy)]
pub struct OutFrame<'a> {
    /// One Annex B access unit.
    pub data: &'a [u8],
    pub keyframe: bool,
    /// Capture-to-send time on the host, 0.1 ms units, saturating.
    pub host_delay: u16,
}

/// Cuts frames into data and parity shards.
pub struct Packetizer {
    session: u32,
    next_frame: u32,
    fec_percent: u8,
    shard_len: usize,
}

impl Packetizer {
    pub fn new(session: u32) -> Self {
        Self {
            session,
            next_frame: 0,
            fec_percent: DEFAULT_FEC_PERCENT,
            shard_len: crate::MAX_SHARD_LEN,
        }
    }

    /// Clamped to [`MIN_FEC_PERCENT`]..=[`MAX_FEC_PERCENT`].
    pub fn set_fec_percent(&mut self, percent: u8) {
        let _ = percent;
        todo!()
    }

    pub fn fec_percent(&self) -> u8 {
        self.fec_percent
    }

    /// Calls `emit` with every datagram of `frame`, in sending order (each
    /// block's data shards, then its parity shards). Returns the frame's
    /// number. An empty frame is sent as one zero-length data shard.
    pub fn packetize(&mut self, frame: &OutFrame<'_>, emit: impl FnMut(&[u8])) -> u32 {
        let _ = (frame, emit, self.session, self.next_frame, self.shard_len);
        todo!()
    }
}

/// A frame put back together.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InFrame {
    pub frame: u32,
    pub data: Vec<u8>,
    pub keyframe: bool,
    pub host_delay: u16,
    /// From its first shard's arrival to its completion.
    pub assembly: Duration,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Reassembled {
    Frame(InFrame),
    /// This frame will not arrive. Until a keyframe completes, no frame is
    /// delivered (they depend on the lost one); the viewer should ask for
    /// one.
    Lost {
        frame: u32,
    },
}

/// Monotonic counters for the viewer's statistics.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ReassemblyStats {
    pub frames_delivered: u64,
    pub frames_lost: u64,
    /// Frames that completed but were dropped while waiting for a keyframe.
    pub frames_skipped: u64,
    pub shards_received: u64,
    /// Data shards rebuilt from parity.
    pub shards_recovered: u64,
    /// Duplicates and shards of frames already delivered, lost or skipped.
    pub shards_late: u64,
    /// Data shards (not parity) that frames needed and never got, counting
    /// a lost frame's missing data shards.
    pub shards_missing: u64,
}

/// Collects shards into frames and hands them out in frame order.
pub struct Reassembler {
    session: u32,
}

impl Reassembler {
    pub fn new(session: u32) -> Self {
        Self { session }
    }

    /// Takes one shard. Shards of frames already delivered or given up on,
    /// duplicates and shards inconsistent with their frame's earlier shards
    /// are counted and dropped.
    pub fn push(&mut self, shard: &VideoShard<'_>, now: Instant) {
        let _ = (shard, now, self.session);
        todo!()
    }

    /// The next frame or loss, in frame order, once it is decided.
    pub fn poll(&mut self, now: Instant) -> Option<Reassembled> {
        let _ = now;
        todo!()
    }

    /// When `poll` should next be called even if no shard arrives, so a
    /// timeout is noticed; `None` when nothing is pending.
    pub fn next_deadline(&self) -> Option<Instant> {
        todo!()
    }

    /// A frame was lost and no keyframe has completed since.
    pub fn waiting_for_keyframe(&self) -> bool {
        todo!()
    }

    /// The last frame delivered by `poll`.
    pub fn last_delivered(&self) -> Option<u32> {
        todo!()
    }

    pub fn stats(&self) -> ReassemblyStats {
        todo!()
    }
}

/// When to send `KeyframeRequest`: at once after a loss, then every
/// `max(100 ms, rtt)` until a keyframe newer than the request arrives.
pub struct KeyframeRequests {
    last_sent: Option<Instant>,
}

impl KeyframeRequests {
    pub fn new() -> Self {
        Self { last_sent: None }
    }

    /// `waiting`: the reassembler waits for a keyframe. Returns whether to
    /// send a request now.
    pub fn due(&mut self, waiting: bool, now: Instant, rtt: Option<Duration>) -> bool {
        let _ = (waiting, now, rtt, self.last_sent);
        todo!()
    }
}

impl Default for KeyframeRequests {
    fn default() -> Self {
        Self::new()
    }
}

/// Wrapping comparison of frame numbers: `a` comes before `b`.
pub fn before(a: u32, b: u32) -> bool {
    (a.wrapping_sub(b) as i32) < 0
}
