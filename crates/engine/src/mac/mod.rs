//! ScreenCaptureKit retains IOSurfaces through VideoToolbox encoding. Sound has its own stream.

mod capture;
mod encoder;
mod ffi;
mod input;

use crate::audio::{AudioCapture, CHANNELS, FRAME};
use crate::input::InputSink;
use crate::video::{Codec, EncodedVideo, Pacer, VideoConfig, VideoInfo, VideoPipeline};
use anyhow::{bail, Result};
use capture::Capture;
use encoder::Encoder;
use std::time::{Duration, Instant};

/// How often a running capture asks whether its display changed.
const GEOMETRY_CHECK: Duration = Duration::from_millis(250);

pub fn open_video(config: &VideoConfig) -> Result<Box<dyn VideoPipeline>> {
    capture::permission()?;
    let encoder = Encoder::new(config)?;
    let capture = Capture::new(config.width, config.height, config.fps, false)?;
    let mut video = Video {
        capture,
        encoder,
        config: config.clone(),
        generation: 0,
        sent: None,
        pacer: Pacer::new(config.fps),
        pending: None,
        checked: Instant::now(),
    };
    // Some virtual Macs advertise hardware that fails only on the first encode.
    // Resolve that fallback before the server snapshots VideoInfo for status.
    video.pending = Some(video.next(true, Duration::ZERO)?);
    Ok(Box::new(video))
}
pub fn open_audio() -> Result<Box<dyn AudioCapture>> {
    Ok(Box::new(Audio {
        capture: Capture::new(2, 2, 1, true)?,
    }))
}
pub fn open_input() -> Result<Box<dyn InputSink>> {
    input::open()
}
pub fn encodable() -> Vec<Codec> {
    [Codec::Hevc, Codec::H264High, Codec::H264Baseline]
        .into_iter()
        .filter(|codec| {
            Encoder::new(&VideoConfig {
                width: 640,
                height: 480,
                fps: 60,
                bitrate_kbps: 10_000,
                codec: *codec,
            })
            .is_ok()
        })
        .collect()
}
struct Video {
    capture: Capture,
    encoder: Encoder,
    config: VideoConfig,
    generation: u64,
    sent: Option<Instant>,
    pacer: Pacer,
    pending: Option<EncodedVideo>,
    checked: Instant,
}
impl VideoPipeline for Video {
    fn info(&self) -> VideoInfo {
        VideoInfo {
            encoder: format!(
                "VideoToolbox {}",
                if self.config.codec == Codec::Hevc {
                    "HEVC"
                } else {
                    "H.264"
                }
            ),
            hardware: self.encoder.hardware,
            codec: self.config.codec,
            width: self.config.width,
            height: self.config.height,
            display: self.capture.display,
        }
    }
    fn next(&mut self, mut keyframe: bool, idle: Duration) -> Result<EncodedVideo> {
        if let Some(frame) = self.pending.take() {
            return Ok(frame);
        }
        let interval = self.pacer.interval();
        if self.checked.elapsed() >= GEOMETRY_CHECK {
            self.checked = Instant::now();
            // A new capture refits the picture, and info() hands the server
            // the display's new place for input.
            if self.capture.moved() {
                tracing::info!("display changed size or place; restarting screen capture");
                self.capture = Capture::new(
                    self.config.width,
                    self.config.height,
                    self.config.fps,
                    false,
                )?;
                self.generation = 0;
                keyframe = true;
            }
        }
        let start = Instant::now();
        loop {
            let mut state = self.capture.shared.state.lock();
            if let Some(error) = state.stopped.take() {
                drop(state);
                tracing::warn!(%error, "reopening stopped screen capture");
                self.capture = Capture::new(
                    self.config.width,
                    self.config.height,
                    self.config.fps,
                    false,
                )?;
                self.generation = 0;
                keyframe = true;
                continue;
            }
            let now = Instant::now();
            let changed = state.generation != self.generation;
            // A new picture or a keyframe goes at the pacer's next slot; an
            // idle repeat once `idle` has passed since the last frame.
            let ready_at = match self.sent {
                Some(sent) if !keyframe && !changed => sent + idle.max(interval),
                _ => self.pacer.due().unwrap_or(now),
            };
            if now >= ready_at {
                if let Some(picture) = &state.picture {
                    let image = ffi::Owned::retain(picture.image.0);
                    // A repeat is stamped when it is sent again, so the
                    // viewer's host delay is about this frame.
                    let captured = if changed { picture.captured } else { now };
                    self.generation = state.generation;
                    drop(state);
                    self.pacer.taken(now);
                    let first = self.sent.is_none();
                    self.sent = Some(now);
                    let frame = self.encoder.encode(image.0, captured, keyframe || first)?;
                    // A real-time encoder may drop under load. Pace the retry and
                    // preserve a pending IDR request until a picture is emitted.
                    if let Some(frame) = frame {
                        return Ok(frame);
                    }
                    self.pacer.wait();
                    keyframe = true;
                    continue;
                }
            }
            if state.picture.is_none() && start.elapsed() >= Duration::from_secs(10) {
                bail!("ScreenCaptureKit produced no complete picture in 10 seconds");
            }
            let wait = if state.picture.is_none() {
                Duration::from_millis(100)
            } else {
                ready_at.saturating_duration_since(now)
            };
            self.capture.shared.ready.wait_for(&mut state, wait);
        }
    }
    fn set_bitrate(&mut self, kbps: u32) {
        match self.encoder.bitrate(kbps) {
            Ok(()) => self.config.bitrate_kbps = kbps,
            Err(error) => tracing::warn!(%error, "update VideoToolbox bitrate"),
        }
    }
}
struct Audio {
    capture: Capture,
}
impl AudioCapture for Audio {
    fn read(&mut self, out: &mut [i16; FRAME * CHANNELS], timeout: Duration) -> Result<bool> {
        let start = Instant::now();
        loop {
            let mut state = self.capture.shared.state.lock();
            if let Some(frame) = state.frames.pop_front() {
                *out = frame;
                return Ok(true);
            }
            if let Some(error) = &state.stopped {
                bail!("ScreenCaptureKit audio stopped: {error}");
            }
            let wait = timeout.saturating_sub(start.elapsed());
            if wait.is_zero() {
                return Ok(false);
            }
            self.capture.shared.ready.wait_for(&mut state, wait);
        }
    }
    fn describe(&self) -> String {
        "ScreenCaptureKit system audio · 48 kHz stereo".to_string()
    }
}
