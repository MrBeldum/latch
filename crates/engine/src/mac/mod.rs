//! ScreenCaptureKit retains IOSurfaces through VideoToolbox encoding. Sound has its own stream.

mod capture;
mod encoder;
mod ffi;
mod input;

use crate::audio::{AudioCapture, CHANNELS, FRAME};
use crate::input::InputSink;
use crate::video::{Codec, EncodedVideo, VideoConfig, VideoInfo, VideoPipeline};
use anyhow::{bail, Result};
use capture::Capture;
use encoder::Encoder;
use std::time::{Duration, Instant};

pub fn open_video(config: &VideoConfig) -> Result<Box<dyn VideoPipeline>> {
    capture::permission()?;
    let encoder = Encoder::new(config)?;
    let capture = Capture::new(config.width, config.height, config.fps, false)?;
    let mut video = Video {
        capture,
        encoder,
        config: config.clone(),
        generation: 0,
        cadence: crate::cadence::Cadence::new(config.fps),
        sent: None,
        pending: None,
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
    cadence: crate::cadence::Cadence,
    sent: Option<Instant>,
    pending: Option<EncodedVideo>,
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
        let interval = Duration::from_secs_f64(1.0 / f64::from(self.config.fps));
        self.cadence.wait();
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
            let elapsed = self
                .sent
                .map_or(Duration::MAX, |sent| now.saturating_duration_since(sent));
            let due = if keyframe || changed {
                Duration::ZERO
            } else {
                idle.max(interval)
            };
            if elapsed >= due {
                if let Some(picture) = &state.picture {
                    let image = ffi::Owned::retain(picture.image.0);
                    // A repeat is stamped when it is sent again, so the
                    // viewer's host delay is about this frame.
                    let captured = if changed { picture.captured } else { now };
                    self.generation = state.generation;
                    drop(state);
                    let first = self.sent.is_none();
                    self.sent = Some(now);
                    let frame =
                        self.encoder
                            .encode(image.0, captured, keyframe || first)?;
                    // A real-time encoder may drop under load. Pace the retry and
                    // preserve a pending IDR request until a picture is emitted.
                    if let Some(frame) = frame {
                        return Ok(frame);
                    }
                    self.cadence.wait();
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
                due.saturating_sub(elapsed)
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
