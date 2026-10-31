//! X11 pixels stay in shared memory until a completed GetImage reply makes
//! them safe to read. Damage bounds limit copies; the cursor never dirties
//! the saved desktop, so moving it restores the old pixels automatically.

use crate::input::DisplayRect;
use crate::software::{Bgra, CpuCapture};
use anyhow::{ensure, Context};
use std::os::fd::AsRawFd;
use std::ptr::NonNull;
use std::time::{Duration, Instant};
use x11rb::connection::Connection;
use x11rb::protocol::xproto::ConnectionExt as _;
use x11rb::protocol::{damage, randr, shm, xfixes, xproto, Event};
use x11rb::rust_connection::RustConnection;

struct Segment {
    id: shm::Seg,
    ptr: NonNull<u8>,
    len: usize,
}

// The mapping has a single Rust owner. The server writes only during a
// synchronous GetImage, and the reply is received before any Rust read.
unsafe impl Send for Segment {}

impl Segment {
    fn new(conn: &RustConnection, len: usize) -> anyhow::Result<Self> {
        let id = conn.generate_id()?;
        let reply = shm::create_segment(conn, id, u32::try_from(len)?, false)?.reply()?;
        // SAFETY: the server supplied an fd for a segment of exactly `len` bytes.
        let ptr = unsafe {
            libc::mmap(
                std::ptr::null_mut(),
                len,
                libc::PROT_READ,
                libc::MAP_SHARED,
                reply.shm_fd.as_raw_fd(),
                0,
            )
        };
        if ptr == libc::MAP_FAILED {
            let _ = shm::detach(conn, id);
            return Err(std::io::Error::last_os_error().into());
        }
        Ok(Self {
            id,
            ptr: NonNull::new(ptr.cast()).context("null SHM mapping")?,
            len,
        })
    }
    fn bytes(&self, len: usize) -> anyhow::Result<&[u8]> {
        ensure!(len <= self.len, "X image exceeds SHM segment");
        // SAFETY: the mapping is live and GetImage has completed before this call.
        Ok(unsafe { std::slice::from_raw_parts(self.ptr.as_ptr(), len) })
    }
}
impl Drop for Segment {
    fn drop(&mut self) {
        // SAFETY: this is the original mapping and length, owned by this object.
        unsafe {
            libc::munmap(self.ptr.as_ptr().cast(), self.len);
        }
    }
}

/// Root-window capture on `$DISPLAY`, authenticated by x11rb using `$XAUTHORITY`.
pub struct X11Capture {
    conn: RustConnection,
    root: u32,
    width: u16,
    height: u16,
    segment: Option<Segment>,
    damage: Option<u32>,
    fixes: bool,
    desktop: Bgra,
    cursor: Option<xfixes::GetCursorImageReply>,
    first: bool,
    cursor_drawn: bool,
    last_reported_serial: Option<u32>,
    last_reported_visible: Option<bool>,
    pending_cursor: Option<crate::video::CursorUpdate>,
}

impl X11Capture {
    pub fn new() -> anyhow::Result<Self> {
        let (conn, screen) = x11rb::connect(None).context("connect to X11 DISPLAY")?;
        let setup = conn.setup();
        let screen = &setup.roots[screen];
        let root = screen.root;
        let format = setup
            .pixmap_formats
            .iter()
            .find(|f| f.depth == screen.root_depth)
            .context("root pixmap format missing")?;
        let visual = screen
            .allowed_depths
            .iter()
            .flat_map(|d| &d.visuals)
            .find(|v| v.visual_id == screen.root_visual)
            .context("root visual missing")?;
        ensure!(
            format.bits_per_pixel == 32
                && setup.image_byte_order == xproto::ImageOrder::LSB_FIRST
                && visual.red_mask == 0xff0000
                && visual.green_mask == 0xff00
                && visual.blue_mask == 0xff,
            "X11 capture requires a little-endian 32-bit BGR root visual"
        );
        let geometry = conn.get_geometry(root)?.reply()?;
        let fixes = xfixes::query_version(&conn, 5, 0)
            .ok()
            .and_then(|c| c.reply().ok())
            .is_some();
        let damage = if fixes
            && damage::query_version(&conn, 1, 1)
                .ok()
                .and_then(|c| c.reply().ok())
                .is_some()
        {
            let id = conn.generate_id()?;
            damage::create(&conn, id, root, damage::ReportLevel::RAW_RECTANGLES)?.check()?;
            Some(id)
        } else {
            None
        };
        if randr::query_version(&conn, 1, 2)
            .ok()
            .and_then(|c| c.reply().ok())
            .is_some()
        {
            randr::select_input(
                &conn,
                root,
                randr::NotifyMask::SCREEN_CHANGE | randr::NotifyMask::CRTC_CHANGE,
            )?
            .check()?;
        }
        conn.change_window_attributes(
            root,
            &xproto::ChangeWindowAttributesAux::new()
                .event_mask(xproto::EventMask::STRUCTURE_NOTIFY),
        )?
        .check()?;
        let mut this = Self {
            conn,
            root,
            width: geometry.width,
            height: geometry.height,
            segment: None,
            damage,
            fixes,
            desktop: Bgra::default(),
            cursor: None,
            first: true,
            cursor_drawn: true,
            last_reported_serial: None,
            last_reported_visible: None,
            pending_cursor: None,
        };
        this.allocate();
        Ok(this)
    }

    /// Takes a fresh full-root image even when Damage reports no change.
    /// Benchmarks use this to measure the transfer rather than an idle poll.
    pub fn capture_now(&mut self, frame: &mut Bgra) -> anyhow::Result<()> {
        self.first = true;
        self.grab(frame, Duration::ZERO)?;
        Ok(())
    }

    /// Whether this connection can capture through server-allocated MIT-SHM.
    pub fn uses_shm(&self) -> bool {
        self.segment.is_some()
    }

    fn allocate(&mut self) {
        if let Some(segment) = self.segment.take() {
            let _ = shm::detach(&self.conn, segment.id);
        }
        let len = self.width as usize * self.height as usize * 4;
        self.segment = Segment::new(&self.conn, len).ok();
        self.desktop = Bgra {
            width: self.width.into(),
            height: self.height.into(),
            stride: self.width as usize * 4,
            data: vec![0; len],
        };
        self.first = true;
    }

    fn pixels(&mut self, area: xproto::Rectangle) -> anyhow::Result<()> {
        let stride = area.width as usize * 4;
        let len = stride * area.height as usize;
        let owned;
        let bytes = if let Some(segment) = &self.segment {
            match shm::get_image(
                &self.conn,
                self.root,
                area.x,
                area.y,
                area.width,
                area.height,
                u32::MAX,
                u8::from(xproto::ImageFormat::Z_PIXMAP),
                segment.id,
                0,
            )?
            .reply()
            {
                Ok(reply) => {
                    ensure!(reply.size as usize >= len, "short MIT-SHM image");
                    segment.bytes(len)?
                }
                Err(_) => {
                    // Remote X and some nested servers advertise SHM but reject GetImage.
                    owned = self
                        .conn
                        .get_image(
                            xproto::ImageFormat::Z_PIXMAP,
                            self.root,
                            area.x,
                            area.y,
                            area.width,
                            area.height,
                            u32::MAX,
                        )?
                        .reply()?
                        .data;
                    &owned
                }
            }
        } else {
            owned = self
                .conn
                .get_image(
                    xproto::ImageFormat::Z_PIXMAP,
                    self.root,
                    area.x,
                    area.y,
                    area.width,
                    area.height,
                    u32::MAX,
                )?
                .reply()?
                .data;
            &owned
        };
        ensure!(bytes.len() >= len, "short X11 image");
        for row in 0..area.height as usize {
            let dst = (area.y as usize + row) * self.desktop.stride + area.x as usize * 4;
            self.desktop.data[dst..dst + stride]
                .copy_from_slice(&bytes[row * stride..(row + 1) * stride]);
        }
        Ok(())
    }
}

impl CpuCapture for X11Capture {
    fn size(&self) -> (u32, u32) {
        (self.width.into(), self.height.into())
    }
    fn display(&self) -> DisplayRect {
        DisplayRect {
            x: 0,
            y: 0,
            width: self.width.into(),
            height: self.height.into(),
        }
    }
    fn name(&self) -> String {
        "X11".into()
    }
    fn grab(&mut self, frame: &mut Bgra, timeout: Duration) -> anyhow::Result<bool> {
        let start = Instant::now();
        loop {
            let mut dirty = None;
            let mut resized = false;
            while let Some(event) = self.conn.poll_for_event()? {
                match event {
                    Event::DamageNotify(event) => {
                        dirty = union(dirty, event.area, self.width, self.height);
                    }
                    Event::RandrScreenChangeNotify(_)
                    | Event::RandrNotify(_)
                    | Event::ConfigureNotify(_) => resized = true,
                    Event::Error(error) => anyhow::bail!("X11 capture error: {error:?}"),
                    _ => {}
                }
            }
            if resized {
                let geometry = self.conn.get_geometry(self.root)?.reply()?;
                if (self.width, self.height) != (geometry.width, geometry.height) {
                    self.width = geometry.width;
                    self.height = geometry.height;
                    self.allocate();
                }
            }
            if self.first || self.damage.is_none() {
                dirty = Some(xproto::Rectangle {
                    x: 0,
                    y: 0,
                    width: self.width,
                    height: self.height,
                });
            }
            let cursor = if self.fixes {
                Some(xfixes::get_cursor_image(&self.conn)?.reply()?)
            } else {
                None
            };
            if let Some(cursor) = &cursor {
                let all_transparent = cursor.cursor_image.iter().all(|&pixel| (pixel >> 24) == 0);
                let visible = !cursor.cursor_image.is_empty() && !all_transparent;
                if visible {
                    if self.last_reported_serial != Some(cursor.cursor_serial) {
                        let mut rgba = Vec::with_capacity(cursor.cursor_image.len() * 4);
                        for &pixel in &cursor.cursor_image {
                            let a = (pixel >> 24) as u8;
                            if a == 0 {
                                rgba.extend_from_slice(&[0, 0, 0, 0]);
                            } else {
                                let r_pre = ((pixel >> 16) & 0xff) as u32;
                                let g_pre = ((pixel >> 8) & 0xff) as u32;
                                let b_pre = (pixel & 0xff) as u32;
                                let a_u32 = a as u32;
                                let r = ((r_pre * 255 + a_u32 / 2) / a_u32).min(255) as u8;
                                let g = ((g_pre * 255 + a_u32 / 2) / a_u32).min(255) as u8;
                                let b = ((b_pre * 255 + a_u32 / 2) / a_u32).min(255) as u8;
                                rgba.extend_from_slice(&[r, g, b, a]);
                            }
                        }
                        let shape = latch_proto::control::CursorShape::new(
                            cursor.cursor_serial,
                            cursor.width.into(),
                            cursor.height.into(),
                            cursor.xhot.into(),
                            cursor.yhot.into(),
                            &rgba,
                        );
                        self.last_reported_serial = Some(cursor.cursor_serial);
                        self.last_reported_visible = Some(true);
                        self.pending_cursor = Some(crate::video::CursorUpdate {
                            visible: true,
                            shape: Some(shape),
                        });
                    } else if self.last_reported_visible != Some(true) {
                        self.last_reported_visible = Some(true);
                        self.pending_cursor = Some(crate::video::CursorUpdate {
                            visible: true,
                            shape: None,
                        });
                    }
                } else if self.last_reported_visible != Some(false) {
                    self.last_reported_visible = Some(false);
                    self.pending_cursor = Some(crate::video::CursorUpdate {
                        visible: false,
                        shape: None,
                    });
                }
            } else if self.last_reported_visible != Some(false) && self.fixes {
                self.last_reported_visible = Some(false);
                self.pending_cursor = Some(crate::video::CursorUpdate {
                    visible: false,
                    shape: None,
                });
            }

            let cursor_changed = match (&self.cursor, &cursor) {
                (Some(a), Some(b)) => {
                    if self.cursor_drawn {
                        (a.x, a.y, a.cursor_serial) != (b.x, b.y, b.cursor_serial)
                    } else {
                        a.cursor_serial != b.cursor_serial
                    }
                }
                (None, None) => false,
                _ => true,
            };
            if let Some(area) = dirty {
                if let Some(id) = self.damage {
                    damage::subtract(&self.conn, id, x11rb::NONE, x11rb::NONE)?.check()?;
                }
                self.pixels(area)?;
            }
            self.cursor = cursor;
            if dirty.is_some() || cursor_changed || self.first {
                frame.width = self.desktop.width;
                frame.height = self.desktop.height;
                frame.stride = self.desktop.stride;
                frame.data.clone_from(&self.desktop.data);
                if self.cursor_drawn {
                    if let Some(cursor) = &self.cursor {
                        composite(frame, cursor);
                    }
                }
                self.first = false;
                return Ok(true);
            }
            let remaining = timeout.saturating_sub(start.elapsed());
            if remaining.is_zero() {
                return Ok(false);
            }
            super::wait_fd(
                self.conn.stream().as_raw_fd(),
                remaining.min(Duration::from_millis(8)),
            )?;
        }
    }

    fn set_cursor_drawn(&mut self, drawn: bool) -> bool {
        self.cursor_drawn = drawn;
        true
    }

    fn take_cursor(&mut self) -> Option<crate::video::CursorUpdate> {
        self.pending_cursor.take()
    }
}

impl Drop for X11Capture {
    fn drop(&mut self) {
        if let Some(id) = self.damage {
            let _ = damage::destroy(&self.conn, id);
        }
        if let Some(segment) = &self.segment {
            let _ = shm::detach(&self.conn, segment.id);
        }
        let _ = self.conn.flush();
    }
}

fn union(
    old: Option<xproto::Rectangle>,
    rect: xproto::Rectangle,
    width: u16,
    height: u16,
) -> Option<xproto::Rectangle> {
    let mut left = i32::from(rect.x).max(0);
    let mut top = i32::from(rect.y).max(0);
    let mut right = (i32::from(rect.x) + i32::from(rect.width)).min(width.into());
    let mut bottom = (i32::from(rect.y) + i32::from(rect.height)).min(height.into());
    if right <= left || bottom <= top {
        return old;
    }
    if let Some(old) = old {
        left = left.min(old.x.into());
        top = top.min(old.y.into());
        right = right.max(i32::from(old.x) + i32::from(old.width));
        bottom = bottom.max(i32::from(old.y) + i32::from(old.height));
    }
    Some(xproto::Rectangle {
        x: left as i16,
        y: top as i16,
        width: (right - left) as u16,
        height: (bottom - top) as u16,
    })
}

fn composite(frame: &mut Bgra, cursor: &xfixes::GetCursorImageReply) {
    let x = i32::from(cursor.x) - i32::from(cursor.xhot);
    let y = i32::from(cursor.y) - i32::from(cursor.yhot);
    for row in 0..cursor.height as usize {
        let dy = y + row as i32;
        if !(0..frame.height as i32).contains(&dy) {
            continue;
        }
        for col in 0..cursor.width as usize {
            let dx = x + col as i32;
            if !(0..frame.width as i32).contains(&dx) {
                continue;
            }
            let Some(&pixel) = cursor.cursor_image.get(row * cursor.width as usize + col) else {
                continue;
            };
            let alpha = pixel >> 24;
            let dst = dy as usize * frame.stride + dx as usize * 4;
            for channel in 0..3 {
                let src = (pixel >> (channel * 8)) & 255;
                frame.data[dst + channel] = (src
                    + (u32::from(frame.data[dst + channel]) * (255 - alpha) + 127) / 255)
                    .min(255) as u8;
            }
            frame.data[dst + 3] = 255;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn clips_damage_and_premultiplied_cursor() {
        let rect = xproto::Rectangle {
            x: -2,
            y: -1,
            width: 4,
            height: 3,
        };
        let area = union(None, rect, 10, 10).unwrap();
        assert_eq!((area.x, area.y, area.width, area.height), (0, 0, 2, 2));
        let mut frame = Bgra {
            width: 2,
            height: 2,
            stride: 8,
            data: vec![100; 16],
        };
        let cursor = xfixes::GetCursorImageReply {
            x: -1,
            y: 0,
            width: 2,
            height: 1,
            cursor_image: vec![0xffffffff, 0x80402010],
            ..Default::default()
        };
        composite(&mut frame, &cursor);
        assert_eq!(&frame.data[..4], &[66, 82, 114, 255]);
        assert_eq!(&frame.data[4..8], &[100; 4]);
    }
}
