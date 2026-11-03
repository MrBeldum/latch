//! Which hardware encoder a Linux host uses, found once per process (the
//! host starts looking when its service starts, not when the first viewer
//! asks): NVENC, then VA-API, each tried for real. A codec counts only if
//! its encoder opens, encodes an IDR with its parameter sets in band and
//! then a P-frame; a missing library, device or driver, or a driver that
//! leaves the parameter sets out, is no more than the software encoder
//! taking over. What each encoder takes (its largest picture, its rate,
//! the alignment HEVC needs) is asked of the driver while it is open.
//!
//! `$LATCH_LINUX_ENCODER` chooses: `auto` (the default), `software`,
//! `nvenc` or `vaapi`. Under `auto` a viewer that asks for Constrained
//! Baseline (one that decodes with `latch-codec`) still gets the software
//! encoder, whose output that decoder is tested against, and hardware
//! Baseline is not tried; naming a hardware encoder uses it for Baseline
//! too.
//!
//! An encoder that fails to open for a session is given up for the rest
//! of the process only when the failure says it never will open
//! ([`Unusable`]: no such profile, a driver that does not initialize);
//! anything else (a GeForce card's session limit, memory, a size the
//! encoder does not take) fails only that open.

use super::{nvenc, vaapi};
use crate::software::PictureEncoder;
use crate::video::{Codec, Limits, VideoConfig};
use anyhow::Result;
use std::sync::OnceLock;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Backend {
    Nvenc,
    Vaapi,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Preference {
    Auto,
    Software,
    Only(Backend),
}

fn preference(value: Option<&str>) -> Preference {
    match value.map(str::trim) {
        Some(v) if v.eq_ignore_ascii_case("software") || v == "0" => Preference::Software,
        Some(v) if v.eq_ignore_ascii_case("nvenc") => Preference::Only(Backend::Nvenc),
        Some(v) if v.eq_ignore_ascii_case("vaapi") || v.eq_ignore_ascii_case("va-api") => {
            Preference::Only(Backend::Vaapi)
        }
        _ => Preference::Auto,
    }
}

fn chosen() -> Preference {
    preference(std::env::var("LATCH_LINUX_ENCODER").ok().as_deref())
}

/// An open failure that will not go away: the driver has no such
/// encoder, or does not initialize. Every other failure is for one open.
#[derive(Debug)]
pub struct Unusable(pub String);

impl std::fmt::Display for Unusable {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for Unusable {}

/// `error`, marked as one that will not go away.
pub fn unusable(error: anyhow::Error) -> anyhow::Error {
    if is_unusable(&error) {
        return error;
    }
    Unusable(format!("{error:#}")).into()
}

pub fn is_unusable(error: &anyhow::Error) -> bool {
    error.chain().any(|e| e.is::<Unusable>())
}

/// What one hardware encoder takes, as its driver says.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Caps {
    pub max_width: u32,
    pub max_height: u32,
    /// Largest picture in coded 16-pixel blocks' pixels; 0 when the
    /// driver does not say (the codec's level holds).
    pub max_pixels: u64,
    /// Pixels a second, likewise.
    pub max_pixel_rate: u64,
    /// Both sides must be a multiple of this: HEVC through VA-API, whose
    /// parameters have no conformance window to crop a padded picture
    /// with, codes the picture's own size in whole minimum coding blocks.
    pub align: u32,
}

impl Caps {
    /// A driver that says nothing about its sizes.
    pub const UNKNOWN: Caps = Caps {
        max_width: latch_proto::control::MAX_SIDE,
        max_height: latch_proto::control::MAX_SIDE,
        max_pixels: 0,
        max_pixel_rate: 0,
        align: 2,
    };

    /// Whether a `width` × `height` picture opens.
    pub fn fits(&self, width: u32, height: u32) -> bool {
        let align = self.align.max(1);
        width <= self.max_width
            && height <= self.max_height
            && width.is_multiple_of(align)
            && height.is_multiple_of(align)
            && (self.max_pixels == 0
                || crate::video::coded_pixels(width, height) <= self.max_pixels)
    }

    /// As the engine's limits: the codec's level, lowered to these.
    fn limits(&self, codec: Codec) -> Limits {
        let level = Limits::level(codec);
        let area = u64::from(self.max_width) * u64::from(self.max_height);
        let pixels = match self.max_pixels {
            0 => area,
            max => max.min(area),
        };
        let rate = match self.max_pixel_rate {
            0 => level.max_pixel_rate,
            max => max.min(level.max_pixel_rate),
        };
        Limits {
            max_side: self.max_width.max(self.max_height).min(level.max_side),
            max_pixels: pixels.min(level.max_pixels),
            max_pixel_rate: rate,
            max_kbps: level.max_kbps,
        }
    }
}

pub fn open(
    backend: Backend,
    codec: Codec,
    width: u32,
    height: u32,
    config: &VideoConfig,
) -> Result<Box<dyn PictureEncoder>> {
    Ok(open_with_caps(backend, codec, width, height, config)?.0)
}

fn open_with_caps(
    backend: Backend,
    codec: Codec,
    width: u32,
    height: u32,
    config: &VideoConfig,
) -> Result<(Box<dyn PictureEncoder>, Caps)> {
    Ok(match backend {
        Backend::Nvenc => {
            let encoder = nvenc::Nvenc::open(codec, width, height, config)?;
            let caps = encoder.caps();
            (Box::new(encoder), caps)
        }
        Backend::Vaapi => {
            let encoder = vaapi::Vaapi::open(codec, width, height, config)?;
            let caps = encoder.caps();
            (Box::new(encoder), caps)
        }
    })
}

/// Every codec hardware here encodes, with the backend that does and what
/// it takes.
fn found() -> &'static [(Codec, Backend, Caps)] {
    static FOUND: OnceLock<Vec<(Codec, Backend, Caps)>> = OnceLock::new();
    FOUND.get_or_init(|| {
        let preference = chosen();
        let backends: &[Backend] = match preference {
            Preference::Software => &[],
            Preference::Only(Backend::Nvenc) => &[Backend::Nvenc],
            Preference::Only(_) => &[Backend::Vaapi],
            Preference::Auto => &[Backend::Nvenc, Backend::Vaapi],
        };
        // Under `auto` Baseline is always the software encoder's.
        let codecs: &[Codec] = match preference {
            Preference::Auto => &[Codec::Av1, Codec::Hevc, Codec::H264High],
            _ => &[
                Codec::Av1,
                Codec::Hevc,
                Codec::H264High,
                Codec::H264Baseline,
            ],
        };
        let mut found: Vec<(Codec, Backend, Caps)> = Vec::new();
        for &backend in backends {
            let present = match backend {
                Backend::Nvenc => nvenc::present(),
                Backend::Vaapi => vaapi::present(),
            };
            if !present {
                continue;
            }
            let mut first_error = None;
            let before = found.len();
            for &codec in codecs {
                if found.iter().any(|&(c, _, _)| c == codec) {
                    continue;
                }
                match probe(backend, codec) {
                    Ok(caps) => {
                        tracing::info!("{backend:?} encodes {codec:?}: {caps:?}");
                        found.push((codec, backend, caps));
                    }
                    Err(e) => {
                        tracing::debug!("{backend:?} {codec:?}: {e:#}");
                        first_error.get_or_insert(e);
                    }
                }
            }
            // A driver that is installed but encodes nothing (no encoder, or
            // a render node this user may not open) is worth one line.
            if found.len() == before {
                if let Some(e) = first_error {
                    tracing::info!("{backend:?} encodes nothing here: {e:#}");
                }
            }
        }
        found
    })
}

/// Codecs whose encoder failed to open for a session, after its probe, in
/// a way that will not go away.
static FAILED: std::sync::Mutex<Vec<Codec>> = std::sync::Mutex::new(Vec::new());

fn hardware(codec: Codec) -> Option<(Backend, Caps)> {
    if codec == Codec::H264Baseline && chosen() == Preference::Auto {
        return None;
    }
    if FAILED.lock().is_ok_and(|failed| failed.contains(&codec)) {
        return None;
    }
    found()
        .iter()
        .find(|&&(c, _, _)| c == codec)
        .map(|&(_, b, caps)| (b, caps))
}

/// The hardware encoder for `codec`, if one should be used.
pub fn backend(codec: Codec) -> Option<Backend> {
    hardware(codec).map(|(backend, _)| backend)
}

/// Whether the hardware encoder for `codec` takes `width` × `height`.
pub fn fits(codec: Codec, width: u32, height: u32) -> bool {
    hardware(codec).is_some_and(|(_, caps)| caps.fits(width, height))
}

/// What the hardware encoder for `codec` takes; `None` when there is none.
pub fn limits(codec: Codec) -> Option<Limits> {
    hardware(codec).map(|(_, caps)| caps.limits(codec))
}

/// `codec`'s encoder failed to open for a session. Later sessions are not
/// offered it if the failure will not go away.
pub fn failed(codec: Codec, error: &anyhow::Error) {
    if !is_unusable(error) {
        tracing::warn!(
            "hardware {} encoder failed to open for this session: {error:#}",
            codec.name()
        );
        return;
    }
    tracing::warn!(
        "hardware {} encoder failed, not offering it again: {error:#}",
        codec.name()
    );
    if let Ok(mut failed) = FAILED.lock() {
        failed.push(codec);
    }
}

/// What this machine encodes in hardware, plus the software encoder's
/// Constrained Baseline.
pub fn encodable() -> Vec<Codec> {
    let mut codecs: Vec<Codec> = [Codec::Av1, Codec::Hevc, Codec::H264High]
        .into_iter()
        .filter(|&c| backend(c).is_some())
        .collect();
    codecs.push(Codec::H264Baseline);
    codecs
}

/// Opens `codec` on `backend` at a small size and encodes an IDR and a
/// P-frame of a grey picture with a moving bar.
fn probe(backend: Backend, codec: Codec) -> Result<Caps> {
    let (width, height) = (640, 360);
    let config = VideoConfig {
        width,
        height,
        fps: 60,
        bitrate_kbps: 4000,
        codec,
    };
    let (mut encoder, caps) = open_with_caps(backend, codec, width, height, &config)?;
    let mut picture = latch_codec::Yuv420::new(width, height);
    let mut first = Vec::new();
    anyhow::ensure!(
        encoder.encode(&picture, true, &mut first)?,
        "no keyframe when asked"
    );
    for row in picture.y.chunks_mut(picture.y_stride) {
        row[100..140].fill(200);
    }
    let mut second = Vec::new();
    encoder.encode(&picture, false, &mut second)?;
    anyhow::ensure!(!second.is_empty(), "no P-frame");
    check_keyframe(codec, &first)?;
    Ok(caps)
}

/// Whether `unit` starts a stream on its own: parameter sets (a sequence
/// header for AV1) and an intra picture.
fn check_keyframe(codec: Codec, unit: &[u8]) -> Result<()> {
    match codec {
        Codec::Av1 => {
            let types = obu_types(unit);
            anyhow::ensure!(types.contains(&1), "AV1 keyframe without a sequence header");
            anyhow::ensure!(
                types.contains(&6) || types.contains(&3),
                "AV1 keyframe without a frame"
            );
        }
        Codec::Hevc => {
            let types: Vec<u8> = nal_units(unit).map(|n| (n[0] >> 1) & 0x3f).collect();
            for (t, what) in [(32, "VPS"), (33, "SPS"), (34, "PPS")] {
                anyhow::ensure!(types.contains(&t), "HEVC keyframe without a {what}");
            }
            anyhow::ensure!(
                types.iter().any(|t| (16..=21).contains(t)),
                "HEVC keyframe without an IRAP picture"
            );
        }
        Codec::H264High | Codec::H264Baseline => {
            let types: Vec<u8> = nal_units(unit).map(|n| n[0] & 0x1f).collect();
            for (t, what) in [(7, "SPS"), (8, "PPS"), (5, "IDR slice")] {
                anyhow::ensure!(types.contains(&t), "H.264 keyframe without a {what}");
            }
        }
    }
    Ok(())
}

/// The NAL units of an Annex B stream, without start codes.
fn nal_units(stream: &[u8]) -> impl Iterator<Item = &[u8]> {
    let mut starts = Vec::new();
    let mut i = 0;
    while i + 3 <= stream.len() {
        if stream[i..i + 3] == [0, 0, 1] {
            starts.push(i + 3);
            i += 3;
        } else {
            i += 1;
        }
    }
    let ends: Vec<usize> = starts
        .iter()
        .skip(1)
        .map(|&s| {
            let mut end = s - 3;
            while end > 0 && stream[end - 1] == 0 {
                end -= 1;
            }
            end
        })
        .chain(std::iter::once(stream.len()))
        .collect();
    starts
        .into_iter()
        .zip(ends)
        .filter(|(s, e)| e > s)
        .map(move |(s, e)| &stream[s..e])
}

/// The OBU types of a low-overhead AV1 temporal unit (every OBU with its
/// size, as encoders write them for streaming).
fn obu_types(mut unit: &[u8]) -> Vec<u8> {
    let mut types = Vec::new();
    while let Some(&header) = unit.first() {
        let kind = (header >> 3) & 0xf;
        let extension = header & 4 != 0;
        if header & 2 == 0 {
            types.push(kind);
            break;
        }
        let mut at = 1 + usize::from(extension);
        let mut size = 0usize;
        let mut shift = 0;
        loop {
            let Some(&b) = unit.get(at) else {
                return types;
            };
            at += 1;
            size |= usize::from(b & 0x7f) << shift;
            shift += 7;
            if b & 0x80 == 0 || shift > 56 {
                break;
            }
        }
        types.push(kind);
        unit = unit.get(at + size..).unwrap_or_default();
    }
    types
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_preference_comes_from_the_environment_value() {
        assert_eq!(preference(None), Preference::Auto);
        assert_eq!(preference(Some("auto")), Preference::Auto);
        assert_eq!(preference(Some("software")), Preference::Software);
        assert_eq!(preference(Some("0")), Preference::Software);
        assert_eq!(
            preference(Some(" NVENC ")),
            Preference::Only(Backend::Nvenc)
        );
        assert_eq!(preference(Some("va-api")), Preference::Only(Backend::Vaapi));
    }

    #[test]
    fn only_failures_that_last_give_a_codec_up() {
        let size = anyhow::anyhow!("VA-API HEVC takes up to 4096 × 4096 in multiples of 8");
        assert!(!is_unusable(&size));
        let missing = unusable(anyhow::anyhow!("Intel iHD encodes no HEVC"));
        assert!(is_unusable(&missing));
        assert!(is_unusable(&missing.context("open the HEVC encoder")));
        assert!(is_unusable(&unusable(unusable(anyhow::anyhow!("twice")))));
    }

    #[test]
    fn caps_say_which_sizes_open() {
        let hevc = Caps {
            max_width: 4096,
            max_height: 2304,
            max_pixels: 4096 * 2304,
            max_pixel_rate: 0,
            align: 8,
        };
        assert!(hevc.fits(1920, 1080));
        assert!(hevc.fits(1600, 896));
        // 1366 and 900 are not whole 8-pixel coding blocks.
        assert!(!hevc.fits(1366, 768));
        assert!(!hevc.fits(1600, 900));
        assert!(!hevc.fits(5120, 1440), "wider than the encoder");
        let limits = hevc.limits(Codec::Hevc);
        assert_eq!(limits.max_side, 4096);
        assert_eq!(limits.max_pixels, 4096 * 2304);
        assert_eq!(
            limits.max_pixel_rate,
            Limits::level(Codec::Hevc).max_pixel_rate
        );
        // NVENC's H.264, as a driver reports it.
        let h264 = Caps {
            max_width: 4096,
            max_height: 4096,
            max_pixels: 65_536 * 256,
            max_pixel_rate: 983_040 * 256,
            align: 2,
        };
        assert!(h264.fits(1366, 768));
        assert!(!h264.fits(5120, 1440));
        assert_eq!(h264.limits(Codec::H264High).max_pixel_rate, 983_040 * 256);
        assert_eq!(
            Caps::UNKNOWN.limits(Codec::Av1),
            Limits::level(Codec::Av1),
            "a driver that says nothing leaves the level"
        );
    }

    #[test]
    fn keyframes_need_their_parameter_sets() {
        let h264 = [
            0, 0, 0, 1, 0x67, 1, 2, 0, 0, 1, 0x68, 3, 0, 0, 1, 0x65, 4, 5,
        ];
        assert!(check_keyframe(Codec::H264High, &h264).is_ok());
        assert!(check_keyframe(Codec::H264High, &h264[9..]).is_err());
        let units: Vec<_> = nal_units(&h264).collect();
        assert_eq!(units, [&[0x67, 1, 2][..], &[0x68, 3], &[0x65, 4, 5]]);
        let hevc = [
            0, 0, 1, 0x40, 1, 0, 0, 1, 0x42, 1, 0, 0, 1, 0x44, 1, 0, 0, 0, 1, 0x26, 1, 9,
        ];
        assert!(check_keyframe(Codec::Hevc, &hevc).is_ok());
        assert!(check_keyframe(Codec::Hevc, &hevc[5..]).is_err());
        // Temporal delimiter, sequence header (3 bytes), frame (2 bytes).
        let av1 = [0x12, 0, 0x0a, 3, 1, 2, 3, 0x32, 2, 9, 9];
        assert_eq!(obu_types(&av1), [2, 1, 6]);
        assert!(check_keyframe(Codec::Av1, &av1).is_ok());
        assert!(check_keyframe(Codec::Av1, &av1[..2]).is_err());
    }

    #[test]
    fn the_software_baseline_is_always_offered() {
        let codecs = encodable();
        assert_eq!(codecs.last(), Some(&Codec::H264Baseline));
        // With no NVIDIA driver and no VA-API driver (CI, stuzha: libva and
        // a render node it cannot use) nothing else is.
        if !nvenc::present() && !vaapi::opens() {
            assert_eq!(codecs, [Codec::H264Baseline]);
            assert_eq!(backend(Codec::Hevc), None);
        }
    }
}
