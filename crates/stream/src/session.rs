//! One stream from a Latch host (`docs/PROTOCOL.md`): asks the host's
//! control port for a session, then receives video and audio over UDP,
//! decodes them, and sends input. Each session owns its sockets and
//! threads; nothing is global, so a session can start while the last one
//! is still letting go.
//!
//! Threads: the session's own thread runs the TCP control messages and
//! the timers that end a dead session; a receive thread takes every
//! datagram, puts frames together, plays audio and resends input; a decode
//! thread decodes frames in order and publishes the newest to the renderer.

use crate::audio::Player;
use crate::video::{self, Decoder, FrameSlot};
use latch_proto::audio::{AudioOut, AudioReceiver};
use latch_proto::control::{self, Accept, Codec, Message, Report, StreamRequest};
use latch_proto::gamepad::{Feedback, LatestReceiver, PadKind, PadSender, PadState};
use latch_proto::input::{Event as InputEvent, InputSender};
use latch_proto::rtt::RttEstimator;
use latch_proto::video::{InFrame, KeyframeRequests, Reassembled, Reassembler, ReassemblyStats};
use latch_proto::wire::{self, Datagram};
use parking_lot::{Condvar, Mutex};
use std::collections::VecDeque;
use std::io::Write;
use std::net::{IpAddr, Shutdown, SocketAddr, TcpStream, UdpSocket};
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::mpsc::{self, Sender};
use std::sync::Arc;
use std::time::{Duration, Instant};

/// Includes takeover (up to 30 s), this viewer's prep (15 s), and capture open.
const START_TIMEOUT: Duration = Duration::from_secs(60);
/// After the host accepts, its first frame must arrive within this.
const MEDIA_TIMEOUT: Duration = Duration::from_secs(8);
/// A host heard from neither over TCP nor UDP for this long is gone.
const SILENT: Duration = Duration::from_secs(10);
const KEEPALIVE: Duration = Duration::from_secs(1);
const HELLO_FAST: Duration = Duration::from_millis(50);
const HELLO_SLOW: Duration = Duration::from_secs(1);
const PING_EVERY: Duration = Duration::from_millis(500);
/// Complete frames waiting for the decoder. Past this the decoder cannot
/// keep up: the queue is dropped and the stream restarts at a keyframe,
/// rather than the picture falling further and further behind.
const DECODE_QUEUE: usize = 8;
const DECODE_MAX_AGE: Duration = Duration::from_millis(50);
const REPORT_EVERY: Duration = Duration::from_millis(50);
const STATS_EVERY: Duration = Duration::from_millis(250);
/// How long a stopping session waits for a decode in progress. A platform
/// decoder can block inside the OS (VideoToolbox after a sleep or a GPU
/// reset); the window must still be able to leave the stream.
const DECODER_STOP: Duration = Duration::from_secs(1);

#[derive(Debug, Clone, PartialEq)]
pub enum Event {
    /// A connection stage is starting.
    Stage(String),
    Connected,
    /// Setup failed before the stream started.
    Failed {
        stage: String,
        message: String,
    },
    /// The stream ended after it started. `code` 0 means the host or this
    /// side ended it on purpose; the others are `latch_proto::control::end`
    /// codes, or -1 when the connection broke.
    Terminated {
        code: i32,
        message: String,
    },
    /// The network is struggling (`true`) or fine again (`false`).
    Poor(bool),
    /// This machine cannot play the stream's sound; the reason.
    NoAudio(String),
    /// Something the host wants the person to know (it cannot inject
    /// input, say).
    Notice(String),
}

/// Where the host is.
#[derive(Debug, Clone)]
pub struct Server {
    pub address: IpAddr,
    /// The host's control port, which is also its media port.
    pub port: u16,
}

#[derive(Debug, Clone)]
pub struct Settings {
    pub color: latch_proto::color::ColorCapabilities,
    pub width: u32,
    pub height: u32,
    pub fps: u32,
    pub bitrate_kbps: u32,
    /// Ask for HEVC when both sides can; otherwise H.264.
    pub hevc: bool,
    /// Ask for AV1 when both sides can.
    pub av1: bool,
    pub audio: bool,
    /// This machine's name, for the host's log.
    pub name: String,
    /// Ask the host to stream cursor shapes and omit the pointer from video.
    pub cursor: bool,
    /// Offer gamepads.
    pub gamepads: bool,
}

/// Where a session sends the host's rumble and lights: slot and feedback.
pub type FeedbackFn = Box<dyn Fn(u8, Feedback) + Send + Sync>;

#[derive(Debug, Clone, Default)]
pub struct CursorState {
    pub visible: bool,
    pub shape: Option<Arc<latch_proto::control::CursorShape>>,
}

#[derive(Debug, Clone, Default)]
pub struct Stats {
    pub color: latch_proto::color::ColorDescription,
    pub fps: f32,
    /// Encoded video in the frames that came together, in Mbps: what the
    /// bitrate setting caps. FEC parity, headers and audio are not in it.
    pub mbps: f32,
    /// Every video and audio datagram the host's media brought, FEC parity
    /// and Latch headers included, in Mbps: what the network delivered.
    pub received_mbps: f32,
    pub rtt_ms: u32,
    pub rtt_var_ms: u32,
    pub decode_ms: f32,
    /// Capture to send on the host.
    pub host_ms: f32,
    /// First shard to whole frame.
    pub assembly_ms: f32,
    /// Whole frame to the decoder starting on it.
    pub queue_ms: f32,
    pub width: u32,
    pub height: u32,
    pub decoder: &'static str,
    /// The host's encoder, in its own words.
    pub encoder: String,
    pub codec: &'static str,
    /// Share of video packets in the last second that arrived too late or
    /// not at all and could not be rebuilt from FEC.
    pub loss_pct: f32,
    /// Packets FEC did rebuild in the last second.
    pub fec_recovered: u32,
    /// Where sound goes ("audio → MacBook Pro Speakers"), or why it does
    /// not; empty until the first second of video.
    pub audio: String,
    /// Persistent video diagnosis, even while the transport is connected.
    pub video_problem: Option<String>,
    pub applied_bitrate_kbps: u32,
    pub requested_bitrate_kbps: u32,
    pub pacing_queue_ms: f32,
    pub send_errors: u64,
    pub send_retries: u64,
    pub socket_send_bytes: usize,
    pub socket_receive_bytes: usize,
    pub host_socket_send_bytes: usize,
    pub host_socket_receive_bytes: usize,
    pub frames_lost: u64,
    pub window_ms: u32,
    /// Whole frames put together a second, before the decoder: what the
    /// network brought, where [`Stats::fps`] is what was decoded.
    pub received_fps: f32,
    /// Share of video packets in the last second the network lost,
    /// whether FEC rebuilt them or not; [`Stats::loss_pct`] is what was
    /// left after FEC.
    pub raw_loss_pct: f32,
    /// Frames that arrived whole in the last second but were never
    /// decoded: thrown away while the decoder caught up or while the
    /// stream waited for a keyframe.
    pub frames_dropped: u64,
    /// How unevenly whole frames arrive: the mean distance of each gap
    /// between two from the mean gap, in ms.
    pub frame_jitter_ms: f32,
}

#[derive(Default)]
struct VideoHealth {
    connected_at: Option<Instant>,
    last_frame: Option<Instant>,
    black_since: Option<Instant>,
    last_error: Option<String>,
}

impl VideoHealth {
    fn frame(&mut self, now: Instant, black: bool) {
        self.last_frame = Some(now);
        self.last_error = None;
        if black {
            self.black_since.get_or_insert(now);
        } else {
            self.black_since = None;
        }
    }

    fn problem(&self, now: Instant) -> Option<String> {
        let connected = self.connected_at?;
        if now.duration_since(self.last_frame.unwrap_or(connected)) >= Duration::from_secs(5) {
            return Some(if let Some(error) = &self.last_error {
                format!("Video cannot be decoded: {error}. Try restarting the stream or selecting H.264 in Settings.")
            } else if self.last_frame.is_some() {
                "Video stopped arriving from the PC. Try restarting the stream or lowering Quality."
                    .into()
            } else {
                "Connected, but no video has arrived from the PC. Check its display, or restart the stream.".into()
            });
        }
        if self
            .black_since
            .is_some_and(|since| now.duration_since(since) >= Duration::from_secs(5))
        {
            return Some("The PC is sending a black picture: the connection and the video are healthy, but every frame is blank. Asking the PC why…".into());
        }
        None
    }
}

/// What the decode thread adds up between two stats updates.
#[derive(Default)]
struct DecodeWindow {
    frames: u32,
    decode_us: u64,
    queue_us: u64,
    host_tenths: u64,
    assembly_us: u64,
}

/// What input needs to reach the host.
struct Link {
    udp: UdpSocket,
    host: SocketAddr,
    sender: InputSender,
    pads: PadSender,
    buf: Vec<u8>,
}

impl Link {
    fn flush(&mut self, now: Instant) {
        if self.sender.poll(now, &mut self.buf) {
            let _ = self.udp.send_to(&self.buf, self.host);
        }
        if self.pads.poll(now, &mut self.buf) {
            let _ = self.udp.send_to(&self.buf, self.host);
        }
    }

    fn next_deadline(&self) -> Option<Instant> {
        self.sender
            .next_deadline()
            .into_iter()
            .chain(self.pads.next_deadline())
            .min()
    }
}

struct Inner {
    events: Sender<Event>,
    wake: Box<dyn Fn() + Send + Sync>,
    frames: Arc<FrameSlot>,
    stats: Mutex<Stats>,
    /// Persists across the receive thread's periodic player description.
    host_audio_problem: Mutex<String>,
    video_health: Mutex<VideoHealth>,
    decode: Mutex<DecodeWindow>,
    link: Mutex<Option<Link>>,
    /// Frames waiting for the decoder, and whether it must skip to a
    /// keyframe.
    queue: Mutex<DecodeQueue>,
    queue_cv: Condvar,
    /// The TCP connection, so `stop` can unblock its reader.
    tcp: Mutex<Option<TcpStream>>,
    epoch: Instant,
    /// When anything last came from the host, µs since `epoch`.
    heard_us: AtomicU64,
    /// Video packets the network lost in the last second, rebuilt or not,
    /// in thousandths of a percent: what the host sizes its FEC by.
    feedback: Mutex<Report>,
    /// When the decoder last published a picture, µs since `epoch`.
    published_us: AtomicU64,
    /// Frames thrown away before decoding since the last stats window.
    dropped: AtomicU64,
    connected: AtomicBool,
    stop: AtomicBool,
    finished: AtomicBool,
    audio_gain: Arc<AtomicU32>,
    done: Mutex<bool>,
    done_cv: Condvar,
    cursor_supported: AtomicBool,
    cursor_state: Mutex<Option<CursorState>>,
    control_writer: Mutex<Option<TcpStream>>,
    /// Gamepad slots the host takes; 0 until it accepts, or if it takes none.
    pad_slots: AtomicU32,
    pad_problem: Mutex<String>,
    pad_feedback: Mutex<Option<FeedbackFn>>,
    /// Pads' newest states from before the host accepted, sent once it has.
    /// Locked after `link`, never before.
    pending_pads: Mutex<[Option<(PadKind, PadState)>; latch_proto::gamepad::MAX_PADS]>,
}

#[derive(Default)]
struct DecodeQueue {
    frames: VecDeque<(InFrame, Instant)>,
    want_keyframe: bool,
}

impl Inner {
    fn emit(&self, e: Event) {
        let _ = self.events.send(e);
        (self.wake)();
    }

    fn now_us(&self) -> u64 {
        self.epoch.elapsed().as_micros() as u64
    }

    fn heard(&self) {
        self.heard_us.store(self.now_us(), Ordering::Relaxed);
    }

    fn stopping(&self) -> bool {
        self.stop.load(Ordering::Acquire)
    }

    fn finish(&self) {
        self.finished.store(true, Ordering::Release);
        *self.done.lock() = true;
        self.done_cv.notify_all();
        (self.wake)();
    }
}

pub struct Session {
    inner: Arc<Inner>,
}

/// The keyboard-and-mouse side of a session, cheap to clone and safe to
/// use from any thread: a worker that has to type on the PC after a
/// network round trip holds one of these instead of the session.
#[derive(Clone)]
pub struct Input {
    inner: Arc<Inner>,
}

impl Input {
    pub fn connected(&self) -> bool {
        self.inner.connected.load(Ordering::Acquire)
    }

    fn send(&self, event: InputEvent) {
        if !self.connected() {
            return;
        }
        if let Some(link) = self.inner.link.lock().as_mut() {
            link.sender.push(event);
            link.flush(Instant::now());
        }
    }

    pub fn mouse_move(&self, dx: i16, dy: i16) {
        if dx != 0 || dy != 0 {
            self.send(InputEvent::MouseMove { dx, dy });
        }
    }

    /// `x, y` in a `width × height` picture.
    pub fn mouse_position(&self, x: i16, y: i16, width: i16, height: i16) {
        let clamp = |v: i16| v.max(0) as u16;
        self.send(InputEvent::MousePosition {
            x: clamp(x),
            y: clamp(y),
            width: clamp(width).max(1),
            height: clamp(height).max(1),
        });
    }

    /// `button` is one of `crate::input::BUTTON_*`.
    pub fn mouse_button(&self, button: u8, down: bool) {
        self.send(InputEvent::MouseButton { button, down });
    }

    /// `vk` is a Windows virtual-key code, `modifiers` the
    /// `crate::input::MOD_*` bits held.
    pub fn key(&self, vk: i16, down: bool, modifiers: u8) {
        self.send(InputEvent::Key {
            vk: vk as u16,
            down,
            modifiers,
        });
    }

    pub fn text(&self, text: &str) {
        if !text.is_empty() {
            self.send(InputEvent::Text(text.to_string()));
        }
    }

    /// 120 per notch; up and right positive.
    pub fn scroll(&self, vertical: i16, horizontal: i16) {
        if vertical != 0 || horizontal != 0 {
            self.send(InputEvent::Scroll {
                vertical,
                horizontal,
            });
        }
    }

    /// Lets go of every key and button the host holds for this viewer.
    pub fn release_all(&self) {
        self.send(InputEvent::ReleaseAll);
    }

    /// Gamepad slots the host takes (0 while connecting, or when it takes
    /// none).
    pub fn pad_slots(&self) -> u8 {
        self.inner.pad_slots.load(Ordering::Acquire) as u8
    }

    /// The pad in `slot` looks like this now. Sent at once, then again until
    /// the host has it; before the host has accepted, kept until it has. A
    /// slot the host does not take is ignored.
    pub fn pad(&self, slot: u8, kind: PadKind, state: PadState) {
        self.set_pad(slot, Some((kind, state)));
    }

    /// The pad in `slot` went away.
    pub fn pad_gone(&self, slot: u8) {
        self.set_pad(slot, None);
    }

    fn set_pad(&self, slot: u8, pad: Option<(PadKind, PadState)>) {
        let mut link = self.inner.link.lock();
        let Some(link) = link.as_mut() else {
            if let Some(p) = self.inner.pending_pads.lock().get_mut(slot as usize) {
                *p = pad;
            }
            return;
        };
        if slot >= self.pad_slots() {
            return;
        }
        match pad {
            Some((kind, state)) => link.pads.update(slot, kind, state),
            None => link.pads.remove(slot),
        }
        link.flush(Instant::now());
    }
}

impl Session {
    pub fn start(
        server: Server,
        settings: Settings,
        frames: Arc<FrameSlot>,
        events: Sender<Event>,
        wake: impl Fn() + Send + Sync + 'static,
    ) -> Self {
        let inner = Arc::new(Inner {
            events,
            wake: Box::new(wake),
            frames,
            stats: Mutex::new(Stats::default()),
            host_audio_problem: Mutex::new(String::new()),
            video_health: Mutex::new(VideoHealth::default()),
            decode: Mutex::new(DecodeWindow::default()),
            link: Mutex::new(None),
            queue: Mutex::new(DecodeQueue::default()),
            queue_cv: Condvar::new(),
            tcp: Mutex::new(None),
            epoch: Instant::now(),
            heard_us: AtomicU64::new(0),
            feedback: Mutex::new(Report::default()),
            published_us: AtomicU64::new(0),
            dropped: AtomicU64::new(0),
            connected: AtomicBool::new(false),
            stop: AtomicBool::new(false),
            finished: AtomicBool::new(false),
            audio_gain: Arc::new(AtomicU32::new(1.0f32.to_bits())),
            done: Mutex::new(false),
            done_cv: Condvar::new(),
            cursor_supported: AtomicBool::new(false),
            cursor_state: Mutex::new(None),
            control_writer: Mutex::new(None),
            pad_slots: AtomicU32::new(0),
            pad_problem: Mutex::new(String::new()),
            pad_feedback: Mutex::new(None),
            pending_pads: Mutex::new([None; latch_proto::gamepad::MAX_PADS]),
        });
        let worker = inner.clone();
        let spawned = std::thread::Builder::new()
            .name("stream".into())
            .spawn(move || {
                run(&worker, &server, &settings);
                worker.finish();
            });
        if let Err(e) = spawned {
            inner.emit(Event::Failed {
                stage: "starting".into(),
                message: format!("could not start the stream thread: {e}"),
            });
            inner.finish();
        }
        Self { inner }
    }

    pub fn connected(&self) -> bool {
        self.inner.connected.load(Ordering::Acquire)
    }

    /// The session has ended and let go of everything.
    pub fn finished(&self) -> bool {
        self.inner.finished.load(Ordering::Acquire)
    }

    /// Whether the host agreed to stream cursor shapes and omit the host pointer.
    pub fn cursor_supported(&self) -> bool {
        self.inner.cursor_supported.load(Ordering::Acquire)
    }

    /// The latest cursor shape and visibility received from the host, or None
    /// if cursor shapes are not supported or none have arrived yet.
    pub fn cursor(&self) -> Option<CursorState> {
        if !self.inner.cursor_supported.load(Ordering::Acquire) {
            return None;
        }
        self.inner.cursor_state.lock().clone()
    }

    /// Tell the host whether to draw the cursor into its video stream.
    pub fn draw_cursor(&self, draw: bool) {
        if let Some(w) = self.inner.control_writer.lock().as_mut() {
            let _ = control::write_message(
                w,
                &Message::DrawCursor(latch_proto::control::DrawCursor { draw }),
            );
        }
    }

    /// Ends the session; returns at once. [`Session::finished`] says when
    /// it is over.
    pub fn stop(&self) {
        self.inner.stop.store(true, Ordering::Release);
        if let Some(tcp) = self.inner.tcp.lock().as_ref() {
            let _ = tcp.shutdown(Shutdown::Both);
        }
        self.inner.queue_cv.notify_all();
    }

    pub fn stats(&self) -> Stats {
        let mut stats = self.inner.stats.lock().clone();
        stats.video_problem = self.inner.video_health.lock().problem(Instant::now());
        stats
    }

    /// When the newest picture was published to the [`FrameSlot`]: the
    /// renderer takes how long it waited from this.
    pub fn published_at(&self) -> Option<Instant> {
        match self.inner.published_us.load(Ordering::Relaxed) {
            0 => None,
            us => Some(self.inner.epoch + Duration::from_micros(us)),
        }
    }

    pub fn set_volume(&self, gain: f32) {
        self.inner
            .audio_gain
            .store(gain.clamp(0.0, 2.0).to_bits(), Ordering::Relaxed);
    }

    pub fn volume(&self) -> f32 {
        f32::from_bits(self.inner.audio_gain.load(Ordering::Relaxed))
    }

    pub fn input(&self) -> Input {
        Input {
            inner: self.inner.clone(),
        }
    }

    /// Why the host takes no gamepads, once it has said so.
    pub fn pad_problem(&self) -> String {
        self.inner.pad_problem.lock().clone()
    }

    /// Where the host's rumble and lights go, from the receive thread.
    pub fn on_pad_feedback(&self, f: impl Fn(u8, Feedback) + Send + Sync + 'static) {
        *self.inner.pad_feedback.lock() = Some(Box::new(f));
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        self.stop();
    }
}

/// The codecs to offer, best first.
fn offer(settings: &Settings) -> Vec<Codec> {
    video::decodable()
        .into_iter()
        .filter(|&c| (settings.hevc || c != Codec::Hevc) && (settings.av1 || c != Codec::Av1))
        .collect()
}

/// The sentence in a refusal's `Ack` body, or the status.
fn refusal(status: u16, body: &str) -> String {
    serde_json::from_str::<serde_json::Value>(body)
        .ok()
        .and_then(|v| v.get("error").and_then(|e| e.as_str()).map(str::to_string))
        .filter(|e| !e.is_empty())
        .unwrap_or_else(|| format!("the host answered HTTP {status}"))
}

fn run(inner: &Arc<Inner>, server: &Server, settings: &Settings) {
    if inner.stopping() {
        return;
    }
    let fail = |stage: &str, message: String| {
        if !inner.stopping() {
            inner.emit(Event::Failed {
                stage: stage.into(),
                message,
            });
        }
    };
    inner.emit(Event::Stage("Asking for the stream…".into()));
    let bind: SocketAddr = if server.address.is_ipv6() {
        "[::]:0".parse().expect("address")
    } else {
        "0.0.0.0:0".parse().expect("address")
    };
    let udp = match UdpSocket::bind(bind) {
        Ok(u) => u,
        Err(e) => return fail("connecting", format!("no UDP socket for the picture: {e}")),
    };
    match latch_proto::transport::configure_socket(&udp, 8 * 1024 * 1024, 8 * 1024 * 1024) {
        Ok(buffers) => {
            tracing::info!(send_bytes = buffers.send, receive_bytes = buffers.receive, "viewer UDP buffers (requested 8388608/8388608; Linux may cap at net.core.wmem_max/rmem_max)");
            let mut stats = inner.stats.lock();
            stats.socket_send_bytes = buffers.send;
            stats.socket_receive_bytes = buffers.receive;
        }
        Err(e) => return fail("connecting", format!("cannot configure media buffers: {e}")),
    }
    let udp_port = udp.local_addr().map(|a| a.port()).unwrap_or(0);
    let codecs = offer(settings);
    let request = StreamRequest {
        color: settings.color,
        protocol: latch_proto::PROTOCOL,
        width: settings.width,
        height: settings.height,
        fps: settings.fps,
        bitrate_kbps: settings.bitrate_kbps,
        codecs: codecs.clone(),
        udp_port,
        audio: settings.audio,
        name: settings.name.clone(),
        cursor: settings.cursor,
        gamepads: settings.gamepads,
    };
    let body = serde_json::to_vec(&request).unwrap_or_default();
    let host = SocketAddr::new(server.address, server.port);
    let mut tcp = match latch_core::http::upgrade_cancellable(
        host,
        control::PATH,
        control::UPGRADE,
        &body,
        START_TIMEOUT,
        |stream| {
            *inner.tcp.lock() = Some(stream.try_clone()?);
            Ok(())
        },
        || inner.stopping(),
    ) {
        Ok(Ok(tcp)) => tcp,
        Ok(Err(reply)) => {
            return fail(
                "starting",
                if reply.status == 404 {
                    "the host runs a Latch older than 5.0 and needs updating".into()
                } else {
                    refusal(reply.status, &reply.body)
                },
            )
        }
        Err(e) => {
            if inner.stopping() {
                return;
            }
            return fail("connecting", format!("{e:#}"));
        }
    };
    let _ = tcp.set_nodelay(true);
    let _ = tcp.set_read_timeout(Some(START_TIMEOUT));
    *inner.tcp.lock() = tcp.try_clone().ok();
    *inner.control_writer.lock() = tcp.try_clone().ok();
    if inner.stopping() {
        let _ = tcp.shutdown(Shutdown::Both);
        return;
    }
    let accept = match control::read_message(&mut tcp) {
        Ok(Message::Accept(a)) => a,
        Ok(Message::End(e)) => return fail("starting", e.message),
        Ok(other) => return fail("starting", format!("the host said {other:?} first")),
        Err(e) => {
            return fail(
                "starting",
                format!("the host did not start the stream: {e}"),
            )
        }
    };
    inner
        .cursor_supported
        .store(accept.cursor, Ordering::Release);
    let pad_slots = accept.gamepads.min(latch_proto::gamepad::MAX_PADS as u8);
    inner.pad_slots.store(
        pad_slots as u32 * settings.gamepads as u32,
        Ordering::Release,
    );
    if settings.gamepads && pad_slots == 0 {
        *inner.pad_problem.lock() = if accept.gamepad_problem.is_empty() {
            "the host takes no gamepads".into()
        } else {
            accept.gamepad_problem.clone()
        };
    }
    let Some(token) = control::token_from_hex(&accept.token) else {
        return fail("starting", "the host sent a malformed session token".into());
    };
    if !codecs.contains(&accept.codec) {
        return fail(
            "starting",
            format!("the host chose {:?}, which was not offered", accept.codec),
        );
    }
    if !accept.color.supported()
        || (accept.color.bit_depth == 10 && !settings.color.ten_bit)
        || (accept.color.is_hdr() && (!settings.color.hdr || settings.color.peak_nits == 0))
    {
        return fail("connecting", "host selected unoffered colour format".into());
    }
    let decoder = match video::new_decoder_color(accept.codec, accept.color) {
        Ok(d) => d,
        Err(e) => return fail("decoding", format!("{e:#}")),
    };
    {
        let mut stats = inner.stats.lock();
        stats.width = accept.width;
        stats.height = accept.height;
        stats.decoder = decoder.name();
        stats.encoder = accept.encoder.clone();
        stats.codec = accept.codec.name();
        stats.color = accept.color;
        if accept.audio.is_none() && settings.audio {
            stats.audio = if accept.audio_problem.is_empty() {
                "no sound from the host".into()
            } else {
                format!("no sound from the host: {}", accept.audio_problem)
            };
        }
    }
    let player = accept.audio.as_ref().and_then(|a| {
        match Player::with_gain(
            a.rate,
            a.channels as usize,
            a.frame_samples as usize,
            inner.audio_gain.clone(),
        ) {
            Ok(p) => Some(p),
            Err(e) => {
                inner.emit(Event::NoAudio(format!("{e:#}")));
                None
            }
        }
    });
    let media = SocketAddr::new(server.address, server.port);
    let send_udp = match udp.try_clone() {
        Ok(u) => u,
        Err(e) => return fail("connecting", format!("{e}")),
    };
    {
        let mut link = Link {
            udp: send_udp,
            host: media,
            sender: InputSender::new(accept.session),
            pads: PadSender::new(accept.session),
            buf: Vec::with_capacity(latch_proto::MAX_DATAGRAM),
        };
        let taken = inner.pad_slots.load(Ordering::Acquire);
        let mut guard = inner.link.lock();
        let pending = std::mem::replace(
            &mut *inner.pending_pads.lock(),
            [None; latch_proto::gamepad::MAX_PADS],
        );
        for (slot, pad) in pending.into_iter().enumerate() {
            if let Some((kind, state)) = pad.filter(|_| (slot as u32) < taken) {
                link.pads.update(slot as u8, kind, state);
            }
        }
        link.flush(Instant::now());
        *guard = Some(link);
    }
    inner.heard();
    let receiver = {
        let inner = inner.clone();
        let accept = accept.clone();
        std::thread::Builder::new()
            .name("stream-receive".into())
            .spawn(move || receive(&inner, udp, media, &accept, token, player))
    };
    let decoding = {
        let inner = inner.clone();
        Worker::spawn("stream-decode", move || {
            decode_color(&inner, decoder, accept.color)
        })
    };
    let outcome = control_loop(inner, &mut tcp, &accept);
    inner.stop.store(true, Ordering::Release);
    inner.connected.store(false, Ordering::Release);
    inner.queue_cv.notify_all();
    let _ = tcp.shutdown(Shutdown::Both);
    // After the host's FIN macOS refuses that shutdown: only closing every
    // handle sends ours, which lets the host finish without waiting.
    *inner.control_writer.lock() = None;
    inner.tcp.lock().take();
    drop(tcp);
    if let Ok(r) = receiver {
        let _ = r.join();
    }
    if let Ok(d) = decoding {
        if !d.join_within(DECODER_STOP) {
            tracing::warn!("the decoder did not return within {DECODER_STOP:?}; leaving it behind");
        }
    }
    *inner.link.lock() = None;
    match outcome {
        Outcome::Stopped => {}
        Outcome::Failed(stage, message) => inner.emit(Event::Failed { stage, message }),
        Outcome::Ended(code, message) => inner.emit(Event::Terminated { code, message }),
    }
}

/// A thread that can be waited for with a time limit.
struct Worker {
    handle: std::thread::JoinHandle<()>,
    exited: mpsc::Receiver<()>,
}

impl Worker {
    fn spawn(name: &str, f: impl FnOnce() + Send + 'static) -> std::io::Result<Self> {
        let (alive, exited) = mpsc::channel();
        let handle = std::thread::Builder::new()
            .name(name.into())
            .spawn(move || {
                // Dropped when `f` returns or panics.
                let _alive: Sender<()> = alive;
                f()
            })?;
        Ok(Self { handle, exited })
    }

    /// Joins the thread if it ends within `limit`; otherwise leaves it to
    /// end by itself and returns false.
    fn join_within(self, limit: Duration) -> bool {
        if let Err(mpsc::RecvTimeoutError::Timeout) = self.exited.recv_timeout(limit) {
            return false;
        }
        let _ = self.handle.join();
        true
    }
}

enum Outcome {
    /// This side stopped.
    Stopped,
    /// Never got going.
    Failed(String, String),
    /// Ended after it started.
    Ended(i32, String),
}

/// The control connection: keepalives and reports out, notices and the end
/// in; and the clock that gives up on a silent host.
fn send_control(inner: &Inner, message: &Message) -> bool {
    inner.control_writer.lock().as_mut().is_some_and(|writer| {
        control::write_message(writer, message).is_ok() && writer.flush().is_ok()
    })
}

fn control_loop(inner: &Arc<Inner>, tcp: &mut TcpStream, accept: &Accept) -> Outcome {
    let (messages, inbox) = mpsc::channel();
    if let Ok(mut reader) = tcp.try_clone() {
        let _ = reader.set_read_timeout(None);
        let _ = std::thread::Builder::new()
            .name("stream-control".into())
            .spawn(move || loop {
                match control::read_message(&mut reader) {
                    Ok(m) => {
                        if messages.send(m).is_err() {
                            return;
                        }
                    }
                    Err(_) => return,
                }
            });
    }
    let accepted = Instant::now();
    let mut last_report = Instant::now();
    let mut last_keepalive = Instant::now();
    loop {
        if inner.stopping() {
            if let Some(w) = inner.control_writer.lock().as_mut() {
                let _ = control::write_message(
                    w,
                    &Message::End(control::End {
                        code: control::end::ENDED,
                        message: String::new(),
                    }),
                );
            }
            *inner.control_writer.lock() = None;
            return Outcome::Stopped;
        }
        match inbox.recv_timeout(Duration::from_millis(25)) {
            Ok(Message::End(e)) => {
                let message = if e.message.is_empty() {
                    "The host ended the stream.".to_string()
                } else {
                    e.message
                };
                return if inner.connected.load(Ordering::Acquire) {
                    Outcome::Ended(e.code as i32, message)
                } else {
                    Outcome::Failed("starting".into(), message)
                };
            }
            Ok(Message::Report(report)) => {
                inner.heard();
                let mut stats = inner.stats.lock();
                stats.applied_bitrate_kbps = report.applied_bitrate_kbps;
                stats.requested_bitrate_kbps = report.requested_bitrate_kbps;
                stats.pacing_queue_ms = report.pacing_queue_ms;
                stats.send_errors = report.send_errors;
                stats.send_retries = report.send_retries;
                stats.host_socket_send_bytes = report.socket_send_bytes;
                stats.host_socket_receive_bytes = report.socket_receive_bytes;
            }
            Ok(Message::Cursor(c)) => {
                inner.heard();
                let mut state = inner.cursor_state.lock();
                let prev_shape = state.as_ref().and_then(|s| s.shape.clone());
                let shape = c.shape.map(Arc::new).or(prev_shape);
                *state = Some(CursorState {
                    visible: c.visible,
                    shape,
                });
                (inner.wake)();
            }
            Ok(Message::Notice(n)) => {
                inner.heard();
                match n.kind.as_str() {
                    "audio" => {
                        *inner.host_audio_problem.lock() = n.message.clone();
                        if !n.message.is_empty() {
                            inner.stats.lock().audio =
                                format!("no sound from the host: {}", n.message);
                        } else {
                            inner.stats.lock().audio.clear();
                        }
                    }
                    _ if !n.message.is_empty() => inner.emit(Event::Notice(n.message)),
                    _ => {}
                }
            }
            Ok(_) => inner.heard(),
            Err(mpsc::RecvTimeoutError::Timeout) => {}
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                if inner.stopping() {
                    return Outcome::Stopped;
                }
                return if inner.connected.load(Ordering::Acquire) {
                    Outcome::Ended(-1, "The connection to the host closed.".into())
                } else {
                    Outcome::Failed("starting".into(), "the host closed the connection".into())
                };
            }
        }
        let now = Instant::now();
        if now - last_keepalive >= KEEPALIVE {
            last_keepalive = now;
            if !send_control(inner, &Message::Keepalive) && !inner.stopping() {
                return Outcome::Ended(-1, "The connection to the host broke.".into());
            }
        }
        if now - last_report >= REPORT_EVERY {
            last_report = now;
            let report = inner.feedback.lock().clone();
            if !send_control(inner, &Message::Report(report)) && !inner.stopping() {
                return Outcome::Ended(-1, "The connection to the host broke.".into());
            }
        }
        if !inner.connected.load(Ordering::Acquire) && accepted.elapsed() >= MEDIA_TIMEOUT {
            return Outcome::Failed(
                "connecting".into(),
                format!(
                    "the host started the stream ({}), but no picture arrived: UDP port {} may be blocked between the two machines",
                    accept.encoder,
                    latch_core::CONTROL_PORT
                ),
            );
        }
        let silent = inner
            .now_us()
            .saturating_sub(inner.heard_us.load(Ordering::Relaxed));
        if silent >= SILENT.as_micros() as u64 {
            return Outcome::Ended(
                control::end::VIEWER_SILENT as i32,
                "The host stopped answering.".into(),
            );
        }
    }
}

/// The receive thread: every datagram from the host, the hello and ping
/// timers, input resends and keyframe requests, and the statistics.
fn receive(
    inner: &Arc<Inner>,
    udp: UdpSocket,
    host: SocketAddr,
    accept: &Accept,
    token: [u8; 16],
    mut player: Option<Player>,
) {
    crate::priority::media_thread();
    let session = accept.session;
    // Readiness wakes immediately for datagrams on macOS, Linux and Windows;
    // socket receive timeouts round up to scheduler ticks on Windows.
    let Ok(mut poll) = mio::Poll::new() else {
        return;
    };
    if udp.set_nonblocking(true).is_err() {
        return;
    }
    let Ok(clone) = udp.try_clone() else {
        return;
    };
    let mut ready_socket = mio::net::UdpSocket::from_std(clone);
    if poll
        .registry()
        .register(&mut ready_socket, mio::Token(0), mio::Interest::READABLE)
        .is_err()
    {
        return;
    }
    let mut events = mio::Events::with_capacity(4);
    let accepted = Instant::now();
    #[cfg(not(any(target_os = "linux", target_os = "macos", windows)))]
    let mut buf = vec![0u8; latch_proto::MAX_DATAGRAM + 1];
    #[cfg(any(target_os = "linux", target_os = "macos", windows))]
    let mut batch = latch_proto::transport::ReceiveBatch::default();
    let mut out = Vec::with_capacity(64);
    let mut reassembler = Reassembler::new(session);
    let mut audio_rx = accept.audio.as_ref().map_or_else(
        || AudioReceiver::new(session),
        |format| AudioReceiver::with_format(session, format.frame_samples, format.rate),
    );
    let mut keyframes = KeyframeRequests::new();
    let mut feedback = LatestReceiver::new();
    let mut rtt = RttEstimator::new();
    let mut media_seen = false;
    let mut last_hello: Option<Instant> = None;
    let mut last_ping = Instant::now();
    let mut window_start = Instant::now();
    let mut throughput = Throughput::default();
    let mut arrivals = Arrivals::default();
    let mut window_base = reassembler.stats();
    let mut report_start = Instant::now();
    let mut report_base = window_base;
    let mut poor = false;
    let mut good_windows = 0u32;
    // The socket ran dry on the last pass, after which everything that had
    // arrived was handled: wait for the next datagram or timer.
    let mut drained = false;
    while !inner.stopping() {
        if drained {
            let now = Instant::now();
            let deadline = reassembler
                .next_deadline()
                .into_iter()
                .chain(audio_rx.next_deadline())
                .chain(inner.link.lock().as_ref().and_then(Link::next_deadline))
                .min()
                .unwrap_or(now + Duration::from_millis(20));
            let wait = deadline
                .saturating_duration_since(now)
                .min(Duration::from_millis(20));
            let _ = poll.poll(&mut events, Some(wait));
            drained = false;
        }
        let now = Instant::now();
        let hello_every = if media_seen { HELLO_SLOW } else { HELLO_FAST };
        if last_hello.is_none_or(|t| now - t >= hello_every) {
            wire::write_hello(&mut out, session, Some(&token));
            let _ = udp.send_to(&out, host);
            last_hello = Some(now);
        }
        if media_seen && now - last_ping >= PING_EVERY {
            wire::write_ping(&mut out, session, inner.now_us());
            let _ = udp.send_to(&out, host);
            last_ping = now;
        }
        if let Some(link) = inner.link.lock().as_mut() {
            link.flush(now);
        }
        #[cfg(any(target_os = "linux", target_os = "macos", windows))]
        match receive_ready(&ready_socket, &mut batch, &udp, |bytes, from| {
            if from.ip() == host.ip() {
                if let Ok(datagram) = wire::parse(bytes) {
                    if datagram.session() == session {
                        inner.heard();
                        handle(
                            inner,
                            &datagram,
                            &udp,
                            from,
                            &mut out,
                            &mut reassembler,
                            &mut audio_rx,
                            &mut feedback,
                            &mut rtt,
                            &mut media_seen,
                            &mut throughput,
                            bytes.len(),
                        );
                        hand_off_video(
                            inner,
                            &mut reassembler,
                            &mut throughput,
                            &mut arrivals,
                            Instant::now(),
                        );
                    }
                }
            }
        }) {
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => drained = true,
            _ => {}
        }
        #[cfg(not(any(target_os = "linux", target_os = "macos", windows)))]
        // Drain a bounded batch before timers and feedback bookkeeping.
        for _ in 0..64 {
            match ready_socket.recv_from(&mut buf) {
                Ok((n, from)) if from.ip() == host.ip() => {
                    if let Ok(datagram) = wire::parse(&buf[..n]) {
                        if datagram.session() == session {
                            inner.heard();
                            handle(
                                inner,
                                &datagram,
                                &udp,
                                from,
                                &mut out,
                                &mut reassembler,
                                &mut audio_rx,
                                &mut feedback,
                                &mut rtt,
                                &mut media_seen,
                                &mut throughput,
                                n,
                            );
                        }
                    }
                }
                Ok(_) => {}
                // Hand on what completed before waiting for more: a frame's
                // last datagram must not wait for the next frame's first.
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                    drained = true;
                    break;
                }
                Err(_) => {}
            }
        }
        let now = Instant::now();
        // Also when no audio arrived: a lost packet at the end of a sound
        // must not hold the rest of it until the next sound.
        while let Some(out) = audio_rx.pop(now) {
            if let Some(player) = player.as_mut() {
                match out {
                    AudioOut::Frame { payload, .. } => player.push(&payload),
                    AudioOut::Missing { .. } => player.push(&[]),
                }
            }
        }
        hand_off_video(inner, &mut reassembler, &mut throughput, &mut arrivals, now);
        let waiting = reassembler.needs_keyframe(accepted, now) || inner.queue.lock().want_keyframe;
        if keyframes.due(waiting, now, rtt.srtt()) {
            wire::write_keyframe_request(
                &mut out,
                session,
                reassembler.last_delivered().unwrap_or(0),
            );
            let _ = udp.send_to(&out, host);
        }
        if now - report_start >= REPORT_EVERY {
            let stats = reassembler.stats();
            let (loss, _, raw_loss) = loss_in_window(&report_base, &stats);
            let secs = (now - report_start).as_secs_f32();
            *inner.feedback.lock() = Report {
                fps: (stats.frames_delivered - report_base.frames_delivered) as f32 / secs,
                loss_pct: raw_loss,
                decode_ms: inner.stats.lock().decode_ms,
                rtt_ms: rtt.srtt().map_or(0.0, |d| d.as_secs_f32() * 1000.0),
                received_kbps: (std::mem::take(&mut throughput.feedback_received) as f32 * 8.0
                    / secs
                    / 1000.0) as u32,
                frames_lost: stats.frames_lost - report_base.frames_lost,
                unrecovered_loss_pct: Some(loss),
                window_ms: (secs * 1000.0) as u32,
                ..Default::default()
            };
            report_start = now;
            report_base = stats;
        }
        if now - window_start >= STATS_EVERY {
            let stats = reassembler.stats();
            let (loss, recovered, raw_loss) = loss_in_window(&window_base, &stats);
            let lost_count = stats.frames_lost.saturating_sub(window_base.frames_lost);
            let skipped = stats
                .frames_skipped
                .saturating_sub(window_base.frames_skipped);
            let lost_frames = lost_count > 0;
            window_base = stats;
            let secs = (now - window_start).as_secs_f32();
            window_start = now;
            let decode = std::mem::take(&mut *inner.decode.lock());
            let frames = decode.frames.max(1) as f32;
            {
                let mut s = inner.stats.lock();
                s.window_ms = (secs * 1000.0) as u32;
                s.frames_lost = lost_count;
                s.fps = decode.frames as f32 / secs;
                (s.received_fps, s.frame_jitter_ms) = arrivals.take(secs);
                s.raw_loss_pct = raw_loss;
                s.frames_dropped = skipped + inner.dropped.swap(0, Ordering::Relaxed);
                (s.mbps, s.received_mbps) = throughput.take(secs);
                s.decode_ms = decode.decode_us as f32 / frames / 1000.0;
                s.queue_ms = decode.queue_us as f32 / frames / 1000.0;
                s.host_ms = decode.host_tenths as f32 / frames / 10.0;
                s.assembly_ms = decode.assembly_us as f32 / frames / 1000.0;
                s.loss_pct = loss;
                s.fec_recovered = recovered;
                s.rtt_ms = rtt.srtt().map_or(0, |d| d.as_millis() as u32);
                s.rtt_var_ms = rtt.rttvar().as_millis() as u32;
                if let Some(p) = &player {
                    let problem = inner.host_audio_problem.lock();
                    s.audio = if problem.is_empty() {
                        p.describe()
                    } else {
                        format!("no sound from the host: {problem}")
                    };
                }
            }
            let bad = loss > 1.0 || lost_frames;
            if bad && !poor {
                poor = true;
                good_windows = 0;
                inner.emit(Event::Poor(true));
            } else if !bad && poor {
                good_windows += 1;
                if good_windows >= 3 {
                    poor = false;
                    inner.emit(Event::Poor(false));
                }
            }
        }
    }
}

fn hand_off_video(
    inner: &Arc<Inner>,
    reassembler: &mut Reassembler,
    throughput: &mut Throughput,
    arrivals: &mut Arrivals,
    now: Instant,
) {
    while let Some(event) = reassembler.poll(now) {
        match event {
            Reassembled::Frame(frame) => {
                throughput.frame(&frame);
                arrivals.frame(now);
                let mut q = inner.queue.lock();
                if q.frames.len() >= DECODE_QUEUE
                    || q.frames
                        .front()
                        .is_some_and(|(_, at)| now.saturating_duration_since(*at) >= DECODE_MAX_AGE)
                {
                    tracing::warn!("the decoder fell behind; restarting at a keyframe");
                    inner
                        .dropped
                        .fetch_add(q.frames.len() as u64, Ordering::Relaxed);
                    q.frames.clear();
                    q.want_keyframe = true;
                }
                q.frames.push_back((frame, now));
                drop(q);
                inner.queue_cv.notify_one();
            }
            Reassembled::Lost { frame } => {
                tracing::debug!("video frame {frame} lost");
            }
        }
    }
}

/// When whole frames arrived in the current statistics window.
#[derive(Default)]
struct Arrivals {
    last: Option<Instant>,
    frames: u32,
    /// Gaps between consecutive frames, in µs.
    gaps: Vec<u64>,
}

impl Arrivals {
    fn frame(&mut self, now: Instant) {
        if let Some(last) = self.last.replace(now) {
            self.gaps
                .push(now.saturating_duration_since(last).as_micros() as u64);
        }
        self.frames += 1;
    }

    /// Frames a second over `secs`, and the mean absolute deviation of the
    /// gaps between them in ms; the window starts again.
    fn take(&mut self, secs: f32) -> (f32, f32) {
        let fps = self.frames as f32 / secs.max(f32::EPSILON);
        let jitter = if self.gaps.len() < 2 {
            0.0
        } else {
            let mean = self.gaps.iter().sum::<u64>() as f64 / self.gaps.len() as f64;
            let spread = self
                .gaps
                .iter()
                .map(|&g| (g as f64 - mean).abs())
                .sum::<f64>()
                / self.gaps.len() as f64;
            (spread / 1000.0) as f32
        };
        self.frames = 0;
        self.gaps.clear();
        (fps, jitter)
    }
}

/// Media bytes in the current statistics window.
#[derive(Default)]
struct Throughput {
    /// Video and audio datagrams as received.
    received: u64,
    feedback_received: u64,
    /// Encoded video in the frames put back together.
    video: u64,
}

impl Throughput {
    fn datagram(&mut self, datagram: &Datagram<'_>, len: usize) {
        if matches!(datagram, Datagram::Video { .. } | Datagram::Audio { .. }) {
            self.received += len as u64;
            self.feedback_received += len as u64;
        }
    }

    fn frame(&mut self, frame: &InFrame) {
        self.video += frame.data.len() as u64;
    }

    /// Video and received Mbps over `secs`; starts the next window.
    fn take(&mut self, secs: f32) -> (f32, f32) {
        let window = std::mem::take(self);
        let mbps = |bytes: u64| bytes as f32 * 8.0 / secs / 1e6;
        (mbps(window.video), mbps(window.received))
    }
}

/// Unrecoverable loss in percent, shards FEC rebuilt, and the raw loss the
/// network caused (rebuilt or not), between two snapshots.
fn loss_in_window(before: &ReassemblyStats, after: &ReassemblyStats) -> (f32, u32, f32) {
    let received = after.shards_received.saturating_sub(before.shards_received);
    let recovered = after
        .shards_recovered
        .saturating_sub(before.shards_recovered);
    let missing = after.shards_missing.saturating_sub(before.shards_missing);
    let total = received + recovered + missing;
    if total == 0 {
        return (0.0, 0, 0.0);
    }
    (
        missing as f32 * 100.0 / total as f32,
        recovered.min(u32::MAX as u64) as u32,
        (recovered + missing) as f32 * 100.0 / total as f32,
    )
}

#[allow(clippy::too_many_arguments)]
fn handle(
    inner: &Arc<Inner>,
    datagram: &Datagram<'_>,
    udp: &UdpSocket,
    from: SocketAddr,
    out: &mut Vec<u8>,
    reassembler: &mut Reassembler,
    audio_rx: &mut AudioReceiver,
    feedback: &mut LatestReceiver,
    rtt: &mut RttEstimator,
    media_seen: &mut bool,
    throughput: &mut Throughput,
    len: usize,
) {
    let session = datagram.session();
    let now = Instant::now();
    throughput.datagram(datagram, len);
    match datagram {
        Datagram::Hello { .. } => {}
        Datagram::Video { shard, .. } => {
            if !*media_seen {
                *media_seen = true;
                let mut health = inner.video_health.lock();
                health.connected_at = Some(now);
                drop(health);
                inner.connected.store(true, Ordering::Release);
                inner.emit(Event::Connected);
            }
            reassembler.push(shard, now);
        }
        Datagram::Audio { packet, .. } => audio_rx.push(packet, now),
        Datagram::InputAck { next, .. } => {
            if let Some(link) = inner.link.lock().as_mut() {
                link.sender.ack(*next);
            }
        }
        Datagram::PadAck { acks, .. } => {
            if let Some(link) = inner.link.lock().as_mut() {
                for ack in acks.iter() {
                    link.pads.ack(ack);
                }
            }
        }
        Datagram::PadFeedback { updates, .. } => {
            let mut acks = Vec::with_capacity(updates.count());
            let deliver = inner.pad_feedback.lock();
            for u in updates.iter() {
                if feedback.accept(u.slot, u.seq) {
                    if let Some(f) = deliver.as_ref() {
                        f(u.slot, u.feedback);
                    }
                }
                acks.extend(feedback.ack(u.slot));
            }
            drop(deliver);
            if !acks.is_empty() {
                wire::write_pad_feedback_ack(out, session, &acks);
                let _ = udp.send_to(out, from);
            }
        }
        Datagram::Ping { time, .. } => {
            wire::write_pong(out, session, *time);
            let _ = udp.send_to(out, from);
        }
        Datagram::Pong { time, .. } => {
            let sample = inner.now_us().saturating_sub(*time);
            rtt.sample(Duration::from_micros(sample));
        }
        _ => {}
    }
}

/// The decode thread: frames in order, newest picture out.
#[cfg(test)]
fn decode(inner: &Arc<Inner>, decoder: Box<dyn Decoder>) {
    decode_color(inner, decoder, Default::default());
}
fn decode_color(
    inner: &Arc<Inner>,
    mut decoder: Box<dyn Decoder>,
    color: latch_proto::color::ColorDescription,
) {
    crate::priority::media_thread();
    // A decoder knows whether it runs on hardware only once it has a
    // session, after the first keyframe; the stats follow it.
    let mut named = decoder.name();
    loop {
        let (frame, ready) = {
            let mut q = inner.queue.lock();
            loop {
                if inner.stopping() {
                    return;
                }
                if q.frames
                    .front()
                    .is_some_and(|(_, at)| at.elapsed() >= DECODE_MAX_AGE)
                {
                    inner
                        .dropped
                        .fetch_add(q.frames.len() as u64, Ordering::Relaxed);
                    q.frames.clear();
                    q.want_keyframe = true;
                }
                if let Some(next) = q.frames.pop_front() {
                    if q.want_keyframe && !next.0.keyframe {
                        inner.dropped.fetch_add(1, Ordering::Relaxed);
                        continue;
                    }
                    if next.0.keyframe {
                        q.want_keyframe = false;
                    }
                    break next;
                }
                inner.queue_cv.wait_for(&mut q, Duration::from_millis(100));
            }
        };
        let start = Instant::now();
        let spare = inner.frames.spare();
        let decoded = decoder.decode(&frame.data, frame.keyframe, spare);
        if inner.stopping() {
            // The session may have finished without waiting for this.
            return;
        }
        match decoded {
            Ok(Some(mut picture)) => {
                if picture.color.bit_depth != color.bit_depth {
                    tracing::error!("decoder bit depth differs from negotiated stream");
                    return;
                }
                picture.color = color;
                let decoded = Instant::now();
                if decoder.name() != named {
                    named = decoder.name();
                    inner.stats.lock().decoder = named;
                }
                if picture.width > 0 && picture.height > 0 {
                    let mut st = inner.stats.lock();
                    st.width = picture.width;
                    st.height = picture.height;
                }
                let black = picture.is_black();
                inner.frames.publish(picture);
                inner
                    .published_us
                    .store(inner.now_us().max(1), Ordering::Relaxed);
                (inner.wake)();
                inner.video_health.lock().frame(decoded, black);
                let mut w = inner.decode.lock();
                w.frames += 1;
                w.decode_us += (decoded - start).as_micros() as u64;
                w.queue_us += (start - ready).as_micros() as u64;
                w.host_tenths += u64::from(frame.host_delay);
                w.assembly_us += frame.assembly.as_micros() as u64;
            }
            Ok(None) => {}
            Err(e) => {
                tracing::debug!("decode: {e:#}");
                inner.video_health.lock().last_error = Some(format!("{e:#}"));
                inner.queue.lock().want_keyframe = true;
            }
        }
    }
}

#[cfg(any(target_os = "linux", target_os = "macos", windows))]
fn receive_ready(
    ready: &mio::net::UdpSocket,
    batch: &mut latch_proto::transport::ReceiveBatch,
    udp: &UdpSocket,
    handle: impl FnMut(&[u8], SocketAddr),
) -> std::io::Result<usize> {
    // Mio must see WouldBlock even when I/O uses the standard-socket clone:
    // on Windows this rearms the interests disabled by the readable event.
    ready.try_io(|| batch.receive(udp, handle))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(windows)]
    #[test]
    fn batch_receive_rearms_after_a_drained_burst() {
        for offload in [false, true] {
            let udp = UdpSocket::bind("127.0.0.1:0").unwrap();
            udp.set_nonblocking(true).unwrap();
            let mut ready = mio::net::UdpSocket::from_std(udp.try_clone().unwrap());
            let mut poll = mio::Poll::new().unwrap();
            poll.registry()
                .register(&mut ready, mio::Token(0), mio::Interest::READABLE)
                .unwrap();
            let mut events = mio::Events::with_capacity(4);
            let mut batch = latch_proto::transport::ReceiveBatch::with_offload(offload);
            let tx = UdpSocket::bind("127.0.0.1:0").unwrap();
            let addr = udp.local_addr().unwrap();
            for burst in 0..4 {
                let sender = tx.try_clone().unwrap();
                let child = std::thread::spawn(move || {
                    std::thread::sleep(Duration::from_millis(10));
                    for _ in 0..8 {
                        sender.send_to(&[burst], addr).unwrap();
                    }
                });
                let start = Instant::now();
                poll.poll(&mut events, Some(Duration::from_secs(1)))
                    .unwrap();
                assert!(
                    events.iter().any(|event| event.is_readable()),
                    "missed burst {burst}, offload={offload}"
                );
                assert!(
                    start.elapsed() < Duration::from_millis(200),
                    "timer-driven receive"
                );
                child.join().unwrap();
                let mut count = 0;
                loop {
                    match receive_ready(&ready, &mut batch, &udp, |bytes, _| {
                        assert_eq!(bytes, &[burst]);
                        count += 1;
                    }) {
                        Ok(_) => {}
                        Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => break,
                        Err(e) => panic!("{e}"),
                    }
                }
                assert_eq!(count, 8);
            }
        }
    }

    #[test]
    fn arrivals_count_frames_and_how_unevenly_they_come() {
        let mut a = Arrivals::default();
        let t = Instant::now();
        for ms in [0, 10, 20, 30, 40] {
            a.frame(t + Duration::from_millis(ms));
        }
        let (fps, jitter) = a.take(0.05);
        assert_eq!(fps, 100.0);
        assert!(jitter < 0.01, "even gaps: {jitter}");
        for ms in [50, 52, 70, 72, 90] {
            a.frame(t + Duration::from_millis(ms));
        }
        let (_, jitter) = a.take(0.05);
        assert!(
            (jitter - 6.4).abs() < 0.01,
            "gaps 10, 2, 18, 2, 18 ms: {jitter}"
        );
        assert_eq!(a.take(1.0), (0.0, 0.0), "a new window");
    }

    #[test]
    fn video_health_distinguishes_missing_black_frozen_and_recovered_video() {
        let start = Instant::now();
        let mut h = VideoHealth::default();
        assert_eq!(h.problem(start), None);
        h.connected_at = Some(start);
        let later = start + Duration::from_secs(6);
        assert!(h.problem(later).unwrap().contains("no video has arrived"));
        h.frame(later, true);
        assert_eq!(h.problem(later), None);
        let black = later + Duration::from_secs(6);
        h.frame(black, true);
        assert!(h.problem(black).unwrap().contains("black picture"));
        h.frame(black, false);
        assert_eq!(h.problem(black), None);
        let frozen = black + Duration::from_secs(6);
        assert!(h.problem(frozen).unwrap().contains("stopped arriving"));
        h.last_error = Some("bad slice".into());
        assert!(h
            .problem(frozen)
            .unwrap()
            .contains("cannot be decoded: bad slice"));
    }

    #[test]
    fn loss_is_the_share_of_packets_fec_could_not_save() {
        let before = ReassemblyStats::default();
        let after = ReassemblyStats {
            shards_received: 90,
            shards_recovered: 8,
            shards_missing: 2,
            ..Default::default()
        };
        let (loss, recovered, raw) = loss_in_window(&before, &after);
        assert!((loss - 2.0).abs() < 1e-3);
        assert_eq!(recovered, 8);
        assert!((raw - 10.0).abs() < 1e-3);
        assert_eq!(loss_in_window(&after, &after), (0.0, 0, 0.0));
    }

    #[test]
    fn a_decoder_stuck_in_the_os_does_not_hold_the_session_open() {
        let (release, stuck) = mpsc::channel::<()>();
        let decoding = Worker::spawn("stuck-decode", move || {
            let _ = stuck.recv();
        })
        .unwrap();
        assert!(!decoding.join_within(Duration::from_millis(50)));
        drop(release);
        let decoding = Worker::spawn("decode", || {}).unwrap();
        assert!(decoding.join_within(Duration::from_secs(10)));
        let decoding = Worker::spawn("panicking-decode", || panic!("decoder bug")).unwrap();
        assert!(decoding.join_within(Duration::from_secs(10)));
    }

    #[test]
    fn the_bitrate_is_encoded_video_and_received_counts_fec_and_audio() {
        use latch_proto::audio::AudioPacketizer;
        use latch_proto::video::{OutFrame, Packetizer};
        let mut datagrams = Vec::new();
        let mut video = Packetizer::new(1);
        video.set_fec_percent(50);
        let frame = OutFrame {
            data: &[7; 100_000],
            keyframe: true,
            host_delay: 0,
        };
        video.packetize(&frame, |dg| datagrams.push(dg.to_vec()));
        let mut audio = AudioPacketizer::new(1);
        for seq in 0..8 {
            audio.packetize(seq * 5000, &[1; 500], |dg| datagrams.push(dg.to_vec()));
        }
        let now = Instant::now();
        let mut reassembler = Reassembler::new(1);
        let mut throughput = Throughput::default();
        for dg in &datagrams {
            let datagram = wire::parse(dg).unwrap();
            throughput.datagram(&datagram, dg.len());
            if let Datagram::Video { shard, .. } = datagram {
                reassembler.push(&shard, now);
            }
        }
        let mut hello = Vec::new();
        wire::write_hello(&mut hello, 1, None);
        throughput.datagram(&wire::parse(&hello).unwrap(), hello.len());
        while let Some(Reassembled::Frame(f)) = reassembler.poll(now) {
            throughput.frame(&f);
        }
        let (mbps, received) = throughput.take(1.0);
        assert_eq!(mbps, 0.8, "100 000 bytes of video in one second");
        let all: usize = datagrams.iter().map(Vec::len).sum();
        assert_eq!(received, all as f32 * 8.0 / 1e6);
        assert!(
            received > 1.5 * mbps,
            "parity, headers and audio: {received}"
        );
        assert_eq!(throughput.take(1.0), (0.0, 0.0), "a new window");
    }

    #[test]
    fn refusals_read_the_hosts_sentence() {
        assert_eq!(
            refusal(406, r#"{"ok":false,"error":"this host encodes HEVC"}"#),
            "this host encodes HEVC"
        );
        assert_eq!(refusal(500, "not json"), "the host answered HTTP 500");
    }

    #[test]
    fn hevc_is_offered_only_when_the_settings_allow_it() {
        let mut s = Settings {
            color: Default::default(),
            cursor: false,
            gamepads: false,
            width: 1920,
            height: 1080,
            fps: 60,
            bitrate_kbps: 20_000,
            hevc: false,
            av1: false,
            audio: true,
            name: "test".into(),
        };
        assert!(!offer(&s).contains(&Codec::Hevc));
        assert!(offer(&s).contains(&Codec::H264Baseline));
        s.hevc = true;
        assert_eq!(
            offer(&s).contains(&Codec::Hevc),
            video::decodable().contains(&Codec::Hevc)
        );
    }

    #[test]
    fn stop_interrupts_blocked_upgrade() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let session = Session::start(
            Server {
                address: "127.0.0.1".parse().unwrap(),
                port,
            },
            Settings {
                color: Default::default(),
                cursor: false,
                gamepads: false,
                width: 1920,
                height: 1080,
                fps: 60,
                bitrate_kbps: 10_000,
                hevc: false,
                av1: false,
                audio: false,
                name: "test".into(),
            },
            Arc::new(FrameSlot::default()),
            std::sync::mpsc::channel().0,
            || {},
        );
        let (mut server_sock, _) = listener.accept().unwrap();
        server_sock
            .set_read_timeout(Some(Duration::from_secs(5)))
            .unwrap();
        use std::io::Read;
        let mut head = Vec::new();
        while !head.ends_with(b"\r\n\r\n") {
            let mut byte = [0];
            server_sock.read_exact(&mut byte).unwrap();
            head.push(byte[0]);
        }
        let start = Instant::now();
        session.stop();
        while !session.finished() {
            if start.elapsed() > Duration::from_secs(3) {
                panic!("session did not finish within 3 seconds of stop");
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        drop(server_sock);
    }

    #[test]
    fn decoded_picture_updates_session_stats_dimensions() {
        let (tx, _rx) = mpsc::channel();
        let (wake_tx, wake_rx) = mpsc::channel();
        let frames = Arc::new(FrameSlot::default());
        let inner = Arc::new(Inner {
            events: tx,
            wake: Box::new(move || {
                let _ = wake_tx.send(());
            }),
            frames,
            stats: Mutex::new(Stats {
                width: 64,
                height: 64,
                ..Default::default()
            }),
            host_audio_problem: Mutex::new(String::new()),
            video_health: Mutex::new(VideoHealth::default()),
            decode: Mutex::new(DecodeWindow::default()),
            link: Mutex::new(None),
            queue: Mutex::new(DecodeQueue::default()),
            queue_cv: Condvar::new(),
            tcp: Mutex::new(None),
            epoch: Instant::now(),
            heard_us: AtomicU64::new(0),
            feedback: Mutex::new(Report::default()),
            published_us: AtomicU64::new(0),
            dropped: AtomicU64::new(0),
            connected: AtomicBool::new(false),
            stop: AtomicBool::new(false),
            finished: AtomicBool::new(false),
            audio_gain: Arc::new(AtomicU32::new(1.0f32.to_bits())),
            cursor_supported: AtomicBool::new(false),
            cursor_state: Mutex::new(None),
            control_writer: Mutex::new(None),
            pad_slots: AtomicU32::new(0),
            pad_problem: Mutex::new(String::new()),
            pad_feedback: Mutex::new(None),
            pending_pads: Mutex::new([None; latch_proto::gamepad::MAX_PADS]),
            done: Mutex::new(false),
            done_cv: Condvar::new(),
        });

        struct FakeDecoder;
        impl video::Decoder for FakeDecoder {
            fn decode(
                &mut self,
                _annexb: &[u8],
                _idr: bool,
                spare: Option<video::Frame>,
            ) -> anyhow::Result<Option<video::Frame>> {
                let mut f = spare.unwrap_or_default();
                f.width = 128;
                f.height = 64;
                Ok(Some(f))
            }
            fn name(&self) -> &'static str {
                "fake"
            }
        }

        let in_frame = InFrame {
            frame: 1,
            data: vec![0, 0, 0, 1],
            keyframe: true,
            host_delay: 0,
            assembly: Duration::ZERO,
        };
        inner
            .queue
            .lock()
            .frames
            .push_back((in_frame, Instant::now()));

        let inner_clone = inner.clone();
        let handle = std::thread::spawn(move || {
            decode(&inner_clone, Box::new(FakeDecoder));
        });

        let decoded = wake_rx.recv_timeout(Duration::from_secs(10));
        inner.stop.store(true, Ordering::Release);
        inner.queue_cv.notify_all();
        handle.join().unwrap();

        decoded.expect("decoder did not publish a frame");
        assert_eq!(inner.stats.lock().width, 128);
        assert_eq!(inner.stats.lock().height, 64);
    }
}
