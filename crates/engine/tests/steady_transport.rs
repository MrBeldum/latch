//! 35 s codec-free frame/FEC/FIFO runs behind a fixed cap: budget spread,
//! standing queue and frame damage over the last 30 s, with the production
//! pacer. Run --nocapture for the table in docs/measurements/2026-11-03/transport.
#[path = "../src/congestion.rs"]
mod congestion;
#[path = "../src/pacing.rs"]
mod pacing;
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
    for (name, cap, ceiling, base, interval, back, legacy) in [
        ("steady20", 20000, 100000, 10, 50, 10, false),
        ("steady50", 50000, 100000, 10, 50, 10, false),
        ("steady100", 100000, 200000, 10, 50, 10, false),
        ("cap20rtt150", 20000, 100000, 150, 50, 150, false),
        ("cap20rtt250", 20000, 100000, 250, 50, 250, false),
        ("cap100rtt150", 100000, 200000, 150, 50, 150, false),
        ("cap100rtt250", 100000, 200000, 250, 50, 250, false),
        ("rtt150", 50000, 100000, 150, 50, 150, false),
        ("rtt250", 50000, 100000, 250, 50, 250, false),
        ("slow250", 50000, 100000, 10, 250, 125, false),
        ("legacy1000", 50000, 100000, 10, 1000, 10, true),
        ("legacy_radio", 100000, 100000, 10, 1000, 10, true),
        ("cap2", 2000, 100000, 10, 50, 10, false),
        ("cap5", 5000, 100000, 10, 50, 10, false),
        ("route", 100000, 100000, 10, 50, 10, false),
        ("lan1g", 1000000, 1000000, 1, 50, 1, false),
        ("loss1cap20", 20000, 20000, 10, 50, 10, false),
        ("loss3cap20", 20000, 20000, 10, 50, 10, false),
        ("loss5cap20", 20000, 20000, 10, 50, 10, false),
        ("loss1cap50", 50000, 50000, 10, 50, 10, false),
        ("loss3cap50", 50000, 50000, 10, 50, 10, false),
        ("loss5cap50", 50000, 50000, 10, 50, 10, false),
        ("loss1cap100", 100000, 100000, 10, 50, 10, false),
        ("loss3cap100", 100000, 100000, 10, 50, 10, false),
        ("loss5cap100", 100000, 100000, 10, 50, 10, false),
        ("idle_motion", 50000, 50000, 10, 50, 10, false),
    ] {
        let fps = 120;
        let random = name.starts_with("loss");
        let impairment = Impairment {
            loss: if random {
                f64::from(name.as_bytes()[4] - b'0') / 100.0
            } else if name == "legacy_radio" {
                0.05
            } else {
                0.0
            },
            burst_enter: if random { 0.002 } else { 0.0 },
            burst_exit: 0.3,
            duplicate: if random { 0.01 } else { 0.0 },
            reorder: if random { 0.01 } else { 0.0 },
            reorder_delay: Duration::from_millis(4),
            jitter: Duration::from_millis(if random { 4 } else { 0 }),
            delay: Duration::from_millis(base / 2),
        };
        let start = Instant::now();
        let mut pending = VecDeque::<Vec<u8>>::new();
        let mut pacer = pacing::Pacer::new(start);
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
        let mut minimum = ceiling;
        let mut fec_loss = 0.0;
        let mut fec_burst: f32 = 0.0;
        let mut wire_bytes = 0u64;
        let mut parity_bytes = 0u64;
        let mut idrs = 0;
        let mut full_rate_at = None;
        let mut budgets = Vec::new();
        let mut route_recovered = false;
        for ms in 0..35_000u64 {
            let now = start + Duration::from_millis(ms);
            let capacity = cap;
            link.set_bandwidth(capacity, Duration::from_millis(40));
            if ms * fps >= next_frame * 1000 {
                let size = (u64::from(congestion::video_budget(
                    controller.budget(),
                    tx.fec_percent(),
                    fec_burst.round() as u8,
                    fps as u32,
                    false,
                )) * 1000
                    / 8
                    / fps) as usize;
                let size = if name == "idle_motion" && ms < 5000 {
                    size / 100
                } else {
                    size
                };
                let data = vec![0x5a; size];
                let keyframe = idr_due.is_some_and(|at| at <= now)
                    && now - last_idr >= Duration::from_millis(base);
                if keyframe {
                    idr_due = None;
                    last_idr = now;
                    idrs += 1;
                }
                tx.packetize(
                    &OutFrame {
                        data: &data,
                        keyframe,
                        host_delay: 0,
                    },
                    |dg| {
                        wire_bytes += (dg.len() + 80) as u64;
                        if let Ok(Datagram::Video { shard, .. }) = latch_proto::wire::parse(dg) {
                            if shard.shard >= shard.data {
                                parity_bytes += (dg.len() + 80) as u64;
                            }
                        }
                        pending.push_back(dg.to_vec());
                    },
                );
                next_frame += 1;
            }
            while let Some(dg) = pending.front() {
                let pace = if controller.pacing() {
                    controller.budget()
                } else {
                    0
                };
                if !pacer.delay(now, dg.len(), pace).is_zero() {
                    break;
                }
                let dg = pending.pop_front().unwrap();
                sent += dg.len() + 80;
                link.send(now, &dg);
            }
            let queue = link.queue_delay(now).as_secs_f32() * 1000.0;
            if ms >= 5000 {
                queue_samples.push(queue);
                budgets.push(controller.budget() as f64);
            }
            if ms % 20 == 0 {
                ping_low = ping_low.min(
                    (if name == "route" && ms >= 5000 {
                        100.0
                    } else {
                        base as f32
                    }) + queue,
                );
            }
            for dg in link.receive(now) {
                wire_received += dg.len() + 80;
                if let Ok(Datagram::Video { shard, .. }) = latch_proto::wire::parse(&dg) {
                    rx.push(&shard, now);
                }
                while let Some(event) = rx.poll(now) {
                    if let Reassembled::Frame(ref frame) = event {
                        if name == "idle_motion" && ms >= 5000 && frame.data.len() > 30000 {
                            full_rate_at.get_or_insert(ms - 5000);
                        }
                    }
                    if matches!(event, Reassembled::Lost { .. }) && idr_due.is_none() {
                        idr_due = Some(now + Duration::from_millis(base));
                    }
                }
            }
            while rx.poll(now).is_some() {}
            if rx.waiting_for_keyframe() && idr_due.is_none() {
                idr_due = Some(now + Duration::from_millis(base));
            }
            if ms > 0 && ms % interval == 0 {
                let stats = rx.stats();
                let recovered = stats.shards_recovered - previous.shards_recovered;
                let missing = stats.shards_missing - previous.shards_missing;
                let received = stats.shards_received - previous.shards_received;
                let denom = (received + missing).max(1) as f32;
                let report = Report {
                    rtt_ms: (if name == "route" && ms >= 5000 {
                        100.0
                    } else {
                        base as f32
                    }) + queue,
                    received_kbps: (wire_received * 8 / interval as usize) as u32,
                    frames_lost: stats.frames_lost - previous.frames_lost,
                    loss_pct: (missing + recovered) as f32 * 100.0 / denom,
                    unrecovered_loss_pct: if legacy {
                        None
                    } else {
                        Some(missing as f32 * 100.0 / denom)
                    },
                    loss_burst_packets: rx.take_loss_burst(),
                    window_ms: interval as u32,
                    ..Default::default()
                };
                feedback.push_back((
                    now + Duration::from_millis(back),
                    report,
                    ping_low,
                    (sent * 8 / interval as usize) as u32,
                ));
                ping_low = f32::MAX;
                sent = 0;
                wire_received = 0;
                previous = stats;
            }
            while feedback.front().is_some_and(|(at, ..)| *at <= now) {
                let (_, report, rtt, sent) = feedback.pop_front().unwrap();
                controller.observe_sent(sent);
                controller.update(now, &report, rtt, 0.0);
                fec_loss = latch_proto::video::fec_loss_sample(
                    fec_loss,
                    report.loss_pct,
                    Duration::from_millis(interval),
                );
                fec_burst = latch_proto::video::fec_loss_sample(
                    fec_burst,
                    if controller.pacing()
                        || report.unrecovered_loss_pct.is_some_and(|loss| loss > 10.0)
                    {
                        0.0
                    } else {
                        f32::from(report.loss_burst_packets)
                    },
                    Duration::from_millis(interval),
                );
                tx.set_fec_burst(fec_burst.round() as u8);
                tx.set_fec_percent(latch_proto::video::fec_percent_for(
                    fec_loss,
                    controller.base_rtt(),
                ));
            }
            minimum = minimum.min(controller.budget());
            if name == "route" && ms >= 15000 && controller.budget() == ceiling {
                route_recovered = true;
            }
        }
        queue_samples.sort_by(f32::total_cmp);
        let p95 = queue_samples[queue_samples.len() * 95 / 100];
        let mean = budgets.iter().sum::<f64>() / budgets.len() as f64;
        let sd =
            (budgets.iter().map(|v| (v - mean).powi(2)).sum::<f64>() / budgets.len() as f64).sqrt();
        let stats = rx.stats();
        println!(
            "STEADY {name} mean={mean:.1} sd={sd:.1} queue_p95={p95:.3} lost={} skipped={} min={minimum} final={}",
            stats.frames_lost,
            stats.frames_skipped,
            controller.budget()
        );
        println!("OVERHEAD {name} parity_pct={:.2} parity_mbps={:.3} wire_mbps={:.3} lost_per_min={:.1} idrs_per_min={:.1} motion_ms={}",
            parity_bytes as f64 * 100.0 / wire_bytes as f64,
            parity_bytes as f64 * 8.0 / 35_000_000.0,
            wire_bytes as f64 * 8.0 / 35_000_000.0,
            stats.frames_lost as f64 * 60.0 / 35.0, idrs as f64 * 60.0 / 35.0,
            full_rate_at.map_or(-1, |ms| ms as i64));
        if random {
            assert!(
                minimum >= cap * 4 / 5,
                "{name}: random loss collapsed budget: {minimum}"
            );
        }
        if name == "idle_motion" {
            assert_eq!(minimum, ceiling, "idle encoder must retain the budget");
            assert!(
                full_rate_at.is_some_and(|ms| ms < 50),
                "motion must use the budget immediately"
            );
        }
        // Main's controller spreads 1.7/3.5/7.1 Mbps at these caps (10 ms,
        // 50 ms reports); stay at or under it at every RTT and at 5.1's cadence.
        if interval <= 250
            && !legacy
            && name != "route"
            && name != "lan1g"
            && cap >= 20000
            && !random
            && name != "idle_motion"
        {
            let sd_bound = match cap {
                20000 => 1700.0,
                50000 => 3500.0,
                _ => 7100.0,
            };
            assert!(sd <= sd_bound, "{name}: budget SD {sd} > {sd_bound}");
            assert!(p95 < 16.0, "{name}: queue p95 {p95}");
            assert!(stats.frames_lost < 120, "{name}: frame damage");
            assert!(stats.frames_skipped < 60, "{name}: skipped frames");
            assert!(mean > f64::from(cap) * 0.85, "{name}: underused path");
        }
        if name == "legacy1000" {
            assert!(p95 < 30.0 && mean > 40_000.0, "{name}: {p95} {mean}");
        }
        // A 100 Mbps request must still adapt to a 2-5 Mbps link.
        if name == "cap2" || name == "cap5" {
            assert!(
                stats.frames_lost < 50 && stats.frames_skipped < 10,
                "{name}"
            );
            assert!(controller.budget() < cap);
        }
        if name == "route" {
            assert!(route_recovered);
            assert_eq!(controller.budget(), ceiling);
        }
        if name == "legacy_radio" {
            assert_eq!(controller.budget(), ceiling);
        }
    }
}
