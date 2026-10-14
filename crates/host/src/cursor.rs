//! Keep a visible Windows cursor inside the streamed display, including its
//! shape. A game's transparent, top-left hotspot can otherwise remain on
//! screen while the entire visible cursor is clipped off the bottom/right.

fn cursor_clip(
    desktop: [i32; 4],
    original: [i32; 4],
    shape: (i32, i32, i32, i32),
) -> Option<[i32; 4]> {
    let (width, height, hot_x, hot_y) = shape;
    if width <= 0 || height <= 0 || !(0..width).contains(&hot_x) || !(0..height).contains(&hot_y) {
        return None;
    }
    // ClipCursor's far edges are exclusive. At the last allowed hotspot,
    // the shape's last pixel must still fit inside the desktop.
    let rect = [
        original[0].max(desktop[0] + hot_x),
        original[1].max(desktop[1] + hot_y),
        original[2].min(desktop[2] - width + hot_x + 1),
        original[3].min(desktop[3] - height + hot_y + 1),
    ];
    (rect[0] < rect[2] && rect[1] < rect[3]).then_some(rect)
}

#[cfg(windows)]
mod win {
    use super::cursor_clip;
    use anyhow::{bail, Context, Result};
    use std::mem::size_of;
    use std::os::windows::process::CommandExt;
    use std::process::{Command, Stdio};
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::time::{Duration, Instant};
    use windows::Win32::Foundation::{CloseHandle, RECT};
    use windows::Win32::Graphics::Gdi::{DeleteObject, GetObjectW, BITMAP};
    use windows::Win32::System::Diagnostics::ToolHelp::{
        CreateToolhelp32Snapshot, Process32FirstW, Process32NextW, PROCESSENTRY32W,
        TH32CS_SNAPPROCESS,
    };
    use windows::Win32::UI::HiDpi::{
        SetThreadDpiAwarenessContext, DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2,
    };
    use windows::Win32::UI::WindowsAndMessaging::{
        ClipCursor, GetClipCursor, GetCursorInfo, GetIconInfo, GetSystemMetrics, LoadCursorW,
        CURSORINFO, CURSOR_SHOWING, HICON, ICONINFO, IDC_APPSTARTING, IDC_ARROW, IDC_CROSS,
        IDC_HAND, IDC_HELP, IDC_IBEAM, IDC_NO, IDC_SIZEALL, IDC_SIZENESW, IDC_SIZENS, IDC_SIZENWSE,
        IDC_SIZEWE, IDC_UPARROW, IDC_WAIT, SM_CMONITORS, SM_CXVIRTUALSCREEN, SM_CYVIRTUALSCREEN,
        SM_XVIRTUALSCREEN, SM_YVIRTUALSCREEN,
    };

    const TASK: &str = "LatchCursorUser";

    /// The node can run in session 0. The cursor belongs to the logged-on
    /// desktop, so run the same windowless executable there, without elevation.
    pub fn start() -> Result<()> {
        static REGISTERED: AtomicBool = AtomicBool::new(false);
        let run = |program: &str, args: &[&str]| {
            Command::new(program)
                .args(args)
                .creation_flags(0x0800_0000)
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status()
        };
        if !REGISTERED.load(Ordering::Relaxed) {
            let exe = std::env::current_exe()?;
            let tr = format!(
                "\"{}\" --cursor-guard {}",
                exe.display(),
                std::process::id()
            );
            let status = run(
                "schtasks",
                &[
                    "/Create", "/TN", TASK, "/TR", &tr, "/SC", "ONLOGON", "/IT", "/F", "/RL",
                    "LIMITED",
                ],
            )
            .context("register cursor task")?;
            if !status.success() {
                bail!("could not register {TASK}");
            }
            // Scheduled tasks otherwise stop after 72 hours or on battery.
            // This helper must last as long as the node does.
            let status = run("powershell.exe", &[
                "-NoProfile", "-NonInteractive", "-Command",
                "$ErrorActionPreference='Stop'; $settings=New-ScheduledTaskSettingsSet -AllowStartIfOnBatteries -DontStopIfGoingOnBatteries -ExecutionTimeLimit ([TimeSpan]::Zero); Set-ScheduledTask -TaskName LatchCursorUser -Settings $settings | Out-Null",
            ]).context("configure cursor task")?;
            if !status.success() {
                bail!("could not configure {TASK}");
            }
            REGISTERED.store(true, Ordering::Relaxed);
        }
        // A previous build's helper exits when its node exits. Retrying also
        // starts this build after that handover or after a Windows logon.
        if !run("powershell.exe", &[
            "-NoProfile", "-NonInteractive", "-Command",
            "$ErrorActionPreference='Stop'; if ((Get-ScheduledTask -TaskName LatchCursorUser).State -ne 'Running') { Start-ScheduledTask -TaskName LatchCursorUser }",
        ])?.success() {
            bail!("could not run {TASK}");
        }
        Ok(())
    }

    fn coordinates(r: RECT) -> [i32; 4] {
        [r.left, r.top, r.right, r.bottom]
    }

    fn rectangle(r: [i32; 4]) -> RECT {
        RECT {
            left: r[0],
            top: r[1],
            right: r[2],
            bottom: r[3],
        }
    }

    #[derive(Default)]
    struct Clip {
        original: RECT,
        applied: Option<RECT>,
    }

    impl Clip {
        fn observe(&mut self, current: RECT) {
            if self.applied != Some(current) {
                // A game changed its own confinement. Respect the new bounds.
                self.original = current;
                self.applied = None;
            }
        }

        fn release(&mut self) {
            if let Some(applied) = self.applied.take() {
                let mut current = RECT::default();
                unsafe {
                    if GetClipCursor(&mut current).is_ok() && current == applied {
                        let _ = ClipCursor(Some(&self.original));
                    }
                }
            }
        }
    }

    impl Drop for Clip {
        fn drop(&mut self) {
            self.release();
        }
    }

    fn shape(cursor: HICON) -> Option<(i32, i32, i32, i32)> {
        let mut icon = ICONINFO::default();
        unsafe { GetIconInfo(cursor, &mut icon).ok()? };
        let mut bitmap = BITMAP::default();
        let color = !icon.hbmColor.is_invalid();
        let read = unsafe {
            GetObjectW(
                if color { icon.hbmColor } else { icon.hbmMask },
                size_of::<BITMAP>() as i32,
                Some((&mut bitmap as *mut BITMAP).cast()),
            )
        };
        unsafe {
            if color {
                let _ = DeleteObject(icon.hbmColor);
            }
            if !icon.hbmMask.is_invalid() {
                let _ = DeleteObject(icon.hbmMask);
            }
        }
        (read != 0).then_some((
            bitmap.bmWidth,
            if color {
                bitmap.bmHeight
            } else {
                bitmap.bmHeight / 2
            },
            icon.xHotspot as i32,
            icon.yHotspot as i32,
        ))
    }

    fn streaming() -> bool {
        latch_core::http::request(
            ("127.0.0.1", latch_core::SUNSHINE_PORT),
            "GET",
            "/serverinfo",
            None,
            Duration::from_millis(200),
        )
        .ok()
        .filter(|r| r.status == 200)
        .and_then(|r| crate::streamer::session_in_serverinfo(&r.body))
        .unwrap_or(false)
    }

    fn parent_running(pid: u32) -> Result<bool> {
        // A limited desktop task cannot open an elevated service process,
        // even just to wait for its exit. A process snapshot needs no such
        // access and lets the guard release its clip when that node exits.
        let snapshot = unsafe { CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0) }?;
        let mut entry = PROCESSENTRY32W {
            dwSize: size_of::<PROCESSENTRY32W>() as u32,
            ..Default::default()
        };
        let mut found = false;
        unsafe {
            let mut next = Process32FirstW(snapshot, &mut entry);
            while next.is_ok() {
                if entry.th32ProcessID == pid {
                    found = true;
                    break;
                }
                next = Process32NextW(snapshot, &mut entry);
            }
            let _ = CloseHandle(snapshot);
        }
        Ok(found)
    }

    pub fn run(parent_pid: u32) -> Result<()> {
        let dpi =
            unsafe { SetThreadDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2) };
        if dpi.is_invalid() {
            bail!("could not use physical cursor coordinates");
        }
        tracing::info!("cursor guard for node {parent_pid}");
        let mut clip = Clip::default();
        let mut active = false;
        let mut checked = Instant::now() - Duration::from_secs(1);
        let mut cached = None;
        let mut standard = Vec::new();
        loop {
            std::thread::sleep(Duration::from_millis(16));
            if checked.elapsed() >= Duration::from_secs(1) {
                if !parent_running(parent_pid)? {
                    break;
                }
                active = streaming();
                checked = Instant::now();
                // DPI changes can replace a cursor's bitmap without changing
                // its handle. Refresh the shape along with the display state.
                cached = None;
                // Desktop pointers must still reach scrollbars and controls
                // at the last pixel. Only custom game cursors need this guard.
                standard = [
                    IDC_ARROW,
                    IDC_IBEAM,
                    IDC_WAIT,
                    IDC_CROSS,
                    IDC_UPARROW,
                    IDC_SIZENWSE,
                    IDC_SIZENESW,
                    IDC_SIZEWE,
                    IDC_SIZENS,
                    IDC_SIZEALL,
                    IDC_NO,
                    IDC_HAND,
                    IDC_APPSTARTING,
                    IDC_HELP,
                ]
                .into_iter()
                .filter_map(|id| unsafe { LoadCursorW(None, id).ok() })
                .collect();
            }
            let mut current = RECT::default();
            let mut cursor = CURSORINFO {
                cbSize: size_of::<CURSORINFO>() as u32,
                ..Default::default()
            };
            unsafe {
                if GetClipCursor(&mut current).is_err() {
                    continue;
                }
                clip.observe(current);
                if !active
                    || GetSystemMetrics(SM_CMONITORS) != 1
                    || GetCursorInfo(&mut cursor).is_err()
                    || cursor.flags != CURSOR_SHOWING
                    || standard.contains(&cursor.hCursor)
                {
                    clip.release();
                    cached = None;
                    continue;
                }
                if cached.as_ref().map(|(handle, _)| *handle) != Some(cursor.hCursor) {
                    cached = shape(HICON(cursor.hCursor.0)).map(|s| (cursor.hCursor, s));
                }
                let Some((_, size)) = cached else {
                    clip.release();
                    continue;
                };
                let (x, y) = (
                    GetSystemMetrics(SM_XVIRTUALSCREEN),
                    GetSystemMetrics(SM_YVIRTUALSCREEN),
                );
                let desktop = [
                    x,
                    y,
                    x + GetSystemMetrics(SM_CXVIRTUALSCREEN),
                    y + GetSystemMetrics(SM_CYVIRTUALSCREEN),
                ];
                let Some(bounds) = cursor_clip(desktop, coordinates(clip.original), size) else {
                    clip.release();
                    continue;
                };
                let bounds = rectangle(bounds);
                if bounds != current && ClipCursor(Some(&bounds)).is_ok() {
                    clip.applied = Some(bounds);
                }
            }
        }
        // Restore before leaving the desktop, including node updates.
        clip.release();
        unsafe {
            SetThreadDpiAwarenessContext(dpi);
        }
        tracing::info!("cursor guard released after node {parent_pid} exited");
        Ok(())
    }
}

#[cfg(windows)]
pub use win::{run, start};

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_entire_shape_fits_at_all_four_edges() {
        for desktop in [
            [0, 0, 1920, 1080],
            [-2560, -1440, 0, 0],
            [17, 33, 3041, 1997],
        ] {
            for shape in [(32, 32, 0, 0), (64, 48, 31, 20)] {
                let r = cursor_clip(desktop, desktop, shape).unwrap();
                let (w, h, hx, hy) = shape;
                assert_eq!((r[0] - hx, r[1] - hy), (desktop[0], desktop[1]));
                assert_eq!(
                    (r[2] - 1 - hx + w, r[3] - 1 - hy + h),
                    (desktop[2], desktop[3])
                );
            }
        }
    }

    #[test]
    fn game_bounds_are_preserved_and_invalid_shapes_are_ignored() {
        let desktop = [0, 0, 1920, 1080];
        let game = [400, 300, 401, 301];
        assert_eq!(cursor_clip(desktop, game, (32, 32, 0, 0)), Some(game));
        assert_eq!(cursor_clip(desktop, desktop, (0, 32, 0, 0)), None);
        assert_eq!(cursor_clip(desktop, desktop, (32, 32, 32, 0)), None);
        assert_eq!(
            cursor_clip([0, 0, 16, 16], [0, 0, 16, 16], (32, 32, 0, 0)),
            None
        );
    }
}
