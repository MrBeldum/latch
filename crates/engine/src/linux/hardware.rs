//! Which hardware encoder a Linux host uses, found once per process: NVENC,
//! then VA-API, each tried for real. A codec counts only if its encoder
//! opens, encodes an IDR with its parameter sets in band and then a
//! P-frame; a missing library, device or driver, or a driver that leaves
//! the parameter sets out, is no more than the software encoder taking
//! over.
//!
//! `$LATCH_LINUX_ENCODER` chooses: `auto` (the default), `software`,
//! `nvenc` or `vaapi`. Under `auto` a viewer that asks for Constrained
//! Baseline (one that decodes with `latch-codec`) still gets the software
//! encoder, whose output that decoder is tested against; naming a
//! hardware encoder uses it for Baseline too.

use super::{nvenc, vaapi};
use crate::software::PictureEncoder;
use crate::video::{Codec, VideoConfig};
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

pub fn open(
    backend: Backend,
    codec: Codec,
    width: u32,
    height: u32,
    config: &VideoConfig,
) -> Result<Box<dyn PictureEncoder>> {
    Ok(match backend {
        Backend::Nvenc => Box::new(nvenc::Nvenc::open(codec, width, height, config)?),
        Backend::Vaapi => Box::new(vaapi::Vaapi::open(codec, width, height, config)?),
    })
}

/// Every codec hardware here encodes, with the backend that does.
fn found() -> &'static [(Codec, Backend)] {
    static FOUND: OnceLock<Vec<(Codec, Backend)>> = OnceLock::new();
    FOUND.get_or_init(|| {
        let backends: &[Backend] = match chosen() {
            Preference::Software => &[],
            Preference::Only(Backend::Nvenc) => &[Backend::Nvenc],
            Preference::Only(_) => &[Backend::Vaapi],
            Preference::Auto => &[Backend::Nvenc, Backend::Vaapi],
        };
        let mut found = Vec::new();
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
            for codec in [
                Codec::Av1,
                Codec::Hevc,
                Codec::H264High,
                Codec::H264Baseline,
            ] {
                if found.iter().any(|&(c, _)| c == codec) {
                    continue;
                }
                match probe(backend, codec) {
                    Ok(()) => {
                        tracing::info!("{backend:?} encodes {codec:?}");
                        found.push((codec, backend));
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

/// Codecs whose encoder failed to open for a session after its probe.
static FAILED: std::sync::Mutex<Vec<Codec>> = std::sync::Mutex::new(Vec::new());

/// The hardware encoder for `codec`, if one should be used.
pub fn backend(codec: Codec) -> Option<Backend> {
    if codec == Codec::H264Baseline && chosen() == Preference::Auto {
        return None;
    }
    if FAILED.lock().is_ok_and(|failed| failed.contains(&codec)) {
        return None;
    }
    found().iter().find(|&&(c, _)| c == codec).map(|&(_, b)| b)
}

/// `codec`'s encoder failed for a session: later sessions are not offered it.
pub fn failed(codec: Codec, error: &anyhow::Error) {
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
fn probe(backend: Backend, codec: Codec) -> Result<()> {
    let (width, height) = (640, 360);
    let config = VideoConfig {
        width,
        height,
        fps: 60,
        bitrate_kbps: 4000,
        codec,
    };
    let mut encoder = open(backend, codec, width, height, &config)?;
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
    check_keyframe(codec, &first)
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
