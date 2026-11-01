use crate::display::Previous;
use anyhow::{bail, Context, Result};
use windows::{
    core::PCWSTR,
    Win32::{
        Devices::Display::*,
        Foundation::{ERROR_INSUFFICIENT_BUFFER, HWND},
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

// Previous's shared contract contains dimensions only. Keep the full topology
// here so disconnect restores paths by adapter/target identity, not DISPLAYn.
struct Saved {
    previous: Previous,
    paths: Vec<DISPLAYCONFIG_PATH_INFO>,
    modes: Vec<DISPLAYCONFIG_MODE_INFO>,
}
static SAVED: parking_lot::Mutex<Option<Saved>> = parking_lot::Mutex::new(None);
fn topology() -> Result<(Vec<DISPLAYCONFIG_PATH_INFO>, Vec<DISPLAYCONFIG_MODE_INFO>)> {
    for _ in 0..4 {
        let (mut np, mut nm) = (0, 0);
        // SAFETY: Sized output arrays remain live through the query.
        unsafe {
            GetDisplayConfigBufferSizes(QDC_ONLY_ACTIVE_PATHS, &mut np, &mut nm).ok()?;
            let mut paths = vec![DISPLAYCONFIG_PATH_INFO::default(); np as usize];
            let mut modes = vec![DISPLAYCONFIG_MODE_INFO::default(); nm as usize];
            let rc = QueryDisplayConfig(
                QDC_ONLY_ACTIVE_PATHS,
                &mut np,
                paths.as_mut_ptr(),
                &mut nm,
                modes.as_mut_ptr(),
                None,
            );
            if rc == ERROR_INSUFFICIENT_BUFFER {
                continue;
            }
            rc.ok()?;
            paths.truncate(np as usize);
            modes.truncate(nm as usize);
            return Ok((paths, modes));
        }
    }
    bail!("display topology kept changing")
}
fn apply_topology(
    paths: &[DISPLAYCONFIG_PATH_INFO],
    modes: &[DISPLAYCONFIG_MODE_INFO],
) -> Result<()> {
    // SAFETY: Paths and their mode indices came from QueryDisplayConfig.
    let rc = unsafe {
        SetDisplayConfig(
            Some(paths),
            Some(modes),
            SDC_APPLY | SDC_USE_SUPPLIED_DISPLAY_CONFIG | SDC_ALLOW_CHANGES,
        )
    };
    if rc != 0 {
        bail!("SetDisplayConfig returned {rc}");
    }
    Ok(())
}
fn only_virtual(
    name: &[u16; 32],
    paths: &[DISPLAYCONFIG_PATH_INFO],
    modes: &[DISPLAYCONFIG_MODE_INFO],
) -> Result<()> {
    for path in paths {
        let mut source = DISPLAYCONFIG_SOURCE_DEVICE_NAME {
            header: DISPLAYCONFIG_DEVICE_INFO_HEADER {
                r#type: DISPLAYCONFIG_DEVICE_INFO_GET_SOURCE_NAME,
                size: std::mem::size_of::<DISPLAYCONFIG_SOURCE_DEVICE_NAME>() as u32,
                adapterId: path.sourceInfo.adapterId,
                id: path.sourceInfo.id,
            },
            ..Default::default()
        };
        // SAFETY: Sized device-info packet and validated source mode index.
        unsafe {
            if DisplayConfigGetDeviceInfo(&mut source.header) != 0
                || source.viewGdiDeviceName != *name
            {
                continue;
            }
            let mut modes = modes.to_vec();
            let index = path.sourceInfo.Anonymous.modeInfoIdx as usize;
            let mode = modes
                .get_mut(index)
                .context("virtual display has no source mode")?;
            anyhow::ensure!(
                mode.infoType == DISPLAYCONFIG_MODE_INFO_TYPE_SOURCE,
                "invalid display source mode"
            );
            mode.Anonymous.sourceMode.position = Default::default();
            return apply_topology(&[*path], &modes);
        }
    }
    bail!("virtual display is missing from active display topology")
}

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
pub(super) fn sdr_white_level(name: &[u16; 32]) -> f32 {
    let Ok((paths, _)) = topology() else {
        return 1.0;
    };
    for path in paths {
        let mut source = DISPLAYCONFIG_SOURCE_DEVICE_NAME {
            header: DISPLAYCONFIG_DEVICE_INFO_HEADER {
                r#type: DISPLAYCONFIG_DEVICE_INFO_GET_SOURCE_NAME,
                size: std::mem::size_of::<DISPLAYCONFIG_SOURCE_DEVICE_NAME>() as u32,
                adapterId: path.sourceInfo.adapterId,
                id: path.sourceInfo.id,
            },
            ..Default::default()
        };
        let mut level = DISPLAYCONFIG_SDR_WHITE_LEVEL {
            header: DISPLAYCONFIG_DEVICE_INFO_HEADER {
                r#type: DISPLAYCONFIG_DEVICE_INFO_GET_SDR_WHITE_LEVEL,
                size: std::mem::size_of::<DISPLAYCONFIG_SDR_WHITE_LEVEL>() as u32,
                adapterId: path.targetInfo.adapterId,
                id: path.targetInfo.id,
            },
            ..Default::default()
        };
        // SAFETY: Both device-info packets have the SDK size and matching IDs.
        unsafe {
            if DisplayConfigGetDeviceInfo(&mut source.header) == 0
                && source.viewGdiDeviceName == *name
                && DisplayConfigGetDeviceInfo(&mut level.header) == 0
                && level.SDRWhiteLevel > 0
            {
                return level.SDRWhiteLevel as f32 / 1000.0;
            }
        }
    }
    1.0
}
fn selection_rank(virtual_display: bool, primary: bool) -> (bool, bool) {
    (!virtual_display, !primary)
}
pub(super) fn select() -> Result<Output> {
    let mut outputs = outputs()?;
    let index = outputs
        .iter()
        .enumerate()
        .min_by_key(|(_, o)| {
            selection_rank(
                o.virtual_display,
                o.desc.DesktopCoordinates.left == 0 && o.desc.DesktopCoordinates.top == 0,
            )
        })
        .map_or(0, |(i, _)| i);
    if outputs.is_empty() {
        bail!("no DXGI output attached to the desktop");
    }
    Ok(outputs.swap_remove(index))
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
    let mut mode = best.unwrap_or_else(|| {
        tracing::warn!(
            width,
            height,
            fps,
            "virtual display has no requested size; keeping current mode and scaling capture"
        );
        current
    });
    if mode.dmDisplayFrequency < fps {
        tracing::info!(
            requested_fps = fps,
            refresh = mode.dmDisplayFrequency,
            "using highest available virtual display refresh rate"
        );
    }
    mode.dmFields = DM_PELSWIDTH | DM_PELSHEIGHT | DM_DISPLAYFREQUENCY;
    let previous = Previous {
        width: current.dmPelsWidth,
        height: current.dmPelsHeight,
        refresh_hz: current.dmDisplayFrequency,
    };
    let mut saved = SAVED.lock();
    anyhow::ensure!(saved.is_none(), "display topology already prepared");
    let (paths, modes) = topology()?;
    only_virtual(&output.desc.DeviceName, &paths, &modes)?;
    if let Err(e) = change(&output.desc.DeviceName, &mode) {
        apply_topology(&paths, &modes).context("restore topology after mode failure")?;
        return Err(e);
    }
    *saved = Some(Saved {
        previous: previous.clone(),
        paths,
        modes,
    });
    Ok(Some(previous))
}
pub(super) fn restore(previous: &Previous) -> Result<()> {
    let mut saved = SAVED.lock();
    if let Some(state) = saved.take_if(|state| &state.previous == previous) {
        // The snapshot includes the original modes and adapter/target identities.
        // Restore them atomically rather than resizing an intermediate topology.
        // It is taken either way: a snapshot that no longer applies (an
        // adapter went away mid-stream) left behind would refuse every
        // later session with "display topology already prepared".
        match apply_topology(&state.paths, &state.modes) {
            Ok(()) => return Ok(()),
            Err(e) => tracing::warn!(
                "restore the saved display topology: {e:#}; setting the size back instead"
            ),
        }
    }
    let output = select()?;
    if !output.virtual_display {
        return Ok(());
    }
    let name = output.desc.DeviceName;
    let mode = DEVMODEW {
        dmSize: std::mem::size_of::<DEVMODEW>() as u16,
        dmFields: DM_PELSWIDTH | DM_PELSHEIGHT | DM_DISPLAYFREQUENCY,
        dmPelsWidth: previous.width,
        dmPelsHeight: previous.height,
        dmDisplayFrequency: previous.refresh_hz,
        ..Default::default()
    };
    change(&name, &mode)?;
    Ok(())
}
#[cfg(test)]
mod tests {
    use super::selection_rank;
    #[test]
    fn virtual_output_wins_over_primary() {
        let candidates = [(false, true), (true, false)];
        assert_eq!(
            candidates
                .iter()
                .min_by_key(|(v, p)| selection_rank(*v, *p)),
            Some(&(true, false))
        );
        assert!(selection_rank(false, true) < selection_rank(false, false));
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
