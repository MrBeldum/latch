//! Delay/loss feedback over a wire budget (video, parity, headers and audio), with a fake-clock API.
use latch_proto::control::Report;
use std::time::{Duration, Instant};

/// How long a policer's rate caps the climb before it is probed again.
const LOSSY_MEMORY: Duration = Duration::from_secs(3);

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
    /// Mean change in queueing delay between clean windows: radio jitter.
    jitter: f32,
    /// Smoothed viewer-reported received rate.
    received_kbps: f64,
    /// About what got through when loss without a queue last forced a cut:
    /// a policer's rate.
    lossy: Option<(u32, Instant)>,
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
            // Until windows show otherwise, assume a Wi-Fi-like path.
            jitter: 3.0,
            received_kbps: 0.0,
            lossy: None,
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
                }
            }
        }
        // Radio jitter moves even the lowest ping in a window by several ms,
        // so queue thresholds sit above it. Only falls measure it: a filling
        // queue only rises, and the drain after a cut is not counted.
        let fall = (self.previous_delay - delay).clamp(0.0, 10.0) * 2.0;
        self.gradient = self.gradient * 0.5 + (delay - self.previous_delay) * 0.5;
        self.previous_delay = delay;
        let threshold = 6.0f32.max(self.min_rtt[1].min(self.min_rtt[0]) * 0.1);
        queued |= delay > threshold.max(self.jitter * 3.0);
        queued |= delay > 3.0f32.max(self.jitter * 1.5) && self.gradient > self.jitter.max(1.0);
        if !queued && !self.pacing {
            self.jitter += (fall - self.jitter) / 8.0;
        }
        let loss = report.unrecovered_loss_pct.unwrap_or(report.loss_pct);
        // Legacy loss_pct is raw loss. FEC-recovered random radio loss does
        // not cut a new viewer's budget. Uncorrected loss or lost frames in
        // two windows running do, however low the raw loss: a policer drops
        // only the overshoot and never queues, so delay never rises.
        let damaged = loss > 3.0 || report.frames_lost > 0;
        let pressure = queued || queue_ms > 20.0 || damaged;
        self.pressure_windows = if pressure {
            self.pressure_windows.saturating_add(1)
        } else {
            0
        };
        // A 50 ms window holds a few frames, so its rate is only a coarse
        // match for the host's: smooth it for capacity estimates. But under
        // half of what was sent arriving while the rest is dropped or queued
        // is a capacity drop by any margin: cut on this window, not the next.
        let received = report.received_kbps;
        if received > 0 {
            let weight = if self.received_kbps > 0.0 { 0.3 } else { 1.0 };
            self.received_kbps += (f64::from(received) - self.received_kbps) * weight;
        }
        let sent = self.sent_kbps.unwrap_or(0);
        let collapsed =
            received > 0 && sent >= 2000 && received < sent / 2 && (loss > 10.0 || delay > 20.0);
        // While a policer's rate is remembered, damage near it is a repeat of
        // that overshoot, not a blip.
        let lossy = self
            .lossy
            .filter(|(_, at)| now.duration_since(*at) < LOSSY_MEMORY)
            .map(|(kbps, _)| kbps);
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
        let repeat = damaged && lossy.is_some_and(|kbps| self.budget > kbps * 9 / 10);
        if self.pressure_windows >= 2 || collapsed || repeat {
            self.pacing = true;
            self.clean_since = now;
            // A queue that persists after a cut needs another to drain. Loss
            // reported in the window after a cut was mostly sent before it:
            // wait a round trip and a report window past it.
            let rtt = self.base_rtt().unwrap_or_default();
            let spacing = if queued {
                rtt.max(Duration::from_millis(50))
            } else {
                rtt + Duration::from_millis(u64::from(report.window_ms.clamp(25, 250)))
            };
            if now.duration_since(self.last_cut) >= spacing {
                // A target cut above the encoder's actual wire rate has no effect.
                let offered = self.sent_kbps.unwrap_or(self.budget).min(self.budget);
                // Delay gives a capacity estimate before the FIFO drops packets.
                // With loss or a collapse, received throughput bounds the new
                // offered rate.
                let measured = if received > 0 && (damaged || collapsed) {
                    (u64::from(received) * 107 / 100).min(u64::from(u32::MAX)) as u32
                } else {
                    offered
                };
                let cut = if delay > 20.0 || loss > 10.0 { 60 } else { 80 };
                self.budget =
                    ((u64::from(offered.min(measured)) * cut / 100) as u32).max(self.floor);
                // Loss without a queue is a policer, whose rate stays put:
                // remember about what got through, to climb back just under it.
                if damaged && !queued && self.received_kbps > 0.0 {
                    let through = (self.received_kbps * 1.07) as u32;
                    self.lossy = Some((through.min(offered), now));
                }
                self.last_cut = now;
            }
        } else if !pressure
            && now.duration_since(self.clean_since)
                >= Duration::from_millis(200).max(Duration::from_secs_f64(elapsed * 2.0))
        {
            self.pacing = false;
            // Double in 400 ms while clean. A cut can regain 5x headroom in
            // about 1.2 s, rather than spending seconds at an unusable floor.
            // A remembered policer caps the climb until the memory lapses.
            let cap = lossy.map_or(self.ceiling, |kbps| (kbps * 97 / 100).max(self.budget));
            self.budget = self
                .budget
                .saturating_add(
                    (f64::from(self.budget) * (2.0f64.powf(elapsed.min(0.1) / 0.4) - 1.0)) as u32,
                )
                .min(cap)
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

    #[test]
    fn a_policer_rate_caps_the_climb_until_it_lapses() {
        let start = Instant::now();
        let mut c = Controller::new(100_000, start);
        let mut t = start;
        // Lost frames with no queue in two windows: the policer passes ~85.
        for _ in 0..2 {
            t += Duration::from_millis(50);
            c.observe_sent(98_000);
            let r = Report {
                rtt_ms: 10.0,
                window_ms: 50,
                received_kbps: 80_000,
                frames_lost: 2,
                ..Default::default()
            };
            c.update(t, &r, 10.0, 0.0);
        }
        assert!(c.budget() < 85_000, "{}", c.budget());
        let cap = c.lossy.unwrap().0 * 97 / 100;
        for _ in 0..40 {
            t += Duration::from_millis(50);
            c.observe_sent(c.budget());
            let r = Report {
                rtt_ms: 10.0,
                window_ms: 50,
                received_kbps: c.budget(),
                ..Default::default()
            };
            c.update(t, &r, 10.0, 0.0);
            assert!(c.budget() <= cap, "{} > {cap}", c.budget());
        }
        // One damaged window near that rate cuts at once.
        t += Duration::from_millis(50);
        let r = Report {
            rtt_ms: 10.0,
            window_ms: 50,
            received_kbps: c.budget(),
            frames_lost: 1,
            ..Default::default()
        };
        let held = c.budget();
        c.update(t, &r, 10.0, 0.0);
        assert!(c.budget() < held);
        // Clean for longer than the memory: the climb probes the ceiling.
        for _ in 0..80 {
            t += Duration::from_millis(50);
            c.observe_sent(c.budget());
            let r = Report {
                rtt_ms: 10.0,
                window_ms: 50,
                received_kbps: c.budget(),
                ..Default::default()
            };
            c.update(t, &r, 10.0, 0.0);
        }
        assert_eq!(c.budget(), 100_000);
    }
}
