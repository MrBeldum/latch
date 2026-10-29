//! Windows capture and media objects live on MTA workers. The session may
//! move its trait objects between threads without moving COM apartments.
mod audio;
mod capture;
mod display;
mod encode;
mod input;
mod mf;
mod runtime;

use crate::{
    audio::AudioCapture,
    input::InputSink,
    software::SoftwarePipeline,
    video::{Codec, EncodedVideo, VideoConfig, VideoInfo, VideoPipeline},
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
use windows::core::Interface;

struct MfPipeline {
    capture: Capture,
    converter: Converter,
    encoder: Encoder,
    config: VideoConfig,
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
    fn next(&mut self, keyframe: bool, idle: Duration) -> Result<EncodedVideo> {
        let interval = Duration::from_secs_f64(1.0 / f64::from(self.config.fps));
        let due = self.last_sent + interval;
        if let Some(wait) = due.checked_duration_since(Instant::now()) {
            thread::sleep(wait);
        }
        let deadline = Instant::now() + Duration::from_secs(5);
        let mut force = keyframe;
        let mut buffered = false;
        loop {
            let timeout = if force && self.capture.has_frame {
                Duration::ZERO
            } else {
                interval.min(idle.saturating_sub(self.last_sent.elapsed()))
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
                let texture = self.converter.convert(&self.capture.frame)?;
                let sample = if self.encoder.hardware {
                    Encoder::texture_sample(&texture)?
                } else {
                    let (bytes, stride) = self
                        .capture
                        .gpu
                        .read(&texture, self.config.height as usize * 3 / 2)?;
                    let mut tight = Vec::with_capacity(
                        self.config.width as usize * self.config.height as usize * 3 / 2,
                    );
                    for row in bytes.chunks(stride) {
                        tight.extend_from_slice(&row[..self.config.width as usize]);
                    }
                    encode::memory_sample(&tight)?
                };
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
            }
            if Instant::now() >= deadline && (!self.capture.has_frame || buffered) {
                bail!("desktop/encoder produced no frame within five seconds");
            }
            if !changed {
                thread::sleep(Duration::from_millis(1));
            }
        }
    }
}
enum Pipeline {
    Mf(Box<MfPipeline>),
    Software(SoftwarePipeline<Capture>),
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
                        config: config.clone(),
                        last_sent: Instant::now() - Duration::from_secs(1),
                        last_capture: Instant::now(),
                    })))
                }
                Err(e) => {
                    tracing::warn!("Media Foundation unavailable, using Latch software: {e:#}")
                }
            }
        }
        Ok(Self::Software(SoftwarePipeline::new(capture, config)?))
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
            *self = Self::Software(software);
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
}
struct VideoProxy {
    requests: mpsc::Sender<Request>,
    info: VideoInfo,
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
    if !matches!(config.codec, Codec::H264Baseline | Codec::H264High) {
        bail!("Windows supports H.264 only");
    }
    let config = config.clone();
    let (requests, receive) = mpsc::channel();
    let (ready, startup) = mpsc::sync_channel(1);
    thread::Builder::new()
        .name("latch-dxgi-mf".into())
        .spawn(move || {
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
                }
            }
        })
        .context("spawn video worker")?;
    let info = startup
        .recv()
        .context("video worker stopped during startup")??;
    Ok(Box::new(VideoProxy { requests, info }))
}
pub fn open_audio() -> Result<Box<dyn AudioCapture>> {
    audio::open()
}
pub fn open_input() -> Result<Box<dyn InputSink>> {
    Ok(Box::<input::Injector>::default())
}
pub fn encodable() -> Vec<Codec> {
    thread::Builder::new()
        .name("latch-mf-probe".into())
        .spawn(|| {
            let Ok(_runtime) = Runtime::new(true) else {
                return vec![Codec::H264Baseline];
            };
            if encode::available() {
                vec![Codec::H264High, Codec::H264Baseline]
            } else {
                vec![Codec::H264Baseline]
            }
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
                if let Some(decoded) = decoder.decode(&annexb)? {
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
