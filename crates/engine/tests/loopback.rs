//! A whole session on one machine: the engine in test-pattern mode (see
//! `testmode.rs`) serves a `latch-stream` viewer over loopback, through
//! the same HTTP upgrade the control service uses. Checks that the picture
//! that arrives is the picture that was drawn, that video keeps flowing,
//! how long a key press takes to show up in a decoded frame, and that
//! gamepads plug in, follow the viewer, rumble back and unplug.

use latch_core::http::{self, Response};
use latch_engine::pattern::{Motion, Pattern};
use latch_engine::server::{Options, Server};
use latch_engine::software::{Bgra, CpuCapture};
use latch_proto::control::{self, StreamRequest};
use latch_stream::{Event, Frame, FrameSlot, Session, Settings};
use std::io::Write;
use std::net::{IpAddr, Ipv4Addr, TcpListener, UdpSocket};
use std::sync::{mpsc, Arc, Mutex, MutexGuard, PoisonError};
use std::time::{Duration, Instant};

const W: u32 = 640;
const H: u32 = 360;
const FPS: u32 = 60;

/// A host on 127.0.0.1: an HTTP listener that upgrades `/v1/stream` into
/// the engine's server, both on the same port number.
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
                    if req.path != control::PATH {
                        return Response::json(404, &serde_json::json!({ "ok": false }));
                    }
                    let request: StreamRequest = match req.json() {
                        Ok(r) => r,
                        Err(e) => {
                            return Response::json(
                                400,
                                &serde_json::json!({ "ok": false, "error": e.to_string() }),
                            )
                        }
                    };
                    match server.prepare(request, peer) {
                        Ok(prepared) => {
                            let server = server.clone();
                            Response::switching(control::UPGRADE, move |tcp| {
                                server.run(prepared, tcp)
                            })
                        }
                        Err(r) => Response::json(
                            r.status,
                            &serde_json::json!({ "ok": false, "error": r.message }),
                        ),
                    }
                },
            )
        });
        return port;
    }
    panic!("no free port");
}

fn connect(port: u16, cursor: bool) -> (Session, Arc<FrameSlot>, mpsc::Receiver<Event>) {
    connect_with(port, cursor, false)
}

fn connect_with(
    port: u16,
    cursor: bool,
    gamepads: bool,
) -> (Session, Arc<FrameSlot>, mpsc::Receiver<Event>) {
    let frames = Arc::new(FrameSlot::default());
    let (tx, rx) = mpsc::channel();
    let session = Session::start(
        latch_stream::Server {
            address: IpAddr::V4(Ipv4Addr::LOCALHOST),
            port,
        },
        Settings {
            width: W,
            height: H,
            fps: FPS,
            bitrate_kbps: 8_000,
            hevc: false,
            av1: false,
            audio: false,
            name: "loopback test".into(),
            cursor,
            gamepads,
        },
        frames.clone(),
        tx,
        || {},
    );
    let start = Instant::now();
    loop {
        match rx.recv_timeout(Duration::from_millis(50)) {
            Ok(Event::Connected) => break,
            Ok(Event::Failed { stage, message }) => panic!("failed at {stage}: {message}"),
            Ok(_) => {}
            Err(_) => assert!(start.elapsed() < Duration::from_secs(10), "never connected"),
        }
    }
    (session, frames, rx)
}

/// The newest frame published after `after`, waiting up to `timeout`.
fn next_frame(frames: &FrameSlot, after: u64, timeout: Duration) -> Option<(u64, Frame)> {
    let start = Instant::now();
    while start.elapsed() < timeout {
        let seq = frames.seq();
        if seq > after {
            if let Some(mut f) = frames.take() {
                // A Mac's decoder hands over its own buffer; read it here.
                f.ensure_cpu_planes();
                return Some((seq, f));
            }
        }
        std::thread::sleep(Duration::from_millis(1));
    }
    None
}

fn luma_psnr(a: &Frame, reference: &[u8], width: usize, height: usize) -> f64 {
    let mut se = 0f64;
    for y in 0..height {
        for x in 0..width {
            let d = a.y[y * a.y_stride + x] as f64 - reference[y * width + x] as f64;
            se += d * d;
        }
    }
    let mse = se / (width * height) as f64;
    if mse == 0.0 {
        99.0
    } else {
        10.0 * (255.0f64 * 255.0 / mse).log10()
    }
}

/// The luma the pattern should arrive as: drawn, converted the way the
/// engine converts it.
fn reference_luma(motion: Motion) -> Vec<u8> {
    let mut pattern = Pattern::new(W, H, FPS, motion);
    let mut bgra = Bgra::default();
    pattern.grab(&mut bgra, Duration::from_millis(100)).unwrap();
    let mut yuv = latch_codec::Yuv420::new(W, H);
    latch_codec::color::bgra_to_yuv420(&bgra.data, W, H, bgra.stride, &mut yuv);
    let mut y = Vec::with_capacity((W * H) as usize);
    for row in 0..H as usize {
        y.extend_from_slice(&yuv.y[row * yuv.y_stride..row * yuv.y_stride + W as usize]);
    }
    y
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

static LOOPBACK_LOCK: Mutex<()> = Mutex::new(());

/// A test still running after this is stuck: every one here takes a few
/// seconds, and each of its own waits gives up well within this.
const WATCHDOG: Duration = Duration::from_secs(60);

/// One loopback session at a time, with a watchdog. Test mode feeds input
/// to the pattern the process opened last (see `testmode.rs`), so a second
/// session opening alongside takes the first one's key presses: its square
/// never flips. Past [`WATCHDOG`] the whole run fails, with every thread's
/// stack where the platform can print them, instead of hanging CI.
struct Exclusive {
    _lock: MutexGuard<'static, ()>,
    _armed: mpsc::Sender<()>,
}

fn exclusive(test: &'static str) -> Exclusive {
    // A test that failed holding it leaves nothing behind for the next.
    let lock = LOOPBACK_LOCK.lock().unwrap_or_else(PoisonError::into_inner);
    std::env::set_var("LATCH_TEST_PATTERN", "still");
    let (armed, disarmed) = mpsc::channel::<()>();
    std::thread::spawn(move || {
        if let Err(mpsc::RecvTimeoutError::Timeout) = disarmed.recv_timeout(WATCHDOG) {
            // Straight to stderr: the harness captures `eprintln!` here.
            let _ = writeln!(
                std::io::stderr(),
                "{test} has been running for over {WATCHDOG:?}; every thread's stack:"
            );
            print_stacks();
            std::process::exit(101);
        }
    });
    Exclusive {
        _lock: lock,
        _armed: armed,
    }
}

/// This process's stacks on stderr, where a tool for it is at hand.
fn print_stacks() {
    use std::process::Command;
    let pid = std::process::id().to_string();
    if cfg!(target_os = "macos") {
        let file = std::env::temp_dir().join(format!("loopback-{pid}.sample.txt"));
        let sampled = Command::new("sample")
            .args([pid.as_str(), "2", "-mayDie", "-file"])
            .arg(&file)
            .stdout(std::process::Stdio::null())
            .status();
        if let (Ok(_), Ok(text)) = (sampled, std::fs::read_to_string(&file)) {
            let _ = std::io::stderr().write_all(text.as_bytes());
        }
    } else if cfg!(target_os = "linux") {
        let _ = Command::new("gdb")
            .args(["-batch", "-p", pid.as_str(), "-ex", "thread apply all bt"])
            .status();
    }
}

#[test]
fn the_picture_arrives_and_keys_show_up_in_it() {
    let _exclusive = exclusive("the_picture_arrives_and_keys_show_up_in_it");
    let port = host();
    let (session, frames, events) = connect(port, false);
    let (mut seq, first) =
        next_frame(&frames, 0, Duration::from_secs(5)).expect("no frame arrived");
    assert_eq!((first.width, first.height), (W, H));
    let psnr = luma_psnr(
        &first,
        &reference_luma(Motion::Still),
        W as usize,
        H as usize,
    );
    eprintln!("first frame: luma PSNR {psnr:.1} dB against the drawn pattern");
    assert!(
        psnr > 30.0,
        "picture does not match what was drawn: {psnr:.1} dB"
    );
    let mut white = square_is_white(&first);
    frames.recycle(first);

    // Input to photon on the viewer's own clock: press a key, wait for the
    // square to flip in a decoded frame.
    let input = session.input();
    let mut times = Vec::new();
    for i in 0..20 {
        let pressed = Instant::now();
        input.key(0x41, true, 0);
        loop {
            // Frames keep coming while the square stays as it was: give up
            // on the press, not on each frame.
            let left = Duration::from_secs(2).saturating_sub(pressed.elapsed());
            let (s, f) = next_frame(&frames, seq, left)
                .unwrap_or_else(|| panic!("press {i}: the square never changed"));
            seq = s;
            let now = square_is_white(&f);
            frames.recycle(f);
            if now != white {
                white = now;
                times.push(pressed.elapsed());
                break;
            }
        }
        input.key(0x41, false, 0);
        std::thread::sleep(Duration::from_millis(30));
    }
    times.sort();
    let ms = |d: Duration| d.as_secs_f64() * 1000.0;
    eprintln!(
        "input to decoded picture over loopback, {} presses: min {:.1} ms, p50 {:.1} ms, p95 {:.1} ms, max {:.1} ms",
        times.len(),
        ms(times[0]),
        ms(times[times.len() / 2]),
        ms(times[times.len() * 95 / 100]),
        ms(times[times.len() - 1])
    );
    assert!(times[times.len() / 2] < Duration::from_millis(150));
    let stats = session.stats();
    eprintln!("{stats:?}");
    let (takeover, picture, _events) = connect(port, false);
    next_frame(&picture, 0, Duration::from_secs(5)).expect("takeover has no picture");
    let deadline = Instant::now() + Duration::from_secs(3);
    loop {
        if let Event::Terminated { code, .. } = events
            .recv_timeout(deadline.saturating_duration_since(Instant::now()))
            .expect("old viewer was not told about takeover")
        {
            assert_eq!(code, control::end::TAKEN_OVER as i32);
            break;
        }
    }
    takeover.stop();
    session.stop();
    let start = Instant::now();
    while !session.finished() {
        assert!(
            start.elapsed() < Duration::from_secs(5),
            "the session never ended"
        );
        std::thread::sleep(Duration::from_millis(10));
    }
}

#[test]
fn local_cursor_negotiated_and_host_omits_pointer_when_asked() {
    let _exclusive = exclusive("local_cursor_negotiated_and_host_omits_pointer_when_asked");
    let port = host();
    let (session, frames, _events) = connect(port, true);

    // 1. Check cursor negotiated
    assert!(
        session.cursor_supported(),
        "cursor support must be negotiated with host"
    );

    // 2. Wait for Cursor shape to arrive
    let start = Instant::now();
    let shape = loop {
        if let Some(cursor_state) = session.cursor() {
            assert!(cursor_state.visible);
            if let Some(shape) = cursor_state.shape {
                break shape;
            }
        }
        assert!(
            start.elapsed() < Duration::from_secs(5),
            "cursor shape did not arrive"
        );
        std::thread::sleep(Duration::from_millis(10));
    };

    assert_eq!(shape.width, 16);
    assert_eq!(shape.height, 16);
    assert_eq!(shape.hot_x, 0);
    assert_eq!(shape.hot_y, 0);

    // 3. Since cursor: true, host starts with draw_cursor: false.
    // Receive a frame with cursor omitted.
    let (seq1, frame_omitted) = next_frame(&frames, 0, Duration::from_secs(5))
        .expect("frame without cursor did not arrive");
    let luma_omitted = frame_omitted.y.clone();
    frames.recycle(frame_omitted);

    // 4. Request host to draw cursor
    session.draw_cursor(true);

    // Wait for frame with cursor drawn (at synthetic position 100, 100)
    let mut seq = seq1;
    let start = Instant::now();
    let mut cursor_drawn_seen = false;
    while start.elapsed() < Duration::from_secs(3) {
        if let Some((s, f)) = next_frame(&frames, seq, Duration::from_millis(100)) {
            seq = s;
            // Compare around (100, 100) where synthetic arrow is composited
            let mut diff = 0u64;
            for y in 95..125 {
                for x in 95..125 {
                    let idx = y * f.y_stride + x;
                    if idx < f.y.len() && idx < luma_omitted.len() {
                        let d = f.y[idx].abs_diff(luma_omitted[idx]);
                        diff += d as u64;
                    }
                }
            }
            frames.recycle(f);
            if diff > 100 {
                cursor_drawn_seen = true;
                break;
            }
        }
    }
    assert!(
        cursor_drawn_seen,
        "host did not composite cursor when draw_cursor(true)"
    );

    // 5. Request host to omit cursor again
    session.draw_cursor(false);

    let start = Instant::now();
    let mut cursor_omitted_again = false;
    while start.elapsed() < Duration::from_secs(3) {
        if let Some((s, f)) = next_frame(&frames, seq, Duration::from_millis(100)) {
            seq = s;
            let mut diff = 0u64;
            for y in 95..125 {
                for x in 95..125 {
                    let idx = y * f.y_stride + x;
                    if idx < f.y.len() && idx < luma_omitted.len() {
                        let d = f.y[idx].abs_diff(luma_omitted[idx]);
                        diff += d as u64;
                    }
                }
            }
            frames.recycle(f);
            if diff < 50 {
                cursor_omitted_again = true;
                break;
            }
        }
    }
    assert!(
        cursor_omitted_again,
        "host did not omit cursor when draw_cursor(false)"
    );

    session.stop();
    let start = Instant::now();
    while !session.finished() {
        assert!(
            start.elapsed() < Duration::from_secs(5),
            "the session never ended"
        );
        std::thread::sleep(Duration::from_millis(10));
    }
    std::thread::sleep(Duration::from_millis(300));
}

#[test]
fn gamepads_plug_in_follow_rumble_back_and_unplug() {
    use latch_proto::gamepad::{button, Feedback, PadKind, PadState};
    let _exclusive = exclusive("gamepads_plug_in_follow_rumble_back_and_unplug");
    let port = host();
    let (session, _frames, _events) = connect_with(port, false, true);
    let input = session.input();
    assert_eq!(input.pad_slots(), 16);
    let (rumble_tx, rumble) = mpsc::channel::<(u8, Feedback)>();
    let rumble_tx = std::sync::Mutex::new(rumble_tx);
    session.on_pad_feedback(move |slot, f| {
        let _ = rumble_tx.lock().unwrap().send((slot, f));
    });
    let wait_for = |what: &dyn Fn() -> bool| {
        let start = Instant::now();
        while !what() {
            assert!(start.elapsed() < Duration::from_secs(5), "timed out");
            std::thread::sleep(Duration::from_millis(1));
        }
        start.elapsed()
    };
    let pulled = PadState {
        buttons: button::SOUTH | button::DPAD_LEFT,
        left_x: -20_000,
        right_trigger: 40_000,
        left_trigger: 1_000,
        ..Default::default()
    };
    input.pad(2, PadKind::PlayStation, pulled);
    input.pad(0, PadKind::Xbox, PadState::default());
    let arrived = wait_for(&|| {
        let pads = latch_engine::testmode::pads();
        pads[2] == Some((PadKind::PlayStation, pulled)) && pads[0].is_some()
    });
    // The test host rumbles back what the triggers say.
    let start = Instant::now();
    let (slot, f) = loop {
        let (slot, f) = rumble.recv_timeout(Duration::from_secs(5)).unwrap();
        if slot == 2 {
            break (slot, f);
        }
    };
    assert_eq!((slot, f.large, f.small), (2, 40_000, 1_000));
    let round_trip = arrived + start.elapsed();
    eprintln!("pad state arrived in {arrived:?}; rumble back after {round_trip:?}");
    // A newer state replaces it; then the pad goes.
    let released = PadState::default();
    input.pad(2, PadKind::PlayStation, released);
    wait_for(&|| latch_engine::testmode::pads()[2] == Some((PadKind::PlayStation, released)));
    input.pad_gone(2);
    wait_for(&|| latch_engine::testmode::pads()[2].is_none());
    // Ending the session unplugs what is left.
    session.stop();
    wait_for(&|| session.finished());
    wait_for(&|| latch_engine::testmode::pads().iter().all(Option::is_none));
}
