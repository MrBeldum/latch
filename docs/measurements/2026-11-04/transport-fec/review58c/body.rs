// Review harness for PR #58 re-review (shared by main and head via shims).
// Copied from the 58b review; probe trains (sent, metered and reported)
// go through shim functions, which are no-ops for revisions without them.
use latch_proto::{
    control::Report,
    sim::{Impairment, Link},
    video::{OutFrame, Packetizer, Reassembled, Reassembler},
    wire::Datagram,
};
use std::{
    collections::{HashMap, VecDeque},
    time::{Duration, Instant},
};

#[derive(Clone, Copy, PartialEq)]
pub enum Content {
    /// Full-rate motion throughout.
    Motion,
    /// 1% frames until `at`, then motion.
    IdleThenMotion { at: u64 },
    /// Desktop: `idle` ms of 1% frames, then `motion` ms of full frames, repeating.
    Desktop { idle: u64, motion: u64 },
    /// Sparse: frames of at most `kbps` worth; after `idle_until` (1% before).
    Sparse { kbps: u32, idle_until: u64 },
}

pub struct Scenario {
    pub name: &'static str,
    pub fps: u64,
    pub ceiling: u32,
    /// (from_ms, kbps) steps; first entry from 0.
    pub caps: &'static [(u64, u32)],
    pub fifo_ms: u64,
    pub rtt: u64,
    pub interval: u64,
    pub loss: f64,
    pub burst_enter: f64,
    pub jitter_ms: u64,
    pub legacy: bool,
    pub content: Content,
    /// (from, to, extra_ms) added to pings (and viewer RTT) in this span.
    pub spike: Option<(u64, u64, f32)>,
    /// Force a keyframe of `mult` x frame size (or fixed bytes if mult==0) at these ms.
    pub idr_at: &'static [(u64, usize)],
    pub secs: u64,
    pub seed: u64,
}

pub struct Outcome {
    pub lost: u64,
    pub skipped: u64,
    pub queue_peak: f32,
    pub queue_p95: f32,
    pub mean_enc: f64,
    pub timeline: Vec<(u64, u32, u32, u32)>,
    pub full_rate_at: Option<u64>,
    pub idr_latency: Vec<(u64, usize, u64)>,
    pub motion_enc_mean: f64,
    pub final_budget: u32,
}

pub fn run(s: &Scenario) -> Outcome {
    let start = Instant::now();
    let impairment = Impairment {
        loss: s.loss,
        burst_enter: s.burst_enter,
        burst_exit: 0.3,
        duplicate: 0.0,
        reorder: 0.0,
        reorder_delay: Duration::from_millis(4),
        jitter: Duration::from_millis(s.jitter_ms),
        delay: Duration::from_millis(s.rtt / 2),
    };
    let mut link = Link::new(impairment, s.seed);
    let mut pacer = pacing::Pacer::new(start);
    let mut c = congestion::Controller::new(s.ceiling, start);
    let mut tx = Packetizer::new(1);
    let mut rx = Reassembler::new(1);
    let mut fec = shim::Fec::default();
    let mut pending = VecDeque::<(Vec<u8>, u32)>::new();
    let mut frames_pending = 0usize;
    let mut enqueued: HashMap<u32, (u64, usize, bool)> = HashMap::new();
    let mut last_dg_of: HashMap<u32, usize> = HashMap::new();
    let mut feedback = VecDeque::new();
    let mut previous = rx.stats();
    let (mut sent, mut wire_received) = (0usize, 0usize);
    let mut ping_low = f32::MAX;
    let mut local_queue = 0.0f32;
    let mut next_frame = 0u64;
    let mut idr_due = Some(start);
    let mut last_idr = start - Duration::from_secs(1);
    let mut queue_samples = Vec::new();
    let mut queue_peak = 0.0f32;
    let mut enc_sum = 0f64;
    let mut enc_n = 0f64;
    let mut motion_sum = 0f64;
    let mut motion_n = 0f64;
    let mut timeline = Vec::new();
    let mut full_rate_at = None;
    let mut idr_latency = Vec::new();
    let mut forced: Vec<(u64, usize)> = s.idr_at.to_vec();
    let mut sent_dg_count: HashMap<u32, usize> = HashMap::new();
    let mut meter = shim::Meter::default();
    let mut train = 0u16;
    let mut echoes = VecDeque::new();
    for ms in 0..s.secs * 1000 {
        let now = start + Duration::from_millis(ms);
        let cap = s.caps.iter().rev().find(|(from, _)| *from <= ms).unwrap().1;
        link.set_bandwidth(cap, Duration::from_millis(s.fifo_ms));
        let motion = match s.content {
            Content::Motion => true,
            Content::IdleThenMotion { at } => ms >= at,
            Content::Desktop { idle, motion } => ms % (idle + motion) >= idle,
            Content::Sparse { .. } => false,
        };
        if ms * s.fps >= next_frame * 1000 && frames_pending < 3 {
            let mut keyframe =
                idr_due.is_some_and(|at| at <= now) && now - last_idr >= Duration::from_millis(s.rtt);
            let full = (u64::from(shim::video_budget(
                shim::enc_budget(&c),
                tx.fec_percent(),
                shim::burst(&tx),
                s.fps as u32,
                keyframe,
            )) * 1000
                / 8
                / s.fps) as usize;
            let mut size = match s.content {
                Content::Sparse { kbps, idle_until } => {
                    if ms < idle_until {
                        full / 100
                    } else {
                        full.min((u64::from(kbps) * 1000 / 8 / s.fps) as usize)
                    }
                }
                _ => {
                    if motion {
                        full
                    } else {
                        full / 100
                    }
                }
            }
            .max(1);
            let mut fixed = None;
            if let Some(pos) = forced.iter().position(|(at, _)| *at <= ms) {
                let (_, bytes) = forced.remove(pos);
                keyframe = true;
                fixed = Some(bytes);
            }
            if let Some(bytes) = fixed {
                size = bytes;
            }
            if keyframe {
                idr_due = None;
                last_idr = now;
            }
            let data = vec![0x5a; size];
            let number = tx.next_frame();
            let mut count = 0;
            tx.packetize(
                &OutFrame {
                    data: &data,
                    keyframe,
                    host_delay: 0,
                },
                |dg| {
                    pending.push_back((dg.to_vec(), number));
                    count += 1;
                },
            );
            last_dg_of.insert(number, count);
            enqueued.insert(number, (ms, size, fixed.is_some()));
            frames_pending += 1;
            next_frame = ms * s.fps / 1000 + 1;
        }
        if let Some(count) = shim::probe(&mut c, now) {
            train = train.wrapping_add(1);
            for dg in shim::train(train, count) {
                link.send(now, &dg);
            }
            if shim::TRAIN_ECHO {
                let mut dg = Vec::new();
                latch_proto::wire::write_ping(&mut dg, 1, ms * 1000);
                link.send(now, &dg);
            }
        }
        while let Some((dg, number)) = pending.front() {
            let number = *number;
            if !pacer.delay(now, dg.len(), shim::pace(&c)).is_zero() {
                // Waiting in the pacer is local queue (server's queue_us).
                if let Some((at, ..)) = enqueued.get(&number) {
                    local_queue = local_queue.max((ms - at) as f32);
                }
                break;
            }
            let (dg, _) = pending.pop_front().unwrap();
            let n = sent_dg_count.entry(number).or_insert(0);
            *n += 1;
            if *n == last_dg_of[&number] {
                frames_pending -= 1;
            }
            if let Some((at, ..)) = enqueued.get(&number) {
                local_queue = local_queue.max((ms - at) as f32);
            }
            sent += dg.len() + 80;
            link.send(now, &dg);
        }
        let queue = link.queue_delay(now).as_secs_f32() * 1000.0;
        queue_peak = queue_peak.max(queue);
        queue_samples.push(queue);
        let extra = s
            .spike
            .filter(|(from, to, _)| (*from..*to).contains(&ms))
            .map_or(0.0, |(_, _, e)| e);
        if ms % 20 == 0 {
            ping_low = ping_low.min(s.rtt as f32 + queue + extra);
        }
        for dg in link.receive(now) {
            if let Ok(Datagram::Ping { time, .. }) = latch_proto::wire::parse(&dg) {
                echoes.push_back((now + Duration::from_millis(s.rtt / 2), time));
                continue;
            }
            if shim::metered(&mut meter, &dg, now) {
                continue;
            }
            wire_received += dg.len() + 80;
            if let Ok(Datagram::Video { shard, .. }) = latch_proto::wire::parse(&dg) {
                rx.push(&shard, now);
            }
            while let Some(event) = rx.poll(now) {
                match event {
                    Reassembled::Frame(ref f) => {
                        if let Some((at, size, forced)) = enqueued.get(&f.frame) {
                            if *forced {
                                idr_latency.push((*at, *size, ms - at));
                            }
                        }
                        let full_ceiling = u64::from(s.ceiling) * 1000 / 8 / s.fps * 7 / 10;
                        let motion_start = match s.content {
                            Content::IdleThenMotion { at } => Some(at),
                            _ => None,
                        };
                        if let Some(at) = motion_start {
                            if ms >= at && f.data.len() as u64 > full_ceiling.min(u64::from(cap) * 1000 / 8 / s.fps * 7 / 10) {
                                full_rate_at.get_or_insert(ms - at);
                            }
                        }
                    }
                    Reassembled::Lost { .. } => {
                        if idr_due.is_none() {
                            idr_due = Some(now + Duration::from_millis(s.rtt));
                        }
                    }
                }
            }
        }
        if rx.waiting_for_keyframe() && idr_due.is_none() {
            idr_due = Some(now + Duration::from_millis(s.rtt));
        }
        if ms > 0 && ms % s.interval == 0 {
            let stats = rx.stats();
            let recovered = stats.shards_recovered - previous.shards_recovered;
            let missing = stats.shards_missing - previous.shards_missing;
            let received = stats.shards_received - previous.shards_received;
            let denom = (received + missing).max(1) as f32;
            let mut report = Report {
                rtt_ms: s.rtt as f32 + queue + extra,
                received_kbps: (wire_received * 8 / s.interval as usize) as u32,
                frames_lost: stats.frames_lost - previous.frames_lost,
                loss_pct: (missing + recovered) as f32 * 100.0 / denom,
                unrecovered_loss_pct: if s.legacy {
                    None
                } else {
                    Some(missing as f32 * 100.0 / denom)
                },
                window_ms: s.interval as u32,
                ..Default::default()
            };
            if s.name.starts_with("v50") {
                // 5.0.x Report: fps, raw loss_pct, decode_ms, rtt_ms only.
                report.window_ms = 0;
                report.received_kbps = 0;
                report.frames_lost = 0;
            } else {
                shim::set_burst(&mut report, &mut rx);
                shim::set_probe(&mut report, &mut meter, now);
            }
            feedback.push_back((
                now + Duration::from_millis(s.rtt / 2),
                report,
                ping_low,
                (sent * 8 / s.interval as usize) as u32,
                local_queue,
            ));
            ping_low = f32::MAX;
            local_queue = 0.0;
            sent = 0;
            wire_received = 0;
            previous = stats;
        }
        while echoes.front().is_some_and(|(at, _)| *at <= now) {
            let (_, sent_us) = echoes.pop_front().unwrap();
            shim::echo(&mut c, (ms * 1000 - sent_us) as f32 / 1000.0, now);
        }
        while feedback.front().is_some_and(|(at, ..)| *at <= now) {
            let (_, report, rtt, sent, lq) = feedback.pop_front().unwrap();
            c.observe_sent(sent);
            c.update(now, &report, rtt, lq);
            let (p, b) = fec.update(&report, &c, Duration::from_millis(s.interval));
            tx.set_fec_percent(p);
            shim::set_tx_burst(&mut tx, b);
            if ms % 100 < s.interval {
                timeline.push((ms, c.budget(), shim::enc_budget(&c), sent));
            }
        }
        let e = f64::from(shim::enc_budget(&c));
        if ms >= 5000 {
            enc_sum += e;
            enc_n += 1.0;
        }
        if motion && ms >= 5000 {
            motion_sum += e;
            motion_n += 1.0;
        }
    }
    let stats = rx.stats();
    queue_samples.sort_by(f32::total_cmp);
    Outcome {
        lost: stats.frames_lost,
        skipped: stats.frames_skipped,
        queue_peak,
        queue_p95: queue_samples[queue_samples.len() * 95 / 100],
        mean_enc: enc_sum / enc_n.max(1.0),
        timeline,
        full_rate_at,
        idr_latency,
        motion_enc_mean: motion_sum / motion_n.max(1.0),
        final_budget: shim::enc_budget(&c),
    }
}

pub fn report(s: &Scenario, o: &Outcome, verbose: bool) {
    println!(
        "R58B {} lost={} skipped={} queue_peak={:.1} queue_p95={:.1} mean_enc={:.0} motion_enc_mean={:.0} full_rate_at={:?} final={} idr={:?}",
        s.name, o.lost, o.skipped, o.queue_peak, o.queue_p95, o.mean_enc, o.motion_enc_mean, o.full_rate_at, o.final_budget, o.idr_latency
    );
    if verbose {
        for (ms, b, e, sent) in &o.timeline {
            println!("  T {} {ms} budget={b} enc={e} sent={sent}", s.name);
        }
    }
}
