//! Probe trains: while video is idle, the host sends a few padding
//! datagrams back to back. The bottleneck spreads them out to its own rate,
//! so their spread at the viewer measures what the path carries without
//! sending video at that rate (`docs/PROTOCOL.md`, "Probe").
use std::time::{Duration, Instant};

/// Fewer received datagrams than this measure nothing useful.
pub const MIN_TRAIN: usize = 4;
/// IP, UDP and tunnel bytes per datagram, as the host's pacer counts them.
const OVERHEAD: usize = 80;
/// A train with nothing new for this long has ended (its tail was lost).
const QUIET: Duration = Duration::from_millis(50);

struct Train {
    id: u16,
    count: u8,
    len: usize,
    arrivals: Vec<Instant>,
    seen: u16,
    ordered: bool,
}

/// The viewer's side: the fastest trustworthy train between two reports.
#[derive(Default)]
pub struct TrainMeter {
    current: Option<Train>,
    last: Option<u16>,
    best: u32,
}

impl TrainMeter {
    /// One `Probe` datagram of `len` bytes, as it arrived.
    pub fn push(&mut self, train: u16, index: u8, count: u8, len: usize, now: Instant) {
        if !(MIN_TRAIN as u8..=16).contains(&count) || index >= count {
            return;
        }
        if self.current.as_ref().is_none_or(|t| t.id != train) {
            // Delayed tails and duplicates must not start a second sample.
            if self
                .last
                .is_some_and(|last| train.wrapping_sub(last) >= 0x8000 || train == last)
            {
                return;
            }
            self.finish();
            self.last = Some(train);
            self.current = Some(Train {
                id: train,
                count,
                len,
                arrivals: Vec::new(),
                seen: 0,
                ordered: true,
            });
        }
        if let Some(t) = self.current.as_mut() {
            if t.seen & (1 << index) != 0 {
                return;
            }
            t.ordered &= usize::from(index) == t.arrivals.len() && count == t.count && len == t.len;
            t.seen |= 1 << index;
            t.arrivals.push(now);
        }
        if index + 1 == count {
            self.finish();
        }
    }

    fn finish(&mut self) {
        let Some(t) = self.current.take() else { return };
        let received = t.arrivals.len();
        // A contiguous prefix can survive tail loss. Missing interior
        // positions and reordering cannot establish a slower path.
        if !t.ordered || received < MIN_TRAIN {
            return;
        }
        // Use the median eight-position packet pair, rather than total
        // span: a late arrival must not lower the estimate. Wider pairs
        // tolerate receive batching / coarse clocks. Their observed spread
        // measures timing noise; serialization must clearly exceed it.
        let span = t.arrivals[received - 1].saturating_duration_since(t.arrivals[0]);
        let largest = t
            .arrivals
            .windows(2)
            .map(|w| w[1].saturating_duration_since(w[0]))
            .max()
            .unwrap_or_default();
        if largest * 2 > span {
            return;
        }
        let width = 8.min(received - 2);
        let mut gaps: Vec<_> = t
            .arrivals
            .windows(width + 1)
            .map(|w| w[width].saturating_duration_since(w[0]))
            .collect();
        gaps.sort_unstable();
        let fastest = gaps[0];
        let jitter = gaps[gaps.len() - 1] - fastest;
        // Equal timestamps expose batching / coarse receive clocks. Allow
        // one millisecond of quantization noise across these wider pairs;
        // it still needs independent queue evidence before any target cut.
        let batched = t.arrivals.windows(2).any(|w| w[0] == w[1]);
        if fastest < jitter * 2 && !(batched && jitter <= Duration::from_millis(1)) {
            return;
        }
        // An entirely batched train only supplies a high lower bound.
        let secs = gaps[gaps.len() / 2].as_secs_f64().max(1e-4);
        let kbps = ((t.len + OVERHEAD) as f64 * width as f64 * 8.0 / secs / 1000.0)
            .min(f64::from(u32::MAX)) as u32;
        self.best = self.best.max(kbps.max(1));
    }

    /// Fastest confident wire rate since the last call; 0 means unknown.
    pub fn take_kbps(&mut self, now: Instant) -> u32 {
        if self
            .current
            .as_ref()
            .and_then(|t| t.arrivals.last())
            .is_some_and(|last| now.saturating_duration_since(*last) >= QUIET)
        {
            self.finish();
        }
        std::mem::take(&mut self.best)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn batched_millisecond_timestamps_keep_clean_rates_usable() {
        let start = Instant::now();
        for cap in [5000u64, 20_000, 30_000, 50_000, 100_000] {
            let mut meter = TrainMeter::default();
            for i in 0..16u8 {
                let nanos = u64::from(i) * 10_240_000_000 / cap;
                let millis = nanos.div_ceil(1_000_000);
                meter.push(1, i, 16, 1200, start + Duration::from_millis(millis));
            }
            let rate = meter.take_kbps(start + Duration::from_secs(1));
            assert!(u64::from(rate) >= cap * 3 / 4, "{cap}: {rate}");
        }
    }

    #[test]
    fn damaged_positions_do_not_fabricate_capacity() {
        let start = Instant::now();
        for mode in 0..4 {
            let mut events = Vec::new();
            for i in 0..16u8 {
                if mode == 0 && i % 2 == 1 && i != 15 || mode == 1 && (5..9).contains(&i) {
                    continue;
                }
                let at = u64::from(i) * 102400
                    + if mode == 3 && (8..15).contains(&i) {
                        4_000_000
                    } else {
                        0
                    };
                events.push((at, i));
                if mode == 2 {
                    events.push((at + 1000, i));
                }
            }
            events.sort_unstable();
            let mut meter = TrainMeter::default();
            for (at, i) in events {
                meter.push(1, i, 16, 1200, start + Duration::from_nanos(at));
            }
            let rate = meter.take_kbps(start + Duration::from_secs(1));
            assert_eq!(rate, if mode == 2 { 100_000 } else { 0 }, "mode {mode}");
        }
    }

    #[test]
    fn distributed_arrival_jitter_is_not_a_slow_bottleneck() {
        let start = Instant::now();
        let mut seed = 17u64;
        for _ in 0..10_000 {
            let mut events = Vec::new();
            for i in 0..16u8 {
                seed ^= seed >> 12;
                seed ^= seed << 25;
                seed ^= seed >> 27;
                let jitter = seed.wrapping_mul(0x2545f4914f6cdd1d) % 4_000_000;
                events.push((u64::from(i) * 102400 + jitter, i));
            }
            events.sort_unstable();
            let mut meter = TrainMeter::default();
            for (at, i) in events {
                meter.push(1, i, 16, 1200, start + Duration::from_nanos(at));
            }
            let rate = meter.take_kbps(start + Duration::from_secs(1));
            assert!(rate == 0 || rate >= 75_000, "jitter read {rate}");
        }
    }

    #[test]
    fn spread_measures_the_bottleneck_and_a_lost_tail_still_counts() {
        let start = Instant::now();
        let mut meter = TrainMeter::default();
        // 16 datagrams of 1200 bytes through a 5 Mbps bottleneck.
        let gap = Duration::from_secs_f64(1280.0 * 8.0 / 5_000_000.0);
        for i in 0..16u8 {
            meter.push(1, i, 16, 1200, start + gap * u32::from(i));
        }
        let kbps = meter.take_kbps(start + Duration::from_secs(1));
        assert!((4900..=5100).contains(&kbps), "{kbps}");
        assert_eq!(meter.take_kbps(start + Duration::from_secs(1)), 0);
        // The last six are lost: the train ends when it goes quiet.
        for i in 0..10u8 {
            meter.push(2, i, 16, 1200, start + gap * u32::from(i));
        }
        assert_eq!(meter.take_kbps(start + gap * 9), 0);
        let kbps = meter.take_kbps(start + Duration::from_secs(1));
        assert!((4900..=5100).contains(&kbps), "{kbps}");
        // Too few datagrams arrived to time anything.
        for i in 0..3u8 {
            meter.push(3, i, 16, 1200, start + gap * u32::from(i));
        }
        meter.push(4, 0, 16, 1200, start);
        assert_eq!(meter.take_kbps(start + Duration::from_secs(1)), 0);
        // One receive batch: faster than the viewer can time.
        for i in 0..8u8 {
            meter.push(5, i, 8, 1200, start);
        }
        assert!(meter.take_kbps(start) > 300_000);
        // A fast path through a receiver that stalled 60 ms mid-train: the
        // stall is not the path's rate, and the train says nothing.
        for i in 0..6u8 {
            let stall = if i >= 3 {
                Duration::from_millis(60)
            } else {
                Duration::ZERO
            };
            meter.push(
                6,
                i,
                6,
                1200,
                start + stall + Duration::from_micros(100) * u32::from(i),
            );
        }
        assert_eq!(meter.take_kbps(start + Duration::from_secs(1)), 0);
        // Random 0-4 ms per-datagram jitter on a 5 Mbps bottleneck still
        // reads as slow, not as the jitter.
        let offsets = [0u64, 3, 1, 4, 2, 0, 3, 1, 2, 4, 0, 2, 1, 3, 4, 0];
        let mut times: Vec<Instant> = (0..16u32)
            .map(|i| start + gap * i + Duration::from_micros(offsets[i as usize] * 250))
            .collect();
        times.sort();
        for (i, at) in times.into_iter().enumerate() {
            meter.push(7, i as u8, 16, 1200, at);
        }
        let kbps = meter.take_kbps(start + Duration::from_secs(1));
        assert!((3500..=8000).contains(&kbps), "{kbps}");
    }
}
