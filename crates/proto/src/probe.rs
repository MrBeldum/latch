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
    first: Instant,
    last: Instant,
    /// Wire bytes after the first datagram, and how many datagrams arrived.
    bytes: usize,
    received: usize,
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
                first: now,
                last: now,
                bytes: 0,
                received: 1,
            });
        } else if let Some(t) = self.current.as_mut() {
            t.last = now;
            t.bytes += len + OVERHEAD;
            t.received += 1;
        }
        if index + 1 >= count {
            self.finish();
        }
    }

    fn finish(&mut self) {
        let Some(t) = self.current.take() else {
            return;
        };
        if t.received < MIN_TRAIN.min(usize::from(t.count)) || t.received < 2 {
            return;
        }
        // Datagrams handled in one receive batch can share an instant: such
        // a train only shows the path is faster than this viewer can time.
        let secs = t.last.duration_since(t.first).as_secs_f64().max(1e-4);
        let kbps = (t.bytes as f64 * 8.0 / secs / 1000.0).min(f64::from(u32::MAX)) as u32;
        self.best = self.best.max(kbps.max(1));
    }

    /// The fastest train completed since the last call, in wire kbps; 0
    /// when none was.
    pub fn take_kbps(&mut self, now: Instant) -> u32 {
        if self
            .current
            .as_ref()
            .is_some_and(|t| now.saturating_duration_since(t.last) >= QUIET)
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
    }
}
