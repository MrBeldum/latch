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
    app_limited_windows: u8,
    pacing: bool,
    idle_since: Option<Instant>,
    restart: Option<u32>,
    restart_at: Instant,
    resumed: bool,
    previous_delay: f32,
    gradient: f32,
    /// Mean change in queueing delay between clean windows: radio jitter.
    jitter: f32,
    /// Smoothed viewer-reported received rate.
    received_kbps: f64,
    /// About what got through when loss without a queue last forced a cut:
    /// a policer's rate.
    lossy: Option<(u32, Instant)>,
    /// Sustainable FIFO wire rate and the next bounded probe's deadline.
    capacity: Option<u32>,
    probe_at: Instant,
    /// The budget before the probe in flight, and when it started.
    probe: Option<(u32, Instant)>,
    /// The next probe's fraction above capacity; doubles while probes hold.
    step: f64,
    /// The offered rate when the path last congested.
    overload: Option<u32>,
    /// A lossless RTT plateau that survives a fourfold load reduction.
    route: Option<(f32, Instant, u32)>,
    loss_since: Option<Instant>,
    severe_since: Option<Instant>,
    /// A reversible 20% reduction tests whether queue-free loss follows load.
    loss_probe: Option<LossProbe>,
    random_until: Instant,
    policer_until: Instant,
    random_floor: Option<f32>,
}

struct LossProbe {
    budget: u32,
    sent: u32,
    loss: f32,
    at: Instant,
    sum: f32,
    samples: u32,
}

impl Controller {
    pub(crate) fn new(ceiling: u32, now: Instant) -> Self {
        Self {
            ceiling,
            budget: ceiling,
            floor: 1000.min(ceiling),
            min_rtt: [f32::MAX; 2],
            last_cut: now - Duration::from_secs(1),
            last_step: now,
            clean_since: now,
            pressure_windows: 0,
            sent_kbps: None,
            app_limited_windows: 0,
            pacing: false,
            idle_since: None,
            restart: None,
            restart_at: now,
            resumed: false,
            previous_delay: 0.0,
            gradient: 0.0,
            // Until windows show otherwise, assume a Wi-Fi-like path.
            jitter: 3.0,
            received_kbps: 0.0,
            lossy: None,
            capacity: None,
            probe_at: now,
            probe: None,
            step: 0.0,
            overload: None,
            route: None,
            loss_since: None,
            severe_since: None,
            loss_probe: None,
            random_until: now,
            policer_until: now,
            random_floor: None,
        }
    }

    pub(crate) fn budget(&self) -> u32 {
        self.budget
    }

    pub(crate) fn observe_sent(&mut self, kbps: u32) {
        self.sent_kbps = Some(kbps);
        self.app_limited_windows = if kbps < self.budget * 4 / 5 {
            self.app_limited_windows.saturating_add(1)
        } else {
            0
        };
    }

    pub(crate) fn application_limited(&self) -> bool {
        self.app_limited_windows >= 4
    }

    pub(crate) fn pacing(&self) -> bool {
        self.pacing
    }

    /// After idle, the old target is not proof that a packet train still fits.
    pub(crate) fn pace_kbps(&self) -> u32 {
        self.restart.map_or(
            if self.pacing {
                self.budget.saturating_mul(5) / 2
            } else {
                0
            },
            |rate| rate.min(self.budget),
        )
    }

    /// Bound restart frames to 80 ms of the probe rate; once motion is seen,
    /// feed the encoder at that rate so a stale target cannot build a FIFO.
    pub(crate) fn encoder_wire_budget(&self, fps: u32) -> u32 {
        self.restart.map_or(self.budget, |rate| {
            let frames = if self.resumed {
                1.25
            } else {
                f64::from(fps) * 0.08
            };
            self.budget.min((f64::from(rate) * frames.max(1.0)) as u32)
        })
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
        let falling = delay + 0.5 < self.previous_delay;
        self.gradient = self.gradient * 0.5 + (delay - self.previous_delay) * 0.5;
        self.previous_delay = delay;
        let jitter = self
            .jitter
            .max(if self.application_limited() && !self.pacing {
                3.0
            } else {
                0.0
            });
        let threshold = 6.0f32.max(self.min_rtt[1].min(self.min_rtt[0]) * 0.1);
        queued |= delay > threshold.max(jitter * 3.0);
        queued |= delay > 3.0f32.max(jitter * 1.5) && self.gradient > jitter.max(1.0);
        if !queued && !self.pacing {
            self.jitter += (fall - self.jitter) / 8.0;
        }
        let loss = report.unrecovered_loss_pct.unwrap_or(report.loss_pct);
        // FEC-recovered loss is not congestion. Residual damage starts a
        // rate-correlation test below; severe loss and legacy reports without
        // usable rate feedback retain the conservative two-window fallback.
        let mut damaged = report.frames_lost > 0
            || report
                .unrecovered_loss_pct
                .map_or(report.loss_pct > 10.0, |loss| loss > 3.0);
        let rtt = self.base_rtt().unwrap_or_default();
        let window = Duration::from_millis(u64::from(report.window_ms.clamp(25, 1000)));
        // Coarse windows measure the bottleneck less precisely: hold further under it.
        let margin = if window >= Duration::from_millis(200) {
            92
        } else {
            97
        };
        let horizon = (rtt * 2
            + window
                * if window >= Duration::from_millis(200) {
                    2
                } else {
                    1
                })
        .max(Duration::from_millis(50));
        // A permanent route change doesn't drain when load falls. Require
        // lossless, stable RTT through a >=4x reduction before rebasing; a
        // real FIFO's falling RTT, loss or local queue cancels this evidence.
        let sample = if host_rtt_ms.is_finite() && host_rtt_ms > 0.0 {
            host_rtt_ms
        } else {
            report.rtt_ms
        };
        if queued && !damaged && queue_ms < 1.0 && sample.is_finite() {
            let (level, since, load) = *self.route.get_or_insert((sample, now, self.budget));
            if (sample - level).abs() > 1.0 {
                self.route = Some((sample, now, self.budget));
            } else if now.duration_since(since) >= Duration::from_secs(2) && self.budget <= load / 4
            {
                for (minimum, current) in self.min_rtt.iter_mut().zip([report.rtt_ms, host_rtt_ms])
                {
                    if current.is_finite() && current > 0.0 {
                        *minimum = current;
                    }
                }
                self.previous_delay = 0.0;
                self.gradient = 0.0;
                delay = 0.0;
                queued = false;
                self.capacity = None;
                self.overload = None;
                self.step = 0.0;
                self.probe = None;
                self.route = None;
            }
        } else {
            self.route = None;
        }
        // A small probe needs a smaller queue threshold than an emergency
        // cut. Wait for its own feedback before increasing again.
        let probe_queued = self.probe.is_some() && delay > 3.0f32.max(self.jitter * 2.0);
        queued |= probe_queued || (self.restart.is_some() && delay > 6.0);
        let old = self.budget;
        let underfilled = self
            .sent_kbps
            .is_some_and(|sent| sent < self.budget * 4 / 5);
        let deficit = self
            .sent_kbps
            .is_some_and(|sent| report.received_kbps < sent * 9 / 10);
        let severe = if loss > 10.0 && deficit {
            let since = *self.severe_since.get_or_insert(now);
            now.duration_since(since) >= horizon.max(Duration::from_millis(500))
        } else {
            self.severe_since = None;
            false
        };
        if underfilled && !queued && loss > 10.0 && !severe {
            damaged = false;
        }
        if self
            .random_floor
            .is_some_and(|floor| floor > 1.0 && report.loss_pct <= floor + 2.0)
            && !queued
        {
            damaged = false;
        }
        let app_limited = self.application_limited() && !self.pacing;
        // Give parity feedback a chance before testing loss against offered load.
        // A radio burst cannot prove a policer: loss must improve at a lower
        // *actual* send rate. Never ratchet an inconclusive test down again.
        if !queued
            && self.sent_kbps.is_some()
            && report.received_kbps > 0
            && (report.loss_pct > 0.0 || self.loss_probe.is_some())
            && loss <= 10.0
        {
            let raw = report.loss_pct.max(loss);
            let mut confirmed = false;
            if let Some(probe) = self.loss_probe.as_mut() {
                if now.duration_since(probe.at) >= horizon {
                    probe.sum += raw;
                    probe.samples += 1;
                }
                if now.duration_since(probe.at) >= horizon + Duration::from_secs(1) {
                    let probe = self.loss_probe.take().unwrap();
                    confirmed = self.sent_kbps.unwrap_or(0) < probe.sent * 9 / 10
                        && probe.samples > 0
                        && (probe.sum / (probe.samples as f32) < probe.loss * 0.5
                            || probe.loss - probe.sum / (probe.samples as f32)
                                > (probe.loss * 0.35).max(2.0));
                    if confirmed {
                        let floor = probe.sum / probe.samples as f32;
                        let capacity = (f64::from(probe.sent) * f64::from(100.0 - probe.loss)
                            / f64::from((100.0 - floor).max(1.0)))
                            as u32;
                        // Tiny raw loss can hide a policer dropping parity:
                        // retain the conservative tested rate in that case.
                        let capacity = if probe.loss < 5.0 {
                            self.capacity
                                .filter(|rate| *rate < probe.sent)
                                .unwrap_or(probe.sent * 4 / 5)
                        } else {
                            capacity
                        };
                        self.random_floor = Some(floor);
                        self.capacity = Some(capacity);
                        self.lossy = Some((capacity, now));
                        self.budget = (capacity * 97 / 100).max(self.floor);
                        self.probe = None;
                        self.step = 0.0;
                        self.probe_at = now + Duration::from_secs(10);
                    } else {
                        self.budget = probe.budget;
                        self.random_until = now + LOSSY_MEMORY;
                    }
                    self.loss_since = None;
                }
            } else if damaged && now >= self.random_until {
                let since = *self.loss_since.get_or_insert(now);
                if now.duration_since(since) >= Duration::from_millis(500) {
                    self.loss_probe = Some(LossProbe {
                        budget: self.budget,
                        sent: self.sent_kbps.unwrap_or(self.budget),
                        loss: raw.max(0.1),
                        at: now,
                        sum: 0.0,
                        samples: 0,
                    });
                    self.budget = (self.sent_kbps.unwrap_or(self.budget).min(self.budget) * 4 / 5)
                        .max(self.floor);
                }
            } else if !damaged {
                self.loss_since = None;
            }
            if confirmed {
                self.policer_until = now + Duration::from_secs(10);
                self.pressure_windows = 0;
            }
            damaged = false;
        } else if queued {
            self.loss_probe = None;
            self.loss_since = None;
        }
        // Expire idle capacity without lowering the encoder target. Restart
        // with a conservative, strictly paced probe, then validate increases
        // one feedback horizon at a time. Queueing or damage lowers the cap.
        if self.application_limited()
            && self.sent_kbps.unwrap_or(0) < self.budget / 2
            && self
                .restart
                .is_none_or(|rate| self.sent_kbps.unwrap_or(0) < rate / 4)
        {
            let since = *self.idle_since.get_or_insert(now);
            if now.duration_since(since) >= Duration::from_secs(2) {
                self.restart = Some(self.sent_kbps.unwrap_or(0).max(5000).min(self.budget));
                self.restart_at = now;
                self.resumed = false;
                self.capacity = None;
            }
        } else {
            self.idle_since = None;
        }
        if let Some(rate) = self.restart {
            if queued || damaged {
                self.restart = Some(rate.min(report.received_kbps.max(self.floor)));
                self.resumed = true;
            } else if self.sent_kbps.is_some_and(|sent| sent >= rate * 4 / 5)
                && now.duration_since(self.restart_at) >= horizon
            {
                self.restart_at = now;
                self.resumed = true;
                // Bound each probe to at most 4 ms of excess at the last
                // delivered rate, even across a long feedback round trip.
                let step = (0.004 / horizon.as_secs_f64()).min(1.0);
                let next = (f64::from(rate) * (1.0 + step)) as u32;
                self.restart = Some(next.min(self.budget));
            }
        }
        let pressure = queued || (queue_ms > 20.0 && !app_limited) || damaged;
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
        // The path carries at least what it has been seen to deliver.
        if let Some(kbps) = self.capacity.as_mut() {
            *kbps = (*kbps).max(self.received_kbps as u32);
        }
        let sent = self.sent_kbps.unwrap_or(0);
        let collapsed = received > 0
            && sent >= 2000
            && received < sent / 2
            && ((loss > 10.0 && (!underfilled || severe)) || delay > 20.0);
        // While a policer's rate is remembered, damage near it is a repeat of
        // that overshoot, not a blip.
        let lossy = self
            .lossy
            .filter(|(_, at)| now.duration_since(*at) < LOSSY_MEMORY || now < self.policer_until)
            .map(|(kbps, _)| kbps);
        let elapsed = now.saturating_duration_since(self.last_step);
        // Reports up to 2 s apart integrate all elapsed time (including 5.1's
        // 250 ms cadence). After a longer silence require fresh feedback;
        // never spend missing time as permission for a burst of growth.
        let stale = elapsed > Duration::from_secs(2);
        self.last_step = now;
        // Require two windows: a late ping or a short radio burst is a blip.
        if damaged {
            self.clean_since = now;
        }
        let repeat = damaged && lossy.is_some_and(|kbps| self.budget > kbps * 9 / 10);
        if self.pressure_windows >= 2 || collapsed || repeat || probe_queued {
            self.pacing = true;
            self.clean_since = now;
            // A queue that persists after a cut needs another to drain. Loss
            // reported in the window after a cut was mostly sent before it:
            // wait a round trip and a report window past it.
            let rtt = self.base_rtt().unwrap_or_default();
            let spacing = if queued && damaged {
                (rtt + window).max(Duration::from_millis(50))
            } else if queued {
                horizon
            } else {
                rtt + Duration::from_millis(u64::from(report.window_ms.clamp(25, 250)))
            };
            // Loss in a window that delivered more than is now sent was
            // sent before the last cut: it says nothing new.
            let late = !queued && !collapsed && received > self.budget.max(sent);
            // Soon after a cut, a falling queue is draining as intended.
            let draining = falling
                && !damaged
                && !collapsed
                && !probe_queued
                && now.duration_since(self.last_cut) < horizon * 3;
            if now.duration_since(self.last_cut) >= spacing && !late && !draining {
                // A target cut above the encoder's actual wire rate has no effect.
                let offered = if app_limited && queued && !damaged && self.restart.is_none() {
                    self.budget
                } else {
                    self.sent_kbps.unwrap_or(self.budget).min(self.budget)
                };
                // What got through bounds the new offered rate: a standing
                // FIFO delivers its bottleneck rate. A viewer that omits
                // throughput still reports loss: about the offered rate less
                // the share lost got through.
                let through = if received > 0 {
                    received
                } else {
                    (f64::from(offered) * (1.0 - f64::from(loss.clamp(0.0, 100.0)) / 100.0)) as u32
                };
                let measured = if app_limited && queued && !damaged && self.restart.is_none() {
                    offered
                } else if received > 0 || damaged || collapsed {
                    (u64::from(through) * 107 / 100).min(u64::from(u32::MAX)) as u32
                } else {
                    offered
                };
                let cut = if delay > 20.0 || loss > 10.0 { 60 } else { 80 };
                self.budget =
                    ((u64::from(offered.min(measured)) * cut / 100) as u32).max(self.floor);
                let probe = self.probe.take();
                self.step = 0.0;
                self.overload = Some(offered);
                if let Some((before, _)) = probe.filter(|_| !collapsed && !damaged) {
                    // Only the probe's excess queued: go as far under the
                    // last rate that held as the probe went over it, so that
                    // excess drains as fast as it built, then climb back.
                    let held = self.capacity.unwrap_or(before);
                    let drain = held.saturating_sub(offered.saturating_sub(held));
                    self.budget = drain.max(held * 9 / 10).min(before).max(self.floor);
                } else {
                    // A queue without loss or throughput feedback says only
                    // that the offered rate was too high, not what the path
                    // carries: recover by elapsed time.
                    self.capacity = ((!app_limited || damaged) && (received > 0 || loss > 0.0))
                        .then(|| through.min(offered).max(self.floor));
                }
                self.probe_at = now + Duration::from_millis(500).max(horizon);
                // Loss without a queue is a policer, whose rate stays put:
                // remember about what got through, to climb back just under it.
                if damaged && !queued && self.received_kbps > 0.0 {
                    let through = (self.received_kbps * 1.07) as u32;
                    self.lossy = Some((through, now));
                }
                self.last_cut = now;
            }
        } else if !pressure
            && self.loss_probe.is_none()
            && now.duration_since(self.clean_since) >= Duration::from_millis(200)
        {
            self.pacing = false;
            if stale {
                self.clean_since = now;
                self.probe = None;
                self.probe_at = now + horizon;
            } else {
                let cap = lossy.map_or(self.ceiling, |kbps| kbps * 97 / 100);
                let growth =
                    (f64::from(self.budget) * 2.0f64.powf(elapsed.as_secs_f64() / 0.4)) as u32;
                // Bound a first probe's excess to a few ms of queue even on
                // long RTT paths; only one increase is in flight at a time.
                let base = (0.0025 / horizon.as_secs_f64()).clamp(0.01, 0.05);
                if let Some((_, at)) = self.probe {
                    if now.duration_since(at) >= horizon {
                        // The probe survived a feedback round trip: its rate
                        // holds, and the next probe may reach twice as far.
                        if !app_limited {
                            self.capacity = Some(self.budget);
                            self.step = (self.step.max(base) * 2.0).min(1.0);
                        }
                        self.probe = None;
                    }
                }
                if self.probe.is_some() {
                    // Hold the probe's rate until its feedback arrives.
                } else if let Some(capacity) = self.capacity {
                    let safe = (capacity * margin / 100).max(self.floor).min(cap);
                    if self.budget < safe {
                        self.budget = growth.min(safe);
                    } else if now >= self.probe_at && self.budget < cap && !app_limited {
                        self.probe = Some((self.budget, now));
                        let step = self.step.max(base);
                        let mut target = (f64::from(capacity) * (1.0 + step)) as u32;
                        // Approach the rate that last congested by steps; only
                        // a probe that holds there may accelerate past it.
                        if let Some(kbps) = self.overload.filter(|kbps| capacity < *kbps) {
                            target =
                                target.min(kbps.max((f64::from(capacity) * (1.0 + base)) as u32));
                        }
                        self.budget = target.min(cap);
                    }
                } else {
                    self.budget = growth.min(cap);
                }
                self.budget = self.budget.min(self.ceiling);
            }
        }
        self.budget != old
    }
}

/// The encoder budget accounts for rounded FEC and Latch/IP/tunnel headers.
/// Audio is reserved separately; at tiny rates keep a usable video floor.
pub(crate) fn video_budget(
    wire_kbps: u32,
    fec: u8,
    burst: u8,
    fps: u32,
    audio: bool,
    protected: bool,
) -> u32 {
    let available = wire_kbps
        .saturating_sub(if audio { 1200 } else { 0 })
        .max(wire_kbps / 2);
    let fps = u64::from(fps.max(1));
    let allowance = u64::from(available) * 1000 * 96 / 100 / 8 / fps;
    // Five minimum datagrams cannot fit a smaller frame slot. Permit only
    // that irreducible layout; the sender still paces its actual wire bytes.
    let allowance = allowance.max(if protected { 5 * 109 } else { 0 });
    let mut low = 0;
    let mut high = available;
    // Use the packetizer's rounded layout. Percentage-only reservation can
    // exceed the wire budget on small frames once a burst needs extra shards.
    while low < high {
        let mid = low + (high - low).div_ceil(2);
        let bytes = u64::from(mid) * 1000 / 8 / fps;
        let len = bytes.clamp(1, latch_proto::MAX_SHARD_LEN as u64);
        let shards = bytes.div_ceil(len).max(1);
        let blocks = shards.div_ceil(latch_proto::MAX_BLOCK_DATA as u64);
        let mut packets = shards;
        for block in 0..blocks {
            let data = shards / blocks + u64::from(block < shards % blocks);
            let parity =
                latch_proto::video::fec_parity_shards(data as usize, fec, burst, protected) as u64;
            packets += parity;
        }
        if packets * (len + 108) <= allowance {
            low = mid;
        } else {
            high = mid - 1;
        }
    }
    low.max(1)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn loss_only_reductions_require_rate_correlation() {
        for policer in [false, true] {
            let start = Instant::now();
            let mut c = Controller::new(100_000, start);
            let mut minimum = c.budget();
            for tick in 0..160 {
                let sent = c.budget();
                let raw = if policer {
                    if sent > 85_000 {
                        8.0
                    } else {
                        0.0
                    }
                } else {
                    5.0
                };
                c.observe_sent(sent);
                c.update(
                    start + Duration::from_millis(tick * 50),
                    &Report {
                        rtt_ms: 10.0,
                        window_ms: 50,
                        loss_pct: raw,
                        unrecovered_loss_pct: Some(if raw > 0.0 { 4.0 } else { 0.0 }),
                        frames_lost: u64::from(raw > 0.0),
                        received_kbps: (sent as f32 * (1.0 - raw / 100.0)) as u32,
                        ..Default::default()
                    },
                    10.0,
                    0.0,
                );
                minimum = minimum.min(c.budget());
            }
            if policer {
                assert!(minimum < 80_000, "must confirm the rate test");
            } else {
                assert!(minimum >= 80_000, "random loss ratcheted budget: {minimum}");
            }
        }
    }

    #[test]
    fn idle_retains_a_sustainable_budget_and_zero_is_application_limited() {
        let start = Instant::now();
        let mut c = Controller::new(50_000, start);
        c.capacity = Some(50_000);
        for _ in 0..4 {
            c.observe_sent(0);
        }
        for tick in 0..200 {
            c.observe_sent(if tick % 2 == 0 { 0 } else { 1000 });
            c.update(
                start + Duration::from_millis(tick * 50),
                &Report {
                    rtt_ms: 10.0,
                    received_kbps: 1000,
                    unrecovered_loss_pct: Some(0.0),
                    window_ms: 50,
                    ..Default::default()
                },
                10.0,
                25.0,
            );
            assert!(c.application_limited());
            assert_eq!(c.budget(), 50_000);
            if tick >= 40 {
                assert!(c.capacity.is_none());
            }
        }
        assert!(c.pace_kbps() > 0);
        assert!(c.encoder_wire_budget(120) < c.budget());
        assert!(video_budget(1000, 10, 0, 240, false, false) > 0);
        c.observe_sent(50_000);
        assert!(!c.application_limited());
        assert_eq!(c.budget(), 50_000);
    }

    #[test]
    fn recovery_integrates_report_time_and_rejects_stale_credit() {
        // Same fake-clock setup as the independent review's recovery probe.
        for interval in [50, 250, 1000] {
            let start = Instant::now();
            let mut c = Controller::new(100_000, start);
            let clean = Report {
                rtt_ms: 10.0,
                window_ms: interval,
                ..Default::default()
            };
            c.update(start, &clean, 10.0, 0.0);
            for ms in [50, 100, 200] {
                c.update(
                    start + Duration::from_millis(ms),
                    &Report::default(),
                    40.0,
                    0.0,
                );
            }
            let cut = c.budget();
            let mut doubled = None;
            let mut ceiling = None;
            for tick in 1..=40 {
                let ms = u64::from(tick * interval);
                c.update(start + Duration::from_millis(200 + ms), &clean, 10.0, 0.0);
                if c.budget() >= cut * 2 {
                    doubled.get_or_insert(ms);
                }
                if c.budget() == 100_000 {
                    ceiling.get_or_insert(ms);
                }
            }
            println!(
                "RECOVERY interval={interval} cut={cut} double_ms={} ceiling_ms={}",
                doubled.unwrap(),
                ceiling.unwrap()
            );
            assert!(doubled.unwrap() <= 1000);
            assert!(ceiling.unwrap() <= 2000);
            c.budget = cut;
            c.update(start + Duration::from_secs(60), &clean, 10.0, 0.0);
            assert_eq!(c.budget(), cut, "silence is not growth credit");
            c.update(start + Duration::from_millis(60_250), &clean, 10.0, 0.0);
            assert!(c.budget() > cut, "fresh reports resume growth");
        }
    }

    #[test]
    fn route_rebase_requires_lossless_delay_unresponsive_to_load_reduction() {
        for damaged in [false, true] {
            let start = Instant::now();
            let mut c = Controller::new(100_000, start);
            let mut r = Report {
                rtt_ms: 10.0,
                window_ms: 50,
                ..Default::default()
            };
            c.update(start, &r, 10.0, 0.0);
            for tick in 1..=100 {
                r.rtt_ms = 100.0;
                r.frames_lost = u64::from(damaged);
                c.update(start + Duration::from_millis(tick * 50), &r, 100.0, 0.0);
            }
            if damaged {
                assert_eq!(c.base_rtt(), Some(Duration::from_millis(10)));
                assert_eq!(c.budget(), 1000);
            } else {
                assert!(
                    c.base_rtt().unwrap().abs_diff(Duration::from_millis(100))
                        < Duration::from_micros(1)
                );
                assert_eq!(c.budget(), 100_000);
            }
        }
        // A FIFO responds to reduced offered load: its falling RTT must
        // invalidate the route-change candidate, even without any loss.
        let start = Instant::now();
        let mut c = Controller::new(100_000, start);
        let r = Report {
            rtt_ms: 10.0,
            window_ms: 50,
            ..Default::default()
        };
        c.update(start, &r, 10.0, 0.0);
        for tick in 1..=100 {
            c.update(
                start + Duration::from_millis(tick * 50),
                &r,
                100.0 - tick as f32 * 0.8,
                0.0,
            );
        }
        assert_eq!(c.base_rtt(), Some(Duration::from_millis(10)));
    }

    #[test]
    fn underfilled_encoder_cuts_actual_rate_and_only_pressure_enables_pacing() {
        let start = Instant::now();
        let mut c = Controller::new(50_000, start);
        let mut r = Report {
            rtt_ms: 12.0,
            ..Default::default()
        };
        for _ in 0..4 {
            c.observe_sent(8_000);
        }
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
        assert_eq!(c.budget(), 30_000);
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
        assert!(minimum >= 1_000);
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
