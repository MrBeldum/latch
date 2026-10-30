//! A byte token bucket. At most sixteen datagrams are sent without waiting.
use std::time::{Duration, Instant};

pub(crate) struct Pacer {
    tokens: f64,
    updated: Instant,
}
const BURST: f64 = (16 * (latch_proto::MAX_DATAGRAM + 80)) as f64;
impl Pacer {
    pub(crate) fn new(now: Instant) -> Self {
        Self {
            tokens: BURST,
            updated: now,
        }
    }
    pub(crate) fn delay(&mut self, now: Instant, bytes: usize, kbps: u32) -> Duration {
        let rate = f64::from(kbps.max(1)) * 1000.0 / 8.0 * 2.5;
        self.tokens = (self.tokens
            + now.saturating_duration_since(self.updated).as_secs_f64() * rate)
            .min(BURST);
        self.updated = now;
        let bytes = (bytes + 80) as f64; // IP, UDP and tunnel allowance
        if self.tokens >= bytes {
            self.tokens -= bytes;
            Duration::ZERO
        } else {
            Duration::from_secs_f64((bytes - self.tokens) / rate)
        }
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn keyframe_is_a_train_with_a_bounded_initial_burst() {
        let start = Instant::now();
        let mut now = start;
        let mut p = Pacer::new(now);
        for index in 0..500 {
            let delay = p.delay(now, 1200, 20_000);
            if index < 16 {
                assert!(delay.is_zero());
            }
            now += delay;
            if !delay.is_zero() {
                assert!(p
                    .delay(now + Duration::from_nanos(1), 1200, 20_000)
                    .is_zero());
            }
        }
        assert!(now - start > Duration::from_millis(90));
    }
}
