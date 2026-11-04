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
    /// Arrival times, and wire bytes after the first datagram.
    arrivals: Vec<Instant>,
    bytes: usize,
}

/// The viewer's side: the fastest train between two reports.
#[derive(Default)]
pub struct TrainMeter {
    current: Option<Train>,
    best: u32,
}

impl TrainMeter {
    /// One `Probe` datagram of `len` bytes, as it arrived.
    pub fn push(&mut self, train: u16, index: u8, count: u8, len: usize, now: Instant) {
        if self.current.as_ref().is_none_or(|t| t.id != train) {
            self.finish();
            self.current = Some(Train {
                id: train,
                count,
                arrivals: vec![now],
                bytes: 0,
            });
        } else if let Some(t) = self.current.as_mut() {
            t.arrivals.push(now);
            t.bytes += len + OVERHEAD;
        }
        if index + 1 >= count {
            self.finish();
        }
    }

    fn finish(&mut self) {
        let Some(t) = self.current.take() else {
            return;
        };
        let received = t.arrivals.len();
        if received < MIN_TRAIN.min(usize::from(t.count)) || received < 2 {
            return;
        }
        // A bottleneck spaces datagrams about evenly; a stalled receiver
        // (or relay) puts one long gap among near-zero ones. A train whose
        // largest gap is over half its span timed the stall, not the path.
        // Datagrams handled in one receive batch share an instant: such a
        // train only shows the path is faster than this viewer can time.
        let span = t.arrivals[received - 1].saturating_duration_since(t.arrivals[0]);
        let largest = t
            .arrivals
            .windows(2)
            .map(|w| w[1].saturating_duration_since(w[0]))
            .max()
            .unwrap_or_default();
        if received > 2 && largest * 2 > span {
            return;
        }
        let secs = span.as_secs_f64().max(1e-4);
        let kbps = (t.bytes as f64 * 8.0 / secs / 1000.0).min(f64::from(u32::MAX)) as u32;
        self.best = self.best.max(kbps.max(1));
    }

    /// The fastest train completed since the last call, in wire kbps; 0
    /// when none was.
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
        assert!(meter.take_kbps(start) > 500_000);
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
