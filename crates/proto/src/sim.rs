//! A deterministic network for tests: loss (independent and in bursts, by a
//! Gilbert–Elliott model), duplication, reordering, delay and jitter. Seeded,
//! so a failing test fails the same way every time.

use std::time::{Duration, Instant};

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Impairment {
    /// Chance a datagram is lost in the good state.
    pub loss: f64,
    /// Chance per datagram of entering the bad (burst) state, and of
    /// leaving it; in the bad state every datagram is lost.
    pub burst_enter: f64,
    pub burst_exit: f64,
    pub duplicate: f64,
    /// Chance a datagram is held back by `reorder_delay` extra.
    pub reorder: f64,
    pub reorder_delay: Duration,
    pub delay: Duration,
    /// Uniform extra delay in `0..jitter`.
    pub jitter: Duration,
}

impl Impairment {
    pub const NONE: Impairment = Impairment {
        loss: 0.0,
        burst_enter: 0.0,
        burst_exit: 1.0,
        duplicate: 0.0,
        reorder: 0.0,
        reorder_delay: Duration::ZERO,
        delay: Duration::ZERO,
        jitter: Duration::ZERO,
    };
}

/// One direction of a simulated link.
pub struct Link {
    impairment: Impairment,
    seed: u64,
    bad: bool,
    flight: Vec<(Instant, Vec<u8>)>,
    counts: (u64, u64, u64),
}

impl Link {
    pub fn new(impairment: Impairment, seed: u64) -> Self {
        Self {
            impairment,
            seed: if seed == 0 { 1 } else { seed },
            bad: false,
            flight: Vec::new(),
            counts: (0, 0, 0),
        }
    }

    fn random(&mut self) -> f64 {
        self.seed ^= self.seed >> 12;
        self.seed ^= self.seed << 25;
        self.seed ^= self.seed >> 27;
        (self.seed.wrapping_mul(0x2545f4914f6cdd1d) >> 11) as f64 / ((1u64 << 53) as f64)
    }
    fn arrival(&mut self, now: Instant) -> Instant {
        let jitter = self.impairment.jitter.mul_f64(self.random());
        let reorder = if self.random() < self.impairment.reorder {
            self.impairment.reorder_delay
        } else {
            Duration::ZERO
        };
        now + self.impairment.delay + jitter + reorder
    }
    /// Sends a datagram at `now`.
    pub fn send(&mut self, now: Instant, datagram: &[u8]) {
        self.counts.0 += 1;
        if self.bad {
            if self.random() < self.impairment.burst_exit {
                self.bad = false;
            }
        } else if self.random() < self.impairment.burst_enter {
            self.bad = true;
        }
        if self.bad || self.random() < self.impairment.loss {
            self.counts.1 += 1;
            return;
        }
        let at = self.arrival(now);
        self.flight.push((at, datagram.to_vec()));
        if self.random() < self.impairment.duplicate {
            self.counts.2 += 1;
            let at = self.arrival(now);
            self.flight.push((at, datagram.to_vec()));
        }
    }

    /// Datagrams that have arrived by `now`, in arrival order.
    pub fn receive(&mut self, now: Instant) -> Vec<Vec<u8>> {
        self.flight.sort_by_key(|x| x.0);
        let end = self.flight.partition_point(|x| x.0 <= now);
        self.flight.drain(..end).map(|x| x.1).collect()
    }

    /// When the next datagram in flight arrives.
    pub fn next_arrival(&self) -> Option<Instant> {
        self.flight.iter().map(|x| x.0).min()
    }

    /// Datagrams sent, lost, duplicated so far.
    pub fn counts(&self) -> (u64, u64, u64) {
        self.counts
    }
}
