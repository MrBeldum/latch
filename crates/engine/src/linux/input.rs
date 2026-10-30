//! XTest uses server keycodes, cached from the current keyboard mapping.
//! Text borrows an unused keycode and leaves time for clients to consume
//! each mapping before restoring it. Grabbing the server would prevent
//! their MappingNotify handlers from fetching the temporary keysym.

use super::X11;
use crate::input::{DisplayRect, Event, Held, InputSink};
use anyhow::{ensure, Context};
use std::collections::BTreeMap;
use x11rb::connection::Connection;
use x11rb::protocol::xproto::ConnectionExt as _;
use x11rb::protocol::{xkb, xproto, xtest, Event as XEvent};

pub(super) struct X11Input {
    conn: X11,
    root: u32,
    mapping: xproto::GetKeyboardMappingReply,
    min_keycode: u8,
    key_names: xkb::GetNamesReply,
    held: Held,
    pressed: BTreeMap<u16, u8>,
    scroll: [i32; 2],
}
impl X11Input {
    pub(super) fn new() -> anyhow::Result<Self> {
        let (conn, screen) =
            super::connect(super::X11_SILENCE).context("connect to X11 for XTest")?;
        xtest::get_version(&conn, 2, 2)?.reply()?;
        ensure!(
            xkb::use_extension(&conn, 1, 0)?.reply()?.supported,
            "XKB unavailable"
        );
        let key_names = xkb::get_names(
            &conn,
            xkb::ID::USE_CORE_KBD.into(),
            xkb::NameDetail::KEY_NAMES,
        )?
        .reply()?;
        let setup = conn.setup();
        let root = setup.roots[screen].root;
        let min_keycode = setup.min_keycode;
        let mapping = conn
            .get_keyboard_mapping(min_keycode, setup.max_keycode - min_keycode + 1)?
            .reply()?;
        Ok(Self {
            conn,
            root,
            mapping,
            min_keycode,
            key_names,
            held: Held::default(),
            pressed: BTreeMap::new(),
            scroll: [0; 2],
        })
    }
    fn refresh(&mut self) -> anyhow::Result<()> {
        let mut changed = false;
        while let Some(event) = self.conn.poll_for_event()? {
            match event {
                XEvent::MappingNotify(_) => changed = true,
                XEvent::Error(error) => anyhow::bail!("X11 input error: {error:?}"),
                _ => {}
            }
        }
        if changed {
            self.key_names = xkb::get_names(
                &self.conn,
                xkb::ID::USE_CORE_KBD.into(),
                xkb::NameDetail::KEY_NAMES,
            )?
            .reply()?;
            self.mapping = self
                .conn
                .get_keyboard_mapping(
                    self.min_keycode,
                    self.conn.setup().max_keycode - self.min_keycode + 1,
                )?
                .reply()?;
        }
        Ok(())
    }
    fn fake(&self, kind: u8, detail: u8, x: i16, y: i16) -> anyhow::Result<()> {
        xtest::fake_input(
            &self.conn,
            kind,
            detail,
            x11rb::CURRENT_TIME,
            self.root,
            x,
            y,
            0,
        )?
        .check()?;
        Ok(())
    }
    fn button(&self, button: u8, down: bool) -> anyhow::Result<()> {
        self.fake(
            if down {
                xproto::BUTTON_PRESS_EVENT
            } else {
                xproto::BUTTON_RELEASE_EVENT
            },
            button,
            0,
            0,
        )
    }
    fn text(&mut self, text: &str) -> anyhow::Result<()> {
        let per = self.mapping.keysyms_per_keycode as usize;
        ensure!(per > 0, "empty X keyboard mapping");
        let keys = self.conn.query_keymap()?.reply()?.keys;
        let spare = self
            .mapping
            .keysyms
            .chunks(per)
            .enumerate()
            .rev()
            .find(|(i, symbols)| {
                let code = usize::from(self.min_keycode) + i;
                symbols.iter().all(|&s| s == 0) && keys[code / 8] & (1 << (code % 8)) == 0
            })
            .map(|(i, symbols)| (self.min_keycode + i as u8, symbols.to_vec()))
            .context("no unused X keycode for Unicode text")?;
        let result: anyhow::Result<()> = (|| {
            for ch in text.chars() {
                let mut symbols = vec![0; per];
                // Bind both shift levels to avoid changing text under held Shift.
                symbols.fill(0x01000000 | u32::from(ch));
                self.conn
                    .change_keyboard_mapping(1, spare.0, per as u8, &symbols)?
                    .check()?;
                self.fake(xproto::KEY_PRESS_EVENT, spare.0, 0, 0)?;
                self.fake(xproto::KEY_RELEASE_EVENT, spare.0, 0, 0)?;
                // X11 has no acknowledgement that the focused application has
                // looked up a keycode. Match the pacing used by text injectors.
                std::thread::sleep(std::time::Duration::from_millis(12));
            }
            Ok(())
        })();
        let release = if result.is_err() {
            self.fake(xproto::KEY_RELEASE_EVENT, spare.0, 0, 0)
        } else {
            Ok(())
        };
        let restore: anyhow::Result<()> = (|| {
            self.conn
                .change_keyboard_mapping(1, spare.0, per as u8, &spare.1)?
                .check()?;
            Ok(())
        })();
        result?;
        release?;
        restore?;
        Ok(())
    }
}
fn button_code(button: u8) -> anyhow::Result<u8> {
    match button {
        1..=3 => Ok(button),
        4 => Ok(8),
        5 => Ok(9),
        _ => anyhow::bail!("unknown mouse button {button}"),
    }
}
fn notches(remainder: &mut i32, delta: i16) -> i32 {
    *remainder += i32::from(delta);
    let count = *remainder / 120;
    *remainder %= 120;
    count
}
impl InputSink for X11Input {
    fn apply(&mut self, event: &Event, display: DisplayRect) -> anyhow::Result<()> {
        self.refresh()?;
        match event {
            Event::Key { vk, down, .. } => {
                let code = if let Some(&code) = self.pressed.get(vk) {
                    code
                } else {
                    if !down {
                        return Ok(());
                    }
                    let name = crate::input::keymap::vk_to_xkb_name(*vk)
                        .context("unmapped virtual key")?;
                    self.key_names
                        .value_list
                        .key_names
                        .as_ref()
                        .context("XKB key names unavailable")?
                        .iter()
                        .position(|key| key.name == name)
                        .map(|i| self.key_names.first_key + i as u8)
                        .context("physical key absent from XKB keyboard mapping")?
                };
                self.fake(
                    if *down {
                        xproto::KEY_PRESS_EVENT
                    } else {
                        xproto::KEY_RELEASE_EVENT
                    },
                    code,
                    0,
                    0,
                )?;
                self.held.note(event);
                if *down {
                    self.pressed.insert(*vk, code);
                } else {
                    self.pressed.remove(vk);
                }
            }
            Event::MousePosition {
                x,
                y,
                width,
                height,
            } => {
                let (x, y) = display.map(*x, *y, *width, *height);
                self.fake(
                    xproto::MOTION_NOTIFY_EVENT,
                    0,
                    i16::try_from(x)?,
                    i16::try_from(y)?,
                )?;
            }
            Event::MouseMove { dx, dy } => {
                // XTest MotionNotify detail=1 interprets x/y as relative deltas.
                // Drop the void cookie: errors are handled by refresh, without a round trip.
                drop(xtest::fake_input(
                    &self.conn,
                    xproto::MOTION_NOTIFY_EVENT,
                    1,
                    x11rb::CURRENT_TIME,
                    x11rb::NONE,
                    *dx,
                    *dy,
                    0,
                )?);
            }
            Event::MouseButton { button, down } => {
                let code = button_code(*button)?;
                if *down || self.held.buttons.contains(button) {
                    self.button(code, *down)?;
                    self.held.note(event);
                }
            }
            Event::Scroll {
                vertical,
                horizontal,
            } => {
                for (axis, delta) in [*vertical, *horizontal].into_iter().enumerate() {
                    let count = notches(&mut self.scroll[axis], delta);
                    let button = match (axis, count > 0) {
                        (0, true) => 4,
                        (0, false) => 5,
                        (_, true) => 7,
                        (_, false) => 6,
                    };
                    for _ in 0..count.unsigned_abs() {
                        self.button(button, true)?;
                        self.button(button, false)?;
                    }
                }
            }
            Event::Text(text) => self.text(text)?,
            Event::ReleaseAll => {
                self.release_all();
                ensure!(
                    self.held.keys.is_empty() && self.held.buttons.is_empty(),
                    "input releases still pending"
                );
            }
        }
        self.conn.flush()?;
        Ok(())
    }
    fn release_all(&mut self) {
        for (vk, code) in std::mem::take(&mut self.pressed) {
            if let Err(e) = self.fake(xproto::KEY_RELEASE_EVENT, code, 0, 0) {
                tracing::warn!("release key: {e:#}");
                self.pressed.insert(vk, code);
            } else {
                self.held.keys.remove(&vk);
            }
        }
        for button in std::mem::take(&mut self.held.buttons) {
            if let Ok(code) = button_code(button) {
                if let Err(e) = self.button(code, false) {
                    tracing::warn!("release button: {e:#}");
                    self.held.buttons.insert(button);
                }
            }
        }
        self.scroll = [0; 2];
        let _ = self.conn.flush();
    }
}
impl Drop for X11Input {
    fn drop(&mut self) {
        self.release_all();
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn scroll_remainders_and_buttons() {
        let mut r = 0;
        assert_eq!(notches(&mut r, 60), 0);
        assert_eq!(notches(&mut r, 90), 1);
        assert_eq!(r, 30);
        assert_eq!(notches(&mut r, -270), -2);
        assert_eq!(r, 0);
        assert_eq!(button_code(4).unwrap(), 8);
        assert_eq!(button_code(5).unwrap(), 9);
        assert!(button_code(6).is_err());
    }
}

#[cfg(test)]
mod timeout_tests {
    use super::*;
    use std::io::{Read, Write};
    use std::os::unix::net::UnixStream;
    use std::time::Duration;
    use x11rb::rust_connection::{DefaultStream, RustConnection};
    use x11rb::x11_utils::Serialize;

    #[test]
    fn silent_xtest_fails_instead_of_holding_the_session_input_lock() {
        let (client, mut server) = UnixStream::pair().unwrap();
        let worker = std::thread::spawn(move || {
            let mut request = [0; 12];
            server.read_exact(&mut request).unwrap();
            let mut setup = xproto::Setup {
                status: 1,
                protocol_major_version: 11,
                resource_id_mask: 0xffff,
                roots: vec![xproto::Screen::default()],
                ..Default::default()
            };
            setup.length = ((setup.serialize().len() - 8) / 4) as u16;
            server.write_all(&setup.serialize()).unwrap();
            let mut sequence = 0;
            loop {
                let mut header = [0; 4];
                if server.read_exact(&mut header).is_err() {
                    break;
                }
                let len = usize::from(u16::from_ne_bytes([header[2], header[3]])) * 4;
                server.read_exact(&mut vec![0; len - 4]).unwrap();
                sequence += 1;
                if header[0] == 98 {
                    let mut reply = xproto::QueryExtensionReply {
                        sequence,
                        present: true,
                        major_opcode: 140,
                        ..Default::default()
                    }
                    .serialize()
                    .to_vec();
                    reply.resize(32, 0);
                    server.write_all(&reply).unwrap();
                }
                // Leave XTest and its synchronization request unanswered.
            }
        });
        let (stream, _) = DefaultStream::from_unix_stream(client).unwrap();
        let conn = RustConnection::connect_to_stream(
            super::super::Bounded {
                stream,
                silence: Duration::from_millis(100),
            },
            0,
        )
        .unwrap();
        let input = X11Input {
            conn,
            root: 0,
            mapping: Default::default(),
            min_keycode: 8,
            key_names: Default::default(),
            held: Default::default(),
            pressed: Default::default(),
            scroll: [0; 2],
        };
        assert!(input.fake(xproto::KEY_PRESS_EVENT, 8, 0, 0).is_err());
        drop(input);
        worker.join().unwrap();
    }
}
