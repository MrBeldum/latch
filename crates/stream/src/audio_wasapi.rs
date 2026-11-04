//! Windows output through WASAPI directly: a shared-mode stream at the
//! audio engine's smallest period (IAudioClient3), where cpal's stays at
//! the engine's default 10 ms whatever buffer it asks for. Drivers that
//! support it (most since Windows 10 with the inbox HDA driver, many
//! vendors' too) run the engine at 128–144 frames (2.67–3 ms at 48 kHz)
//! for as long as such a stream is open; others offer only their default,
//! which is then what this asks for. Any failure leaves the output to cpal.

use super::Mixer;
use crate::speakers;
use anyhow::{bail, Context, Result};
use latch_codec::audio::Speaker;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc, Arc};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};
use windows::core::{HSTRING, PCWSTR};
use windows::Win32::Foundation::{CloseHandle, HANDLE, WAIT_OBJECT_0};
use windows::Win32::Media::Audio::*;
use windows::Win32::Media::KernelStreaming::{KSDATAFORMAT_SUBTYPE_PCM, WAVE_FORMAT_EXTENSIBLE};
use windows::Win32::Media::Multimedia::{KSDATAFORMAT_SUBTYPE_IEEE_FLOAT, WAVE_FORMAT_IEEE_FLOAT};
use windows::Win32::System::Com::*;
use windows::Win32::System::Threading::{
    AvRevertMmThreadCharacteristics, AvSetMmThreadCharacteristicsW, CreateEventW,
    WaitForSingleObject,
};

/// What the stream was opened as.
pub(super) struct Opened {
    pub rate: u32,
    pub channels: u16,
    pub layout: Vec<Speaker>,
    /// The engine period this stream runs at, and the endpoint's default
    /// and smallest, in frames.
    pub period: u32,
    pub default_period: u32,
    pub min_period: u32,
}

/// The render thread, stopped and joined when dropped.
pub(super) struct Render {
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

impl Drop for Render {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

/// How many engine periods to keep written ahead of the device:
/// `LATCH_AUDIO_PERIODS`, 2 by default (one playing, one ready).
fn periods_ahead() -> u32 {
    std::env::var("LATCH_AUDIO_PERIODS")
        .ok()
        .and_then(|v| v.parse().ok())
        .filter(|n| (1..=16).contains(n))
        .unwrap_or(2)
}

/// Opens `endpoint` (an IMMDevice id, as cpal names it) and plays what
/// `mixer` makes until the [`Render`] is dropped. `lost` is raised when the
/// device goes away, or (`follow_default`) stops being the default.
pub(super) fn open(
    endpoint: String,
    follow_default: bool,
    mixer: impl FnOnce(u32, &[Speaker]) -> Mixer + Send + 'static,
    lost: Arc<AtomicBool>,
) -> Result<(Render, Opened)> {
    let stop = Arc::new(AtomicBool::new(false));
    let (tx, rx) = mpsc::sync_channel(1);
    let thread = {
        let stop = stop.clone();
        std::thread::Builder::new()
            .name("audio-wasapi".into())
            .spawn(move || {
                // SAFETY: COM objects are created and used on this thread
                // only; CoUninitialize after they are dropped.
                unsafe {
                    let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
                }
                match Stream::open(&endpoint) {
                    Ok(mut stream) => {
                        let _ = tx.send(Ok(stream.opened()));
                        let mut m = mixer(stream.rate, &stream.layout);
                        if let Err(e) = stream.run(&mut m, &stop, follow_default) {
                            tracing::info!("audio output: {e:#}; opening it again");
                        }
                        if !stop.load(Ordering::Relaxed) {
                            lost.store(true, Ordering::Relaxed);
                        }
                    }
                    Err(e) => {
                        let _ = tx.send(Err(e));
                    }
                }
                // SAFETY: Balances the CoInitializeEx above.
                unsafe { CoUninitialize() };
            })
            .context("spawn the WASAPI thread")?
    };
    let render = Render {
        stop,
        thread: Some(thread),
    };
    match rx.recv() {
        Ok(Ok(opened)) => Ok((render, opened)),
        Ok(Err(e)) => Err(e),
        Err(_) => bail!("the WASAPI thread ended before opening"),
    }
}

struct Event(HANDLE);
impl Drop for Event {
    fn drop(&mut self) {
        // SAFETY: This wrapper uniquely owns the event handle.
        unsafe {
            let _ = CloseHandle(self.0);
        }
    }
}

struct Stream {
    enumerator: IMMDeviceEnumerator,
    endpoint: String,
    client: IAudioClient3,
    render: IAudioRenderClient,
    clock: IAudioClock,
    event: Event,
    rate: u32,
    channels: u16,
    float: bool,
    layout: Vec<Speaker>,
    buffer: u32,
    period: u32,
    default_period: u32,
    min_period: u32,
    latency: Duration,
}

impl Stream {
    fn open(endpoint: &str) -> Result<Self> {
        // SAFETY: Called on the render thread with COM initialized; the mix
        // format is copied out and freed once, after its last use.
        unsafe {
            let enumerator: IMMDeviceEnumerator =
                CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL)
                    .context("create audio device enumerator")?;
            let device = enumerator
                .GetDevice(&HSTRING::from(endpoint))
                .context("find the output endpoint")?;
            let client: IAudioClient3 = device
                .Activate(CLSCTX_ALL, None)
                .context("IAudioClient3 (Windows 10 or later)")?;
            let format = client.GetMixFormat().context("mix format")?;
            if format.is_null() {
                bail!("no mix format");
            }
            let opened = Self::init(&client, format);
            let mask = speakers_of(format);
            CoTaskMemFree(Some(format.cast()));
            let (rate, channels, float, period, default_period, min_period) = opened?;
            let event = Event(CreateEventW(None, false, false, PCWSTR::null())?);
            client.SetEventHandle(event.0).context("set the event")?;
            let buffer = client.GetBufferSize().context("buffer size")?;
            let render: IAudioRenderClient = client.GetService().context("render service")?;
            let clock: IAudioClock = client.GetService().context("clock service")?;
            let latency = client.GetStreamLatency().unwrap_or(0);
            Ok(Self {
                enumerator,
                endpoint: endpoint.to_string(),
                client,
                render,
                clock,
                event,
                rate,
                channels,
                float,
                layout: layout_of(mask, usize::from(channels)),
                buffer,
                period,
                default_period,
                min_period,
                latency: Duration::from_nanos(latency.max(0) as u64 * 100),
            })
        }
    }

    /// Initializes `client` at the smallest period the engine allows for
    /// `format`, or the period it already runs at when another stream
    /// holds it; returns rate, channels, float, period, default, minimum.
    unsafe fn init(
        client: &IAudioClient3,
        format: *const WAVEFORMATEX,
    ) -> Result<(u32, u16, bool, u32, u32, u32)> {
        let wave = format.read_unaligned();
        let (tag, rate, channels, bits) = (
            wave.wFormatTag,
            wave.nSamplesPerSec,
            wave.nChannels,
            wave.wBitsPerSample,
        );
        let sub = (tag == WAVE_FORMAT_EXTENSIBLE as u16 && wave.cbSize >= 22).then(|| {
            (format as *const WAVEFORMATEXTENSIBLE)
                .read_unaligned()
                .SubFormat
        });
        let float =
            tag == WAVE_FORMAT_IEEE_FLOAT as u16 || sub == Some(KSDATAFORMAT_SUBTYPE_IEEE_FLOAT);
        let pcm = tag == WAVE_FORMAT_PCM as u16 || sub == Some(KSDATAFORMAT_SUBTYPE_PCM);
        if !((float && bits == 32) || (pcm && bits == 16)) || channels == 0 || rate == 0 {
            bail!("mix format {tag:#x}, {bits} bits: not one this plays");
        }
        let (mut default, mut fundamental, mut min, mut max) = (0, 0, 0, 0);
        client
            .GetSharedModeEnginePeriod(format, &mut default, &mut fundamental, &mut min, &mut max)
            .context("engine periods")?;
        let wanted = match std::env::var("LATCH_AUDIO_PERIOD")
            .ok()
            .and_then(|v| v.parse().ok())
        {
            // A multiple of the fundamental within the range.
            Some(frames) => {
                let f: u32 = fundamental.max(1);
                (u32::max(frames, min) / f * f).clamp(min, max)
            }
            None => min,
        };
        let mut period = wanted;
        let first = client.InitializeSharedAudioStream(
            AUDCLNT_STREAMFLAGS_EVENTCALLBACK,
            period,
            format,
            None,
        );
        if let Err(e) = first {
            // Another stream fixed the engine's period: join it.
            if e.code() != AUDCLNT_E_ENGINE_PERIODICITY_LOCKED {
                return Err(e).context(format!("low-latency stream at {period} frames"));
            }
            let mut current_format = std::ptr::null_mut();
            client
                .GetCurrentSharedModeEnginePeriod(&mut current_format, &mut period)
                .context("current engine period")?;
            CoTaskMemFree(Some(current_format.cast()));
            client
                .InitializeSharedAudioStream(
                    AUDCLNT_STREAMFLAGS_EVENTCALLBACK,
                    period,
                    format,
                    None,
                )
                .context(format!("stream at the engine's {period} frames"))?;
        }
        Ok((rate, channels, float, period, default, min))
    }

    fn opened(&self) -> Opened {
        Opened {
            rate: self.rate,
            channels: self.channels,
            layout: self.layout.clone(),
            period: self.period,
            default_period: self.default_period,
            min_period: self.min_period,
        }
    }

    /// Keeps `periods_ahead` periods written ahead of the device, woken
    /// by the engine each period, until `stop`; an error is the device's.
    fn run(&mut self, mixer: &mut Mixer, stop: &AtomicBool, follow_default: bool) -> Result<()> {
        let ahead = (self.period * periods_ahead()).min(self.buffer);
        let channels = usize::from(self.channels);
        let task = HSTRING::from("Pro Audio");
        let mut index = 0u32;
        // SAFETY: Everything below is this thread's COM objects; the
        // buffer GetBuffer returns holds `frames × channels` samples of the
        // mix format until ReleaseBuffer.
        unsafe {
            let mmcss = AvSetMmThreadCharacteristicsW(&task, &mut index).ok();
            let frequency = self.clock.GetFrequency().unwrap_or(0).max(1);
            let mut written: u64 = 0;
            let mut check_at = Instant::now();
            let mut scratch = Vec::<f32>::new();
            let result = (|| -> Result<()> {
                self.client.Start().context("start")?;
                while !stop.load(Ordering::Relaxed) {
                    if WaitForSingleObject(self.event.0, 200) != WAIT_OBJECT_0 {
                        // No wake for 200 ms: the engine stopped pulling.
                        self.client.GetCurrentPadding().context("device")?;
                    }
                    if follow_default && check_at.elapsed() >= Duration::from_secs(1) {
                        check_at = Instant::now();
                        if !self.is_default() {
                            bail!("the default output changed");
                        }
                    }
                    let padding = self.client.GetCurrentPadding().context("device")?;
                    let frames = ahead.saturating_sub(padding);
                    if frames == 0 {
                        continue;
                    }
                    // As cpal stamps it: what is written but not played,
                    // and the stream's latency, until the first new frame
                    // plays.
                    let mut position = 0u64;
                    let played = match self.clock.GetPosition(&mut position, None) {
                        Ok(()) => position as u128 * 1_000_000_000 / u128::from(frequency),
                        Err(_) => 0,
                    };
                    let queued = (u128::from(written) * 1_000_000_000 / u128::from(self.rate))
                        .saturating_sub(played);
                    mixer.device_delay = Duration::from_nanos(queued as u64) + self.latency;
                    let data = self.render.GetBuffer(frames).context("render buffer")?;
                    let n = frames as usize * channels;
                    if self.float {
                        mixer.fill(std::slice::from_raw_parts_mut(data.cast::<f32>(), n));
                    } else {
                        scratch.resize(n, 0.0);
                        mixer.fill(&mut scratch);
                        let out = std::slice::from_raw_parts_mut(data.cast::<i16>(), n);
                        for (o, s) in out.iter_mut().zip(&scratch) {
                            *o = (s.clamp(-1.0, 1.0) * 32767.0) as i16;
                        }
                    }
                    self.render.ReleaseBuffer(frames, 0).context("release")?;
                    written += u64::from(frames);
                }
                Ok(())
            })();
            let _ = self.client.Stop();
            if let Some(h) = mmcss {
                let _ = AvRevertMmThreadCharacteristics(h);
            }
            result
        }
    }

    fn is_default(&self) -> bool {
        // SAFETY: GetId's string is copied, then freed.
        unsafe {
            let Ok(device) = self.enumerator.GetDefaultAudioEndpoint(eRender, eConsole) else {
                return false;
            };
            let Ok(id) = device.GetId() else {
                return true;
            };
            let same = id.to_string().map_or(true, |s| s == self.endpoint);
            CoTaskMemFree(Some(id.0.cast()));
            same
        }
    }
}

/// The mix format's speakers, when its channel mask names them.
unsafe fn speakers_of(format: *const WAVEFORMATEX) -> Option<u32> {
    let wave = format.read_unaligned();
    (wave.wFormatTag == WAVE_FORMAT_EXTENSIBLE as u16 && wave.cbSize >= 22)
        .then(|| {
            (format as *const WAVEFORMATEXTENSIBLE)
                .read_unaligned()
                .dwChannelMask
        })
        .filter(|m| *m != 0)
}

fn layout_of(mask: Option<u32>, channels: usize) -> Vec<Speaker> {
    speakers::fit(mask.map(|m| speakers::from_mask(m, channels)), channels)
}
