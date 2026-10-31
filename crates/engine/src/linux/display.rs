//! RandR uses the same mode policy as stuzha-display, without changing
//! desktop scaling. Grow the framebuffer before the CRTC, shrink it after.

use super::X11;
use crate::display::Previous;
use anyhow::{ensure, Context};
use std::time::Duration;
use x11rb::connection::Connection;
use x11rb::protocol::randr::{self, ConnectionExt as _};
use x11rb::protocol::xproto::ConnectionExt as _;

/// A mode switch can keep a real GPU's X server busy for a second or two;
/// a server silent for longer than this has stalled, and restore (the
/// session's undo) must not wait on it forever.
const SILENCE: Duration = Duration::from_secs(10);

struct NewMode<'a> {
    conn: &'a X11,
    output: u32,
    mode: u32,
    keep: bool,
}
impl Drop for NewMode<'_> {
    fn drop(&mut self) {
        if self.keep {
            return;
        }
        if let Ok(cookie) = self.conn.randr_delete_output_mode(self.output, self.mode) {
            let _ = cookie.check();
        }
        if let Ok(cookie) = self.conn.randr_destroy_mode(self.mode) {
            let _ = cookie.check();
        }
    }
}

/// A mode `prepare` put on an output, creating it unless the server
/// already had one by that name. Previous's shared contract carries only
/// dimensions, so `restore` finds these here and takes them off again.
struct Attached {
    output: u32,
    mode: u32,
    created: bool,
}
static ATTACHED: parking_lot::Mutex<Vec<Attached>> = parking_lot::Mutex::new(Vec::new());

fn detach(conn: &X11, attached: &Attached) -> anyhow::Result<()> {
    conn.randr_delete_output_mode(attached.output, attached.mode)?
        .check()?;
    if attached.created {
        conn.randr_destroy_mode(attached.mode)?.check()?;
    }
    Ok(())
}

fn connect() -> anyhow::Result<(X11, u32)> {
    let (conn, screen) = super::connect(SILENCE).context("connect to X11 for RandR")?;
    let root = conn.setup().roots[screen].root;
    Ok((conn, root))
}

fn refresh(mode: &randr::ModeInfo) -> f64 {
    if mode.htotal == 0 || mode.vtotal == 0 {
        return 0.0;
    }
    let mut hz = f64::from(mode.dot_clock) / f64::from(mode.htotal) / f64::from(mode.vtotal);
    if mode.mode_flags.contains(randr::ModeFlag::INTERLACE) {
        hz *= 2.0;
    }
    if mode.mode_flags.contains(randr::ModeFlag::DOUBLE_SCAN) {
        hz /= 2.0;
    }
    hz
}

/// A mode's size on screen, or the mode for a size on screen: a quarter
/// turn swaps the sides. Requests and [`Previous`] are sizes on screen.
fn on_screen(width: u32, height: u32, rotation: randr::Rotation) -> (u32, u32) {
    if rotation.intersects(randr::Rotation::ROTATE90 | randr::Rotation::ROTATE270) {
        (height, width)
    } else {
        (width, height)
    }
}

fn timings(width: u32, height: u32) -> anyhow::Result<randr::ModeInfo> {
    ensure!(
        (640..=5120).contains(&width) && (360..=2880).contains(&height),
        "display size must be in 640x360..=5120x2880"
    );
    let name = format!("{width}x{height}_60");
    Ok(randr::ModeInfo {
        width: width as u16,
        height: height as u16,
        dot_clock: (width + 24) * (height + 3) * 60,
        hsync_start: (width + 8) as u16,
        hsync_end: (width + 16) as u16,
        htotal: (width + 24) as u16,
        vsync_start: (height + 1) as u16,
        vsync_end: (height + 2) as u16,
        vtotal: (height + 3) as u16,
        name_len: name.len() as u16,
        ..Default::default()
    })
}

pub(super) fn prepare(width: u32, height: u32, fps: u32) -> anyhow::Result<Option<Previous>> {
    if let Err(error) = timings(width, height) {
        tracing::warn!(%error, width, height, "keeping current display mode; streaming at the capture size");
        return Ok(None);
    }
    let (conn, root) = connect()?;
    let Some((previous, attached)) = configure(&conn, root, width, height, fps.max(50), 50.0)?
    else {
        return Ok(None);
    };
    ATTACHED.lock().extend(attached);
    Ok(Some(previous))
}

fn configure(
    conn: &X11,
    root: u32,
    width: u32,
    height: u32,
    fps: u32,
    min_refresh: f64,
) -> anyhow::Result<Option<(Previous, Option<Attached>)>> {
    conn.randr_query_version(1, 2)?.reply()?;
    let resources = conn.randr_get_screen_resources(root)?.reply()?;
    let mut selected = None;
    for &output in &resources.outputs {
        let info = conn
            .randr_get_output_info(output, resources.config_timestamp)?
            .reply()?;
        if info.connection == randr::Connection::CONNECTED && info.crtc != 0 {
            selected = Some((output, info));
            break;
        }
    }
    let (output, info) = selected.context("no connected output with an active CRTC")?;
    let crtc = conn
        .randr_get_crtc_info(info.crtc, resources.config_timestamp)?
        .reply()?;
    let old_mode = resources
        .modes
        .iter()
        .find(|m| m.id == crtc.mode)
        .context("active RandR mode missing")?;
    let (old_width, old_height) =
        on_screen(old_mode.width.into(), old_mode.height.into(), crtc.rotation);
    let previous = Previous {
        width: old_width,
        height: old_height,
        refresh_hz: refresh(old_mode).round() as u32,
    };
    // The CRTC keeps its rotation, so the mode is the request turned back.
    let (mode_width, mode_height) = on_screen(width, height, crtc.rotation);
    let target = resources
        .modes
        .iter()
        .filter(|m| {
            info.modes.contains(&m.id)
                && u32::from(m.width) == mode_width
                && u32::from(m.height) == mode_height
                && refresh(m) >= min_refresh
        })
        .min_by(|a, b| {
            (refresh(a) - fps as f64)
                .abs()
                .total_cmp(&(refresh(b) - fps as f64).abs())
        });
    let mut created = None;
    let mut attached = None;
    let mode = if let Some(mode) = target {
        mode.id
    } else {
        let name = format!("{mode_width}x{mode_height}_60");
        let mut offset = 0;
        let existing = resources.modes.iter().find(|m| {
            let end = offset + m.name_len as usize;
            let matches = resources.names.get(offset..end) == Some(name.as_bytes());
            offset = end;
            matches
                && u32::from(m.width) == mode_width
                && u32::from(m.height) == mode_height
                && refresh(m) >= min_refresh
        });
        let mode = if let Some(mode) = existing {
            mode.id
        } else {
            let mode = conn
                .randr_create_mode(root, timings(mode_width, mode_height)?, name.as_bytes())?
                .reply()?
                .mode;
            created = Some(NewMode {
                conn,
                output,
                mode,
                keep: false,
            });
            mode
        };
        if !info.modes.contains(&mode) {
            conn.randr_add_output_mode(output, mode)?.check()?;
            attached = Some(Attached {
                output,
                mode,
                created: created.is_some(),
            });
        }
        mode
    };
    if mode == crtc.mode {
        return Ok(None);
    }
    let geometry = conn.get_geometry(root)?.reply()?;
    let size = conn.randr_get_screen_size_range(root)?.reply()?;
    let mut target_width = u32::from(crtc.x.max(0) as u16) + width;
    let mut target_height = u32::from(crtc.y.max(0) as u16) + height;
    for &id in &resources.crtcs {
        if id == info.crtc {
            continue;
        }
        let other = conn
            .randr_get_crtc_info(id, resources.config_timestamp)?
            .reply()?;
        if other.mode != 0 {
            target_width = target_width.max(other.x.max(0) as u32 + u32::from(other.width));
            target_height = target_height.max(other.y.max(0) as u32 + u32::from(other.height));
        }
    }
    target_width = target_width.max(size.min_width.into());
    target_height = target_height.max(size.min_height.into());
    ensure!(
        target_width <= size.max_width.into() && target_height <= size.max_height.into(),
        "mode exceeds RandR framebuffer limits"
    );
    let set_size = |w: u16, h: u16| -> anyhow::Result<()> {
        conn.randr_set_screen_size(
            root,
            w,
            h,
            (u32::from(w) * 254 / 960).max(1),
            (u32::from(h) * 254 / 960).max(1),
        )?
        .check()?;
        Ok(())
    };
    let switch = |mode| -> anyhow::Result<()> {
        let reply = conn
            .randr_set_crtc_config(
                info.crtc,
                x11rb::CURRENT_TIME,
                x11rb::CURRENT_TIME,
                crtc.x,
                crtc.y,
                mode,
                crtc.rotation,
                &crtc.outputs,
            )?
            .reply()?;
        ensure!(
            reply.status == randr::SetConfig::SUCCESS,
            "RandR SetCrtcConfig failed: {:?}",
            reply.status
        );
        Ok(())
    };
    let result = (|| {
        set_size(
            geometry.width.max(target_width as u16),
            geometry.height.max(target_height as u16),
        )?;
        switch(mode)?;
        set_size(target_width as u16, target_height as u16)?;
        Ok(Some((previous, attached)))
    })();
    if result.is_err() {
        let _ = set_size(
            geometry.width.max(target_width as u16),
            geometry.height.max(target_height as u16),
        );
        let _ = switch(crtc.mode);
        let _ = set_size(geometry.width, geometry.height);
    } else if let Some(created) = &mut created {
        created.keep = true;
    }
    result
}

pub(super) fn restore(previous: &Previous) -> anyhow::Result<()> {
    let (conn, root) = connect()?;
    configure(
        &conn,
        root,
        previous.width,
        previous.height,
        previous.refresh_hz,
        0.0,
    )?;
    // No CRTC shows prepare's modes now. Modes the server had before Latch
    // were never recorded and stay.
    for attached in ATTACHED.lock().drain(..) {
        if let Err(error) = detach(&conn, &attached) {
            tracing::warn!(%error, mode = attached.mode, "could not remove a Latch display mode");
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn minimal_blanking_is_sixty_hz() {
        let m = timings(3024, 1964).unwrap();
        assert_eq!(refresh(&m), 60.0);
        assert_eq!((m.htotal, m.vtotal), (3048, 1967));
        assert!(timings(1, 1080).is_err());
        assert!(prepare(6016, 3384, 60).unwrap().is_none());
    }
    #[test]
    fn a_quarter_turn_swaps_the_mode_sides() {
        use randr::Rotation;
        // A portrait CRTC showing a rotated 1920x1080 mode: a 1280x720
        // viewer needs the 720x1280 mode, and restore asks for 1080x1920.
        assert_eq!(on_screen(1280, 720, Rotation::ROTATE90), (720, 1280));
        assert_eq!(
            on_screen(1920, 1080, Rotation::ROTATE270 | Rotation::REFLECT_X),
            (1080, 1920)
        );
        assert_eq!(on_screen(1280, 720, Rotation::ROTATE180), (1280, 720));
        assert_eq!(
            on_screen(1280, 720, Rotation::ROTATE0 | Rotation::REFLECT_Y),
            (1280, 720)
        );
    }
}
