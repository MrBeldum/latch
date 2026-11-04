//! Windows capture and media objects live on MTA workers. The session may
//! move its trait objects between threads without moving COM apartments.
mod amf;
mod audio;
mod capture;
mod display;
mod elevated;
mod encode;
mod input;
mod mf;
mod runtime;
mod vigem;

use crate::{
    audio::AudioCapture,
    input::InputSink,
    software::SoftwarePipeline,
    video::{Codec, EncodedVideo, Pacer, VideoConfig, VideoInfo, VideoPipeline},
};
use anyhow::{bail, Context, Result};
use capture::{Capture, Converter};
use encode::Encoder as MfEncoder;
use runtime::Runtime;
use std::{
    collections::VecDeque,
    sync::mpsc,
    thread,
    time::{Duration, Instant},
};
use windows::{core::Interface, Win32::Media::MediaFoundation::IMFSample};

/// How long a capture waits for the screen to change, at most. The wait
/// returns the moment it does; this only bounds how long the loop goes
/// without looking at its deadlines.
const MAX_WAIT: Duration = Duration::from_millis(100);

enum VideoEncoder {
    Mf(Box<MfEncoder>),
    Amf(Box<amf::Encoder>),
}
impl VideoEncoder {
    fn new(config: &VideoConfig, gpu: &capture::Gpu) -> Result<Self> {
        let selected = std::env::var("LATCH_WINDOWS_ENCODER").unwrap_or_default();
        // SAFETY: the device owns the adapter returned by DXGI.
        let amd = unsafe {
            gpu.device
                .cast::<windows::Win32::Graphics::Dxgi::IDXGIDevice>()?
                .GetAdapter()?
                .GetDesc()?
                .VendorId
                == 0x1002
        };
        if config.codec == Codec::Hevc && amd && selected != "mf" {
            match amf::Encoder::new(config, gpu) {
                Ok(encoder) => return Ok(Self::Amf(Box::new(encoder))),
                Err(e) => tracing::warn!("AMF unavailable; using Media Foundation: {e:#}"),
            }
        }
        Ok(Self::Mf(Box::new(MfEncoder::new(config, gpu)?)))
    }
    fn hardware(&self) -> bool {
        match self {
            Self::Amf(_) => true,
            Self::Mf(e) => e.hardware,
        }
    }
    fn name(&self) -> &str {
        match self {
            Self::Amf(_) => "AMF HEVC (AMD)",
            Self::Mf(e) => &e.name,
        }
    }
    fn bitrate(&mut self, kbps: u32) -> Result<()> {
        match self {
            Self::Amf(e) => e.bitrate(kbps),
            Self::Mf(e) => e.bitrate(kbps),
        }
    }
}

struct MfPipeline {
    capture: Capture,
    converter: Converter,
    encoder: VideoEncoder,
    /// One MF sample per converter slot, made when the slot is first used.
    samples: Vec<Option<IMFSample>>,
    config: VideoConfig,
    pacer: Pacer,
    last_sent: Instant,
    last_capture: Instant,
    /// AMF inputs whose output is still to come, oldest first: when each
    /// was captured, whether it was forced to be a keyframe, its picture
    /// number and the picture a recovery frame recovers from.
    ahead: VecDeque<(Instant, bool, u64, Option<u64>)>,
    /// The display changed while a frame was ahead; reopen before the next.
    moved: bool,
    /// A recovery frame is owed: encode the next picture without waiting
    /// for the screen to change.
    prompt: bool,
}
impl MfPipeline {
    fn info(&self) -> VideoInfo {
        VideoInfo {
            encoder: self.encoder.name().to_string(),
            hardware: self.encoder.hardware(),
            codec: self.config.codec,
            width: self.config.width,
            height: self.config.height,
            display: self.capture.display,
        }
    }
    /// The converted picture as an encoder input.
    fn sample(&mut self, picture: &capture::Converted) -> Result<IMFSample> {
        if self.encoder.hardware() {
            let slot = &mut self.samples[picture.slot];
            if slot.is_none() {
                *slot = Some(MfEncoder::texture_sample(&picture.texture)?);
            }
            return Ok(slot.clone().expect("sample was just made"));
        }
        let (width, height) = (self.config.width as usize, self.config.height as usize);
        // Straight from the mapped surface into the sample's memory.
        self.capture
            .read(&picture.texture, height * 3 / 2, |surface, stride| {
                encode::nv12_sample(surface, stride, width, height)
            })?
    }
    /// A new converter, encoder and samples for the display's new device or size.
    fn reopen(&mut self) -> Result<()> {
        self.converter = Converter::new(
            &self.capture.gpu,
            (self.capture.display.width, self.capture.display.height),
            (self.config.width, self.config.height),
            self.config.fps,
        )?;
        self.samples = vec![None; capture::RING];
        self.ahead.clear();
        self.encoder = VideoEncoder::new(&self.config, &self.capture.gpu)
            .context("reopen encoder after display change")?;
        Ok(())
    }
    fn next(&mut self, keyframe: bool, idle: Duration) -> Result<EncodedVideo> {
        if !self.ahead.is_empty() {
            if !keyframe {
                return self.collect(true);
            }
            // A requested IDR must not sit behind an endlessly replenished
            // queue. Discard the old outputs, then restart the reference chain.
            while !self.ahead.is_empty() {
                self.collect(false)?;
            }
        }
        // Frames go out on the pacer's grid, so the time spent capturing,
        // converting and encoding one comes out of its interval.
        self.pacer.wait();
        let deadline = Instant::now() + Duration::from_secs(5);
        let mut force = keyframe;
        let prompt = std::mem::take(&mut self.prompt);
        if std::mem::take(&mut self.moved) {
            self.reopen()?;
            force = true;
        }
        let mut buffered = false;
        loop {
            let timeout = if self.capture.has_frame && (force || buffered || prompt) {
                Duration::ZERO
            } else if self.capture.has_frame {
                // Returns as soon as the screen changes; the idle repeat is due
                // when this runs out.
                idle.saturating_sub(self.last_sent.elapsed()).min(MAX_WAIT)
            } else {
                MAX_WAIT
            };
            let device = self.capture.gpu.device.as_raw();
            let size = (self.capture.display.width, self.capture.display.height);
            let changed = self.capture.grab(timeout)?;
            if device != self.capture.gpu.device.as_raw()
                || size != (self.capture.display.width, self.capture.display.height)
            {
                self.reopen()?;
                force = true;
            }
            if changed {
                self.last_capture = self.capture.captured;
            }
            if self.capture.has_frame
                && (changed || force || buffered || prompt || self.last_sent.elapsed() >= idle)
            {
                // A repeat is stamped when it is sent again, so the viewer's
                // host delay is about this frame.
                if !changed && !buffered {
                    self.last_capture = Instant::now();
                }
                if !buffered {
                    self.pacer.taken(Instant::now());
                }
                let picture = self.converter.convert(&self.capture.frame)?;
                if let VideoEncoder::Amf(encoder) = &mut self.encoder {
                    let (number, recovery) = encoder.submit(&picture.texture, force)?;
                    self.ahead
                        .push_back((self.last_capture, force, number, recovery));
                    return self.collect(true);
                }
                let sample = self.sample(&picture)?;
                let VideoEncoder::Mf(encoder) = &mut self.encoder else {
                    unreachable!("AMF returned above")
                };
                if let Some((data, idr)) = encoder.encode(&sample, force)? {
                    if force && !idr {
                        bail!("Media Foundation encoder did not honour force-IDR");
                    }
                    self.last_sent = Instant::now();
                    return Ok(EncodedVideo {
                        data,
                        keyframe: idr,
                        captured: self.last_capture,
                        picture: 0,
                        recovery: None,
                    });
                }
                // A synchronous MFT may initially buffer an input despite low latency.
                force = keyframe;
                buffered = true;
                self.pacer.wait();
                self.pacer.taken(Instant::now());
            }
            if Instant::now() >= deadline && (!self.capture.has_frame || buffered) {
                bail!("desktop/encoder produced no frame within five seconds");
            }
            // No sleep here: a grab that finds nothing has waited out its
            // timeout (or its reopen backoff) itself.
        }
    }
}
impl MfPipeline {
    /// The oldest AMF frame's access unit. While the encoder works on it,
    /// the next picture is captured and handed over as soon as the grid
    /// says it is due, so a stream faster than one-at-a-time encoding keeps
    /// the encoder busy and runs at what the GPU can do. A stream the
    /// encoder keeps up with never has a picture due before its output,
    /// so nothing waits behind anything.
    fn collect(&mut self, allow_ahead: bool) -> Result<EncodedVideo> {
        let deadline = Instant::now() + Duration::from_secs(3);
        loop {
            let VideoEncoder::Amf(encoder) = &mut self.encoder else {
                bail!("frames ahead without an AMF encoder");
            };
            // Waits up to the encoder's 1 ms query timeout.
            if let Some((data, idr)) = encoder.receive(Duration::ZERO)? {
                let (captured, forced, picture, recovery) =
                    self.ahead.pop_front().context("AMF output without input")?;
                if forced && !idr {
                    bail!("AMF encoder did not honour force-IDR");
                }
                self.last_sent = Instant::now();
                return Ok(EncodedVideo {
                    data,
                    keyframe: idr,
                    captured,
                    picture,
                    recovery: recovery.filter(|_| !idr),
                });
            }
            if allow_ahead
                && self.ahead.len() < AHEAD
                && !self.moved
                && self.pacer.due().is_some_and(|due| due <= Instant::now())
            {
                let device = self.capture.gpu.device.as_raw();
                let size = (self.capture.display.width, self.capture.display.height);
                // Only a new picture goes ahead; repeats wait their turn.
                if self.capture.grab(Duration::ZERO)? {
                    if device != self.capture.gpu.device.as_raw()
                        || size != (self.capture.display.width, self.capture.display.height)
                    {
                        self.moved = true;
                    } else {
                        self.pacer.taken(Instant::now());
                        self.last_capture = self.capture.captured;
                        let picture = self.converter.convert(&self.capture.frame)?;
                        let (number, recovery) = encoder.submit(&picture.texture, false)?;
                        self.ahead
                            .push_back((self.last_capture, false, number, recovery));
                    }
                }
            }
            if Instant::now() >= deadline {
                bail!("AMF output timed out");
            }
        }
    }
}
/// AMF inputs in flight at most: the encoder's input queue, and fewer than
/// the converter's ring so no slot is rewritten while it is read.
const AHEAD: usize = 2;
const _: () = assert!(AHEAD < capture::RING);
enum Pipeline {
    Mf(Box<MfPipeline>),
    Software(Box<SoftwarePipeline<Capture>>),
    Failed(VideoInfo),
}
impl Pipeline {
    fn new(config: &VideoConfig, mf: bool) -> Result<Self> {
        let capture = Capture::new()?;
        if mf {
            let attempt = (|| {
                let converter = Converter::new(
                    &capture.gpu,
                    (capture.display.width, capture.display.height),
                    (config.width, config.height),
                    config.fps,
                )?;
                let encoder = VideoEncoder::new(config, &capture.gpu)?;
                Ok::<_, anyhow::Error>((converter, encoder))
            })();
            match attempt {
                Ok((converter, encoder)) => {
                    return Ok(Self::Mf(Box::new(MfPipeline {
                        capture,
                        converter,
                        encoder,
                        samples: vec![None; capture::RING],
                        config: config.clone(),
                        pacer: Pacer::new(config.fps),
                        last_sent: Instant::now() - Duration::from_secs(1),
                        last_capture: Instant::now(),
                        ahead: VecDeque::new(),
                        moved: false,
                        prompt: false,
                    })))
                }
                Err(e) if matches!(config.codec, Codec::Hevc | Codec::Av1) => {
                    // Latch's own encoder makes H.264 only, and the viewer
                    // has been told HEVC: fail this session, and offer H.264
                    // to the next one.
                    encode::codec_failed(config.codec, &e);
                    return Err(e.context("open hardware codec"));
                }
                Err(e) => {
                    tracing::warn!("Media Foundation unavailable, using Latch software: {e:#}")
                }
            }
        } else if matches!(config.codec, Codec::Hevc | Codec::Av1) {
            bail!("codec needs Media Foundation");
        }
        Ok(Self::Software(Box::new(SoftwarePipeline::new(
            capture, config,
        )?)))
    }
    fn info(&self) -> VideoInfo {
        match self {
            Self::Mf(p) => p.info(),
            Self::Software(p) => p.info(),
            Self::Failed(info) => info.clone(),
        }
    }
    fn next(&mut self, keyframe: bool, idle: Duration) -> Result<EncodedVideo> {
        match self {
            Self::Software(p) => return p.next(keyframe, idle),
            Self::Failed(_) => bail!("Windows video pipeline failed to reopen"),
            Self::Mf(p) => {
                let result = p.next(keyframe, idle).or_else(|error| {
                    if !matches!(p.encoder, VideoEncoder::Amf(_)) {
                        return Err(error);
                    }
                    tracing::warn!("AMF pipeline failed; reopening Media Foundation: {error:#}");
                    p.ahead.clear();
                    p.encoder =
                        VideoEncoder::Mf(Box::new(MfEncoder::new(&p.config, &p.capture.gpu)?));
                    p.next(true, idle)
                });
                match result {
                    Ok(frame) => return Ok(frame),
                    Err(e) if matches!(p.config.codec, Codec::Hevc | Codec::Av1) => {
                        // Neither Microsoft's encoder nor Latch's software one
                        // makes HEVC, and the viewer decodes nothing else on
                        // this stream: end it, so the next offers H.264.
                        encode::codec_failed(p.config.codec, &e);
                        let info = self.info();
                        *self = Self::Failed(info);
                        return Err(e.context("hardware codec pipeline failed"));
                    }
                    Err(e) => tracing::warn!("Media Foundation pipeline failed: {e:#}"),
                }
                if p.encoder.hardware() {
                    match MfEncoder::software(&p.config) {
                        Ok(encoder) => {
                            p.encoder = VideoEncoder::Mf(Box::new(encoder));
                            match p.next(true, idle) {
                                Ok(frame) => return Ok(frame),
                                Err(e) => tracing::warn!("Microsoft encoder failed: {e:#}"),
                            }
                        }
                        Err(e) => tracing::warn!("Microsoft encoder unavailable: {e:#}"),
                    }
                }
            }
        }
        let info = self.info();
        if let Self::Mf(p) = std::mem::replace(self, Self::Failed(info)) {
            let MfPipeline {
                capture, config, ..
            } = *p;
            let software =
                SoftwarePipeline::new(capture, &config).context("open Latch software fallback")?;
            *self = Self::Software(Box::new(software));
        }
        match self {
            Self::Software(p) => p.next(true, idle),
            _ => bail!("Windows video pipeline has no fallback"),
        }
    }
    fn recovers(&self) -> bool {
        match self {
            Self::Mf(p) => matches!(&p.encoder, VideoEncoder::Amf(e) if e.recovers()),
            Self::Software(p) => p.recovers(),
            Self::Failed(_) => false,
        }
    }
    fn recover_from(&mut self, picture: u64) -> bool {
        match self {
            Self::Mf(p) => {
                let VideoEncoder::Amf(encoder) = &mut p.encoder else {
                    return false;
                };
                let done = encoder.recover_from(picture);
                p.prompt |= done;
                done
            }
            Self::Software(p) => p.recover_from(picture),
            Self::Failed(_) => false,
        }
    }
    fn bitrate(&mut self, kbps: u32) {
        match self {
            Self::Mf(p) => match p.encoder.bitrate(kbps) {
                Ok(()) => p.config.bitrate_kbps = kbps,
                Err(e) => tracing::warn!("encoder bitrate: {e:#}"),
            },
            Self::Software(p) => p.set_bitrate(kbps),
            Self::Failed(_) => {}
        }
    }
}
enum Request {
    Next {
        keyframe: bool,
        idle: Duration,
        reply: mpsc::SyncSender<Result<(EncodedVideo, VideoInfo)>>,
    },
    Bitrate(u32),
    Recover {
        picture: u64,
        reply: mpsc::SyncSender<(bool, bool)>,
    },
    Shutdown,
}
struct VideoProxy {
    requests: mpsc::Sender<Request>,
    info: VideoInfo,
    /// Whether the pipeline makes recovery frames, as of the last answer.
    recovers: bool,
    worker: Option<thread::JoinHandle<()>>,
}
impl Drop for VideoProxy {
    fn drop(&mut self) {
        let _ = self.requests.send(Request::Shutdown);
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}
impl VideoPipeline for VideoProxy {
    fn info(&self) -> VideoInfo {
        self.info.clone()
    }
    fn next(&mut self, keyframe: bool, idle: Duration) -> Result<EncodedVideo> {
        let (reply, receive) = mpsc::sync_channel(1);
        self.requests
            .send(Request::Next {
                keyframe,
                idle,
                reply,
            })
            .context("video worker stopped")?;
        let (frame, info) = receive.recv().context("video worker stopped")??;
        self.info = info;
        Ok(frame)
    }
    fn set_bitrate(&mut self, kbps: u32) {
        let _ = self.requests.send(Request::Bitrate(kbps));
    }
    fn recovers(&self) -> bool {
        self.recovers
    }
    fn recover_from(&mut self, picture: u64) -> bool {
        let (reply, receive) = mpsc::sync_channel(1);
        if self
            .requests
            .send(Request::Recover { picture, reply })
            .is_err()
        {
            return false;
        }
        let Ok((done, recovers)) = receive.recv() else {
            return false;
        };
        self.recovers = recovers;
        done
    }
}
pub fn open_video(config: &VideoConfig) -> Result<Box<dyn VideoPipeline>> {
    if config.width == 0
        || config.height == 0
        || !config.width.is_multiple_of(2)
        || !config.height.is_multiple_of(2)
        || config.width > 16384
        || config.height > 16384
        || config.fps == 0
        || config.bitrate_kbps > u32::MAX / 1000
        || config.bitrate_kbps == 0
    {
        bail!("invalid Windows video size, fps, or bitrate");
    }
    if !matches!(config.codec, Codec::H264Baseline | Codec::H264High)
        && !(matches!(config.codec, Codec::Hevc | Codec::Av1)
            && encode::codec_allowed(config.codec))
    {
        bail!("Windows codec is unavailable");
    }
    let config = config.clone();
    let (requests, receive) = mpsc::channel();
    let (ready, startup) = mpsc::sync_channel(1);
    let worker = thread::Builder::new()
        .name("latch-dxgi-mf".into())
        .spawn(move || {
            crate::priority::media_thread();
            let initialized = (|| {
                let runtime = Runtime::new(false)?;
                let mf = Runtime::new(true)
                    .map_err(|e| tracing::warn!("Media Foundation startup: {e:#}"))
                    .ok();
                let pipeline = Pipeline::new(&config, mf.is_some())?;
                Ok::<_, anyhow::Error>((runtime, mf, pipeline))
            })();
            let (_runtime, _mf, mut pipeline) = match initialized {
                Ok(v) => v,
                Err(e) => {
                    let _ = ready.send(Err(e));
                    return;
                }
            };
            if ready
                .send(Ok((pipeline.info(), pipeline.recovers())))
                .is_err()
            {
                return;
            }
            for request in receive {
                match request {
                    Request::Next {
                        keyframe,
                        idle,
                        reply,
                    } => {
                        let result = pipeline
                            .next(keyframe, idle)
                            .map(|frame| (frame, pipeline.info()));
                        let _ = reply.send(result);
                    }
                    Request::Bitrate(kbps) => pipeline.bitrate(kbps),
                    Request::Recover { picture, reply } => {
                        let done = pipeline.recover_from(picture);
                        let _ = reply.send((done, pipeline.recovers()));
                    }
                    Request::Shutdown => break,
                }
            }
        })
        .context("spawn video worker")?;
    let (info, recovers) = match startup
        .recv()
        .context("video worker stopped during startup")
        .and_then(|v| v)
    {
        Ok(started) => started,
        Err(e) => {
            let _ = worker.join();
            return Err(e);
        }
    };
    Ok(Box::new(VideoProxy {
        requests,
        info,
        recovers,
        worker: Some(worker),
    }))
}
pub fn open_audio() -> Result<Box<dyn AudioCapture>> {
    audio::open()
}
pub fn open_input() -> Result<Box<dyn InputSink>> {
    Ok(Box::new(input::Injector::new()))
}
pub fn open_pads(feedback: crate::gamepad::FeedbackFn) -> Result<crate::gamepad::Opened> {
    Ok(crate::gamepad::Pads::opened(vigem::open()?, feedback))
}
pub use elevated::{
    helper_exe as input_helper_exe, run_helper as run_input_helper, TASK as INPUT_TASK,
};
pub fn encodable() -> Vec<Codec> {
    thread::Builder::new()
        .name("latch-mf-probe".into())
        .spawn(|| {
            let Ok(_runtime) = Runtime::new(true) else {
                return vec![Codec::H264Baseline];
            };
            let mut codecs = Vec::new();
            for codec in [Codec::Av1, Codec::Hevc] {
                if encode::codec_available(codec) {
                    codecs.push(codec);
                }
            }
            if encode::available() {
                codecs.extend([Codec::H264High, Codec::H264Baseline]);
            } else {
                codecs.push(Codec::H264Baseline);
            }
            codecs
        })
        .ok()
        .and_then(|worker| worker.join().ok())
        .unwrap_or_else(|| vec![Codec::H264Baseline])
}
pub fn prepare_display(
    width: u32,
    height: u32,
    fps: u32,
) -> Result<Option<crate::display::Previous>> {
    display::prepare(width, height, fps)
}
pub fn restore_display(previous: &crate::display::Previous) -> Result<()> {
    display::restore(previous)
}

#[cfg(test)]
fn unavailable(error: &anyhow::Error, facility: &str) -> bool {
    use windows::Win32::{
        Foundation::*,
        Graphics::Dxgi::*,
        Media::{Audio::*, MediaFoundation::*},
    };
    let code = error
        .downcast_ref::<windows::core::Error>()
        .map(|e| e.code());
    let missing = code.is_some_and(|code| {
        [
            E_ACCESSDENIED,
            E_NOINTERFACE,
            REGDB_E_CLASSNOTREG,
            CO_E_DLLNOTFOUND,
            windows::core::HRESULT::from_win32(ERROR_MOD_NOT_FOUND.0),
            windows::core::HRESULT::from_win32(ERROR_PROC_NOT_FOUND.0),
            windows::core::HRESULT::from_win32(ERROR_NOT_FOUND.0),
            DXGI_ERROR_NOT_FOUND,
            DXGI_ERROR_UNSUPPORTED,
            DXGI_ERROR_NOT_CURRENTLY_AVAILABLE,
            DXGI_ERROR_SESSION_DISCONNECTED,
            DXGI_ERROR_ACCESS_LOST,
            MF_E_PLATFORM_NOT_INITIALIZED,
            MF_E_DISABLED_IN_SAFEMODE,
            MF_E_TOPO_CODEC_NOT_FOUND,
            AUDCLNT_E_DEVICE_INVALIDATED,
            AUDCLNT_E_SERVICE_NOT_RUNNING,
        ]
        .contains(&code)
    }) || error.to_string() == "no DXGI output attached to the desktop";
    if missing {
        eprintln!("SKIP {facility} unavailable: {error:#}");
    }
    missing
}

#[cfg(test)]
#[path = "../../../stream/src/video/mediafoundation.rs"]
mod test_decoder;
#[cfg(test)]
mod tests {
    /// Run with a changing desktop to exercise the bounded AMF input queue.
    #[test]
    #[ignore]
    fn hardware_pipeline_keyframes() -> anyhow::Result<()> {
        use super::*;
        let _runtime = Runtime::new(true)?;
        let config = VideoConfig {
            width: 1920,
            height: 1080,
            fps: 1000,
            bitrate_kbps: 50_000,
            codec: Codec::Hevc,
        };
        let mut pipeline = Pipeline::new(&config, true)?;
        for i in 0..96 {
            let force = i % 7 == 0;
            let frame = pipeline.next(force, Duration::ZERO)?;
            anyhow::ensure!(!force || frame.keyframe, "IDR request lost at frame {i}");
        }
        Ok(())
    }
    use super::*;
    #[test]
    fn proxy_drop_waits_for_worker_resources() {
        let (requests, receive) = mpsc::channel();
        let released = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let flag = released.clone();
        let worker = thread::spawn(move || {
            assert!(matches!(receive.recv().unwrap(), Request::Shutdown));
            flag.store(true, std::sync::atomic::Ordering::SeqCst);
        });
        drop(VideoProxy {
            requests,
            info: VideoInfo {
                encoder: String::new(),
                hardware: false,
                codec: Codec::H264Baseline,
                width: 2,
                height: 2,
                display: Default::default(),
            },
            recovers: false,
            worker: Some(worker),
        });
        assert!(released.load(std::sync::atomic::Ordering::SeqCst));
    }
    #[test]
    fn software_mf_round_trip() -> Result<()> {
        let _runtime = match Runtime::new(true) {
            Ok(r) => r,
            Err(e) => {
                if unavailable(&e, "Media Foundation") {
                    return Ok(());
                }
                return Err(e);
            }
        };
        let config = VideoConfig {
            width: 160,
            height: 98,
            fps: 30,
            bitrate_kbps: 4000,
            codec: Codec::H264Baseline,
        };
        let mut encoder = match MfEncoder::software(&config) {
            Ok(e) => e,
            Err(e) => {
                if unavailable(&e, "software H.264 encoder") {
                    return Ok(());
                }
                return Err(e);
            }
        };
        let mut decoder = match test_decoder::MfDecoder::new() {
            Ok(d) => d,
            Err(e) => {
                if unavailable(&e, "H.264 decoder") {
                    return Ok(());
                }
                return Err(e);
            }
        };
        eprintln!("decoder: {}", decoder.name());
        // A non-macroblock-aligned height exercises SPS cropping end to end.
        let mut picture = vec![128; 160 * 98 * 3 / 2];
        for y in 0..98 {
            for x in 0..160 {
                picture[y * 160 + x] = (32 + x / 2 + y / 2) as u8;
            }
        }
        let mut pictures = 0;
        for i in 0..16 {
            if let Some((annexb, idr)) =
                encoder.encode(&encode::memory_sample(&picture)?, i == 0 || i == 4)?
            {
                if idr {
                    for kind in [7, 8] {
                        assert!(
                            annexb
                                .windows(4)
                                .any(|v| v[..3] == [0, 0, 1] && v[3] & 31 == kind),
                            "IDR missing parameter set {kind}"
                        );
                    }
                }
                let decoded = decoder.decode(&annexb)?;
                if i == 0 {
                    assert!(
                        decoded.is_some(),
                        "first IDR must decode immediately in low-latency mode"
                    );
                }
                if let Some(decoded) = decoded {
                    assert_eq!((decoded.width, decoded.height), (160, 98));
                    let mut error = 0f64;
                    let mut count = 0;
                    for y in 0..98 {
                        for x in 0..160 {
                            error += (f64::from(decoded.y[y * decoded.y_stride + x])
                                - f64::from(picture[y * 160 + x]))
                            .powi(2);
                            count += 1;
                        }
                    }
                    for y in 0..49 {
                        for x in 0..160 {
                            error +=
                                (f64::from(decoded.uv[y * decoded.uv_stride + x]) - 128.0).powi(2);
                            count += 1;
                        }
                    }
                    let psnr =
                        10.0 * (255.0f64.powi(2) / (error / count as f64).max(1e-12)).log10();
                    assert!(psnr > 35.0, "PSNR {psnr}");
                    pictures += 1;
                    if pictures >= 8 {
                        return Ok(());
                    }
                }
            }
        }
        bail!("MF round trip produced only {pictures} decoded pictures")
    }
}
