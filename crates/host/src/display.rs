//! Display diagnostics for a PC whose stream cannot show its own settings.
//!
//! A black stream has several causes that look identical from the Mac: no
//! monitor attached, a monitor that is powered down, a locked session, or a
//! capture that Sunshine cannot read. The probe answers all of them at once.
//! It reads the desktop's own brightness as a handful of sampled pixels —
//! numbers, never an image — so a black picture can be blamed on the PC's
//! desktop or on Sunshine's capture without anyone looking at the screen.

use anyhow::Result;
use serde_json::Value;

/// This is a fixed read-only probe, not a remote command runner.
pub fn probe() -> Result<Value> {
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        let output = std::process::Command::new("powershell.exe")
            .args(["-NoProfile", "-NonInteractive", "-Command", PROBE])
            .creation_flags(0x0800_0000)
            .output()?;
        anyhow::ensure!(
            output.status.success(),
            "Windows display probe failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        let text = String::from_utf8_lossy(&output.stdout);
        Ok(serde_json::from_str(
            text.trim_start_matches('\u{feff}').trim(),
        )?)
    }
    #[cfg(not(windows))]
    {
        Ok(serde_json::json!({"supported": false}))
    }
}

/// Windows' "advanced colour" — HDR — for every display path, and the
/// power to turn it off.
///
/// A PC whose monitor is gone keeps composing in the colour mode that
/// monitor asked for — half-float, ten bits a channel — while the
/// placeholder display left behind reports no luminance at all. A capture
/// converting that to an ordinary picture has nothing to describe the
/// brightness with, and every frame comes out black even though the PC's
/// own desktop draws normally.
///
/// Where the mode is one the display supports, turning it off restores an
/// eight-bit desktop that captures normally. Where Windows is enforcing it
/// on a placeholder display, it refuses every switch with
/// ERROR_NOT_SUPPORTED: the mode is a consequence of having no display,
/// not a setting, and only attaching one clears it.
pub fn advanced_color() -> Result<Value> {
    #[cfg(windows)]
    {
        win::report()
    }
    #[cfg(not(windows))]
    {
        Ok(serde_json::json!({"supported": false}))
    }
}

/// Turn advanced colour on or off wherever the display supports it, and
/// report what the displays say afterwards. Reversible, and never touches
/// a display that is already as asked.
pub fn set_advanced_color(on: bool) -> Result<Value> {
    #[cfg(windows)]
    {
        win::set(on)?;
        win::report()
    }
    #[cfg(not(windows))]
    {
        let _ = on;
        anyhow::bail!("only a Windows PC has advanced colour to turn off")
    }
}

#[cfg(windows)]
mod win {
    use anyhow::{bail, ensure, Result};
    use serde_json::Value;
    use std::mem::size_of;
    use windows::Win32::Devices::Display::{
        DisplayConfigGetDeviceInfo, DisplayConfigSetDeviceInfo, GetDisplayConfigBufferSizes,
        QueryDisplayConfig, DISPLAYCONFIG_DEVICE_INFO_GET_ADVANCED_COLOR_INFO,
        DISPLAYCONFIG_DEVICE_INFO_GET_SDR_WHITE_LEVEL, DISPLAYCONFIG_DEVICE_INFO_GET_SOURCE_NAME,
        DISPLAYCONFIG_DEVICE_INFO_HEADER, DISPLAYCONFIG_DEVICE_INFO_SET_ADVANCED_COLOR_STATE,
        DISPLAYCONFIG_DEVICE_INFO_TYPE, DISPLAYCONFIG_GET_ADVANCED_COLOR_INFO,
        DISPLAYCONFIG_MODE_INFO, DISPLAYCONFIG_PATH_INFO, DISPLAYCONFIG_SDR_WHITE_LEVEL,
        DISPLAYCONFIG_SOURCE_DEVICE_NAME, QDC_ALL_PATHS, QDC_ONLY_ACTIVE_PATHS,
    };

    /// Windows 11 replaced the single "advanced colour" switch with two —
    /// HDR and wide colour — and a mode that says which is active. The
    /// bindings in use predate them, so the three packets are declared
    /// here; each is the standard header and one bitfield, and Windows
    /// checks `size` against the type, so an older Windows answers
    /// ERROR_INVALID_PARAMETER rather than acting on the wrong packet.
    /// See wingdi.h: GET_ADVANCED_COLOR_INFO_2 = 15, SET_HDR_STATE = 16,
    /// SET_WCG_STATE = 17.
    const GET_ADVANCED_COLOR_INFO_2: DISPLAYCONFIG_DEVICE_INFO_TYPE =
        DISPLAYCONFIG_DEVICE_INFO_TYPE(15);
    const SET_HDR_STATE: DISPLAYCONFIG_DEVICE_INFO_TYPE = DISPLAYCONFIG_DEVICE_INFO_TYPE(16);
    const SET_WCG_STATE: DISPLAYCONFIG_DEVICE_INFO_TYPE = DISPLAYCONFIG_DEVICE_INFO_TYPE(17);

    #[repr(C)]
    #[derive(Clone, Copy, Default)]
    struct AdvancedColorInfo2 {
        header: DISPLAYCONFIG_DEVICE_INFO_HEADER,
        value: u32,
        color_encoding: u32,
        bits_per_color_channel: u32,
        active_color_mode: u32,
    }

    /// The payload of both setters: one bit, then padding.
    #[repr(C)]
    #[derive(Clone, Copy, Default)]
    struct SetColorState {
        header: DISPLAYCONFIG_DEVICE_INFO_HEADER,
        value: u32,
    }

    // DISPLAYCONFIG_GET_ADVANCED_COLOR_INFO_2's bits.
    const ACTIVE_2: u32 = 1 << 1;
    const LIMITED_BY_POLICY_2: u32 = 1 << 3;
    const HDR_SUPPORTED_2: u32 = 1 << 4;
    const HDR_USER_ENABLED_2: u32 = 1 << 5;
    const WIDE_SUPPORTED_2: u32 = 1 << 6;
    const WIDE_USER_ENABLED_2: u32 = 1 << 7;

    /// 0 SDR, 1 wide colour, 2 HDR.
    fn mode_name(mode: u32) -> &'static str {
        match mode {
            0 => "SDR",
            1 => "wide colour",
            2 => "HDR",
            _ => "unknown",
        }
    }

    const SUPPORTED: u32 = 1 << 0;
    const ENABLED: u32 = 1 << 1;
    const WIDE_COLOR_ENFORCED: u32 = 1 << 2;
    const FORCE_DISABLED: u32 = 1 << 3;

    /// What Windows is driving, or — when a PC has lost its monitor and
    /// drives nothing — every path it still knows about.
    fn paths() -> Result<Vec<DISPLAYCONFIG_PATH_INFO>> {
        let mut last = None;
        for flags in [QDC_ONLY_ACTIVE_PATHS, QDC_ALL_PATHS] {
            let (mut n_paths, mut n_modes) = (0u32, 0u32);
            let rc = unsafe { GetDisplayConfigBufferSizes(flags, &mut n_paths, &mut n_modes) };
            if rc.0 != 0 {
                last = Some(format!("display paths could not be counted ({})", rc.0));
                continue;
            }
            if n_paths == 0 {
                last = Some("Windows is driving no display".into());
                continue;
            }
            let mut ps = vec![DISPLAYCONFIG_PATH_INFO::default(); n_paths as usize];
            let mut ms = vec![DISPLAYCONFIG_MODE_INFO::default(); n_modes as usize];
            let rc = unsafe {
                QueryDisplayConfig(
                    flags,
                    &mut n_paths,
                    ps.as_mut_ptr(),
                    &mut n_modes,
                    ms.as_mut_ptr(),
                    None,
                )
            };
            if rc.0 != 0 {
                last = Some(format!("display paths could not be read ({})", rc.0));
                continue;
            }
            ps.truncate(n_paths as usize);
            if !ps.is_empty() {
                return Ok(ps);
            }
        }
        bail!(last.unwrap_or_else(|| "Windows lists no display".into()))
    }

    /// `\\.\DISPLAY1`, to line the state up with the screens Windows lists.
    fn source_name(path: &DISPLAYCONFIG_PATH_INFO) -> Option<String> {
        let mut name = DISPLAYCONFIG_SOURCE_DEVICE_NAME {
            header: DISPLAYCONFIG_DEVICE_INFO_HEADER {
                r#type: DISPLAYCONFIG_DEVICE_INFO_GET_SOURCE_NAME,
                size: size_of::<DISPLAYCONFIG_SOURCE_DEVICE_NAME>() as u32,
                adapterId: path.sourceInfo.adapterId,
                id: path.sourceInfo.id,
            },
            ..Default::default()
        };
        if unsafe { DisplayConfigGetDeviceInfo(&mut name.header) } != 0 {
            return None;
        }
        let end = name
            .viewGdiDeviceName
            .iter()
            .position(|&c| c == 0)
            .unwrap_or(name.viewGdiDeviceName.len());
        Some(String::from_utf16_lossy(&name.viewGdiDeviceName[..end]))
    }

    fn color_info(path: &DISPLAYCONFIG_PATH_INFO) -> Result<DISPLAYCONFIG_GET_ADVANCED_COLOR_INFO> {
        let mut info = DISPLAYCONFIG_GET_ADVANCED_COLOR_INFO {
            header: DISPLAYCONFIG_DEVICE_INFO_HEADER {
                r#type: DISPLAYCONFIG_DEVICE_INFO_GET_ADVANCED_COLOR_INFO,
                size: size_of::<DISPLAYCONFIG_GET_ADVANCED_COLOR_INFO>() as u32,
                adapterId: path.targetInfo.adapterId,
                id: path.targetInfo.id,
            },
            ..Default::default()
        };
        let rc = unsafe { DisplayConfigGetDeviceInfo(&mut info.header) };
        ensure!(rc == 0, "advanced colour state unavailable ({rc})");
        Ok(info)
    }

    /// The Windows 11 view, when this Windows has one.
    fn color_info_2(path: &DISPLAYCONFIG_PATH_INFO) -> Result<AdvancedColorInfo2> {
        let mut info = AdvancedColorInfo2 {
            header: DISPLAYCONFIG_DEVICE_INFO_HEADER {
                r#type: GET_ADVANCED_COLOR_INFO_2,
                size: size_of::<AdvancedColorInfo2>() as u32,
                adapterId: path.targetInfo.adapterId,
                id: path.targetInfo.id,
            },
            ..Default::default()
        };
        let rc = unsafe { DisplayConfigGetDeviceInfo(&mut info.header) };
        ensure!(rc == 0, "this Windows has no colour mode to report ({rc})");
        Ok(info)
    }

    /// Ask one display to leave, or take up, a colour mode.
    fn write_state(
        path: &DISPLAYCONFIG_PATH_INFO,
        kind: DISPLAYCONFIG_DEVICE_INFO_TYPE,
        on: bool,
    ) -> i32 {
        let packet = SetColorState {
            header: DISPLAYCONFIG_DEVICE_INFO_HEADER {
                r#type: kind,
                size: size_of::<SetColorState>() as u32,
                adapterId: path.targetInfo.adapterId,
                id: path.targetInfo.id,
            },
            value: u32::from(on),
        };
        unsafe { DisplayConfigSetDeviceInfo(&packet.header) }
    }

    /// How bright the display says plain white is, in thousandths of the
    /// usual 80 nits: the scale an HDR or wide-colour desktop is converted
    /// down by. Worth reading next to the colour mode, because a normal
    /// white level rules this out as the reason a capture is black.
    fn sdr_white_level(path: &DISPLAYCONFIG_PATH_INFO) -> Option<u32> {
        let mut level = DISPLAYCONFIG_SDR_WHITE_LEVEL {
            header: DISPLAYCONFIG_DEVICE_INFO_HEADER {
                r#type: DISPLAYCONFIG_DEVICE_INFO_GET_SDR_WHITE_LEVEL,
                size: size_of::<DISPLAYCONFIG_SDR_WHITE_LEVEL>() as u32,
                adapterId: path.targetInfo.adapterId,
                id: path.targetInfo.id,
            },
            ..Default::default()
        };
        (unsafe { DisplayConfigGetDeviceInfo(&mut level.header) } == 0)
            .then_some(level.SDRWhiteLevel)
    }

    pub fn report() -> Result<Value> {
        let mut displays = Vec::new();
        for path in paths()? {
            let name = source_name(&path);
            let mut entry = match color_info(&path) {
                Ok(info) => {
                    let bits = unsafe { info.Anonymous.value };
                    serde_json::json!({
                        "display": name,
                        "supported": bits & SUPPORTED != 0,
                        "enabled": bits & ENABLED != 0,
                        "wide_color_enforced": bits & WIDE_COLOR_ENFORCED != 0,
                        "force_disabled": bits & FORCE_DISABLED != 0,
                        "bits_per_color": info.bitsPerColorChannel,
                        "color_encoding": info.colorEncoding.0,
                    })
                }
                Err(e) => serde_json::json!({"display": name, "error": e.to_string()}),
            };
            // Windows 11 says which of the two modes is actually running,
            // which is what decides the switch that can turn it off.
            if let Some(level) = sdr_white_level(&path) {
                entry["sdr_white_level"] = level.into();
            }
            if let Ok(two) = color_info_2(&path) {
                entry["mode"] = mode_name(two.active_color_mode).into();
                entry["active"] = (two.value & ACTIVE_2 != 0).into();
                entry["hdr_supported"] = (two.value & HDR_SUPPORTED_2 != 0).into();
                entry["hdr_on"] = (two.value & HDR_USER_ENABLED_2 != 0).into();
                entry["wide_color_supported"] = (two.value & WIDE_SUPPORTED_2 != 0).into();
                entry["wide_color_on"] = (two.value & WIDE_USER_ENABLED_2 != 0).into();
                entry["limited_by_policy"] = (two.value & LIMITED_BY_POLICY_2 != 0).into();
            }
            displays.push(entry);
        }
        Ok(serde_json::json!({"supported": true, "displays": displays}))
    }

    pub fn set(on: bool) -> Result<()> {
        let mut refused = Vec::new();
        let mut touched = 0;
        for path in paths()? {
            let Ok(info) = color_info(&path) else {
                continue;
            };
            let bits = unsafe { info.Anonymous.value };
            // A display whose monitor is gone reports advanced colour as
            // unsupported while Windows carries on composing in it — the
            // very state worth undoing. Only what it is now decides.
            if (bits & ENABLED != 0) == on {
                continue;
            }
            // Windows 11 refuses the old single switch on a display that
            // does not claim to support HDR, even while it composes in
            // wide colour; its own two switches are the way out. Try the
            // one the display says is on, then the other, then the old
            // one, and stop at the first Windows accepts.
            let two = color_info_2(&path).ok();
            let mut order: Vec<DISPLAYCONFIG_DEVICE_INFO_TYPE> = Vec::new();
            if let Some(two) = two {
                if two.value & WIDE_USER_ENABLED_2 != 0 || two.active_color_mode == 1 {
                    order.push(SET_WCG_STATE);
                }
                if two.value & HDR_USER_ENABLED_2 != 0 || two.active_color_mode == 2 {
                    order.push(SET_HDR_STATE);
                }
                order.push(SET_WCG_STATE);
                order.push(SET_HDR_STATE);
            }
            order.push(DISPLAYCONFIG_DEVICE_INFO_SET_ADVANCED_COLOR_STATE);
            let mut tried: Vec<i32> = Vec::new();
            order.retain(|k| {
                tried.iter().all(|&t| t != k.0) && {
                    tried.push(k.0);
                    true
                }
            });

            let mut codes = Vec::new();
            let mut done = false;
            for kind in order {
                let rc = write_state(&path, kind, on);
                codes.push(format!("{}→{rc}", kind.0));
                if rc == 0 {
                    // Believe the display, not the return code.
                    let still_on = color_info(&path)
                        .map(|i| unsafe { i.Anonymous.value } & ENABLED != 0)
                        .unwrap_or(!on);
                    if still_on == on {
                        done = true;
                        break;
                    }
                }
            }
            if done {
                touched += 1;
            } else {
                refused.push(format!(
                    "{}: Windows refused every switch ({})",
                    source_name(&path).unwrap_or_else(|| "a display".into()),
                    codes.join(", ")
                ));
            }
        }
        if touched == 0 && !refused.is_empty() {
            bail!(refused.join("; "));
        }
        Ok(())
    }
}

#[cfg(all(test, windows))]
mod tests {
    #[test]
    fn windows_probe_reports_displays_without_modifying_them() {
        let status = super::probe().expect("Windows display probe");
        assert_eq!(status["supported"], true);
        assert!(status["elevated"].is_boolean());
        assert!(status["locked"].is_boolean());
        assert!(status["screens"].is_array());
        assert!(status["monitors"].is_array());
        assert!(status["monitor_ids"].is_array());
        assert!(status["desktop_monitors"].is_array());
        assert!(status["gpus"].is_array());
        assert!(status["sunshine"].is_array());
        assert!(status["sunshine_service"].is_array());
        // Every section reports its own failure rather than losing the rest.
        assert!(status["errors"].is_object());
        let desktop = &status["desktop"];
        assert!(
            desktop.is_null() || desktop["max"].is_number(),
            "desktop brightness: {desktop}"
        );
    }

    /// Reading the state must never change it: a runner with no display
    /// says so, and one with a display answers for each of them.
    #[test]
    fn advanced_colour_is_reported_per_display() {
        let Ok(state) = super::advanced_color() else {
            return;
        };
        assert_eq!(state["supported"], true);
        let displays = state["displays"].as_array().expect("displays");
        for d in displays {
            if d.get("error").is_some() {
                continue;
            }
            assert!(d["supported"].is_boolean(), "{d}");
            assert!(d["enabled"].is_boolean(), "{d}");
            assert!(d["bits_per_color"].is_number(), "{d}");
        }
    }
}

/// Each section stands on its own: a class that is missing on this Windows
/// build leaves one error behind instead of losing the whole report.
// Compiled into tests everywhere, so the service names it looks for are
// checked against setup's on a Mac too.
#[cfg(any(windows, test))]
const PROBE: &str = r#"
$ErrorActionPreference = 'Stop'
[Console]::OutputEncoding = New-Object System.Text.UTF8Encoding($false)
Add-Type -AssemblyName System.Windows.Forms
Add-Type -AssemblyName System.Drawing
$errors = @{}

$me = [Security.Principal.WindowsIdentity]::GetCurrent()
$principal = New-Object Security.Principal.WindowsPrincipal($me)

$locked = $false
try { $locked = @(Get-Process -Name LogonUI -ErrorAction SilentlyContinue).Count -gt 0 }
catch { $errors['locked'] = $_.Exception.Message }

$screens = @()
try {
    $screens = @([Windows.Forms.Screen]::AllScreens | ForEach-Object {
        @{name=$_.DeviceName; primary=$_.Primary; width=$_.Bounds.Width;
          height=$_.Bounds.Height; left=$_.Bounds.X; top=$_.Bounds.Y; bpp=$_.BitsPerPixel}
    })
} catch { $errors['screens'] = $_.Exception.Message }

$monitors = @()
try {
    $monitors = @(Get-PnpDevice -Class Monitor -ErrorAction Stop |
        Select-Object FriendlyName,Status,Present,InstanceId)
} catch { $errors['monitors'] = $_.Exception.Message }

# Nothing here means Windows sees no monitor hardware at all: the desktop
# is then a phantom that Desktop Duplication captures as black.
$monitorIds = @()
try {
    $monitorIds = @(Get-CimInstance -Namespace root\wmi -ClassName WmiMonitorID -ErrorAction Stop |
        ForEach-Object {
            @{instance=$_.InstanceName; active=$_.Active;
              name=(-join ($_.UserFriendlyName | Where-Object {$_ -gt 0} | ForEach-Object {[char]$_}))}
        })
} catch { $errors['monitor_ids'] = $_.Exception.Message }

# Availability 3 is powered on; 8 means the monitor is off.
$desktopMonitors = @()
try {
    $desktopMonitors = @(Get-CimInstance Win32_DesktopMonitor -ErrorAction Stop |
        Select-Object Name,Availability,ScreenWidth,ScreenHeight,Status,PNPDeviceID)
} catch { $errors['desktop_monitors'] = $_.Exception.Message }

$connections = @()
try {
    $connections = @(Get-CimInstance -Namespace root\wmi -ClassName WmiMonitorConnectionParams -ErrorAction Stop |
        Select-Object InstanceName,VideoOutputTechnology,Active)
} catch { $errors['connections'] = $_.Exception.Message }

$gpus = @()
try {
    $gpus = @(Get-CimInstance Win32_VideoController -ErrorAction Stop |
        Select-Object Name,DriverVersion,CurrentHorizontalResolution,CurrentVerticalResolution,
                      CurrentRefreshRate,CurrentBitsPerPixel,VideoModeDescription,Status,Availability)
} catch { $errors['gpus'] = $_.Exception.Message }

$sunshine = @()
try {
    $sunshine = @(Get-CimInstance Win32_Process -Filter "Name='sunshine.exe'" -ErrorAction Stop |
        Select-Object ProcessId,SessionId,ExecutablePath)
} catch { $errors['sunshine'] = $_.Exception.Message }

# The engine's service under every name it has had (Latch's, the one
# before 4.1, upstream's) and any other whose program is the engine's
# service wrapper. PathName shows which engine folder each one runs.
$service = @()
try {
    $service = @(Get-CimInstance Win32_Service -Filter "Name='LatchStream' OR Name='BroLinkStream' OR Name like '%unshine%' OR PathName like '%sunshinesvc.exe%'" -ErrorAction Stop |
        Select-Object Name,State,StartMode,ProcessId,StartName,PathName)
} catch { $errors['sunshine_service'] = $_.Exception.Message }

# The desktop's own brightness, as sampled numbers rather than a picture:
# black here means the PC has nothing to show, not that capture failed.
$desktop = $null
try {
    $s = [Windows.Forms.Screen]::PrimaryScreen
    if ($s) {
        $w = $s.Bounds.Width
        $h = $s.Bounds.Height
        $bmp = New-Object Drawing.Bitmap($w, $h)
        $g = [Drawing.Graphics]::FromImage($bmp)
        $g.CopyFromScreen($s.Bounds.X, $s.Bounds.Y, 0, 0, (New-Object Drawing.Size($w, $h)))
        $g.Dispose()
        $cols = 40
        $rows = 25
        $max = 0
        $sum = 0
        $lit = 0
        for ($row = 0; $row -lt $rows; $row++) {
            for ($col = 0; $col -lt $cols; $col++) {
                $px = [int](($col + 0.5) * $w / $cols)
                $py = [int](($row + 0.5) * $h / $rows)
                $p = $bmp.GetPixel($px, $py)
                $v = [int]$p.R
                if ([int]$p.G -gt $v) { $v = [int]$p.G }
                if ([int]$p.B -gt $v) { $v = [int]$p.B }
                $sum += $v
                if ($v -gt $max) { $max = $v }
                if ($v -gt 16) { $lit++ }
            }
        }
        $bmp.Dispose()
        $desktop = @{width=$w; height=$h; samples=($cols*$rows); max=$max;
                     mean=[math]::Round($sum / ($cols*$rows), 1); lit=$lit}
    }
} catch { $errors['desktop'] = $_.Exception.Message }

@{supported=$true;
  elevated=$principal.IsInRole([Security.Principal.WindowsBuiltInRole]::Administrator);
  session=[Diagnostics.Process]::GetCurrentProcess().SessionId;
  user=$me.Name;
  locked=$locked;
  screens=$screens;
  monitors=$monitors;
  monitor_ids=$monitorIds;
  desktop_monitors=$desktopMonitors;
  connections=$connections;
  gpus=$gpus;
  sunshine=$sunshine;
  sunshine_service=$service;
  desktop=$desktop;
  errors=$errors} | ConvertTo-Json -Depth 6 -Compress
"#;

#[cfg(test)]
mod probe_tests {
    #[test]
    fn the_probe_finds_the_engine_service_under_every_name_latch_gave_it() {
        for name in [crate::setup::SERVICE, crate::legacy::WINDOWS_SERVICE] {
            assert!(
                super::PROBE.contains(&format!("Name='{name}'")),
                "{name} is not looked for"
            );
        }
        assert!(super::PROBE.contains("Name like '%unshine%'"));
        assert!(super::PROBE.contains("PathName like '%sunshinesvc.exe%'"));
    }
}
