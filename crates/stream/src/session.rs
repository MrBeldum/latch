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
use latch_proto::input::{Event as InputEvent, InputSender};
use latch_proto::rtt::RttEstimator;
use latch_proto::video::{InFrame, KeyframeRequests, Reassembled, Reassembler, ReassemblyStats};
use latch_proto::wire::{self, Datagram};
use parking_lot::{Condvar, Mutex};
use std::collections::VecDeque;
use std::net::{IpAddr, Shutdown, SocketAddr, TcpStream, UdpSocket};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
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
const REPORT_EVERY: Duration = Duration::from_millis(250);
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
    pub width: u32,
    pub height: u32,
    pub fps: u32,
    pub bitrate_kbps: u32,
    /// Ask for HEVC when both sides can; otherwise H.264.
    pub hevc: bool,
    pub audio: bool,
    /// This machine's name, for the host's log.
    pub name: String,
}

#[derive(Debug, Clone, Default)]
pub struct Stats {
    pub fps: f32,
    pub mbps: f32,
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
    buf: Vec<u8>,
}

impl Link {
    fn flush(&mut self, now: Instant) {
        if self.sender.poll(now, &mut self.buf) {
            let _ = self.udp.send_to(&self.buf, self.host);
        }
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
    raw_loss_milli: AtomicU64,
    connected: AtomicBool,
    stop: AtomicBool,
    finished: AtomicBool,
    done: Mutex<bool>,
    done_cv: Condvar,
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
            raw_loss_milli: AtomicU64::new(0),
            connected: AtomicBool::new(false),
            stop: AtomicBool::new(false),
            finished: AtomicBool::new(false),
            done: Mutex::new(false),
            done_cv: Condvar::new(),
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

    /// Ends the session; returns at once. [`Session::finished`] says when
    /// it is over.
    pub fn stop(&self) {
        self.inner.stop.store(true, Ordering::Release);
        if let Some(tcp) = self.inner.tcp.lock().as_ref() {
            let _ = tcp.shutdown(Shutdown::Read);
        }
        self.inner.queue_cv.notify_all();
    }

    pub fn stats(&self) -> Stats {
        let mut stats = self.inner.stats.lock().clone();
        stats.video_problem = self.inner.video_health.lock().problem(Instant::now());
        stats
    }

    pub fn input(&self) -> Input {
        Input {
            inner: self.inner.clone(),
        }
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
        .filter(|&c| settings.hevc || c != Codec::Hevc)
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
    match latch_proto::transport::configure_socket(&udp, 1024 * 1024, 4 * 1024 * 1024) {
        Ok(buffers) => {
            tracing::info!(send_bytes = buffers.send, receive_bytes = buffers.receive, "viewer UDP buffers (requested 1048576/4194304; Linux may cap at net.core.wmem_max/rmem_max)");
            let mut stats = inner.stats.lock();
            stats.socket_send_bytes = buffers.send;
            stats.socket_receive_bytes = buffers.receive;
        }
        Err(e) => return fail("connecting", format!("cannot configure media buffers: {e}")),
    }
    let udp_port = udp.local_addr().map(|a| a.port()).unwrap_or(0);
    let codecs = offer(settings);
    let request = StreamRequest {
        protocol: latch_proto::PROTOCOL,
        width: settings.width,
        height: settings.height,
        fps: settings.fps,
        bitrate_kbps: settings.bitrate_kbps,
        codecs: codecs.clone(),
        udp_port,
        audio: settings.audio,
        name: settings.name.clone(),
    };
    let body = serde_json::to_vec(&request).unwrap_or_default();
    let host = SocketAddr::new(server.address, server.port);
    let mut tcp = match latch_core::http::upgrade(
        host,
        control::PATH,
        control::UPGRADE,
        &body,
        START_TIMEOUT,
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
        Err(e) => return fail("connecting", format!("{e:#}")),
    };
    let _ = tcp.set_nodelay(true);
    let _ = tcp.set_read_timeout(Some(START_TIMEOUT));
    *inner.tcp.lock() = tcp.try_clone().ok();
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
    let Some(token) = control::token_from_hex(&accept.token) else {
        return fail("starting", "the host sent a malformed session token".into());
    };
    if !codecs.contains(&accept.codec) {
        return fail(
            "starting",
            format!("the host chose {:?}, which was not offered", accept.codec),
        );
    }
    let decoder = match video::new_decoder(accept.codec) {
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
        if accept.audio.is_none() && settings.audio {
            stats.audio = if accept.audio_problem.is_empty() {
                "no sound from the host".into()
            } else {
                format!("no sound from the host: {}", accept.audio_problem)
            };
        }
    }
    let player = accept.audio.as_ref().and_then(|a| {
        match Player::new(a.rate, a.channels as usize, a.frame_samples as usize) {
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
    *inner.link.lock() = Some(Link {
        udp: send_udp,
        host: media,
        sender: InputSender::new(accept.session),
        buf: Vec::with_capacity(latch_proto::MAX_DATAGRAM),
    });
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
        Worker::spawn("stream-decode", move || decode(&inner, decoder))
    };
    let outcome = control_loop(inner, &mut tcp, &accept);
    inner.stop.store(true, Ordering::Release);
    inner.connected.store(false, Ordering::Release);
    inner.queue_cv.notify_all();
    let _ = tcp.shutdown(Shutdown::Both);
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
            let _ = control::write_message(
                tcp,
                &Message::End(control::End {
                    code: control::end::ENDED,
                    message: String::new(),
                }),
            );
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
            if control::write_message(tcp, &Message::Keepalive).is_err() && !inner.stopping() {
                return Outcome::Ended(-1, "The connection to the host broke.".into());
            }
        }
        if now - last_report >= REPORT_EVERY {
            last_report = now;
            let stats = inner.stats.lock().clone();
            let report = Report {
                fps: stats.fps,
                loss_pct: inner.raw_loss_milli.load(Ordering::Relaxed) as f32 / 1000.0,
                decode_ms: stats.decode_ms,
                rtt_ms: stats.rtt_ms as f32,
                received_kbps: (stats.mbps * 1000.0) as u32,
                frames_lost: stats.frames_lost,
                unrecovered_loss_pct: Some(stats.loss_pct),
                queue_ms: stats.queue_ms,
                window_ms: stats.window_ms,
                ..Default::default()
            };
            if control::write_message(tcp, &Message::Report(report)).is_err() && !inner.stopping() {
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
    let mut buf = vec![0u8; latch_proto::MAX_DATAGRAM + 1];
    let mut out = Vec::with_capacity(64);
    let mut reassembler = Reassembler::new(session);
    let mut audio_rx = accept.audio.as_ref().map_or_else(
        || AudioReceiver::new(session),
        |format| AudioReceiver::with_format(session, format.frame_samples, format.rate),
    );
    let mut keyframes = KeyframeRequests::new();
    let mut rtt = RttEstimator::new();
    let mut media_seen = false;
    let mut last_hello: Option<Instant> = None;
    let mut last_ping = Instant::now();
    let mut window_start = Instant::now();
    let mut window_bytes = 0u64;
    let mut window_base = reassembler.stats();
    let mut poor = false;
    let mut good_windows = 0u32;
    while !inner.stopping() {
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
                                &mut rtt,
                                &mut media_seen,
                                &mut window_bytes,
                                n,
                            );
                        }
                    }
                }
                Ok(_) => {}
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                    let now = Instant::now();
                    let deadline = reassembler
                        .next_deadline()
                        .into_iter()
                        .chain(audio_rx.next_deadline())
                        .min()
                        .unwrap_or(now + Duration::from_millis(20));
                    let wait = deadline
                        .saturating_duration_since(now)
                        .min(Duration::from_millis(20));
                    let _ = poll.poll(&mut events, Some(wait));
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
        while let Some(event) = reassembler.poll(now) {
            match event {
                Reassembled::Frame(frame) => {
                    let mut q = inner.queue.lock();
                    if q.frames.len() >= DECODE_QUEUE
                        || q.frames.front().is_some_and(|(_, at)| {
                            now.saturating_duration_since(*at) >= DECODE_MAX_AGE
                        })
                    {
                        tracing::warn!("the decoder fell behind; restarting at a keyframe");
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
        let waiting = reassembler.needs_keyframe(accepted, now) || inner.queue.lock().want_keyframe;
        if keyframes.due(waiting, now, rtt.srtt()) {
            wire::write_keyframe_request(
                &mut out,
                session,
                reassembler.last_delivered().unwrap_or(0),
            );
            let _ = udp.send_to(&out, host);
        }
        if now - window_start >= REPORT_EVERY {
            let stats = reassembler.stats();
            let (loss, recovered, raw_loss) = loss_in_window(&window_base, &stats);
            let lost_count = stats.frames_lost.saturating_sub(window_base.frames_lost);
            let lost_frames = lost_count > 0;
            window_base = stats;
            inner
                .raw_loss_milli
                .store((raw_loss * 1000.0) as u64, Ordering::Relaxed);
            let secs = (now - window_start).as_secs_f32();
            window_start = now;
            let decode = std::mem::take(&mut *inner.decode.lock());
            let frames = decode.frames.max(1) as f32;
            {
                let mut s = inner.stats.lock();
                s.window_ms = (secs * 1000.0) as u32;
                s.frames_lost = lost_count;
                s.fps = decode.frames as f32 / secs;
                s.mbps = window_bytes as f32 * 8.0 / secs / 1e6;
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
            window_bytes = 0;
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
    rtt: &mut RttEstimator,
    media_seen: &mut bool,
    window_bytes: &mut u64,
    len: usize,
) {
    let session = datagram.session();
    let now = Instant::now();
    match datagram {
        Datagram::Hello { .. } => {}
        Datagram::Video { shard, .. } => {
            *window_bytes += len as u64;
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
fn decode(inner: &Arc<Inner>, mut decoder: Box<dyn Decoder>) {
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
                    q.frames.clear();
                    q.want_keyframe = true;
                }
                if let Some(next) = q.frames.pop_front() {
                    if q.want_keyframe && !next.0.keyframe {
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
            Ok(Some(picture)) => {
                let decoded = Instant::now();
                if decoder.name() != named {
                    named = decoder.name();
                    inner.stats.lock().decoder = named;
                }
                let black = picture.is_black();
                inner.frames.publish(picture);
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

#[cfg(test)]
mod tests {
    use super::*;

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
            width: 1920,
            height: 1080,
            fps: 60,
            bitrate_kbps: 20_000,
            hevc: false,
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
}
