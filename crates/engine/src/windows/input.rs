use crate::input::{DisplayRect, Event, Held, InputSink};
use anyhow::{bail, Result};
use windows::Win32::UI::{Input::KeyboardAndMouse::*, WindowsAndMessaging::*};

#[derive(Default)]
pub(super) struct Injector {
    held: Held,
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
fn construct(event: &Event, display: DisplayRect) -> Result<Vec<INPUT>> {
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
            for unit in text.encode_utf16() {
                out.push(keyboard(0, unit, KEYEVENTF_UNICODE));
                out.push(keyboard(0, unit, KEYEVENTF_UNICODE | KEYEVENTF_KEYUP));
            }
        }
        Event::MouseMove { dx, dy } => {
            out.push(mouse(i32::from(*dx), i32::from(*dy), 0, MOUSEEVENTF_MOVE))
        }
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
            let norm = |n: i32, start: i32, size: i32| {
                ((i64::from(n) - i64::from(start)) * 65535 / i64::from((size - 1).max(1)))
                    .clamp(0, 65535) as i32
            };
            out.push(mouse(
                norm(x, left, w),
                norm(y, top, h),
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
impl InputSink for Injector {
    fn apply(&mut self, event: &Event, display: DisplayRect) -> Result<()> {
        if matches!(event, Event::ReleaseAll) {
            self.release_all();
            return Ok(());
        }
        let inputs = construct(event, display)?;
        post(&inputs)?;
        self.held.note(event);
        Ok(())
    }
    fn release_all(&mut self) {
        for event in self.held.drain() {
            if let Err(e) = construct(&event, DisplayRect::default()).and_then(|v| post(&v)) {
                tracing::warn!("release input: {e:#}");
            }
        }
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
    fn constructs_without_posting() -> Result<()> {
        let v = construct(&Event::Text("A😀".into()), DisplayRect::default())?;
        assert_eq!(v.len(), 6);
        // SAFETY: These were constructed with the INPUT_KEYBOARD discriminator.
        unsafe {
            assert_eq!(v[2].Anonymous.ki.wScan, 0xd83d);
            assert_eq!(v[4].Anonymous.ki.wScan, 0xde00);
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
