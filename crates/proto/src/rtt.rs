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
        if let Some(srtt) = self.srtt {
            self.rttvar = self.rttvar.mul_f64(0.75) + srtt.abs_diff(rtt).mul_f64(0.25);
            self.srtt = Some(srtt.mul_f64(0.875) + rtt.mul_f64(0.125));
        } else {
            self.srtt = Some(rtt);
            self.rttvar = rtt / 2;
        }
    }

    pub fn srtt(&self) -> Option<Duration> {
        self.srtt
    }

    pub fn rttvar(&self) -> Duration {
        self.rttvar
    }
}
