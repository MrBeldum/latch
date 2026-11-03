//! Delay/loss feedback over a wire budget (video, parity, headers and audio), with a fake-clock API.
use latch_proto::control::Report;
use std::time::{Duration, Instant};

pub(crate) struct Controller {
    ceiling: u32,
    budget: u32,
    floor: u32,
    min_rtt: [f32; 2],
    last_cut: Instant,
    last_step: Instant,
    clean_since: Instant,
    pressure_windows: u8,
    sent_kbps: Option<u32>,
    pacing: bool,
    previous_delay: f32,
    gradient: f32,
}

impl Controller {
    pub(crate) fn new(ceiling: u32, now: Instant) -> Self {
        Self {
            ceiling,
            budget: ceiling,
            floor: (ceiling / 10).max(1000).min(ceiling),
            min_rtt: [f32::MAX; 2],
            last_cut: now - Duration::from_secs(1),
            last_step: now,
            clean_since: now,
            pressure_windows: 0,
            sent_kbps: None,
            pacing: false,
            previous_delay: 0.0,
            gradient: 0.0,
        }
    }

    pub(crate) fn budget(&self) -> u32 {
        self.budget
    }

    pub(crate) fn observe_sent(&mut self, kbps: u32) {
        self.sent_kbps = (kbps > 0).then_some(kbps);
    }

    pub(crate) fn pacing(&self) -> bool {
        self.pacing
    }

    pub(crate) fn base_rtt(&self) -> Option<Duration> {
        let rtt = self.min_rtt[1].min(self.min_rtt[0]);
        (rtt < f32::MAX).then(|| Duration::from_secs_f32(rtt / 1000.0))
    }

    pub(crate) fn update(
        &mut self,
        now: Instant,
        report: &Report,
        host_rtt_ms: f32,
        queue_ms: f32,
    ) -> bool {
        let mut queued = false;
        let mut delay: f32 = 0.0;
        for (i, rtt) in [report.rtt_ms, host_rtt_ms].into_iter().enumerate() {
            if rtt.is_finite() && rtt > 0.0 {
                self.min_rtt[i] = self.min_rtt[i].min(rtt);
                // Prefer the lowest host ping in this report window. Viewer
                // smoothed RTT includes transient jitter, not just standing queues.
                if i == 1 || host_rtt_ms <= 0.0 {
                    delay = (rtt - self.min_rtt[i]).max(0.0);
                    queued |= delay > 6.0f32.max(self.min_rtt[i] * 0.1);
                }
            }
        }
        self.gradient = self.gradient * 0.5 + (delay - self.previous_delay) * 0.5;
        self.previous_delay = delay;
        queued |= delay > 3.0 && self.gradient > 1.0;
        let loss = report.unrecovered_loss_pct.unwrap_or(report.loss_pct);
        // Legacy loss_pct is raw loss. Only sustained uncorrected loss, not
        // FEC-recovered random radio loss, cuts a new viewer's budget.
        let damaged =
            loss > 3.0 && (report.unrecovered_loss_pct.is_none() || report.loss_pct > 10.0);
        let pressure = queued || queue_ms > 20.0 || damaged;
        self.pressure_windows = if pressure {
            self.pressure_windows.saturating_add(1)
        } else {
            0
        };
        let old = self.budget;
        let elapsed = now
            .saturating_duration_since(self.last_step)
            .as_secs_f64()
            .min(1.0);
        self.last_step = now;
        // Require two windows: a late ping or a short radio burst is a blip.
        if damaged {
            self.clean_since = now;
        }
        let sustained = self.pressure_windows >= 2;
        if sustained {
            self.pacing = true;
            self.clean_since = now;
            if self.pressure_windows >= 2
                && now.duration_since(self.last_cut)
                    >= self
                        .base_rtt()
                        .unwrap_or_default()
                        .max(Duration::from_millis(50))
            {
                // A target cut above the encoder's actual wire rate has no effect.
                let offered = self.sent_kbps.unwrap_or(self.budget).min(self.budget);
                // Delay gives a capacity estimate before the FIFO drops packets.
                // With loss, received throughput bounds the new offered rate.
                let received = report.received_kbps;
                let measured = if received > 0 && damaged {
                    (u64::from(received) * 107 / 100).min(u64::from(u32::MAX)) as u32
                } else {
                    offered
                };
                let cut = if delay > 20.0 || loss > 10.0 { 60 } else { 80 };
                self.budget =
                    ((u64::from(offered.min(measured)) * cut / 100) as u32).max(self.floor);
                self.last_cut = now;
            }
        } else if !pressure
            && now.duration_since(self.clean_since)
                >= Duration::from_millis(200).max(Duration::from_secs_f64(elapsed * 2.0))
        {
            self.pacing = false;
            // Double in 400 ms while clean. A cut can regain 5x headroom in
            // about 1.2 s, rather than spending seconds at an unusable floor.
            self.budget = self
                .budget
                .saturating_add(
                    (f64::from(self.budget) * (2.0f64.powf(elapsed.min(0.1) / 0.4) - 1.0)) as u32,
                )
                .min(self.ceiling);
        }
        self.budget != old
    }
}

/// The encoder budget accounts for rounded FEC and Latch/IP/tunnel headers.
/// Audio is reserved separately; at tiny rates keep a usable video floor.
pub(crate) fn video_budget(wire_kbps: u32, fec: u8, audio: bool) -> u32 {
    let available = wire_kbps
        .saturating_sub(if audio { 1200 } else { 0 })
        .max(wire_kbps / 2);
    ((u64::from(available) * 10000 / (112 * (102 + u64::from(fec)))) as u32).max(1)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn underfilled_encoder_cuts_actual_rate_and_only_pressure_enables_pacing() {
        let start = Instant::now();
        let mut c = Controller::new(50_000, start);
        let mut r = Report {
            rtt_ms: 12.0,
            ..Default::default()
        };
        c.observe_sent(8_000);
        c.update(start, &r, 12.0, 0.0);
        assert!(!c.pacing());
        assert_eq!(c.base_rtt(), Some(Duration::from_millis(12)));
        // Smoothed viewer RTT spikes must not override a clean host window.
        r.rtt_ms = 50.0;
        c.update(start + Duration::from_millis(250), &r, 12.0, 0.0);
        assert!(!c.pacing());
        for tick in 2..=3 {
            c.update(start + Duration::from_millis(tick * 250), &r, 40.0, 0.0);
        }
        assert!(c.pacing());
        assert_eq!(c.budget(), 5_000);
        r.rtt_ms = 12.0;
        c.update(start + Duration::from_secs(3), &r, 12.0, 0.0);
        assert!(!c.pacing());
    }

    #[test]
    fn a_lone_late_window_does_not_stall_recovery() {
        let start = Instant::now();
        let mut c = Controller::new(50_000, start);
        let r = Report {
            rtt_ms: 12.0,
            ..Default::default()
        };
        // Learn the base, then a standing queue cuts the budget.
        c.update(start, &r, 12.0, 0.0);
        let mut t = start;
        for _ in 0..12 {
            t += Duration::from_millis(250);
            c.update(t, &r, 60.0, 0.0);
        }
        let floor = c.budget();
        assert!(floor < 50_000);
        // The queue drains; every fourth window one late pong arrives.
        for i in 0..40 {
            t += Duration::from_millis(250);
            let host = if i % 4 == 3 { 40.0 } else { 12.0 };
            c.update(t, &r, host, 0.0);
        }
        assert!(c.budget() > floor * 2, "{} vs {floor}", c.budget());
        assert!(!c.pacing());
    }

    #[test]
    fn capacity_steps_random_loss_and_steady_path() {
        let start = Instant::now();
        let mut c = Controller::new(100_000, start);
        let mut minimum = 100_000;
        for tick in 0..500 {
            let now = start + Duration::from_millis(tick * 250);
            let capacity = if (8..48).contains(&tick) {
                15_000
            } else {
                100_000
            };
            let congested = c.budget() > capacity;
            let report = Report {
                rtt_ms: if congested { 45.0 } else { 10.0 },
                loss_pct: 2.0,
                unrecovered_loss_pct: Some(0.0),
                ..Default::default()
            };
            c.update(now, &report, report.rtt_ms, 0.0);
            if tick == 28 {
                assert!(c.budget() <= 18_000, "did not converge: {}", c.budget());
            }
            minimum = minimum.min(c.budget());
            if tick < 8 {
                assert_eq!(c.budget(), 100_000);
            }
        }
        assert!(minimum >= 10_000);
        assert_eq!(c.budget(), 100_000, "must recover to ceiling");
    }
}
