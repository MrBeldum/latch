//! Desktop sizing and what this host's encoders can do.

use latch_core::api::CodecCaps;
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
/// or the capture size changes, never while `streaming`; until the first
/// look ends nothing is known and nothing is limited.
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
                let max_fps = if software {
                    latch_engine::software::measure_fps(size[0], size[1], MEASURE_FOR)
                } else {
                    0
                };
                tracing::info!("encoder capacity: {max_fps} fps in software at {size:?} (0: not measured), {codecs:?}");
                *MEASURED.lock() = Some(Measured {
                    size,
                    software,
                    max_fps,
                    codecs,
                });
                LOOKING.store(false, Ordering::Release);
            });
        if spawned.is_err() {
            LOOKING.store(false, Ordering::Release);
        }
    }
    measured.map_or((0, Vec::new()), |m| (m.max_fps, m.codecs))
}

/// How long the software path is timed for.
const MEASURE_FOR: Duration = Duration::from_millis(1500);

#[derive(Debug, Clone)]
struct Measured {
    size: [u32; 2],
    software: bool,
    max_fps: u32,
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

    #[test]
    fn codec_caps_carry_the_wire_name() {
        let c = caps(Codec::H264High, Limits::level(Codec::H264High));
        assert_eq!(c.codec, "h264-high");
        assert_eq!(c.max_kbps, 1_000_000);
        assert_eq!(caps(Codec::Av1, Limits::level(Codec::Av1)).codec, "av1");
    }
}
