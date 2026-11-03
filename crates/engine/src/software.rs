//! The CPU path: any source of BGRA pictures, converted on the CPU and
//! encoded by `latch-codec` or by a hardware encoder that takes pictures
//! from memory (VA-API, NVENC on Linux). Linux always takes it; Windows
//! falls back to it.

use crate::input::DisplayRect;
use crate::video::{Codec, EncodedVideo, Pacer, VideoConfig, VideoInfo, VideoPipeline};
use anyhow::ensure;
pub use latch_codec::color::Rect;
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

    /// Where the picture the last `grab` that returned true differs from
    /// the one the source returned before it; `None` when it cannot tell.
    /// Lets the pipeline convert only what changed.
    fn damage(&self) -> Option<Vec<Rect>> {
        None
    }

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

/// Turns converted pictures into access units.
pub trait PictureEncoder: Send {
    /// For people: "Latch software H.264", "NVENC HEVC (NVIDIA GeForce …)".
    fn name(&self) -> String;
    fn hardware(&self) -> bool;
    fn codec(&self) -> Codec;
    /// Appends one Annex B access unit for `picture` (parameter sets in
    /// band before every keyframe) to `out`. Returns whether it is a
    /// keyframe, which it is when asked.
    fn encode(
        &mut self,
        picture: &Yuv420,
        keyframe: bool,
        out: &mut Vec<u8>,
    ) -> anyhow::Result<bool>;
    /// [`PictureEncoder::encode`], told where `picture` may differ from
    /// the last picture given (pixel rectangles; `None`: anywhere).
    fn encode_changed(
        &mut self,
        picture: &Yuv420,
        changed: Option<&[Rect]>,
        keyframe: bool,
        out: &mut Vec<u8>,
    ) -> anyhow::Result<bool> {
        let _ = changed;
        self.encode(picture, keyframe, out)
    }
    /// From the next frame.
    fn set_bitrate(&mut self, kbps: u32);
}

/// Opens an encoder for pictures of `width × height` and `config`'s rate.
pub type OpenEncoder =
    Box<dyn FnMut(u32, u32, &VideoConfig) -> anyhow::Result<Box<dyn PictureEncoder>> + Send>;

/// `latch-codec`'s H.264 Constrained Baseline encoder.
pub struct LatchEncoder(Encoder);

impl LatchEncoder {
    pub fn open(width: u32, height: u32, config: &VideoConfig) -> anyhow::Result<Self> {
        Ok(Self(Encoder::new(EncoderConfig {
            width,
            height,
            fps: config.fps,
            bitrate_kbps: config.bitrate_kbps,
            threads: std::thread::available_parallelism().map_or(1, |n| n.get().min(4)),
        })?))
    }
}

impl PictureEncoder for LatchEncoder {
    fn name(&self) -> String {
        "Latch software H.264".into()
    }
    fn hardware(&self) -> bool {
        false
    }
    fn codec(&self) -> Codec {
        Codec::H264Baseline
    }
    fn encode(
        &mut self,
        picture: &Yuv420,
        keyframe: bool,
        out: &mut Vec<u8>,
    ) -> anyhow::Result<bool> {
        Ok(self.0.encode(picture, keyframe, out)?.keyframe)
    }
    fn encode_changed(
        &mut self,
        picture: &Yuv420,
        changed: Option<&[Rect]>,
        keyframe: bool,
        out: &mut Vec<u8>,
    ) -> anyhow::Result<bool> {
        Ok(self
            .0
            .encode_changed(picture, changed, keyframe, out)?
            .keyframe)
    }
    fn set_bitrate(&mut self, kbps: u32) {
        self.0.set_bitrate(kbps);
    }
}

/// [`CpuCapture`] → [`latch_codec::color`] → a [`PictureEncoder`]
/// (`latch-codec` unless [`SoftwarePipeline::with_encoder`] says otherwise).
/// Sends the source's own size (no scaling); repeats the last picture as
/// an all-skip frame when idle; re-creates the encoder if the source's size
/// changes.
pub struct SoftwarePipeline<C: CpuCapture> {
    capture: C,
    config: VideoConfig,
    frame: Bgra,
    picture: Yuv420,
    encoder: Option<Box<dyn PictureEncoder>>,
    /// The size `encoder` was opened for.
    encoder_size: (u32, u32),
    open: OpenEncoder,
    /// The encoder's name, whether it is hardware, and its codec.
    describe: (String, bool, Codec),
    pacer: Pacer,
    last_sent: Option<Instant>,
    captured: Instant,
    timings: SoftwareTimings,
    threads: usize,
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
            encoder_size: (0, 0),
            open: Box::new(|width, height, config| {
                Ok(Box::new(LatchEncoder::open(width, height, config)?))
            }),
            describe: ("Latch software H.264".into(), false, Codec::H264Baseline),
            pacer: Pacer::new(config.fps),
            last_sent: None,
            captured: Instant::now(),
            timings: SoftwareTimings::default(),
            threads: std::thread::available_parallelism().map_or(1, |n| n.get().min(4)),
        })
    }

    /// Encodes with what `open` opens instead: opened here, for the
    /// capture's present size, so that a failure shows before streaming.
    pub fn with_encoder(
        capture: C,
        config: &VideoConfig,
        mut open: OpenEncoder,
    ) -> anyhow::Result<Self> {
        let (width, height) = capture.size();
        let encoder = open(width, height, config)?;
        let mut this = Self::new(capture, config)?;
        this.describe = (encoder.name(), encoder.hardware(), encoder.codec());
        this.encoder = Some(encoder);
        this.encoder_size = (width, height);
        this.open = open;
        Ok(this)
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

/// Frames a second this machine's CPU takes through the software path at
/// `width` × `height`: the test pattern's moving bar drawn, converted and
/// encoded as a session would, at no frame-rate limit, for about
/// `budget`. A host that encodes in software reports this as its ceiling,
/// so a viewer asks for what it keeps up with rather than what a core
/// count suggests. 0 when the pipeline cannot be opened.
pub fn measure_fps(width: u32, height: u32, budget: Duration) -> u32 {
    let fps = latch_proto::control::MAX_FPS;
    let pattern = crate::pattern::Pattern::new(width, height, fps, crate::pattern::Motion::Sweep);
    let config = VideoConfig {
        width,
        height,
        fps,
        bitrate_kbps: 20_000,
        codec: Codec::H264Baseline,
    };
    let Ok(mut pipeline) = SoftwarePipeline::new(pattern, &config) else {
        return 0;
    };
    // The first frame is an IDR and sets up the encoder: not counted.
    if pipeline.next(true, Duration::ZERO).is_err() {
        return 0;
    }
    let (start, mut frames) = (Instant::now(), 0u32);
    while start.elapsed() < budget {
        if pipeline.next(false, Duration::ZERO).is_err() {
            return 0;
        }
        frames += 1;
    }
    (f64::from(frames) / start.elapsed().as_secs_f64()) as u32
}

/// What the software path keeps up with: `fps` frames a second at
/// `width` × `height`, as [`measure_fps`] found it. Encoding time follows
/// the pixel count, so it holds any picture to the same pixels a second.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Ceiling {
    pub width: u32,
    pub height: u32,
    pub fps: u32,
}

impl Ceiling {
    /// Until the host has measured: 30 fps at 1080p, what 5.1 held a
    /// small CPU to. A first connection must not ask a slow CPU for its
    /// display's refresh rate at 4K while the measurement runs.
    pub const PROVISIONAL: Ceiling = Ceiling {
        width: 1920,
        height: 1080,
        fps: 30,
    };

    fn pixel_rate(&self) -> u64 {
        u64::from(self.fps) * u64::from(self.width) * u64::from(self.height)
    }

    /// Frames a second at `width` × `height`.
    pub fn fps_at(&self, width: u32, height: u32) -> u32 {
        let pixels = (u64::from(width) * u64::from(height)).max(1);
        u32::try_from(self.pixel_rate() / pixels).unwrap_or(u32::MAX)
    }

    /// `width` × `height` at `fps` held to this: the frame rate comes down
    /// first, to no less than 30 (or what was asked, if less), then the
    /// picture shrinks in proportion. Sides stay even.
    pub fn fit(&self, width: u32, height: u32, fps: u32) -> (u32, u32, u32) {
        let at = self.fps_at(width, height);
        if fps <= at {
            return (width, height, fps);
        }
        let least = fps.min(SMOOTH);
        if at >= least {
            return (width, height, at);
        }
        let pixels = (u64::from(width) * u64::from(height)).max(1);
        let scale = ((self.pixel_rate() / u64::from(least.max(1))) as f64 / pixels as f64).sqrt();
        let even = |v: u32| ((f64::from(v) * scale) as u32 / 2 * 2).max(64);
        (even(width), even(height), least)
    }
}

/// The least frame rate [`Ceiling::fit`] trades picture size for.
const SMOOTH: u32 = 30;

static CEILING: parking_lot::Mutex<Option<Ceiling>> = parking_lot::Mutex::new(None);

/// What a session is held to while this host encodes in software; None
/// when it does not, and nothing is held. The host sets it (see
/// `set_ceiling`) from what it measured, so a request it did not fit
/// itself cannot ask for more.
pub fn ceiling() -> Option<Ceiling> {
    *CEILING.lock()
}

pub fn set_ceiling(ceiling: Option<Ceiling>) {
    *CEILING.lock() = ceiling.filter(|c| c.fps > 0 && c.width > 0 && c.height > 0);
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
        let (name, hardware, codec) = &self.describe;
        VideoInfo {
            encoder: format!("{name} ({})", self.capture.name()),
            hardware: *hardware,
            codec: *codec,
            width,
            height,
            display: self.capture.display(),
        }
    }

    fn next(&mut self, keyframe: bool, idle: Duration) -> anyhow::Result<EncodedVideo> {
        let changed = self.acquire(keyframe, idle)?;
        let resized =
            (self.picture.width, self.picture.height) != (self.frame.width, self.frame.height);
        let size = (self.frame.width, self.frame.height);
        if self.encoder.is_none() || self.encoder_size != size {
            self.encoder = None;
            let encoder = (self.open)(size.0, size.1, &self.config)?;
            // At another size the encoder may be another (software where
            // hardware does not take it).
            self.describe = (encoder.name(), encoder.hardware(), encoder.codec());
            self.encoder = Some(encoder);
            self.encoder_size = size;
        }
        if resized {
            self.picture = Yuv420::new(self.frame.width, self.frame.height);
        }
        let started = Instant::now();
        // What differs from the picture encoded last: a repeat changes
        // nothing.
        let mut damage = Some(Vec::new());
        if changed || resized {
            let whole = [Rect {
                x: 0,
                y: 0,
                width: self.frame.width,
                height: self.frame.height,
            }];
            damage = if resized { None } else { self.capture.damage() };
            latch_codec::color::bgra_to_yuv420_rects(
                &self.frame.data,
                self.frame.width,
                self.frame.height,
                self.frame.stride,
                damage.as_deref().unwrap_or(&whole),
                self.threads,
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
        let sent_keyframe = self
            .encoder
            .as_mut()
            .ok_or_else(|| anyhow::anyhow!("encoder unavailable"))?
            .encode_changed(
                &self.picture,
                damage.as_deref(),
                keyframe || resized,
                &mut data,
            )?;
        self.timings.encode = started.elapsed();
        self.last_sent = Some(self.captured);
        Ok(EncodedVideo {
            data,
            keyframe: sent_keyframe,
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
        waits: Vec<Duration>,
    }
    impl CpuCapture for Fake {
        fn size(&self) -> (u32, u32) {
            self.size
        }
        fn grab(&mut self, frame: &mut Bgra, timeout: Duration) -> anyhow::Result<bool> {
            self.waits.push(timeout);
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
                waits: Vec::new(),
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
        p.capture.waits.clear();
        assert!(!p.acquire(true, Duration::from_secs(10)).unwrap());
        // A keyframe polls once and ignores the idle wait. Cadence's
        // fake-clock test verifies pacing without relying on VM scheduling.
        assert_eq!(p.capture.waits, [Duration::ZERO]);
        p.capture.waits.clear();
        let last_sent = Instant::now();
        p.last_sent = Some(last_sent);
        assert!(!p.acquire(false, Duration::from_millis(25)).unwrap());
        assert!(last_sent.elapsed() >= Duration::from_millis(25));
        assert!(p
            .capture
            .waits
            .iter()
            .all(|&wait| wait <= Duration::from_millis(10)));
        p.capture.size = (32, 16);
        p.capture.changed = true;
        assert!(p.acquire(false, Duration::ZERO).unwrap());
        assert_eq!(p.info().width, 32);
        p.set_bitrate(2000);
        assert_eq!(p.config.bitrate_kbps, 2000);
    }
    #[test]
    fn the_software_ceiling_is_measured() {
        let fps = measure_fps(320, 240, Duration::from_millis(200));
        assert!(fps > 0, "a small picture encodes at some rate");
        // The pacer's grid runs 1% fast, and over 200 ms the first frame,
        // due at once, adds 5 fps: 500 asked measures up to about 511.
        assert!(fps <= latch_proto::control::MAX_FPS + 25, "{fps} fps");
        assert_eq!(
            measure_fps(3, 3, Duration::from_millis(10)),
            0,
            "odd sizes cannot open"
        );
    }

    #[test]
    fn a_software_ceiling_holds_the_pixels_a_second() {
        let measured = Ceiling {
            width: 1920,
            height: 1080,
            fps: 120,
        };
        assert_eq!(measured.fit(1920, 1080, 60), (1920, 1080, 60));
        assert_eq!(measured.fit(1920, 1080, 240), (1920, 1080, 120));
        assert_eq!(measured.fit(1280, 720, 240), (1280, 720, 240));
        assert_eq!(measured.fit(3840, 2160, 120), (3840, 2160, 30));
        // Until measured, a cold 4K request at a display's refresh rate
        // becomes 1080p at 30.
        let (w, h, fps) = Ceiling::PROVISIONAL.fit(3840, 2160, 120);
        assert_eq!((fps, w % 2, h % 2), (30, 0, 0));
        assert!(
            u64::from(w) * u64::from(h) <= 1920 * 1080 && w >= 1900,
            "{w} × {h}"
        );
        assert_eq!(Ceiling::PROVISIONAL.fit(1920, 1080, 24), (1920, 1080, 24));
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

    struct Counting {
        kbps: u32,
    }
    impl PictureEncoder for Counting {
        fn name(&self) -> String {
            "Counting HEVC".into()
        }
        fn hardware(&self) -> bool {
            true
        }
        fn codec(&self) -> Codec {
            Codec::Hevc
        }
        fn encode(
            &mut self,
            p: &Yuv420,
            keyframe: bool,
            out: &mut Vec<u8>,
        ) -> anyhow::Result<bool> {
            out.extend_from_slice(&[0, 0, 0, 1, p.width as u8, self.kbps as u8]);
            Ok(keyframe)
        }
        fn set_bitrate(&mut self, kbps: u32) {
            self.kbps = kbps;
        }
    }
    #[test]
    fn a_hardware_encoder_opens_up_front_and_again_on_resize() {
        let opened = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let log = opened.clone();
        let p = pipeline();
        let mut p = SoftwarePipeline::with_encoder(
            p.capture,
            &p.config,
            Box::new(move |w, h, config| {
                log.lock().unwrap().push((w, h));
                Ok(Box::new(Counting {
                    kbps: config.bitrate_kbps,
                }))
            }),
        )
        .unwrap();
        let info = p.info();
        assert_eq!(info.encoder, "Counting HEVC (fake)");
        assert!(info.hardware);
        assert_eq!(info.codec, Codec::Hevc);
        assert_eq!(*opened.lock().unwrap(), [(16, 16)]);
        let first = p.next(true, Duration::ZERO).unwrap();
        assert!(first.keyframe);
        assert_eq!(first.data, [0, 0, 0, 1, 16, 232]);
        p.set_bitrate(7);
        assert!(!p.next(false, Duration::ZERO).unwrap().keyframe);
        p.capture.size = (32, 16);
        p.capture.changed = true;
        let resized = p.next(false, Duration::ZERO).unwrap();
        assert!(resized.keyframe, "a new size starts with a keyframe");
        assert_eq!(*opened.lock().unwrap(), [(16, 16), (32, 16)]);
    }
}
