//! Read the current display mode, never a list of supported (inactive) modes.
#[cfg(target_os = "macos")]
pub fn hz() -> Option<u32> {
    crate::macos::display_hz()
}

#[cfg(windows)]
pub fn hz() -> Option<u32> {
    use windows_sys::Win32::Graphics::Gdi::*;
    // The foreground window is ours when initiating a connection.
    let monitor = unsafe {
        MonitorFromWindow(
            windows_sys::Win32::UI::WindowsAndMessaging::GetForegroundWindow(),
            MONITOR_DEFAULTTOPRIMARY,
        )
    };
    let mut info: MONITORINFOEXW = unsafe { std::mem::zeroed() };
    info.monitorInfo.cbSize = std::mem::size_of::<MONITORINFOEXW>() as u32;
    let mut mode: DEVMODEW = unsafe { std::mem::zeroed() };
    mode.dmSize = std::mem::size_of::<DEVMODEW>() as u16;
    let ok = unsafe {
        GetMonitorInfoW(monitor, &mut info.monitorInfo) != 0
            && EnumDisplaySettingsW(info.szDevice.as_ptr(), ENUM_CURRENT_SETTINGS, &mut mode) != 0
    };
    (ok && mode.dmDisplayFrequency > 1).then_some(mode.dmDisplayFrequency)
}

#[cfg(target_os = "linux")]
pub fn hz() -> Option<u32> {
    use x11rb::{connection::Connection, protocol::randr::ConnectionExt};
    // Xwayland describes a virtual output, not the compositor's real mode.
    if std::env::var_os("WAYLAND_DISPLAY").is_some() {
        return None;
    }
    // Connection is initiated by a click on this display. Query in the worker,
    // never in update(): X11 replies can block.
    let (conn, screen) = x11rb::connect(None).ok()?;
    let root = conn.setup().roots.get(screen)?.root;
    let resources = conn
        .randr_get_screen_resources_current(root)
        .ok()?
        .reply()
        .ok()?;
    use x11rb::protocol::xproto::ConnectionExt as _;
    let pointer = conn.query_pointer(root).ok()?.reply().ok()?;
    let crtc = resources.crtcs.iter().find_map(|id| {
        let c = conn
            .randr_get_crtc_info(*id, resources.config_timestamp)
            .ok()?
            .reply()
            .ok()?;
        let x = i32::from(pointer.root_x) - i32::from(c.x);
        let y = i32::from(pointer.root_y) - i32::from(c.y);
        (c.mode != 0 && x >= 0 && y >= 0 && x < i32::from(c.width) && y < i32::from(c.height))
            .then_some(c)
    })?;
    let mode = resources.modes.iter().find(|m| m.id == crtc.mode)?;
    let pixels = u64::from(mode.htotal) * u64::from(mode.vtotal);
    (pixels > 0)
        .then(|| (u64::from(mode.dot_clock) / pixels) as u32)
        .filter(|hz| *hz > 0)
}
