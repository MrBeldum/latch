//! Sessions: the control service hands over an authorized, upgraded TCP
//! connection and its request; this runs the session until either side
//! ends it. One session at a time; a new one takes over.
//!
//! Threads per session: the connection's own thread runs the control
//! messages and timers, one thread captures and sends video, one captures
//! and sends audio. One thread for the whole server receives datagrams and
//! applies input as it arrives, so a key press never waits behind a frame.

use crate::audio::{self, AudioCapture};
use crate::input::{self, DisplayRect, InputSink};
use crate::video::{self, Codec, VideoConfig, VideoInfo, VideoPipeline};
use latch_proto::audio::AudioPacketizer;
use latch_proto::control::{self, end, Accept, AudioFormat, End, Message, Notice, StreamRequest};
use latch_proto::input::InputReceiver;
use latch_proto::rtt::RttEstimator;
use latch_proto::video::{fec_percent_for_loss, OutFrame, Packetizer, DEFAULT_FEC_PERCENT};
use latch_proto::wire::{self, Datagram};
use parking_lot::{Condvar, Mutex};
use std::collections::VecDeque;
use std::io::Write;
use std::net::{IpAddr, Ipv4Addr, Shutdown, SocketAddr, TcpStream, UdpSocket};
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicU8, Ordering};
use std::sync::mpsc;
use std::sync::{Arc, Weak};
use std::time::{Duration, Instant};

/// A still screen still gets a nearly empty frame this often, so a viewer
/// notices a lost last frame and the stream never looks dead. It also bounds
/// how long a keyframe request waits while next() waits for the screen to
/// change: on a still screen a 100 ms wait shows nothing new. Measured on a
/// 2-core ARM host at 720p: about 3% of a core; every 25 ms cost a Mac
/// viewer 2.5 times the CPU for nothing it could see.
const IDLE_REPEAT: Duration = Duration::from_millis(100);
const KEEPALIVE: Duration = Duration::from_secs(1);
/// A viewer heard from neither over TCP nor UDP for this long is gone.
const SILENT: Duration = Duration::from_secs(10);
const HELLO_EVERY: Duration = Duration::from_millis(50);
const PING_EVERY: Duration = Duration::from_millis(500);
/// Keyframes for requests at most this often: a burst of requests for one
/// loss must not turn into a burst of keyframes.
const KEYFRAME_GAP: Duration = Duration::from_millis(50);
/// How long a new session waits for the one it replaces to let go of the
/// screen and the encoder.
/// Initial capture can wait 10 seconds; undo commands can take another 15.
const TAKEOVER_WAIT: Duration = Duration::from_secs(30);
const VIDEO_RECOVERY_WINDOW: Duration = Duration::from_secs(60);
const VIDEO_RECOVERIES: usize = 3;
/// Capture supplies 10 ms; each coded packet carries 5 ms to fit the MTU,
/// including the parity packet's extra two length bytes.
const AUDIO_FRAME: usize = 240;

/// Readies the desktop for a session (for example, sizes it to the
/// viewer); returns what undoes that when the session ends.
pub type Prepare =
    Box<dyn Fn(&StreamRequest) -> anyhow::Result<Option<Box<dyn FnOnce() + Send>>> + Send + Sync>;

pub struct Options {
    pub prepare: Option<Prepare>,
    /// Lines for the owner's log (the host window shows them).
    pub log: Box<dyn Fn(String) + Send + Sync>,
}

impl Default for Options {
    fn default() -> Self {
        Self {
            prepare: None,
            log: Box::new(|line| tracing::info!("{line}")),
        }
    }
}

/// What `/v1/status` says about streaming.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ServerStatus {
    /// The viewer's machine name while a session runs.
    pub viewer: Option<String>,
    /// The encoder of the running or last session.
    pub encoder: String,
    pub hardware: bool,
    /// Why the running or last session had no sound.
    pub audio_problem: String,
}

/// Why a stream request was turned down, as an HTTP status and a sentence.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Refusal {
    pub status: u16,
    pub message: String,
}

impl Refusal {
    fn new(status: u16, message: impl Into<String>) -> Self {
        Self {
            status,
            message: message.into(),
        }
    }
}

pub struct Server {
    udp: UdpSocket,
    epoch: Instant,
    options: Options,
    current: Mutex<Option<Arc<Session>>>,
    /// Held while a session is being opened, so two requests cannot both
    /// take the screen.
    opening: Arc<Opening>,
    status: Mutex<ServerStatus>,
}

impl Server {
    /// Binds the media socket on every IPv4 address at `port` and starts
    /// receiving.
    pub fn bind(port: u16, options: Options) -> anyhow::Result<Arc<Self>> {
        let udp = UdpSocket::bind((Ipv4Addr::UNSPECIFIED, port))
            .map_err(|e| anyhow::anyhow!("cannot listen on UDP port {port}: {e}"))?;
        udp.set_read_timeout(Some(Duration::from_millis(200)))?;
        let server = Arc::new(Self {
            udp,
            epoch: Instant::now(),
            options,
            current: Mutex::new(None),
            opening: Arc::new(Opening::default()),
            status: Mutex::new(ServerStatus::default()),
        });
        let weak = Arc::downgrade(&server);
        std::thread::Builder::new()
            .name("stream-udp".into())
            .spawn(move || receive(weak))?;
        Ok(server)
    }

    pub fn port(&self) -> u16 {
        self.udp.local_addr().map(|a| a.port()).unwrap_or(0)
    }

    pub fn status(&self) -> ServerStatus {
        let mut status = self.status.lock().clone();
        status.viewer = self.current.lock().as_ref().map(|s| s.viewer.clone());
        status
    }

    /// Ends the running session, if any.
    pub fn stop(&self, code: u32, message: &str) {
        let current = self.current.lock().clone();
        if let Some(session) = current {
            session.end(code, message);
            session.wait_finished(TAKEOVER_WAIT);
        }
    }

    fn log(&self, line: String) {
        (self.options.log)(line);
    }

    fn now_us(&self) -> u64 {
        self.epoch.elapsed().as_micros() as u64
    }

    /// Opens what a session needs, after ending the one running. A refusal
    /// is for the HTTP reply; nothing is left open after one.
    pub fn prepare(&self, req: StreamRequest, peer: SocketAddr) -> Result<Prepared, Refusal> {
        if req.protocol != latch_proto::PROTOCOL {
            return Err(Refusal::new(
                426,
                format!(
                    "this host speaks stream protocol {}, not {}",
                    latch_proto::PROTOCOL,
                    req.protocol
                ),
            ));
        }
        let encodable = video::encodable();
        let Some(codec) = req.codecs.iter().copied().find(|c| encodable.contains(c)) else {
            return Err(Refusal::new(
                406,
                format!(
                    "this host encodes {}, which the viewer cannot decode",
                    names(&encodable)
                ),
            ));
        };
        let opening = self.opening.lock();
        let running = self.current.lock().clone();
        if let Some(old) = running {
            self.log(format!(
                "{} takes the stream over from {}",
                display_name(&req.name, peer.ip()),
                old.viewer
            ));
            old.end(end::TAKEN_OVER, "Another device took this stream over.");
            if !old.wait_finished(TAKEOVER_WAIT) {
                return Err(Refusal::new(
                    503,
                    "the previous stream is still letting go of the screen; try again",
                ));
            }
        }
        let undo = match &self.options.prepare {
            Some(prepare) => prepare(&req)
                .map_err(|e| Refusal::new(500, format!("could not prepare the desktop: {e:#}")))?,
            None => None,
        };
        let mut prepared = Prepared {
            undo,
            pipeline: None,
            audio: None,
            audio_problem: String::new(),
            input: None,
            input_problem: None,
            info: None,
            codec,
            req,
            peer,
            opening: Some(opening),
        };
        let config = VideoConfig {
            width: even(prepared.req.width.clamp(64, 8192)),
            height: even(prepared.req.height.clamp(64, 8192)),
            fps: prepared.req.fps.clamp(1, 240),
            bitrate_kbps: prepared.req.bitrate_kbps.clamp(500, 500_000),
            codec,
        };
        let pipeline = video::open(&config)
            .map_err(|e| Refusal::new(500, format!("could not capture the screen: {e:#}")))?;
        prepared.info = Some(pipeline.info());
        prepared.pipeline = Some((pipeline, config));
        if prepared.req.audio {
            match audio::open() {
                Ok(capture) => prepared.audio = Some(capture),
                Err(e) => prepared.audio_problem = format!("{e:#}"),
            }
        }
        match input::open() {
            Ok(sink) => prepared.input = Some(sink),
            Err(e) => prepared.input_problem = Some(format!("{e:#}")),
        }
        Ok(prepared)
    }

    /// Runs a prepared session on the upgraded connection until it ends.
    pub fn run(self: &Arc<Self>, mut prepared: Prepared, tcp: TcpStream) {
        let (pipeline, config) = prepared.pipeline.take().expect("prepared has a pipeline");
        let info = prepared.info.take().expect("prepared has video info");
        let token: [u8; 16] = rand::random();
        let id: u32 = rand::random();
        let session = Arc::new(Session {
            id,
            token,
            viewer: display_name(&prepared.req.name, prepared.peer.ip()),
            announced: SocketAddr::new(prepared.peer.ip(), prepared.req.udp_port),
            addr: Mutex::new(None),
            ended: Mutex::new(None),
            audio_notice: Mutex::new(None),
            stop: AtomicBool::new(false),
            finished: Mutex::new(false),
            finished_cv: Condvar::new(),
            keyframe: AtomicBool::new(true),
            input: Mutex::new(InputState {
                receiver: InputReceiver::new(),
                sink: prepared.input.take(),
                display: info.display,
            }),
            rtt: Mutex::new(RttEstimator::new()),
            fec: AtomicU8::new(DEFAULT_FEC_PERCENT),
            heard_us: AtomicU64::new(self.now_us()),
            frames: AtomicU64::new(0),
            bytes: AtomicU64::new(0),
        });
        // prepare owns the reservation until installation, so a second
        // prepare must see this session and wait for its undo before opening.
        *self.current.lock() = Some(session.clone());
        prepared.opening.take();
        {
            let mut status = self.status.lock();
            status.encoder = info.encoder.clone();
            status.hardware = info.hardware;
            status.audio_problem = prepared.audio_problem.clone();
        }
        let audio = prepared.audio.take();
        let accept = Accept {
            session: id,
            token: control::token_hex(&token),
            codec: info.codec,
            width: info.width,
            height: info.height,
            fps: config.fps,
            bitrate_kbps: config.bitrate_kbps,
            encoder: info.encoder.clone(),
            audio: audio.as_ref().map(|_| AudioFormat {
                rate: audio::RATE,
                channels: audio::CHANNELS as u32,
                frame_samples: AUDIO_FRAME as u32,
                codec: "lpcm-rice".into(),
            }),
            audio_problem: prepared.audio_problem.clone(),
        };
        self.log(format!(
            "{} is watching: {}×{} at {} fps, {} kb/s, {}{}",
            session.viewer,
            info.width,
            info.height,
            config.fps,
            config.bitrate_kbps,
            info.encoder,
            if audio.is_some() {
                String::new()
            } else if prepared.req.audio {
                format!("; no sound ({})", prepared.audio_problem)
            } else {
                String::new()
            }
        ));
        let started = Instant::now();
        let reason = self.control(&session, tcp, accept, prepared.input_problem.take(), {
            let server = self.clone();
            let session = session.clone();
            move || {
                let video = {
                    let server = server.clone();
                    let session = session.clone();
                    std::thread::Builder::new()
                        .name("stream-video".into())
                        .spawn(move || send_video(&server, &session, pipeline, config, video::open))
                };
                let audio = audio.map(|capture| {
                    let server = server.clone();
                    let session = session.clone();
                    std::thread::Builder::new()
                        .name("stream-audio".into())
                        .spawn(move || send_audio(&server, &session, capture, audio::open))
                });
                (video.ok(), audio.and_then(Result::ok))
            }
        });
        {
            let mut state = session.input.lock();
            if let Some(sink) = state.sink.as_mut() {
                sink.release_all();
            }
        }
        if let Some(undo) = prepared.undo.take() {
            undo();
        }
        {
            let mut current = self.current.lock();
            if current.as_ref().is_some_and(|c| Arc::ptr_eq(c, &session)) {
                *current = None;
            }
        }
        self.log(format!(
            "{} stopped watching after {}: {reason}",
            session.viewer,
            human_duration(started.elapsed())
        ));
        session.mark_finished();
    }

    /// The session's control loop on its TCP connection; returns why it
    /// ended once the media threads have stopped.
    fn control(
        &self,
        session: &Arc<Session>,
        tcp: TcpStream,
        accept: Accept,
        input_problem: Option<String>,
        start_media: impl FnOnce() -> (
            Option<std::thread::JoinHandle<()>>,
            Option<std::thread::JoinHandle<()>>,
        ),
    ) -> String {
        let _ = tcp.set_nodelay(true);
        let mut writer = match tcp.try_clone() {
            Ok(w) => w,
            Err(e) => return format!("the connection failed: {e}"),
        };
        if let Err(e) = control::write_message(&mut writer, &Message::Accept(accept)) {
            return format!("the viewer left before the stream started ({e})");
        }
        if let Some(problem) = input_problem {
            let _ = control::write_message(
                &mut writer,
                &Message::Notice(Notice {
                    kind: "input".into(),
                    message: problem,
                }),
            );
        }
        let (messages, inbox) = mpsc::channel();
        let reader = tcp.try_clone();
        if let Ok(mut reader) = reader {
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
        let (video, audio) = start_media();
        let mut last_keepalive = Instant::now();
        let mut last_hello = Instant::now() - HELLO_EVERY;
        let mut last_ping = Instant::now();
        let reason = loop {
            if let Some(e) = session.ended.lock().clone() {
                let _ = control::write_message(&mut writer, &Message::End(e.clone()));
                break e.message;
            }
            let audio_notice = session.audio_notice.lock().take();
            if let Some(message) = audio_notice {
                if control::write_message(
                    &mut writer,
                    &Message::Notice(Notice {
                        kind: "audio".into(),
                        message,
                    }),
                )
                .is_err()
                {
                    break "the viewer's connection broke".to_string();
                }
            }
            match inbox.recv_timeout(Duration::from_millis(20)) {
                Ok(Message::End(e)) => {
                    break if e.message.is_empty() {
                        "the viewer disconnected".to_string()
                    } else {
                        e.message
                    }
                }
                Ok(Message::Report(r)) => {
                    session.heard(self.now_us());
                    let fec = fec_percent_for_loss(r.loss_pct);
                    if session.fec.swap(fec, Ordering::Relaxed) != fec {
                        tracing::debug!(
                            "viewer reports {:.1}% loss; parity now {fec}%",
                            r.loss_pct
                        );
                    }
                }
                Ok(_) => session.heard(self.now_us()),
                Err(mpsc::RecvTimeoutError::Timeout) => {}
                Err(mpsc::RecvTimeoutError::Disconnected) => {
                    break "the viewer disconnected".to_string()
                }
            }
            let now = Instant::now();
            if now - last_keepalive >= KEEPALIVE {
                if control::write_message(&mut writer, &Message::Keepalive).is_err() {
                    break "the viewer's connection broke".to_string();
                }
                last_keepalive = now;
            }
            let addr = *session.addr.lock();
            match addr {
                None if now - last_hello >= HELLO_EVERY => {
                    let mut buf = Vec::with_capacity(16);
                    wire::write_hello(&mut buf, session.id, None);
                    let _ = self.udp.send_to(&buf, session.announced);
                    last_hello = now;
                }
                Some(addr) if now - last_ping >= PING_EVERY => {
                    let mut buf = Vec::with_capacity(16);
                    wire::write_ping(&mut buf, session.id, self.now_us());
                    let _ = self.udp.send_to(&buf, addr);
                    last_ping = now;
                }
                _ => {}
            }
            let silent = self
                .now_us()
                .saturating_sub(session.heard_us.load(Ordering::Relaxed));
            if silent >= SILENT.as_micros() as u64 {
                let e = End {
                    code: end::VIEWER_SILENT,
                    message: "The viewer stopped answering.".into(),
                };
                let _ = control::write_message(&mut writer, &Message::End(e.clone()));
                break e.message;
            }
        };
        session.stop.store(true, Ordering::Release);
        let _ = writer.flush();
        let _ = tcp.shutdown(Shutdown::Both);
        if let Some(v) = video {
            let _ = v.join();
        }
        if let Some(a) = audio {
            let _ = a.join();
        }
        reason
    }
}

/// An owned reservation, since Prepared crosses the HTTP upgrade callback.
#[derive(Default)]
struct Opening {
    busy: Mutex<bool>,
    cv: Condvar,
}

impl Opening {
    fn lock(self: &Arc<Self>) -> OpeningGuard {
        let mut busy = self.busy.lock();
        while *busy {
            self.cv.wait(&mut busy);
        }
        *busy = true;
        OpeningGuard(self.clone())
    }
}

struct OpeningGuard(Arc<Opening>);

impl Drop for OpeningGuard {
    fn drop(&mut self) {
        *self.0.busy.lock() = false;
        self.0.cv.notify_one();
    }
}

/// What [`Server::prepare`] opened, for [`Server::run`]. Dropped unused, it
/// undoes the desktop preparation.
pub struct Prepared {
    undo: Option<Box<dyn FnOnce() + Send>>,
    pipeline: Option<(Box<dyn VideoPipeline>, VideoConfig)>,
    audio: Option<Box<dyn AudioCapture>>,
    audio_problem: String,
    input: Option<Box<dyn InputSink>>,
    input_problem: Option<String>,
    info: Option<VideoInfo>,
    codec: Codec,
    req: StreamRequest,
    peer: SocketAddr,
    opening: Option<OpeningGuard>,
}

impl Prepared {
    pub fn codec(&self) -> Codec {
        self.codec
    }
}

impl Drop for Prepared {
    fn drop(&mut self) {
        // Release capture before restoring the display, and keep the
        // reservation until both resource cleanup and undo have finished.
        self.pipeline.take();
        self.audio.take();
        self.input.take();
        if let Some(undo) = self.undo.take() {
            undo();
        }
        self.opening.take();
    }
}

struct InputState {
    receiver: InputReceiver,
    sink: Option<Box<dyn InputSink>>,
    display: DisplayRect,
}

struct Session {
    id: u32,
    token: [u8; 16],
    viewer: String,
    /// The viewer's tailnet address and media port, for the host's Hello.
    announced: SocketAddr,
    /// Where media goes: the source of the viewer's valid Hello.
    addr: Mutex<Option<SocketAddr>>,
    /// Set once to end the session, with what to tell the viewer.
    ended: Mutex<Option<End>>,
    audio_notice: Mutex<Option<String>>,
    /// The media threads stop.
    stop: AtomicBool,
    finished: Mutex<bool>,
    finished_cv: Condvar,
    keyframe: AtomicBool,
    input: Mutex<InputState>,
    rtt: Mutex<RttEstimator>,
    fec: AtomicU8,
    /// When the viewer was last heard from, on the server's clock.
    heard_us: AtomicU64,
    frames: AtomicU64,
    bytes: AtomicU64,
}

impl Session {
    fn end(&self, code: u32, message: &str) {
        let mut ended = self.ended.lock();
        if ended.is_none() {
            *ended = Some(End {
                code,
                message: message.into(),
            });
        }
    }

    fn heard(&self, now_us: u64) {
        self.heard_us.store(now_us, Ordering::Relaxed);
    }

    fn stopping(&self) -> bool {
        self.stop.load(Ordering::Acquire) || self.ended.lock().is_some()
    }

    fn mark_finished(&self) {
        *self.finished.lock() = true;
        self.finished_cv.notify_all();
    }

    fn wait_finished(&self, timeout: Duration) -> bool {
        let deadline = Instant::now() + timeout;
        let mut finished = self.finished.lock();
        while !*finished {
            if self
                .finished_cv
                .wait_until(&mut finished, deadline)
                .timed_out()
            {
                return *finished;
            }
        }
        true
    }
}

/// The server's datagram loop: hellos, input, keyframe requests, pings.
fn receive(server: Weak<Server>) {
    crate::priority::media_thread();
    let mut buf = vec![0u8; latch_proto::MAX_DATAGRAM + 1];
    let mut out = Vec::with_capacity(64);
    loop {
        let Some(server) = server.upgrade() else {
            return;
        };
        let (n, from) = match server.udp.recv_from(&mut buf) {
            Ok(r) => r,
            Err(e)
                if matches!(
                    e.kind(),
                    std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                ) =>
            {
                continue
            }
            // A reset from an earlier datagram to a closed port (Windows
            // reports those on the next receive).
            Err(_) => continue,
        };
        let Ok(datagram) = wire::parse(&buf[..n]) else {
            continue;
        };
        let Some(session) = server.current.lock().clone() else {
            continue;
        };
        if datagram.session() != session.id {
            continue;
        }
        if let Datagram::Hello {
            token: Some(token), ..
        } = datagram
        {
            if *token == session.token {
                let mut addr = session.addr.lock();
                if *addr != Some(from) {
                    if addr.is_some() {
                        server.log(format!("{} moved to {from}", session.viewer));
                    }
                    *addr = Some(from);
                    session.keyframe.store(true, Ordering::Release);
                }
                drop(addr);
                session.heard(server.now_us());
            }
            continue;
        }
        if *session.addr.lock() != Some(from) {
            continue;
        }
        session.heard(server.now_us());
        match datagram {
            Datagram::Input { first, events, .. } => {
                let next = {
                    let mut guard = session.input.lock();
                    let state = &mut *guard;
                    let display = state.display;
                    let sink = &mut state.sink;
                    state.receiver.receive(first, events.iter(), |event| {
                        let Some(sink) = sink.as_mut() else {
                            return;
                        };
                        if event == input::Event::ReleaseAll {
                            sink.release_all();
                        } else if let Err(e) = sink.apply(&event, display) {
                            tracing::debug!("input {event:?}: {e:#}");
                        }
                    })
                };
                wire::write_input_ack(&mut out, session.id, next);
                let _ = server.udp.send_to(&out, from);
            }
            Datagram::KeyframeRequest { .. } => {
                session.keyframe.store(true, Ordering::Release);
            }
            Datagram::Ping { time, .. } => {
                wire::write_pong(&mut out, session.id, time);
                let _ = server.udp.send_to(&out, from);
            }
            Datagram::Pong { time, .. } => {
                let rtt = server.now_us().saturating_sub(time);
                session.rtt.lock().sample(Duration::from_micros(rtt));
            }
            _ => {}
        }
    }
}

fn send_video(
    server: &Server,
    session: &Session,
    mut pipeline: Box<dyn VideoPipeline>,
    config: VideoConfig,
    mut reopen: impl FnMut(&VideoConfig) -> anyhow::Result<Box<dyn VideoPipeline>>,
) {
    crate::priority::media_thread();
    let mut packetizer = Packetizer::new(session.id);
    let mut last_keyframe: Option<Instant> = None;
    let mut info = pipeline.info();
    let mut recoveries = VecDeque::new();
    while !session.stopping() {
        let due = last_keyframe.is_none_or(|t| t.elapsed() >= KEYFRAME_GAP);
        let keyframe = due && session.keyframe.swap(false, Ordering::AcqRel);
        // Short idle repeats bound how long a request arriving inside next
        // waits. Send every returned frame: discarding an encoded repeat
        // would discard a reference needed by subsequent P-frames.
        let frame = match pipeline.next(keyframe, IDLE_REPEAT) {
            Ok(f) => f,
            Err(e) => {
                if session.stopping() {
                    return;
                }
                server.log(format!("video stopped: {e:#}; starting it again"));
                // Some capture APIs allow only one owner of an output.
                drop(pipeline);
                let now = Instant::now();
                recoveries.retain(|t| now - *t < VIDEO_RECOVERY_WINDOW);
                if recoveries.len() < VIDEO_RECOVERIES {
                    recoveries.push_back(now);
                    if let Ok(p) = reopen(&config) {
                        pipeline = p;
                        session.keyframe.store(true, Ordering::Release);
                        continue;
                    }
                }
                session.end(
                    end::CAPTURE_FAILED,
                    &format!("The host could not capture its screen: {e:#}"),
                );
                return;
            }
        };
        let next_info = pipeline.info();
        if next_info != info {
            if next_info.display != info.display {
                session.input.lock().display = next_info.display;
            }
            if next_info.encoder != info.encoder || next_info.hardware != info.hardware {
                let mut status = server.status.lock();
                status.encoder = next_info.encoder.clone();
                status.hardware = next_info.hardware;
            }
            info = next_info;
        }
        if keyframe && !frame.keyframe {
            // The pipeline could not make this one a keyframe; ask again.
            session.keyframe.store(true, Ordering::Release);
        }
        if frame.keyframe {
            last_keyframe = Some(Instant::now());
        }
        let Some(addr) = *session.addr.lock() else {
            continue;
        };
        packetizer.set_fec_percent(session.fec.load(Ordering::Relaxed));
        let delay = (frame.captured.elapsed().as_micros() / 100).min(u16::MAX as u128) as u16;
        let out = OutFrame {
            data: &frame.data,
            keyframe: frame.keyframe,
            host_delay: delay,
        };
        packetizer.packetize(&out, |datagram| {
            send(&server.udp, datagram, addr);
        });
        session.frames.fetch_add(1, Ordering::Relaxed);
        session
            .bytes
            .fetch_add(frame.data.len() as u64, Ordering::Relaxed);
    }
}

fn send_audio(
    server: &Server,
    session: &Session,
    capture: Box<dyn AudioCapture>,
    mut reopen: impl FnMut() -> anyhow::Result<Box<dyn AudioCapture>>,
) {
    crate::priority::media_thread();
    let mut packetizer = AudioPacketizer::new(session.id);
    let mut pcm = [0i16; audio::FRAME * audio::CHANNELS];
    let mut coded = Vec::with_capacity(2048);
    let mut capture = Some(capture);
    let mut backoff = Duration::from_millis(100);
    let mut problem = false;
    while !session.stopping() {
        if capture.is_none() {
            // Check stop during backoff so takeover never waits for a retry.
            let retry_at = Instant::now() + backoff;
            while !session.stopping() && Instant::now() < retry_at {
                std::thread::sleep(
                    Duration::from_millis(20)
                        .min(retry_at.saturating_duration_since(Instant::now())),
                );
            }
            if session.stopping() {
                return;
            }
            match reopen() {
                Ok(c) => capture = Some(c),
                Err(e) => {
                    audio_problem(server, session, format!("{e:#}"));
                    backoff = (backoff * 2).min(Duration::from_secs(5));
                    continue;
                }
            }
        }
        match capture
            .as_mut()
            .expect("open audio")
            .read(&mut pcm, Duration::from_millis(100))
        {
            Ok(true) => {
                if problem {
                    audio_problem(server, session, String::new());
                    problem = false;
                }
                backoff = Duration::from_millis(100);
            }
            Ok(false) => continue,
            Err(e) => {
                server.log(format!("sound stopped: {e:#}"));
                capture.take();
                audio_problem(server, session, format!("{e:#}"));
                problem = true;
                backoff = (backoff * 2).min(Duration::from_secs(5));
                continue;
            }
        }
        let Some(addr) = *session.addr.lock() else {
            continue;
        };
        let pts = server.now_us() as u32;
        for (i, half) in pcm
            .as_chunks::<{ AUDIO_FRAME * audio::CHANNELS }>()
            .0
            .iter()
            .enumerate()
        {
            coded.clear();
            latch_codec::audio::encode(half, &mut coded);
            let pts = pts.wrapping_add((i as u32) * 5_000);
            // 240 samples always fit (lpcm-rice's worst case is 965 bytes);
            // a refusal would mean the codec changed underneath.
            let fits = packetizer.packetize(pts, &coded, |datagram| {
                send(&server.udp, datagram, addr);
            });
            debug_assert!(fits, "a coded 5 ms audio frame outgrew a datagram");
        }
    }
}

fn audio_problem(server: &Server, session: &Session, message: String) {
    server.status.lock().audio_problem = message.clone();
    *session.audio_notice.lock() = Some(message);
}

/// One datagram, retried briefly when the socket's buffer is full (macOS
/// says ENOBUFS when a keyframe outruns the interface).
fn send(udp: &UdpSocket, datagram: &[u8], to: SocketAddr) {
    for _ in 0..3 {
        match udp.send_to(datagram, to) {
            Ok(_) => return,
            Err(e)
                if e.raw_os_error() == Some(55) || e.kind() == std::io::ErrorKind::WouldBlock =>
            {
                std::thread::sleep(Duration::from_micros(200));
            }
            Err(_) => return,
        }
    }
}

fn even(v: u32) -> u32 {
    v & !1
}

fn names(codecs: &[Codec]) -> String {
    if codecs.is_empty() {
        return "nothing".into();
    }
    codecs
        .iter()
        .map(|c| match c {
            Codec::Hevc => "HEVC",
            Codec::H264High => "H.264 High",
            Codec::H264Baseline => "H.264 Baseline",
        })
        .collect::<Vec<_>>()
        .join(", ")
}

fn display_name(name: &str, ip: IpAddr) -> String {
    let name = name.trim();
    if name.is_empty() {
        ip.to_string()
    } else {
        name.chars().take(64).collect()
    }
}

fn human_duration(d: Duration) -> String {
    let s = d.as_secs();
    if s < 60 {
        format!("{s} s")
    } else if s < 3600 {
        format!("{} min", s / 60)
    } else {
        format!("{} h {} min", s / 3600, s / 60 % 60)
    }
}

#[cfg(test)]
#[path = "../tests/support/lifecycle.rs"]
mod tests;
