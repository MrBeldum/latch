//! Windows capture and media objects live on MTA workers. The session may
//! move its trait objects between threads without moving COM apartments.
mod audio;
mod capture;
mod display;
mod elevated;
mod encode;
mod input;
mod mf;
mod runtime;

use crate::{
    audio::AudioCapture,
    input::InputSink,
    software::SoftwarePipeline,
    video::{Codec, EncodedVideo, Pacer, VideoConfig, VideoInfo, VideoPipeline},
};
use anyhow::{bail, Context, Result};
use capture::{Capture, Converter};
use encode::Encoder;
use runtime::Runtime;
use std::{
    sync::mpsc,
    thread,
    time::{Duration, Instant},
};
use windows::{core::Interface, Win32::Media::MediaFoundation::IMFSample};

/// How long a capture waits for the screen to change, at most. The wait
/// returns the moment it does; this only bounds how long the loop goes
/// without looking at its deadlines.
const MAX_WAIT: Duration = Duration::from_millis(100);

struct MfPipeline {
    capture: Capture,
    converter: Converter,
    encoder: Encoder,
    /// One MF sample per converter slot, made when the slot is first used.
    samples: Vec<Option<IMFSample>>,
    config: VideoConfig,
    pacer: Pacer,
    last_sent: Instant,
    last_capture: Instant,
}
impl MfPipeline {
    fn info(&self) -> VideoInfo {
        VideoInfo {
            encoder: self.encoder.name.clone(),
            hardware: self.encoder.hardware,
            codec: self.config.codec,
            width: self.config.width,
            height: self.config.height,
            display: self.capture.display,
        }
    }
    /// The converted picture as an encoder input.
    fn sample(&mut self, picture: &capture::Converted) -> Result<IMFSample> {
        if self.encoder.hardware {
            let slot = &mut self.samples[picture.slot];
            if slot.is_none() {
                *slot = Some(Encoder::texture_sample(&picture.texture)?);
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
    fn next(&mut self, keyframe: bool, idle: Duration) -> Result<EncodedVideo> {
        // Frames go out on the pacer's grid, so the time spent capturing,
        // converting and encoding one comes out of its interval.
        self.pacer.wait();
        let deadline = Instant::now() + Duration::from_secs(5);
        let mut force = keyframe;
        let mut buffered = false;
        loop {
            let timeout = if self.capture.has_frame && (force || buffered) {
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
                self.converter = Converter::new(
                    &self.capture.gpu,
                    (self.capture.display.width, self.capture.display.height),
                    (self.config.width, self.config.height),
                    self.config.fps,
                )?;
                self.samples = vec![None; capture::RING];
                self.encoder = Encoder::new(&self.config, &self.capture.gpu)
                    .context("reopen encoder after display change")?;
                force = true;
            }
            if changed {
                self.last_capture = Instant::now();
            }
            if self.capture.has_frame
                && (changed || force || buffered || self.last_sent.elapsed() >= idle)
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
                let sample = self.sample(&picture)?;
                if let Some((data, idr)) = self.encoder.encode(&sample, force)? {
                    if force && !idr {
                        bail!("Media Foundation encoder did not honour force-IDR");
                    }
                    self.last_sent = Instant::now();
                    return Ok(EncodedVideo {
                        data,
                        keyframe: idr,
                        captured: self.last_capture,
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
                let encoder = Encoder::new(config, &capture.gpu)?;
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
                    })))
                }
                Err(e) if config.codec == Codec::Hevc => {
                    // Latch's own encoder makes H.264 only, and the viewer
                    // has been told HEVC: fail this session, and offer H.264
                    // to the next one.
                    encode::hevc_failed(&e);
                    return Err(e.context("open HEVC encoder"));
                }
                Err(e) => {
                    tracing::warn!("Media Foundation unavailable, using Latch software: {e:#}")
                }
            }
        } else if config.codec == Codec::Hevc {
            bail!("HEVC needs Media Foundation");
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
                match p.next(keyframe, idle) {
                    Ok(frame) => return Ok(frame),
                    Err(e) if p.config.codec == Codec::Hevc => {
                        // Neither Microsoft's encoder nor Latch's software one
                        // makes HEVC, and the viewer decodes nothing else on
                        // this stream: end it, so the next offers H.264.
                        encode::hevc_failed(&e);
                        let info = self.info();
                        *self = Self::Failed(info);
                        return Err(e.context("HEVC pipeline failed"));
                    }
                    Err(e) => tracing::warn!("Media Foundation pipeline failed: {e:#}"),
                }
                if p.encoder.hardware {
                    match Encoder::software(&p.config) {
                        Ok(encoder) => {
                            p.encoder = encoder;
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
    Shutdown,
}
struct VideoProxy {
    requests: mpsc::Sender<Request>,
    info: VideoInfo,
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
}
pub fn open_video(config: &VideoConfig) -> Result<Box<dyn VideoPipeline>> {
    if config.width == 0
        || config.height == 0
        || !config.width.is_multiple_of(2)
        || !config.height.is_multiple_of(2)
        || config.width > 16384
        || config.height > 16384
        || config.fps == 0
        || config.fps > 1000
        || config.bitrate_kbps == 0
    {
        bail!("invalid Windows video size, fps, or bitrate");
    }
    if !matches!(config.codec, Codec::H264Baseline | Codec::H264High)
        && !(config.codec == Codec::Hevc && encode::hevc_allowed())
    {
        bail!("Windows supports H.264 only");
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
            if ready.send(Ok(pipeline.info())).is_err() {
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
                    Request::Shutdown => break,
                }
            }
        })
        .context("spawn video worker")?;
    let info = match startup
        .recv()
        .context("video worker stopped during startup")
        .and_then(|v| v)
    {
        Ok(info) => info,
        Err(e) => {
            let _ = worker.join();
            return Err(e);
        }
    };
    Ok(Box::new(VideoProxy {
        requests,
        info,
        worker: Some(worker),
    }))
}
pub fn open_audio() -> Result<Box<dyn AudioCapture>> {
    audio::open()
}
pub fn open_input() -> Result<Box<dyn InputSink>> {
    Ok(Box::new(input::Injector::new()))
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
            // Off unless LATCH_WINDOWS_HEVC=1, and only once a GPU encoder has
            // made a keyframe here.
            if encode::hevc_available() {
                codecs.push(Codec::Hevc);
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
        let mut encoder = match Encoder::software(&config) {
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
