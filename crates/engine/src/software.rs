//! The CPU path: any source of BGRA pictures, converted and encoded by
//! `latch-codec`. Linux always takes it; Windows falls back to it.

use crate::input::DisplayRect;
use crate::video::{Codec, EncodedVideo, Pacer, VideoConfig, VideoInfo, VideoPipeline};
use anyhow::ensure;
use latch_codec::h264::{Encoder, EncoderConfig};
use latch_codec::Yuv420;
use std::time::{Duration, Instant};

/// A BGRA picture: `stride` bytes per row.
#[derive(Debug, Clone, Default)]
pub struct Bgra {
    pub width: u32,
    pub height: u32,
    pub stride: usize,
    pub data: Vec<u8>,
}

/// A screen the CPU can read.
pub trait CpuCapture: Send {
    /// The screen's size now.
    fn size(&self) -> (u32, u32);

    /// Waits up to `timeout` for the screen to change (a source that cannot
    /// tell waits one frame interval), then copies it into `frame` with the
    /// cursor drawn in. Returns false when it did not change.
    fn grab(&mut self, frame: &mut Bgra, timeout: Duration) -> anyhow::Result<bool>;

    fn display(&self) -> DisplayRect;

    /// For [`VideoInfo::encoder`]'s sake: "X11", "test pattern".
    fn name(&self) -> String;

    fn set_cursor_drawn(&mut self, _drawn: bool) -> bool {
        false
    }

    fn take_cursor(&mut self) -> Option<crate::video::CursorUpdate> {
        None
    }
}

/// Time spent in each stage of the last frame. Capture includes waiting
/// for damage; frame pacing happens outside these measurements.
#[derive(Debug, Clone, Copy, Default)]
pub struct SoftwareTimings {
    pub capture: Duration,
    pub convert: Duration,
    pub encode: Duration,
}

/// [`CpuCapture`] → [`latch_codec::color`] → [`latch_codec::h264::Encoder`].
/// Sends the source's own size (no scaling); repeats the last picture as
/// an all-skip frame when idle; re-creates the encoder if the source's size
/// changes.
pub struct SoftwarePipeline<C: CpuCapture> {
    capture: C,
    config: VideoConfig,
    frame: Bgra,
    picture: Yuv420,
    encoder: Option<Encoder>,
    pacer: Pacer,
    last_sent: Option<Instant>,
    captured: Instant,
    timings: SoftwareTimings,
}

impl<C: CpuCapture> SoftwarePipeline<C> {
    pub fn new(capture: C, config: &VideoConfig) -> anyhow::Result<Self> {
        ensure!(
            config.fps > 0 && config.fps <= 1000,
            "fps must be in 1..=1000"
        );
        ensure!(config.bitrate_kbps > 0, "bitrate must be positive");
        let (width, height) = capture.size();
        validate_size(width, height)?;
        Ok(Self {
            capture,
            config: config.clone(),
            frame: Bgra::default(),
            picture: Yuv420::default(),
            encoder: None,
            pacer: Pacer::new(config.fps),
            last_sent: None,
            captured: Instant::now(),
            timings: SoftwareTimings::default(),
        })
    }

    pub fn timings(&self) -> SoftwareTimings {
        self.timings
    }

    fn acquire(&mut self, keyframe: bool, idle: Duration) -> anyhow::Result<bool> {
        let interval = self.pacer.interval();
        self.pacer.wait();
        let deadline = self.last_sent.unwrap_or_else(Instant::now) + idle.max(interval);
        loop {
            let started = Instant::now();
            let changed = self.capture.grab(&mut self.frame, Duration::ZERO)?;
            self.timings.capture = started.elapsed();
            if changed {
                validate_frame(&self.frame)?;
                self.captured = Instant::now();
                self.pacer.taken(started);
                return Ok(true);
            }
            let now = Instant::now();
            if keyframe || now >= deadline {
                ensure!(
                    !self.frame.data.is_empty(),
                    "capture supplied no initial picture"
                );
                self.pacer.taken(now);
                return Ok(false);
            }
            // Polling also lets a source without damage notifications honour the deadline.
            let wait = interval.min(deadline.saturating_duration_since(now));
            let started = Instant::now();
            if self.capture.grab(&mut self.frame, wait)? {
                self.timings.capture = started.elapsed();
                validate_frame(&self.frame)?;
                self.captured = Instant::now();
                self.pacer.taken(self.captured);
                return Ok(true);
            }
            std::thread::sleep(wait.saturating_sub(started.elapsed()));
        }
    }
}

fn validate_size(width: u32, height: u32) -> anyhow::Result<()> {
    ensure!(
        width > 0 && height > 0 && width <= 16384 && height <= 16384,
        "capture dimensions must be in 1..=16384"
    );
    ensure!(
        width.is_multiple_of(2) && height.is_multiple_of(2),
        "software H.264 requires even dimensions"
    );
    Ok(())
}

fn validate_frame(frame: &Bgra) -> anyhow::Result<()> {
    validate_size(frame.width, frame.height)?;
    ensure!(
        frame.stride >= frame.width as usize * 4,
        "short BGRA stride"
    );
    let needed = frame
        .stride
        .checked_mul(frame.height as usize)
        .ok_or_else(|| anyhow::anyhow!("BGRA size overflow"))?;
    ensure!(frame.data.len() >= needed, "short BGRA picture");
    Ok(())
}

impl<C: CpuCapture> VideoPipeline for SoftwarePipeline<C> {
    fn info(&self) -> VideoInfo {
        let (width, height) = if self.frame.data.is_empty() {
            self.capture.size()
        } else {
            (self.frame.width, self.frame.height)
        };
        VideoInfo {
            encoder: format!("Latch software H.264 ({})", self.capture.name()),
            hardware: false,
            codec: Codec::H264Baseline,
            width,
            height,
            display: self.capture.display(),
        }
    }

    fn next(&mut self, keyframe: bool, idle: Duration) -> anyhow::Result<EncodedVideo> {
        let changed = self.acquire(keyframe, idle)?;
        let resized =
            (self.picture.width, self.picture.height) != (self.frame.width, self.frame.height);
        if resized || self.encoder.is_none() {
            self.encoder = Some(Encoder::new(EncoderConfig {
                width: self.frame.width,
                height: self.frame.height,
                fps: self.config.fps,
                bitrate_kbps: self.config.bitrate_kbps,
                threads: std::thread::available_parallelism().map_or(1, |n| n.get().min(4)),
            })?);
            self.picture = Yuv420::new(self.frame.width, self.frame.height);
        }
        let started = Instant::now();
        if changed || resized {
            latch_codec::color::bgra_to_yuv420(
                &self.frame.data,
                self.frame.width,
                self.frame.height,
                self.frame.stride,
                &mut self.picture,
            );
        }
        self.timings.convert = started.elapsed();
        // A repeat shows the old picture, but it was taken up now: what the
        // viewer learns about the host's delay is about this frame.
        if !changed && !resized {
            self.captured = started;
        }
        let mut data = Vec::new();
        let started = Instant::now();
        let info = self
            .encoder
            .as_mut()
            .ok_or_else(|| anyhow::anyhow!("encoder unavailable"))?
            .encode(&self.picture, keyframe || resized, &mut data)?;
        self.timings.encode = started.elapsed();
        self.last_sent = Some(Instant::now());
        Ok(EncodedVideo {
            data,
            keyframe: info.keyframe,
            captured: self.captured,
        })
    }

    fn set_bitrate(&mut self, kbps: u32) {
        self.config.bitrate_kbps = kbps.max(1);
        if let Some(encoder) = &mut self.encoder {
            encoder.set_bitrate(self.config.bitrate_kbps);
        }
    }

    fn set_cursor_drawn(&mut self, drawn: bool) -> bool {
        self.capture.set_cursor_drawn(drawn)
    }

    fn take_cursor(&mut self) -> Option<crate::video::CursorUpdate> {
        self.capture.take_cursor()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Fake {
        changed: bool,
        size: (u32, u32),
    }
    impl CpuCapture for Fake {
        fn size(&self) -> (u32, u32) {
            self.size
        }
        fn grab(&mut self, frame: &mut Bgra, _: Duration) -> anyhow::Result<bool> {
            if !std::mem::take(&mut self.changed) {
                return Ok(false);
            }
            *frame = Bgra {
                width: self.size.0,
                height: self.size.1,
                stride: self.size.0 as usize * 4,
                data: vec![128; (self.size.0 * self.size.1 * 4) as usize],
            };
            Ok(true)
        }
        fn display(&self) -> DisplayRect {
            DisplayRect {
                width: self.size.0,
                height: self.size.1,
                ..Default::default()
            }
        }
        fn name(&self) -> String {
            "fake".into()
        }
    }
    fn pipeline() -> SoftwarePipeline<Fake> {
        SoftwarePipeline::new(
            Fake {
                changed: true,
                size: (16, 16),
            },
            &VideoConfig {
                width: 1920,
                height: 1080,
                fps: 100,
                bitrate_kbps: 1000,
                codec: Codec::H264Baseline,
            },
        )
        .unwrap()
    }
    #[test]
    fn capture_idle_keyframe_pacing_and_info() {
        let mut p = pipeline();
        assert_eq!(p.info().encoder, "Latch software H.264 (fake)");
        assert_eq!(p.info().width, 16);
        assert!(!p.info().hardware);
        assert!(p.acquire(false, Duration::ZERO).unwrap());
        p.last_sent = Some(Instant::now());
        let start = Instant::now();
        assert!(!p.acquire(true, Duration::from_secs(10)).unwrap());
        // The pacer's grid runs 1% fast: 9.9 ms at 100 fps.
        assert!(start.elapsed() >= Duration::from_micros(9_500));
        assert!(start.elapsed() < Duration::from_millis(100));
        p.last_sent = Some(Instant::now());
        let start = Instant::now();
        assert!(!p.acquire(false, Duration::from_millis(25)).unwrap());
        assert!(start.elapsed() >= Duration::from_millis(25));
        p.capture.size = (32, 16);
        p.capture.changed = true;
        assert!(p.acquire(false, Duration::ZERO).unwrap());
        assert_eq!(p.info().width, 32);
        p.set_bitrate(2000);
        assert_eq!(p.config.bitrate_kbps, 2000);
    }
    #[test]
    fn rejects_bad_frames_and_config() {
        let mut p = pipeline();
        p.config.fps = 0;
        assert!(SoftwarePipeline::new(p.capture, &p.config).is_err());
        assert!(validate_frame(&Bgra {
            width: 16,
            height: 16,
            stride: 64,
            data: vec![0; 3]
        })
        .is_err());
        assert!(validate_size(15, 16).is_err());
    }
    #[test]
    #[ignore = "requires the parallel latch-codec encoder and color implementation"]
    fn encodes_repeats_keyframes_and_resizes() {
        let mut p = pipeline();
        assert!(p.next(false, Duration::ZERO).unwrap().keyframe);
        assert!(!p.next(false, Duration::ZERO).unwrap().keyframe);
        assert!(p.next(true, Duration::from_secs(10)).unwrap().keyframe);
        p.set_bitrate(2000);
        p.capture.size = (32, 16);
        p.capture.changed = true;
        assert!(p.next(false, Duration::ZERO).unwrap().keyframe);
        assert_eq!(p.info().width, 32);
    }
}
