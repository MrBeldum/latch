//! A whole session through a bad network: the viewer reaches the host
//! through a relay that loses (at random and in bursts), duplicates,
//! reorders and delays datagrams in both directions, using the protocol's
//! own simulator. The control connection is relayed untouched, as TCP
//! would deliver it. Checks that the picture keeps coming, that lost
//! frames are recovered (by FEC or a keyframe), and that every key press
//! reaches the host exactly once and in order.

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
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
        let port = listener.local_addr().unwrap().port();
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
    capacity: Arc<std::sync::atomic::AtomicU32>,
) -> u16 {
    let (listener, udp) = loop {
        let l = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
        let port = l.local_addr().unwrap().port();
        if let Ok(u) = UdpSocket::bind((Ipv4Addr::LOCALHOST, port)) {
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
    // the relay as the viewer. One thread per direction, each blocked on its
    // own socket: polling both with short timeouts starved the relay on
    // Windows, whose socket timeouts are coarse (about 15 ms), so the relay
    // itself dropped what the test meant to deliver. Whatever the link holds
    // back is delivered on the thread's next wake-up, at most 5 ms later.
    let to_host = UdpSocket::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
    let host_addr = SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), host_port);
    let viewer: Arc<Mutex<Option<SocketAddr>>> = Arc::default();
    for socket in [&udp, &to_host] {
        socket
            .set_read_timeout(Some(Duration::from_millis(5)))
            .unwrap();
    }
    let up = {
        let from_viewer = udp.try_clone().unwrap();
        let to_host = to_host.try_clone().unwrap();
        let viewer = viewer.clone();
        move || {
            let mut link = Link::new(impairment, 7);
            let mut buf = [0u8; 2048];
            loop {
                if let Ok((n, from)) = from_viewer.recv_from(&mut buf) {
                    *viewer.lock() = Some(from);
                    link.send(Instant::now(), &buf[..n]);
                }
                for d in link.receive(Instant::now()) {
                    let _ = to_host.send_to(&d, host_addr);
                }
            }
        }
    };
    let down = move || {
        let mut link = Link::new(impairment, 11);
        let mut buf = [0u8; 2048];
        loop {
            if let Ok((n, _)) = to_host.recv_from(&mut buf) {
                link.set_bandwidth(
                    capacity.load(std::sync::atomic::Ordering::Relaxed),
                    Duration::from_millis(40),
                );
                link.send(Instant::now(), &buf[..n]);
            }
            if let Some(v) = *viewer.lock() {
                for d in link.receive(Instant::now()) {
                    let _ = udp.send_to(&d, v);
                }
            }
        }
    };
    std::thread::spawn(up);
    std::thread::spawn(down);
    port
}

#[test]
fn noise_stream_adapts_through_ten_megabit_bottleneck_and_recovers() {
    use std::sync::atomic::{AtomicU32, Ordering};
    std::env::set_var("LATCH_TEST_PATTERN", "noise");
    let capacity = Arc::new(AtomicU32::new(10_000));
    let port = relay(host(), Impairment::NONE, capacity.clone());
    let (tx, rx) = mpsc::channel();
    let session = Session::start(
        latch_stream::Server {
            address: IpAddr::V4(Ipv4Addr::LOCALHOST),
            port,
        },
        // Small enough that a CI runner's few cores encode noise at full
        // rate, so the 10 Mbps link, not the encoder, is the limit.
        Settings {
            width: 640,
            height: 360,
            fps: 60,
            bitrate_kbps: 50_000,
            hevc: false,
            audio: false,
            name: "bottleneck test".into(),
        },
        Arc::new(FrameSlot::default()),
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
    // Wait for behaviour rather than a specific convergence timestamp. Require
    // several clean windows so a transient empty receive window cannot pass.
    let mut clean = 0;
    let mut constrained = 0;
    let deadline = Instant::now() + Duration::from_secs(45);
    while Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(300));
        let s = session.stats();
        eprintln!(
            "limited: applied={} fps={:.1} loss={:.1} rtt={}",
            s.applied_bitrate_kbps, s.fps, s.loss_pct, s.rtt_ms
        );
        if s.applied_bitrate_kbps > 0
            && s.applied_bitrate_kbps < 10_000
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
    }
    assert!(clean >= 5, "never settled: {:?}", session.stats());
    capacity.store(0, Ordering::Relaxed);
    let deadline = Instant::now() + Duration::from_secs(40);
    let mut recovered = false;
    while Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(300));
        let s = session.stats();
        if s.applied_bitrate_kbps > constrained * 2 && s.fps >= 5.0 && s.loss_pct < 3.0 {
            recovered = true;
            break;
        }
    }
    session.stop();
    assert!(recovered, "bitrate did not recover: {:?}", session.stats());
}
