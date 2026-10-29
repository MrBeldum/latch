//! Build CoreGraphics events separately from posting so tests cannot control the owner's desktop.

use super::ffi::{Owned, Ref};
use crate::input::{DisplayRect, Event, Held, InputSink};
use anyhow::{bail, ensure, Result};
use std::ptr;

#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq)]
struct Point {
    x: f64,
    y: f64,
}
#[link(name = "ApplicationServices", kind = "framework")]
extern "C" {
    fn AXIsProcessTrusted() -> bool;
}
#[link(name = "CoreGraphics", kind = "framework")]
extern "C" {
    fn CGEventCreate(source: Ref) -> Ref;
    fn CGEventGetLocation(event: Ref) -> Point;
    fn CGEventCreateMouseEvent(source: Ref, kind: u32, point: Point, button: u32) -> Ref;
    fn CGEventCreateKeyboardEvent(source: Ref, key: u16, down: bool) -> Ref;
    fn CGEventCreateScrollWheelEvent2(
        source: Ref,
        units: u32,
        count: u32,
        vertical: i32,
        horizontal: i32,
        z: i32,
    ) -> Ref;
    fn CGEventSetFlags(event: Ref, flags: u64);
    fn CGEventSetIntegerValueField(event: Ref, field: u32, value: i64);
    fn CGEventKeyboardSetUnicodeString(event: Ref, length: usize, text: *const u16);
    fn CGEventPost(tap: u32, event: Ref);
    #[cfg(test)]
    fn CGEventGetType(event: Ref) -> u32;
    #[cfg(test)]
    fn CGEventGetFlags(event: Ref) -> u64;
    #[cfg(test)]
    fn CGEventGetIntegerValueField(event: Ref, field: u32) -> i64;
}
pub fn open() -> Result<Box<dyn InputSink>> {
    // SAFETY: this only queries authorization and never shows an authorization prompt.
    ensure!(unsafe { AXIsProcessTrusted() }, "Latch needs Accessibility permission to control this Mac: System Settings → Privacy & Security → Accessibility");
    Ok(Box::new(Sink {
        held: Held::default(),
        caps_lock: false,
        posting: true,
    }))
}
struct Sink {
    held: Held,
    caps_lock: bool,
    posting: bool,
}
fn clamp(point: Point, display: DisplayRect) -> Point {
    Point {
        x: point.x.clamp(
            f64::from(display.x),
            f64::from(display.x) + f64::from(display.width.saturating_sub(1)),
        ),
        y: point.y.clamp(
            f64::from(display.y),
            f64::from(display.y) + f64::from(display.height.saturating_sub(1)),
        ),
    }
}
fn button_number(button: u8) -> Result<u32> {
    use latch_proto::input::*;
    match button {
        BUTTON_LEFT => Ok(0),
        BUTTON_RIGHT => Ok(1),
        BUTTON_MIDDLE => Ok(2),
        BUTTON_BACK => Ok(3),
        BUTTON_FORWARD => Ok(4),
        _ => bail!("unsupported mouse button {button}"),
    }
}
fn button_type(button: u32, down: bool) -> u32 {
    match (button, down) {
        (0, true) => 1,
        (0, false) => 2,
        (1, true) => 3,
        (1, false) => 4,
        (_, true) => 25,
        (_, false) => 26,
    }
}
impl Sink {
    fn flags(&self) -> u64 {
        let has = |codes: &[u16]| codes.iter().any(|c| self.held.keys.contains(c));
        let mut flags = 0;
        if has(&[0x10, 0xa0, 0xa1]) {
            flags |= 1 << 17;
        }
        if has(&[0x11, 0xa2, 0xa3]) {
            flags |= 1 << 18;
        }
        if has(&[0x12, 0xa4, 0xa5]) {
            flags |= 1 << 19;
        }
        if has(&[0x5b, 0x5c]) {
            flags |= 1 << 20;
        }
        if self.caps_lock {
            flags |= 1 << 16;
        }
        flags
    }
    fn build(&mut self, event: &Event, display: DisplayRect, current: Point) -> Result<Vec<Owned>> {
        let mut events = Vec::new();
        // SAFETY: each Create result becomes an Owned CF object. No event is posted here; all
        // text pointers remain live until CoreGraphics has copied their contents.
        unsafe {
            match event {
                Event::MouseMove { dx, dy } => {
                    let point = clamp(
                        Point {
                            x: current.x + f64::from(*dx),
                            y: current.y + f64::from(*dy),
                        },
                        display,
                    );
                    let e = self.movement(point)?;
                    CGEventSetIntegerValueField(e.0, 4, i64::from(*dx));
                    CGEventSetIntegerValueField(e.0, 5, i64::from(*dy));
                    events.push(e);
                }
                Event::MousePosition {
                    x,
                    y,
                    width,
                    height,
                } => {
                    let (x, y) = display.map(*x, *y, *width, *height);
                    events.push(self.movement(clamp(
                        Point {
                            x: x as f64,
                            y: y as f64,
                        },
                        display,
                    ))?);
                }
                Event::MouseButton { button, down } => {
                    let number = button_number(*button)?;
                    let e = Owned::new(CGEventCreateMouseEvent(
                        ptr::null(),
                        button_type(number, *down),
                        clamp(current, display),
                        number,
                    ))?;
                    if self.held.note(event) {
                        events.push(e);
                    }
                }
                Event::Key { vk, down, .. } => {
                    if let Some(key) = crate::input::keymap::vk_to_mac(*vk) {
                        let e = Owned::new(CGEventCreateKeyboardEvent(ptr::null(), key, *down))?;
                        if *vk == 0x14 && *down && !self.held.keys.contains(vk) {
                            self.caps_lock = !self.caps_lock;
                        }
                        if self.held.note(event) {
                            events.push(e);
                        }
                    }
                }
                Event::Scroll {
                    vertical,
                    horizontal,
                } => {
                    // One conventional 120-unit wheel notch becomes 40 pixels. Keep tiny trackpad
                    // deltas nonzero instead of losing them to integer rounding.
                    let pixels = |v: i16| {
                        let v = i32::from(v);
                        if v == 0 {
                            0
                        } else {
                            (v / 3).abs().max(1) * v.signum()
                        }
                    };
                    events.push(Owned::new(CGEventCreateScrollWheelEvent2(
                        ptr::null(),
                        0,
                        2,
                        pixels(*vertical),
                        pixels(*horizontal),
                        0,
                    ))?);
                }
                Event::Text(text) => {
                    let units: Vec<u16> = text.encode_utf16().collect();
                    let mut start = 0;
                    while start < units.len() {
                        let mut end = (start + 20).min(units.len());
                        if end < units.len() && (0xd800..=0xdbff).contains(&units[end - 1]) {
                            end -= 1;
                        }
                        for down in [true, false] {
                            let e = Owned::new(CGEventCreateKeyboardEvent(ptr::null(), 0, down))?;
                            CGEventKeyboardSetUnicodeString(
                                e.0,
                                end - start,
                                units[start..end].as_ptr(),
                            );
                            events.push(e);
                        }
                        start = end;
                    }
                }
                Event::ReleaseAll => {
                    unreachable!("release_all is handled before event construction")
                }
            }
            for e in &events {
                CGEventSetFlags(e.0, self.flags());
            }
        }
        Ok(events)
    }
    fn movement(&self, point: Point) -> Result<Owned> {
        let mut kind = 5;
        let mut number = 0;
        for button in &self.held.buttons {
            if let Ok(b) = button_number(*button) {
                number = b;
                kind = match b {
                    0 => 6,
                    1 => 7,
                    _ => 27,
                };
                break;
            }
        }
        // SAFETY: this only constructs an event; the type/button combination agrees with CoreGraphics.
        Owned::new(unsafe { CGEventCreateMouseEvent(ptr::null(), kind, point, number) })
    }
    fn current() -> Result<Point> {
        // SAFETY: reading the cursor location does not inject input. Owned balances the Create.
        unsafe {
            let event = Owned::new(CGEventCreate(ptr::null()))?;
            Ok(CGEventGetLocation(event.0))
        }
    }
}
impl InputSink for Sink {
    fn apply(&mut self, event: &Event, display: DisplayRect) -> Result<()> {
        if matches!(event, Event::ReleaseAll) {
            self.release_all();
            return Ok(());
        }
        let current = Self::current()?;
        let events = self.build(event, display, current)?;
        if self.posting {
            for event in events {
                // SAFETY: only an authorized production sink posts constructed events to kCGHIDEventTap.
                unsafe { CGEventPost(0, event.0) }
            }
        }
        Ok(())
    }
    fn release_all(&mut self) {
        let mut releases: Vec<_> = self
            .held
            .keys
            .iter()
            .map(|vk| Event::Key {
                vk: *vk,
                down: false,
                modifiers: 0,
            })
            .collect();
        releases.extend(self.held.buttons.iter().map(|button| Event::MouseButton {
            button: *button,
            down: false,
        }));
        // Releases preserve the cursor position even if the main display has since changed.
        let display = DisplayRect {
            x: i32::MIN / 2,
            y: i32::MIN / 2,
            width: i32::MAX as u32,
            height: i32::MAX as u32,
        };
        for event in releases {
            if let Err(error) = self.apply(&event, display) {
                tracing::warn!(%error, "release macOS input");
            }
        }
    }
}
impl Drop for Sink {
    fn drop(&mut self) {
        if self.posting {
            self.release_all();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn sink() -> Sink {
        Sink {
            held: Held::default(),
            caps_lock: false,
            posting: false,
        }
    }
    fn display() -> DisplayRect {
        DisplayRect {
            x: -100,
            y: 20,
            width: 200,
            height: 100,
        }
    }
    #[test]
    fn constructs_moves_drags_buttons_and_scroll_without_posting() {
        let mut sink = sink();
        let origin = Point { x: 0., y: 40. };
        let event = sink
            .build(&Event::MouseMove { dx: 300, dy: -100 }, display(), origin)
            .unwrap()
            .pop()
            .unwrap();
        // SAFETY: test only reads properties of locally constructed events and never posts them.
        unsafe {
            assert_eq!(CGEventGetType(event.0), 5);
            assert_eq!(CGEventGetLocation(event.0), Point { x: 99., y: 20. });
            assert_eq!(CGEventGetIntegerValueField(event.0, 4), 300);
            assert_eq!(CGEventGetIntegerValueField(event.0, 5), -100);
            for button in 1..=5 {
                let down = sink
                    .build(
                        &Event::MouseButton { button, down: true },
                        display(),
                        origin,
                    )
                    .unwrap();
                assert_eq!(
                    CGEventGetType(down[0].0),
                    button_type(button_number(button).unwrap(), true)
                );
                let moved = sink
                    .build(&Event::MouseMove { dx: 1, dy: 1 }, display(), origin)
                    .unwrap();
                assert_eq!(
                    CGEventGetType(moved[0].0),
                    match button {
                        1 => 6,
                        3 => 7,
                        _ => 27,
                    }
                );
                sink.build(
                    &Event::MouseButton {
                        button,
                        down: false,
                    },
                    display(),
                    origin,
                )
                .unwrap();
            }
            let scroll = sink
                .build(
                    &Event::Scroll {
                        vertical: 120,
                        horizontal: -120,
                    },
                    display(),
                    origin,
                )
                .unwrap();
            assert_eq!(CGEventGetIntegerValueField(scroll[0].0, 96), 40);
            assert_eq!(CGEventGetIntegerValueField(scroll[0].0, 97), -40);
            let absolute = sink
                .build(
                    &Event::MousePosition {
                        x: 100,
                        y: 50,
                        width: 200,
                        height: 100,
                    },
                    display(),
                    origin,
                )
                .unwrap();
            assert_eq!(CGEventGetLocation(absolute[0].0), Point { x: 0., y: 70. });
        }
    }
    #[test]
    fn unicode_and_modifier_flags_without_keymap_or_posting() {
        let mut sink = sink();
        sink.held.keys.extend([0xa0, 0xa1, 0xa2, 0xa4, 0x5b]);
        let events = sink
            .build(
                &Event::Text(format!("{}😀x", "a".repeat(19))),
                display(),
                Point::default(),
            )
            .unwrap();
        assert_eq!(events.len(), 4);
        // SAFETY: inspection only; these events are never posted.
        unsafe {
            assert_eq!(CGEventGetFlags(events[0].0), 0x1e0000);
        }
        sink.held.keys.remove(&0xa0);
        assert_ne!(sink.flags() & (1 << 17), 0);
        sink.held.keys.remove(&0xa1);
        assert_eq!(sink.flags() & (1 << 17), 0);
        assert_eq!(
            clamp(Point { x: -1e9, y: 1e9 }, display()),
            Point { x: -100., y: 119. }
        );
    }
}
