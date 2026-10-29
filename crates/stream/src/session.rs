//! One streaming session: starts the moonlight-common-c connection on a
//! worker thread, routes its callbacks to the decoders, and reports what
//! happens through [`Event`]s. moonlight-common-c holds exactly one
//! connection at a time, so sessions are serialised through a global lock.

use crate::audio::{Output, Player};
use crate::ffi;
use crate::video::{self, Decoder, FrameSlot};
use parking_lot::{Condvar, Mutex};
use std::ffi::{CStr, CString};
use std::os::raw::{c_char, c_int, c_void};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::Sender;
use std::sync::Arc;
use std::time::{Duration, Instant};

static CONNECTION: Mutex<()> = Mutex::new(());
/// Held for every `LiSend*` and around `LiStopConnection`, so input cannot
/// run while moonlight is destroying its queues.
static INPUT: Mutex<()> = Mutex::new(());
/// Keeps `Inner` alive for C callbacks, including moonlight's detached
/// termination thread that can fire after `bl_stop` returns.
static CURRENT: Mutex<Option<Arc<Inner>>> = Mutex::new(None);

fn current() -> Option<Arc<Inner>> {
    CURRENT.lock().clone()
}

fn send_input(connected: &std::sync::atomic::AtomicBool, f: impl FnOnce()) {
    let _guard = INPUT.lock();
    if connected.load(Ordering::Acquire) {
        f();
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum Event {
    /// A connection stage is starting.
    Stage(String),
    Connected,
    /// Setup failed before the connection was established.
    Failed {
        stage: String,
        code: i32,
    },
    /// The connection ended after it was established. `code` 0 means the PC
    /// closed it on purpose.
    Terminated {
        code: i32,
        message: String,
    },
    /// The network is struggling (`true`) or fine again (`false`).
    Poor(bool),
    /// This machine cannot play the stream's sound; the reason.
    NoAudio(String),
}

#[derive(Debug, Clone)]
pub struct Settings {
    pub width: u32,
    pub height: u32,
    pub fps: u32,
    pub bitrate_kbps: u32,
    /// Ask for HEVC when both sides can; otherwise H.264.
    pub hevc: bool,
}

/// What `Session::start` needs from `/serverinfo` and `/launch`.
#[derive(Debug, Clone)]
pub struct Server {
    pub address: String,
    pub app_version: String,
    pub gfe_version: String,
    pub rtsp_url: String,
    pub codec_mode_support: i32,
}

#[derive(Debug, Clone, Default)]
pub struct Stats {
    pub fps: f32,
    pub mbps: f32,
    pub rtt_ms: u32,
    pub rtt_var_ms: u32,
    pub decode_ms: f32,
    pub host_ms: f32,
    pub assembly_ms: f32,
    pub queue_ms: f32,
    pub width: u32,
    pub height: u32,
    pub decoder: &'static str,
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

#[derive(Default)]
struct Window {
    since: Option<Instant>,
    frames: u32,
    bytes: u64,
    decode_us: u64,
    host_tenths: u64,
    /// RTP totals at the start of the window, to difference against.
    rtp: Option<ffi::RtpVideoStats>,
}

struct Inner {
    events: Sender<Event>,
    wake: Box<dyn Fn() + Send + Sync>,
    frames: Arc<FrameSlot>,
    decoder: Mutex<Option<Box<dyn Decoder>>>,
    audio: Mutex<Option<Player>>,
    /// Why the player could not be made, when it could not.
    audio_error: Mutex<Option<String>>,
    /// The one notice about sound has gone out.
    audio_warned: AtomicBool,
    window: Mutex<Window>,
    stats: Mutex<Stats>,
    video_health: Mutex<VideoHealth>,
    connected: AtomicBool,
    finished: AtomicBool,
    /// Set when the connection ended or a stop was asked for.
    done: Mutex<bool>,
    done_cv: Condvar,
}

impl Inner {
    fn emit(&self, e: Event) {
        let _ = self.events.send(e);
        (self.wake)();
    }

    fn finish(&self) {
        *self.done.lock() = true;
        self.done_cv.notify_all();
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

/// One UTF-8 text event is kept this small: the control stream's packet
/// buffer is 128 bytes on hosts without the newer encryption.
const TEXT_CHUNK: usize = 32;

impl Input {
    pub fn connected(&self) -> bool {
        self.inner.connected.load(Ordering::Acquire)
    }

    pub fn mouse_move(&self, dx: i16, dy: i16) {
        send_input(&self.inner.connected, || unsafe {
            ffi::LiSendMouseMoveEvent(dx, dy);
        });
    }

    pub fn mouse_position(&self, x: i16, y: i16, width: i16, height: i16) {
        send_input(&self.inner.connected, || unsafe {
            ffi::LiSendMousePositionEvent(x, y, width, height);
        });
    }

    /// `button` is one of `ffi::BUTTON_*`.
    pub fn mouse_button(&self, button: c_int, down: bool) {
        send_input(&self.inner.connected, || {
            let action = if down {
                ffi::BUTTON_ACTION_PRESS
            } else {
                ffi::BUTTON_ACTION_RELEASE
            };
            unsafe { ffi::LiSendMouseButtonEvent(action, button) };
        });
    }

    /// `vk` is a Windows virtual-key code; `modifiers` a mask of `ffi::MODIFIER_*`.
    pub fn key(&self, vk: i16, down: bool, modifiers: c_char) {
        send_input(&self.inner.connected, || {
            let action = if down {
                ffi::KEY_ACTION_DOWN
            } else {
                ffi::KEY_ACTION_UP
            };
            unsafe { ffi::LiSendKeyboardEvent(vk, action, modifiers) };
        });
    }

    /// Type `text` on the PC as it is, whatever the keyboard layouts.
    pub fn text(&self, text: &str) {
        send_input(&self.inner.connected, || {
            for chunk in text_chunks(text) {
                unsafe {
                    ffi::LiSendUtf8TextEvent(chunk.as_ptr() as *const c_char, chunk.len() as u32);
                }
            }
        });
    }

    /// Vertical and horizontal scroll in 1/120ths of a wheel click.
    pub fn scroll(&self, vertical: i16, horizontal: i16) {
        send_input(&self.inner.connected, || {
            if vertical != 0 {
                unsafe { ffi::LiSendHighResScrollEvent(vertical) };
            }
            if horizontal != 0 {
                unsafe { ffi::LiSendHighResHScrollEvent(horizontal) };
            }
        });
    }
}

/// `text` in pieces of at most [`TEXT_CHUNK`] bytes, cut between characters.
fn text_chunks(text: &str) -> Vec<&str> {
    let mut out = Vec::new();
    let mut start = 0;
    while start < text.len() {
        let mut end = (start + TEXT_CHUNK).min(text.len());
        while end > start && !text.is_char_boundary(end) {
            end -= 1;
        }
        if end == start {
            break;
        }
        out.push(&text[start..end]);
        start = end;
    }
    out
}

impl Session {
    /// Connect on a worker thread. `wake` is called whenever there is a new
    /// event or frame, so a UI can repaint.
    pub fn start(
        server: Server,
        settings: Settings,
        ri_key: [u8; 16],
        ri_iv: [u8; 16],
        frames: Arc<FrameSlot>,
        events: Sender<Event>,
        wake: impl Fn() + Send + Sync + 'static,
    ) -> Session {
        let inner = Arc::new(Inner {
            events,
            wake: Box::new(wake),
            frames,
            decoder: Mutex::new(None),
            audio: Mutex::new(None),
            audio_error: Mutex::new(None),
            audio_warned: AtomicBool::new(false),
            window: Mutex::new(Window::default()),
            stats: Mutex::new(Stats::default()),
            video_health: Mutex::new(VideoHealth::default()),
            connected: AtomicBool::new(false),
            finished: AtomicBool::new(false),
            done: Mutex::new(false),
            done_cv: Condvar::new(),
        });
        let worker = inner.clone();
        std::thread::Builder::new()
            .name("stream".into())
            .spawn(move || run(worker, server, settings, ri_key, ri_iv))
            .expect("spawn stream thread");
        Session { inner }
    }

    pub fn connected(&self) -> bool {
        self.inner.connected.load(Ordering::Acquire)
    }

    /// The worker has torn everything down; a new session may start.
    pub fn finished(&self) -> bool {
        self.inner.finished.load(Ordering::Acquire)
    }

    /// Ask the connection to end. Returns at once; poll [`Session::finished`].
    pub fn stop(&self) {
        if *self.inner.done.lock() {
            return;
        }
        unsafe { ffi::bl_interrupt() };
        self.inner.finish();
    }

    pub fn stats(&self) -> Stats {
        let mut stats = self.inner.stats.lock().clone();
        let health = self.inner.video_health.lock();
        let now = Instant::now();
        if health
            .last_frame
            .is_none_or(|last| now.duration_since(last) >= Duration::from_secs(2))
        {
            stats.fps = 0.0;
            stats.mbps = 0.0;
        }
        if self.connected() {
            stats.video_problem = health.problem(now);
        }
        stats
    }

    /// A handle for sending input from other threads.
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

/// The bitrate to hand moonlight-common-c so the PC's encoder actually
/// targets `target_kbps`.
///
/// Neither side encodes at the number it is given. moonlight-common-c
/// (`SdpGenerator.c`) keeps 20% of the request for FEC and, on a remote
/// stream, another 500 kbps for audio and control (Latch never flags a
/// stream remote). Sunshine ignores that reduced figure anyway: it takes the
/// raw request (`x-ml-video.configuredBitrateKbps`,
/// `rtsp.cpp`) and makes its own deductions, 20% for FEC, then audio
/// (512 kbps for high-quality stereo, at most a fifth of what is left) and
/// 500 kbps for packet and control overhead (at most a tenth). A plain
/// request of 20 Mbps therefore encodes at 15 Mbps, and one padded only for
/// moonlight's 20% still lands at 18988 kbps. Latch treats the user's
/// number as the video target, not a total budget, so this inverts the
/// whole chain: the smallest request that survives every deduction at or
/// above the target.
pub fn request_bitrate_kbps(target_kbps: u32) -> u32 {
    let target = u64::from(target_kbps);
    // Closed form for the regime where both of Sunshine's caps are inactive
    // (request above 5 Mbps): encoder = request * 0.8 - 512 - 500.
    let mut request = (target + SUNSHINE_AUDIO_KBPS + SUNSHINE_CONTROL_KBPS) * 5 / 4;
    // Settle on the exact integer: the caps make small requests deduct
    // less, and the closed form rounds.
    while sunshine_encoder_kbps(request) < target {
        request += 1;
    }
    while request > 1 && sunshine_encoder_kbps(request - 1) >= target {
        request -= 1;
    }
    (request as u32).min(MAX_REQUEST_KBPS)
}

/// The most moonlight-common-c is asked for; keeps the wire budget inside
/// what a Tailscale path is expected to carry.
const MAX_REQUEST_KBPS: u32 = 200_000;
/// Sunshine's deduction for high-quality stereo Opus (256 kbps a channel).
const SUNSHINE_AUDIO_KBPS: u64 = 512;
/// Sunshine's deduction for A/V packet and control traffic overhead.
const SUNSHINE_CONTROL_KBPS: u64 = 500;

/// What Sunshine's encoder targets for a moonlight request of `request`
/// kbps: `rtsp.cpp`'s arithmetic on `configuredBitrateKbps`, FEC at
/// Latch's 20% (`fec_percentage` in the engine profile), high-quality
/// stereo audio. moonlight's own remote-stream deduction of 500 kbps only
/// touches the figure Sunshine does not use, so it plays no part.
fn sunshine_encoder_kbps(request: u64) -> u64 {
    // configuredBitrateKbps /= 100.f / (100 - fec_percentage)  (float division)
    let mut kbps = (request as f32 / (100.0 / 80.0)) as u64;
    kbps -= SUNSHINE_AUDIO_KBPS.min(kbps / 5);
    kbps -= SUNSHINE_CONTROL_KBPS.min(kbps / 10);
    kbps
}

fn run(inner: Arc<Inner>, server: Server, s: Settings, ri_key: [u8; 16], ri_iv: [u8; 16]) {
    let _one_at_a_time = CONNECTION.lock();
    if *inner.done.lock() {
        inner.finished.store(true, Ordering::Release);
        return;
    }
    let address = CString::new(server.address).unwrap_or_default();
    let app_version = CString::new(server.app_version).unwrap_or_default();
    let gfe_version = CString::new(server.gfe_version).unwrap_or_default();
    let rtsp_url = CString::new(server.rtsp_url).unwrap_or_default();
    let si = ffi::ServerInfo {
        address: address.as_ptr(),
        app_version: app_version.as_ptr(),
        gfe_version: gfe_version.as_ptr(),
        rtsp_url: rtsp_url.as_ptr(),
        codec_mode_support: server.codec_mode_support,
    };
    let mut formats = video::supported_formats();
    if !s.hevc {
        formats &= ffi::VIDEO_FORMAT_MASK_H264;
    }
    let cfg = ffi::StreamConfig {
        width: s.width as c_int,
        height: s.height as c_int,
        fps: s.fps as c_int,
        bitrate_kbps: request_bitrate_kbps(s.bitrate_kbps) as c_int,
        // moonlight-common-c would cap a remote IPv4 stream at 1024-byte
        // packets to survive raw-internet fragmentation. Latch never rides
        // raw internet: every stream goes through a Tailscale (WireGuard)
        // tunnel whose path MTU is a guaranteed 1280 bytes, so 1184 fits with
        // room for RTP/UDP/IP headers, the value moonlight itself trusts on a
        // 1280-guaranteed IPv6 path. Bigger packets mean fewer per frame,
        // which keeps large frames inside Sunshine's four-FEC-block limit
        // instead of shipping them unprotected and stalling on the first loss.
        // Always LOCAL + 1184: a "remote" flag would reopen 1024-byte packets
        // and subtract another 500 kbps, which is what made bitrate swing.
        packet_size: 1184,
        remote: ffi::STREAM_CFG_LOCAL,
        video_formats: formats,
        color_space: ffi::COLORSPACE_REC_709,
        color_range: ffi::COLOR_RANGE_LIMITED,
        encryption_flags: ffi::ENCFLG_ALL,
        video_capabilities: video::capabilities(),
        audio_capabilities: ffi::CAPABILITY_DIRECT_SUBMIT
            | ffi::CAPABILITY_SUPPORTS_ARBITRARY_AUDIO_DURATION,
        ri_key: ri_key.as_ptr(),
        ri_iv: ri_iv.as_ptr(),
    };
    let cb = ffi::Callbacks {
        video_setup,
        video_cleanup,
        video_frame,
        audio_setup,
        audio_cleanup,
        audio_packet,
        stage,
        connected,
        terminated,
        status,
        log,
    };
    let ctx = Arc::as_ptr(&inner) as *mut c_void;
    *CURRENT.lock() = Some(inner.clone());
    let rc = unsafe { ffi::bl_start(&si, &cfg, &cb, ctx) };
    if rc == 0 {
        let mut done = inner.done.lock();
        while !*done {
            inner.done_cv.wait(&mut done);
        }
    }
    inner.connected.store(false, Ordering::Release);
    {
        let _input = INPUT.lock();
        unsafe { ffi::bl_stop() };
    }
    *CURRENT.lock() = None;
    *inner.decoder.lock() = None;
    *inner.audio.lock() = None;
    inner.frames.clear();
    inner.finished.store(true, Ordering::Release);
    (inner.wake)();
}

unsafe extern "C" fn video_setup(
    _p: *mut c_void,
    format: c_int,
    w: c_int,
    h: c_int,
    _fps: c_int,
) -> c_int {
    let Some(inner) = current() else { return -1 };
    match video::new_decoder(format) {
        Ok(d) => {
            let mut st = inner.stats.lock();
            st.width = w as u32;
            st.height = h as u32;
            st.decoder = d.name();
            drop(st);
            *inner.decoder.lock() = Some(d);
            0
        }
        Err(e) => {
            tracing::error!("video decoder: {e:#}");
            -1
        }
    }
}

unsafe extern "C" fn video_cleanup(_p: *mut c_void) {
    if let Some(inner) = current() {
        *inner.decoder.lock() = None;
    }
}

unsafe extern "C" fn video_frame(
    _p: *mut c_void,
    data: *const u8,
    len: c_int,
    frame_type: c_int,
    _frame_number: c_int,
    host_latency: u16,
    receive_us: u64,
    enqueue_us: u64,
) -> c_int {
    let Some(inner) = current() else {
        return ffi::DR_OK;
    };
    if data.is_null() || len <= 0 {
        return ffi::DR_OK;
    }
    let bytes = std::slice::from_raw_parts(data, len as usize);
    let mut guard = inner.decoder.lock();
    let Some(dec) = guard.as_mut() else {
        return ffi::DR_OK;
    };
    let now_us = ffi::LiGetMicroseconds();
    let t = Instant::now();
    let result = dec.decode(
        bytes,
        frame_type == ffi::FRAME_TYPE_IDR,
        inner.frames.spare(),
    );
    let decode_us = t.elapsed().as_micros() as u64;
    {
        let mut stats = inner.stats.lock();
        stats.decoder = dec.name();
        stats.assembly_ms = enqueue_us.saturating_sub(receive_us) as f32 / 1000.0;
        stats.queue_ms = now_us.saturating_sub(enqueue_us) as f32 / 1000.0;
    }
    drop(guard);
    match result {
        Ok(Some(frame)) => {
            inner
                .video_health
                .lock()
                .frame(Instant::now(), frame.is_black());
            inner.frames.publish(frame);
            account(&inner, len as u64, decode_us, host_latency);
            (inner.wake)();
            ffi::DR_OK
        }
        Ok(None) => ffi::DR_OK,
        Err(e) => {
            let error = format!("{e:#}");
            let mut health = inner.video_health.lock();
            if health.last_error.as_ref() != Some(&error) {
                tracing::warn!("decode: {error}");
            }
            health.last_error = Some(error);
            ffi::DR_NEED_IDR
        }
    }
}

/// Roll per-second averages into `stats`.
fn account(inner: &Inner, bytes: u64, decode_us: u64, host_tenths: u16) {
    let mut w = inner.window.lock();
    let since = *w.since.get_or_insert_with(Instant::now);
    w.frames += 1;
    w.bytes += bytes;
    w.decode_us += decode_us;
    w.host_tenths += host_tenths as u64;
    let connected = inner.connected.load(Ordering::Relaxed);
    if w.rtp.is_none() && connected {
        w.rtp = rtp_stats();
    }
    let elapsed = since.elapsed().as_secs_f32();
    if elapsed >= 1.0 {
        let mut st = inner.stats.lock();
        st.fps = w.frames as f32 / elapsed;
        st.mbps = w.bytes as f32 * 8.0 / elapsed / 1_000_000.0;
        st.decode_ms = w.decode_us as f32 / w.frames.max(1) as f32 / 1000.0;
        st.host_ms = w.host_tenths as f32 / w.frames.max(1) as f32 / 10.0;
        let mut rtt = 0u32;
        let mut var = 0u32;
        if connected && unsafe { ffi::LiGetEstimatedRttInfo(&mut rtt, &mut var) } {
            st.rtt_ms = rtt;
            st.rtt_var_ms = var;
        }
        let now = if connected { rtp_stats() } else { None };
        if let (Some(before), Some(after)) = (w.rtp, now) {
            let (loss, recovered) = loss_in_window(&before, &after);
            st.loss_pct = loss;
            st.fec_recovered = recovered;
        }
        let (audio, problem) = audio_state(inner);
        st.audio = audio;
        drop(st);
        if let Some(p) = problem {
            if !inner.audio_warned.swap(true, Ordering::Relaxed) {
                inner.emit(Event::NoAudio(p));
            }
        }
        *w = Window {
            since: Some(Instant::now()),
            rtp: now,
            ..Default::default()
        };
    }
}

/// The stats phrase for sound and, when this machine cannot play it, the
/// reason worth one notice. Silence from the PC is not a problem here:
/// Sunshine sends nothing while nothing plays there.
fn audio_state(inner: &Inner) -> (String, Option<String>) {
    if let Some(e) = inner.audio_error.lock().as_ref() {
        return (format!("no sound: {e}"), Some(e.clone()));
    }
    let guard = inner.audio.lock();
    let Some(a) = guard.as_ref() else {
        return (String::new(), None);
    };
    match a.output() {
        Output::Failed(e) => (format!("no sound: {e}"), Some(e)),
        _ => (a.describe(), None),
    }
}

fn rtp_stats() -> Option<ffi::RtpVideoStats> {
    let p = unsafe { ffi::LiGetRTPVideoStats() };
    if p.is_null() {
        None
    } else {
        Some(unsafe { *p })
    }
}

/// Loss as a percentage of the video packets seen between two readings of
/// the RTP counters, and how many packets FEC saved in that time.
fn loss_in_window(before: &ffi::RtpVideoStats, after: &ffi::RtpVideoStats) -> (f32, u32) {
    let video = after
        .packet_count_video
        .saturating_sub(before.packet_count_video);
    let failed = after
        .packet_count_fec_failed
        .saturating_sub(before.packet_count_fec_failed);
    let recovered = after
        .packet_count_fec_recovered
        .saturating_sub(before.packet_count_fec_recovered);
    let seen = u64::from(video) + u64::from(failed);
    let loss = if seen == 0 {
        0.0
    } else {
        failed as f32 * 100.0 / seen as f32
    };
    (loss, recovered)
}

unsafe extern "C" fn audio_setup(
    _p: *mut c_void,
    sample_rate: c_int,
    channels: c_int,
    streams: c_int,
    coupled: c_int,
    samples_per_frame: c_int,
    mapping: *const u8,
) -> c_int {
    let Some(inner) = current() else { return 0 };
    // moonlight's mapping array contains at most eight entries. Reject
    // invalid lengths before turning its pointer into a Rust slice.
    if mapping.is_null() || !(1..=8).contains(&channels) {
        *inner.audio_error.lock() = Some("invalid audio channel mapping".into());
        return 0;
    }
    let mapping = std::slice::from_raw_parts(mapping, channels as usize);
    match Player::new(
        sample_rate as u32,
        channels as usize,
        streams,
        coupled,
        samples_per_frame as usize,
        mapping,
    ) {
        Ok(pl) => {
            *inner.audio.lock() = Some(pl);
            0
        }
        Err(e) => {
            // Streaming without sound beats not streaming; say why.
            tracing::warn!("audio: {e:#}");
            *inner.audio_error.lock() = Some(format!("{e:#}"));
            0
        }
    }
}

unsafe extern "C" fn audio_cleanup(_p: *mut c_void) {
    if let Some(inner) = current() {
        *inner.audio.lock() = None;
        *inner.audio_error.lock() = None;
    }
}

unsafe extern "C" fn audio_packet(_p: *mut c_void, data: *const u8, len: c_int) {
    let Some(inner) = current() else { return };
    let packet: &[u8] = if data.is_null() || len <= 0 {
        &[]
    } else {
        std::slice::from_raw_parts(data, len as usize)
    };
    let mut audio = inner.audio.lock();
    if let Some(a) = audio.as_mut() {
        a.push(packet);
    }
}

unsafe extern "C" fn stage(_p: *mut c_void, stage: c_int, state: c_int, error: c_int) {
    let Some(inner) = current() else { return };
    let name = ffi::stage_name(stage);
    match state {
        0 => inner.emit(Event::Stage(name)),
        2 => {
            inner.emit(Event::Failed {
                stage: name,
                code: error,
            });
            inner.finish();
        }
        _ => {}
    }
}

unsafe extern "C" fn connected(_p: *mut c_void) {
    let Some(inner) = current() else { return };
    inner.video_health.lock().connected_at = Some(Instant::now());
    inner.connected.store(true, Ordering::Release);
    inner.emit(Event::Connected);
}

unsafe extern "C" fn terminated(_p: *mut c_void, code: c_int) {
    let Some(inner) = current() else { return };
    inner.connected.store(false, Ordering::Release);
    inner.emit(Event::Terminated {
        code,
        message: termination_message(code),
    });
    inner.finish();
}

unsafe extern "C" fn status(_p: *mut c_void, status: c_int) {
    if let Some(inner) = current() {
        inner.emit(Event::Poor(status == ffi::CONN_STATUS_POOR));
    }
}

unsafe extern "C" fn log(_p: *mut c_void, line: *const c_char) {
    if !line.is_null() {
        tracing::debug!("moonlight: {}", CStr::from_ptr(line).to_string_lossy());
    }
}

pub fn termination_message(code: i32) -> String {
    match code {
        ffi::ML_ERROR_GRACEFUL_TERMINATION => "The PC ended the session.".into(),
        ffi::ML_ERROR_NO_VIDEO_TRAFFIC => {
            "No video arrived. UDP port 47998 to the PC is blocked, or its firewall is.".into()
        }
        ffi::ML_ERROR_NO_VIDEO_FRAME => {
            "Video could not be assembled. The connection is too poor for this bitrate.".into()
        }
        ffi::ML_ERROR_UNEXPECTED_EARLY_TERMINATION => {
            "The PC ended the session as soon as it started.".into()
        }
        ffi::ML_ERROR_PROTECTED_CONTENT => "Protected content is on the PC's screen.".into(),
        ffi::ML_ERROR_FRAME_CONVERSION => {
            "The PC could not convert frames (HDR or resolution mismatch).".into()
        }
        n => format!("Connection lost (code {n})."),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn video_health_distinguishes_missing_black_frozen_and_recovered_video() {
        let start = Instant::now();
        let at = |s| start + Duration::from_secs(s);
        let mut h = VideoHealth::default();
        assert!(
            h.problem(at(20)).is_none(),
            "connection setup has no video deadline"
        );
        h.connected_at = Some(start);
        assert!(h.problem(at(4)).is_none());
        assert!(h.problem(at(5)).unwrap().contains("no video"));
        h.frame(at(5), true);
        h.frame(at(9), true);
        assert!(h.problem(at(9)).is_none());
        assert!(h.problem(at(10)).unwrap().contains("black picture"));
        h.frame(at(11), false);
        assert!(h.problem(at(11)).is_none());
        assert!(h.problem(at(16)).unwrap().contains("stopped arriving"));
        h.last_error = Some("decode failed (-12911)".into());
        assert!(h.problem(at(17)).unwrap().contains("-12911"));
        h.frame(at(18), false);
        assert!(h.problem(at(18)).is_none());
        assert!(h.last_error.is_none());
    }

    #[test]
    fn text_is_typed_in_small_pieces_between_characters() {
        let short = "powershell";
        assert_eq!(text_chunks(short), vec![short]);
        let long = "é".repeat(40); // 80 bytes
        let chunks = text_chunks(&long);
        assert!(chunks.iter().all(|c| c.len() <= TEXT_CHUNK), "{chunks:?}");
        assert!(chunks.iter().all(|c| c.chars().all(|ch| ch == 'é')));
        assert_eq!(chunks.concat(), long);
        assert!(text_chunks("").is_empty());
    }

    #[test]
    fn loss_is_the_share_of_packets_fec_could_not_save() {
        let before = ffi::RtpVideoStats {
            packet_count_video: 1000,
            packet_count_fec_failed: 10,
            packet_count_fec_recovered: 5,
            ..Default::default()
        };
        let after = ffi::RtpVideoStats {
            packet_count_video: 1900,
            packet_count_fec_failed: 110,
            packet_count_fec_recovered: 25,
            ..Default::default()
        };
        let (loss, recovered) = loss_in_window(&before, &after);
        assert!((loss - 10.0).abs() < 0.01, "{loss}");
        assert_eq!(recovered, 20);
        assert_eq!(loss_in_window(&after, &after), (0.0, 0));
        // Counters reset (a new session): never negative, never a panic.
        assert_eq!(loss_in_window(&after, &before), (0.0, 0));
    }

    #[test]
    fn requested_bitrate_undoes_every_reduction() {
        // Sunshine encodes at request / 1.25 - 512 - 500 (rtsp.cpp). After
        // our compensation the encoder should land exactly on the target.
        for target in [
            2_000u32, 5_000, 12_000, 20_000, 35_000, 50_000, 80_000, 150_000,
        ] {
            let request = request_bitrate_kbps(target);
            let encoder = sunshine_encoder_kbps(u64::from(request));
            assert_eq!(
                encoder,
                u64::from(target),
                "target={target} request={request}"
            );
            // And it is the smallest such request.
            assert!(
                sunshine_encoder_kbps(u64::from(request) - 1) < u64::from(target),
                "target={target} request={request} is not minimal"
            );
        }
        // The figures Sunshine's log showed for the old compensation: a
        // 20 Mbps target asked for 25 000 and encoded at 18 988.
        assert_eq!(sunshine_encoder_kbps(25_000), 18_988);
        assert_eq!(request_bitrate_kbps(20_000), 26_265);
        assert!(request_bitrate_kbps(500_000) <= MAX_REQUEST_KBPS, "clamped");
    }

    #[test]
    fn stage_names_and_messages_are_readable() {
        assert!(!ffi::stage_name(4).is_empty());
        assert!(termination_message(0).contains("ended"));
        assert!(termination_message(-100).contains("47998"));
        assert!(termination_message(-7).contains("-7"));
        assert!(!ffi::launch_query().is_empty());
    }
}

/// Stream from the Sunshine on this machine for a few seconds:
/// `cargo test -p latch-stream stream_real -- --ignored --nocapture`
/// (run `pair_real` first).
#[cfg(test)]
mod real {
    use super::*;
    use crate::nvhttp::Client;
    use std::time::Duration;

    #[test]
    #[ignore = "needs a paired Sunshine on this machine"]
    fn stream_real() {
        let number = |name: &str, default: u32| {
            std::env::var(name)
                .map(|value| value.parse::<u32>().expect(name))
                .unwrap_or(default)
        };
        let width = number("LATCH_TEST_WIDTH", 1280);
        let height = number("LATCH_TEST_HEIGHT", 720);
        let fps = number("LATCH_TEST_FPS", 60);
        let bitrate_kbps = number("LATCH_TEST_BITRATE_KBPS", 10_000);
        let seconds = number("LATCH_TEST_SECONDS", 12);
        assert!(width > 0 && height > 0 && fps > 0 && bitrate_kbps > 0 && seconds >= 12);
        eprintln!("requested: {width}x{height} {fps} fps {bitrate_kbps} kbps duration={seconds}s");
        let dir = std::env::var_os("LATCH_TEST_DIR")
            .map(std::path::PathBuf::from)
            .unwrap_or_else(|| std::env::temp_dir().join("latch-pair-test"));
        let identity = crate::Identity::load_or_create(&dir).unwrap();
        let ip: std::net::IpAddr = std::env::var("LATCH_TEST_IP")
            .unwrap_or_else(|_| "127.0.0.1".into())
            .parse()
            .unwrap();
        let cert = std::fs::read(dir.join("server.der")).ok();
        let mut client = Client::new(&identity, ip, cert).unwrap();
        let mut info = client.server_info().unwrap();
        if std::env::var_os("LATCH_TEST_IP").is_some() {
            assert!(
                info.paired,
                "a remote PC must be reached without pairing again"
            );
        }
        if !info.paired {
            let der = client
                .pair("4321", "latch-test", || {
                    let _ = latch_core::http::post_json::<_, latch_core::api::Ack>(
                        ("127.0.0.1", latch_core::CONTROL_PORT),
                        "/v1/pin",
                        &latch_core::api::PinRequest {
                            pin: "4321".into(),
                            name: "latch-test".into(),
                        },
                        Duration::from_secs(10),
                    );
                })
                .unwrap();
            std::fs::write(dir.join("server.der"), &der).unwrap();
            info = client.server_info().unwrap();
        }
        eprintln!("{info:?}");
        let apps = client.app_list().unwrap();
        let desktop = apps.iter().find(|a| a.title == "Desktop").unwrap();
        let ri_key: [u8; 16] = rand::random();
        let ri_id: u32 = rand::random();
        let mut ri_iv = [0u8; 16];
        ri_iv[..4].copy_from_slice(&ri_id.to_be_bytes());
        let resume = info.current_game != 0;
        let rtsp = client
            .launch(
                desktop.id,
                width,
                height,
                fps,
                bitrate_kbps,
                &ri_key,
                ri_id,
                resume,
            )
            .unwrap();
        eprintln!("rtsp: {rtsp} (resume={resume})");
        let frames = Arc::new(FrameSlot::default());
        let (tx, rx) = std::sync::mpsc::channel();
        let session = Session::start(
            Server {
                address: ip.to_string(),
                app_version: info.app_version.clone(),
                gfe_version: info.gfe_version.clone(),
                rtsp_url: rtsp,
                codec_mode_support: info.codec_mode_support,
            },
            Settings {
                width,
                height,
                fps,
                bitrate_kbps,
                hevc: std::env::var_os("LATCH_TEST_HEVC").is_some(),
            },
            ri_key,
            ri_iv,
            frames.clone(),
            tx,
            || {},
        );
        let start = Instant::now();
        let mut connected = false;
        let mut sampled_second = 0;
        while start.elapsed() < Duration::from_secs(u64::from(seconds)) {
            while let Ok(ev) = rx.try_recv() {
                eprintln!("{:>6.2}s {ev:?}", start.elapsed().as_secs_f32());
                if ev == Event::Connected {
                    connected = true;
                }
                if matches!(ev, Event::Failed { .. } | Event::Terminated { .. }) {
                    panic!("session ended early: {ev:?}");
                }
            }
            if connected && std::env::var_os("LATCH_TEST_MOUSE_SWEEP").is_some() {
                // Oscillate so the cursor stays on screen while a game reading
                // raw input sees continuous relative motion. Watch the PC's
                // cursor (GetCursorPos) to confirm the relative injection lands.
                let phase = (start.elapsed().as_millis() / 400) % 2;
                session
                    .input()
                    .mouse_move(if phase == 0 { 40 } else { -40 }, 0);
            }
            if let Some(f) = frames.take() {
                if frames.seq() % 60 == 1 {
                    eprintln!(
                        "frame {}x{} stride {} seq {}",
                        f.width,
                        f.height,
                        f.y_stride,
                        frames.seq()
                    );
                }
                // Exercise the same buffer reuse as the renderer; discarding
                // every frame makes this test profile fresh allocations instead.
                frames.recycle(f);
            }
            let second = start.elapsed().as_secs();
            if connected && second > sampled_second {
                sampled_second = second;
                eprintln!("sample {second}s: {:?}", session.stats());
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        let stats = session.stats();
        let published = frames.seq();
        eprintln!("stats: {stats:?}, frames published: {published}");
        session.stop();
        let t = Instant::now();
        while !session.finished() && t.elapsed() < Duration::from_secs(10) {
            std::thread::sleep(Duration::from_millis(20));
        }
        eprintln!("stopped in {:?}", t.elapsed());
        // Leave the PC as it was found: end the app this test started.
        if !resume {
            let _ = client.quit();
        }
        assert!(connected, "never connected");
        assert!(published > 60, "too few frames: {published}");
        assert!(session.finished(), "session did not finish");
    }
}
