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
        self.fec_percent = percent.clamp(MIN_FEC_PERCENT, MAX_FEC_PERCENT);
    }

    pub fn fec_percent(&self) -> u8 {
        self.fec_percent
    }

    /// Calls `emit` with every datagram of `frame`, in sending order (each
    /// block's data shards, then its parity shards). Returns the frame's
    /// number. An empty frame is sent as one zero-length data shard.
    pub fn packetize(&mut self, frame: &OutFrame<'_>, emit: impl FnMut(&[u8])) -> u32 {
        let number = self.next_frame;
        self.next_frame = self.next_frame.wrapping_add(1);
        let len = frame.data.len().clamp(1, self.shard_len);
        let total = frame.data.len().div_ceil(len).max(1);
        let blocks = total.div_ceil(crate::MAX_BLOCK_DATA);
        if blocks > u8::MAX as usize || frame.data.len() > u32::MAX as usize {
            return number;
        }
        let mut emit = emit;
        let mut offset = 0;
        let mut buf = Vec::with_capacity(32 + len);

        for block in 0..blocks {
            let data = total / blocks + usize::from(block < total % blocks);
            let parity = (data * self.fec_percent as usize)
                .div_ceil(100)
                .clamp(1, 255 - data);

            let mut refs: Vec<&[u8]> = Vec::with_capacity(data);
            let mut padded = None;
            for _ in 0..data {
                let remaining = frame.data.len().saturating_sub(offset);
                if remaining >= len {
                    refs.push(&frame.data[offset..offset + len]);
                    offset += len;
                } else {
                    let mut pad = vec![0u8; len];
                    if remaining > 0 {
                        pad[..remaining].copy_from_slice(&frame.data[offset..offset + remaining]);
                        offset += remaining;
                    }
                    padded = Some(pad);
                }
            }
            if let Some(ref pad) = padded {
                refs.push(pad);
            }

            let mut parity_flat = vec![0u8; parity * len];
            let mut out: Vec<&mut [u8]> = parity_flat.chunks_exact_mut(len).collect();

            if let Ok(coder) = crate::fec::ReedSolomon::cached(data, parity) {
                if coder.encode(&refs, &mut out).is_err() {
                    return number;
                }
            } else {
                return number;
            }

            for (index, &bytes) in refs.iter().enumerate() {
                let shard = VideoShard {
                    keyframe: frame.keyframe,
                    host_delay: frame.host_delay,
                    frame: number,
                    frame_len: frame.data.len() as u32,
                    shard: index as u16,
                    block: block as u8,
                    blocks: blocks as u8,
                    data: data as u16,
                    parity: parity as u16,
                    shard_len: len as u16,
                    bytes,
                };
                crate::wire::write_video(&mut buf, self.session, &shard);
                emit(&buf);
            }

            for (index, bytes) in parity_flat.chunks_exact(len).enumerate() {
                let shard = VideoShard {
                    keyframe: frame.keyframe,
                    host_delay: frame.host_delay,
                    frame: number,
                    frame_len: frame.data.len() as u32,
                    shard: (data + index) as u16,
                    block: block as u8,
                    blocks: blocks as u8,
                    data: data as u16,
                    parity: parity as u16,
                    shard_len: len as u16,
                    bytes,
                };
                crate::wire::write_video(&mut buf, self.session, &shard);
                emit(&buf);
            }
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
struct Block {
    shards: Vec<Option<Vec<u8>>>,
    data: usize,
    complete: bool,
}
struct Pending {
    len: usize,
    shard_len: usize,
    keyframe: bool,
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
            host_delay: shard.host_delay,
            first: now,
            complete: None,
            blocks: (0..blocks).map(|_| None).collect(),
        });
        if frame.len != len
            || frame.shard_len != sl
            || frame.keyframe != shard.keyframe
            || frame.blocks.len() != blocks
        {
            return;
        }
        let block = frame.blocks[shard.block as usize].get_or_insert_with(|| Block {
            shards: vec![None; count + shard.parity as usize],
            data: count,
            complete: false,
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
        if block.shards.iter().filter(|s| s.is_some()).count() >= count {
            let missing = block.shards[..count].iter().filter(|s| s.is_none()).count();
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
                if (self.last.is_none() || self.waiting) && !frame.keyframe {
                    self.waiting = true;
                    self.stats.frames_skipped += 1;
                    for block in frame.blocks.into_iter().flatten() {
                        for shard in block.shards.into_iter().flatten() {
                            if self.shard_pool.len() < 4096 {
                                self.shard_pool.push(shard);
                            }
                        }
                    }
                    continue;
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
                    host_delay: frame.host_delay,
                    assembly: frame.complete?.saturating_duration_since(frame.first),
                }));
            }
            let timeout = self.pending.values().any(|f| {
                f.complete.is_none() && now.saturating_duration_since(f.first) >= FRAME_TIMEOUT
            });
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

    /// When `poll` should next be called even if no shard arrives, so a
    /// timeout is noticed; `None` when nothing is pending.
    pub fn next_deadline(&self) -> Option<Instant> {
        let next = self.next?;
        let timeout = self
            .pending
            .values()
            .filter(|f| f.complete.is_none())
            .map(|f| f.first + FRAME_TIMEOUT)
            .min();
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

    /// The last frame delivered by `poll`.
    pub fn last_delivered(&self) -> Option<u32> {
        self.last
    }

    pub fn stats(&self) -> ReassemblyStats {
        self.stats
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
        if !waiting {
            self.last_sent = None;
            return false;
        }
        let interval = rtt.unwrap_or_default().max(Duration::from_millis(100));
        if self
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

/// Wrapping comparison of frame numbers: `a` comes before `b`.
pub fn before(a: u32, b: u32) -> bool {
    (a.wrapping_sub(b) as i32) < 0
}
