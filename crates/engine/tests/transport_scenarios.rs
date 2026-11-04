//! Codec-free, deterministic frame/FEC/FIFO feedback scenarios. Run --nocapture
//! for measurements; all time is simulated, including delayed feedback and IDR.
#[path = "../src/congestion.rs"]
mod congestion;
use latch_proto::{
    control::Report,
    sim::{Impairment, Link},
    video::{OutFrame, Packetizer, Reassembled, Reassembler},
    wire::Datagram,
};
use std::{
    collections::VecDeque,
    time::{Duration, Instant},
};

#[test]
fn transport_scenarios() {
    for name in [
        "step",
        "240fps",
        "random2",
        "wifi",
        "rtt100",
        "jitter",
        "wifi_jitter",
        "policer5",
        "policer10",
        "policer20",
    ] {
        // A token bucket two frames deep drops traffic above its rate and
        // never queues it, so only loss, not delay, reveals the overshoot.
        // Its rate is the ceiling's wire rate after the first IDR, less
        // the overshoot.
        let police = name
            .strip_prefix("policer")
            .map(|over| 100.0 / (100.0 + over.parse::<f64>().unwrap()));
        let mut police_rate = None;
        let mut early_wire = 0usize;
        let base = if name == "rtt100" { 100 } else { 10 };
        let fps = if name == "240fps" { 240 } else { 120 };
        let ceiling = if name == "240fps" { 200_000 } else { 100_000 };
        let impairment = Impairment {
            loss: if name == "random2" { 0.02 } else { 0.0 },
            burst_enter: if name == "wifi" { 0.002 } else { 0.0 },
            burst_exit: 0.25,
            delay: Duration::from_millis(base / 2),
            jitter: Duration::from_millis(match name {
                "jitter" => 6,
                "wifi_jitter" => 10,
                _ => 0,
            }),
            ..Impairment::NONE
        };
        let start = Instant::now();
        let mut controller = congestion::Controller::new(ceiling, start);
        let mut link = Link::new(impairment, 17);
        let mut tx = Packetizer::new(1);
        let mut rx = Reassembler::new(1);
        let mut next_frame = 0;
        let mut feedback = VecDeque::new();
        let mut previous = rx.stats();
        let mut sent = 0;
        let mut wire_received = 0;
        let mut idr_due = Some(start);
        let mut last_idr = start - Duration::from_secs(1);
        let mut queue_samples = Vec::new();
        let mut ping_low = f32::MAX;
        let mut ceiling_at = None;
        let mut cut_at = None;
        let mut peak = 0;
        let mut minimum = ceiling;
        let mut fec_loss = 0.0;
        let mut tokens = 0.0f64;
        let mut seed = 0x9e37_79b9_u32;
        let mut damaged_windows = 0;
        let mut lost_windows = 0;
        let mut raw_when_lost = Vec::new();
        let mut low_phase_cuts = 0;
        for ms in 0..12_000u64 {
            let now = start + Duration::from_millis(ms);
            let capacity = if name == "step" && (2000..8000).contains(&ms) {
                20_000
            } else {
                ceiling
            };
            if police.is_none() && name != "wifi_jitter" {
                link.set_bandwidth(capacity, Duration::from_millis(40));
            }
            if ms == 150 {
                police_rate = police.map(|share| early_wire as f64 / 100.0 * share);
            }
            // Bytes per ms; the bucket holds two frames at the policed rate.
            if let Some(rate) = police_rate {
                tokens = (tokens + rate).min(rate * 2000.0 / fps as f64);
            }
            if ms * fps >= next_frame * 1000 {
                let size = (u64::from(congestion::video_budget(
                    controller.budget(),
                    tx.fec_percent(),
                    false,
                )) * 1000
                    / 8
                    / fps) as usize;
                let data = vec![0x5a; size];
                let keyframe = idr_due.is_some_and(|at| at <= now)
                    && now - last_idr >= Duration::from_millis(base);
                if keyframe {
                    idr_due = None;
                    last_idr = now;
                }
                tx.packetize(
                    &OutFrame {
                        data: &data,
                        keyframe,
                        host_delay: 0,
                    },
                    |dg| {
                        sent += dg.len() + 80;
                        if ms >= 50 {
                            early_wire += dg.len() + 80;
                        }
                        if police_rate.is_none() {
                            link.send(now, dg);
                        } else if tokens >= (dg.len() + 80) as f64 {
                            tokens -= (dg.len() + 80) as f64;
                            link.send(now, dg);
                        }
                    },
                );
                next_frame += 1;
            }
            let queue = link.queue_delay(now).as_secs_f32() * 1000.0;
            queue_samples.push(queue);
            if ms % 20 == 0 {
                // Wi-Fi scheduling delays each ping 5..15 ms on top of the
                // packets' own 0..10 ms jitter; no serialization queue builds.
                seed = seed.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
                let jitter = if name == "wifi_jitter" {
                    5.0 + (seed >> 8) as f32 / (1u32 << 24) as f32 * 10.0
                } else {
                    0.0
                };
                ping_low = ping_low.min(base as f32 + queue + jitter);
            }
            for dg in link.receive(now) {
                wire_received += dg.len() + 80;
                if let Ok(Datagram::Video { shard, .. }) = latch_proto::wire::parse(&dg) {
                    rx.push(&shard, now);
                }
                while let Some(event) = rx.poll(now) {
                    if matches!(event, Reassembled::Lost { .. }) && idr_due.is_none() {
                        idr_due = Some(now + Duration::from_millis(base));
                    }
                }
            }
            while rx.poll(now).is_some() {}
            if rx.waiting_for_keyframe() && idr_due.is_none() {
                idr_due = Some(now + Duration::from_millis(base));
            }
            if ms > 0 && ms % 50 == 0 {
                let stats = rx.stats();
                let recovered = stats.shards_recovered - previous.shards_recovered;
                let missing = stats.shards_missing - previous.shards_missing;
                let received = stats.shards_received - previous.shards_received;
                let denom = (received + missing).max(1) as f32;
                let report = Report {
                    rtt_ms: base as f32 + queue,
                    received_kbps: (wire_received * 8 / 50) as u32,
                    frames_lost: stats.frames_lost - previous.frames_lost,
                    loss_pct: (missing + recovered) as f32 * 100.0 / denom,
                    unrecovered_loss_pct: Some(missing as f32 * 100.0 / denom),
                    window_ms: 50,
                    ..Default::default()
                };
                if report.unrecovered_loss_pct.unwrap() > 3.0 && report.loss_pct <= 10.0 {
                    damaged_windows += 1;
                }
                if report.frames_lost > 0 {
                    lost_windows += 1;
                    raw_when_lost.push(report.loss_pct);
                }
                feedback.push_back((
                    now + Duration::from_millis(base),
                    report,
                    ping_low,
                    (sent * 8 / 50) as u32,
                ));
                ping_low = f32::MAX;
                sent = 0;
                wire_received = 0;
                previous = stats;
            }
            while feedback.front().is_some_and(|(at, ..)| *at <= now) {
                let (_, report, rtt, sent) = feedback.pop_front().unwrap();
                controller.observe_sent(sent);
                let before = controller.budget();
                controller.update(now, &report, rtt, 0.0);
                if name == "step" && (2500..8000).contains(&ms) && controller.budget() < before {
                    low_phase_cuts += 1;
                }
                fec_loss = latch_proto::video::fec_loss_sample(
                    fec_loss,
                    report.loss_pct,
                    Duration::from_millis(50),
                );
                tx.set_fec_percent(
                    latch_proto::video::fec_percent_for(fec_loss, controller.base_rtt()).min(30),
                );
            }
            if ms >= 8000 && controller.budget() >= ceiling * 95 / 100 {
                ceiling_at.get_or_insert(ms - 8000);
            }
            if name == "step" && ms >= 2000 && controller.budget() <= 20_000 {
                cut_at.get_or_insert(ms - 2000);
            }
            peak = peak.max(controller.budget());
            minimum = minimum.min(controller.budget());
        }
        queue_samples.sort_by(f32::total_cmp);
        let p95 = queue_samples[queue_samples.len() * 95 / 100];
        println!("{name}: low_phase_cuts={low_phase_cuts} ceiling_ms={} cut_ms={} queue_p95_ms={p95:.2} lost={} skipped={} peak_kbps={peak} min_kbps={minimum} low_raw_damage_windows={damaged_windows} lost_windows={lost_windows} raw_loss_median_when_lost={:.1}", ceiling_at.map_or(-1, |v| v as i64), cut_at.map_or(-1, |v| v as i64), rx.stats().frames_lost, rx.stats().frames_skipped, {
            raw_when_lost.sort_by(f32::total_cmp);
            raw_when_lost.get(raw_when_lost.len() / 2).copied().unwrap_or(0.0)
        });
        assert!(p95 < 16.0, "{name}: standing queue {p95}");
        if name == "step" {
            assert!(cut_at.is_some_and(|ms| ms <= 60));
            assert!(ceiling_at.is_some_and(|ms| ms <= 2000));
            assert!(rx.stats().frames_lost <= 40);
        }
        if name == "wifi" {
            assert!(rx.stats().frames_lost < 25);
        }
        if let Some(share) = police {
            // 5% over mostly costs the tail parity, which the viewer never
            // misses; larger overshoots lose frames until the rate drops.
            if name != "policer5" {
                assert!(lost_windows > 3, "{name}: must exercise frame damage");
                assert!(
                    f64::from(minimum) < f64::from(ceiling) * share,
                    "{name}: must cut without queue growth"
                );
            }
            assert!(
                rx.stats().frames_lost < 30,
                "{name}: must stop repeated frame damage"
            );
        }
        if name == "wifi_jitter" {
            assert_eq!(minimum, ceiling, "5–15 ms radio jitter must not cut rate");
        }
        if name == "random2" {
            assert_eq!(rx.stats().frames_lost, 0);
            assert_eq!(minimum, ceiling, "radio loss must not cut rate");
        }
    }
}

#[test]
fn policer_damage_does_not_need_high_raw_loss() {
    for (raw, unrecovered, frames) in [(5.0, 4.0, 0), (8.0, 4.0, 0), (15.0, 4.0, 0), (2.0, 0.0, 1)]
    {
        let start = Instant::now();
        let mut c = congestion::Controller::new(100_000, start);
        let report = Report {
            loss_pct: raw,
            unrecovered_loss_pct: Some(unrecovered),
            frames_lost: frames,
            rtt_ms: 10.0,
            ..Default::default()
        };
        c.update(start, &report, 10.0, 0.0);
        assert_eq!(c.budget(), 100_000, "one damaged window is a blip");
        c.update(start + Duration::from_millis(50), &report, 10.0, 0.0);
        assert!(
            c.budget() < 100_000,
            "sustained damage must cut at raw loss {raw}"
        );
    }
}
