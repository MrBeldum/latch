//! Desktop sizing and a conservative ceiling for software encoding.

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

/// Small CPUs keep room for the desktop by limiting software encoding at 1080p.
pub fn max_fps(encoder: &str, cores: usize) -> u32 {
    if encoder.to_ascii_lowercase().contains("software") && cores < 4 {
        30
    } else {
        0
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn software_on_a_small_cpu_has_a_ceiling() {
        assert_eq!(super::max_fps("Latch software H.264", 2), 30);
        assert_eq!(super::max_fps("Latch software H.264", 4), 0);
        assert_eq!(super::max_fps("VideoToolbox HEVC", 2), 0);
    }
}
