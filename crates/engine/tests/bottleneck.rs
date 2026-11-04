//! Congestion feedback through an encoded noise-pattern desktop, UDP relay and viewer.
//! A sustained downstream queue-delay step exercises adaptation even when a
//! slow encoder underfills its budget. A fake-clock saturated FIFO separately
//! checks convergence at 10 Mbps; an encoder budget is not measured traffic.

// Exercise the production controller without widening its public API.
#[allow(dead_code)]
#[path = "../src/congestion.rs"]
mod congestion;
#[path = "support/relay.rs"]
mod relay;

use latch_core::http::{self, Response};
use latch_engine::server::{Options, Server};
use latch_proto::control::{self, StreamRequest};
use latch_proto::sim::{Impairment, Link};
use latch_stream::{Event, FrameSlot, Session, Settings};
use std::net::{IpAddr, Ipv4Addr, TcpListener, UdpSocket};
use std::sync::{mpsc, Arc};
use std::time::{Duration, Instant};

fn host() -> u16 {
    for _ in 0..50 {
        // The OS's next free UDP port, then TCP on the same number. Taking
        // TCP's next port first failed 50 times running on Windows CI: its
        // TCP ports come in sequence and can sit inside a block reserved
        // (by Hyper-V) against UDP.
        let port = UdpSocket::bind((Ipv4Addr::UNSPECIFIED, 0))
            .and_then(|u| u.local_addr())
            .unwrap()
            .port();
        let Ok(listener) = TcpListener::bind((Ipv4Addr::LOCALHOST, port)) else {
            continue;
        };
        let Ok(server) = Server::bind(port, Options::default()) else {
            continue;
        };
        std::thread::spawn(move || {
            http::serve_with_peer_check(
                listener,
                |_| true,
                move |peer, req| {
                    let request: StreamRequest = req.json().unwrap();
                    match server.prepare(request, peer) {
                        Ok(prepared) => {
                            let server = server.clone();
                            Response::switching(control::UPGRADE, move |tcp| {
                                server.run(prepared, tcp)
                            })
                        }
                        Err(r) => {
                            Response::json(r.status, &serde_json::json!({ "error": r.message }))
                        }
                    }
                },
            )
        });
        return port;
    }
    panic!("no free port");
}

#[test]
fn noise_stream_adapts_to_sustained_queue_delay_and_recovers() {
    use std::sync::atomic::{AtomicU32, Ordering};
    std::env::set_var("LATCH_TEST_PATTERN", "noise");
    let queue_delay_ms = Arc::new(AtomicU32::new(5));
    let port = relay::relay(host(), Impairment::NONE, queue_delay_ms.clone());
    let frames = Arc::new(FrameSlot::default());
    let (tx, rx) = mpsc::channel();
    let session = Session::start(
        latch_stream::Server {
            address: IpAddr::V4(Ipv4Addr::LOCALHOST),
            port,
        },
        // The pattern has a small noise patch, not full-frame entropy. Use a
        // modest frame rate to deliberately underfill the requested bitrate;
        // the standing queue must still drive adaptation on a slow CI runner.
        Settings {
            color: Default::default(),
            cursor: false,
            gamepads: false,
            mic: false,
            width: 640,
            height: 360,
            fps: 12,
            bitrate_kbps: 50_000,
            hevc: false,
            av1: false,
            audio: false,
            name: "bottleneck test".into(),
        },
        frames.clone(),
        tx,
        || {},
    );
    let start = Instant::now();
    loop {
        match rx.recv_timeout(Duration::from_millis(100)) {
            Ok(Event::Connected) => break,
            Ok(Event::Failed { stage, message }) => panic!("{stage}: {message}"),
            _ => assert!(start.elapsed() < Duration::from_secs(15), "never connected"),
        }
    }
    // A 5 ms propagation delay keeps the unloaded RTT above integer-ms
    // rounding; introduce an additional standing 40 ms queue after learning it.
    // Count fresh decoded pictures, not repeated reads of one stats window.
    let deadline = Instant::now() + Duration::from_secs(30);
    let baseline = loop {
        std::thread::sleep(Duration::from_millis(300));
        let s = session.stats();
        if frames.seq() >= 10 && s.rtt_ms > 0 && s.applied_bitrate_kbps > 0 && s.application_limited
        {
            break s;
        }
        assert!(Instant::now() < deadline, "no baseline: {s:?}");
    };
    eprintln!("baseline: {baseline:?}");
    assert!(baseline.wire_budget_kbps >= 40_000, "idle budget collapsed");
    assert!(
        baseline.application_limited,
        "must exercise an underfilled encoder"
    );
    queue_delay_ms.store(45, Ordering::Relaxed);
    let mut clean = 0;
    let mut constrained = 0;
    let mut clean_from = frames.seq();
    let mut settled = false;
    let deadline = Instant::now() + Duration::from_secs(45);
    while Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(300));
        let s = session.stats();
        eprintln!(
            "queued: applied={} received={:.2} fps={:.1} loss={:.1} rtt={}",
            s.applied_bitrate_kbps, s.received_mbps, s.fps, s.loss_pct, s.rtt_ms
        );
        // At the floor rate pictures come unevenly, a few a second: one
        // 300 ms poll may see none, so the picture must keep coming over the
        // whole settled run, not in every poll. 12 fps over a ~250 ms stats
        // window is 1–3 frames: one frame reads as 3.8 fps.
        if s.applied_bitrate_kbps > 0
            && s.applied_bitrate_kbps < (baseline.applied_bitrate_kbps / 4).max(1_000)
            && s.rtt_ms >= baseline.rtt_ms + 20
            && s.fps >= 3.0
            && s.loss_pct < 3.0
        {
            if clean == 0 {
                clean_from = frames.seq();
            }
            clean += 1;
            constrained = s.applied_bitrate_kbps;
            if clean >= 5 && frames.seq() >= clean_from + 3 {
                settled = true;
                break;
            }
        } else {
            clean = 0;
        }
    }
    assert!(settled, "never settled: {:?}", session.stats());
    queue_delay_ms.store(5, Ordering::Relaxed);
    let deadline = Instant::now() + Duration::from_secs(40);
    let recovery_frame = frames.seq();
    let mut recovered = false;
    while Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(300));
        let s = session.stats();
        if s.applied_bitrate_kbps > constrained * 2
            && s.rtt_ms < baseline.rtt_ms + 20
            && frames.seq() > recovery_frame + 10
            && s.fps >= 3.0
            && s.loss_pct < 3.0
        {
            eprintln!("recovered: {s:?}");
            recovered = true;
            break;
        }
    }
    session.stop();
    assert!(recovered, "bitrate did not recover: {:?}", session.stats());
}

#[test]
fn saturated_ten_megabit_fifo_converges_and_recovers_with_fake_clock() {
    let start = Instant::now();
    let mut controller = congestion::Controller::new(50_000, start);
    let mut link = Link::new(Impairment::NONE, 11);
    let mut pressured = 0;
    let mut clean = 0;
    let mut settled = Vec::new();
    for tick in 0..256 {
        let limited = (8..128).contains(&tick);
        let now = start + Duration::from_millis(tick * 250);
        link.set_bandwidth(
            if limited { 10_000 } else { 50_000 },
            Duration::from_millis(40),
        );
        // Generate the full wire budget, including Link's 80 byte overhead,
        // without an encoder or scheduler limiting the offered load.
        let packet = [0u8; 1200];
        let spacing =
            Duration::from_secs_f64(1280.0 * 8.0 / (f64::from(controller.budget()) * 1000.0));
        let before = link.counts();
        let mut at = now;
        while at < now + Duration::from_millis(250) {
            link.send(at, &packet);
            link.receive(at);
            at += spacing;
        }
        let after = link.counts();
        let loss = (after.1 - before.1) as f32 * 100.0 / (after.0 - before.0) as f32;
        let report = control::Report {
            rtt_ms: 10.0,
            loss_pct: loss,
            unrecovered_loss_pct: Some(loss),
            ..Default::default()
        };
        controller.update(now + Duration::from_millis(250), &report, 10.0, 0.0);
        if limited && loss > 3.0 {
            pressured += 1;
        }
        if limited && loss == 0.0 && controller.budget() <= 10_000 {
            clean += 1;
        }
        // Probes deliberately cross capacity: judge convergence over the
        // constrained interval rather than one probe's phase.
        if limited && tick >= 24 {
            settled.push(controller.budget());
        }
    }
    let mean = settled.iter().sum::<u32>() / settled.len() as u32;
    assert!((8_000..=10_500).contains(&mean), "did not converge: {mean}");
    assert!(congestion::video_budget(10_000, 10, 0, 120, false, false) < 10_000);
    assert!(pressured >= 4, "must exercise sustained overload");
    assert!(
        clean >= 8,
        "must deliver clean windows under capacity: {clean}"
    );
    assert_eq!(
        controller.budget(),
        50_000,
        "must recover to requested rate"
    );
}
