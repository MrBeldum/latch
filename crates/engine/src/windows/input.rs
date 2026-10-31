use super::elevated::{self, Client};
use crate::input::{DisplayRect, Event, Held, InputSink};
use anyhow::{bail, Result};
use std::sync::mpsc::{self, TryRecvError};
use windows::Win32::UI::{Input::KeyboardAndMouse::*, WindowsAndMessaging::*};

/// SendInput from this process. The elevated helper injects with one too.
#[derive(Default)]
pub(super) struct Local {
    held: Held,
}

/// The host's injector: through the elevated helper once it answers, so
/// windows that run as administrator follow the viewer too; by itself
/// until then, without setup's helper, or after the helper went away.
pub(super) struct Injector {
    local: Local,
    helper: Helper,
    /// This process's integrity level, and the foreground window last
    /// compared with it, to say once why a window ignores the viewer.
    level: u32,
    checked: Option<isize>,
}
enum Helper {
    Connecting(mpsc::Receiver<Client>),
    Connected(Client),
    Gone,
}

fn keyboard(vk: u16, scan: u16, flags: KEYBD_EVENT_FLAGS) -> INPUT {
    INPUT {
        r#type: INPUT_KEYBOARD,
        Anonymous: INPUT_0 {
            ki: KEYBDINPUT {
                wVk: VIRTUAL_KEY(vk),
                wScan: scan,
                dwFlags: flags,
                ..Default::default()
            },
        },
    }
}
fn mouse(dx: i32, dy: i32, data: u32, flags: MOUSE_EVENT_FLAGS) -> INPUT {
    INPUT {
        r#type: INPUT_MOUSE,
        Anonymous: INPUT_0 {
            mi: MOUSEINPUT {
                dx,
                dy,
                mouseData: data,
                dwFlags: flags,
                ..Default::default()
            },
        },
    }
}
fn extended(vk: u16) -> bool {
    matches!(vk, 0x21..=0x28 | 0x2c..=0x2e | 0x5b..=0x5d | 0x6f | 0x90 | 0xa3 | 0xa5)
}
// Target the middle of a pixel's 1/65536 interval, rather than its left edge.
fn normalize(n: i32, start: i32, size: i32) -> i32 {
    let size = i64::from(size.max(1));
    let pixel = (i64::from(n) - i64::from(start)).clamp(0, size - 1);
    (((2 * pixel + 1) * 65536 / (2 * size)).clamp(0, 65535)) as i32
}

fn construct(event: &Event, display: DisplayRect) -> Result<Vec<INPUT>> {
    let _dpi = super::display::DpiAwareness::physical();
    let mut out = Vec::new();
    match event {
        Event::Key { vk, down, .. } => {
            // SAFETY: MapVirtualKeyW has no pointer parameters or side effects.
            let scan = unsafe { MapVirtualKeyW(u32::from(*vk), MAPVK_VK_TO_VSC) } as u16;
            let mut flags = if *down {
                KEYBD_EVENT_FLAGS(0)
            } else {
                KEYEVENTF_KEYUP
            };
            if extended(*vk) {
                flags |= KEYEVENTF_EXTENDEDKEY;
            }
            if scan != 0 {
                flags |= KEYEVENTF_SCANCODE;
            }
            out.push(keyboard(if scan == 0 { *vk } else { 0 }, scan, flags));
        }
        Event::Text(text) => {
            for ch in text.chars() {
                let mut buffer = [0; 2];
                let units = ch.encode_utf16(&mut buffer);
                for flags in [KEYEVENTF_UNICODE, KEYEVENTF_UNICODE | KEYEVENTF_KEYUP] {
                    for &unit in units.iter() {
                        out.push(keyboard(0, unit, flags));
                    }
                }
            }
        }
        Event::MouseMove { dx, dy } => out.push(mouse(
            i32::from(*dx),
            i32::from(*dy),
            0,
            MOUSEEVENTF_MOVE | MOUSEEVENTF_MOVE_NOCOALESCE,
        )),
        Event::MousePosition {
            x,
            y,
            width,
            height,
        } => {
            let (x, y) = display.map(*x, *y, *width, *height);
            // SAFETY: These queries only read the virtual desktop geometry.
            let (left, top, w, h) = unsafe {
                (
                    GetSystemMetrics(SM_XVIRTUALSCREEN),
                    GetSystemMetrics(SM_YVIRTUALSCREEN),
                    GetSystemMetrics(SM_CXVIRTUALSCREEN),
                    GetSystemMetrics(SM_CYVIRTUALSCREEN),
                )
            };
            out.push(mouse(
                normalize(x, left, w),
                normalize(y, top, h),
                0,
                MOUSEEVENTF_MOVE | MOUSEEVENTF_ABSOLUTE | MOUSEEVENTF_VIRTUALDESK,
            ));
        }
        Event::MouseButton { button, down } => {
            let (up, dn, data) = match button {
                1 => (MOUSEEVENTF_LEFTUP, MOUSEEVENTF_LEFTDOWN, 0),
                2 => (MOUSEEVENTF_MIDDLEUP, MOUSEEVENTF_MIDDLEDOWN, 0),
                3 => (MOUSEEVENTF_RIGHTUP, MOUSEEVENTF_RIGHTDOWN, 0),
                4 => (MOUSEEVENTF_XUP, MOUSEEVENTF_XDOWN, 1),
                5 => (MOUSEEVENTF_XUP, MOUSEEVENTF_XDOWN, 2),
                _ => bail!("unknown mouse button {button}"),
            };
            out.push(mouse(0, 0, data, if *down { dn } else { up }));
        }
        Event::Scroll {
            vertical,
            horizontal,
        } => {
            if *vertical != 0 {
                out.push(mouse(0, 0, i32::from(*vertical) as u32, MOUSEEVENTF_WHEEL));
            }
            if *horizontal != 0 {
                out.push(mouse(
                    0,
                    0,
                    i32::from(*horizontal) as u32,
                    MOUSEEVENTF_HWHEEL,
                ));
            }
        }
        Event::ReleaseAll => {}
    }
    Ok(out)
}
fn post(inputs: &[INPUT]) -> Result<()> {
    if inputs.is_empty() {
        return Ok(());
    }
    // SAFETY: SendInput copies the initialized slice during this call.
    let sent = unsafe { SendInput(inputs, std::mem::size_of::<INPUT>() as i32) };
    if sent as usize != inputs.len() {
        bail!(
            "SendInput inserted {sent}/{} events (desktop access or UIPI restriction): {}",
            inputs.len(),
            windows::core::Error::from_win32()
        );
    }
    Ok(())
}
impl Local {
    pub(super) fn apply(&mut self, event: &Event, display: DisplayRect) -> Result<()> {
        if matches!(event, Event::ReleaseAll) {
            self.release_all();
            anyhow::ensure!(
                self.held.keys.is_empty() && self.held.buttons.is_empty(),
                "input releases still pending"
            );
            return Ok(());
        }
        let inputs = construct(event, display)?;
        post(&inputs)?;
        self.held.note(event);
        Ok(())
    }
    pub(super) fn release_all(&mut self) {
        for event in self.held.drain() {
            if let Err(e) = construct(&event, DisplayRect::default()).and_then(|v| post(&v)) {
                tracing::warn!("release input: {e:#}");
                match event {
                    Event::Key { vk, .. } => {
                        self.held.keys.insert(vk);
                    }
                    Event::MouseButton { button, .. } => {
                        self.held.buttons.insert(button);
                    }
                    _ => {}
                }
            }
        }
    }
}
impl Drop for Local {
    fn drop(&mut self) {
        self.release_all();
    }
}

impl Injector {
    pub(super) fn new() -> Self {
        Self {
            local: Local::default(),
            helper: Helper::Connecting(elevated::connect()),
            level: elevated::own_integrity(),
            checked: None,
        }
    }
    fn client(&mut self) -> Option<&mut Client> {
        if let Helper::Connecting(started) = &self.helper {
            match started.try_recv() {
                Ok(client) => {
                    tracing::info!("input goes through the elevated helper, so windows that run as administrator follow the viewer too");
                    self.helper = Helper::Connected(client);
                }
                Err(TryRecvError::Disconnected) => self.helper = Helper::Gone,
                Err(TryRecvError::Empty) => {}
            }
        }
        match &mut self.helper {
            Helper::Connected(client) => Some(client),
            _ => None,
        }
    }
    /// The helper's result, or `None` when there is no helper (any more).
    fn remote(&mut self, event: &Event, display: DisplayRect) -> Option<Result<()>> {
        match self.client()?.apply(event, display) {
            Ok(result) => Some(result),
            Err(e) => {
                tracing::warn!(
                    "the elevated input helper stopped ({e}); Latch injects input by itself again"
                );
                self.helper = Helper::Gone;
                None
            }
        }
    }
    /// Windows drops this process's input to a window of a higher
    /// integrity level and reports it as sent: say so, once per window.
    fn warn_if_ignored(&mut self) {
        // SAFETY: Reads the foreground window's handle.
        let window = unsafe { GetForegroundWindow() };
        if self.checked == Some(window.0 as isize) {
            return;
        }
        self.checked = Some(window.0 as isize);
        if let Some((name, level)) = elevated::window_owner(window) {
            if level > self.level {
                tracing::warn!("{name} runs as administrator, so Windows ignores the viewer's mouse and keyboard while it is in front; run setup again (Set up as administrator) to allow it");
            }
        }
    }
}
impl InputSink for Injector {
    fn apply(&mut self, event: &Event, display: DisplayRect) -> Result<()> {
        if matches!(event, Event::ReleaseAll) {
            // Each side lets go of what it pressed.
            let remote = self.remote(event, display).unwrap_or(Ok(()));
            let local = self.local.apply(event, display);
            return remote.and(local);
        }
        if let Some(result) = self.remote(event, display) {
            return result;
        }
        self.warn_if_ignored();
        self.local.apply(event, display)
    }
    fn release_all(&mut self) {
        if let Some(Err(e)) = self.remote(&Event::ReleaseAll, DisplayRect::default()) {
            tracing::warn!("release input: {e:#}");
        }
        self.local.release_all();
    }
}
impl Drop for Injector {
    fn drop(&mut self) {
        self.release_all();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn absolute_coordinates_reconstruct_every_pixel() {
        for size in [1, 1920, 3840, 7680] {
            for p in 0..size {
                let normalized = normalize(p - 1920, -1920, size);
                assert_eq!(
                    i64::from(normalized) * i64::from(size) / 65536,
                    i64::from(p)
                );
            }
        }
    }
    #[test]
    fn relative_motion_disables_windows_coalescing() -> Result<()> {
        let v = construct(&Event::MouseMove { dx: 1, dy: -2 }, DisplayRect::default())?;
        // SAFETY: construct produces mouse INPUT for MouseMove.
        unsafe {
            assert!(v[0]
                .Anonymous
                .mi
                .dwFlags
                .contains(MOUSEEVENTF_MOVE_NOCOALESCE));
        }
        Ok(())
    }
    #[test]
    fn constructs_without_posting() -> Result<()> {
        let v = construct(&Event::Text("A😀".into()), DisplayRect::default())?;
        assert_eq!(v.len(), 6);
        // SAFETY: These were constructed with the INPUT_KEYBOARD discriminator.
        unsafe {
            assert_eq!(v[2].Anonymous.ki.wScan, 0xd83d);
            assert_eq!(v[3].Anonymous.ki.wScan, 0xde00);
            assert!(!v[2].Anonymous.ki.dwFlags.contains(KEYEVENTF_KEYUP));
            assert!(!v[3].Anonymous.ki.dwFlags.contains(KEYEVENTF_KEYUP));
            assert!(v[4].Anonymous.ki.dwFlags.contains(KEYEVENTF_KEYUP));
            assert!(v[5].Anonymous.ki.dwFlags.contains(KEYEVENTF_KEYUP));
        }
        for vk in [0x25, 0xa3, 0xa5, 0x5b, 0x90, 0x6f, 0x2c, 0x5d] {
            let v = construct(
                &Event::Key {
                    vk,
                    down: false,
                    modifiers: 0,
                },
                DisplayRect::default(),
            )?;
            // SAFETY: The event above constructs keyboard input.
            unsafe {
                assert!(v[0]
                    .Anonymous
                    .ki
                    .dwFlags
                    .contains(KEYEVENTF_EXTENDEDKEY | KEYEVENTF_KEYUP));
            }
        }
        let v = construct(
            &Event::MouseButton {
                button: 5,
                down: true,
            },
            DisplayRect::default(),
        )?;
        // SAFETY: The event above constructs mouse input.
        unsafe {
            assert_eq!(v[0].Anonymous.mi.mouseData, 2);
        }
        Ok(())
    }
}
