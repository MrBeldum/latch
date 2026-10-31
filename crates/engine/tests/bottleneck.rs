//! Congestion feedback through an encoded noise-pattern desktop, UDP relay and viewer.
//! A sustained downstream queue-delay step exercises adaptation even when a
//! slow encoder underfills its budget. A fake-clock saturated FIFO separately
//! checks convergence at 10 Mbps; an encoder budget is not measured traffic.

// Exercise the production controller without widening its public API.
#[path = "../src/congestion.rs"]
mod congestion;

use latch_core::http::{self, Response};
use latch_engine::server::{Options, Server};
use latch_proto::control::{self, StreamRequest};
use latch_proto::sim::{Impairment, Link};
use latch_stream::{Event, FrameSlot, Session, Settings};
use parking_lot::Mutex;
use std::io::{Read, Write};
use std::net::{IpAddr, Ipv4Addr, SocketAddr, TcpListener, TcpStream, UdpSocket};
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

/// A relay on 127.0.0.1: TCP passed through as is, UDP through two
/// impaired links. Returns its port (the same number for TCP and UDP).
fn relay(
    host_port: u16,
    impairment: Impairment,
    queue_delay_ms: Arc<std::sync::atomic::AtomicU32>,
) -> u16 {
    let (listener, udp) = loop {
        // UDP picks the port, as in `host`.
        let u = UdpSocket::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
        let port = u.local_addr().unwrap().port();
        if let Ok(l) = TcpListener::bind((Ipv4Addr::LOCALHOST, port)) {
            break (l, u);
        }
    };
    let port = listener.local_addr().unwrap().port();
    std::thread::spawn(move || {
        for client in listener.incoming().flatten() {
            let server = TcpStream::connect((Ipv4Addr::LOCALHOST, host_port)).unwrap();
            let pipe = |mut from: TcpStream, mut to: TcpStream| {
                std::thread::spawn(move || {
                    let mut buf = [0u8; 16384];
                    while let Ok(n) = from.read(&mut buf) {
                        if n == 0 || to.write_all(&buf[..n]).is_err() {
                            break;
                        }
                    }
                    let _ = to.shutdown(std::net::Shutdown::Both);
                });
            };
            pipe(client.try_clone().unwrap(), server.try_clone().unwrap());
            pipe(server, client);
        }
    });
    // Viewer → relay socket → host from a second socket, so the host sees
    // the relay as the viewer. Per direction, one thread blocks on its socket
    // and puts what arrives into the link; another sleeps until the link's
    // next datagram is due and sends it then. Releasing only when something
    // else arrives (or when a socket timeout fires, about 15 ms on Windows)
    // held pings behind a thin stream and looked like a queue that never
    // drained.
    let to_host = UdpSocket::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
    let host_addr = SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), host_port);
    let viewer: Arc<Mutex<Option<SocketAddr>>> = Arc::default();
    let up = Arc::new(Mutex::new(Link::new(impairment, 7)));
    let down = Arc::new(Mutex::new(Link::new(impairment, 11)));
    {
        let (from_viewer, link, viewer) = (udp.try_clone().unwrap(), up.clone(), viewer.clone());
        std::thread::spawn(move || {
            let mut buf = [0u8; 2048];
            while let Ok((n, from)) = from_viewer.recv_from(&mut buf) {
                *viewer.lock() = Some(from);
                link.lock().send(Instant::now(), &buf[..n]);
            }
        });
    }
    {
        let (from_host, link) = (to_host.try_clone().unwrap(), down.clone());
        std::thread::spawn(move || {
            let mut buf = [0u8; 2048];
            while let Ok((n, _)) = from_host.recv_from(&mut buf) {
                // Model standing cross-traffic queueing independently of the
                // encoder's offered rate. All downstream traffic, including
                // ping/pong, waits in the same queue; TCP reports stay untouched.
                let delay = Duration::from_millis(u64::from(
                    queue_delay_ms.load(std::sync::atomic::Ordering::Relaxed),
                ));
                link.lock().send(Instant::now() + delay, &buf[..n]);
            }
        });
    }
    let deliver = move |link: Arc<Mutex<Link>>,
                        out: UdpSocket,
                        to: Box<dyn Fn() -> Option<SocketAddr> + Send>| {
        move || loop {
            let now = Instant::now();
            let (due, next) = {
                let mut link = link.lock();
                (link.receive(now), link.next_arrival())
            };
            if let Some(to) = to() {
                for d in due {
                    let _ = out.send_to(&d, to);
                }
            }
            // std's sleep is high resolution on every platform this runs on.
            let wait = next.map_or(Duration::from_millis(1), |t| {
                t.saturating_duration_since(now)
            });
            std::thread::sleep(wait.clamp(Duration::from_micros(100), Duration::from_millis(1)));
        }
    };
    let up = deliver(up, to_host, Box::new(move || Some(host_addr)));
    let down = deliver(down, udp, Box::new(move || *viewer.lock()));
    std::thread::spawn(up);
    std::thread::spawn(down);
    port
}

#[test]
fn noise_stream_adapts_to_sustained_queue_delay_and_recovers() {
    use std::sync::atomic::{AtomicU32, Ordering};
    std::env::set_var("LATCH_TEST_PATTERN", "noise");
    let queue_delay_ms = Arc::new(AtomicU32::new(5));
    let port = relay(host(), Impairment::NONE, queue_delay_ms.clone());
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
            cursor: false,
            width: 640,
            height: 360,
            fps: 12,
            bitrate_kbps: 50_000,
            hevc: false,
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
        if frames.seq() >= 10 && s.rtt_ms > 0 && s.applied_bitrate_kbps > 0 {
            break s;
        }
        assert!(Instant::now() < deadline, "no baseline: {s:?}");
    };
    eprintln!("baseline: {baseline:?}");
    queue_delay_ms.store(45, Ordering::Relaxed);
    let mut clean = 0;
    let mut constrained = 0;
    let mut last_frame = frames.seq();
    let deadline = Instant::now() + Duration::from_secs(45);
    while Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(300));
        let s = session.stats();
        let frame = frames.seq();
        eprintln!(
            "queued: applied={} received={:.2} fps={:.1} loss={:.1} rtt={}",
            s.applied_bitrate_kbps, s.received_mbps, s.fps, s.loss_pct, s.rtt_ms
        );
        if s.applied_bitrate_kbps > 0
            && s.applied_bitrate_kbps < baseline.applied_bitrate_kbps / 4
            && s.rtt_ms >= baseline.rtt_ms + 20
            && frame > last_frame
            && s.fps >= 5.0
            && s.loss_pct < 3.0
        {
            clean += 1;
            constrained = s.applied_bitrate_kbps;
            if clean >= 5 {
                break;
            }
        } else {
            clean = 0;
        }
        last_frame = frame;
    }
    assert!(clean >= 5, "never settled: {:?}", session.stats());
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
            && s.fps >= 5.0
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
        if tick == 40 {
            assert!(
                controller.budget() < 10_000,
                "did not converge: {}",
                controller.budget()
            );
            assert!(congestion::video_budget(controller.budget(), 0, false) < 10_000);
        }
    }
    assert!(pressured >= 4, "must exercise sustained overload");
    assert!(clean >= 8, "must deliver clean windows under capacity");
    assert_eq!(
        controller.budget(),
        50_000,
        "must recover to requested rate"
    );
}
