//! Keyboard and mouse translation: egui events in, Moonlight input calls out.

use egui::{Key, Modifiers, PointerButton};
use latch_stream::ffi::{
    BUTTON_LEFT, BUTTON_MIDDLE, BUTTON_RIGHT, BUTTON_X1, BUTTON_X2, MODIFIER_ALT, MODIFIER_CTRL,
    MODIFIER_META, MODIFIER_SHIFT,
};
use latch_stream::Input;
use std::collections::BTreeSet;
use std::ffi::c_char;

pub const VK_SHIFT: i16 = 0x10;
pub const VK_CONTROL: i16 = 0x11;
pub const VK_MENU: i16 = 0x12;
pub const VK_LWIN: i16 = 0x5B;
pub const VK_INSERT: i16 = 0x2D;
pub const VK_DELETE: i16 = 0x2E;
pub const VK_ESCAPE: i16 = 0x1B;
pub const VK_TAB: i16 = 0x09;
pub const VK_RETURN: i16 = 0x0D;
pub const VK_SNAPSHOT: i16 = 0x2C;
pub const VK_C: i16 = 0x43;
pub const VK_R: i16 = 0x52;
pub const VK_V: i16 = 0x56;
pub const VK_X: i16 = 0x58;

/// Windows virtual-key code for an egui key, on a US layout.
pub fn vk(key: Key) -> Option<i16> {
    use Key::*;
    Some(match key {
        ArrowDown => 0x28,
        ArrowLeft => 0x25,
        ArrowRight => 0x27,
        ArrowUp => 0x26,
        Escape => VK_ESCAPE,
        Tab => VK_TAB,
        Backspace => 0x08,
        Enter => 0x0D,
        Space => 0x20,
        Insert => VK_INSERT,
        Delete => VK_DELETE,
        Home => 0x24,
        End => 0x23,
        PageUp => 0x21,
        PageDown => 0x22,
        Colon | Semicolon => 0xBA,
        Comma => 0xBC,
        Backslash | Pipe => 0xDC,
        Slash | Questionmark => 0xBF,
        Exclamationmark => 0x31,
        OpenBracket | OpenCurlyBracket => 0xDB,
        CloseBracket | CloseCurlyBracket => 0xDD,
        Backtick => 0xC0,
        Minus => 0xBD,
        Period => 0xBE,
        Plus | Equals => 0xBB,
        Quote => 0xDE,
        Num0 => 0x30,
        Num1 => 0x31,
        Num2 => 0x32,
        Num3 => 0x33,
        Num4 => 0x34,
        Num5 => 0x35,
        Num6 => 0x36,
        Num7 => 0x37,
        Num8 => 0x38,
        Num9 => 0x39,
        A => 0x41,
        B => 0x42,
        C => 0x43,
        D => 0x44,
        E => 0x45,
        F => 0x46,
        G => 0x47,
        H => 0x48,
        I => 0x49,
        J => 0x4A,
        K => 0x4B,
        L => 0x4C,
        M => 0x4D,
        N => 0x4E,
        O => 0x4F,
        P => 0x50,
        Q => 0x51,
        R => 0x52,
        S => 0x53,
        T => 0x54,
        U => 0x55,
        V => 0x56,
        W => 0x57,
        X => 0x58,
        Y => 0x59,
        Z => 0x5A,
        F1 => 0x70,
        F2 => 0x71,
        F3 => 0x72,
        F4 => 0x73,
        F5 => 0x74,
        F6 => 0x75,
        F7 => 0x76,
        F8 => 0x77,
        F9 => 0x78,
        F10 => 0x79,
        F11 => 0x7A,
        F12 => 0x7B,
        F13 => 0x7C,
        F14 => 0x7D,
        F15 => 0x7E,
        F16 => 0x7F,
        F17 => 0x80,
        F18 => 0x81,
        F19 => 0x82,
        F20 => 0x83,
        F21 => 0x84,
        F22 => 0x85,
        F23 => 0x86,
        F24 => 0x87,
        _ => return None,
    })
}

pub fn button(b: PointerButton) -> Option<i32> {
    Some(match b {
        PointerButton::Primary => BUTTON_LEFT,
        PointerButton::Secondary => BUTTON_RIGHT,
        PointerButton::Middle => BUTTON_MIDDLE,
        PointerButton::Extra1 => BUTTON_X1,
        PointerButton::Extra2 => BUTTON_X2,
    })
}

/// One key event as Moonlight takes it: the key, down or up, and the
/// modifier mask sent with it.
type KeyEvent = (i16, bool, c_char);

/// The modifier keys, in the order their mask bits are listed.
const MODIFIERS: [i16; 4] = [VK_SHIFT, VK_CONTROL, VK_MENU, VK_LWIN];

fn send(input: &Input, events: &[KeyEvent]) {
    for &(vk, down, mask) in events {
        input.key(vk, down, mask);
    }
}

/// Press `keys` in order and release them in reverse, with `mask` for the
/// modifiers already held. Usable from any thread, which is what a paste
/// that first has to reach the PC's clipboard needs.
pub fn press_chord(input: &Input, keys: &[i16], mask: c_char) {
    send(input, &chord_events(keys, mask));
}

fn chord_events(keys: &[i16], mask: c_char) -> Vec<KeyEvent> {
    let mut events = Vec::new();
    let mut held = Vec::new();
    for &k in keys {
        held.push(k);
        events.push((k, true, mask | mask_of(&held)));
    }
    for &k in keys.iter().rev() {
        held.retain(|&h| h != k);
        events.push((k, false, mask | mask_of(&held)));
    }
    events
}

/// [`press_chord`] for a chord that must arrive with no other modifier:
/// those in `mask` that `keys` does not use are let go first, so ⌘ held as
/// Ctrl does not turn Shift+Insert into Ctrl+Shift+Insert. They stay up;
/// the window presses a modifier the user still holds back down once the
/// paste is done (see `clipboard::Sync::pastes_done`).
pub fn press_chord_alone(input: &Input, keys: &[i16], mask: c_char) {
    send(input, &alone_events(keys, keys, mask, false));
}

/// The events that send `keys` with no other modifier down. Modifiers in
/// `mask` outside `keys` are released first and, with `restore`, pressed
/// again after; `press` is the part of `keys` to press, all of it when
/// `mask` may be stale (a worker thread's copy).
fn alone_events(keys: &[i16], press: &[i16], mask: c_char, restore: bool) -> Vec<KeyEvent> {
    let others: Vec<i16> = MODIFIERS
        .into_iter()
        .filter(|&vk| mask & mask_of(&[vk]) != 0 && !keys.contains(&vk))
        .collect();
    let mut mask = mask;
    let mut events = Vec::new();
    for &vk in &others {
        mask &= !mask_of(&[vk]);
        events.push((vk, false, mask));
    }
    events.extend(chord_events(press, mask));
    if restore {
        for &vk in &others {
            mask |= mask_of(&[vk]);
            events.push((vk, true, mask));
        }
    }
    events
}

fn mask_of(keys: &[i16]) -> c_char {
    let mut m: c_char = 0;
    if keys.contains(&VK_SHIFT) {
        m |= MODIFIER_SHIFT;
    }
    if keys.contains(&VK_CONTROL) {
        m |= MODIFIER_CTRL;
    }
    if keys.contains(&VK_MENU) {
        m |= MODIFIER_ALT;
    }
    if keys.contains(&VK_LWIN) {
        m |= MODIFIER_META;
    }
    m
}

/// Which keys and buttons the PC currently believes are down, so they can all
/// be released when focus or capture is lost.
#[derive(Default)]
pub struct Held {
    keys: BTreeSet<i16>,
    buttons: BTreeSet<i32>,
}

impl Held {
    pub fn key(&mut self, input: &Input, vk: i16, down: bool) {
        if down {
            self.keys.insert(vk);
        } else {
            self.keys.remove(&vk);
        }
        input.key(vk, down, self.mask());
    }

    pub fn button(&mut self, input: &Input, b: i32, down: bool) {
        if down {
            self.buttons.insert(b);
        } else {
            self.buttons.remove(&b);
        }
        input.mouse_button(b, down);
    }

    /// Turn modifier changes into key presses. Cmd maps to Ctrl or the
    /// Windows key.
    pub fn modifiers(&mut self, input: &Input, now: Modifiers, cmd_is_ctrl: bool) {
        for (vk, down) in self.modifier_changes(now, cmd_is_ctrl) {
            self.key(input, vk, down);
        }
    }

    /// Compare the effective remote keys, since Cmd and Ctrl can share one
    /// key. This also releases the old key if the mapping changes mid-hold.
    fn modifier_changes(&self, now: Modifiers, cmd_is_ctrl: bool) -> Vec<(i16, bool)> {
        [
            (VK_SHIFT, now.shift),
            (VK_CONTROL, now.ctrl || (now.mac_cmd && cmd_is_ctrl)),
            (VK_MENU, now.alt),
            (VK_LWIN, now.mac_cmd && !cmd_is_ctrl),
        ]
        .into_iter()
        .filter(|(vk, down)| self.keys.contains(vk) != *down)
        .collect()
    }

    /// The modifier byte Moonlight wants alongside every key event.
    pub fn mask(&self) -> c_char {
        [
            (VK_SHIFT, MODIFIER_SHIFT),
            (VK_CONTROL, MODIFIER_CTRL),
            (VK_MENU, MODIFIER_ALT),
            (VK_LWIN, MODIFIER_META),
        ]
        .into_iter()
        .filter(|(vk, _)| self.keys.contains(vk))
        .fold(0, |mask, (_, bit)| mask | bit)
    }

    pub fn release_all(&mut self, input: &Input) {
        for k in std::mem::take(&mut self.keys) {
            input.key(k, false, 0);
        }
        for b in std::mem::take(&mut self.buttons) {
            input.mouse_button(b, false);
        }
    }

    /// Press a chord and release it, e.g. Ctrl+Alt+Del. A key that is
    /// already down (⌘ held while ⌘C, ⌘V follow each other) is left down:
    /// releasing it here would make the next shortcut in the same hold
    /// arrive without its modifier.
    pub fn chord(&mut self, input: &Input, keys: &[i16]) {
        let fresh = self.chord_plan(keys);
        press_chord(input, &fresh, self.mask());
    }

    /// [`Held::chord`] for a chord that must arrive with no other modifier:
    /// held modifiers it does not use are let go for it and pressed again
    /// after, so the PC ends up holding what the user holds.
    pub fn chord_alone(&mut self, input: &Input, keys: &[i16]) {
        send(input, &self.chord_alone_events(keys));
    }

    fn chord_alone_events(&self, keys: &[i16]) -> Vec<KeyEvent> {
        alone_events(keys, &self.chord_plan(keys), self.mask(), true)
    }

    /// The keys of `keys` a chord would have to press: those not held.
    pub fn chord_plan(&self, keys: &[i16]) -> Vec<i16> {
        keys.iter()
            .copied()
            .filter(|k| !self.keys.contains(k))
            .collect()
    }

    pub fn any_down(&self) -> bool {
        !self.keys.is_empty() || !self.buttons.is_empty()
    }

    pub fn is_down(&self, vk: i16) -> bool {
        self.keys.contains(&vk)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn command_and_control_share_one_remote_control_key() {
        let mut held = Held::default();
        held.keys.insert(VK_CONTROL);
        // Releasing either physical key must leave Ctrl down for the other.
        for now in [
            Modifiers {
                ctrl: true,
                ..Modifiers::NONE
            },
            Modifiers {
                mac_cmd: true,
                ..Modifiers::NONE
            },
            Modifiers {
                ctrl: true,
                mac_cmd: true,
                ..Modifiers::NONE
            },
        ] {
            assert!(held.modifier_changes(now, true).is_empty());
        }
        assert_eq!(
            held.modifier_changes(Modifiers::NONE, true),
            [(VK_CONTROL, false)]
        );
    }

    #[test]
    fn changing_command_mapping_releases_the_previous_remote_key() {
        let mut held = Held::default();
        held.keys.insert(VK_CONTROL);
        let command = Modifiers {
            mac_cmd: true,
            ..Modifiers::NONE
        };
        assert_eq!(
            held.modifier_changes(command, false),
            [(VK_CONTROL, false), (VK_LWIN, true)]
        );
        held.keys.clear();
        held.keys.insert(VK_LWIN);
        assert_eq!(
            held.modifier_changes(command, true),
            [(VK_CONTROL, true), (VK_LWIN, false)]
        );
    }

    #[test]
    fn a_chord_leaves_held_modifiers_alone() {
        let mut held = Held::default();
        // ⌘ is down (as Ctrl); ⌘C then ⌘V arrive without ⌘ being released.
        held.keys.insert(VK_CONTROL);
        assert_eq!(held.chord_plan(&[VK_CONTROL, VK_C]), vec![VK_C]);
        assert_eq!(held.chord_plan(&[VK_CONTROL, VK_V]), vec![VK_V]);
        assert_eq!(held.mask(), MODIFIER_CTRL);
        // Nothing held: the whole chord is pressed.
        let none = Held::default();
        assert_eq!(
            none.chord_plan(&[VK_CONTROL, VK_MENU, VK_DELETE]),
            vec![VK_CONTROL, VK_MENU, VK_DELETE]
        );
        assert_eq!(
            mask_of(&[VK_CONTROL, VK_MENU]),
            MODIFIER_CTRL | MODIFIER_ALT
        );
        assert_eq!(mask_of(&[VK_LWIN]), MODIFIER_META);
        assert_eq!(mask_of(&[VK_C]), 0);
    }

    #[test]
    fn a_chord_alone_lets_go_of_other_modifiers_and_restores_them() {
        // ⌘ is down as Ctrl. Copy on Linux is Ctrl+Insert: Ctrl stays down.
        let mut held = Held::default();
        held.keys.insert(VK_CONTROL);
        assert_eq!(
            held.chord_alone_events(&[VK_CONTROL, VK_INSERT]),
            vec![
                (VK_INSERT, true, MODIFIER_CTRL),
                (VK_INSERT, false, MODIFIER_CTRL)
            ]
        );
        // Cut is Shift+Delete: Ctrl+Shift+Delete would be another shortcut
        // (Firefox clears history), so Ctrl goes up for it and back down.
        assert_eq!(
            held.chord_alone_events(&[VK_SHIFT, VK_DELETE]),
            vec![
                (VK_CONTROL, false, 0),
                (VK_SHIFT, true, MODIFIER_SHIFT),
                (VK_DELETE, true, MODIFIER_SHIFT),
                (VK_DELETE, false, MODIFIER_SHIFT),
                (VK_SHIFT, false, 0),
                (VK_CONTROL, true, MODIFIER_CTRL),
            ]
        );
        // ⌘ as the Windows key: Super+Ctrl+Insert would copy nothing.
        let mut held = Held::default();
        held.keys.insert(VK_LWIN);
        assert_eq!(
            held.chord_alone_events(&[VK_CONTROL, VK_INSERT]),
            vec![
                (VK_LWIN, false, 0),
                (VK_CONTROL, true, MODIFIER_CTRL),
                (VK_INSERT, true, MODIFIER_CTRL),
                (VK_INSERT, false, MODIFIER_CTRL),
                (VK_CONTROL, false, 0),
                (VK_LWIN, true, MODIFIER_META),
            ]
        );
    }

    #[test]
    fn a_paste_alone_from_the_worker_presses_the_whole_chord_and_restores_nothing() {
        // The mask is the worker's copy from when ⌘V arrived; the window
        // puts Ctrl back once the paste is counted.
        assert_eq!(
            alone_events(
                &[VK_SHIFT, VK_INSERT],
                &[VK_SHIFT, VK_INSERT],
                MODIFIER_CTRL,
                false
            ),
            vec![
                (VK_CONTROL, false, 0),
                (VK_SHIFT, true, MODIFIER_SHIFT),
                (VK_INSERT, true, MODIFIER_SHIFT),
                (VK_INSERT, false, MODIFIER_SHIFT),
                (VK_SHIFT, false, 0),
            ]
        );
        assert_eq!(
            alone_events(&[VK_CONTROL, VK_V], &[VK_CONTROL, VK_V], 0, false),
            chord_events(&[VK_CONTROL, VK_V], 0),
            "nothing held: the same as a plain chord"
        );
    }

    #[test]
    fn letters_digits_and_symbols_map_to_us_layout_codes() {
        assert_eq!(vk(Key::A), Some(0x41));
        assert_eq!(vk(Key::Z), Some(0x5A));
        assert_eq!(vk(Key::Num0), Some(0x30));
        assert_eq!(vk(Key::F12), Some(0x7B));
        assert_eq!(vk(Key::Colon), vk(Key::Semicolon));
        assert_eq!(vk(Key::Copy), None, "clipboard keys are handled as events");
    }
}
