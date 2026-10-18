//! Keep a visible Windows cursor in the streamed picture. A game's cursor
//! can have a transparent hotspot with every visible pixel to its right or
//! below it: at the bottom or right edge the hotspot is still on screen,
//! but the whole visible cursor is off it. The guard stops such a cursor
//! where its nearest visible pixel is still in the picture. A cursor whose
//! hotspot is on its visible part, like every standard pointer and most
//! game cursors, is never held back from an edge, so edge scrolling and
//! controls in the last pixels keep working.

/// A cursor's hotspot and the box around the pixels it draws (left, top,
/// right, bottom, all inclusive), in the cursor's own pixels.
#[derive(Debug, Clone, Copy, PartialEq)]
struct Shape {
    hot: (i32, i32),
    visible: [i32; 4],
}

/// The confinement that keeps `shape`'s nearest visible pixel on `desktop`,
/// within the one already in force (`original`). `None` when the cursor
/// needs none: its hotspot is in line with visible pixels both ways, so
/// some of it shows wherever the hotspot can go.
fn cursor_clip(desktop: [i32; 4], original: [i32; 4], shape: Shape) -> Option<[i32; 4]> {
    let (hot_x, hot_y) = shape.hot;
    let [x0, y0, x1, y1] = shape.visible;
    // How far the hotspot is from the visible pixels nearest to it, per side.
    let left = (hot_x - x1).max(0);
    let top = (hot_y - y1).max(0);
    let right = (x0 - hot_x).max(0);
    let bottom = (y0 - hot_y).max(0);
    if left == 0 && top == 0 && right == 0 && bottom == 0 {
        return None;
    }
    // ClipCursor's far edges are exclusive. At the last allowed hotspot,
    // the nearest visible pixel is on the desktop's last pixel.
    let rect = [
        original[0].max(desktop[0] + left),
        original[1].max(desktop[1] + top),
        original[2].min(desktop[2] - right),
        original[3].min(desktop[3] - bottom),
    ];
    (rect[0] < rect[2] && rect[1] < rect[3]).then_some(rect)
}

/// One of a cursor's bitmaps as `GetBitmapBits` copies it: `height` rows
/// of `stride` bytes, top row first.
struct Bitmap<'a> {
    width: i32,
    height: i32,
    stride: usize,
    bits_per_pixel: u16,
    data: &'a [u8],
}

impl Bitmap<'_> {
    /// Whether the bitmap holds `rows` rows of `width` pixels.
    fn holds(&self, width: i32, rows: i32) -> bool {
        let (Ok(width), Ok(rows)) = (usize::try_from(width), usize::try_from(rows)) else {
            return false;
        };
        self.stride * 8 >= width * usize::from(self.bits_per_pixel)
            && self.data.len() >= self.stride * rows
    }

    /// A monochrome pixel: set or clear.
    fn bit(&self, x: i32, y: i32) -> bool {
        let at = y as usize * self.stride + x as usize / 8;
        self.data
            .get(at)
            .is_some_and(|b| b & (0x80 >> (x % 8)) != 0)
    }

    /// A 32-bit pixel: blue, green, red, alpha.
    fn pixel(&self, x: i32, y: i32) -> [u8; 4] {
        let at = y as usize * self.stride + x as usize * 4;
        self.data
            .get(at..at + 4)
            .map_or([0; 4], |p| [p[0], p[1], p[2], p[3]])
    }
}

/// The box around the pixels a cursor draws: left, top, right, bottom, all
/// inclusive. A colour cursor with an alpha channel draws where alpha is
/// not 0; one without draws where its AND mask is clear or its colour
/// inverts the screen. A monochrome cursor's mask is its AND mask above its
/// XOR mask; it draws where AND is clear or XOR is set. `None` when no
/// pixel shows, or for a format this does not read.
fn visible_box(color: Option<&Bitmap>, mask: &Bitmap) -> Option<[i32; 4]> {
    let (width, height) = match color {
        Some(c) if c.bits_per_pixel == 32 => (c.width, c.height),
        Some(_) => return None,
        None => (mask.width, mask.height / 2),
    };
    let readable = mask.bits_per_pixel == 1
        && mask.holds(width, if color.is_some() { height } else { 2 * height })
        && color.is_none_or(|c| c.holds(width, height));
    if !readable || width <= 0 || height <= 0 {
        return None;
    }
    let alpha =
        color.is_some_and(|c| (0..height).any(|y| (0..width).any(|x| c.pixel(x, y)[3] != 0)));
    let shows = |x: i32, y: i32| match color {
        Some(c) if alpha => c.pixel(x, y)[3] != 0,
        Some(c) => !mask.bit(x, y) || c.pixel(x, y)[..3] != [0, 0, 0],
        None => !mask.bit(x, y) || mask.bit(x, y + height),
    };
    let mut found: Option<[i32; 4]> = None;
    for y in 0..height {
        for x in (0..width).filter(|&x| shows(x, y)) {
            let b = found.get_or_insert([x, y, x, y]);
            *b = [b[0].min(x), b[1].min(y), b[2].max(x), b[3].max(y)];
        }
    }
    found
}

/// Whether the engine's log says a viewer is connected: its last word on
/// one is CLIENT CONNECTED, not CLIENT DISCONNECTED or a ping timeout
/// (which ends a session without the former). The engine keeps the
/// Desktop app running after a viewer leaves, so a running app alone does
/// not say a stream is on. `None` when the log says neither.
fn viewer_connected(log: &str) -> Option<bool> {
    crate::streamer::viewer_connected_in(log)
}

/// What the guard reads and sets on Windows; a fake in the tests.
trait Desktop {
    /// The virtual screen: left, top, right, bottom, far edges exclusive.
    fn bounds(&self) -> [i32; 4];
    /// The confinement in force: the virtual screen when there is none.
    fn clip(&self) -> Option<[i32; 4]>;
    /// Confine the cursor to a rectangle, or free it with `None`.
    fn set_clip(&mut self, rect: Option<[i32; 4]>) -> bool;
    /// The custom cursor showing on a single display, if any.
    fn custom_cursor(&mut self) -> Option<Shape>;
}

/// The guard's record of the cursor's confinement, so it only ever lifts
/// its own.
#[derive(Debug, Default)]
struct Clip {
    /// The confinement someone else set (a game, usually); `None` for none.
    original: Option<[i32; 4]>,
    /// The rectangle the guard set, while it is in force.
    applied: Option<[i32; 4]>,
}

impl Clip {
    /// One look at the cursor; `active` while a viewer is connected.
    fn step(&mut self, desktop: &mut impl Desktop, active: bool) {
        let Some(current) = desktop.clip() else {
            return;
        };
        let screen = desktop.bounds();
        let shape = if active {
            desktop.custom_cursor()
        } else {
            None
        };
        self.observe(
            current,
            screen,
            shape.and_then(|s| cursor_clip(screen, screen, s)),
        );
        match shape.and_then(|s| cursor_clip(screen, self.original.unwrap_or(screen), s)) {
            Some(bounds) if bounds == current => {}
            Some(bounds) => {
                if desktop.set_clip(Some(bounds)) {
                    self.applied = Some(bounds);
                }
            }
            None => self.release(desktop),
        }
    }

    /// Take note of the confinement in force. `ours` is what the guard
    /// would set for this cursor on a free desktop.
    fn observe(&mut self, current: [i32; 4], screen: [i32; 4], ours: Option<[i32; 4]>) {
        if self.applied == Some(current) {
            return;
        }
        if current != screen && ours == Some(current) {
            // A guard that was ended abruptly left its clip behind. It is
            // this one's to lift, not a game's to respect.
            self.original = None;
            self.applied = Some(current);
        } else {
            // A game changed its own confinement, or Windows reset it after
            // a display change. Respect the new bounds.
            self.original = (current != screen).then_some(current);
            self.applied = None;
        }
    }

    /// Put back what the guard's clip replaced, unless someone has replaced
    /// it since. No confinement before means none now, whatever size the
    /// desktop has become.
    fn release(&mut self, desktop: &mut impl Desktop) {
        if let Some(applied) = self.applied.take() {
            if desktop.clip() == Some(applied) {
                desktop.set_clip(self.original);
            }
        }
    }
}

#[cfg(windows)]
mod win {
    use super::{visible_box, Bitmap, Clip, Desktop, Shape};
    use anyhow::{bail, Context, Result};
    use std::mem::size_of;
    use std::os::windows::process::CommandExt;
    use std::process::{Child, Command, Stdio};
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::Mutex;
    use std::time::{Duration, Instant};
    use windows::Win32::Foundation::{CloseHandle, RECT};
    use windows::Win32::Graphics::Gdi::{DeleteObject, GetBitmapBits, GetObjectW, BITMAP, HBITMAP};
    use windows::Win32::System::Diagnostics::ToolHelp::{
        CreateToolhelp32Snapshot, Process32FirstW, Process32NextW, PROCESSENTRY32W,
        TH32CS_SNAPPROCESS,
    };
    use windows::Win32::UI::HiDpi::{
        SetThreadDpiAwarenessContext, DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2,
    };
    use windows::Win32::UI::WindowsAndMessaging::{
        ClipCursor, GetClipCursor, GetCursorInfo, GetIconInfo, GetSystemMetrics, LoadCursorW,
        CURSORINFO, CURSOR_SHOWING, HCURSOR, HICON, ICONINFO, IDC_APPSTARTING, IDC_ARROW,
        IDC_CROSS, IDC_HAND, IDC_HELP, IDC_IBEAM, IDC_NO, IDC_SIZEALL, IDC_SIZENESW, IDC_SIZENS,
        IDC_SIZENWSE, IDC_SIZEWE, IDC_UPARROW, IDC_WAIT, SM_CMONITORS, SM_CXVIRTUALSCREEN,
        SM_CYVIRTUALSCREEN, SM_XVIRTUALSCREEN, SM_YVIRTUALSCREEN,
    };

    const TASK: &str = "LatchCursorUser";
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;

    fn session_id() -> u32 {
        let mut id = 0u32;
        let _ = unsafe {
            windows::Win32::System::RemoteDesktop::ProcessIdToSessionId(std::process::id(), &mut id)
        };
        id
    }

    /// Keep the guard running beside this node; the service calls this
    /// every 30 seconds. The cursor belongs to the logged-on desktop. A node
    /// started there (from the Run key, as usual) runs the guard as its own
    /// windowless child. One in session 0 (started over SSH) hands it to a
    /// task that runs in the desktop session, without elevation.
    pub fn start() -> Result<()> {
        static CHILD: Mutex<Option<Child>> = Mutex::new(None);
        if session_id() == 0 {
            return start_task();
        }
        let mut child = CHILD.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(running) = child.as_mut() {
            if running.try_wait()?.is_none() {
                return Ok(());
            }
        }
        let spawned = Command::new(std::env::current_exe()?)
            .arg("--cursor-guard")
            .arg(std::process::id().to_string())
            .creation_flags(CREATE_NO_WINDOW)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .context("start cursor guard")?;
        *child = Some(spawned);
        Ok(())
    }

    /// The guard from session 0: a task registered once per node, with
    /// this node's process ID. It has no trigger of its own, so it never
    /// starts for a node that is gone; the node runs it, and running a task
    /// that is already running leaves it alone.
    fn start_task() -> Result<()> {
        static REGISTERED: AtomicBool = AtomicBool::new(false);
        let run = |program: &str, args: &[&str]| {
            Command::new(program)
                .args(args)
                .creation_flags(CREATE_NO_WINDOW)
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status()
        };
        // One attempt per node: a failure here would fail the same way
        // every 30 seconds.
        if !REGISTERED.swap(true, Ordering::Relaxed) {
            let exe = std::env::current_exe()?;
            let tr = format!(
                "\"{}\" --cursor-guard {}",
                exe.display(),
                std::process::id()
            );
            let status = run(
                "schtasks",
                &[
                    "/Create", "/TN", TASK, "/TR", &tr, "/SC", "ONCE", "/ST", "00:00", "/IT", "/F",
                    "/RL", "LIMITED",
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
        }
        // A previous build's helper exits when its node exits. Retrying also
        // starts this build after that handover or after a Windows logon.
        if !run("schtasks", &["/Run", "/TN", TASK])?.success() {
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

    /// A bitmap's description and a copy of its pixels.
    fn bits(bitmap: HBITMAP) -> Option<(BITMAP, Vec<u8>)> {
        let mut info = BITMAP::default();
        let read = unsafe {
            GetObjectW(
                bitmap,
                size_of::<BITMAP>() as i32,
                Some((&mut info as *mut BITMAP).cast()),
            )
        };
        if read == 0 {
            return None;
        }
        let size = i32::try_from(i64::from(info.bmWidthBytes) * i64::from(info.bmHeight)).ok()?;
        let mut data = vec![0u8; usize::try_from(size).ok()?];
        let copied = unsafe { GetBitmapBits(bitmap, size, data.as_mut_ptr().cast()) };
        (copied > 0).then_some((info, data))
    }

    fn bitmap<'a>(info: &BITMAP, data: &'a [u8]) -> Bitmap<'a> {
        Bitmap {
            width: info.bmWidth,
            height: info.bmHeight,
            stride: usize::try_from(info.bmWidthBytes).unwrap_or(0),
            bits_per_pixel: info.bmBitsPixel,
            data,
        }
    }

    fn shape(cursor: HICON) -> Option<Shape> {
        let mut icon = ICONINFO::default();
        unsafe { GetIconInfo(cursor, &mut icon).ok()? };
        let has_color = !icon.hbmColor.is_invalid();
        let color = if has_color { bits(icon.hbmColor) } else { None };
        let mask = bits(icon.hbmMask);
        unsafe {
            if has_color {
                let _ = DeleteObject(icon.hbmColor);
            }
            if !icon.hbmMask.is_invalid() {
                let _ = DeleteObject(icon.hbmMask);
            }
        }
        let (mask_info, mask_data) = mask?;
        let visible = match (has_color, &color) {
            (true, Some((info, data))) => {
                visible_box(Some(&bitmap(info, data)), &bitmap(&mask_info, &mask_data))?
            }
            (true, None) => return None,
            (false, _) => visible_box(None, &bitmap(&mask_info, &mask_data))?,
        };
        Some(Shape {
            hot: (icon.xHotspot as i32, icon.yHotspot as i32),
            visible,
        })
    }

    /// The desktop as the guard sees it.
    #[derive(Default)]
    struct Screen {
        /// Windows' own pointers, which must still reach scrollbars and
        /// controls at the last pixel.
        standard: Vec<HCURSOR>,
        /// The last cursor looked at, and its shape.
        cached: Option<(HCURSOR, Option<Shape>)>,
    }

    impl Screen {
        fn refresh(&mut self) {
            // DPI changes can replace a cursor's bitmap without changing
            // its handle. Refresh the shape along with the display state.
            self.cached = None;
            self.standard = [
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
    }

    impl Desktop for Screen {
        fn bounds(&self) -> [i32; 4] {
            unsafe {
                let (x, y) = (
                    GetSystemMetrics(SM_XVIRTUALSCREEN),
                    GetSystemMetrics(SM_YVIRTUALSCREEN),
                );
                [
                    x,
                    y,
                    x + GetSystemMetrics(SM_CXVIRTUALSCREEN),
                    y + GetSystemMetrics(SM_CYVIRTUALSCREEN),
                ]
            }
        }

        fn clip(&self) -> Option<[i32; 4]> {
            let mut current = RECT::default();
            unsafe { GetClipCursor(&mut current) }.ok()?;
            Some(coordinates(current))
        }

        fn set_clip(&mut self, rect: Option<[i32; 4]>) -> bool {
            let rect = rect.map(rectangle);
            unsafe { ClipCursor(rect.as_ref().map(|r| r as *const RECT)) }.is_ok()
        }

        fn custom_cursor(&mut self) -> Option<Shape> {
            let mut cursor = CURSORINFO {
                cbSize: size_of::<CURSORINFO>() as u32,
                ..Default::default()
            };
            unsafe {
                if GetSystemMetrics(SM_CMONITORS) != 1 || GetCursorInfo(&mut cursor).is_err() {
                    return None;
                }
            }
            if cursor.flags != CURSOR_SHOWING || self.standard.contains(&cursor.hCursor) {
                return None;
            }
            if self.cached.as_ref().map(|(handle, _)| *handle) != Some(cursor.hCursor) {
                let found = shape(HICON(cursor.hCursor.0));
                self.cached = Some((cursor.hCursor, found));
            }
            self.cached.as_ref().and_then(|(_, found)| *found)
        }
    }

    /// Whether a viewer is watching: the engine runs an app and its log
    /// says a viewer is connected. A log that cannot be read leaves the
    /// running app to decide.
    fn streaming() -> bool {
        let running = latch_core::http::request(
            ("127.0.0.1", latch_core::SUNSHINE_PORT),
            "GET",
            "/serverinfo",
            None,
            Duration::from_millis(200),
        )
        .ok()
        .filter(|r| r.status == 200)
        .and_then(|r| crate::streamer::session_in_serverinfo(&r.body))
        .unwrap_or(false);
        running
            && crate::streamer::find()
                .and_then(|install| crate::streamer::log_text(&install))
                .and_then(|log| super::viewer_connected(&log))
                .unwrap_or(true)
    }

    /// This program's file name, as process snapshots list it.
    fn own_name() -> Option<String> {
        let exe = std::env::current_exe().ok()?;
        Some(exe.file_name()?.to_string_lossy().to_lowercase())
    }

    fn parent_running(pid: u32, name: Option<&str>) -> Result<bool> {
        // A limited desktop task cannot open an elevated service process,
        // even just to wait for its exit. A process snapshot needs no such
        // access and lets the guard release its clip when that node exits.
        // The name check keeps a reused process ID from passing for the node.
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
                    let len = entry
                        .szExeFile
                        .iter()
                        .position(|&c| c == 0)
                        .unwrap_or(entry.szExeFile.len());
                    let exe = String::from_utf16_lossy(&entry.szExeFile[..len]).to_lowercase();
                    found = name.is_none_or(|name| exe == name);
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
        let name = own_name();
        let mut screen = Screen::default();
        let mut clip = Clip::default();
        let mut active = false;
        let mut checked: Option<Instant> = None;
        loop {
            if checked.is_none_or(|at| at.elapsed() >= Duration::from_secs(1)) {
                if !parent_running(parent_pid, name.as_deref()).unwrap_or(false) {
                    break;
                }
                active = streaming();
                checked = Some(Instant::now());
                screen.refresh();
            }
            clip.step(&mut screen, active);
            // With no viewer there is nothing to follow but the next check.
            std::thread::sleep(Duration::from_millis(if active { 16 } else { 1000 }));
        }
        // Restore before leaving the desktop, including node updates.
        clip.release(&mut screen);
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

    /// The 1080p desktop most tests use.
    const HD: [i32; 4] = [0, 0, 1920, 1080];

    fn shape(hot: (i32, i32), visible: [i32; 4]) -> Shape {
        Shape { hot, visible }
    }

    #[test]
    fn a_cursor_drawn_at_its_hotspot_reaches_every_edge() {
        // A 64x64 game cursor with its tip at the hotspot, and one with a
        // centred hotspot (a crosshair): edge scrolling and the last
        // pixels' buttons stay reachable.
        assert_eq!(cursor_clip(HD, HD, shape((0, 0), [0, 0, 40, 60])), None);
        assert_eq!(cursor_clip(HD, HD, shape((31, 31), [0, 0, 63, 63])), None);
        // In line with visible pixels both ways, though not on one itself.
        assert_eq!(cursor_clip(HD, HD, shape((10, 10), [0, 0, 20, 20])), None);
    }

    #[test]
    fn the_nearest_visible_pixel_stays_on_screen_at_every_edge() {
        for desktop in [HD, [-2560, -1440, 0, 0], [17, 33, 3041, 1997]] {
            // Transparent top-left hotspot, visible pixels from (12, 9).
            let s = shape((0, 0), [12, 9, 40, 30]);
            let r = cursor_clip(desktop, desktop, s).unwrap();
            assert_eq!((r[0], r[1]), (desktop[0], desktop[1]));
            // At the last allowed hotspot, the first visible column and row
            // are on the desktop's last pixel.
            assert_eq!(
                (r[2] - 1 + 12, r[3] - 1 + 9),
                (desktop[2] - 1, desktop[3] - 1)
            );
            // Visible pixels above and left of the hotspot hold it back
            // from the top and left edges instead.
            let s = shape((50, 40), [0, 0, 20, 30]);
            let r = cursor_clip(desktop, desktop, s).unwrap();
            assert_eq!((r[0] - 50 + 20, r[1] - 40 + 30), (desktop[0], desktop[1]));
            assert_eq!((r[2], r[3]), (desktop[2], desktop[3]));
        }
    }

    #[test]
    fn game_bounds_are_preserved_and_impossible_bounds_are_dropped() {
        let game = [400, 300, 401, 301];
        let offset = shape((0, 0), [8, 8, 31, 31]);
        assert_eq!(cursor_clip(HD, game, offset), Some(game));
        assert_eq!(
            cursor_clip(
                [0, 0, 16, 16],
                [0, 0, 16, 16],
                shape((0, 0), [20, 0, 31, 31])
            ),
            None
        );
    }

    #[test]
    fn the_visible_box_is_read_from_every_cursor_format() {
        // Monochrome, 8x2 (stride 2): AND above XOR. Row 0 shows an inverted
        // pixel at x 6 (AND set, XOR set); row 1 a black pixel at x 2.
        let data = [0xFF, 0, 0xDF, 0, 0x02, 0, 0, 0];
        let mask = Bitmap {
            width: 8,
            height: 4,
            stride: 2,
            bits_per_pixel: 1,
            data: &data,
        };
        assert_eq!(visible_box(None, &mask), Some([2, 0, 6, 1]));

        // 32-bit with alpha: only pixels with alpha count.
        let mut pixels = vec![0u8; 4 * 4 * 4];
        pixels[(4 + 1) * 4 + 3] = 255; // (1, 1)
        pixels[(3 * 4 + 2) * 4 + 3] = 10; // (2, 3)
        let color = Bitmap {
            width: 4,
            height: 4,
            stride: 16,
            bits_per_pixel: 32,
            data: &pixels,
        };
        let and = [0u8, 0, 0, 0, 0, 0, 0, 0];
        let mask = Bitmap {
            width: 4,
            height: 4,
            stride: 2,
            bits_per_pixel: 1,
            data: &and,
        };
        assert_eq!(visible_box(Some(&color), &mask), Some([1, 1, 2, 3]));

        // 32-bit without alpha: AND clear draws, AND set with a colour inverts.
        let mut pixels = vec![0u8; 4 * 4 * 4];
        pixels[(2 * 4 + 3) * 4] = 0x80; // (3, 2): blue, over a set AND bit
        let color = Bitmap {
            width: 4,
            height: 4,
            stride: 16,
            bits_per_pixel: 32,
            data: &pixels,
        };
        // Every AND bit set but (1, 0).
        let and = [0xBF, 0, 0xFF, 0, 0xFF, 0, 0xFF, 0];
        let mask = Bitmap {
            width: 4,
            height: 4,
            stride: 2,
            bits_per_pixel: 1,
            data: &and,
        };
        assert_eq!(visible_box(Some(&color), &mask), Some([1, 0, 3, 2]));

        // Nothing drawn, a format it does not read, and a short copy.
        let clear = [0xFFu8, 0, 0xFF, 0, 0, 0, 0, 0];
        let mask = Bitmap {
            width: 8,
            height: 4,
            stride: 2,
            bits_per_pixel: 1,
            data: &clear,
        };
        assert_eq!(visible_box(None, &mask), None);
        let color = Bitmap {
            width: 4,
            height: 4,
            stride: 12,
            bits_per_pixel: 24,
            data: &[0; 48],
        };
        assert_eq!(visible_box(Some(&color), &mask), None);
        let short = Bitmap {
            width: 8,
            height: 4,
            stride: 2,
            bits_per_pixel: 1,
            data: &[0; 6],
        };
        assert_eq!(visible_box(None, &short), None);
    }

    #[test]
    fn only_a_connected_viewer_counts_as_a_stream() {
        let log = "Info: Executing [Desktop]\nInfo: CLIENT CONNECTED\nInfo: Detecting displays";
        assert_eq!(viewer_connected(log), Some(true));
        let left = format!("{log}\nInfo: CLIENT DISCONNECTED\nInfo: Setting default sink");
        assert_eq!(viewer_connected(&left), Some(false));
        // A viewer that vanished: the session ends on a ping timeout.
        let lost = format!("{log}\nInfo: 100.111.100.57: Ping Timeout");
        assert_eq!(viewer_connected(&lost), Some(false));
        let back = format!("{left}\nInfo: CLIENT CONNECTED");
        assert_eq!(viewer_connected(&back), Some(true));
        assert_eq!(viewer_connected("Info: Ready"), None);
    }

    /// Windows' side of the guard, as far as the tests need it.
    struct Fake {
        screen: [i32; 4],
        clip: Option<[i32; 4]>,
        cursor: Option<Shape>,
    }

    impl Fake {
        fn new(cursor: Option<Shape>) -> Fake {
            Fake {
                screen: HD,
                clip: None,
                cursor,
            }
        }

        /// What `GetClipCursor` would answer.
        fn current(&self) -> [i32; 4] {
            self.clip.unwrap_or(self.screen)
        }
    }

    impl Desktop for Fake {
        fn bounds(&self) -> [i32; 4] {
            self.screen
        }

        fn clip(&self) -> Option<[i32; 4]> {
            Some(self.current())
        }

        fn set_clip(&mut self, rect: Option<[i32; 4]>) -> bool {
            self.clip = rect;
            true
        }

        fn custom_cursor(&mut self) -> Option<Shape> {
            self.cursor
        }
    }

    const OFFSET: Shape = Shape {
        hot: (0, 0),
        visible: [16, 16, 47, 47],
    };

    #[test]
    fn the_guard_clips_only_while_a_viewer_watches_and_lifts_only_its_own_clip() {
        let mut desktop = Fake::new(Some(OFFSET));
        let mut clip = Clip::default();
        clip.step(&mut desktop, true);
        assert_eq!(desktop.clip, Some([0, 0, 1904, 1064]));
        clip.step(&mut desktop, true);
        assert_eq!(desktop.clip, Some([0, 0, 1904, 1064]), "steady");
        // The viewer leaves: the cursor is free again.
        clip.step(&mut desktop, false);
        assert_eq!(desktop.clip, None);
        // A standard pointer needs nothing either.
        clip.step(&mut desktop, true);
        desktop.cursor = None;
        clip.step(&mut desktop, true);
        assert_eq!(desktop.clip, None);
    }

    #[test]
    fn a_games_own_confinement_is_kept_and_restored() {
        let mut desktop = Fake::new(Some(OFFSET));
        let window = [100, 100, 1920, 1080];
        desktop.clip = Some(window);
        let mut clip = Clip::default();
        clip.step(&mut desktop, true);
        assert_eq!(desktop.clip, Some([100, 100, 1904, 1064]));
        // The game re-clips to a smaller window while the guard's clip is on.
        let smaller = [200, 200, 800, 600];
        desktop.clip = Some(smaller);
        clip.step(&mut desktop, true);
        assert_eq!(desktop.clip, Some(smaller), "inside the safe area already");
        // The guard lets go: the game's bounds stay, and a guard clip ends
        // with the game's bounds back, not with none.
        desktop.clip = Some(window);
        clip.step(&mut desktop, true);
        assert_eq!(desktop.clip, Some([100, 100, 1904, 1064]));
        clip.step(&mut desktop, false);
        assert_eq!(desktop.clip, Some(window));
    }

    #[test]
    fn a_clip_someone_else_set_is_never_lifted() {
        let mut desktop = Fake::new(Some(OFFSET));
        let mut clip = Clip::default();
        clip.step(&mut desktop, true);
        // A game takes over the confinement; then no viewer is left.
        let game = [10, 10, 500, 500];
        desktop.clip = Some(game);
        clip.step(&mut desktop, false);
        assert_eq!(desktop.clip, Some(game));
    }

    #[test]
    fn a_free_cursor_is_freed_again_whatever_the_desktop_became() {
        let mut desktop = Fake::new(Some(OFFSET));
        let mut clip = Clip::default();
        clip.step(&mut desktop, true);
        assert_eq!(desktop.clip, Some([0, 0, 1904, 1064]));
        // The stream switches to 1440p, and Windows keeps the old clip.
        desktop.screen = [0, 0, 2560, 1440];
        clip.step(&mut desktop, true);
        assert_eq!(
            desktop.clip,
            Some([0, 0, 2544, 1424]),
            "follows the new size"
        );
        clip.step(&mut desktop, false);
        assert_eq!(desktop.clip, None, "not the 1080p desktop");
    }

    #[test]
    fn a_clip_left_by_an_ended_guard_is_lifted() {
        // The previous guard was killed with its clip on.
        let mut desktop = Fake::new(Some(OFFSET));
        desktop.clip = Some([0, 0, 1904, 1064]);
        let mut clip = Clip::default();
        clip.step(&mut desktop, true);
        assert_eq!(desktop.clip, Some([0, 0, 1904, 1064]));
        desktop.cursor = None;
        clip.step(&mut desktop, true);
        assert_eq!(desktop.clip, None);
    }
}
