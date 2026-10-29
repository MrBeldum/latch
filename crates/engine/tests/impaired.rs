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
use latch_stream::{Event, Frame, FrameSlot, Session, Settings};
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
fn relay(host_port: u16, impairment: Impairment) -> u16 {
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

fn square_is_white(f: &Frame) -> bool {
    let mut sum = 0u64;
    for y in 16..48 {
        for x in 16..48 {
            sum += f.y[y * f.y_stride + x] as u64;
        }
    }
    sum / (32 * 32) > 128
}

#[test]
fn a_bad_network_slows_the_picture_but_loses_no_key() {
    std::env::set_var("LATCH_TEST_PATTERN", "sweep");
    let bad = Impairment {
        loss: 0.03,
        burst_enter: 0.002,
        burst_exit: 0.3,
        duplicate: 0.01,
        reorder: 0.01,
        reorder_delay: Duration::from_millis(4),
        delay: Duration::from_millis(10),
        jitter: Duration::from_millis(4),
    };
    let port = relay(host(), bad);
    let frames = Arc::new(FrameSlot::default());
    let (tx, rx) = mpsc::channel();
    let session = Session::start(
        latch_stream::Server {
            address: IpAddr::V4(Ipv4Addr::LOCALHOST),
            port,
        },
        Settings {
            width: 640,
            height: 360,
            fps: 60,
            bitrate_kbps: 6_000,
            hevc: false,
            audio: false,
            name: "impaired test".into(),
        },
        frames.clone(),
        tx,
        || {},
    );
    let start = Instant::now();
    loop {
        match rx.recv_timeout(Duration::from_millis(50)) {
            Ok(Event::Connected) => break,
            Ok(Event::Failed { stage, message }) => panic!("{stage}: {message}"),
            _ => assert!(start.elapsed() < Duration::from_secs(10), "never connected"),
        }
    }
    // Every press flips the square once. Presses go out back to back, far
    // faster than a person types, through the same losses: the square must
    // end on the colour the count of presses says.
    let input = session.input();
    let presses = 41;
    for _ in 0..presses {
        input.key(0x41, true, 0);
        std::thread::sleep(Duration::from_millis(5));
        input.key(0x41, false, 0);
        std::thread::sleep(Duration::from_millis(5));
    }
    let mut seconds = Vec::new();
    for _ in 0..8 {
        std::thread::sleep(Duration::from_secs(1));
        let s = session.stats();
        eprintln!(
            "{:5.1} fps {:5.2} Mbps loss {:4.1}% fec {:3} decode {:4.1} ms",
            s.fps, s.mbps, s.loss_pct, s.fec_recovered, s.decode_ms
        );
        seconds.push(s);
    }
    let mut white = None;
    let deadline = Instant::now() + Duration::from_secs(3);
    let mut seq = 0;
    while Instant::now() < deadline {
        if frames.seq() > seq {
            seq = frames.seq();
            if let Some(f) = frames.take() {
                white = Some(square_is_white(&f));
                frames.recycle(f);
            }
        }
        std::thread::sleep(Duration::from_millis(5));
    }
    // The pattern starts black; an odd number of presses leaves it white.
    assert_eq!(
        white,
        Some(presses % 2 == 1),
        "a key press was lost or doubled"
    );
    let fps: f32 = seconds.iter().map(|s| s.fps).sum::<f32>() / seconds.len() as f32;
    let recovered: u32 = seconds.iter().map(|s| s.fec_recovered).sum();
    eprintln!("average {fps:.1} fps through the bad network; FEC rebuilt {recovered} shards");
    assert!(fps > 30.0, "the picture nearly stopped: {fps:.1} fps");
    assert!(
        recovered > 0,
        "FEC never had to rebuild anything: was there loss?"
    );
    session.stop();
}
