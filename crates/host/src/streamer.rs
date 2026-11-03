//! Desktop sizing and what this host's encoders can do.

use latch_core::api::CodecCaps;
use latch_engine::software::Ceiling;
use latch_engine::video::{Codec, Limits};
use parking_lot::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

#[cfg(target_os = "linux")]
pub fn capture_size() -> Option<[u32; 2]> {
    if display_follows_viewer() {
        return None;
    }
    latch_engine::linux::capture_size().ok()
}

#[cfg(not(target_os = "linux"))]
pub fn capture_size() -> Option<[u32; 2]> {
    None
}

/// The desktop resizes itself to each viewer (`LATCH_DISPLAY_FOLLOWS_VIEWER=1`):
/// the engine's prep command sets the size a viewer asks for, and its undo
/// puts the idle size back when the session ends.
pub fn display_follows_viewer() -> bool {
    std::env::var_os("LATCH_DISPLAY_FOLLOWS_VIEWER").is_some_and(|v| v == "1")
}

/// The likely encoder before the first session opens its pipeline.
pub fn encoder_description() -> &'static str {
    if cfg!(target_os = "macos") {
        "VideoToolbox HEVC / H.264"
    } else if cfg!(windows) {
        "Media Foundation H.264, with Latch software H.264 fallback"
    } else {
        "Latch software H.264"
    }
}

/// What this host's encoders take, as measured: the software path's frame
/// rate when it is the one in use (at the capture size, or 1920 × 1080),
/// and each codec's limits. Looked at again on a thread when the encoder
/// or the capture size changes, never while `streaming`. Until the first
/// look ends the software path is held to [`Ceiling::PROVISIONAL`] and
/// codecs say nothing (the engine still fits every request to them).
/// What is reported here is also what the engine holds a session to, so a
/// viewer that connects without asking, or before the look ends, gets no
/// more than one that waited.
pub fn capacity(encoder: &str, streaming: bool) -> (u32, Vec<CodecCaps>) {
    static MEASURED: Mutex<Option<Measured>> = Mutex::new(None);
    static LOOKING: AtomicBool = AtomicBool::new(false);
    let software = is_software(encoder);
    let size = capture_size().unwrap_or([1920, 1080]);
    let measured = MEASURED.lock().clone();
    let stale = measured
        .as_ref()
        .is_none_or(|m| m.size != size || m.software != software);
    if stale && !streaming && !LOOKING.swap(true, Ordering::AcqRel) {
        let spawned = std::thread::Builder::new()
            .name("latch-capacity".into())
            .spawn(move || {
                let codecs = latch_engine::video::encodable()
                    .into_iter()
                    .map(|codec| caps(codec, latch_engine::video::limits(codec)))
                    .collect();
                let ceiling = if software {
                    let fps = latch_engine::software::measure_fps(size[0], size[1], MEASURE_FOR);
                    Some(Ceiling {
                        width: size[0],
                        height: size[1],
                        fps,
                    })
                    .filter(|c| c.fps > 0)
                } else {
                    None
                };
                tracing::info!(
                    "encoder capacity: {ceiling:?} in software (None: not measured), {codecs:?}"
                );
                if software {
                    latch_engine::software::set_ceiling(Some(
                        ceiling.unwrap_or(Ceiling::PROVISIONAL),
                    ));
                }
                *MEASURED.lock() = Some(Measured {
                    size,
                    software,
                    ceiling,
                    codecs,
                });
                LOOKING.store(false, Ordering::Release);
            });
        if spawned.is_err() {
            LOOKING.store(false, Ordering::Release);
        }
    }
    // The latest measurement still holds at another size: it is pixels a
    // second. One from the hardware encoder's time says nothing about
    // software, and the reverse.
    let ceiling = software.then(|| {
        measured
            .as_ref()
            .filter(|m| m.software)
            .and_then(|m| m.ceiling)
            .unwrap_or(Ceiling::PROVISIONAL)
    });
    latch_engine::software::set_ceiling(ceiling);
    let max_fps = ceiling.map_or(0, |c| c.fps_at(size[0], size[1]).max(1));
    (max_fps, measured.map(|m| m.codecs).unwrap_or_default())
}

/// How long the software path is timed for.
const MEASURE_FOR: Duration = Duration::from_millis(1500);

#[derive(Debug, Clone)]
struct Measured {
    size: [u32; 2],
    software: bool,
    ceiling: Option<Ceiling>,
    codecs: Vec<CodecCaps>,
}

/// The engine's software H.264, as the running or last session named it
/// or as [`encoder_description`] expects it.
fn is_software(encoder: &str) -> bool {
    encoder.starts_with("Latch software")
}

fn caps(codec: Codec, limits: Limits) -> CodecCaps {
    CodecCaps {
        codec: serde_json::to_value(codec)
            .ok()
            .and_then(|v| v.as_str().map(str::to_string))
            .unwrap_or_default(),
        max_side: limits.max_side,
        max_pixels: limits.max_pixels,
        max_pixel_rate: limits.max_pixel_rate,
        max_kbps: limits.max_kbps,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_the_software_encoder_is_measured() {
        assert!(is_software("Latch software H.264"));
        assert!(is_software("Latch software H.264 (X11)"));
        assert!(!is_software(
            "Media Foundation H.264, with Latch software H.264 fallback"
        ));
        assert!(!is_software("VideoToolbox HEVC"));
    }

    /// Before anything is measured a software host reports, and the engine
    /// holds a session to, the provisional ceiling; a hardware one neither.
    #[test]
    fn software_is_held_to_a_provisional_ceiling_until_measured() {
        let size = capture_size().unwrap_or([1920, 1080]);
        // `streaming` keeps the measurement from starting.
        let (fps, _) = capacity("Latch software H.264", true);
        assert_eq!(fps, Ceiling::PROVISIONAL.fps_at(size[0], size[1]).max(1));
        assert_eq!(
            latch_engine::software::ceiling(),
            Some(Ceiling::PROVISIONAL)
        );
        assert_eq!(capacity("VideoToolbox HEVC", true).0, 0);
        assert_eq!(latch_engine::software::ceiling(), None);
    }

    #[test]
    fn codec_caps_carry_the_wire_name() {
        let c = caps(Codec::H264High, Limits::level(Codec::H264High));
        assert_eq!(c.codec, "h264-high");
        assert_eq!(c.max_kbps, 1_000_000);
        assert_eq!(caps(Codec::Av1, Limits::level(Codec::Av1)).codec, "av1");
    }
}
