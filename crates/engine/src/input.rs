//! Keyboard and mouse into the desktop.

pub use latch_proto::input::Event;
use std::collections::BTreeSet;

/// The captured display in the coordinates the platform's injection uses:
/// points on macOS, virtual-desktop pixels on Windows, screen pixels on X11.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct DisplayRect {
    pub x: i32,
    pub y: i32,
    pub width: u32,
    pub height: u32,
}

impl DisplayRect {
    /// Where a point in a `pw × ph` picture lands on this display.
    pub fn map(&self, x: u16, y: u16, pw: u16, ph: u16) -> (i32, i32) {
        let pw = pw.max(1) as i64;
        let ph = ph.max(1) as i64;
        let fx = (x as i64).min(pw - 1) * self.width as i64 / pw;
        let fy = (y as i64).min(ph - 1) * self.height as i64 / ph;
        (self.x + fx as i32, self.y + fy as i32)
    }
}

pub trait InputSink: Send {
    /// Applies one event. `display` is where absolute positions land.
    fn apply(&mut self, event: &Event, display: DisplayRect) -> anyhow::Result<()>;

    /// Releases every key and button this sink pressed and has not
    /// released (the viewer left, or asked).
    fn release_all(&mut self);
}

/// Opens this platform's injector.
pub fn open() -> anyhow::Result<Box<dyn InputSink>> {
    if crate::testmode::motion().is_some() {
        return crate::testmode::open_input();
    }
    #[cfg(target_os = "macos")]
    return crate::mac::open_input();
    #[cfg(windows)]
    return crate::windows::open_input();
    #[cfg(target_os = "linux")]
    return crate::linux::open_input();
}

/// What a sink holds down, so it can let go of all of it.
#[derive(Debug, Default)]
pub struct Held {
    pub keys: BTreeSet<u16>,
    pub buttons: BTreeSet<u8>,
}

impl Held {
    /// Records a key or button change; returns false for a release of
    /// something not held (which a sink may skip).
    pub fn note(&mut self, event: &Event) -> bool {
        match *event {
            Event::Key { vk, down, .. } => {
                if down {
                    self.keys.insert(vk);
                    true
                } else {
                    self.keys.remove(&vk)
                }
            }
            Event::MouseButton { button, down } => {
                if down {
                    self.buttons.insert(button);
                    true
                } else {
                    self.buttons.remove(&button)
                }
            }
            _ => true,
        }
    }

    /// Everything held, as release events; empties the set.
    pub fn drain(&mut self) -> Vec<Event> {
        let mut out: Vec<Event> = std::mem::take(&mut self.keys)
            .into_iter()
            .map(|vk| Event::Key {
                vk,
                down: false,
                modifiers: 0,
            })
            .collect();
        out.extend(std::mem::take(&mut self.buttons).into_iter().map(|button| {
            Event::MouseButton {
                button,
                down: false,
            }
        }));
        out
    }
}

/// Windows virtual-key codes to other platforms' key codes.
pub mod keymap {
    /// macOS virtual key code (`kVK_*`, Carbon `Events.h`) for a Windows VK.
    pub fn vk_to_mac(vk: u16) -> Option<u16> {
        let _ = vk;
        todo!()
    }

    /// X11 keysym for a Windows VK on a US layout, unshifted.
    pub fn vk_to_keysym(vk: u16) -> Option<u32> {
        let _ = vk;
        todo!()
    }
}
