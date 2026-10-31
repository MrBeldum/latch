//! X11 pixels stay in shared memory until a completed GetImage reply makes
//! them safe to read. Damage bounds limit copies; the cursor never dirties
//! the saved desktop, so moving it restores the old pixels automatically.

use super::X11;
use crate::input::DisplayRect;
use crate::software::{Bgra, CpuCapture};
use anyhow::{ensure, Context};
use std::os::fd::AsRawFd;
use std::ptr::NonNull;
use std::time::{Duration, Instant};
use x11rb::connection::{Connection, RequestConnection as _};
use x11rb::errors::ReplyError;
use x11rb::protocol::xproto::ConnectionExt as _;
use x11rb::protocol::{damage, randr, shm, xfixes, xproto, Event};

struct Segment {
    id: shm::Seg,
    ptr: NonNull<u8>,
    len: usize,
}

// The mapping has a single Rust owner. The server writes only during a
// synchronous GetImage, and the reply is received before any Rust read.
unsafe impl Send for Segment {}

impl Segment {
    fn new(conn: &X11, len: usize) -> anyhow::Result<Self> {
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
/// Every round trip fails after [`super::X11_SILENCE`] without an answer.
pub struct X11Capture {
    conn: X11,
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
        let (conn, screen) = super::connect(super::X11_SILENCE).map_err(|e| {
            let wayland = super::wayland_session(
                std::env::var_os("WAYLAND_DISPLAY").as_deref(),
                std::env::var_os("XDG_SESSION_TYPE").as_deref(),
            );
            e.context(if wayland {
                super::WAYLAND
            } else {
                "connect to X11 DISPLAY"
            })
        })?;
        let setup = conn.setup();
        let screen = &setup.roots[screen];
        let root = screen.root;
        let randr = randr::query_version(&conn, 1, 3)
            .ok()
            .and_then(|c| c.reply().ok())
            .is_some();
        ensure!(!xwayland(&conn, root, randr)?, super::WAYLAND);
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
        if randr {
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

    /// Re-reads the root's size. True, with a fresh image to come, when it changed.
    fn resize(&mut self) -> anyhow::Result<bool> {
        let geometry = self.conn.get_geometry(self.root)?.reply()?;
        if (self.width, self.height) == (geometry.width, geometry.height) {
            return Ok(false);
        }
        self.width = geometry.width;
        self.height = geometry.height;
        self.allocate();
        Ok(true)
    }

    /// Reads `area` into the saved desktop. False when every pixel was
    /// already there: damage also follows repaints of identical content.
    fn pixels(&mut self, area: xproto::Rectangle) -> anyhow::Result<bool> {
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
                // A silent server is not a reason to try again without SHM.
                Err(ReplyError::ConnectionError(e)) => return Err(e.into()),
                Err(ReplyError::X11Error(_)) => {
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
        let mut changed = false;
        for row in 0..area.height as usize {
            let dst = (area.y as usize + row) * self.desktop.stride + area.x as usize * 4;
            let dst = &mut self.desktop.data[dst..dst + stride];
            let src = &bytes[row * stride..(row + 1) * stride];
            if dst != src {
                dst.copy_from_slice(src);
                changed = true;
            }
        }
        Ok(changed)
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
                self.resize()?;
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
            let mut changed = false;
            if let Some(area) = dirty {
                if let Some(id) = self.damage {
                    damage::subtract(&self.conn, id, x11rb::NONE, x11rb::NONE)?.check()?;
                }
                match self.pixels(area) {
                    Ok(pixels) => changed = pixels,
                    // The root can shrink between the event poll and GetImage
                    // (BadMatch). That is a new size to capture, not a failure.
                    Err(_) if self.resize()? => continue,
                    Err(error) => return Err(error),
                }
            }
            self.cursor = cursor;
            // An unchanged picture costs the caller no conversion or encode.
            if changed || cursor_changed || self.first {
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
            // Without Damage each pass reads the whole root: one per call.
            if remaining.is_zero() || self.damage.is_none() {
                return Ok(false);
            }
            super::wait_fd(
                self.conn.stream().as_raw_fd(),
                libc::POLLIN,
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

/// Rootless Xwayland: a Wayland session's X server, whose root window holds
/// no desktop. Xwayland 23.1 and later announce themselves; all name their
/// RandR outputs XWAYLAND0, XWAYLAND1, …
fn xwayland(conn: &X11, root: u32, randr: bool) -> anyhow::Result<bool> {
    if conn.extension_information("XWAYLAND")?.is_some() {
        return Ok(true);
    }
    if !randr {
        return Ok(false);
    }
    let Ok(resources) = randr::get_screen_resources_current(conn, root)?.reply() else {
        return Ok(false);
    };
    let mut names = Vec::new();
    for &output in &resources.outputs {
        // An output that vanished meanwhile is no reason to refuse Xorg.
        if let Ok(info) = randr::get_output_info(conn, output, resources.config_timestamp)?.reply()
        {
            names.push(info.name);
        }
    }
    Ok(super::xwayland_outputs(names.iter().map(Vec::as_slice)))
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

    use std::io::{Read, Write};
    use std::os::unix::net::UnixStream;
    use x11rb::rust_connection::{DefaultStream, RustConnection};
    use x11rb::x11_utils::Serialize;

    /// A client connected to a scripted X server. `answer` gets each
    /// request's opcode and sequence number and returns the bytes to send
    /// back; nothing leaves that request unanswered.
    fn fake_x(
        silence: Duration,
        mut answer: impl FnMut(u8, u16) -> Vec<u8> + Send + 'static,
    ) -> X11 {
        let (client, mut server) = UnixStream::pair().unwrap();
        std::thread::spawn(move || {
            // An unauthenticated setup request is exactly 12 bytes.
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
            let mut sequence = 0u16;
            loop {
                let mut header = [0; 4];
                if server.read_exact(&mut header).is_err() {
                    return;
                }
                let length = usize::from(u16::from_ne_bytes([header[2], header[3]])) * 4;
                let mut body = vec![0; length.saturating_sub(4)];
                if server.read_exact(&mut body).is_err() {
                    return;
                }
                sequence = sequence.wrapping_add(1);
                if server.write_all(&answer(header[0], sequence)).is_err() {
                    return;
                }
            }
        });
        let (stream, _) = DefaultStream::from_unix_stream(client).unwrap();
        RustConnection::connect_to_stream(
            super::super::Bounded {
                stream,
                silence,
                deadline: std::sync::Mutex::new(None),
            },
            0,
        )
        .unwrap()
    }
    /// x11rb serializes a reply's fields; the wire pads it to 32 bytes.
    fn reply(fields: &[u8]) -> Vec<u8> {
        let mut bytes = fields.to_vec();
        bytes.resize(bytes.len().max(32), 0);
        bytes
    }
    /// The QueryExtension answer for MIT-SHM and anything else asked about.
    fn absent(sequence: u16) -> Vec<u8> {
        reply(
            &xproto::QueryExtensionReply {
                sequence,
                present: false,
                ..Default::default()
            }
            .serialize(),
        )
    }
    fn fake_capture(conn: X11, width: u16, height: u16) -> X11Capture {
        let mut capture = X11Capture {
            conn,
            root: 1,
            width,
            height,
            segment: None,
            damage: None,
            fixes: false,
            desktop: Bgra::default(),
            cursor: None,
            first: true,
        };
        capture.allocate();
        assert!(!capture.uses_shm());
        capture
    }

    #[test]
    fn a_silent_x_server_fails_the_grab_instead_of_holding_it() {
        let conn = fake_x(Duration::from_millis(100), |opcode, sequence| {
            if opcode == xproto::GET_IMAGE_REQUEST {
                Vec::new()
            } else {
                absent(sequence)
            }
        });
        let mut capture = fake_capture(conn, 4, 4);
        let (done, result) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let mut frame = Bgra::default();
            let result = capture.grab(&mut frame, Duration::ZERO);
            let _ = done.send(result.map_err(|e| format!("{e:#}")));
        });
        let error = result
            .recv_timeout(Duration::from_secs(30))
            .expect("the grab is still waiting for a silent X server")
            .unwrap_err();
        assert!(error.contains("did not answer"), "{error}");
    }

    #[test]
    fn a_resize_just_before_get_image_is_a_new_size_not_a_failure() {
        let mut shrunk = false;
        let conn = fake_x(
            Duration::from_secs(10),
            move |opcode, sequence| match opcode {
                xproto::GET_IMAGE_REQUEST if !shrunk => {
                    // The root became 2x2 after the event poll: BadMatch.
                    shrunk = true;
                    let mut error = vec![0; 32];
                    error[1] = xproto::MATCH_ERROR;
                    error[2..4].copy_from_slice(&sequence.to_ne_bytes());
                    error[10] = xproto::GET_IMAGE_REQUEST;
                    error
                }
                xproto::GET_IMAGE_REQUEST => reply(
                    &xproto::GetImageReply {
                        depth: 24,
                        sequence,
                        visual: 0,
                        data: vec![7; 16],
                    }
                    .serialize(),
                ),
                xproto::GET_GEOMETRY_REQUEST => reply(
                    &xproto::GetGeometryReply {
                        depth: 24,
                        sequence,
                        root: 1,
                        width: 2,
                        height: 2,
                        ..Default::default()
                    }
                    .serialize(),
                ),
                _ => absent(sequence),
            },
        );
        let mut capture = fake_capture(conn, 4, 4);
        let mut frame = Bgra::default();
        assert!(capture.grab(&mut frame, Duration::ZERO).unwrap());
        assert_eq!((frame.width, frame.height, frame.stride), (2, 2, 8));
        assert_eq!(frame.data, [7; 16]);
        assert_eq!(capture.size(), (2, 2));
    }

    #[test]
    fn a_repaint_with_the_same_pixels_is_not_a_change() {
        // Without Damage every grab reads the whole root; with it, a
        // client repainting identical pixels still reports damage.
        let mut images = 0;
        let conn = fake_x(Duration::from_secs(10), move |opcode, sequence| {
            if opcode != xproto::GET_IMAGE_REQUEST {
                return absent(sequence);
            }
            images += 1;
            let mut data = vec![7; 16];
            data[5] = if images < 3 { 7 } else { 9 };
            reply(
                &xproto::GetImageReply {
                    depth: 24,
                    sequence,
                    visual: 0,
                    data,
                }
                .serialize(),
            )
        });
        let mut capture = fake_capture(conn, 2, 2);
        let mut frame = Bgra::default();
        assert!(capture.grab(&mut frame, Duration::ZERO).unwrap());
        assert!(!capture.grab(&mut frame, Duration::ZERO).unwrap());
        assert_eq!(frame.data, [7; 16]);
        assert!(capture.grab(&mut frame, Duration::ZERO).unwrap());
        assert_eq!(frame.data[5], 9);
    }
}
