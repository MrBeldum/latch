//! Start-to-start pacing, independent of capture, conversion and encode time.
use std::time::{Duration, Instant};

pub(crate) struct Cadence {
    interval: Duration,
    next: Instant,
}

impl Cadence {
    pub(crate) fn new(fps: u32) -> Self {
        Self {
            interval: Duration::from_secs_f64(1.0 / f64::from(fps.max(1))),
            next: Instant::now(),
        }
    }

    // Advance by slots, including after an idle desktop or an overloaded encoder.
    // Never issue a train of catch-up frames after a stall.
    fn slot(&mut self, now: Instant) -> Instant {
        if now.saturating_duration_since(self.next) >= self.interval {
            let late = now.duration_since(self.next).as_nanos() / self.interval.as_nanos();
            self.next += self.interval * (late.min(u32::MAX as u128 - 1) as u32 + 1);
        }
        let slot = self.next.max(now);
        self.next += self.interval;
        slot
    }

    pub(crate) fn wait(&mut self) -> Instant {
        let now = Instant::now();
        let slot = self.slot(now);
        std::thread::sleep(slot.saturating_duration_since(now));
        slot
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn work_does_not_add_to_the_period_and_overload_skips_slots() {
        for fps in [60, 120, 144] {
            for cost in [0, 3, 5, 10] {
                let mut cadence = Cadence::new(fps);
                let start = cadence.next;
                let work = Duration::from_millis(cost);
                let mut now = start;
                for _ in 0..1000 {
                    let missed = now.saturating_duration_since(cadence.next) >= cadence.interval;
                    let slot = cadence.slot(now);
                    assert!(slot >= now);
                    if missed {
                        assert!(slot > now, "overload must wait for a future slot");
                    }
                    now = slot + work;
                }
                if work <= cadence.interval {
                    let rate = 999.0 / (now - start - work).as_secs_f64();
                    assert!((rate / fps as f64 - 1.0).abs() < 0.02);
                }
            }
        }
    }
}
