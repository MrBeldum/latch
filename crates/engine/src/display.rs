//! The desktop's size, for a host whose desktop follows its viewer (a
//! headless Linux X server, Windows' virtual display): set it to the
//! viewer's size for a session and put it back afterwards.

/// What [`restore`] needs.
#[derive(Debug, Clone, PartialEq)]
pub struct Previous {
    pub width: u32,
    pub height: u32,
    pub refresh_hz: u32,
}

/// Sets the captured display to `width × height` at `fps` Hz or the
/// nearest rate it offers. `Ok(None)` when this host does not resize (a
/// Mac, a PC with a real monitor) or already has that size.
pub fn prepare(width: u32, height: u32, fps: u32) -> anyhow::Result<Option<Previous>> {
    #[cfg(target_os = "linux")]
    return crate::linux::prepare_display(width, height, fps);
    #[cfg(windows)]
    return crate::windows::prepare_display(width, height, fps);
    #[cfg(target_os = "macos")]
    {
        let _ = (width, height, fps);
        Ok(None)
    }
}

pub fn restore(previous: &Previous) -> anyhow::Result<()> {
    #[cfg(target_os = "linux")]
    return crate::linux::restore_display(previous);
    #[cfg(windows)]
    return crate::windows::restore_display(previous);
    #[cfg(target_os = "macos")]
    {
        let _ = previous;
        Ok(())
    }
}
