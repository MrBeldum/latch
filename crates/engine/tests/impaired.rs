//! A whole session through a bad network: the viewer reaches the host
//! through a relay that loses (at random and in bursts), duplicates,
//! reorders and delays datagrams in both directions, using the protocol's
//! own simulator. The control connection is relayed untouched, as TCP
//! would deliver it. Checks that the picture keeps coming, that lost
//! frames are recovered (by FEC or a keyframe), and that every key press
//! reaches the host exactly once and in order.

#[path = "support/relay.rs"]
mod relay;

use latch_core::http::{self, Response};
use latch_engine::server::{Options, Server};
use latch_proto::control::{self, StreamRequest};
use latch_proto::sim::Impairment;
use latch_stream::{Event, Frame, FrameSlot, Session, Settings};
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
    let port = relay::relay(host(), bad, Arc::new(std::sync::atomic::AtomicU32::new(0)));
    let frames = Arc::new(FrameSlot::default());
    let (tx, rx) = mpsc::channel();
    let session = Session::start(
        latch_stream::Server {
            address: IpAddr::V4(Ipv4Addr::LOCALHOST),
            port,
        },
        Settings {
            color: Default::default(),
            width: 640,
            height: 360,
            fps: 60,
            bitrate_kbps: 6_000,
            hevc: false,
            av1: false,
            audio: false,
            name: "impaired test".into(),
            cursor: false,
            gamepads: false,
            mic: false,
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
            "{:5.1} fps {:5.2} Mbps loss {:4.1}% fec {:3} decode {:4.1} ms budget {} kbps",
            s.fps, s.mbps, s.loss_pct, s.fec_recovered, s.decode_ms, s.wire_budget_kbps
        );
        seconds.push(s);
    }
    let mut white = None;
    let deadline = Instant::now() + Duration::from_secs(3);
    let mut seq = 0;
    while Instant::now() < deadline {
        if frames.seq() > seq {
            seq = frames.seq();
            if let Some(mut f) = frames.take() {
                // A Mac's decoder hands over its own buffer; read it here.
                f.ensure_cpu_planes();
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
    // The picture must keep coming, to the end: a stream that recovered
    // from nothing would trail off to 0 fps. How fast it comes is the
    // machine's (a debug build on a loaded CI runner manages about 10 fps
    // of this pattern; a release build on the ARM host keeps 50), measured
    // separately in docs/measurements.
    let tail = &seconds[seconds.len() - 3..];
    assert!(
        tail.iter().all(|s| s.fps >= 3.0),
        "the picture stopped: {:?}",
        tail.iter().map(|s| s.fps).collect::<Vec<_>>()
    );
    assert!(fps >= 5.0, "the picture nearly stopped: {fps:.1} fps");
    assert!(
        recovered > 0,
        "FEC never had to rebuild anything: was there loss?"
    );
    assert!(
        tail.iter().all(|s| s.wire_budget_kbps >= 4800),
        "random loss collapsed the budget: {:?}",
        tail.iter().map(|s| s.wire_budget_kbps).collect::<Vec<_>>()
    );
    session.stop();
}
