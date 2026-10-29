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
}

impl Link {
    pub fn new(impairment: Impairment, seed: u64) -> Self {
        Self { impairment, seed }
    }

    /// Sends a datagram at `now`.
    pub fn send(&mut self, now: Instant, datagram: &[u8]) {
        let _ = (now, datagram, self.impairment, self.seed);
        todo!()
    }

    /// Datagrams that have arrived by `now`, in arrival order.
    pub fn receive(&mut self, now: Instant) -> Vec<Vec<u8>> {
        let _ = now;
        todo!()
    }

    /// When the next datagram in flight arrives.
    pub fn next_arrival(&self) -> Option<Instant> {
        todo!()
    }

    /// Datagrams sent, lost, duplicated so far.
    pub fn counts(&self) -> (u64, u64, u64) {
        todo!()
    }
}
