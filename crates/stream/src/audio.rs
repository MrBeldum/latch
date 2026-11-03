//! Coded audio frames in, sound out. Decoding (`lpcm-rice`, lossless, a few
//! microseconds a frame) happens on the stream's receive thread; the OS
//! pulls from a ring buffer on its own thread, resampled to whatever rate
//! the output device runs at and mixed onto its speakers. If the buffer
//! grows past [`MAX_QUEUED_MS`] the oldest audio is dropped: latency matters
//! more than never skipping.
//!
//! How much is kept queued is measured, not fixed. The mixer watches the
//! least audio left just before each device pull (the slack) and trims
//! playback speed by at most 0.2 % to hold that low point at a small
//! margin: as low as this network's jitter, the host's capture bursts and
//! the device's pull size allow, and no lower. A real underrun (not the
//! host going quiet) widens the margin, which then narrows again slowly.
//! The receive thread measures arrival jitter so that starting, and
//! starting again after a gap, waits for about the right cushion at once.
//! What the output side found is kept, so "no sound" comes with its reason.

use anyhow::{bail, Result};
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use parking_lot::Mutex;
use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::mpsc;
use std::sync::Arc;
use std::time::{Duration, Instant};

/// Queued sound beyond this is cut back to the mixer's target. Low, so
/// sound stays close to the picture; a Wi-Fi hiccup longer than this is a
/// click.
const MAX_QUEUED_MS: usize = 120;
const MAX_CHANNELS: usize = latch_codec::audio::MAX_CHANNELS;
/// Most playback is sped up or slowed down to steer the queue while the
/// host's and this device's clocks drift apart: 0.2 %, a pitch change too
/// small to hear and ten times the drift of two ordinary sound clocks.
const MAX_TRIM: f64 = 0.002;
/// The slack error that gets the whole trim.
const TRIM_FULL_MS: f64 = 4.0;
/// The slack the mixer steers for at least, and at most after underruns;
/// each underrun widens it by [`MARGIN_MS`].
const MARGIN_MS: f64 = 2.0;
const MAX_MARGIN_MS: f64 = 40.0;
/// How fast a margin widened by an underrun narrows again.
const MARGIN_DECAY_MS_PER_S: f64 = 0.1;
/// How fast the remembered low point of the slack forgets a dip.
const RELEASE_MS_PER_S: f64 = 0.25;
/// Arrival jitter assumed until some has been measured.
const DEFAULT_JITTER_MS: f64 = 10.0;
/// A queue empty for longer than this before sound came back was the host
/// going quiet (it sends nothing for silence), not an underrun.
const PAUSE: Duration = Duration::from_millis(100);
/// The device buffer asked for, when the device says what it allows: a
/// pull this small every few ms instead of ~10 ms ones. `LATCH_AUDIO_BUFFER`
/// (frames, or `default`) overrides it, for measurements.
const DEVICE_BUFFER_MS: f64 = 2.5;
/// How soon a missing or refusing output device is tried again.
const OUTPUT_RETRY: Duration = Duration::from_secs(1);

/// The output device, as the output thread found it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Output {
    Opening,
    Playing {
        device: String,
        rate: u32,
        channels: u16,
    },
    /// No device, or the device refused the stream; the reason.
    Failed(String),
}

/// What the receive thread and the device's mixer tell each other, and
/// the stats line reads.
#[derive(Debug, Default)]
struct Link {
    /// Arrival jitter the player measured, µs; 0 until it has.
    jitter_us: AtomicU32,
    /// The most audio that arrived at one moment lately, µs.
    burst_us: AtomicU32,
    /// The cushion the mixer waits for before playing, µs.
    target_us: AtomicU32,
    /// Queued audio at the device's pulls, smoothed, µs.
    queued_us: AtomicU32,
    /// The device's own delay after a pull, µs.
    device_us: AtomicU32,
    /// The margin the mixer keeps, µs.
    margin_us: AtomicU32,
    underruns: AtomicU64,
}

impl Link {
    fn publish(&self, (jitter, burst): (f64, f64)) {
        self.jitter_us
            .store((jitter * 1e6).max(1.0) as u32, Ordering::Relaxed);
        self.burst_us.store((burst * 1e6) as u32, Ordering::Relaxed);
    }
}

/// Where the time between a packet arriving and its sound leaving the
/// speaker goes, as the stats line shows it.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct Latency {
    /// Queued, waiting for the device: the jitter buffer.
    pub queued_ms: f32,
    /// The device's buffer and hardware after a pull.
    pub device_ms: f32,
    /// Arrival jitter over the last few seconds.
    pub jitter_ms: f32,
    pub underruns: u64,
}

pub struct Player {
    channels: usize,
    rate: u32,
    samples_per_frame: usize,
    pcm: Vec<i16>,
    link: Arc<Link>,
    arrivals: Arrivals,
    /// The last frame played, faded out in place of a lost one.
    last: Vec<f32>,
    /// Interleaved samples at the stream's rate and channel count.
    queue: Arc<Mutex<VecDeque<f32>>>,
    /// Dropped with the player, which ends the output thread and with it
    /// the device stream.
    _output_thread: mpsc::Sender<()>,
    output: Arc<Mutex<Output>>,
    /// Frames the output device has pulled, silence included: proof that
    /// sound is leaving.
    pulled: Arc<AtomicU64>,
    gain: Arc<AtomicU32>,
    max_queued: usize,
    packets: u64,
    lost: u64,
    undecodable: u64,
}

impl Player {
    pub fn new(sample_rate: u32, channels: usize, samples_per_frame: usize) -> Result<Self> {
        Self::with_gain(
            sample_rate,
            channels,
            samples_per_frame,
            Arc::new(AtomicU32::new(1.0f32.to_bits())),
        )
    }

    pub fn with_gain(
        sample_rate: u32,
        channels: usize,
        samples_per_frame: usize,
        gain: Arc<AtomicU32>,
    ) -> Result<Self> {
        if channels == 0 || channels > MAX_CHANNELS {
            bail!("audio with {channels} channels is not something Latch plays");
        }
        if !(8_000..=192_000).contains(&sample_rate) {
            bail!("unsupported sample rate {sample_rate}");
        }
        if samples_per_frame == 0 || samples_per_frame > latch_codec::audio::MAX_SAMPLES {
            bail!("invalid audio frame size {samples_per_frame}");
        }
        let queue: Arc<Mutex<VecDeque<f32>>> = Arc::default();
        let (stop, stopped) = mpsc::channel();
        let output = Arc::new(Mutex::new(Output::Opening));
        let pulled = Arc::new(AtomicU64::new(0));
        let link = Arc::new(Link::default());
        output_thread(
            sample_rate,
            channels as u16,
            Shared {
                queue: queue.clone(),
                output: output.clone(),
                pulled: pulled.clone(),
                gain: gain.clone(),
                link: link.clone(),
            },
            stopped,
        );
        let per_ms = sample_rate as usize * channels / 1000;
        Ok(Self {
            channels,
            rate: sample_rate,
            samples_per_frame,
            pcm: Vec::with_capacity(samples_per_frame * channels),
            link,
            arrivals: Arrivals::default(),
            last: Vec::new(),
            queue,
            _output_thread: stop,
            output,
            pulled,
            gain,
            max_queued: per_ms * MAX_QUEUED_MS,
            packets: 0,
            lost: 0,
            undecodable: 0,
        })
    }

    pub fn set_volume(&self, gain: f32) {
        self.gain
            .store(gain.clamp(0.0, 2.0).to_bits(), Ordering::Relaxed);
    }

    pub fn volume(&self) -> f32 {
        f32::from_bits(self.gain.load(Ordering::Relaxed))
    }

    /// Decode and queue one frame. An empty slice means the frame was lost:
    /// the last one is repeated, fading, so a gap is a dip rather than a
    /// click.
    pub fn push(&mut self, packet: &[u8]) {
        self.push_at(packet, Instant::now());
    }

    fn push_at(&mut self, packet: &[u8], now: Instant) {
        if packet.is_empty() {
            self.lost += 1;
            if self.last.is_empty() {
                return;
            }
            let n = self.last.len() as f32;
            for (i, s) in self.last.iter_mut().enumerate() {
                *s *= 0.5 * (1.0 - i as f32 / n);
            }
            self.arrived(now);
            self.queue_samples();
            return;
        }
        match latch_codec::audio::decode_channels(packet, &mut self.pcm) {
            Ok(channels) if channels == self.channels => {}
            Ok(channels) => {
                self.undecodable += 1;
                if self.undecodable == 1 {
                    tracing::warn!(
                        "audio: a {channels}-channel frame in a {}-channel stream",
                        self.channels
                    );
                }
                return;
            }
            Err(e) => {
                self.undecodable += 1;
                if self.undecodable == 1 {
                    tracing::warn!("audio: undecodable frame ({e})");
                }
                return;
            }
        }
        if self.pcm.len() / self.channels > self.samples_per_frame * 4 {
            self.undecodable += 1;
            return;
        }
        self.packets += 1;
        self.last.clear();
        self.last
            .extend(self.pcm.iter().map(|&v| f32::from(v) / 32768.0));
        self.arrived(now);
        self.queue_samples();
    }

    /// Notes `last` arriving now, for the jitter estimate.
    fn arrived(&mut self, now: Instant) {
        let secs = (self.last.len() / self.channels) as f64 / f64::from(self.rate);
        if let Some(measured) = self.arrivals.note(now, secs) {
            self.link.publish(measured);
        }
    }

    fn queue_samples(&mut self) {
        let mut q = self.queue.lock();
        q.extend(&self.last);
        if q.len() > self.max_queued {
            // Back to the mixer's cushion, in whole frames: dropping half a
            // frame would swap the channels.
            let target = self.link.target_us.load(Ordering::Relaxed) as usize;
            let keep = (target * self.rate as usize / 1_000_000).max(1) * self.channels;
            let drop = q.len().saturating_sub(keep);
            q.drain(..drop);
        }
    }

    /// Where the delay after arrival goes.
    pub fn latency(&self) -> Latency {
        let ms = |v: &AtomicU32| v.load(Ordering::Relaxed) as f32 / 1000.0;
        Latency {
            queued_ms: ms(&self.link.queued_us),
            device_ms: ms(&self.link.device_us),
            jitter_ms: ms(&self.link.jitter_us),
            underruns: self.link.underruns.load(Ordering::Relaxed),
        }
    }

    pub fn output(&self) -> Output {
        self.output.lock().clone()
    }

    /// Frames the output device has taken so far.
    pub fn pulled(&self) -> u64 {
        self.pulled.load(Ordering::Relaxed)
    }

    /// One phrase for the stats line: where sound goes, or why it does not.
    pub fn describe(&self) -> String {
        phrase(&self.output.lock(), self.packets, self.lost, self.pulled())
    }
}

/// The stats-line phrase for an output state. `pulled` is the device's own
/// count: a device that has opened but taken nothing is not playing, however
/// many packets the PC sent.
fn phrase(output: &Output, packets: u64, lost: u64, pulled: u64) -> String {
    match output {
        Output::Failed(e) => format!("no sound: {e}"),
        Output::Opening => "audio: opening the output".into(),
        Output::Playing { device, .. } => {
            if packets == 0 {
                // The PC sends nothing while it is silent.
                "no audio from the PC yet".into()
            } else if pulled == 0 {
                format!("no sound: {device} is not taking audio")
            } else if lost > 0 {
                format!("audio → {device} · {lost} lost")
            } else {
                format!("audio → {device}")
            }
        }
    }
}

/// Arrival jitter: how late frames come against the host's own cadence
/// (each frame's media time after the first), as the spread between the
/// earliest and the latest over the last few seconds; and the burst, how
/// much audio typically comes at one moment (a capture period's frames
/// leave together; the median, so that late packets bunching up behind a
/// loss count as jitter, not as bursts). A host that went quiet starts it
/// over: it sends nothing for silence.
#[derive(Default)]
struct Arrivals {
    origin: Option<Instant>,
    media: f64,
    last: Option<Instant>,
    /// Audio that has come since the moment the current burst began.
    burst: f64,
    /// (min, max, median burst) of each finished half-second block,
    /// newest last.
    blocks: VecDeque<(f64, f64, f64)>,
    block: Option<(Instant, f64, f64)>,
    bursts: Vec<f64>,
}

impl Arrivals {
    const BLOCK: Duration = Duration::from_millis(500);
    const BLOCKS: usize = 8;

    /// Frames this close together came at one moment.
    const TOGETHER: Duration = Duration::from_millis(1);

    /// Notes a frame of `secs` arriving at `now`; the jitter and the burst
    /// so far, in seconds, once there is enough to tell.
    fn note(&mut self, now: Instant, secs: f64) -> Option<(f64, f64)> {
        if self
            .last
            .is_some_and(|t| now.saturating_duration_since(t) > PAUSE)
        {
            self.origin = None;
            self.blocks.clear();
            self.block = None;
            self.bursts.clear();
            self.burst = 0.0;
        }
        if self
            .last
            .is_none_or(|t| now.saturating_duration_since(t) > Self::TOGETHER)
        {
            if self.burst > 0.0 {
                self.bursts.push(self.burst);
            }
            self.burst = 0.0;
        }
        self.burst += secs;
        self.last = Some(now);
        let origin = *self.origin.get_or_insert_with(|| {
            self.media = 0.0;
            now
        });
        let offset = now.saturating_duration_since(origin).as_secs_f64() - self.media;
        self.media += secs;
        let (start, min, max) = self.block.get_or_insert((now, offset, offset));
        *min = min.min(offset);
        *max = max.max(offset);
        if now.saturating_duration_since(*start) < Self::BLOCK || self.bursts.is_empty() {
            return None;
        }
        self.bursts.sort_by(f64::total_cmp);
        let burst = self.bursts[self.bursts.len() / 2];
        self.bursts.clear();
        self.blocks.push_back((*min, *max, burst));
        self.block = None;
        if self.blocks.len() > Self::BLOCKS {
            self.blocks.pop_front();
        }
        let low = self.blocks.iter().map(|b| b.0).fold(f64::MAX, f64::min);
        let high = self.blocks.iter().map(|b| b.1).fold(f64::MIN, f64::max);
        let burst = self.blocks.iter().map(|b| b.2).fold(0.0, f64::max);
        Some((high - low, burst))
    }
}

/// What the player shares with the output thread and its mixers.
#[derive(Clone)]
struct Shared {
    queue: Arc<Mutex<VecDeque<f32>>>,
    output: Arc<Mutex<Output>>,
    pulled: Arc<AtomicU64>,
    gain: Arc<AtomicU32>,
    link: Arc<Link>,
}

/// The channel count to ask a host for: the default output device's 5.1 or
/// 7.1 when it has one, else stereo.
pub fn output_channels() -> u32 {
    let channels = cpal::default_host()
        .default_output_device()
        .and_then(|d| d.default_output_config().ok())
        .map_or(2, |c| c.channels());
    match channels {
        8.. => 8,
        6 | 7 => 6,
        _ => 2,
    }
}

/// cpal streams are not `Send` everywhere, so the output stream lives on a
/// thread of its own for as long as the player does, opened again on the
/// current default device when the one it played on goes away.
fn output_thread(source_rate: u32, source_channels: u16, shared: Shared, stop: mpsc::Receiver<()>) {
    std::thread::Builder::new()
        .name("audio-out".into())
        .spawn(move || {
            let rebuild = Arc::new(AtomicBool::new(false));
            keep_open(&stop, &rebuild, OUTPUT_RETRY, || {
                open_output(source_rate, source_channels, &shared, &rebuild)
            });
        })
        .expect("spawn audio thread");
}

/// Holds what `open` returns until `stop` disconnects (the player was
/// dropped). Opens again at once when `rebuild` is raised, and `retry`
/// after `open` failed.
fn keep_open<S>(
    stop: &mpsc::Receiver<()>,
    rebuild: &AtomicBool,
    retry: Duration,
    mut open: impl FnMut() -> Option<S>,
) {
    loop {
        rebuild.store(false, Ordering::Relaxed);
        let stream = open();
        let opened = Instant::now();
        loop {
            match stop.recv_timeout(Duration::from_millis(100)) {
                Err(mpsc::RecvTimeoutError::Timeout) => {}
                _ => return,
            }
            if rebuild.load(Ordering::Relaxed) || (stream.is_none() && opened.elapsed() >= retry) {
                break;
            }
        }
        drop(stream);
    }
}

/// What an error from the output stream means for it. CoreAudio moves a
/// default-device stream to the new default by itself and reports
/// `DeviceChanged`: only the name shown needs to follow (it used to be
/// logged as a warning and the stats kept naming the device sound had
/// left). A stream whose device is gone (`DeviceNotAvailable`; CoreAudio
/// also pauses it when no output is left) or that no longer follows the
/// default (`StreamInvalidated`: WASAPI never moves a stream) is opened
/// again on the current default, at that device's rate and channels.
fn on_output_error(
    e: &cpal::Error,
    output: &Mutex<Output>,
    rebuild: &AtomicBool,
    default_name: impl FnOnce() -> Option<String>,
) {
    match e.kind() {
        cpal::ErrorKind::DeviceChanged => {
            let Some(now) = default_name().filter(|n| !n.is_empty()) else {
                return;
            };
            tracing::info!("audio: now playing on {now}");
            if let Output::Playing { device, .. } = &mut *output.lock() {
                *device = now;
            }
        }
        cpal::ErrorKind::DeviceNotAvailable | cpal::ErrorKind::StreamInvalidated => {
            tracing::info!("audio output: {e}; opening the default output again");
            rebuild.store(true, Ordering::Relaxed);
        }
        _ => tracing::warn!("audio output: {e}"),
    }
}

/// Opens the default output device at its own rate, channel count and
/// sample format, and [`Mixer`] converts; asking a device for 48 kHz stereo
/// f32 it does not offer was silence with the reason only in the log. Asks
/// for a small buffer where the device allows one, and takes its default
/// where that fails.
fn open_output(
    source_rate: u32,
    source_channels: u16,
    shared: &Shared,
    rebuild: &Arc<AtomicBool>,
) -> Option<cpal::Stream> {
    let output = &shared.output;
    let fail = |why: String| {
        let mut state = output.lock();
        // Tried again every second: say it once.
        if !matches!(&*state, Output::Failed(w) if *w == why) {
            tracing::warn!("audio output: {why}");
        }
        *state = Output::Failed(why);
    };
    let Some(device) = cpal::default_host().default_output_device() else {
        fail("this Mac has no audio output device".into());
        return None;
    };
    let name = {
        let s = device.to_string();
        if s.is_empty() {
            "the output device".into()
        } else {
            s
        }
    };
    let supported = match device.default_output_config() {
        Ok(c) => c,
        Err(e) => {
            fail(format!("{name} has no output configuration: {e}"));
            return None;
        }
    };
    let rate = supported.sample_rate().max(8_000);
    let channels = supported.channels().max(1);
    let small = buffer_frames(rate, supported.buffer_size());
    let mixer = || {
        Mixer::new(
            source_rate,
            source_channels as usize,
            rate,
            channels as usize,
            shared.queue.clone(),
            shared.pulled.clone(),
            None,
            shared.gain.clone(),
        )
        .linked(shared.link.clone())
    };
    let err_fn = {
        let (output, rebuild) = (output.clone(), rebuild.clone());
        move |e: cpal::Error| {
            on_output_error(&e, &output, &rebuild, || {
                cpal::default_host()
                    .default_output_device()
                    .map(|d| d.to_string())
            })
        }
    };
    let build = |buffer_size: cpal::BufferSize| {
        let config = cpal::StreamConfig {
            channels,
            sample_rate: rate,
            buffer_size,
        };
        match supported.sample_format() {
            cpal::SampleFormat::F32 => {
                let mut m = mixer();
                device.build_output_stream(
                    config,
                    move |out: &mut [f32], info: &cpal::OutputCallbackInfo| {
                        m.device_delay = device_delay(info);
                        m.fill(out)
                    },
                    err_fn.clone(),
                    None,
                )
            }
            cpal::SampleFormat::I16 => {
                let mut m = mixer();
                device.build_output_stream(
                    config,
                    move |out: &mut [i16], info: &cpal::OutputCallbackInfo| {
                        m.device_delay = device_delay(info);
                        m.fill_i16(out)
                    },
                    err_fn.clone(),
                    None,
                )
            }
            cpal::SampleFormat::I32 => {
                let mut m = mixer();
                device.build_output_stream(
                    config,
                    move |out: &mut [i32], info: &cpal::OutputCallbackInfo| {
                        m.device_delay = device_delay(info);
                        m.fill_i32(out)
                    },
                    err_fn.clone(),
                    None,
                )
            }
            other => {
                // Most leftover formats still accept f32 or i16.
                let mut m = mixer();
                match device.build_output_stream(
                    config,
                    move |out: &mut [f32], info: &cpal::OutputCallbackInfo| {
                        m.device_delay = device_delay(info);
                        m.fill(out)
                    },
                    err_fn.clone(),
                    None,
                ) {
                    Ok(s) => Ok(s),
                    Err(e) => {
                        tracing::warn!("audio output: {other} as f32 failed ({e}); trying i16");
                        let mut m = mixer();
                        device.build_output_stream(
                            config,
                            move |out: &mut [i16], info: &cpal::OutputCallbackInfo| {
                                m.device_delay = device_delay(info);
                                m.fill_i16(out)
                            },
                            err_fn.clone(),
                            None,
                        )
                    }
                }
            }
        }
    };
    let stream = match small {
        Some(frames) => build(cpal::BufferSize::Fixed(frames)).or_else(|e| {
            tracing::info!(
                "audio output: {name} refused a {frames}-frame buffer ({e}); using its own"
            );
            build(cpal::BufferSize::Default)
        }),
        None => build(cpal::BufferSize::Default),
    };
    let stream = match stream {
        Ok(s) => s,
        Err(e) => {
            fail(format!(
                "{name} refused {rate} Hz, {channels} channels: {e}"
            ));
            return None;
        }
    };
    if let Err(e) = stream.play() {
        fail(format!("{name} would not start: {e}"));
        return None;
    }
    tracing::info!(
        "audio: playing on {name} at {rate} Hz, {channels} channels, {} buffer",
        small.map_or("default".into(), |f| format!("{f}-frame"))
    );
    *output.lock() = Output::Playing {
        device: name,
        rate,
        channels,
    };
    Some(stream)
}

/// The small device buffer to ask for, in frames, if the device says which
/// sizes it takes (or the environment asks for one).
fn buffer_frames(rate: u32, range: &cpal::SupportedBufferSize) -> Option<u32> {
    let wanted = match std::env::var("LATCH_AUDIO_BUFFER") {
        Ok(v) if v == "default" => return None,
        Ok(v) => v.parse().ok(),
        Err(_) => None,
    };
    match range {
        cpal::SupportedBufferSize::Range { min, max } => {
            let frames = wanted.unwrap_or((f64::from(rate) * DEVICE_BUFFER_MS / 1000.0) as u32);
            Some(frames.clamp(*min, *max))
        }
        // WASAPI's shared mode takes any size (it pulls at its own period
        // whatever is asked; a smaller ring is less delay).
        cpal::SupportedBufferSize::Unknown if cfg!(windows) || wanted.is_some() => {
            Some(wanted.unwrap_or((f64::from(rate) * DEVICE_BUFFER_MS / 1000.0) as u32))
        }
        cpal::SupportedBufferSize::Unknown => None,
    }
}

/// How long after this callback its first frame reaches the speaker.
fn device_delay(info: &cpal::OutputCallbackInfo) -> Duration {
    let t = info.timestamp();
    t.playback.saturating_duration_since(t.callback)
}

/// `LATCH_AUDIO_PROBE` set: the wall-clock moment each click of a host's
/// `LATCH_TEST_AUDIO=clicks` reaches the speaker, as milliseconds past the
/// quarter second the host made it on. Measurement only.
static PROBE: Mutex<Vec<f32>> = Mutex::new(Vec::new());
const PROBE_EVERY_NS: u128 = 250_000_000;

/// The click delays measured since the last call, in ms.
pub fn take_probe() -> Vec<f32> {
    std::mem::take(&mut *PROBE.lock())
}

/// Pulls stream frames from the queue for the device: linear
/// interpolation between rates, the stream's channels mixed onto the
/// device's speakers ([`latch_codec::audio::mix_matrix`]: 5.1 into
/// stereo headphones, stereo onto the front of a 5.1 set), silence when
/// the queue is empty, and the queue steered as the module says.
struct Mixer {
    queue: Arc<Mutex<VecDeque<f32>>>,
    pulled: Arc<AtomicU64>,
    gain: Arc<AtomicU32>,
    link: Arc<Link>,
    src_channels: usize,
    out_channels: usize,
    /// `mix[out][in]`.
    mix: Vec<Vec<f32>>,
    /// Source frames per output frame: 48000 / 44100 ≈ 1.088.
    step: f64,
    out_rate: f64,
    /// Source samples per millisecond.
    per_ms: f64,
    /// How far from `prev` towards `next` the output is; a whole frame or
    /// more means it is time to pull.
    pos: f64,
    prev: [f32; MAX_CHANNELS],
    next: [f32; MAX_CHANNELS],
    /// A fixed cushion (source samples) instead of the measured one, and no
    /// steering; `Some(0)` in tests plays whatever is queued.
    fixed: Option<usize>,
    started: bool,
    /// The least slack seen lately, source samples: dips are taken at once
    /// and forgotten at [`RELEASE_MS_PER_S`].
    low: f64,
    margin_ms: f64,
    /// Queued source samples, smoothed, for the stats.
    level: f64,
    last_pull: Option<Instant>,
    /// When the queue ran dry, until sound comes back.
    dry_at: Option<Instant>,
    scratch: Vec<f32>,
    /// From the device's callback timestamps.
    device_delay: Duration,
    /// Output frames since the last loud one, while probing.
    probe_quiet: Option<usize>,
}

impl Mixer {
    #[allow(clippy::too_many_arguments)]
    fn new(
        src_rate: u32,
        src_channels: usize,
        out_rate: u32,
        out_channels: usize,
        queue: Arc<Mutex<VecDeque<f32>>>,
        pulled: Arc<AtomicU64>,
        fixed: Option<usize>,
        gain: Arc<AtomicU32>,
    ) -> Self {
        let src_channels = src_channels.clamp(1, MAX_CHANNELS);
        let out_channels = out_channels.max(1);
        let mask = latch_codec::audio::speaker_mask(src_channels);
        Self {
            queue,
            pulled,
            gain,
            link: Arc::default(),
            src_channels,
            out_channels,
            mix: latch_codec::audio::mix_matrix(src_channels, mask, out_channels),
            step: src_rate.max(1) as f64 / out_rate.max(1) as f64,
            out_rate: out_rate.max(1) as f64,
            per_ms: src_rate.max(1) as f64 * src_channels as f64 / 1000.0,
            pos: 1.0,
            prev: [0.0; MAX_CHANNELS],
            next: [0.0; MAX_CHANNELS],
            fixed,
            started: false,
            low: 0.0,
            margin_ms: MARGIN_MS,
            level: 0.0,
            last_pull: None,
            dry_at: None,
            scratch: Vec::new(),
            device_delay: Duration::ZERO,
            probe_quiet: std::env::var_os("LATCH_AUDIO_PROBE").map(|_| 0),
        }
    }

    fn linked(mut self, link: Arc<Link>) -> Self {
        self.link = link;
        self
    }

    /// The measured jitter, and the slack to steer the low point to: the
    /// margin, plus whatever of a burst the jitter does not already spread
    /// over every phase of the device's pulls. (With less jitter than a
    /// burst, the clocks' drift slides arrivals past pulls now and then,
    /// and each time the low point drops by a burst at once.)
    fn plan_ms(&self) -> (f64, f64) {
        let jitter = match self.link.jitter_us.load(Ordering::Relaxed) {
            0 => DEFAULT_JITTER_MS,
            us => f64::from(us) / 1000.0,
        };
        let burst = f64::from(self.link.burst_us.load(Ordering::Relaxed)) / 1000.0;
        (jitter, self.margin_ms + (burst - jitter).max(0.0))
    }

    /// Source samples to have queued before playing: this pull's need,
    /// the measured jitter and the slack.
    fn cushion(&self, need: usize) -> usize {
        if let Some(fixed) = self.fixed {
            return fixed;
        }
        let (jitter, slack) = self.plan_ms();
        let samples = need + ((jitter + slack) * self.per_ms) as usize;
        samples.div_ceil(self.src_channels) * self.src_channels
    }

    fn fill(&mut self, out: &mut [f32]) {
        self.fill_at(out, Instant::now());
    }

    fn fill_at(&mut self, out: &mut [f32], now: Instant) {
        let frames_out = out.len() / self.out_channels;
        // Source samples this pull takes, the interpolation's one ahead
        // included.
        let need = ((frames_out as f64 * self.step).ceil() as usize + 1) * self.src_channels;
        let dt = self
            .last_pull
            .map_or(0.0, |t| now.saturating_duration_since(t).as_secs_f64());
        self.last_pull = Some(now);
        let steer = self.fixed.is_none();
        if steer {
            self.margin_ms = (self.margin_ms - MARGIN_DECAY_MS_PER_S * dt).max(MARGIN_MS);
        }
        let mut q = self.queue.lock();
        if !self.started {
            if let Some(dry) = self.dry_at.filter(|_| !q.is_empty()) {
                // Sound came back: soon means it was late, not paused.
                let gap = now.saturating_duration_since(dry);
                if steer && gap < PAUSE {
                    self.margin_ms = (self.margin_ms + MARGIN_MS).min(MAX_MARGIN_MS);
                    self.link.underruns.fetch_add(1, Ordering::Relaxed);
                }
                self.dry_at = None;
            }
            let cushion = self.cushion(need);
            if q.len() < cushion {
                drop(q);
                out.fill(0.0);
                return;
            }
            self.started = true;
            if cushion > 0 {
                // Out of silence, start from the newest cushion's worth: a
                // backlog a stall left behind would stay as added delay.
                let excess = (q.len() - cushion) / self.src_channels * self.src_channels;
                q.drain(..excess);
            }
            self.low = q.len() as f64 - need as f64;
            self.level = q.len() as f64;
        }
        // Steer the low point of the slack to the margin, so clock drift
        // neither grows the delay nor drains it into a gap, and the delay
        // is no more than this path's jitter needs.
        let step = if steer {
            let slack = q.len() as f64 - need as f64;
            self.low = slack.min(self.low + RELEASE_MS_PER_S * dt * self.per_ms);
            let error = (self.low / self.per_ms - self.plan_ms().1) / TRIM_FULL_MS;
            self.level += (q.len() as f64 - self.level) * dt.min(1.0);
            let us = |samples: f64| (samples / self.per_ms * 1000.0).max(0.0) as u32;
            let link = &self.link;
            link.queued_us.store(us(self.level), Ordering::Relaxed);
            link.target_us
                .store(us(self.cushion(need) as f64), Ordering::Relaxed);
            link.margin_us
                .store((self.margin_ms * 1000.0) as u32, Ordering::Relaxed);
            link.device_us
                .store(self.device_delay.as_micros() as u32, Ordering::Relaxed);
            self.step * (1.0 + (MAX_TRIM * error).clamp(-MAX_TRIM, MAX_TRIM))
        } else {
            self.step
        };
        let gain = f32::from_bits(self.gain.load(Ordering::Relaxed));
        let wall = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            + self.device_delay;
        let mut frames = 0u64;
        for frame in out.chunks_mut(self.out_channels) {
            while self.pos >= 1.0 {
                self.pos -= 1.0;
                self.prev = self.next;
                self.next = [0.0; MAX_CHANNELS];
                if q.len() >= self.src_channels {
                    for c in 0..self.src_channels {
                        self.next[c] = q.pop_front().unwrap_or(0.0);
                    }
                } else if self.started {
                    // Ran dry: rebuild the cushion before playing on, or
                    // every later jitter spike would click too.
                    self.started = false;
                    self.dry_at = Some(now);
                }
            }
            let t = self.pos as f32;
            let mut src = [0.0f32; MAX_CHANNELS];
            for (c, s) in src.iter_mut().enumerate().take(self.src_channels) {
                *s = self.prev[c] + (self.next[c] - self.prev[c]) * t;
            }
            for (o, row) in frame.iter_mut().zip(&self.mix) {
                let mixed: f32 = row.iter().zip(&src).map(|(w, s)| w * s).sum();
                *o = mixed * gain;
            }
            if let Some(quiet) = &mut self.probe_quiet {
                let level = src[0].abs();
                if level > 0.2 && *quiet as f64 >= self.out_rate / 10.0 {
                    let at =
                        wall.as_nanos() + frames as u128 * 1_000_000_000 / self.out_rate as u128;
                    PROBE.lock().push((at % PROBE_EVERY_NS) as f32 / 1e6);
                }
                *quiet = if level < 0.05 { *quiet + 1 } else { 0 };
            }
            self.pos += step;
            frames += 1;
        }
        self.pulled.fetch_add(frames, Ordering::Relaxed);
    }

    fn fill_converted<T>(&mut self, out: &mut [T], conv: impl Fn(f32) -> T) {
        let n = out.len();
        let mut scratch = std::mem::take(&mut self.scratch);
        scratch.resize(n, 0.0);
        self.fill(&mut scratch);
        for (o, s) in out.iter_mut().zip(&scratch) {
            *o = conv(*s);
        }
        self.scratch = scratch;
    }

    fn fill_i16(&mut self, out: &mut [i16]) {
        self.fill_converted(out, |s| (s.clamp(-1.0, 1.0) * 32767.0) as i16);
    }

    fn fill_i32(&mut self, out: &mut [i32]) {
        self.fill_converted(out, |s| (s.clamp(-1.0, 1.0) * 2147483647.0) as i32);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{Duration, Instant};

    #[test]
    fn invalid_audio_configuration_is_rejected_before_allocating() {
        for rate in [0, 1_000, u32::MAX] {
            assert!(Player::new(rate, 2, 480).is_err());
        }
        for frame in [0, latch_codec::audio::MAX_SAMPLES + 1, usize::MAX] {
            assert!(Player::new(48_000, 2, frame).is_err());
        }
        assert!(Player::new(48_000, 0, 480).is_err());
        assert!(Player::new(48_000, MAX_CHANNELS + 1, 480).is_err());
    }

    /// One coded 10 ms stereo frame of a tone, as a host sends it.
    fn tone_packet(frames: usize) -> Vec<u8> {
        let pcm: Vec<i16> = (0..frames)
            .flat_map(|i| {
                let s =
                    ((i as f32 * 440.0 * std::f32::consts::TAU / 48_000.0).sin() * 16_000.0) as i16;
                [s, s]
            })
            .collect();
        let mut out = Vec::new();
        latch_codec::audio::encode(&pcm, &mut out);
        out
    }

    #[test]
    fn coded_frames_become_samples_and_the_device_pulls_them() {
        let mut p = Player::new(48_000, 2, 480).unwrap();
        let packet = tone_packet(480);
        for _ in 0..10 {
            p.push(&packet);
        }
        assert_eq!(p.packets, 10);
        assert_eq!(p.queue.lock().len(), 10 * 480 * 2, "480 stereo frames each");
        // A lost frame is concealed at the stream's frame size.
        p.push(&[]);
        assert_eq!(p.queue.lock().len(), 11 * 480 * 2);
        assert_eq!((p.lost, p.undecodable), (1, 0));
        // Garbage is counted, not played.
        p.push(&[0xff; 7]);
        assert_eq!(p.undecodable, 1);
        // The queue never grows past the cap, and never by a partial frame.
        for _ in 0..100 {
            p.push(&packet);
        }
        let len = p.queue.lock().len();
        assert!(len <= p.max_queued, "{len} > {}", p.max_queued);
        assert_eq!(len % 2, 0);
        // Where there is a device, the output thread must be pulling.
        let start = Instant::now();
        loop {
            match p.output() {
                Output::Opening if start.elapsed() < Duration::from_secs(5) => {
                    std::thread::sleep(Duration::from_millis(50));
                }
                Output::Opening => panic!("the output never opened"),
                Output::Failed(e) => {
                    eprintln!("no output here ({e}); skipping the playback check");
                    break;
                }
                Output::Playing {
                    device,
                    rate,
                    channels,
                } => {
                    assert!(
                        rate >= 8_000 && channels >= 1,
                        "{device}: {rate} Hz, {channels} ch"
                    );
                    let t = Instant::now();
                    while p.pulled() == 0 && t.elapsed() < Duration::from_secs(3) {
                        std::thread::sleep(Duration::from_millis(50));
                    }
                    assert!(p.pulled() > 0, "{device} pulled nothing in 3 s");
                    break;
                }
            }
        }
        let desc = p.describe();
        assert!(desc.contains("audio") || desc.contains("sound"), "{desc}");
    }

    #[test]
    fn the_phrase_never_names_a_device_that_takes_nothing() {
        let playing = Output::Playing {
            device: "MacBook Pro Speakers".into(),
            rate: 48_000,
            channels: 2,
        };
        assert_eq!(phrase(&playing, 0, 0, 0), "no audio from the PC yet");
        assert_eq!(
            phrase(&playing, 120, 0, 0),
            "no sound: MacBook Pro Speakers is not taking audio"
        );
        assert_eq!(phrase(&playing, 120, 0, 1), "audio → MacBook Pro Speakers");
        assert_eq!(
            phrase(&playing, 120, 3, 500),
            "audio → MacBook Pro Speakers · 3 lost"
        );
        assert_eq!(
            phrase(&Output::Opening, 120, 0, 0),
            "audio: opening the output"
        );
        assert_eq!(
            phrase(&Output::Failed("no output device".into()), 120, 0, 0),
            "no sound: no output device"
        );
    }

    #[test]
    fn output_errors_rename_on_a_reroute_and_reopen_on_a_lost_device() {
        let playing = |device: &str| Output::Playing {
            device: device.into(),
            rate: 48_000,
            channels: 2,
        };
        let output = Mutex::new(playing("Speakers"));
        let rebuild = AtomicBool::new(false);
        // CoreAudio already moved the stream to the new default.
        let changed = cpal::ErrorKind::DeviceChanged.into();
        on_output_error(&changed, &output, &rebuild, || Some("Headphones".into()));
        assert_eq!(*output.lock(), playing("Headphones"));
        on_output_error(&cpal::ErrorKind::Xrun.into(), &output, &rebuild, || None);
        assert!(!rebuild.load(Ordering::Relaxed));
        // WASAPI keeps playing on the old device after a new default; the
        // only output unplugged pauses a CoreAudio stream for good.
        for kind in [
            cpal::ErrorKind::StreamInvalidated,
            cpal::ErrorKind::DeviceNotAvailable,
        ] {
            rebuild.store(false, Ordering::Relaxed);
            on_output_error(&kind.into(), &output, &rebuild, || None);
            assert!(rebuild.load(Ordering::Relaxed), "{kind:?}");
        }
    }

    #[test]
    fn the_output_is_opened_again_until_the_player_is_dropped() {
        struct Held(Arc<AtomicU64>);
        impl Drop for Held {
            fn drop(&mut self) {
                self.0.fetch_sub(1, Ordering::Relaxed);
            }
        }
        let (stop, stopped) = mpsc::channel::<()>();
        let rebuild = Arc::new(AtomicBool::new(false));
        let (opens, live) = (Arc::new(AtomicU64::new(0)), Arc::new(AtomicU64::new(0)));
        let thread = {
            let (rebuild, opens, live) = (rebuild.clone(), opens.clone(), live.clone());
            std::thread::spawn(move || {
                keep_open(&stopped, &rebuild, Duration::from_millis(10), || {
                    assert_eq!(
                        live.load(Ordering::Relaxed),
                        0,
                        "the old stream is closed first"
                    );
                    // No device at first, then one.
                    if opens.fetch_add(1, Ordering::Relaxed) == 0 {
                        return None;
                    }
                    live.fetch_add(1, Ordering::Relaxed);
                    Some(Held(live.clone()))
                })
            })
        };
        let wait_for = |n: u64| {
            let start = Instant::now();
            while opens.load(Ordering::Relaxed) < n {
                assert!(
                    start.elapsed() < Duration::from_secs(10),
                    "never opened {n} times"
                );
                std::thread::sleep(Duration::from_millis(5));
            }
        };
        wait_for(2);
        std::thread::sleep(Duration::from_millis(250));
        assert_eq!(opens.load(Ordering::Relaxed), 2, "a playing stream is kept");
        rebuild.store(true, Ordering::Relaxed);
        wait_for(3);
        drop(stop);
        thread.join().unwrap();
        assert_eq!(live.load(Ordering::Relaxed), 0);
    }

    #[test]
    fn the_mixer_resamples_and_maps_channels() {
        // 48 kHz stereo into a 96 kHz mono device: two output frames per
        // source frame, left and right averaged.
        let queue: Arc<Mutex<VecDeque<f32>>> = Arc::default();
        queue
            .lock()
            .extend([1.0, 0.0, 1.0, 0.0, 1.0, 0.0, 1.0, 0.0]);
        let pulled = Arc::new(AtomicU64::new(0));
        let mut m = Mixer::new(
            48_000,
            2,
            96_000,
            1,
            queue.clone(),
            pulled.clone(),
            Some(0),
            Arc::new(AtomicU32::new(1.0f32.to_bits())),
        );
        let mut out = vec![9.0f32; 8];
        m.fill(&mut out);
        assert_eq!(pulled.load(Ordering::Relaxed), 8);
        // After the ramp in from silence, every sample is the L/R mean.
        assert!(out[4..].iter().all(|s| (s - 0.5).abs() < 1e-6), "{out:?}");
        // An empty queue plays silence rather than stale sound.
        let mut out = vec![9.0f32; 16];
        m.fill(&mut out);
        assert!(out[8..].iter().all(|s| *s == 0.0), "{out:?}");
        // Stereo into a six-channel device at the same rate: L, R, silence.
        let queue: Arc<Mutex<VecDeque<f32>>> = Arc::default();
        queue
            .lock()
            .extend(std::iter::repeat_n([0.25f32, -0.25], 20).flatten());
        let mut m = Mixer::new(
            48_000,
            2,
            48_000,
            6,
            queue,
            Arc::new(AtomicU64::new(0)),
            Some(0),
            Arc::new(AtomicU32::new(1.0f32.to_bits())),
        );
        let mut out = vec![9.0f32; 6 * 10];
        m.fill(&mut out);
        assert_eq!(&out[6 * 9..], &[0.25, -0.25, 0.0, 0.0, 0.0, 0.0]);
    }

    /// A path for [`long_session`].
    struct Path {
        /// The host's clock against this device's.
        ppm: f64,
        /// Audio per packet, and how often the host's capture hands it
        /// over (several packets at once when it is longer).
        frame_ms: f64,
        capture_ms: f64,
        /// Network delay: a base and a uniformly random extra up to this.
        jitter_ms: f64,
        /// Device frames per pull.
        callback: usize,
    }

    /// Plays a constant 0.5 for `secs` through `path` into a 48 kHz stereo
    /// device, with the player's jitter estimate, adaptive cushion and
    /// overrun cut, on simulated time. Returns the samples that were not
    /// 0.5 after sound began (gaps), the overrun cuts, and the mean queued
    /// audio plus the pull size in ms over the last minute: the delay the
    /// buffer adds.
    fn long_session(path: &Path, secs: u64) -> (u64, u64, f64) {
        let per_ms = 48 * 2;
        let queue: Arc<Mutex<VecDeque<f32>>> = Arc::default();
        let link = Arc::new(Link::default());
        let mut m = Mixer::new(
            48_000,
            2,
            48_000,
            2,
            queue.clone(),
            Arc::default(),
            None,
            Arc::new(AtomicU32::new(1.0f32.to_bits())),
        )
        .linked(link.clone());
        let mut arrivals = Arrivals::default();
        let mut rng = 0x9e37_79b9_7f4a_7c15u64;
        let mut random = move || {
            rng ^= rng << 13;
            rng ^= rng >> 7;
            rng ^= rng << 17;
            (rng >> 11) as f64 / (1u64 << 53) as f64
        };
        let base = Instant::now();
        let at = |t: f64| base + Duration::from_secs_f64(t);
        let scale = 1.0 / (1.0 + path.ppm * 1e-6);
        let per_capture = (path.capture_ms / path.frame_ms).round().max(1.0) as u64;
        let frame_samples = (48.0 * path.frame_ms) as usize * 2;
        let (mut packet, mut arrival, mut next_callback) = (0u64, 0.0f64, 0.0f64);
        let mut out = vec![0.0f32; path.callback * 2];
        let (mut gaps, mut cuts, mut sounding) = (0u64, 0u64, false);
        let (mut depth_sum, mut depth_n) = (0.0f64, 0u64);
        let end = secs as f64;
        while next_callback < end {
            // Leaves the host when its capture period ends; in order, as the
            // receiver hands packets to the player.
            let captured = (packet / per_capture + 1) as f64 * path.capture_ms / 1000.0 * scale;
            arrival = arrival.max(captured + 0.005 + path.jitter_ms / 1000.0 * random());
            if arrival <= next_callback {
                if let Some(measured) = arrivals.note(at(arrival), path.frame_ms / 1000.0) {
                    link.publish(measured);
                }
                let mut q = queue.lock();
                q.extend(std::iter::repeat_n(0.5f32, frame_samples));
                if q.len() > per_ms * MAX_QUEUED_MS {
                    let keep = link.target_us.load(Ordering::Relaxed) as usize * 48 / 1000 * 2;
                    let drop = q.len() - keep;
                    q.drain(..drop);
                    cuts += 1;
                }
                packet += 1;
                continue;
            }
            let before = queue.lock().len();
            m.fill_at(&mut out, at(next_callback));
            let after = queue.lock().len();
            for &s in &out {
                if s == 0.5 {
                    sounding = true;
                } else if sounding {
                    gaps += 1;
                }
            }
            if next_callback >= end - 60.0 {
                // A sample queued at a pull leaves at the pull's end.
                depth_sum +=
                    (before + after) as f64 / 2.0 / per_ms as f64 + path.callback as f64 / 48.0;
                depth_n += 1;
            }
            next_callback += path.callback as f64 / 48_000.0;
        }
        (gaps, cuts, depth_sum / depth_n.max(1) as f64)
    }

    #[test]
    fn audio_latency_stays_bounded_over_a_long_session_with_clock_drift() {
        // 5.1.0's case: 10 ms frames, up to 10 ms of random delay and
        // 512-frame pulls held 40 ms queued (about 45 ms with the pull).
        for ppm in [-300.0, 0.0, 300.0] {
            let path = Path {
                ppm,
                frame_ms: 10.0,
                capture_ms: 10.0,
                jitter_ms: 10.0,
                callback: 512,
            };
            let (gaps, cuts, ms) = long_session(&path, 30 * 60);
            eprintln!("{ppm:+} ppm: {gaps} gap samples, {cuts} cuts, {ms:.1} ms buffered");
            assert_eq!((gaps, cuts), (0, 0), "{ppm:+} ppm");
            assert!(
                (10.0..=36.0).contains(&ms),
                "{ppm:+} ppm: {ms:.1} ms buffered"
            );
        }
    }

    #[test]
    fn a_quiet_path_with_small_pulls_buffers_a_few_milliseconds() {
        // A LAN with 1 ms of jitter into 120-frame pulls: 5 ms frames from
        // 5 ms capture periods, and from WASAPI's usual 10 ms ones (two
        // frames at once).
        for (capture_ms, most) in [(5.0, 14.0), (10.0, 20.0)] {
            for ppm in [-300.0, -20.0, 300.0] {
                let path = Path {
                    ppm,
                    frame_ms: 5.0,
                    capture_ms,
                    jitter_ms: 1.0,
                    callback: 120,
                };
                let (gaps, cuts, ms) = long_session(&path, 10 * 60);
                eprintln!(
                    "{capture_ms} ms capture, {ppm:+} ppm: {gaps} gap samples, {cuts} cuts, {ms:.1} ms buffered"
                );
                assert_eq!((gaps, cuts), (0, 0), "{capture_ms} ms, {ppm:+} ppm");
                assert!(
                    ms <= most,
                    "{capture_ms} ms, {ppm:+} ppm: {ms:.1} ms buffered"
                );
            }
        }
    }

    #[test]
    fn after_a_stall_the_mixer_restarts_at_its_preroll_not_the_backlog() {
        // Ten stereo frames numbered 0..9 piled up; the preroll is four.
        let queue: Arc<Mutex<VecDeque<f32>>> = Arc::default();
        queue.lock().extend((0..10).flat_map(|i| [i as f32; 2]));
        let mut m = Mixer::new(
            48_000,
            2,
            48_000,
            2,
            queue.clone(),
            Arc::default(),
            Some(8),
            Arc::new(AtomicU32::new(1.0f32.to_bits())),
        );
        let mut out = vec![9.0f32; 8];
        m.fill(&mut out);
        // Frames 0..5 are skipped rather than kept as added delay.
        assert_eq!(out, [0.0, 0.0, 6.0, 6.0, 7.0, 7.0, 8.0, 8.0]);
    }

    #[test]
    fn the_mixer_waits_for_its_preroll_again_after_running_dry() {
        // Stereo at the device's rate with a four-frame preroll.
        let queue: Arc<Mutex<VecDeque<f32>>> = Arc::default();
        let mut m = Mixer::new(
            48_000,
            2,
            48_000,
            2,
            queue.clone(),
            Arc::default(),
            Some(8),
            Arc::new(AtomicU32::new(1.0f32.to_bits())),
        );
        queue.lock().extend([0.5; 8]);
        let mut out = vec![9.0f32; 16];
        m.fill(&mut out);
        assert!(out.contains(&0.5), "{out:?}");
        // A gap drained it. What trickles in next waits for a full preroll
        // so the following jitter does not click too.
        queue.lock().extend([0.5; 4]);
        let mut out = vec![9.0f32; 4];
        m.fill(&mut out);
        assert_eq!(out, [0.0; 4]);
        assert_eq!(queue.lock().len(), 4);
        queue.lock().extend([0.5; 4]);
        m.fill(&mut out);
        assert!(out.contains(&0.5), "playing again: {out:?}");
    }
    #[test]
    fn mixer_applies_atomic_gain_and_mute() {
        let queue: Arc<Mutex<VecDeque<f32>>> = Arc::default();
        queue
            .lock()
            .extend(std::iter::repeat_n([0.8f32, 0.8], 30).flatten());
        let gain = Arc::new(AtomicU32::new(0.5f32.to_bits()));
        let mut m = Mixer::new(
            48_000,
            2,
            48_000,
            2,
            queue.clone(),
            Arc::new(AtomicU64::new(0)),
            Some(0),
            gain.clone(),
        );
        let mut out = vec![0.0f32; 20];
        m.fill(&mut out);
        // 0.8 * 0.5 = 0.4
        assert!(
            out[4..].iter().all(|s| (s - 0.4).abs() < 1e-5),
            "expected 0.4 with 0.5 gain: {out:?}"
        );

        // Mute by setting gain to 0.0
        gain.store(0.0f32.to_bits(), Ordering::Relaxed);
        let mut out = vec![0.0f32; 20];
        m.fill(&mut out);
        assert!(
            out.iter().all(|s| *s == 0.0),
            "expected silence when muted: {out:?}"
        );
    }
}
