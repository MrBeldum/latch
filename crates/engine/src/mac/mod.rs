//! ScreenCaptureKit retains IOSurfaces through VideoToolbox encoding. Sound has its own stream.

mod capture;
mod encoder;
mod ffi;
mod input;

use crate::audio::{AudioCapture, CHANNELS, FRAME};
use crate::input::InputSink;
use crate::video::{Codec, EncodedVideo, Limits, Pacer, VideoConfig, VideoInfo, VideoPipeline};
use anyhow::{bail, Result};
use capture::Capture;
use encoder::{Depth, Encoder};
use std::time::{Duration, Instant};

/// How often a running capture asks whether its display changed.
const GEOMETRY_CHECK: Duration = Duration::from_millis(250);

/// How many frames may be in VideoToolbox at once. Two let the media engine
/// overlap a frame with the next, so a stream whose interval is shorter
/// than one frame's encode time keeps its rate; more would only queue.
const IN_FLIGHT: usize = 2;
/// `LATCH_MAC_DISPLAY=<CGDirectDisplayID>` shares that display instead of
/// the main one; the stream request does not name a display yet.
fn wanted_display() -> Option<u32> {
    std::env::var("LATCH_MAC_DISPLAY").ok()?.trim().parse().ok()
}

pub fn open_video(config: &VideoConfig) -> Result<Box<dyn VideoPipeline>> {
    capture::permission()?;
    let depth = if config.color.bit_depth == 10 {
        Depth::Ten
    } else {
        Depth::Eight
    };
    let encoder = Encoder::new(config, depth)?;
    let request = capture::Request {
        width: config.width,
        height: config.height,
        fps: config.fps,
        display: wanted_display(),
        pixel_format: depth.pixel_format(),
        hdr: config.color.is_hdr(),
    };
    let capture = Capture::video(request)?;
    let mut video = Video {
        capture,
        encoder,
        request,
        config: config.clone(),
        generation: 0,
        sent: None,
        pacer: Pacer::new(config.fps),
        pending: None,
        checked: Instant::now(),
    };
    video.wake();
    // Some virtual Macs advertise hardware that fails only on the first encode.
    // Resolve that fallback before the server snapshots VideoInfo for status.
    video.pending = Some(video.next(true, Duration::ZERO)?);
    Ok(Box::new(video))
}
pub fn open_audio() -> Result<Box<dyn AudioCapture>> {
    Ok(Box::new(Audio {
        capture: Capture::audio()?,
    }))
}
pub fn open_input() -> Result<Box<dyn InputSink>> {
    input::open()
}
/// What VideoToolbox's hardware takes for `codec` on this Mac: the
/// largest of a ladder of sizes that encodes without falling back to
/// software, tried once per codec. Media engines differ (an M3 Pro takes
/// H.264 up to 4096 × 2160 and HEVC up to 8192 × 4320) and VideoToolbox
/// has no call that says.
pub fn limits(codec: Codec) -> Limits {
    static PROBED: parking_lot::Mutex<Vec<(Codec, Limits)>> = parking_lot::Mutex::new(Vec::new());
    let mut probed = PROBED.lock();
    if let Some((_, limits)) = probed.iter().find(|(c, _)| *c == codec) {
        return *limits;
    }
    const LADDER: [(u32, u32); 7] = [
        (8192, 4320),
        (7680, 4320),
        (6016, 3384),
        (5120, 2880),
        (4096, 2160),
        (3840, 2160),
        (2560, 1440),
    ];
    let level = Limits::level(codec);
    let limits = LADDER
        .iter()
        .find(|&&(w, h)| Encoder::hardware_encodes(codec, w, h))
        .map_or(level, |&(w, h)| Limits {
            max_side: w.max(h),
            max_pixels: u64::from(w) * u64::from(h),
            ..level
        });
    tracing::info!(
        "VideoToolbox {} takes up to {} pixels a side, {} a picture",
        codec.name(),
        limits.max_side,
        limits.max_pixels
    );
    probed.push((codec, limits));
    limits
}

pub fn encodable() -> Vec<Codec> {
    [Codec::Hevc, Codec::H264High, Codec::H264Baseline]
        .into_iter()
        .filter(|codec| {
            Encoder::new(
                &VideoConfig {
                    color: Default::default(),
                    width: 640,
                    height: 480,
                    fps: 60,
                    bitrate_kbps: 10_000,
                    codec: *codec,
                },
                Depth::Eight,
            )
            .is_ok()
        })
        .collect()
}
struct Video {
    capture: Capture,
    encoder: Encoder,
    request: capture::Request,
    config: VideoConfig,
    generation: u64,
    sent: Option<Instant>,
    pacer: Pacer,
    pending: Option<EncodedVideo>,
    checked: Instant,
}
impl Video {
    /// A frame coming out of the encoder wakes `next` as a new picture does.
    fn wake(&self) {
        let shared = self.capture.shared.clone();
        self.encoder.on_output(move || {
            // Taking the lock orders this after next()'s check for output.
            let _state = shared.state.lock();
            shared.ready.notify_all();
        });
    }
    fn restart_capture(&mut self) -> Result<()> {
        self.capture = Capture::video(self.request)?;
        self.wake();
        self.generation = 0;
        self.encoder.request_keyframe();
        Ok(())
    }
    /// A hardware session that failed is replaced in software; a software
    /// failure ends the stream.
    fn recover(&mut self, error: anyhow::Error) -> Result<()> {
        if !self.encoder.hardware {
            return Err(error);
        }
        tracing::warn!(%error, "VideoToolbox hardware encode failed; continuing in software");
        self.encoder.software()?;
        self.wake();
        self.encoder.request_keyframe();
        Ok(())
    }
}
impl VideoPipeline for Video {
    fn info(&self) -> VideoInfo {
        VideoInfo {
            color: Default::default(),
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
    fn next(&mut self, keyframe: bool, idle: Duration) -> Result<EncodedVideo> {
        if let Some(frame) = self.pending.take() {
            return Ok(frame);
        }
        if keyframe {
            self.encoder.request_keyframe();
        }
        let interval = self.pacer.interval();
        if self.checked.elapsed() >= GEOMETRY_CHECK {
            self.checked = Instant::now();
            // A new capture refits the picture, and info() hands the server
            // the display's new place for input.
            if self.capture.moved() {
                tracing::info!("display changed size or place; restarting screen capture");
                self.restart_capture()?;
            }
        }
        let start = Instant::now();
        loop {
            let mut state = self.capture.shared.state.lock();
            // Finished frames leave first, oldest first, as soon as they are out.
            if let Some((.., frame)) = self.encoder.collect(Duration::ZERO) {
                drop(state);
                match frame {
                    Some(Ok(frame)) => return Ok(frame),
                    Some(Err(error)) => self.recover(error)?,
                    // A real-time encoder may drop under load; the next
                    // picture carries any keyframe that one was.
                    None => self.encoder.request_keyframe(),
                }
                continue;
            }
            if let Some(error) = state.stopped.take() {
                drop(state);
                tracing::warn!(%error, "reopening stopped screen capture");
                self.restart_capture()?;
                continue;
            }
            let now = Instant::now();
            let changed = state.generation != self.generation;
            // A new picture or a keyframe goes at the pacer's next slot; an
            // idle repeat once `idle` has passed since the last frame.
            let ready_at = match self.sent {
                Some(sent) if !self.encoder.needs_keyframe() && !changed => {
                    sent + idle.max(interval)
                }
                _ => self.pacer.due().unwrap_or(now),
            };
            let room = self.encoder.in_flight() < IN_FLIGHT;
            if now >= ready_at && room {
                if let Some(picture) = &state.picture {
                    let image = ffi::Owned::retain(picture.image.0);
                    // A repeat is stamped when it is sent again, so the
                    // viewer's host delay is about this frame.
                    let captured = if changed { picture.captured } else { now };
                    self.generation = state.generation;
                    drop(state);
                    self.pacer.taken(now);
                    let keyframe = self.sent.is_none();
                    self.sent = Some(now);
                    if let Err(error) = self.encoder.submit(image.0, captured, keyframe) {
                        self.recover(error)?;
                        self.encoder.request_keyframe();
                    }
                    continue;
                }
            }
            if state.picture.is_none() && start.elapsed() >= Duration::from_secs(10) {
                bail!("ScreenCaptureKit produced no complete picture in 10 seconds");
            }
            // A frame coming out, a new picture or the next slot wakes this.
            let wait = if state.picture.is_none() {
                Duration::from_millis(100)
            } else if now >= ready_at {
                Duration::from_secs(1)
            } else {
                ready_at - now
            };
            self.capture.shared.ready.wait_for(&mut state, wait);
        }
    }
    fn set_bitrate(&mut self, kbps: u32) {
        if kbps > 0 && kbps != self.config.bitrate_kbps && self.pending.take().is_some() {
            self.encoder.request_keyframe();
        }
        match self.encoder.bitrate(kbps) {
            Ok(()) => {
                self.config.bitrate_kbps = kbps;
            }
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
