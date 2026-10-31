//! Coded audio frames in, sound out. Decoding (`lpcm-rice`, lossless, a few
//! microseconds a frame) happens on the stream's receive thread; the OS
//! pulls from a ring buffer on its own thread, resampled to whatever rate
//! the output device runs at and spread over its channels. If the buffer
//! grows past [`MAX_QUEUED_MS`] the oldest audio is dropped: latency matters
//! more than never skipping. Below that, playback speed is trimmed a hair to
//! hold the preroll's worth queued as the two machines' clocks drift. What
//! the output side found is kept, so "no sound" comes with its reason.

use anyhow::{bail, Result};
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use parking_lot::Mutex;
use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::mpsc;
use std::sync::Arc;
use std::time::{Duration, Instant};

/// Queued sound beyond this is cut back to [`KEEP_MS`]. Low, so sound stays
/// close to the picture; a Wi-Fi hiccup longer than this is a click.
const MAX_QUEUED_MS: usize = 120;
const KEEP_MS: usize = 50;
const MAX_CHANNELS: usize = 8;
/// Most playback is sped up or slowed down to hold the preroll's worth
/// queued while the host's and this device's clocks drift apart: 0.2 %, a
/// pitch change too small to hear and ten times the drift of two ordinary
/// sound clocks.
const MAX_TRIM: f64 = 0.002;
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

pub struct Player {
    channels: usize,
    samples_per_frame: usize,
    pcm: Vec<i16>,
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
    keep: usize,
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
        output_thread(
            sample_rate,
            channels as u16,
            queue.clone(),
            stopped,
            output.clone(),
            pulled.clone(),
            gain.clone(),
        );
        let per_ms = sample_rate as usize * channels / 1000;
        // Whole frames only: dropping half a frame would swap the channels.
        let keep = per_ms * KEEP_MS / channels * channels;
        Ok(Self {
            channels,
            samples_per_frame,
            pcm: Vec::with_capacity(samples_per_frame * channels),
            last: Vec::new(),
            queue,
            _output_thread: stop,
            output,
            pulled,
            gain,
            max_queued: per_ms * MAX_QUEUED_MS,
            keep,
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
        if packet.is_empty() {
            self.lost += 1;
            if self.last.is_empty() {
                return;
            }
            let n = self.last.len() as f32;
            for (i, s) in self.last.iter_mut().enumerate() {
                *s *= 0.5 * (1.0 - i as f32 / n);
            }
            self.queue_samples();
            return;
        }
        if let Err(e) = latch_codec::audio::decode(packet, &mut self.pcm) {
            self.undecodable += 1;
            if self.undecodable == 1 {
                tracing::warn!("audio: undecodable frame ({e})");
            }
            return;
        }
        if !self.pcm.len().is_multiple_of(self.channels)
            || self.pcm.len() / self.channels > self.samples_per_frame * 4
        {
            self.undecodable += 1;
            return;
        }
        self.packets += 1;
        self.last.clear();
        self.last
            .extend(self.pcm.iter().map(|&v| f32::from(v) / 32768.0));
        self.queue_samples();
    }

    fn queue_samples(&mut self) {
        let mut q = self.queue.lock();
        q.extend(&self.last);
        if q.len() > self.max_queued {
            let drop = q.len() - self.keep;
            q.drain(..drop);
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

/// cpal streams are not `Send` everywhere, so the output stream lives on a
/// thread of its own for as long as the player does, opened again on the
/// current default device when the one it played on goes away.
fn output_thread(
    source_rate: u32,
    source_channels: u16,
    queue: Arc<Mutex<VecDeque<f32>>>,
    stop: mpsc::Receiver<()>,
    output: Arc<Mutex<Output>>,
    pulled: Arc<AtomicU64>,
    gain: Arc<AtomicU32>,
) {
    std::thread::Builder::new()
        .name("audio-out".into())
        .spawn(move || {
            let rebuild = Arc::new(AtomicBool::new(false));
            keep_open(&stop, &rebuild, OUTPUT_RETRY, || {
                open_output(
                    source_rate,
                    source_channels,
                    &queue,
                    &output,
                    &pulled,
                    &rebuild,
                    &gain,
                )
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
/// f32 it does not offer was silence with the reason only in the log.
#[allow(clippy::too_many_arguments)]
fn open_output(
    source_rate: u32,
    source_channels: u16,
    queue: &Arc<Mutex<VecDeque<f32>>>,
    output: &Arc<Mutex<Output>>,
    pulled: &Arc<AtomicU64>,
    rebuild: &Arc<AtomicBool>,
    gain: &Arc<AtomicU32>,
) -> Option<cpal::Stream> {
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
    let config = cpal::StreamConfig {
        channels,
        sample_rate: rate,
        buffer_size: cpal::BufferSize::Default,
    };
    let mixer = || {
        Mixer::new(
            source_rate,
            source_channels as usize,
            rate,
            channels as usize,
            queue.clone(),
            pulled.clone(),
            source_rate as usize * source_channels as usize * 40 / 1000,
            gain.clone(),
        )
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
    let stream = match supported.sample_format() {
        cpal::SampleFormat::F32 => {
            let mut m = mixer();
            device.build_output_stream(
                config,
                move |out: &mut [f32], _| m.fill(out),
                err_fn.clone(),
                None,
            )
        }
        cpal::SampleFormat::I16 => {
            let mut m = mixer();
            device.build_output_stream(
                config,
                move |out: &mut [i16], _| m.fill_i16(out),
                err_fn.clone(),
                None,
            )
        }
        cpal::SampleFormat::I32 => {
            let mut m = mixer();
            device.build_output_stream(
                config,
                move |out: &mut [i32], _| m.fill_i32(out),
                err_fn.clone(),
                None,
            )
        }
        other => {
            // Most leftover formats still accept f32 or i16.
            let mut m = mixer();
            match device.build_output_stream(
                config,
                move |out: &mut [f32], _| m.fill(out),
                err_fn.clone(),
                None,
            ) {
                Ok(s) => Ok(s),
                Err(e) => {
                    tracing::warn!("audio output: {other} as f32 failed ({e}); trying i16");
                    let mut m = mixer();
                    device.build_output_stream(
                        config,
                        move |out: &mut [i16], _| m.fill_i16(out),
                        err_fn.clone(),
                        None,
                    )
                }
            }
        }
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
    tracing::info!("audio: playing on {name} at {rate} Hz, {channels} channels");
    *output.lock() = Output::Playing {
        device: name,
        rate,
        channels,
    };
    Some(stream)
}

/// Pulls stream frames from the queue for the device: linear
/// interpolation between rates, left and right onto whatever channels the
/// device has (the mean onto a mono one), silence when the queue is empty.
struct Mixer {
    queue: Arc<Mutex<VecDeque<f32>>>,
    pulled: Arc<AtomicU64>,
    gain: Arc<AtomicU32>,
    src_channels: usize,
    out_channels: usize,
    /// Source frames per output frame: 48000 / 44100 ≈ 1.088.
    step: f64,
    out_rate: f64,
    /// Queued samples, smoothed over about a second.
    level: f64,
    /// How far from `prev` towards `next` the output is; a whole frame or
    /// more means it is time to pull.
    pos: f64,
    prev: [f32; MAX_CHANNELS],
    next: [f32; MAX_CHANNELS],
    /// Source samples that must be queued before the first audible frame,
    /// and again after the queue runs dry, so a late packet is not a click
    /// of silence. Zero in tests.
    preroll: usize,
    started: bool,
    scratch: Vec<f32>,
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
        preroll: usize,
        gain: Arc<AtomicU32>,
    ) -> Self {
        Self {
            queue,
            pulled,
            gain,
            src_channels: src_channels.clamp(1, MAX_CHANNELS),
            out_channels: out_channels.max(1),
            step: src_rate.max(1) as f64 / out_rate.max(1) as f64,
            out_rate: out_rate.max(1) as f64,
            level: 0.0,
            pos: 1.0,
            prev: [0.0; MAX_CHANNELS],
            next: [0.0; MAX_CHANNELS],
            preroll,
            started: false,
            scratch: Vec::new(),
        }
    }

    fn fill(&mut self, out: &mut [f32]) {
        let mut q = self.queue.lock();
        if !self.started {
            if q.len() < self.preroll {
                drop(q);
                out.fill(0.0);
                return;
            }
            self.started = true;
            if self.preroll > 0 {
                // Out of silence, start from the newest preroll's worth: a
                // backlog a stall left behind would stay as added delay.
                let excess = (q.len() - self.preroll) / self.src_channels * self.src_channels;
                q.drain(..excess);
            }
            self.level = q.len() as f64;
        }
        // Steer towards keeping the preroll queued, so clock drift neither
        // grows the delay until the overrun cut nor drains it into a gap.
        // Packets arrive in bursts: steer by the average over a second.
        let step = if self.preroll == 0 {
            self.step
        } else {
            let frames = (out.len() / self.out_channels) as f64;
            self.level += (q.len() as f64 - self.level) * (frames / self.out_rate).min(1.0);
            let error = self.level / self.preroll as f64 - 1.0;
            self.step * (1.0 + (2.0 * MAX_TRIM * error).clamp(-MAX_TRIM, MAX_TRIM))
        };
        let gain = f32::from_bits(self.gain.load(Ordering::Relaxed));
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
                } else {
                    // Ran dry: rebuild the cushion before playing on, or
                    // every later jitter spike would click too.
                    self.started = false;
                }
            }
            let t = self.pos as f32;
            let sample = |c: usize| self.prev[c] + (self.next[c] - self.prev[c]) * t;
            for (c, o) in frame.iter_mut().enumerate() {
                let sample = if self.out_channels == 1 && self.src_channels > 1 {
                    (0..self.src_channels).map(sample).sum::<f32>() / self.src_channels as f32
                } else if c < self.src_channels {
                    sample(c)
                } else if self.src_channels == 1 {
                    sample(0)
                } else {
                    0.0
                };
                *o = sample * gain;
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
            0,
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
            0,
            Arc::new(AtomicU32::new(1.0f32.to_bits())),
        );
        let mut out = vec![9.0f32; 6 * 10];
        m.fill(&mut out);
        assert_eq!(&out[6 * 9..], &[0.25, -0.25, 0.0, 0.0, 0.0, 0.0]);
    }

    /// Plays a constant 0.5 for `secs` from a host whose clock runs `ppm`
    /// fast, sending 10 ms of stereo every 10 ms of its time with up to
    /// 10 ms of random delay, into a 48 kHz stereo device that pulls 512
    /// frames per callback, with the player's preroll and overrun cut.
    /// Returns the samples that were not 0.5 after sound began (gaps), the
    /// overrun cuts, and the mean queued audio in ms over the last minute.
    fn long_session(ppm: f64, secs: u64) -> (u64, u64, f64) {
        const CALLBACK: usize = 512;
        let per_ms = 48 * 2;
        let queue: Arc<Mutex<VecDeque<f32>>> = Arc::default();
        let preroll = per_ms * 40;
        let mut m = Mixer::new(
            48_000,
            2,
            48_000,
            2,
            queue.clone(),
            Arc::default(),
            preroll,
            Arc::new(AtomicU32::new(1.0f32.to_bits())),
        );
        let mut rng = 0x9e37_79b9_7f4a_7c15u64;
        let mut random = move || {
            rng ^= rng << 13;
            rng ^= rng >> 7;
            rng ^= rng << 17;
            (rng >> 11) as f64 / (1u64 << 53) as f64
        };
        let send_every = 0.010 / (1.0 + ppm * 1e-6);
        let (mut sent, mut arrival, mut next_callback) = (0.0f64, 0.0f64, 0.0f64);
        let mut out = vec![0.0f32; CALLBACK * 2];
        let (mut gaps, mut cuts, mut sounding) = (0u64, 0u64, false);
        let (mut depth_sum, mut depth_n) = (0.0f64, 0u64);
        let end = secs as f64;
        while next_callback < end {
            // In order, as the receiver hands packets to the player.
            arrival = arrival.max(sent + 0.005 + 0.010 * random());
            if arrival <= next_callback {
                let mut q = queue.lock();
                q.extend(std::iter::repeat_n(0.5f32, 480 * 2));
                if q.len() > per_ms * MAX_QUEUED_MS {
                    let drop = q.len() - per_ms * KEEP_MS;
                    q.drain(..drop);
                    cuts += 1;
                }
                sent += send_every;
                continue;
            }
            let before = queue.lock().len();
            m.fill(&mut out);
            let after = queue.lock().len();
            for &s in &out {
                if s == 0.5 {
                    sounding = true;
                } else if sounding {
                    gaps += 1;
                }
            }
            if next_callback >= end - 60.0 {
                depth_sum += (before + after) as f64 / 2.0 / per_ms as f64;
                depth_n += 1;
            }
            next_callback += CALLBACK as f64 / 48_000.0;
        }
        (gaps, cuts, depth_sum / depth_n.max(1) as f64)
    }

    #[test]
    fn audio_latency_stays_bounded_over_a_long_session_with_clock_drift() {
        for ppm in [-300.0, 0.0, 300.0] {
            let (gaps, cuts, ms) = long_session(ppm, 30 * 60);
            eprintln!("{ppm:+} ppm: {gaps} gap samples, {cuts} cuts, {ms:.1} ms queued");
            assert_eq!((gaps, cuts), (0, 0), "{ppm:+} ppm");
            assert!(
                (25.0..=45.0).contains(&ms),
                "{ppm:+} ppm: {ms:.1} ms queued"
            );
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
            8,
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
            8,
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
            0,
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
