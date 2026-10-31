//! Compatibility capture for drivers that deny or never deliver DXGI frames.
use crate::input::DisplayRect;
use anyhow::{ensure, Context, Result};
use windows::{
    core::PCWSTR,
    Win32::{Foundation::*, Graphics::Gdi::*, UI::WindowsAndMessaging::*},
};

pub(super) struct Gdi {
    name: [u16; 32],
    pub display: DisplayRect,
    screen: HDC,
    dc: HDC,
    bitmap: HBITMAP,
    previous: HGDIOBJ,
    pixels: *mut u8,
}
fn rectangle(name: &[u16; 32]) -> Result<DisplayRect> {
    let mut mode = DEVMODEW {
        dmSize: std::mem::size_of::<DEVMODEW>() as u16,
        ..Default::default()
    };
    // SAFETY: The name is terminated and the sized descriptor is writable.
    unsafe {
        EnumDisplaySettingsW(PCWSTR(name.as_ptr()), ENUM_CURRENT_SETTINGS, &mut mode)
            .ok()
            .context("read GDI capture display")?;
        Ok(DisplayRect {
            x: mode.Anonymous1.Anonymous2.dmPosition.x,
            y: mode.Anonymous1.Anonymous2.dmPosition.y,
            width: mode.dmPelsWidth,
            height: mode.dmPelsHeight,
        })
    }
}
impl Gdi {
    pub fn new(name: [u16; 32]) -> Result<Self> {
        Self::bitmap(name, rectangle(&name)?)
    }
    fn bitmap(name: [u16; 32], display: DisplayRect) -> Result<Self> {
        let _dpi = super::display::DpiAwareness::physical();
        ensure!(
            display.width > 0
                && display.height > 0
                && display.width <= 16384
                && display.height <= 16384,
            "invalid GDI capture dimensions"
        );
        let mut capture = Self {
            name,
            display,
            screen: HDC::default(),
            dc: HDC::default(),
            bitmap: HBITMAP::default(),
            previous: HGDIOBJ::default(),
            pixels: std::ptr::null_mut(),
        };
        let info = BITMAPINFO {
            bmiHeader: BITMAPINFOHEADER {
                biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
                biWidth: display.width as i32,
                biHeight: -(display.height as i32),
                biPlanes: 1,
                biBitCount: 32,
                biCompression: BI_RGB.0,
                ..Default::default()
            },
            ..Default::default()
        };
        // SAFETY: Owned handles are released by Drop, including partial failures.
        // A negative height makes the 32-bit BGR bitmap top-down, like DXGI.
        unsafe {
            capture.screen = GetDC(HWND::default());
            ensure!(!capture.screen.is_invalid(), "open desktop DC");
            capture.dc = CreateCompatibleDC(capture.screen);
            ensure!(!capture.dc.is_invalid(), "create capture DC");
            let mut pixels = std::ptr::null_mut();
            capture.bitmap = CreateDIBSection(
                capture.screen,
                &info,
                DIB_RGB_COLORS,
                &mut pixels,
                HANDLE::default(),
                0,
            )
            .context("allocate GDI capture bitmap")?;
            capture.pixels = pixels.cast();
            capture.previous = SelectObject(capture.dc, capture.bitmap);
            ensure!(!capture.previous.is_invalid(), "select capture bitmap");
        }
        Ok(capture)
    }
    pub fn grab(&mut self) -> Result<(DisplayRect, &[u8])> {
        let _dpi = super::display::DpiAwareness::physical();
        let display = rectangle(&self.name)?;
        if display != self.display {
            *self = Self::bitmap(self.name, display)?;
        }
        // SAFETY: Both DCs and the selected bitmap are owned. The cursor's
        // temporary mask/color bitmaps are freed even if drawing fails.
        unsafe {
            BitBlt(
                self.dc,
                0,
                0,
                display.width as i32,
                display.height as i32,
                self.screen,
                display.x,
                display.y,
                SRCCOPY | CAPTUREBLT,
            )
            .context("copy desktop with GDI")?;
            let mut cursor = CURSORINFO {
                cbSize: std::mem::size_of::<CURSORINFO>() as u32,
                ..Default::default()
            };
            GetCursorInfo(&mut cursor).context("read GDI cursor")?;
            if cursor.flags == CURSOR_SHOWING {
                let mut icon = ICONINFO::default();
                GetIconInfo(HICON(cursor.hCursor.0), &mut icon).context("read cursor hotspot")?;
                let drawn = DrawIconEx(
                    self.dc,
                    cursor.ptScreenPos.x - display.x - icon.xHotspot as i32,
                    cursor.ptScreenPos.y - display.y - icon.yHotspot as i32,
                    HICON(cursor.hCursor.0),
                    0,
                    0,
                    0,
                    HBRUSH::default(),
                    DI_NORMAL,
                );
                let _ = DeleteObject(icon.hbmMask);
                let _ = DeleteObject(icon.hbmColor);
                drawn.context("draw GDI cursor")?;
            }
            GdiFlush().ok().context("finish GDI capture")?;
            Ok((
                display,
                std::slice::from_raw_parts(
                    self.pixels,
                    display.width as usize * display.height as usize * 4,
                ),
            ))
        }
    }
}
impl Drop for Gdi {
    fn drop(&mut self) {
        // SAFETY: Restore the original bitmap before deleting ours. None of the
        // handles escapes this worker, and default handles are never released.
        unsafe {
            if !self.previous.is_invalid() {
                SelectObject(self.dc, self.previous);
            }
            if !self.bitmap.is_invalid() {
                let _ = DeleteObject(self.bitmap);
            }
            if !self.dc.is_invalid() {
                let _ = DeleteDC(self.dc);
            }
            if !self.screen.is_invalid() {
                ReleaseDC(HWND::default(), self.screen);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn bitmap_is_top_down_bgra() -> Result<()> {
        let capture = Gdi::bitmap(
            [0; 32],
            DisplayRect {
                width: 2,
                height: 2,
                ..Default::default()
            },
        )?;
        // SAFETY: These writes target the owned 2x2 DIB; flushing makes them
        // visible to CPU reads. COLORREF uses RGB while the DIB stores BGR.
        unsafe {
            SetPixelV(capture.dc, 0, 0, COLORREF(0x00332211)).ok()?;
            SetPixelV(capture.dc, 1, 1, COLORREF(0x00665544)).ok()?;
            GdiFlush().ok()?;
            let pixels = std::slice::from_raw_parts(capture.pixels, 16);
            assert_eq!(&pixels[..3], &[0x33, 0x22, 0x11]);
            assert_eq!(&pixels[12..15], &[0x66, 0x55, 0x44]);
        }
        Ok(())
    }
}
