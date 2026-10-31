//! Session boundaries through real TCP connections, without a desktop.
use latch_core::http;
use latch_engine::server::{Options, Server};
use latch_proto::control::{self, Accept, Codec, End, Message, StreamRequest};
use latch_stream::{Event, FrameSlot, Session, Settings};
use std::io::Write;
use std::net::{IpAddr, Ipv4Addr, TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc, Arc};
use std::time::{Duration, Instant};

fn request(name: &str) -> StreamRequest {
    StreamRequest {
        protocol: latch_proto::PROTOCOL,
        width: 64,
        height: 64,
        fps: 60,
        bitrate_kbps: 1000,
        codecs: vec![Codec::H264Baseline],
        udp_port: 9,
        audio: false,
        name: name.into(),
        cursor: false,
    }
}

#[test]
fn preparation_stays_reserved_until_run_or_drop_and_takeover_waits_for_slow_undo() {
    std::env::set_var("LATCH_TEST_PATTERN", "still");
    let active = Arc::new(AtomicBool::new(false));
    let server = Server::bind(
        0,
        Options {
            prepare: Some(Box::new({
                let active = active.clone();
                move |req| {
                    assert!(
                        !active.swap(true, Ordering::AcqRel),
                        "prep overlapped the previous undo"
                    );
                    let active = active.clone();
                    let slow = req.name == "slow";
                    Ok(Some(Box::new(move || {
                        if slow {
                            std::thread::sleep(Duration::from_secs(6));
                        }
                        active.store(false, Ordering::Release);
                    })))
                }
            })),
            ..Options::default()
        },
    )
    .unwrap();
    let peer = "127.0.0.1:12345".parse().unwrap();
    let first = server.prepare(request("unused"), peer).unwrap();
    let (tx, rx) = mpsc::channel();
    let worker = {
        let server = server.clone();
        std::thread::spawn(move || tx.send(server.prepare(request("second"), peer)).unwrap())
    };
    assert!(matches!(
        rx.recv_timeout(Duration::from_millis(100)),
        Err(mpsc::RecvTimeoutError::Timeout)
    ));
    drop(first);
    drop(rx.recv_timeout(Duration::from_secs(3)).unwrap().unwrap());
    worker.join().unwrap();
    assert!(!active.load(Ordering::Acquire));

    let first = server.prepare(request("slow"), peer).unwrap();
    let (tx, rx) = mpsc::channel();
    let next = {
        let server = server.clone();
        std::thread::spawn(move || tx.send(server.prepare(request("takeover"), peer)).unwrap())
    };
    assert!(matches!(
        rx.recv_timeout(Duration::from_millis(100)),
        Err(mpsc::RecvTimeoutError::Timeout)
    ));
    let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
    let mut old_viewer = TcpStream::connect(listener.local_addr().unwrap()).unwrap();
    old_viewer
        .set_read_timeout(Some(Duration::from_secs(3)))
        .unwrap();
    let (tcp, _) = listener.accept().unwrap();
    let running = {
        let server = server.clone();
        std::thread::spawn(move || server.run(first, tcp))
    };
    assert!(matches!(
        control::read_message(&mut old_viewer).unwrap(),
        Message::Accept(_)
    ));
    loop {
        if let Message::End(e) = control::read_message(&mut old_viewer).unwrap() {
            assert_eq!(e.code, control::end::TAKEN_OVER);
            break;
        }
    }
    // The old viewer ends promptly, but its six-second undo must complete
    // before the next prep. The former five-second wait returned HTTP 503.
    let start = Instant::now();
    let prepared = rx.recv_timeout(Duration::from_secs(10)).unwrap().unwrap();
    assert!(start.elapsed() > Duration::from_secs(5));
    drop(prepared);
    next.join().unwrap();
    running.join().unwrap();
    assert!(!active.load(Ordering::Acquire));
}

#[test]
fn post_accept_end_close_and_missing_media_are_reported_as_failures() {
    for mode in ["end", "close", "no media"] {
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
        let port = listener.local_addr().unwrap().port();
        // Serve one request only; the worker is joined below.
        let host = std::thread::spawn(move || {
            let (mut tcp, _) = listener.accept().unwrap();
            assert!(http::read_request(&mut tcp).unwrap().is_some());
            write!(
                tcp,
                "HTTP/1.1 101 Switching Protocols\r\nConnection: Upgrade\r\nUpgrade: {}\r\n\r\n",
                control::UPGRADE
            )
            .unwrap();
            control::write_message(
                &mut tcp,
                &Message::Accept(Accept {
                    session: 123,
                    token: control::token_hex(&[7; 16]),
                    codec: Codec::H264Baseline,
                    width: 64,
                    height: 64,
                    fps: 60,
                    bitrate_kbps: 1000,
                    encoder: "fake host without UDP".into(),
                    audio: None,
                    audio_problem: String::new(),
                    cursor: false,
                }),
            )
            .unwrap();
            match mode {
                "end" => control::write_message(
                    &mut tcp,
                    &Message::End(End {
                        code: control::end::CAPTURE_FAILED,
                        message: "capture failed before media".into(),
                    }),
                )
                .unwrap(),
                "no media" => {
                    tcp.set_read_timeout(Some(Duration::from_secs(12))).unwrap();
                    while control::read_message(&mut tcp).is_ok() {
                        if control::write_message(&mut tcp, &Message::Keepalive).is_err() {
                            break;
                        }
                    }
                }
                _ => {}
            }
        });
        let (tx, rx) = mpsc::channel();
        let session = Session::start(
            latch_stream::Server {
                address: IpAddr::V4(Ipv4Addr::LOCALHOST),
                port,
            },
            Settings {
                width: 64,
                height: 64,
                fps: 60,
                bitrate_kbps: 1000,
                hevc: false,
                audio: false,
                name: "failure test".into(),
                cursor: false,
            },
            Arc::new(FrameSlot::default()),
            tx,
            || {},
        );
        let deadline = Instant::now() + Duration::from_secs(12);
        loop {
            match rx
                .recv_timeout(deadline.saturating_duration_since(Instant::now()))
                .expect("no failure event after Accept")
            {
                Event::Failed { stage, message } => {
                    match mode {
                        "end" => assert!(message.contains("capture failed before media")),
                        "close" => assert!(message.contains("closed the connection")),
                        _ => {
                            assert_eq!(stage, "connecting");
                            assert!(message.contains("no picture arrived"));
                            assert!(message.contains("may be blocked"));
                        }
                    }
                    break;
                }
                Event::Connected | Event::Terminated { .. } => {
                    panic!("incorrect outcome for {mode}")
                }
                _ => {}
            }
        }
        host.join().unwrap();
        session.stop();
    }
}
