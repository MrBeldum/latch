//! Smoothed round-trip time from `Ping`/`Pong`, as TCP does (RFC 6298:
//! α = 1/8, β = 1/4).

use std::time::Duration;

#[derive(Debug, Clone, Copy, Default)]
pub struct RttEstimator {
    srtt: Option<Duration>,
    rttvar: Duration,
}

impl RttEstimator {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn sample(&mut self, rtt: Duration) {
        let _ = rtt;
        todo!()
    }

    pub fn srtt(&self) -> Option<Duration> {
        self.srtt
    }

    pub fn rttvar(&self) -> Duration {
        self.rttvar
    }
}
