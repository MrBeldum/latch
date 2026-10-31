use crate::display::Previous;
use anyhow::{bail, Context, Result};
use windows::{
    core::PCWSTR,
    Win32::{
        Foundation::{HWND, RECT},
        Graphics::{Dxgi::*, Gdi::*},
    },
};

// Scope awareness to engine operations: the background service may never
// create a winit window, and DXGI/input metrics must still use physical pixels.
#[link(name = "user32")]
extern "system" {
    fn SetThreadDpiAwarenessContext(context: isize) -> isize;
}
pub(super) struct DpiAwareness(isize);
impl DpiAwareness {
    pub fn physical() -> Self {
        // SAFETY: -4 is DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2. The previous
        // thread context is restored when this engine operation finishes.
        Self(unsafe { SetThreadDpiAwarenessContext(-4) })
    }
}
impl Drop for DpiAwareness {
    fn drop(&mut self) {
        if self.0 != 0 {
            // SAFETY: this is the context returned for the same thread above.
            unsafe {
                SetThreadDpiAwarenessContext(self.0);
            }
        }
    }
}

// Previous's shared contract contains dimensions only. Keep the display name
// here so a primary-monitor change cannot make restore resize another output.
static SAVED: parking_lot::Mutex<Option<([u16; 32], Previous)>> = parking_lot::Mutex::new(None);

pub(super) struct Output {
    pub adapter: IDXGIAdapter1,
    pub output: IDXGIOutput,
    pub desc: DXGI_OUTPUT_DESC,
    pub virtual_display: bool,
}
fn text(s: &[u16]) -> String {
    String::from_utf16_lossy(&s[..s.iter().position(|&c| c == 0).unwrap_or(s.len())])
}
fn virtual_name(s: &str) -> bool {
    let s = s.to_ascii_lowercase();
    s.contains("virtual display")
        || s.contains("mttvdd")
        || s.contains("vdd by mtt")
        || s.contains("mtt1337")
}
fn is_virtual(name: &[u16], adapter: &str) -> bool {
    if virtual_name(adapter) {
        return true;
    }
    // SAFETY: The device name is terminated; EnumDisplayDevices fills its sized output.
    unsafe {
        let mut device = DISPLAY_DEVICEW {
            cb: std::mem::size_of::<DISPLAY_DEVICEW>() as u32,
            ..Default::default()
        };
        for i in 0.. {
            if !EnumDisplayDevicesW(PCWSTR(name.as_ptr()), i, &mut device, 0).as_bool() {
                break;
            }
            if virtual_name(&text(&device.DeviceString)) || virtual_name(&text(&device.DeviceID)) {
                return true;
            }
        }
    }
    false
}
fn outputs() -> Result<Vec<Output>> {
    let _dpi = DpiAwareness::physical();
    // SAFETY: The factory and enumerated interfaces have owned COM references.
    unsafe {
        let factory: IDXGIFactory1 = CreateDXGIFactory1().context("create DXGI factory")?;
        let mut outputs = Vec::new();
        for a in 0.. {
            let adapter = match factory.EnumAdapters1(a) {
                Ok(a) => a,
                Err(e) if e.code() == DXGI_ERROR_NOT_FOUND => break,
                Err(e) => return Err(e).context("enumerate adapters"),
            };
            let description = text(&adapter.GetDesc1()?.Description);
            for i in 0.. {
                let output = match adapter.EnumOutputs(i) {
                    Ok(o) => o,
                    Err(e) if e.code() == DXGI_ERROR_NOT_FOUND => break,
                    Err(e) => return Err(e).context("enumerate outputs"),
                };
                let desc = output.GetDesc()?;
                if desc.AttachedToDesktop.as_bool() {
                    let virtual_display = is_virtual(&desc.DeviceName, &description);
                    outputs.push(Output {
                        adapter: adapter.clone(),
                        output,
                        desc,
                        virtual_display,
                    });
                }
            }
        }
        Ok(outputs)
    }
}
pub(super) fn select() -> Result<Output> {
    let mut outputs = outputs()?;
    // Prefer the virtual display for both sizing and capture, even when
    // Windows keeps an uncapturable physical monitor as the primary screen.
    let index = outputs
        .iter()
        .enumerate()
        .min_by_key(|(_, o)| output_rank(o.virtual_display, o.desc.DesktopCoordinates))
        .map(|(i, _)| i)
        .unwrap_or(0);
    if outputs.is_empty() {
        bail!("no DXGI output attached to the desktop");
    }
    Ok(outputs.swap_remove(index))
}

fn output_rank(virtual_display: bool, coordinates: RECT) -> (bool, bool) {
    (
        !virtual_display,
        coordinates.left != 0 || coordinates.top != 0,
    )
}

fn current(output: &Output) -> Result<DEVMODEW> {
    let mut mode = DEVMODEW {
        dmSize: std::mem::size_of::<DEVMODEW>() as u16,
        ..Default::default()
    };
    // SAFETY: Output's name is terminated and mode has its required size.
    if !unsafe {
        EnumDisplaySettingsW(
            PCWSTR(output.desc.DeviceName.as_ptr()),
            ENUM_CURRENT_SETTINGS,
            &mut mode,
        )
    }
    .as_bool()
    {
        bail!("read display mode: {}", windows::core::Error::from_win32());
    }
    Ok(mode)
}
fn change(name: &[u16], mode: &DEVMODEW) -> Result<()> {
    // SAFETY: The device name and DEVMODE remain valid for the call. No registry changes.
    let status = unsafe {
        ChangeDisplaySettingsExW(
            PCWSTR(name.as_ptr()),
            Some(mode),
            HWND::default(),
            CDS_TYPE(0),
            None,
        )
    };
    if status != DISP_CHANGE_SUCCESSFUL {
        bail!("ChangeDisplaySettingsExW returned {}", status.0);
    }
    Ok(())
}
// Prefer the lowest sufficient refresh, otherwise the highest available.
fn refresh_rank(hz: u32, requested: u32) -> (bool, u32) {
    (
        hz < requested,
        if hz >= requested { hz } else { u32::MAX - hz },
    )
}
pub(super) fn prepare(width: u32, height: u32, fps: u32) -> Result<Option<Previous>> {
    let output = select()?;
    if !output.virtual_display {
        return Ok(None);
    }
    let current = current(&output)?;
    let mut best: Option<DEVMODEW> = None;
    for i in 0.. {
        let mut mode = DEVMODEW {
            dmSize: std::mem::size_of::<DEVMODEW>() as u16,
            ..Default::default()
        };
        // SAFETY: Both output buffers and the display name are valid.
        if !unsafe {
            EnumDisplaySettingsW(
                PCWSTR(output.desc.DeviceName.as_ptr()),
                ENUM_DISPLAY_SETTINGS_MODE(i),
                &mut mode,
            )
        }
        .as_bool()
        {
            break;
        }
        if mode.dmPelsWidth == width
            && mode.dmPelsHeight == height
            && best.as_ref().is_none_or(|b| {
                refresh_rank(mode.dmDisplayFrequency, fps) < refresh_rank(b.dmDisplayFrequency, fps)
            })
        {
            best = Some(mode);
        }
    }
    let Some(mut mode) = best else {
        tracing::warn!(
            width,
            height,
            fps,
            "virtual display has no requested size; keeping current mode and scaling capture"
        );
        return Ok(None);
    };
    if mode.dmDisplayFrequency < fps {
        tracing::info!(
            requested_fps = fps,
            refresh = mode.dmDisplayFrequency,
            "using highest available virtual display refresh rate"
        );
    }
    if current.dmPelsWidth == width
        && current.dmPelsHeight == height
        && current.dmDisplayFrequency == mode.dmDisplayFrequency
    {
        return Ok(None);
    }
    mode.dmFields = DM_PELSWIDTH | DM_PELSHEIGHT | DM_DISPLAYFREQUENCY;
    let previous = Previous {
        width: current.dmPelsWidth,
        height: current.dmPelsHeight,
        refresh_hz: current.dmDisplayFrequency,
    };
    let mut saved = SAVED.lock();
    change(&output.desc.DeviceName, &mode)?;
    *saved = Some((output.desc.DeviceName, previous.clone()));
    Ok(Some(previous))
}
pub(super) fn restore(previous: &Previous) -> Result<()> {
    let mut saved = SAVED.lock();
    let name = if let Some((name, _)) = saved.as_ref().filter(|(_, mode)| mode == previous) {
        // A removed virtual adapter's DISPLAYn name can be reused by a physical
        // monitor. Recheck the monitor identity before applying the saved mode.
        if !outputs()?
            .iter()
            .any(|o| o.desc.DeviceName == *name && o.virtual_display)
        {
            return Ok(());
        }
        *name
    } else {
        let output = select()?;
        if !output.virtual_display {
            return Ok(());
        }
        output.desc.DeviceName
    };
    let mode = DEVMODEW {
        dmSize: std::mem::size_of::<DEVMODEW>() as u16,
        dmFields: DM_PELSWIDTH | DM_PELSHEIGHT | DM_DISPLAYFREQUENCY,
        dmPelsWidth: previous.width,
        dmPelsHeight: previous.height,
        dmDisplayFrequency: previous.refresh_hz,
        ..Default::default()
    };
    change(&name, &mode)?;
    *saved = None;
    Ok(())
}
#[cfg(test)]
mod tests {
    #[test]
    fn virtual_secondary_is_preferred_to_physical_primary() {
        use windows::Win32::Foundation::RECT;
        let primary = RECT::default();
        let secondary = RECT {
            left: 2560,
            ..Default::default()
        };
        let select = |outputs: &[(bool, RECT)]| {
            outputs
                .iter()
                .enumerate()
                .min_by_key(|(_, (virtual_display, rect))| {
                    super::output_rank(*virtual_display, *rect)
                })
                .map(|(i, _)| i)
        };
        assert_eq!(select(&[(false, primary), (true, secondary)]), Some(1));
        assert_eq!(select(&[(true, secondary), (false, primary)]), Some(0));
        assert_eq!(select(&[(false, secondary), (false, primary)]), Some(1));
        assert_eq!(select(&[(true, secondary), (true, primary)]), Some(1));
        assert_eq!(select(&[(false, secondary)]), Some(0));
        assert_eq!(select(&[]), None);
    }

    #[test]
    fn refresh_falls_back_without_refusing_high_fps() {
        for (requested, expected) in [(30, 60), (60, 60), (90, 120), (144, 120), (240, 120)] {
            assert_eq!(
                [60, 120]
                    .into_iter()
                    .min_by_key(|hz| super::refresh_rank(*hz, requested)),
                Some(expected)
            );
        }
    }
    #[test]
    fn recognises_virtual_display_driver_names() {
        for name in [
            "Virtual Display Driver",
            "MttVDD",
            "VDD by MTT",
            r"MONITOR\MTT1337",
        ] {
            assert!(super::virtual_name(name));
        }
        assert!(!super::virtual_name("NVIDIA GeForce RTX 4070"));
    }
}
