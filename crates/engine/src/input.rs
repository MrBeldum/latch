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
        Some(match vk {
            // Letters (ANSI layout)
            0x41 => 0x00, // A -> kVK_ANSI_A
            0x42 => 0x0B, // B -> kVK_ANSI_B
            0x43 => 0x08, // C -> kVK_ANSI_C
            0x44 => 0x02, // D -> kVK_ANSI_D
            0x45 => 0x0E, // E -> kVK_ANSI_E
            0x46 => 0x03, // F -> kVK_ANSI_F
            0x47 => 0x05, // G -> kVK_ANSI_G
            0x48 => 0x04, // H -> kVK_ANSI_H
            0x49 => 0x22, // I -> kVK_ANSI_I
            0x4A => 0x26, // J -> kVK_ANSI_J
            0x4B => 0x28, // K -> kVK_ANSI_K
            0x4C => 0x25, // L -> kVK_ANSI_L
            0x4D => 0x2E, // M -> kVK_ANSI_M
            0x4E => 0x2D, // N -> kVK_ANSI_N
            0x4F => 0x1F, // O -> kVK_ANSI_O
            0x50 => 0x23, // P -> kVK_ANSI_P
            0x51 => 0x0C, // Q -> kVK_ANSI_Q
            0x52 => 0x0F, // R -> kVK_ANSI_R
            0x53 => 0x01, // S -> kVK_ANSI_S
            0x54 => 0x11, // T -> kVK_ANSI_T
            0x55 => 0x20, // U -> kVK_ANSI_U
            0x56 => 0x09, // V -> kVK_ANSI_V
            0x57 => 0x0D, // W -> kVK_ANSI_W
            0x58 => 0x07, // X -> kVK_ANSI_X
            0x59 => 0x10, // Y -> kVK_ANSI_Y
            0x5A => 0x06, // Z -> kVK_ANSI_Z

            // Digits
            0x30 => 0x1D, // 0 -> kVK_ANSI_0
            0x31 => 0x12, // 1 -> kVK_ANSI_1
            0x32 => 0x13, // 2 -> kVK_ANSI_2
            0x33 => 0x14, // 3 -> kVK_ANSI_3
            0x34 => 0x15, // 4 -> kVK_ANSI_4
            0x35 => 0x17, // 5 -> kVK_ANSI_5
            0x36 => 0x16, // 6 -> kVK_ANSI_6
            0x37 => 0x1A, // 7 -> kVK_ANSI_7
            0x38 => 0x1C, // 8 -> kVK_ANSI_8
            0x39 => 0x19, // 9 -> kVK_ANSI_9

            // Function keys F1 - F20
            0x70 => 0x7A, // F1 -> kVK_F1
            0x71 => 0x78, // F2 -> kVK_F2
            0x72 => 0x63, // F3 -> kVK_F3
            0x73 => 0x76, // F4 -> kVK_F4
            0x74 => 0x60, // F5 -> kVK_F5
            0x75 => 0x61, // F6 -> kVK_F6
            0x76 => 0x62, // F7 -> kVK_F7
            0x77 => 0x64, // F8 -> kVK_F8
            0x78 => 0x65, // F9 -> kVK_F9
            0x79 => 0x6D, // F10 -> kVK_F10
            0x7A => 0x67, // F11 -> kVK_F11
            0x7B => 0x6F, // F12 -> kVK_F12
            0x7C => 0x69, // F13 -> kVK_F13
            0x7D => 0x6B, // F14 -> kVK_F14
            0x7E => 0x71, // F15 -> kVK_F15
            0x7F => 0x6A, // F16 -> kVK_F16
            0x80 => 0x40, // F17 -> kVK_F17
            0x81 => 0x4F, // F18 -> kVK_F18
            0x82 => 0x50, // F19 -> kVK_F19
            0x83 => 0x5A, // F20 -> kVK_F20

            // Arrows
            0x25 => 0x7B, // Left -> kVK_LeftArrow
            0x26 => 0x7E, // Up -> kVK_UpArrow
            0x27 => 0x7C, // Right -> kVK_RightArrow
            0x28 => 0x7D, // Down -> kVK_DownArrow

            // Navigation / Editing
            0x24 => 0x73, // Home -> kVK_Home
            0x23 => 0x77, // End -> kVK_End
            0x21 => 0x74, // PageUp -> kVK_PageUp
            0x22 => 0x79, // PageDown -> kVK_PageDown
            0x2E => 0x75, // Delete (forward) -> kVK_ForwardDelete
            0x2D => 0x72, // Insert -> kVK_Help (per specification)
            0x2F => 0x72, // Help -> kVK_Help

            // Keypad digits and operators
            0x60 => 0x52, // Keypad 0 -> kVK_ANSI_Keypad0
            0x61 => 0x53, // Keypad 1 -> kVK_ANSI_Keypad1
            0x62 => 0x54, // Keypad 2 -> kVK_ANSI_Keypad2
            0x63 => 0x55, // Keypad 3 -> kVK_ANSI_Keypad3
            0x64 => 0x56, // Keypad 4 -> kVK_ANSI_Keypad4
            0x65 => 0x57, // Keypad 5 -> kVK_ANSI_Keypad5
            0x66 => 0x58, // Keypad 6 -> kVK_ANSI_Keypad6
            0x67 => 0x59, // Keypad 7 -> kVK_ANSI_Keypad7
            0x68 => 0x5B, // Keypad 8 -> kVK_ANSI_Keypad8
            0x69 => 0x5C, // Keypad 9 -> kVK_ANSI_Keypad9
            0x6A => 0x43, // Multiply -> kVK_ANSI_KeypadMultiply
            0x6B => 0x45, // Add -> kVK_ANSI_KeypadPlus
            0x6D => 0x4E, // Subtract -> kVK_ANSI_KeypadMinus
            0x6E => 0x41, // Decimal -> kVK_ANSI_KeypadDecimal
            0x6F => 0x4B, // Divide -> kVK_ANSI_KeypadDivide
            0x90 => 0x47, // NumLock -> kVK_ANSI_KeypadClear

            // Control / TTY
            0x1B => 0x35, // Escape -> kVK_Escape
            0x09 => 0x30, // Tab -> kVK_Tab
            0x0D => 0x24, // Return -> kVK_Return
            0x20 => 0x31, // Space -> kVK_Space
            0x08 => 0x33, // Backspace -> kVK_Delete (per specification)
            0x14 => 0x39, // CapsLock -> kVK_CapsLock
            0x5D => 0x6E, // Menu/Apps -> kVK_ContextualMenu

            // Modifiers (both sides; plain map to left ones)
            0x10 => 0x38, // VK_SHIFT -> kVK_Shift
            0xA0 => 0x38, // VK_LSHIFT -> kVK_Shift
            0xA1 => 0x3C, // VK_RSHIFT -> kVK_RightShift
            0x11 => 0x3B, // VK_CONTROL -> kVK_Control
            0xA2 => 0x3B, // VK_LCONTROL -> kVK_Control
            0xA3 => 0x3E, // VK_RCONTROL -> kVK_RightControl
            0x12 => 0x3A, // VK_MENU -> kVK_Option
            0xA4 => 0x3A, // VK_LMENU -> kVK_Option
            0xA5 => 0x3D, // VK_RMENU -> kVK_RightOption
            0x5B => 0x37, // VK_LWIN -> kVK_Command
            0x5C => 0x36, // VK_RWIN -> kVK_RightCommand

            // US OEM Punctuation
            0xBA => 0x29, // VK_OEM_1 (;:) -> kVK_ANSI_Semicolon
            0xBB => 0x18, // VK_OEM_PLUS (=+) -> kVK_ANSI_Equal
            0xBC => 0x2B, // VK_OEM_COMMA (,<) -> kVK_ANSI_Comma
            0xBD => 0x1B, // VK_OEM_MINUS (-_) -> kVK_ANSI_Minus
            0xBE => 0x2F, // VK_OEM_PERIOD (.>) -> kVK_ANSI_Period
            0xBF => 0x2C, // VK_OEM_2 (/?) -> kVK_ANSI_Slash
            0xC0 => 0x32, // VK_OEM_3 (`~) -> kVK_ANSI_Grave
            0xDB => 0x21, // VK_OEM_4 ([{) -> kVK_ANSI_LeftBracket
            0xDC => 0x2A, // VK_OEM_5 (\|) -> kVK_ANSI_Backslash
            0xDD => 0x1E, // VK_OEM_6 (]}) -> kVK_ANSI_RightBracket
            0xDE => 0x27, // VK_OEM_7 ('") -> kVK_ANSI_Quote

            _ => return None,
        })
    }

    /// X11 keysym for a Windows VK on a US layout, unshifted.
    pub fn vk_to_keysym(vk: u16) -> Option<u32> {
        Some(match vk {
            // Lowercase letters ('A'..='Z' -> 0x0061..=0x007a)
            0x41..=0x5A => (vk - 0x41 + 0x0061) as u32,

            // Digits ('0'..='9' -> 0x0030..=0x0039)
            0x30..=0x39 => vk as u32,

            // Function keys F1..F24 (0xffbe..=0xffd5)
            0x70..=0x87 => (vk - 0x70 + 0xffbe) as u32,

            // Modifiers (XK_*)
            0x10 | 0xA0 => 0xffe1, // VK_SHIFT / VK_LSHIFT -> XK_Shift_L
            0xA1 => 0xffe2,        // VK_RSHIFT -> XK_Shift_R
            0x11 | 0xA2 => 0xffe3, // VK_CONTROL / VK_LCONTROL -> XK_Control_L
            0xA3 => 0xffe4,        // VK_RCONTROL -> XK_Control_R
            0x12 | 0xA4 => 0xffe9, // VK_MENU / VK_LMENU -> XK_Alt_L
            0xA5 => 0xffea,        // VK_RMENU -> XK_Alt_R
            0x5B => 0xffeb,        // VK_LWIN -> XK_Super_L
            0x5C => 0xffec,        // VK_RWIN -> XK_Super_R
            0x14 => 0xffe5,        // VK_CAPITAL -> XK_Caps_Lock
            0x90 => 0xff7f,        // VK_NUMLOCK -> XK_Num_Lock
            0x91 => 0xff14,        // VK_SCROLL -> XK_Scroll_Lock

            // Arrows
            0x25 => 0xff51, // VK_LEFT -> XK_Left
            0x26 => 0xff52, // VK_UP -> XK_Up
            0x27 => 0xff53, // VK_RIGHT -> XK_Right
            0x28 => 0xff54, // VK_DOWN -> XK_Down

            // Navigation
            0x21 => 0xff55, // VK_PRIOR -> XK_Page_Up
            0x22 => 0xff56, // VK_NEXT -> XK_Page_Down
            0x23 => 0xff57, // VK_END -> XK_End
            0x24 => 0xff50, // VK_HOME -> XK_Home
            0x2D => 0xff63, // VK_INSERT -> XK_Insert
            0x2E => 0xffff, // VK_DELETE -> XK_Delete

            // Keypad (XK_KP_*)
            0x60..=0x69 => (vk - 0x60 + 0xffb0) as u32, // VK_NUMPAD0..9 -> XK_KP_0..9
            0x6A => 0xffaa,                             // VK_MULTIPLY -> XK_KP_Multiply
            0x6B => 0xffab,                             // VK_ADD -> XK_KP_Add
            0x6C => 0xffac,                             // VK_SEPARATOR -> XK_KP_Separator
            0x6D => 0xffad,                             // VK_SUBTRACT -> XK_KP_Subtract
            0x6E => 0xffae,                             // VK_DECIMAL -> XK_KP_Decimal
            0x6F => 0xffaf,                             // VK_DIVIDE -> XK_KP_Divide

            // Control / TTY
            0x08 => 0xff08,        // VK_BACK -> XK_BackSpace
            0x09 => 0xff09,        // VK_TAB -> XK_Tab
            0x0C => 0xff0b,        // VK_CLEAR -> XK_Clear
            0x0D => 0xff0d,        // VK_RETURN -> XK_Return
            0x13 => 0xff13,        // VK_PAUSE -> XK_Pause
            0x1B => 0xff1b,        // VK_ESCAPE -> XK_Escape
            0x20 => 0x0020,        // VK_SPACE -> XK_space
            0x29 => 0xff60,        // VK_SELECT -> XK_Select
            0x2A | 0x2C => 0xff61, // VK_PRINT / VK_SNAPSHOT -> XK_Print
            0x2B => 0xff62,        // VK_EXECUTE -> XK_Execute
            0x2F => 0xff6a,        // VK_HELP -> XK_Help
            0x5D => 0xff67,        // VK_APPS -> XK_Menu

            // Unshifted US Punctuation
            0xBA => 0x003b, // VK_OEM_1 (;:) -> XK_semicolon
            0xBB => 0x003d, // VK_OEM_PLUS (=+) -> XK_equal
            0xBC => 0x002c, // VK_OEM_COMMA (,<) -> XK_comma
            0xBD => 0x002d, // VK_OEM_MINUS (-_) -> XK_minus
            0xBE => 0x002e, // VK_OEM_PERIOD (.>) -> XK_period
            0xBF => 0x002f, // VK_OEM_2 (/?) -> XK_slash
            0xC0 => 0x0060, // VK_OEM_3 (`~) -> XK_grave
            0xDB => 0x005b, // VK_OEM_4 ([{) -> XK_bracketleft
            0xDC => 0x005c, // VK_OEM_5 (\|) -> XK_backslash
            0xDD => 0x005d, // VK_OEM_6 (]}) -> XK_bracketright
            0xDE => 0x0027, // VK_OEM_7 ('") -> XK_apostrophe

            _ => return None,
        })
    }

    #[cfg(test)]
    mod tests {
        use super::*;
        use std::collections::HashMap;

        #[test]
        fn test_representative_keys() {
            // A -> kVK_ANSI_A (0x00), XK_a (0x0061)
            assert_eq!(vk_to_mac(0x41), Some(0x00));
            assert_eq!(vk_to_keysym(0x41), Some(0x0061));

            // Return -> kVK_Return (0x24), XK_Return (0xff0d)
            assert_eq!(vk_to_mac(0x0D), Some(0x24));
            assert_eq!(vk_to_keysym(0x0D), Some(0xff0d));

            // Backspace -> kVK_Delete (0x33), XK_BackSpace (0xff08)
            assert_eq!(vk_to_mac(0x08), Some(0x33));
            assert_eq!(vk_to_keysym(0x08), Some(0xff08));

            // Forward Delete -> kVK_ForwardDelete (0x75), XK_Delete (0xffff)
            assert_eq!(vk_to_mac(0x2E), Some(0x75));
            assert_eq!(vk_to_keysym(0x2E), Some(0xffff));

            // Insert -> kVK_Help (0x72), XK_Insert (0xff63)
            assert_eq!(vk_to_mac(0x2D), Some(0x72));
            assert_eq!(vk_to_keysym(0x2D), Some(0xff63));

            // F1..F20 on Mac, F1..F24 on X11
            assert_eq!(vk_to_mac(0x70), Some(0x7A)); // F1
            assert_eq!(vk_to_mac(0x83), Some(0x5A)); // F20
            assert_eq!(vk_to_mac(0x84), None); // F21 not in standard Carbon
            assert_eq!(vk_to_keysym(0x70), Some(0xffbe)); // F1
            assert_eq!(vk_to_keysym(0x87), Some(0xffd5)); // F24

            // Left/Right modifiers
            assert_eq!(vk_to_mac(0x10), Some(0x38)); // VK_SHIFT -> Left Shift
            assert_eq!(vk_to_mac(0xA0), Some(0x38)); // VK_LSHIFT -> Left Shift
            assert_eq!(vk_to_mac(0xA1), Some(0x3C)); // VK_RSHIFT -> Right Shift
            assert_eq!(vk_to_keysym(0x10), Some(0xffe1)); // VK_SHIFT -> XK_Shift_L
            assert_eq!(vk_to_keysym(0xA0), Some(0xffe1)); // VK_LSHIFT -> XK_Shift_L
            assert_eq!(vk_to_keysym(0xA1), Some(0xffe2)); // VK_RSHIFT -> XK_Shift_R
        }

        #[test]
        fn test_vk_collisions_mac() {
            // Documented intended collisions for macOS:
            // 1. (VK_SHIFT, VK_LSHIFT) -> kVK_Shift (0x38)
            // 2. (VK_CONTROL, VK_LCONTROL) -> kVK_Control (0x3B)
            // 3. (VK_MENU, VK_LMENU) -> kVK_Option (0x3A)
            // 4. (VK_INSERT, VK_HELP) -> kVK_Help (0x72)
            let mut mac_to_vks: HashMap<u16, Vec<u16>> = HashMap::new();
            for vk in 0..=512 {
                if let Some(mac) = vk_to_mac(vk) {
                    mac_to_vks.entry(mac).or_default().push(vk);
                }
            }

            for (mac, vks) in mac_to_vks {
                if vks.len() > 1 {
                    let expected_collision = match mac {
                        0x38 => vks == [0x10, 0xA0], // VK_SHIFT, VK_LSHIFT
                        0x3B => vks == [0x11, 0xA2], // VK_CONTROL, VK_LCONTROL
                        0x3A => vks == [0x12, 0xA4], // VK_MENU, VK_LMENU
                        0x72 => vks == [0x2D, 0x2F], // VK_INSERT, VK_HELP
                        _ => false,
                    };
                    assert!(
                        expected_collision,
                        "Unexpected collision on macOS code {mac:#x} for VKs: {vks:?}"
                    );
                }
            }
        }

        #[test]
        fn test_vk_collisions_keysym() {
            // Documented intended collisions for X11:
            // 1. (VK_SHIFT, VK_LSHIFT) -> XK_Shift_L (0xffe1)
            // 2. (VK_CONTROL, VK_LCONTROL) -> XK_Control_L (0xffe3)
            // 3. (VK_MENU, VK_LMENU) -> XK_Alt_L (0xffe9)
            // 4. (VK_PRINT, VK_SNAPSHOT) -> XK_Print (0xff61)
            let mut keysym_to_vks: HashMap<u32, Vec<u16>> = HashMap::new();
            for vk in 0..=512 {
                if let Some(sym) = vk_to_keysym(vk) {
                    keysym_to_vks.entry(sym).or_default().push(vk);
                }
            }

            for (sym, vks) in keysym_to_vks {
                if vks.len() > 1 {
                    let expected_collision = match sym {
                        0xffe1 => vks == [0x10, 0xA0], // VK_SHIFT, VK_LSHIFT
                        0xffe3 => vks == [0x11, 0xA2], // VK_CONTROL, VK_LCONTROL
                        0xffe9 => vks == [0x12, 0xA4], // VK_MENU, VK_LMENU
                        0xff61 => vks == [0x2A, 0x2C], // VK_PRINT, VK_SNAPSHOT
                        _ => false,
                    };
                    assert!(
                        expected_collision,
                        "Unexpected collision on X11 keysym {sym:#x} for VKs: {vks:?}"
                    );
                }
            }
        }
    }
}
