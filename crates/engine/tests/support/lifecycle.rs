use super::*;
use crate::video::EncodedVideo;
use std::net::TcpListener;
use std::sync::atomic::AtomicUsize;

fn config() -> VideoConfig {
    VideoConfig {
        color: Default::default(),
        width: 64,
        height: 64,
        fps: 60,
        bitrate_kbps: 1000,
        codec: Codec::H264Baseline,
    }
}

fn info() -> VideoInfo {
    VideoInfo {
        color: Default::default(),
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
        pads: Mutex::new(SessionPads {
            receiver: Default::default(),
            sink: None,
            slots: 0,
            failing: Default::default(),
        }),
        feedback: Default::default(),
        rtt: Mutex::new(RttEstimator::new()),
        rtt_low: Mutex::new(None),
        pace_kbps: AtomicU32::new(0),
        fec: AtomicU8::new(DEFAULT_FEC_PERCENT),
        heard_us: AtomicU64::new(0),
        frames: AtomicU64::new(0),
        bytes: AtomicU64::new(0),
        transport: TransportState::new(1000),
        keyframe_requests: Mutex::default(),
        recovery: Default::default(),
        audio_enabled: false,
        surround_kbps: 0,
        mic: Mutex::new(None),
        cursor_supported: false,
        draw_cursor: AtomicBool::new(true),
        control_writer: Mutex::new(None),
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
        color: Default::default(),
        cursor: false,
        gamepads: 0,
        gamepad_kinds: Vec::new(),
        gamepad_problem: String::new(),
        reference_invalidation: false,
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
        mic: None,
        mic_problem: String::new(),
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
    let guard = server.opening.lock(Duration::ZERO).unwrap();
    assert!(server.busy());
    assert!(server.status().viewer.is_none());
    // A second opening gives up instead of waiting for ever.
    let start = Instant::now();
    assert!(server.opening.lock(Duration::from_millis(50)).is_none());
    assert!(start.elapsed() >= Duration::from_millis(50));
    drop(guard);
    assert!(!server.busy());
    assert!(server.opening.lock(Duration::ZERO).is_some());
}

#[test]
fn a_stuck_media_thread_is_left_behind_at_the_deadline() {
    let (release, stuck) = mpsc::channel::<()>();
    let thread = std::thread::spawn(move || {
        let _ = stuck.recv();
    });
    let start = Instant::now();
    assert!(!join_by(thread, start + Duration::from_millis(50)));
    assert!(start.elapsed() < Duration::from_secs(2));
    drop(release);
    let done = std::thread::spawn(|| {});
    assert!(join_by(done, Instant::now() + Duration::from_secs(5)));
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

/// A codec whose encoder cannot go on (HEVC after a resize to a size its
/// only encoder does not take) ends the session at once, as an encoder
/// failure the viewer reconnects from, not after the reopen budget as a
/// capture failure.
#[test]
fn a_codec_that_cannot_go_on_ends_the_session_for_a_new_negotiation() {
    let server = Server::bind(0, Options::default()).unwrap();
    for during_reopen in [false, true] {
        let session = session("127.0.0.1:9".parse().unwrap());
        let mut attempts = 0;
        let pipeline = Box::new(Video {
            info: info(),
            held: Arc::new(AtomicBool::new(true)),
            next: Box::new(move |_, _, _| {
                if during_reopen {
                    anyhow::bail!("capture lost");
                }
                Err(video::Unencodable("1366 × 768 is not whole coding blocks".into()).into())
            }),
        });
        send_video(&server, &session, pipeline, config(), |_| {
            attempts += 1;
            Err(
                anyhow::Error::from(video::Unencodable("no HEVC at this size".into()))
                    .context("open the HEVC encoder"),
            )
        });
        assert_eq!(attempts, usize::from(during_reopen));
        let ended = session.ended.lock().clone().unwrap();
        assert_eq!(ended.code, end::ENCODER_FAILED);
        assert!(ended.message.contains("reconnecting"), "{}", ended.message);
    }
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
                    generation: 0,
                    data: vec![1; 100],
                    keyframe: true,
                    captured: Instant::now(),
                    picture: 0,
                    recovery: None,
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
                generation: 0,
                data: vec![calls; 100],
                keyframe,
                captured: Instant::now(),
                picture: 0,
                recovery: None,
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
        color: Default::default(),
        cursor: false,
        gamepads: 0,
        gamepad_kinds: Vec::new(),
        gamepad_problem: String::new(),
        reference_invalidation: false,
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
            frame_samples: audio::FRAME as u32,
            codec: "lpcm-rice".into(),
        }),
        audio_problem: String::new(),
        mic: None,
        mic_problem: String::new(),
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
    let mut decoded = Vec::new();
    while packets < 4 || parity < 1 {
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
                // One 5 ms frame, one packet.
                assert_eq!(decoded.len(), audio::FRAME * audio::CHANNELS);
                let mut state = 1 + packets;
                let expected: Vec<i16> = (0..audio::FRAME * audio::CHANNELS)
                    .map(|_| {
                        state = state.wrapping_mul(1664525).wrapping_add(1013904223);
                        (state >> 16) as i16
                    })
                    .collect();
                assert_eq!(decoded, expected);
                packets += 1;
            }
        }
    }
}

#[test]
fn audio_packets_split_the_frame_evenly_and_always_fit() {
    for (channels, samples) in [(1, 240), (2, 240), (6, 80), (8, 60)] {
        assert_eq!(audio_packet_samples(channels), samples, "{channels} ch");
        assert_eq!(audio::FRAME % samples, 0);
        assert!(
            latch_codec::audio::max_coded_len(channels, samples) <= latch_proto::audio::MAX_PAYLOAD
        );
    }
}

#[test]
fn surround_keeps_more_of_the_wire_for_sound_and_stereo_no_more_than_before() {
    assert_eq!(surround_kbps(1), 0);
    assert_eq!(surround_kbps(2), 0);
    assert_eq!(surround_kbps(6), 2400);
    assert_eq!(surround_kbps(8), 3600);
}

#[test]
fn requested_idrs_cannot_exceed_one_per_rtt_even_when_last_delivered_advances() {
    let start = Instant::now();
    for rtt_ms in [10, 100, 300] {
        let rtt = Duration::from_millis(rtt_ms);
        let mut last = None;
        let mut produced = Vec::new();
        for ms in 0..1000 {
            let now = start + Duration::from_millis(ms);
            if super::recovery_due(last, now, Some(rtt)) {
                last = Some(now);
                produced.push(now);
            }
        }
        assert!(produced
            .windows(2)
            .all(|pair| pair[1] - pair[0] >= rtt.max(KEYFRAME_GAP)));
        assert!(produced.len() >= 3, "recovery must not stall");
    }
}

#[test]
fn reopen_refuses_an_old_picture_held_in_the_send_queue() {
    use crate::software::{Bgra, CpuCapture, SoftwarePipeline};
    use latch_codec::h264::Decoder;

    struct Solid(u8);
    impl CpuCapture for Solid {
        fn size(&self) -> (u32, u32) {
            (16, 16)
        }
        fn display(&self) -> DisplayRect {
            info().display
        }
        fn name(&self) -> String {
            "solid".into()
        }
        fn grab(&mut self, frame: &mut Bgra, _: Duration) -> anyhow::Result<bool> {
            *frame = Bgra {
                width: 16,
                height: 16,
                stride: 64,
                data: [self.0, self.0, self.0, 255].repeat(256),
            };
            Ok(true)
        }
    }
    struct ReopenVideo {
        pipeline: SoftwarePipeline<Solid>,
        session: Arc<Session>,
        replacement: bool,
        calls: usize,
    }
    impl VideoPipeline for ReopenVideo {
        fn info(&self) -> VideoInfo {
            self.pipeline.info()
        }
        fn next(&mut self, keyframe: bool, idle: Duration) -> anyhow::Result<EncodedVideo> {
            self.calls += 1;
            anyhow::ensure!(self.calls == 1, "capture lost");
            let frame = self.pipeline.next(keyframe, idle)?;
            if self.replacement {
                self.session.stop.store(true, Ordering::Release);
            }
            Ok(frame)
        }
        fn recover_from(&mut self, picture: u64) -> bool {
            self.pipeline.recover_from(picture)
        }
    }
    let server = Server::bind(0, Options::default()).unwrap();
    let receiver = UdpSocket::bind("127.0.0.1:0").unwrap();
    let mut session = session(receiver.local_addr().unwrap());
    Arc::get_mut(&mut session).unwrap().recovery.enabled = true;
    let open = |replacement| ReopenVideo {
        pipeline: SoftwarePipeline::new(Solid(if replacement { 210 } else { 30 }), &config())
            .unwrap(),
        session: session.clone(),
        replacement,
        calls: 0,
    };
    let (tx, rx) = mpsc::sync_channel(2);
    // Hold both frames in the actual sender queue until encode_video has
    // reconstructed the entire capture/pipeline and cleared its recovery map.
    encode_video(
        &server,
        &session,
        Box::new(open(false)),
        config(),
        |_| Ok(Box::new(open(true))),
        tx,
    );
    let old = rx.recv().unwrap();
    let new = rx.recv().unwrap();
    assert_eq!(
        old.0.picture, new.0.picture,
        "pipeline-local numbers restart"
    );
    assert_ne!(old.0.data, new.0.data);
    let mut decoder = Decoder::new();
    let old_pixels = decoder.decode(&old.0.data).unwrap().unwrap().clone();
    let (tx, rx) = mpsc::sync_channel(2);
    tx.send(old).unwrap();
    tx.send(new).unwrap();
    drop(tx);
    session.stop.store(false, Ordering::Release);
    paced_video(&server, &session, rx);
    assert_eq!(session.recovery.sent.lock().len(), 2);
    let generation = session.recovery.sent.lock()[1].1;
    // The replacement IDR was sent but lost. The viewer only holds frame 0.
    session.keyframe.store(false, Ordering::Release);
    *session.recovery.request.lock() = Some(wire::RecoveryRequest {
        last_delivered: 0,
        first_invalid: 1,
        last_invalid: 1,
        id: 1,
    });
    let mut replacement = open(true);
    replacement.pipeline.next(true, Duration::ZERO).unwrap();
    session
        .recovery
        .take(&server, &session, &mut replacement, generation);
    assert_eq!(session.recovery.refused.load(Ordering::Relaxed), 1);
    assert!(session.keyframe.load(Ordering::Acquire));
    assert_eq!(session.recovery.asked.lock().as_ref(), None);
    let idr = replacement.pipeline.next(true, Duration::ZERO).unwrap();
    assert!(idr.keyframe);
    assert_eq!(idr.recovery, None);
    let pixels = decoder.decode(&idr.data).unwrap().unwrap();
    assert_ne!(pixels, &old_pixels);
    let mut fresh = Decoder::new();
    assert_eq!(pixels, fresh.decode(&idr.data).unwrap().unwrap());
}
