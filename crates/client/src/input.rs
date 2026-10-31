//! Keyboard and mouse translation: egui events in, Latch input calls out.

use egui::{Key, Modifiers, PointerButton};
use latch_stream::input::{
    BUTTON_BACK, BUTTON_FORWARD, BUTTON_LEFT, BUTTON_MIDDLE, BUTTON_RIGHT, MOD_ALT, MOD_CTRL,
    MOD_META, MOD_SHIFT,
};
use latch_stream::Input;
use std::collections::BTreeSet;

pub const VK_SHIFT: i16 = 0x10;
pub const VK_CONTROL: i16 = 0x11;
pub const VK_MENU: i16 = 0x12;
pub const VK_LWIN: i16 = 0x5B;
pub const VK_INSERT: i16 = 0x2D;
pub const VK_DELETE: i16 = 0x2E;
pub const VK_ESCAPE: i16 = 0x1B;
pub const VK_TAB: i16 = 0x09;
pub const VK_CAPITAL: i16 = 0x14;
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
        Equals => 0xBB,
        // egui's Plus comes from the keypad's + key alone (the main row's
        // key is Equals): VK_ADD types "+" there, VK_OEM_PLUS typed "=".
        Plus => 0x6B,
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

pub fn button(b: PointerButton) -> Option<u8> {
    Some(match b {
        PointerButton::Primary => BUTTON_LEFT,
        PointerButton::Secondary => BUTTON_RIGHT,
        PointerButton::Middle => BUTTON_MIDDLE,
        PointerButton::Extra1 => BUTTON_BACK,
        PointerButton::Extra2 => BUTTON_FORWARD,
    })
}

/// Where keyboard and mouse events go: the PC, through Latch, or in
/// tests a recording, so the order and masks the PC would see can be
/// checked.
pub trait Sink {
    /// `vk` is a Windows virtual-key code; `mask` the `MOD_*` bits.
    fn key(&self, vk: i16, down: bool, mask: u8);
    /// `b` is one of the `BUTTON_*` codes.
    fn button(&self, b: u8, down: bool);
    /// An absolute position in a `w` by `h` picture.
    fn position(&self, x: i16, y: i16, w: i16, h: i16);
    /// Relative movement, while the mouse is captured.
    fn motion(&self, dx: i16, dy: i16);
    /// In 1/120ths of a wheel click; positive is up and right.
    fn scroll(&self, vertical: i16, horizontal: i16);
    fn release_all(&self);
    fn text(&self, text: &str);
}

impl Sink for Input {
    fn release_all(&self) {
        Input::release_all(self);
    }
    fn key(&self, vk: i16, down: bool, mask: u8) {
        Input::key(self, vk, down, mask);
    }
    fn button(&self, b: u8, down: bool) {
        self.mouse_button(b, down);
    }
    fn position(&self, x: i16, y: i16, w: i16, h: i16) {
        self.mouse_position(x, y, w, h);
    }
    fn motion(&self, dx: i16, dy: i16) {
        self.mouse_move(dx, dy);
    }
    fn scroll(&self, vertical: i16, horizontal: i16) {
        Input::scroll(self, vertical, horizontal);
    }
    fn text(&self, text: &str) {
        Input::text(self, text);
    }
}

/// What a [`Recorder`] saw, in order.
#[cfg(test)]
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Sent {
    Key(i16, bool, u8),
    Button(u8, bool),
    Position(i16, i16, i16, i16),
    Motion(i16, i16),
    Scroll(i16, i16),
    ReleaseAll,
    Text(String),
}

/// A [`Sink`] that keeps what it is sent, and says whether the stream is
/// connected. Like [`Input`], it drops everything while it is not.
#[cfg(test)]
#[derive(Debug, Default)]
pub struct Recorder {
    pub sent: std::cell::RefCell<Vec<Sent>>,
    pub connected: std::cell::Cell<bool>,
}

#[cfg(test)]
impl Recorder {
    fn record(&self, s: Sent) {
        if self.connected.get() {
            self.sent.borrow_mut().push(s);
        }
    }

    pub fn take(&self) -> Vec<Sent> {
        std::mem::take(&mut self.sent.borrow_mut())
    }

    /// Only the key events, as (key, down).
    pub fn keys(&self) -> Vec<(i16, bool)> {
        self.take()
            .into_iter()
            .filter_map(|s| match s {
                Sent::Key(vk, down, _) => Some((vk, down)),
                _ => None,
            })
            .collect()
    }
}

#[cfg(test)]
impl Sink for Recorder {
    fn release_all(&self) {
        self.record(Sent::ReleaseAll);
    }
    fn key(&self, vk: i16, down: bool, mask: u8) {
        self.record(Sent::Key(vk, down, mask));
    }
    fn button(&self, b: u8, down: bool) {
        self.record(Sent::Button(b, down));
    }
    fn position(&self, x: i16, y: i16, w: i16, h: i16) {
        self.record(Sent::Position(x, y, w, h));
    }
    fn motion(&self, dx: i16, dy: i16) {
        self.record(Sent::Motion(dx, dy));
    }
    fn scroll(&self, vertical: i16, horizontal: i16) {
        self.record(Sent::Scroll(vertical, horizontal));
    }
    fn text(&self, text: &str) {
        self.record(Sent::Text(text.to_string()));
    }
}

/// One key event as Latch takes it: the key, down or up, and the
/// modifier mask sent with it.
type KeyEvent = (i16, bool, u8);

/// The modifier keys, in the order their mask bits are listed.
const MODIFIERS: [i16; 4] = [VK_SHIFT, VK_CONTROL, VK_MENU, VK_LWIN];

fn send(out: &(impl Sink + ?Sized), events: &[KeyEvent]) {
    for &(vk, down, mask) in events {
        out.key(vk, down, mask);
    }
}

/// Press `keys` in order and release them in reverse, with `mask` for the
/// modifiers already held. Usable from any thread with an [`Input`], which
/// is what typing on the PC from a worker needs.
pub fn press_chord(out: &(impl Sink + ?Sized), keys: &[i16], mask: u8) {
    send(out, &chord_events(keys, mask));
}

fn chord_events(keys: &[i16], mask: u8) -> Vec<KeyEvent> {
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

/// The events that send `keys` with no other modifier down: modifiers in
/// `mask` outside `keys` are released first, so ⌘ held as Ctrl does not
/// turn Shift+Insert into Ctrl+Shift+Insert, and pressed again after.
/// `press` is the part of `keys` to press: those not already held.
fn alone_events(keys: &[i16], press: &[i16], mask: u8) -> Vec<KeyEvent> {
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
    for &vk in &others {
        mask |= mask_of(&[vk]);
        events.push((vk, true, mask));
    }
    events
}

fn mask_of(keys: &[i16]) -> u8 {
    let mut m: u8 = 0;
    if keys.contains(&VK_SHIFT) {
        m |= MOD_SHIFT;
    }
    if keys.contains(&VK_CONTROL) {
        m |= MOD_CTRL;
    }
    if keys.contains(&VK_MENU) {
        m |= MOD_ALT;
    }
    if keys.contains(&VK_LWIN) {
        m |= MOD_META;
    }
    m
}

/// Which keys and buttons the PC currently believes are down, so they can all
/// be released when focus or capture is lost.
#[derive(Default)]
pub struct Held {
    keys: BTreeSet<i16>,
    buttons: BTreeSet<u8>,
}

impl Held {
    pub fn key(&mut self, out: &(impl Sink + ?Sized), vk: i16, down: bool) {
        if down {
            self.keys.insert(vk);
        } else {
            self.keys.remove(&vk);
        }
        out.key(vk, down, self.mask());
    }

    pub fn button(&mut self, out: &(impl Sink + ?Sized), b: u8, down: bool) {
        if down {
            self.buttons.insert(b);
        } else {
            self.buttons.remove(&b);
        }
        out.button(b, down);
    }

    /// Turn modifier changes into key presses. Cmd maps to Ctrl or the
    /// Windows key.
    pub fn modifiers(&mut self, out: &(impl Sink + ?Sized), now: Modifiers, cmd_is_ctrl: bool) {
        for (vk, down) in self.modifier_changes(now, cmd_is_ctrl) {
            self.key(out, vk, down);
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

    /// The modifier byte Latch wants alongside every key event.
    pub fn mask(&self) -> u8 {
        [
            (VK_SHIFT, MOD_SHIFT),
            (VK_CONTROL, MOD_CTRL),
            (VK_MENU, MOD_ALT),
            (VK_LWIN, MOD_META),
        ]
        .into_iter()
        .filter(|(vk, _)| self.keys.contains(vk))
        .fold(0, |mask, (_, bit)| mask | bit)
    }

    pub fn release_all(&mut self, out: &(impl Sink + ?Sized)) {
        for k in std::mem::take(&mut self.keys) {
            out.key(k, false, 0);
        }
        for b in std::mem::take(&mut self.buttons) {
            out.button(b, false);
        }
        out.release_all();
    }

    /// Press a chord and release it, e.g. Ctrl+Alt+Del. A key that is
    /// already down (⌘ held while ⌘C, ⌘V follow each other) is left down:
    /// releasing it here would make the next shortcut in the same hold
    /// arrive without its modifier.
    pub fn chord(&mut self, out: &(impl Sink + ?Sized), keys: &[i16]) {
        let fresh = self.chord_plan(keys);
        press_chord(out, &fresh, self.mask());
    }

    /// [`Held::chord`] for a chord that must arrive with no other modifier:
    /// held modifiers it does not use are let go for it and pressed again
    /// after, so the PC ends up holding what the user holds.
    pub fn chord_alone(&mut self, out: &(impl Sink + ?Sized), keys: &[i16]) {
        send(out, &self.chord_alone_events(keys));
    }

    fn chord_alone_events(&self, keys: &[i16]) -> Vec<KeyEvent> {
        alone_events(keys, &self.chord_plan(keys), self.mask())
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
    fn releasing_sends_key_ups_and_a_host_wide_release() {
        let out = Recorder::default();
        out.connected.set(true);
        let mut held = Held::default();
        held.key(&out, VK_CONTROL, true);
        held.button(&out, BUTTON_LEFT, true);
        out.take();
        held.release_all(&out);
        assert_eq!(
            out.take(),
            [
                Sent::Key(VK_CONTROL, false, 0),
                Sent::Button(BUTTON_LEFT, false),
                Sent::ReleaseAll
            ]
        );
        assert!(!held.any_down());
        held.release_all(&out);
        assert_eq!(
            out.take(),
            [Sent::ReleaseAll],
            "also clears keys this view did not track"
        );
    }

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
        assert_eq!(held.mask(), MOD_CTRL);
        // Nothing held: the whole chord is pressed.
        let none = Held::default();
        assert_eq!(
            none.chord_plan(&[VK_CONTROL, VK_MENU, VK_DELETE]),
            vec![VK_CONTROL, VK_MENU, VK_DELETE]
        );
        assert_eq!(mask_of(&[VK_CONTROL, VK_MENU]), MOD_CTRL | MOD_ALT);
        assert_eq!(mask_of(&[VK_LWIN]), MOD_META);
        assert_eq!(mask_of(&[VK_C]), 0);
    }

    #[test]
    fn a_chord_alone_lets_go_of_other_modifiers_and_restores_them() {
        // ⌘ is down as Ctrl. Copy on Linux is Ctrl+Insert: Ctrl stays down.
        let mut held = Held::default();
        held.keys.insert(VK_CONTROL);
        assert_eq!(
            held.chord_alone_events(&[VK_CONTROL, VK_INSERT]),
            vec![(VK_INSERT, true, MOD_CTRL), (VK_INSERT, false, MOD_CTRL)]
        );
        // Cut is Shift+Delete: Ctrl+Shift+Delete would be another shortcut
        // (Firefox clears history), so Ctrl goes up for it and back down.
        assert_eq!(
            held.chord_alone_events(&[VK_SHIFT, VK_DELETE]),
            vec![
                (VK_CONTROL, false, 0),
                (VK_SHIFT, true, MOD_SHIFT),
                (VK_DELETE, true, MOD_SHIFT),
                (VK_DELETE, false, MOD_SHIFT),
                (VK_SHIFT, false, 0),
                (VK_CONTROL, true, MOD_CTRL),
            ]
        );
        // ⌘ as the Windows key: Super+Ctrl+Insert would copy nothing.
        let mut held = Held::default();
        held.keys.insert(VK_LWIN);
        assert_eq!(
            held.chord_alone_events(&[VK_CONTROL, VK_INSERT]),
            vec![
                (VK_LWIN, false, 0),
                (VK_CONTROL, true, MOD_CTRL),
                (VK_INSERT, true, MOD_CTRL),
                (VK_INSERT, false, MOD_CTRL),
                (VK_CONTROL, false, 0),
                (VK_LWIN, true, MOD_META),
            ]
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
        // The keypad's + is VK_ADD; the main row's =/+ key is VK_OEM_PLUS.
        assert_eq!(vk(Key::Plus), Some(0x6B));
        assert_eq!(vk(Key::Equals), Some(0xBB));
    }
}
