use super::*;
use crate::video::EncodedVideo;
use std::net::TcpListener;
use std::sync::atomic::AtomicUsize;

fn config() -> VideoConfig {
    VideoConfig {
        width: 64,
        height: 64,
        fps: 60,
        bitrate_kbps: 1000,
        codec: Codec::H264Baseline,
    }
}

fn info() -> VideoInfo {
    VideoInfo {
        encoder: "initial".into(),
        hardware: true,
        codec: Codec::H264Baseline,
        width: 64,
        height: 64,
        display: DisplayRect {
            width: 64,
            height: 64,
            ..Default::default()
        },
    }
}

fn session(addr: SocketAddr) -> Arc<Session> {
    Arc::new(Session {
        id: 7,
        token: [0; 16],
        viewer: "test".into(),
        announced: addr,
        addr: Mutex::new(Some(addr)),
        ended: Mutex::new(None),
        audio_notice: Mutex::new(None),
        stop: AtomicBool::new(false),
        finished: Mutex::new(false),
        finished_cv: Condvar::new(),
        keyframe: AtomicBool::new(false),
        input: Mutex::new(InputState {
            receiver: InputReceiver::new(),
            sink: None,
            display: info().display,
        }),
        rtt: Mutex::new(RttEstimator::new()),
        fec: AtomicU8::new(DEFAULT_FEC_PERCENT),
        heard_us: AtomicU64::new(0),
        frames: AtomicU64::new(0),
        bytes: AtomicU64::new(0),
        transport: TransportState::new(1000),
        keyframe_request: Mutex::new(None),
        audio_enabled: false,
    })
}

fn sockets() -> (TcpStream, TcpStream) {
    let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
    let client = TcpStream::connect(listener.local_addr().unwrap()).unwrap();
    let (host, _) = listener.accept().unwrap();
    client
        .set_read_timeout(Some(Duration::from_secs(3)))
        .unwrap();
    (host, client)
}

type Next = Box<dyn FnMut(bool, Duration, &mut VideoInfo) -> anyhow::Result<EncodedVideo> + Send>;

struct Video {
    info: VideoInfo,
    next: Next,
    held: Arc<AtomicBool>,
}

impl Drop for Video {
    fn drop(&mut self) {
        self.held.store(false, Ordering::Release);
    }
}

impl VideoPipeline for Video {
    fn info(&self) -> VideoInfo {
        self.info.clone()
    }
    fn next(&mut self, keyframe: bool, idle: Duration) -> anyhow::Result<EncodedVideo> {
        (self.next)(keyframe, idle, &mut self.info)
    }
}

#[test]
fn a_viewer_that_does_not_read_cannot_block_the_control_writer() {
    let server = Server::bind(0, Options::default()).unwrap();
    let session = session("127.0.0.1:9".parse().unwrap());
    let (mut host, _client) = sockets();
    // Windows grows a loopback send backlog on demand, so without a fixed
    // send buffer the prefill below never fills it and the Accept never
    // blocks. Setting SO_SNDBUF turns that autotuning off.
    #[cfg(windows)]
    {
        let _ = socket2::SockRef::from(&host).set_send_buffer_size(8192);
        let _ = socket2::SockRef::from(&_client).set_recv_buffer_size(8192);
    }
    host.set_nonblocking(true).unwrap();
    let until = Instant::now() + Duration::from_millis(200);
    while Instant::now() < until {
        match host.write(&[0; 65536]) {
            Ok(_) => {}
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                std::thread::sleep(Duration::from_millis(1))
            }
            Err(e) => panic!("prefill failed: {e}"),
        }
    }
    host.set_nonblocking(false).unwrap();
    let accept = Accept {
        session: session.id,
        token: control::token_hex(&session.token),
        codec: Codec::H264Baseline,
        width: 64,
        height: 64,
        fps: 60,
        bitrate_kbps: 1000,
        encoder: "x".repeat(60_000),
        audio: None,
        audio_problem: String::new(),
    };
    let (tx, rx) = mpsc::channel();
    let worker = std::thread::spawn(move || {
        let reason = server.control(&session, host, accept, None, || {
            panic!("blocked Accept must fail")
        });
        tx.send(reason).unwrap();
    });
    assert!(rx
        .recv_timeout(Duration::from_secs(10))
        .unwrap()
        .contains("before the stream started"));
    worker.join().unwrap();
}

#[test]
fn an_opening_reservation_is_busy_before_a_viewer_is_installed() {
    let server = Server::bind(0, Options::default()).unwrap();
    assert!(!server.busy());
    let guard = server.opening.lock();
    assert!(server.busy());
    assert!(server.status().viewer.is_none());
    drop(guard);
    assert!(!server.busy());
}

#[test]
fn ending_revokes_and_releases_input_immediately() {
    struct Sink(Arc<AtomicUsize>);
    impl InputSink for Sink {
        fn apply(&mut self, _: &input::Event, _: DisplayRect) -> anyhow::Result<()> {
            Ok(())
        }
        fn release_all(&mut self) {
            self.0.fetch_add(1, Ordering::Relaxed);
        }
    }
    let session = session("127.0.0.1:9".parse().unwrap());
    let releases = Arc::new(AtomicUsize::new(0));
    session.input.lock().sink = Some(Box::new(Sink(releases.clone())));
    session.end(end::ENDED, "done");
    assert!(session.input.lock().sink.is_none());
    assert_eq!(releases.load(Ordering::Relaxed), 1);
    session.end(end::ENDED, "again");
    assert_eq!(releases.load(Ordering::Relaxed), 1);
}

#[test]
fn video_reopen_failure_uses_remaining_budget() {
    let server = Server::bind(0, Options::default()).unwrap();
    let session = session("127.0.0.1:9".parse().unwrap());
    let mut attempts = 0;
    let pipeline = || {
        Box::new(Video {
            info: info(),
            held: Arc::new(AtomicBool::new(true)),
            next: Box::new(|_, _, _| anyhow::bail!("capture lost")),
        }) as Box<dyn VideoPipeline>
    };
    send_video(&server, &session, pipeline(), config(), |_| {
        attempts += 1;
        if attempts == 1 {
            anyhow::bail!("temporarily busy");
        }
        Ok(pipeline())
    });
    assert_eq!(attempts, VIDEO_RECOVERIES);
}

#[test]
fn repeated_video_errors_release_exclusive_capture_and_have_a_retry_budget() {
    let server = Server::bind(0, Options::default()).unwrap();
    let session = session("127.0.0.1:9".parse().unwrap());
    let held = Arc::new(AtomicBool::new(false));
    let attempts = AtomicUsize::new(0);
    let open = |_: &VideoConfig| -> anyhow::Result<Box<dyn VideoPipeline>> {
        assert!(
            !held.swap(true, Ordering::AcqRel),
            "old capture still owns output"
        );
        let mut sent = false;
        Ok(Box::new(Video {
            info: info(),
            next: Box::new(move |_, _, _| {
                if std::mem::replace(&mut sent, true) {
                    anyhow::bail!("capture lost");
                }
                Ok(EncodedVideo {
                    data: vec![1; 100],
                    keyframe: true,
                    captured: Instant::now(),
                })
            }),
            held: held.clone(),
        }))
    };
    send_video(&server, &session, open(&config()).unwrap(), config(), |c| {
        attempts.fetch_add(1, Ordering::Relaxed);
        open(c)
    });
    assert_eq!(attempts.load(Ordering::Relaxed), VIDEO_RECOVERIES);
    // Fatal capture failure cancels queued media; reopening must still be bounded.
    assert!(session.frames.load(Ordering::Relaxed) <= (VIDEO_RECOVERIES + 1) as u64);
    assert!(!held.load(Ordering::Acquire));
    assert_eq!(
        session.ended.lock().as_ref().unwrap().code,
        end::CAPTURE_FAILED
    );
}

#[test]
fn idle_requests_are_prompt_and_each_encoded_frame_updates_mapping_and_is_sent() {
    let server = Server::bind(0, Options::default()).unwrap();
    let udp = UdpSocket::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
    udp.set_read_timeout(Some(Duration::from_secs(1))).unwrap();
    let session = session(udp.local_addr().unwrap());
    let (entered_tx, entered_rx) = mpsc::channel();
    let (request_tx, request_rx) = mpsc::channel();
    let (idr_tx, idr_rx) = mpsc::channel();
    let mut calls = 0;
    let worker_session = session.clone();
    let pipeline = Video {
        info: info(),
        held: Arc::new(AtomicBool::new(true)),
        next: Box::new(move |keyframe, idle, info| {
            calls += 1;
            if calls == 1 {
                assert!(!keyframe);
                entered_tx.send(()).unwrap();
                request_rx.recv_timeout(Duration::from_secs(1)).unwrap();
                std::thread::sleep(idle);
                info.display = DisplayRect {
                    x: -128,
                    y: 10,
                    width: 128,
                    height: 96,
                };
                info.encoder = "fallback".into();
                info.hardware = false;
            } else if calls == 2 {
                assert!(keyframe);
                idr_tx.send(()).unwrap();
            } else {
                while !worker_session.stopping() {
                    std::thread::sleep(Duration::from_millis(1));
                }
                anyhow::bail!("stopped");
            }
            Ok(EncodedVideo {
                data: vec![calls; 100],
                keyframe,
                captured: Instant::now(),
            })
        }),
    };
    let worker = {
        let server = server.clone();
        let session = session.clone();
        std::thread::spawn(move || {
            send_video(&server, &session, Box::new(pipeline), config(), |_| {
                panic!("unexpected reopen")
            })
        })
    };
    entered_rx.recv_timeout(Duration::from_secs(1)).unwrap();
    session.keyframe.store(true, Ordering::Release);
    let start = Instant::now();
    request_tx.send(()).unwrap();
    // What matters is that the very next encode is the keyframe: the mock
    // asserts that on its second call. Wall-clock bounds are left loose;
    // a loaded CI runner can take 4x longer to wake a sleeping thread.
    idr_rx
        .recv_timeout(Duration::from_secs(5))
        .expect("the keyframe request was never served");
    eprintln!(
        "keyframe served {:?} after the request (idle repeat {:?})",
        start.elapsed(),
        IDLE_REPEAT
    );
    let mut seen = [false; 2];
    let mut buf = [0; 2048];
    while !seen.iter().all(|v| *v) {
        let n = udp.recv(&mut buf).unwrap();
        if let Datagram::Video { shard, .. } = wire::parse(&buf[..n]).unwrap() {
            seen[shard.frame as usize] = true;
        }
    }
    session.stop.store(true, Ordering::Release);
    worker.join().unwrap();
    let display = session.input.lock().display;
    assert_eq!(display.map(32, 32, 64, 64), (-64, 58));
    assert_eq!(server.status().encoder, "fallback");
    assert!(!server.status().hardware);
    assert_eq!(session.frames.load(Ordering::Relaxed), 2);
}

struct Sound {
    session: Arc<Session>,
    fail: bool,
    frames: u32,
    held: Arc<AtomicBool>,
}

impl Drop for Sound {
    fn drop(&mut self) {
        self.held.store(false, Ordering::Release);
    }
}

impl AudioCapture for Sound {
    fn read(
        &mut self,
        out: &mut [i16; audio::FRAME * audio::CHANNELS],
        _: Duration,
    ) -> anyhow::Result<bool> {
        if self.fail {
            anyhow::bail!("audio device lost");
        }
        if self.frames == 4 {
            self.session.end(end::ENDED, "test complete");
            return Ok(false);
        }
        std::thread::sleep(Duration::from_millis(10));
        // Independent, full-range channel noise forces large coded frames.
        let mut state = 1 + self.frames;
        for sample in out {
            state = state.wrapping_mul(1664525).wrapping_add(1013904223);
            *sample = (state >> 16) as i16;
        }
        self.frames += 1;
        Ok(true)
    }
    fn describe(&self) -> String {
        "test noise".into()
    }
}

#[test]
fn audio_reports_failure_retries_with_backoff_clears_notice_and_fits_the_mtu() {
    let server = Server::bind(0, Options::default()).unwrap();
    let udp = UdpSocket::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
    udp.set_read_timeout(Some(Duration::from_secs(1))).unwrap();
    let session = session(udp.local_addr().unwrap());
    let held = Arc::new(AtomicBool::new(true));
    let capture = Sound {
        session: session.clone(),
        fail: true,
        frames: 0,
        held: held.clone(),
    };
    let (host, mut client) = sockets();
    let accept = Accept {
        session: session.id,
        token: control::token_hex(&session.token),
        codec: Codec::H264Baseline,
        width: 64,
        height: 64,
        fps: 60,
        bitrate_kbps: 1000,
        encoder: "test".into(),
        audio: Some(AudioFormat {
            rate: audio::RATE,
            channels: 2,
            frame_samples: AUDIO_FRAME as u32,
            codec: "lpcm-rice".into(),
        }),
        audio_problem: String::new(),
    };
    let worker = {
        let server = server.clone();
        let session = session.clone();
        std::thread::spawn(move || {
            server.control(&session, host, accept, None, || {
                let server = server.clone();
                let session = session.clone();
                let mut tries = 0;
                let start = Instant::now();
                let worker_session = session.clone();
                let audio = std::thread::spawn(move || {
                    send_audio(&server, &session, Box::new(capture), || {
                        assert!(
                            !held.load(Ordering::Acquire),
                            "failed audio capture still owns device"
                        );
                        tries += 1;
                        if tries == 1 {
                            anyhow::bail!("audio server still down");
                        }
                        assert!(
                            start.elapsed() >= Duration::from_millis(500),
                            "no retry backoff"
                        );
                        held.store(true, Ordering::Release);
                        Ok(Box::new(Sound {
                            session: worker_session.clone(),
                            fail: false,
                            frames: 0,
                            held: held.clone(),
                        }))
                    })
                });
                (None, Some(audio))
            })
        })
    };
    assert!(matches!(
        control::read_message(&mut client).unwrap(),
        Message::Accept(_)
    ));
    let mut notices = Vec::new();
    loop {
        match control::read_message(&mut client).unwrap() {
            Message::Notice(n) if n.kind == "audio" => notices.push(n.message),
            Message::End(_) => break,
            _ => {}
        }
    }
    worker.join().unwrap();
    assert_eq!(
        notices,
        ["audio device lost", "audio server still down", ""]
    );
    assert!(server.status().audio_problem.is_empty());
    let mut buf = [0; 4096];
    let mut packets = 0u32;
    let mut parity = 0;
    let mut previous_pts = None;
    let mut decoded = Vec::new();
    while packets < 8 || parity < 2 {
        let n = udp.recv(&mut buf).unwrap();
        assert!(
            n <= latch_proto::MAX_DATAGRAM,
            "audio datagram is {n} bytes"
        );
        if let Datagram::Audio { packet, .. } = wire::parse(&buf[..n]).unwrap() {
            if packet.parity {
                parity += 1;
            } else {
                latch_codec::audio::decode(packet.payload, &mut decoded).unwrap();
                assert_eq!(decoded.len(), AUDIO_FRAME * audio::CHANNELS);
                let mut state = 1 + packets / 2;
                let expected: Vec<i16> = (0..audio::FRAME * audio::CHANNELS)
                    .map(|_| {
                        state = state.wrapping_mul(1664525).wrapping_add(1013904223);
                        (state >> 16) as i16
                    })
                    .collect();
                let offset = (packets as usize % 2) * AUDIO_FRAME * audio::CHANNELS;
                assert_eq!(
                    decoded,
                    expected[offset..offset + AUDIO_FRAME * audio::CHANNELS]
                );
                if packets % 2 == 1 {
                    assert_eq!(packet.pts.wrapping_sub(previous_pts.unwrap()), 5000);
                }
                previous_pts = Some(packet.pts);
                packets += 1;
            }
        }
    }
}
