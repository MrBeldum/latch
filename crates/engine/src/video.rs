//! Capture plus encode, as one pipeline per platform, so a platform can
//! keep pictures on the GPU from the screen to the encoder.

use crate::input::DisplayRect;
pub use latch_proto::control::Codec;
use std::time::{Duration, Instant};

#[derive(Debug, Clone, PartialEq)]
pub struct VideoConfig {
    /// The picture size to send. A pipeline that cannot scale sends the
    /// desktop's own size and says so in [`VideoInfo`].
    pub width: u32,
    pub height: u32,
    pub fps: u32,
    pub bitrate_kbps: u32,
    pub codec: Codec,
}

/// One encoded frame: an Annex B access unit (SPS/PPS, or VPS/SPS/PPS for
/// HEVC, in band before every keyframe).
#[derive(Debug, Clone)]
pub struct EncodedVideo {
    pub data: Vec<u8>,
    pub keyframe: bool,
    /// When the picture was captured.
    pub captured: Instant,
}

#[derive(Debug, Clone, PartialEq)]
pub struct VideoInfo {
    /// For people: "VideoToolbox HEVC", "Media Foundation H.264 (NVIDIA)",
    /// "Latch software H.264".
    pub encoder: String,
    /// Encoding runs on dedicated hardware.
    pub hardware: bool,
    pub codec: Codec,
    /// The picture size actually sent.
    pub width: u32,
    pub height: u32,
    /// The captured display, where absolute mouse positions land.
    pub display: DisplayRect,
}

/// A cursor update from the capture pipeline to be sent to the viewer.
#[derive(Debug, Clone, PartialEq)]
pub struct CursorUpdate {
    pub visible: bool,
    pub shape: Option<latch_proto::control::CursorShape>,
}

pub trait VideoPipeline: Send {
    fn info(&self) -> VideoInfo;

    /// The next encoded frame: as soon as the screen changes (at most `fps`
    /// a second), or a repeat of the last picture once `idle` has passed
    /// without a change. With `keyframe`, the next frame is an IDR and comes
    /// within one frame interval even if nothing changed.
    fn next(&mut self, keyframe: bool, idle: Duration) -> anyhow::Result<EncodedVideo>;

    /// Takes effect from the next frame where the encoder allows.
    fn set_bitrate(&mut self, kbps: u32) {
        let _ = kbps;
    }

    /// Controls whether the pipeline composites the host cursor into video frames.
    /// Returns `true` if the pipeline supports controlling cursor drawing.
    fn set_cursor_drawn(&mut self, _drawn: bool) -> bool {
        false
    }

    /// Returns any pending cursor visibility or shape update to send to the viewer.
    fn take_cursor(&mut self) -> Option<CursorUpdate> {
        None
    }
}

/// When frames are taken: on a fixed grid a hair faster than the stream's
/// rate, so that time spent capturing, converting and encoding a frame comes
/// out of its interval instead of adding to it. A screen that refreshes at
/// the stream's rate gives up every refresh (its clock runs a little slower
/// than the grid, so the grid waits for it), and a screen that changes all
/// the time is sampled at the stream's rate. A frame more than an interval
/// late moves the grid rather than bursting to catch up.
#[derive(Debug, Clone)]
pub struct Pacer {
    interval: Duration,
    due: Option<Instant>,
}

impl Pacer {
    pub fn new(fps: u32) -> Self {
        Self {
            interval: Duration::from_secs_f64(PACE / f64::from(fps.max(1))),
            due: None,
        }
    }

    /// The grid's step.
    pub fn interval(&self) -> Duration {
        self.interval
    }

    /// When the next frame may be taken; `None` before the first.
    pub fn due(&self) -> Option<Instant> {
        self.due
    }

    /// Sleeps until the next frame may be taken.
    pub fn wait(&self) {
        if let Some(wait) = self
            .due
            .and_then(|due| due.checked_duration_since(Instant::now()))
        {
            std::thread::sleep(wait);
        }
    }

    /// A frame was taken at `at`.
    pub fn taken(&mut self, at: Instant) {
        self.due = Some(match self.due {
            Some(due) if at < due + self.interval => due + self.interval,
            _ => at + self.interval,
        });
    }
}

/// The grid runs this much of a nominal interval per frame: 1% fast.
const PACE: f64 = 0.99;

/// What one codec's encoder takes on this machine.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Limits {
    /// Longest side of a picture, in pixels.
    pub max_side: u32,
    /// Largest picture, in pixels.
    pub max_pixels: u64,
    /// Pixels a second: picture size times frame rate.
    pub max_pixel_rate: u64,
    /// Kilobits a second.
    pub max_kbps: u32,
}

impl Limits {
    /// The codec's own ceiling: its highest level (H.264 6.2, HEVC Main
    /// 6.2 High tier, AV1 6.3 High tier), which every encoder stays
    /// within. All three allow 8192 × 4352 pixels a picture and 4.28
    /// billion a second (4K at 515 fps, 1080p at 2063).
    pub const fn level(codec: Codec) -> Self {
        Self {
            max_side: latch_proto::control::MAX_SIDE,
            max_pixels: 35_651_584,
            max_pixel_rate: 4_278_190_080,
            max_kbps: match codec {
                // High's coded picture buffer is 1.25 times Baseline's.
                Codec::H264High => 1_000_000,
                Codec::H264Baseline | Codec::Hevc | Codec::Av1 => 800_000,
            },
        }
    }

    /// `width` × `height` at `fps` within these limits: see
    /// [`latch_proto::control::fit`], which a viewer fits with too.
    pub fn fit(&self, width: u32, height: u32, fps: u32) -> (u32, u32, u32) {
        latch_proto::control::fit(
            (self.max_side, self.max_pixels, self.max_pixel_rate),
            width,
            height,
            fps,
        )
    }
}

pub use latch_proto::control::coded_pixels;

/// What the encoder for `codec` takes here.
pub fn limits(codec: Codec) -> Limits {
    if crate::testmode::motion().is_some() {
        return Limits::level(codec);
    }
    #[cfg(target_os = "macos")]
    return crate::mac::limits(codec);
    #[cfg(target_os = "linux")]
    return crate::linux::limits(codec);
    #[cfg(windows)]
    return Limits::level(codec);
}

/// Whether sessions in `codec` are encoded in hardware here, so that the
/// software path's measured ceiling does not hold them. Only a Linux host
/// knows per codec; elsewhere the ceiling holds whenever the host has one.
pub fn hardware_encodes(codec: Codec) -> bool {
    if crate::testmode::motion().is_some() {
        return false;
    }
    #[cfg(target_os = "linux")]
    return crate::linux::hardware_encodes(codec);
    #[cfg(not(target_os = "linux"))]
    {
        let _ = codec;
        false
    }
}

/// Whether `codec` can stream a `width` × `height` picture here: false
/// where its only encoder does not take that size (VA-API HEVC on a size
/// that is not a whole number of its coding blocks).
pub fn takes(codec: Codec, width: u32, height: u32) -> bool {
    if crate::testmode::motion().is_some() {
        return true;
    }
    #[cfg(target_os = "linux")]
    return crate::linux::takes(codec, width, height);
    #[cfg(not(target_os = "linux"))]
    {
        let _ = (codec, width, height);
        true
    }
}

/// A pipeline error that says the session's codec cannot go on here (its
/// only encoder does not take the desktop's new size, or failed to open):
/// the session ends so that the viewer reconnects and negotiates another,
/// instead of reopening the same one.
#[derive(Debug)]
pub struct Unencodable(pub String);

impl std::fmt::Display for Unencodable {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for Unencodable {}

impl Unencodable {
    pub fn is(error: &anyhow::Error) -> bool {
        error.chain().any(|e| e.is::<Unencodable>())
    }
}

/// What this machine can encode, best first.
pub fn encodable() -> Vec<Codec> {
    if crate::testmode::motion().is_some() {
        return vec![Codec::H264Baseline];
    }
    #[cfg(target_os = "macos")]
    return crate::mac::encodable();
    #[cfg(windows)]
    return crate::windows::encodable();
    #[cfg(target_os = "linux")]
    return crate::linux::encodable();
}

/// Opens this platform's pipeline for `config`.
pub fn open(config: &VideoConfig) -> anyhow::Result<Box<dyn VideoPipeline>> {
    if let Some(motion) = crate::testmode::motion() {
        return crate::testmode::open_video(config, motion);
    }
    #[cfg(target_os = "macos")]
    return crate::mac::open_video(config);
    #[cfg(windows)]
    return crate::windows::open_video(config);
    #[cfg(target_os = "linux")]
    return crate::linux::open_video(config);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn limits_scale_a_picture_down_in_proportion_and_then_the_frame_rate() {
        let h264 = Limits {
            max_side: 4096,
            max_pixels: 4096 * 2160,
            ..Limits::level(Codec::H264High)
        };
        assert_eq!(
            h264.fit(3024, 1964, 120),
            (3024, 1964, 120),
            "fits as asked"
        );
        let (w, h, fps) = h264.fit(5120, 2880, 60);
        assert!(
            u64::from(w) * u64::from(h) <= h264.max_pixels && fps == 60,
            "5K shrinks"
        );
        assert!(
            (w as f32 / h as f32 - 16.0 / 9.0).abs() < 0.01,
            "in proportion"
        );
        assert_eq!(h264.fit(5120, 1440, 60), (4096, 1152, 60), "the long side");
        let (w, h, _) = h264.fit(3840, 2400, 60);
        assert!(w % 2 == 0 && h % 2 == 0);
        // 3762 × 2350 has fewer pixels than 4096 × 2160 but more blocks.
        assert!(coded_pixels(w, h) <= h264.max_pixels, "{w} × {h}");
        assert!(coded_pixels(3762, 2350) > h264.max_pixels);
        assert!((w as f32 / h as f32 - 1.6).abs() < 0.01, "{w} × {h}");
        for (width, height) in [(3840, 2400), (5120, 2880), (3456, 2234), (6016, 3384)] {
            let (w, h, _) = h264.fit(width, height, 60);
            assert!(
                coded_pixels(w, h) <= h264.max_pixels,
                "{width} × {height}: {w} × {h}"
            );
            assert!(w.max(h) <= h264.max_side && w % 2 == 0 && h % 2 == 0);
        }
        let level = Limits::level(Codec::Hevc);
        assert_eq!(level.fit(3840, 2160, 500), (3840, 2160, 500));
        assert_eq!(
            level.fit(7680, 4320, 240),
            (7680, 4320, 128),
            "8K at the pixel rate"
        );
        assert_eq!(Limits::level(Codec::H264High).max_kbps, 1_000_000);
        assert_eq!(Limits::level(Codec::H264Baseline).max_kbps, 800_000);
    }

    #[test]
    fn work_comes_out_of_the_interval() {
        let mut pacer = Pacer::new(100);
        let t0 = Instant::now();
        assert_eq!(pacer.due(), None);
        pacer.taken(t0);
        let step = pacer.interval();
        assert!(step < Duration::from_millis(10) && step > Duration::from_micros(9_800));
        // Taken 4 ms late: the next is still on the grid.
        pacer.taken(t0 + step + Duration::from_millis(4));
        assert_eq!(pacer.due(), Some(t0 + step * 2));
        // More than an interval late: the grid moves, no burst.
        let late = t0 + step * 4;
        pacer.taken(late);
        assert_eq!(pacer.due(), Some(late + step));
    }

    #[test]
    fn an_encoder_slower_than_the_grid_is_never_ahead_of_it() {
        let mut pacer = Pacer::new(60);
        let t0 = Instant::now();
        let mut at = t0;
        for _ in 0..50 {
            pacer.taken(at);
            // 20 ms of work per frame at a 16.5 ms grid: each frame is taken
            // as soon as the last is done, never before its slot.
            at = (at + Duration::from_millis(20)).max(pacer.due().unwrap());
        }
        let per_frame = (at - t0) / 50;
        assert!(per_frame >= Duration::from_millis(20) && per_frame < Duration::from_millis(21));
    }
}
