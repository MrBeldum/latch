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
    (10.0 + 4.0 * loss_pct)
        .round()
        .clamp(MIN_FEC_PERCENT as f32, MAX_FEC_PERCENT as f32) as u8
}

/// The parity share for a path with `loss_pct` percent loss and a base
/// round trip of `rtt`. The farther the viewer, the longer a lost frame
/// freezes the picture (a keyframe takes a round trip to ask for and
/// arrive), so a long path carries more parity before any loss is seen.
pub fn fec_percent_for(loss_pct: f32, rtt: Option<std::time::Duration>) -> u8 {
    let floor = match rtt.map_or(0, |rtt| rtt.as_millis()) {
        0..40 => DEFAULT_FEC_PERCENT,
        40..100 => 15,
        _ => 20,
    };
    fec_percent_for_loss(loss_pct).max(floor)
}

/// Fast attack, two-second decay for parity feedback. Brief radio bursts must
/// not lose their protection on the very next clean feedback window.
pub fn fec_loss_sample(previous: f32, loss: f32, elapsed: Duration) -> f32 {
    let loss = if loss.is_finite() {
        loss.clamp(0.0, 100.0)
    } else {
        0.0
    };
    loss.max(previous * (-elapsed.as_secs_f32() / 2.0).exp())
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
    /// The next frame is a recovery frame predicting from this one.
    recovery_base: Option<u32>,
    fec_percent: u8,
    fec_burst: u8,
    shard_len: usize,
    buffers: Vec<Vec<u8>>,
}

impl Packetizer {
    pub fn new(session: u32) -> Self {
        Self {
            session,
            next_frame: 0,
            recovery_base: None,
            fec_percent: DEFAULT_FEC_PERCENT,
            fec_burst: 0,
            shard_len: crate::MAX_SHARD_LEN,
            buffers: (0..crate::transport::BATCH_SIZE)
                .map(|_| Vec::with_capacity(crate::MAX_DATAGRAM))
                .collect(),
        }
    }

    /// Clamped to [`MIN_FEC_PERCENT`]..=[`MAX_FEC_PERCENT`].
    pub fn set_fec_percent(&mut self, percent: u8) {
        self.fec_percent = percent.clamp(MIN_FEC_PERCENT, MAX_FEC_PERCENT);
    }

    /// Longest observed run of missing data shards, bounded to half a block.
    pub fn set_fec_burst(&mut self, packets: u8) {
        self.fec_burst = packets.min(32);
    }

    pub fn fec_percent(&self) -> u8 {
        self.fec_percent
    }

    /// The number the next frame will get.
    pub fn next_frame(&self) -> u32 {
        self.next_frame
    }

    /// Marks the next frame as a recovery frame that predicts from frame
    /// `base` (only for a viewer that negotiated reference invalidation).
    /// A base too far back to say is not marked.
    pub fn mark_recovery(&mut self, base: u32) {
        self.recovery_base = Some(base);
    }

    /// Calls `emit` with every datagram of `frame`, in sending order: the
    /// data shards of every block first, then each block's parity. A frame
    /// that loses nothing is complete as soon as its data has arrived, and
    /// its data never waits for parity to be computed; a burst that takes
    /// out data is less likely to take out the parity sent after it.
    /// Returns the frame's number. An empty frame is sent as one
    /// zero-length data shard.
    pub fn packetize(&mut self, frame: &OutFrame<'_>, mut emit: impl FnMut(&[u8])) -> u32 {
        self.packetize_batches(frame, |batch| {
            for datagram in batch {
                emit(datagram);
            }
        })
    }

    /// Flush data before computing parity, and flush the final partial batch.
    /// Storage is reused between frames; no datagram is allocated per packet.
    pub fn packetize_batches(
        &mut self,
        frame: &OutFrame<'_>,
        mut emit: impl FnMut(&[Vec<u8>]),
    ) -> u32 {
        let number = self.next_frame;
        self.next_frame = self.next_frame.wrapping_add(1);
        let recovery = self
            .recovery_base
            .take()
            .filter(|_| !frame.keyframe)
            .and_then(|base| u16::try_from(number.wrapping_sub(base)).ok())
            .unwrap_or(0);
        let len = frame.data.len().clamp(1, self.shard_len);
        let total = frame.data.len().div_ceil(len).max(1);
        let blocks = total.div_ceil(crate::MAX_BLOCK_DATA);
        if blocks > u8::MAX as usize || frame.data.len() > u32::MAX as usize {
            return number;
        }
        let mut used = 0;
        // The last shard, zero-padded to the shard length.
        let tail = {
            let full = frame.data.len() / len * len;
            (full < frame.data.len() || frame.data.is_empty()).then(|| {
                let mut pad = vec![0u8; len];
                pad[..frame.data.len() - full].copy_from_slice(&frame.data[full..]);
                pad
            })
        };
        let shard_bytes = |index: usize| -> &[u8] {
            let at = index * len;
            if at + len <= frame.data.len() {
                &frame.data[at..at + len]
            } else {
                tail.as_deref().unwrap_or(&[])
            }
        };
        let layout: Vec<(usize, usize, usize)> = (0..blocks)
            .scan(0, |first, block| {
                let data = total / blocks + usize::from(block < total % blocks);
                let protected = frame.keyframe || recovery != 0;
                let percent = self.fec_percent.max(if protected { 30 } else { 0 });
                let burst = usize::from(self.fec_burst);
                let minimum = if protected { 4 } else { 1 };
                let parity = (data * percent as usize)
                    .div_ceil(100)
                    .max(minimum)
                    .max(if burst > 0 { (burst + 2).min(data) } else { 0 })
                    .min(255 - data);
                let start = *first;
                *first += data;
                Some((start, data, parity))
            })
            .collect();
        let header = |block: usize, data: usize, parity: usize| VideoShard {
            keyframe: frame.keyframe,
            recovery,
            host_delay: frame.host_delay,
            frame: number,
            frame_len: frame.data.len() as u32,
            shard: 0,
            block: block as u8,
            blocks: blocks as u8,
            data: data as u16,
            parity: parity as u16,
            shard_len: len as u16,
            bytes: &[],
        };

        for (block, &(first, data, parity)) in layout.iter().enumerate() {
            for index in 0..data {
                let shard = VideoShard {
                    shard: index as u16,
                    bytes: shard_bytes(first + index),
                    ..header(block, data, parity)
                };
                crate::wire::write_video(&mut self.buffers[used], self.session, &shard);
                used += 1;
                if used == self.buffers.len() {
                    emit(&self.buffers);
                    used = 0;
                }
            }
        }

        if used > 0 {
            emit(&self.buffers[..used]);
            used = 0;
        }
        let mut parity_flat = Vec::new();
        for (block, &(first, data, parity)) in layout.iter().enumerate() {
            let refs: Vec<&[u8]> = (first..first + data).map(shard_bytes).collect();
            parity_flat.clear();
            parity_flat.resize(parity * len, 0);
            let mut out: Vec<&mut [u8]> = parity_flat.chunks_exact_mut(len).collect();
            let Ok(coder) = crate::fec::ReedSolomon::cached(data, parity) else {
                return number;
            };
            if coder.encode(&refs, &mut out).is_err() {
                return number;
            }
            for (index, bytes) in parity_flat.chunks_exact(len).enumerate() {
                let shard = VideoShard {
                    shard: (data + index) as u16,
                    bytes,
                    ..header(block, data, parity)
                };
                crate::wire::write_video(&mut self.buffers[used], self.session, &shard);
                used += 1;
                if used == self.buffers.len() {
                    emit(&self.buffers);
                    used = 0;
                }
            }
        }
        if used > 0 {
            emit(&self.buffers[..used]);
        }
        number
    }
}

/// A frame put back together.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InFrame {
    pub frame: u32,
    pub data: Vec<u8>,
    pub keyframe: bool,
    /// A recovery frame: it predicts from the frame delivered before it,
    /// not from the lost ones in between.
    pub recovery: bool,
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
    /// Frames that completed but were dropped while waiting for a keyframe
    /// or a recovery frame.
    pub frames_skipped: u64,
    /// Losses ended by a recovery frame rather than a keyframe.
    pub recoveries: u64,
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
struct Block {
    shards: Vec<Option<Vec<u8>>>,
    data: usize,
    complete: bool,
    received: usize,
    received_data: usize,
}
struct Pending {
    len: usize,
    shard_len: usize,
    keyframe: bool,
    recovery: u16,
    host_delay: u16,
    first: Instant,
    complete: Option<Instant>,
    blocks: Vec<Option<Block>>,
}
pub struct Reassembler {
    session: u32,
    pending: std::collections::HashMap<u32, Pending>,
    next: Option<u32>,
    waiting: bool,
    last: Option<u32>,
    stats: ReassemblyStats,
    shard_pool: Vec<Vec<u8>>,
    loss_burst: u8,
}

impl Reassembler {
    pub fn new(session: u32) -> Self {
        Self {
            session,
            pending: Default::default(),
            next: None,
            waiting: false,
            last: None,
            stats: Default::default(),
            shard_pool: Vec::new(),
            loss_burst: 0,
        }
    }

    /// Takes one shard. Shards of frames already delivered or given up on,
    /// duplicates and shards inconsistent with their frame's earlier shards
    /// are counted and dropped.
    pub fn push(&mut self, shard: &VideoShard<'_>, now: Instant) {
        let _ = self.session;
        let len = shard.frame_len as usize;
        let sl = shard.shard_len as usize;
        if len > MAX_FRAME_LEN || sl == 0 || sl > crate::MAX_SHARD_LEN || shard.bytes.len() != sl {
            return;
        }
        let total = len.div_ceil(sl).max(1);
        let blocks = total.div_ceil(crate::MAX_BLOCK_DATA);
        if blocks != shard.blocks as usize || blocks == 0 || shard.block as usize >= blocks {
            return;
        }
        let count = total / blocks + usize::from((shard.block as usize) < total % blocks);
        if shard.data as usize != count
            || shard.parity == 0
            || count + shard.parity as usize > 255
            || shard.shard as usize >= count + shard.parity as usize
        {
            return;
        }
        if self.next.is_some_and(|n| before(shard.frame, n)) {
            self.stats.shards_late += 1;
            return;
        }
        if self.next.is_none() {
            self.next = Some(shard.frame);
        }
        let frame = self.pending.entry(shard.frame).or_insert_with(|| Pending {
            len,
            shard_len: sl,
            keyframe: shard.keyframe,
            recovery: shard.recovery,
            host_delay: shard.host_delay,
            first: now,
            complete: None,
            blocks: (0..blocks).map(|_| None).collect(),
        });
        if frame.len != len
            || frame.shard_len != sl
            || frame.keyframe != shard.keyframe
            || frame.recovery != shard.recovery
            || frame.blocks.len() != blocks
        {
            return;
        }
        let block = frame.blocks[shard.block as usize].get_or_insert_with(|| Block {
            shards: vec![None; count + shard.parity as usize],
            data: count,
            complete: false,
            received: 0,
            received_data: 0,
        });
        if block.shards.len() != count + shard.parity as usize {
            return;
        }
        let idx = shard.shard as usize;
        if block.shards[idx].is_some() || block.complete {
            self.stats.shards_late += 1;
            return;
        }
        let mut bytes = self.shard_pool.pop().unwrap_or_default();
        bytes.clear();
        bytes.extend_from_slice(shard.bytes);
        block.shards[idx] = Some(bytes);
        self.stats.shards_received += 1;
        block.received += 1;
        block.received_data += usize::from(idx < count);
        if block.received_data == count {
            block.complete = true;
        } else if block.received >= count {
            let missing = count - block.received_data;
            self.loss_burst = self.loss_burst.max(missing_run(&block.shards[..count]));
            if let Ok(coder) = crate::fec::ReedSolomon::cached(count, shard.parity as usize) {
                if coder.reconstruct(&mut block.shards).is_ok() {
                    block.complete = true;
                    self.stats.shards_recovered += missing as u64;
                }
            }
        }
        if frame
            .blocks
            .iter()
            .all(|b| b.as_ref().is_some_and(|b| b.complete))
        {
            frame.complete.get_or_insert(now);
        }
    }

    /// The next frame or loss, in frame order, once it is decided.
    pub fn poll(&mut self, now: Instant) -> Option<Reassembled> {
        loop {
            let next = self.next?;
            if self
                .pending
                .get(&next)
                .is_some_and(|f| f.complete.is_some())
            {
                let frame = self.pending.remove(&next)?;
                self.next = Some(next.wrapping_add(1));
                // A recovery frame decodes only on what the decoder holds:
                // the frame it predicts from and nothing after it.
                let base =
                    (frame.recovery != 0).then(|| next.wrapping_sub(u32::from(frame.recovery)));
                let recovers = base.is_some() && base == self.last;
                if base.is_some() && !recovers && !self.waiting {
                    // Not after what this viewer holds (it never asked):
                    // as good as lost.
                    self.waiting = true;
                    self.stats.frames_lost += 1;
                    self.release(frame);
                    return Some(Reassembled::Lost { frame: next });
                }
                if (self.last.is_none() || self.waiting) && !frame.keyframe && !recovers {
                    self.waiting = true;
                    self.stats.frames_skipped += 1;
                    self.release(frame);
                    continue;
                }
                if self.waiting && recovers {
                    self.stats.recoveries += 1;
                }
                self.waiting = false;
                self.last = Some(next);
                self.stats.frames_delivered += 1;
                let mut data = Vec::with_capacity(frame.len);
                for block in frame.blocks.into_iter().flatten() {
                    for (index, shard) in block.shards.into_iter().enumerate() {
                        if let Some(shard) = shard {
                            if index < block.data {
                                data.extend_from_slice(&shard);
                            }
                            if self.shard_pool.len() < 4096 {
                                self.shard_pool.push(shard);
                            }
                        }
                    }
                }
                data.truncate(frame.len);
                return Some(Reassembled::Frame(InFrame {
                    frame: next,
                    data,
                    keyframe: frame.keyframe,
                    recovery: recovers,
                    host_delay: frame.host_delay,
                    assembly: frame.complete?.saturating_duration_since(frame.first),
                }));
            }
            // A frame still assembling gets its own full timeout; one that
            // never arrived is lost once any later frame times out.
            let timeout = match self.pending.get(&next) {
                Some(f) => now.saturating_duration_since(f.first) >= FRAME_TIMEOUT,
                None => self.pending.values().any(|f| {
                    f.complete.is_none() && now.saturating_duration_since(f.first) >= FRAME_TIMEOUT
                }),
            };
            let later = self.pending.iter().any(|(&n, f)| {
                before(next, n)
                    && f.complete
                        .is_some_and(|t| now.saturating_duration_since(t) >= REORDER_GRACE)
            });
            if !timeout && !later && self.pending.len() <= MAX_PENDING_FRAMES {
                return None;
            }
            if let Some(frame) = self.pending.remove(&next) {
                let total = frame.len.div_ceil(frame.shard_len).max(1);
                let count = frame.blocks.len();
                for (i, block) in frame.blocks.into_iter().enumerate() {
                    self.stats.shards_missing += match block {
                        Some(b) => {
                            self.loss_burst = self.loss_burst.max(missing_run(&b.shards[..b.data]));
                            let missing = b.shards[..b.data].iter().filter(|s| s.is_none()).count();
                            for shard in b.shards.into_iter().flatten() {
                                if self.shard_pool.len() < 4096 {
                                    self.shard_pool.push(shard);
                                }
                            }
                            missing
                        }
                        None => total / count + usize::from(i < total % count),
                    } as u64;
                }
            }
            self.next = Some(next.wrapping_add(1));
            self.waiting = true;
            self.stats.frames_lost += 1;
            return Some(Reassembled::Lost { frame: next });
        }
    }

    fn release(&mut self, frame: Pending) {
        for block in frame.blocks.into_iter().flatten() {
            for shard in block.shards.into_iter().flatten() {
                if self.shard_pool.len() < 4096 {
                    self.shard_pool.push(shard);
                }
            }
        }
    }

    /// The oldest frame not yet delivered or given up on.
    pub fn next_expected(&self) -> Option<u32> {
        self.next
    }

    /// The newest frame any shard has arrived for.
    pub fn newest_seen(&self) -> Option<u32> {
        self.pending
            .keys()
            .copied()
            .reduce(|a, b| if before(a, b) { b } else { a })
    }

    /// When `poll` should next be called even if no shard arrives, so a
    /// timeout is noticed; `None` when nothing is pending.
    pub fn next_deadline(&self) -> Option<Instant> {
        let next = self.next?;
        let timeout = match self.pending.get(&next) {
            Some(f) => f.complete.is_none().then_some(f.first + FRAME_TIMEOUT),
            None => self
                .pending
                .values()
                .filter(|f| f.complete.is_none())
                .map(|f| f.first + FRAME_TIMEOUT)
                .min(),
        };
        let later = self
            .pending
            .iter()
            .filter(|(n, _)| before(next, **n))
            .filter_map(|(_, f)| f.complete.map(|t| t + REORDER_GRACE))
            .min();
        timeout.into_iter().chain(later).min()
    }

    /// A frame was lost and no keyframe has completed since.
    pub fn waiting_for_keyframe(&self) -> bool {
        self.waiting
    }

    /// Recover even when no shard of the initial keyframe reached us. This
    /// startup grace exceeds the assembly timeout, so a healthy IDR can finish.
    pub fn needs_keyframe(&self, accepted: Instant, now: Instant) -> bool {
        self.waiting
            || (self.last.is_none()
                && now.saturating_duration_since(accepted) >= Duration::from_millis(500))
    }

    /// The last frame delivered by `poll`.
    pub fn last_delivered(&self) -> Option<u32> {
        self.last
    }

    /// Longest data-shard erasure run since the last feedback window.
    /// Reordered data still absent when parity completes can overestimate it.
    pub fn take_loss_burst(&mut self) -> u8 {
        std::mem::take(&mut self.loss_burst)
    }

    pub fn stats(&self) -> ReassemblyStats {
        self.stats
    }
}

fn missing_run(shards: &[Option<Vec<u8>>]) -> u8 {
    let mut run = 0u8;
    let mut longest = 0;
    for shard in shards {
        run = if shard.is_none() {
            run.saturating_add(1)
        } else {
            0
        };
        longest = longest.max(run);
    }
    longest.min(32)
}

/// Repeats of an unanswered `KeyframeRequest` at most this often (or the
/// round trip, if longer). A request lost on the way costs this much of
/// frozen picture; the host tells repeats from duplicates
/// ([`KeyframeAnswers`]), so a repeat costs it nothing.
pub const KEYFRAME_RETRY: Duration = Duration::from_millis(50);

/// When to send `KeyframeRequest`: at once after a loss, then every
/// `max(KEYFRAME_RETRY, rtt)` until a keyframe newer than the request
/// arrives. Any frame lost meanwhile may be the keyframe that answered: ask
/// again at once rather than at the next interval. The host tells such a
/// repeat from a duplicate ([`KeyframeAnswers`]).
pub struct KeyframeRequests {
    last_sent: Option<Instant>,
    lost: bool,
}

impl KeyframeRequests {
    pub fn new() -> Self {
        Self {
            last_sent: None,
            lost: false,
        }
    }

    /// A frame was declared lost (the reassembler's `Lost`).
    pub fn lost(&mut self) {
        self.lost = true;
    }

    /// `waiting`: the reassembler waits for a keyframe. Returns whether to
    /// send a request now.
    pub fn due(&mut self, waiting: bool, now: Instant, rtt: Option<Duration>) -> bool {
        let lost = std::mem::take(&mut self.lost);
        if !waiting {
            self.last_sent = None;
            return false;
        }
        let interval = rtt.unwrap_or_default().max(KEYFRAME_RETRY);
        if lost
            || self
                .last_sent
                .is_none_or(|t| now.saturating_duration_since(t) >= interval)
        {
            self.last_sent = Some(now);
            true
        } else {
            false
        }
    }
}

impl Default for KeyframeRequests {
    fn default() -> Self {
        Self::new()
    }
}

/// The viewer's side of reference invalidation (`docs/PROTOCOL.md`,
/// "RecoveryRequest"): after a loss, ask the host for a recovery frame
/// that predicts from the last frame delivered; fall back to keyframe
/// requests when the host does not answer in time, refuses, or the
/// recovery frames keep getting lost.
pub struct Recovery {
    next_id: u32,
    state: RecoveryState,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RecoveryState {
    Idle,
    Asking {
        request: crate::wire::RecoveryRequest,
        first_sent: Instant,
        last_sent: Instant,
        /// When the host said it would try.
        pending: Option<Instant>,
        /// The recovery frame the host sent for this request.
        sent: Option<u32>,
        attempts: u8,
    },
    /// Keyframe requests until the loss is over.
    Keyframe,
}

/// Recovery frames lost in a row before falling back to a keyframe.
const RECOVERY_ATTEMPTS: u8 = 3;
/// How long after the host's "pending" the recovery frame may take: the
/// host makes it with its next frame, and on a still screen that can wait
/// up to its 100 ms idle repeat.
const RECOVERY_PENDING: Duration = Duration::from_millis(250);

impl Recovery {
    pub fn new() -> Self {
        Self {
            next_id: 1,
            state: RecoveryState::Idle,
        }
    }

    /// The host has not answered within this of a request: max(100 ms,
    /// 1.5 round trips), since the answer takes a round trip and jitter.
    pub fn answer_deadline(rtt: Option<Duration>) -> Duration {
        rtt.map_or(Duration::ZERO, |r| r * 3 / 2)
            .max(Duration::from_millis(100))
    }

    /// True while this loss is left to keyframe requests.
    pub fn wants_keyframe(&self) -> bool {
        self.state == RecoveryState::Keyframe
    }

    /// The host's answer.
    pub fn ack(&mut self, ack: crate::wire::RecoveryAck, now: Instant) {
        use crate::wire::RecoveryStatus;
        let RecoveryState::Asking {
            request,
            pending,
            sent,
            ..
        } = &mut self.state
        else {
            return;
        };
        if ack.id != request.id {
            return;
        }
        match ack.status {
            RecoveryStatus::Pending => {
                pending.get_or_insert(now);
            }
            RecoveryStatus::Sent => {
                pending.get_or_insert(now);
                *sent = Some(ack.frame);
            }
            RecoveryStatus::Refused => self.state = RecoveryState::Keyframe,
        }
    }

    /// Call on every pass of the receive loop. `waiting`: the reassembler
    /// lost a frame and has delivered nothing since; `last`, `next` and
    /// `newest` are its last delivered frame, the frame it waits for and
    /// the newest it has seen. Returns a request to send now.
    pub fn due(
        &mut self,
        waiting: bool,
        last: Option<u32>,
        next: Option<u32>,
        newest: Option<u32>,
        now: Instant,
        rtt: Option<Duration>,
    ) -> Option<crate::wire::RecoveryRequest> {
        if !waiting {
            self.state = RecoveryState::Idle;
            return None;
        }
        // Nothing decoded yet: nothing to predict from.
        let Some(last) = last else {
            self.state = RecoveryState::Keyframe;
            return None;
        };
        let deadline = Self::answer_deadline(rtt);
        let new_request = |id: u32| {
            let first = last.wrapping_add(1);
            let newest = newest
                .into_iter()
                .chain(next.map(|n| n.wrapping_sub(1)))
                .fold(first, |a, b| if before(a, b) { b } else { a });
            crate::wire::RecoveryRequest {
                last_delivered: last,
                first_invalid: first,
                last_invalid: newest,
                id,
            }
        };
        match &mut self.state {
            RecoveryState::Keyframe => None,
            RecoveryState::Idle => {
                let request = new_request(self.next_id);
                self.next_id = self.next_id.wrapping_add(1).max(1);
                self.state = RecoveryState::Asking {
                    request,
                    first_sent: now,
                    last_sent: now,
                    pending: None,
                    sent: None,
                    attempts: 1,
                };
                Some(request)
            }
            RecoveryState::Asking {
                request,
                first_sent,
                last_sent,
                pending,
                sent,
                attempts,
            } => {
                // The recovery frame came and went without ending the loss:
                // it was lost too. Ask again from the same frame.
                let missed = sent.is_some_and(|r| next.is_some_and(|n| before(r, n)))
                    || request.last_delivered != last;
                let late = match pending {
                    None => now.saturating_duration_since(*first_sent) >= deadline,
                    Some(at) => now.saturating_duration_since(*at) >= RECOVERY_PENDING + deadline,
                };
                if missed {
                    if *attempts >= RECOVERY_ATTEMPTS {
                        self.state = RecoveryState::Keyframe;
                        return None;
                    }
                    let attempts = *attempts + 1;
                    let request = new_request(self.next_id);
                    self.next_id = self.next_id.wrapping_add(1).max(1);
                    self.state = RecoveryState::Asking {
                        request,
                        first_sent: now,
                        last_sent: now,
                        pending: None,
                        sent: None,
                        attempts,
                    };
                    return Some(request);
                }
                if late {
                    self.state = RecoveryState::Keyframe;
                    return None;
                }
                // One repeat halfway to the deadline, in case the request
                // or its answer was lost.
                if pending.is_none()
                    && *last_sent == *first_sent
                    && now.saturating_duration_since(*first_sent) >= deadline / 2
                {
                    *last_sent = now;
                    return Some(*request);
                }
                None
            }
        }
    }
}

impl Default for Recovery {
    fn default() -> Self {
        Self::new()
    }
}

/// Repeats of a request naming the same last-delivered frame are taken for
/// duplicates of one loss this long, unless [`KeyframeAnswers`] can tell
/// that the keyframe answering it was lost.
pub const DUPLICATE_REQUEST_HOLD: Duration = Duration::from_millis(250);

/// The host's side of `KeyframeRequest`: which requests ask for a keyframe
/// not already on its way. A request naming a newer last-delivered frame
/// reports a new loss. One naming the same frame again is a duplicate while
/// the keyframe answering it may still be in flight; once that keyframe has
/// been out for a round trip, the viewer asking again means it was lost too,
/// and the picture stays frozen until another one arrives.
#[derive(Debug, Default)]
pub struct KeyframeAnswers {
    accepted: Option<(u32, Instant)>,
    sent: Option<Instant>,
}

impl KeyframeAnswers {
    pub fn new() -> Self {
        Self::default()
    }

    /// A keyframe went out at `now`, asked for or not.
    pub fn sent(&mut self, now: Instant) {
        self.sent = Some(now);
    }

    /// A request naming `last` arrived at `now`; `rtt` is a low estimate
    /// of the round trip (the smoothed RTT less its variation: a repeat
    /// sent as soon as the keyframe was found lost takes about one round
    /// trip after it went out, give or take jitter). Returns whether to
    /// make the next frame a keyframe.
    pub fn request(&mut self, last: u32, now: Instant, rtt: Option<Duration>) -> bool {
        let wanted = self.accepted.is_none_or(|(n, at)| {
            before(n, last)
                || (n == last
                    && (now.saturating_duration_since(at) >= DUPLICATE_REQUEST_HOLD
                        || self.sent.is_some_and(|sent| {
                            sent >= at
                                && now.saturating_duration_since(sent) >= rtt.unwrap_or_default()
                        })))
        });
        if wanted {
            self.accepted = Some((last, now));
        }
        wanted
    }
}

/// Wrapping comparison of frame numbers: `a` comes before `b`.
pub fn before(a: u32, b: u32) -> bool {
    (a.wrapping_sub(b) as i32) < 0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keyframes_and_recovery_survive_four_packet_bursts_even_when_tiny() {
        for size in [1, 7 * crate::MAX_SHARD_LEN, 150 * crate::MAX_SHARD_LEN] {
            for recovery in [false, true] {
                let now = Instant::now();
                let mut tx = Packetizer::new(1);
                let mut rx = Reassembler::new(1);
                run(&mut tx, &mut rx, &[(0, true, None)], &[], now);
                if recovery {
                    tx.mark_recovery(0);
                }
                let data = vec![0x5a; size];
                let mut erased = 0;
                tx.packetize(
                    &OutFrame {
                        data: &data,
                        keyframe: !recovery,
                        host_delay: 0,
                    },
                    |dg| {
                        assert!(dg.len() <= crate::MAX_DATAGRAM);
                        let crate::wire::Datagram::Video { shard, .. } =
                            crate::wire::parse(dg).unwrap()
                        else {
                            panic!()
                        };
                        assert!(shard.parity >= 4);
                        assert!(usize::from(shard.data + shard.parity) <= 255);
                        if erased < 4 {
                            erased += 1;
                        } else {
                            rx.push(&shard, now);
                        }
                    },
                );
                let Some(Reassembled::Frame(frame)) = rx.poll(now) else {
                    panic!("lost protected frame size={size}")
                };
                assert_eq!(frame.data, data);
                assert!(rx.take_loss_burst() > 0);
                assert_eq!(rx.take_loss_burst(), 0);
            }
        }
    }

    /// Sends `count` small frames through a packetizer into `reassembler`,
    /// skipping the frames `lost` names, and returns what came out.
    fn run(
        packetizer: &mut Packetizer,
        reassembler: &mut Reassembler,
        frames: &[(u8, bool, Option<u32>)],
        lost: &[u32],
        start: Instant,
    ) -> Vec<Reassembled> {
        let mut out = Vec::new();
        for (i, &(byte, keyframe, base)) in frames.iter().enumerate() {
            // 60 frames a second.
            let now = start + Duration::from_millis(17 * i as u64);
            if let Some(base) = base {
                packetizer.mark_recovery(base);
            }
            let number = packetizer.next_frame();
            let data = [byte; 10];
            packetizer.packetize(
                &OutFrame {
                    data: &data,
                    keyframe,
                    host_delay: 0,
                },
                |datagram| {
                    let Ok(crate::wire::Datagram::Video { shard, .. }) =
                        crate::wire::parse(datagram)
                    else {
                        panic!("not a video datagram");
                    };
                    assert_eq!(shard.recovery != 0, base.is_some() && !keyframe);
                    if !lost.contains(&number) {
                        reassembler.push(&shard, now);
                    }
                },
            );
            while let Some(r) = reassembler.poll(now) {
                out.push(r);
            }
        }
        out
    }

    #[test]
    fn a_recovery_frame_ends_a_loss_only_after_the_frame_it_predicts_from() {
        let mut packetizer = Packetizer::new(1);
        let mut reassembler = Reassembler::new(1);
        let now = Instant::now();
        // 0 key, 1 delivered, 2 lost, 3 skipped, 4 recovers from 1.
        let out = run(
            &mut packetizer,
            &mut reassembler,
            &[
                (0, true, None),
                (1, false, None),
                (2, false, None),
                (3, false, None),
                (4, false, Some(1)),
                (5, false, None),
            ],
            &[2],
            now,
        );
        let delivered: Vec<_> = out
            .iter()
            .filter_map(|r| match r {
                Reassembled::Frame(f) => Some((f.frame, f.recovery)),
                Reassembled::Lost { .. } => None,
            })
            .collect();
        assert_eq!(delivered, [(0, false), (1, false), (4, true), (5, false)]);
        assert_eq!(reassembler.stats().recoveries, 1);
        assert_eq!(reassembler.stats().frames_skipped, 1);
        assert!(!reassembler.waiting_for_keyframe());

        // One that predicts from a frame this viewer did not stop at (3,
        // which it skipped) does not end the next loss.
        let out = run(
            &mut packetizer,
            &mut reassembler,
            &[(6, false, None), (7, false, None), (8, false, Some(7))],
            &[6],
            now,
        );
        assert!(out
            .iter()
            .all(|r| !matches!(r, Reassembled::Frame(f) if f.frame > 5)));
        assert!(reassembler.waiting_for_keyframe());

        // And one that arrives while nothing was lost, from an older frame,
        // is itself a loss: the decoder no longer holds that state.
        let mut packetizer = Packetizer::new(1);
        let mut reassembler = Reassembler::new(1);
        let out = run(
            &mut packetizer,
            &mut reassembler,
            &[
                (0, true, None),
                (1, false, None),
                (2, false, None),
                (3, false, Some(1)),
            ],
            &[],
            now,
        );
        assert!(matches!(out.last(), Some(Reassembled::Lost { frame: 3 })));
        assert!(reassembler.waiting_for_keyframe());
    }

    #[test]
    fn recovery_requests_fall_back_to_keyframes() {
        use crate::wire::{RecoveryAck, RecoveryStatus};
        let t0 = Instant::now();
        let ms = |n: u64| t0 + Duration::from_millis(n);
        let rtt = Some(Duration::from_millis(40));
        let mut r = Recovery::new();
        assert_eq!(r.due(false, Some(5), Some(6), None, t0, rtt), None);
        // A loss: one request at once, one repeat halfway, then keyframes.
        let first = r.due(true, Some(5), Some(7), Some(9), t0, rtt).unwrap();
        assert_eq!(
            (
                first.last_delivered,
                first.first_invalid,
                first.last_invalid
            ),
            (5, 6, 9)
        );
        assert_eq!(r.due(true, Some(5), Some(7), Some(9), ms(10), rtt), None);
        assert_eq!(
            r.due(true, Some(5), Some(7), Some(9), ms(50), rtt),
            Some(first)
        );
        assert_eq!(r.due(true, Some(5), Some(7), Some(9), ms(70), rtt), None);
        assert!(!r.wants_keyframe());
        assert_eq!(r.due(true, Some(5), Some(7), Some(9), ms(100), rtt), None);
        assert!(r.wants_keyframe(), "no answer within 100 ms");
        assert_eq!(r.due(false, Some(20), Some(21), None, ms(200), rtt), None);
        assert!(!r.wants_keyframe(), "a keyframe ended the loss");

        // Answered: wait for the recovery frame; it is lost, so ask again
        // with a new number, up to three times.
        let t1 = ms(1000);
        let at = |n: u64| t1 + Duration::from_millis(n);
        let a = r.due(true, Some(30), Some(31), None, t1, rtt).unwrap();
        assert_ne!(a.id, first.id);
        let sent = |id, frame| RecoveryAck {
            id,
            status: RecoveryStatus::Sent,
            frame,
        };
        r.ack(sent(a.id, 34), at(30));
        assert_eq!(r.due(true, Some(30), Some(33), None, at(200), rtt), None);
        assert!(!r.wants_keyframe(), "pending past the answer deadline");
        let b = r.due(true, Some(30), Some(35), None, at(210), rtt).unwrap();
        assert_eq!(b.last_delivered, 30);
        assert_ne!(b.id, a.id);
        r.ack(sent(b.id, 36), at(240));
        let c = r.due(true, Some(30), Some(37), None, at(260), rtt).unwrap();
        r.ack(sent(c.id, 38), at(290));
        assert_eq!(r.due(true, Some(30), Some(39), None, at(300), rtt), None);
        assert!(r.wants_keyframe(), "three recovery frames lost");

        // Refused: keyframes at once. A stale answer changes nothing.
        r.due(false, Some(50), Some(51), None, at(400), rtt);
        let d = r.due(true, Some(50), Some(52), None, at(400), rtt).unwrap();
        r.ack(
            RecoveryAck {
                id: d.id.wrapping_sub(1),
                status: RecoveryStatus::Refused,
                frame: 0,
            },
            at(410),
        );
        assert!(!r.wants_keyframe());
        r.ack(
            RecoveryAck {
                id: d.id,
                status: RecoveryStatus::Refused,
                frame: 0,
            },
            at(420),
        );
        assert!(r.wants_keyframe());

        // Nothing delivered yet: nothing to recover from.
        let mut r = Recovery::new();
        assert_eq!(r.due(true, None, Some(3), None, t0, rtt), None);
        assert!(r.wants_keyframe());
    }

    #[test]
    fn recovery_datagrams_round_trip_and_old_flags_still_parse() {
        use crate::wire::{self, Datagram, RecoveryAck, RecoveryRequest, RecoveryStatus};
        let mut out = Vec::new();
        let request = RecoveryRequest {
            last_delivered: u32::MAX,
            first_invalid: 0,
            last_invalid: 4,
            id: 9,
        };
        wire::write_recovery_request(&mut out, 3, &request);
        assert_eq!(out.len(), 24);
        assert_eq!(out[0], 14);
        assert!(matches!(
            wire::parse(&out),
            Ok(Datagram::RecoveryRequest { session: 3, request: r }) if r == request
        ));
        for status in [
            RecoveryStatus::Pending,
            RecoveryStatus::Sent,
            RecoveryStatus::Refused,
        ] {
            let ack = RecoveryAck {
                id: 9,
                status,
                frame: 77,
            };
            wire::write_recovery_ack(&mut out, 3, &ack);
            assert_eq!(out[0], 15);
            assert!(matches!(
                wire::parse(&out),
                Ok(Datagram::RecoveryAck { ack: a, .. }) if a == ack
            ));
        }
        out[1] = 3;
        assert!(wire::parse(&out).is_err(), "unknown status");
        out.truncate(15);
        assert!(wire::parse(&out).is_err());

        // Video: a flag without a distance, or a distance without the
        // flag, or a recovery keyframe, is malformed.
        let shard = wire::VideoShard {
            keyframe: false,
            recovery: 3,
            host_delay: 0,
            frame: 10,
            frame_len: 4,
            shard: 0,
            block: 0,
            blocks: 1,
            data: 1,
            parity: 1,
            shard_len: 4,
            bytes: &[1, 2, 3, 4],
        };
        wire::write_video(&mut out, 1, &shard);
        assert_eq!(out[1], 2);
        assert!(matches!(wire::parse(&out), Ok(Datagram::Video { shard: s, .. }) if s == shard));
        for (flags, distance) in [(2u8, 0u16), (0, 3), (3, 3), (5, 0)] {
            let mut bad = out.clone();
            bad[1] = flags;
            bad[26..28].copy_from_slice(&distance.to_le_bytes());
            assert!(wire::parse(&bad).is_err(), "{flags} {distance}");
        }
        // What every host before this sends.
        let mut old = out.clone();
        old[1] = 1;
        old[26..28].fill(0);
        assert!(
            matches!(wire::parse(&old), Ok(Datagram::Video { shard: s, .. }) if s.keyframe && s.recovery == 0)
        );
    }

    #[test]
    fn every_data_shard_goes_out_before_any_parity() {
        let data: Vec<u8> = (0..300_000u32).map(|i| (i * 7) as u8).collect();
        let mut packetizer = Packetizer::new(9);
        let mut kinds = Vec::new();
        let mut reassembler = Reassembler::new(9);
        let now = Instant::now();
        packetizer.packetize(
            &OutFrame {
                data: &data,
                keyframe: true,
                host_delay: 0,
            },
            |datagram| {
                let Ok(crate::wire::Datagram::Video { shard, .. }) = crate::wire::parse(datagram)
                else {
                    panic!("not a video datagram");
                };
                kinds.push(shard.shard >= shard.data);
                // Only the data reaches the viewer: it is enough.
                if shard.shard < shard.data {
                    reassembler.push(&shard, now);
                }
            },
        );
        assert!(kinds.iter().filter(|&&parity| !parity).count() > 64);
        let first_parity = kinds.iter().position(|&parity| parity).unwrap();
        assert!(kinds[first_parity..].iter().all(|&parity| parity));
        let Some(Reassembled::Frame(frame)) = reassembler.poll(now) else {
            panic!("the data alone did not make the frame");
        };
        assert_eq!(frame.data, data);
    }
}
