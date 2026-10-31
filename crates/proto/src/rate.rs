//! Adaptive bitrate. The host starts at the bitrate the viewer asked for,
//! lowers it when the viewer reports loss or the round trip grows (a queue
//! filling somewhere on the path), and climbs back towards the asked-for
//! rate once the path has been clean for a while. A stream that fits its
//! path loses no frames; one that does not loses a frame, waits a round
//! trip for a keyframe and sends that keyframe into the same full queue.
//!
//! The viewer's `Report` (once a second) brings the loss: every datagram
//! the network lost, whether FEC rebuilt it or not. Round trips come from
//! the host's own pings; the lowest round trip of the last ten seconds is
//! the path's base, and anything above it is queueing.

use std::collections::VecDeque;
use std::time::{Duration, Instant};

/// Loss at or above this many percent means the path is full.
const LOSS_CONGESTED: f32 = 4.0;
/// Loss at or above this many percent means it is badly over.
const LOSS_SEVERE: f32 = 10.0;
/// Below this the path counts as clean (Wi-Fi drops a little regardless).
const LOSS_CLEAN: f32 = 1.5;
/// Queueing past `max(this, base / 4)` means the path is full.
const QUEUE_CONGESTED: Duration = Duration::from_millis(20);
const BACKOFF: f64 = 0.85;
const BACKOFF_SEVERE: f64 = 0.65;
/// After a decrease, no increase for this long.
const HOLD: Duration = Duration::from_secs(2);
/// The path must be clean this long before each increase.
const CLEAN_FOR: Duration = Duration::from_secs(2);
/// Each increase multiplies by this and adds a hundredth of the maximum,
/// so a rate far below the maximum still climbs at a useful pace.
const GROWTH: f64 = 1.08;
/// The base round trip is the lowest seen over this long: long enough to
/// see past a queue that fills and drains, short enough that a path moving
/// to a farther relay is not taken for a queue for long.
const BASE_WINDOW: Duration = Duration::from_secs(10);
/// Never below a tenth of the asked-for rate, nor below this.
const FLOOR_KBPS: u32 = 500;

#[derive(Debug, Clone)]
pub struct RateControl {
    max: u32,
    floor: u32,
    rate: u32,
    rtts: VecDeque<(Instant, Duration)>,
    hold_until: Option<Instant>,
    clean_since: Option<Instant>,
}

impl RateControl {
    /// `max_kbps`: what the viewer asked for, and where the rate starts.
    pub fn new(max_kbps: u32) -> Self {
        let max = max_kbps.max(1);
        Self {
            max,
            floor: (max / 10).max(FLOOR_KBPS).min(max),
            rate: max,
            rtts: VecDeque::new(),
            hold_until: None,
            clean_since: None,
        }
    }

    pub fn rate(&self) -> u32 {
        self.rate
    }

    /// The lowest round trip of the last ten seconds.
    pub fn base_rtt(&self) -> Option<Duration> {
        self.rtts.iter().map(|&(_, rtt)| rtt).min()
    }

    /// One viewer report: `loss_pct` of the datagrams sent were lost on the
    /// way, and `rtt` is the round trip measured since the last report (its
    /// lowest sample, so one late pong is not taken for a queue). Returns
    /// the new rate when it changed.
    pub fn report(&mut self, now: Instant, loss_pct: f32, rtt: Option<Duration>) -> Option<u32> {
        let queue = rtt.and_then(|rtt| {
            while self
                .rtts
                .front()
                .is_some_and(|&(t, _)| now.saturating_duration_since(t) > BASE_WINDOW)
            {
                self.rtts.pop_front();
            }
            self.rtts.push_back((now, rtt));
            let base = self.base_rtt()?;
            Some((rtt.saturating_sub(base), base))
        });
        let congested_queue =
            queue.is_some_and(|(queue, base)| queue > QUEUE_CONGESTED.max(base / 4));
        let factor = if loss_pct >= LOSS_SEVERE {
            BACKOFF_SEVERE
        } else if loss_pct >= LOSS_CONGESTED || congested_queue {
            BACKOFF
        } else {
            1.0
        };
        let before = self.rate;
        if factor < 1.0 {
            self.rate = ((self.rate as f64 * factor) as u32).max(self.floor);
            self.hold_until = Some(now + HOLD);
            self.clean_since = None;
        } else {
            let clean = loss_pct < LOSS_CLEAN
                && queue.is_none_or(|(queue, base)| queue <= QUEUE_CONGESTED.max(base / 4) / 2);
            if clean {
                let since = *self.clean_since.get_or_insert(now);
                let held = self.hold_until.is_some_and(|t| now < t);
                if !held && now.saturating_duration_since(since) >= CLEAN_FOR {
                    let grown = self.rate as f64 * GROWTH + self.max as f64 / 100.0;
                    self.rate = (grown as u32).min(self.max);
                }
            } else {
                self.clean_since = None;
            }
        }
        (self.rate != before).then_some(self.rate)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const MS: Duration = Duration::from_millis(1);

    fn run(rc: &mut RateControl, t: &mut Instant, secs: u32, loss: f32, rtt: Duration) {
        for _ in 0..secs {
            *t += Duration::from_secs(1);
            rc.report(*t, loss, Some(rtt));
        }
    }

    #[test]
    fn a_clean_path_keeps_the_asked_for_rate() {
        let mut rc = RateControl::new(20_000);
        let mut t = Instant::now();
        run(&mut rc, &mut t, 60, 0.3, 13 * MS);
        assert_eq!(rc.rate(), 20_000);
    }

    #[test]
    fn loss_backs_off_and_a_clean_path_climbs_back() {
        let mut rc = RateControl::new(20_000);
        let mut t = Instant::now();
        run(&mut rc, &mut t, 3, 0.0, 13 * MS);
        t += Duration::from_secs(1);
        assert_eq!(rc.report(t, 6.0, Some(13 * MS)), Some(17_000));
        t += Duration::from_secs(1);
        assert_eq!(rc.report(t, 15.0, Some(13 * MS)), Some(11_050));
        // Held, then two clean seconds before each step up.
        run(&mut rc, &mut t, 2, 0.0, 13 * MS);
        assert_eq!(rc.rate(), 11_050);
        run(&mut rc, &mut t, 40, 0.0, 13 * MS);
        assert_eq!(rc.rate(), 20_000);
    }

    #[test]
    fn a_growing_queue_backs_off_before_anything_is_lost() {
        let mut rc = RateControl::new(30_000);
        let mut t = Instant::now();
        run(&mut rc, &mut t, 5, 0.0, 150 * MS);
        // 150 ms base: queueing counts past 37.5 ms.
        t += Duration::from_secs(1);
        assert_eq!(rc.report(t, 0.0, Some(180 * MS)), None);
        t += Duration::from_secs(1);
        assert_eq!(rc.report(t, 0.0, Some(200 * MS)), Some(25_500));
    }

    #[test]
    fn the_rate_never_falls_below_the_floor() {
        let mut rc = RateControl::new(20_000);
        let mut t = Instant::now();
        run(&mut rc, &mut t, 30, 50.0, 13 * MS);
        assert_eq!(rc.rate(), 2_000);
        let mut small = RateControl::new(800);
        run(&mut small, &mut t, 30, 50.0, 13 * MS);
        assert_eq!(small.rate(), 500);
    }

    #[test]
    fn the_base_round_trip_forgets_an_old_path() {
        let mut rc = RateControl::new(20_000);
        let mut t = Instant::now();
        run(&mut rc, &mut t, 5, 0.0, 13 * MS);
        // The path moves to a relay 150 ms away: for ten seconds that
        // looks like queueing, then it is the new base.
        run(&mut rc, &mut t, 15, 0.0, 150 * MS);
        assert_eq!(rc.base_rtt(), Some(150 * MS));
        run(&mut rc, &mut t, 60, 0.0, 150 * MS);
        assert_eq!(rc.rate(), 20_000);
    }
}
