//! The PC's clipboard as text, for `/v1/clipboard`: what a Mac reads after
//! a ⌘C on the PC, and what it writes before a ⌘V.
//!
//! The service runs in the signed-in user's session (it starts from the Run
//! key), so the clipboard it sees is the one on the desktop. Only text is
//! handled; an image or a file on the clipboard reads as empty text.

use anyhow::Result;
use latch_core::api::Clipboard;

/// Windows ends lines with CRLF; the Mac with LF. Both directions convert,
/// so a paste never carries stray carriage returns.
#[cfg_attr(not(windows), allow(dead_code))]
pub fn to_windows(text: &str) -> String {
    text.replace("\r\n", "\n").replace('\n', "\r\n")
}

#[cfg_attr(not(windows), allow(dead_code))]
pub fn from_windows(text: &str) -> String {
    text.replace("\r\n", "\n")
}

#[cfg(windows)]
pub fn read() -> Result<Clipboard> {
    use windows::Win32::Foundation::HGLOBAL;
    use windows::Win32::System::DataExchange::{
        CloseClipboard, GetClipboardData, GetClipboardSequenceNumber, IsClipboardFormatAvailable,
    };
    use windows::Win32::System::Memory::{GlobalLock, GlobalSize, GlobalUnlock};
    use windows::Win32::System::Ole::CF_UNICODETEXT;

    let format = u32::from(CF_UNICODETEXT.0);
    unsafe {
        let seq = u64::from(GetClipboardSequenceNumber());
        if IsClipboardFormatAvailable(format).is_err() {
            return Ok(Clipboard::fit("", seq));
        }
        open()?;
        let read = || -> Result<String> {
            let handle = GetClipboardData(format)?;
            let mem = HGLOBAL(handle.0);
            let p = GlobalLock(mem) as *const u16;
            anyhow::ensure!(!p.is_null(), "the clipboard memory could not be locked");
            let units = GlobalSize(mem) / 2;
            let all = std::slice::from_raw_parts(p, units);
            let n = all.iter().position(|&c| c == 0).unwrap_or(units);
            let text = String::from_utf16_lossy(&all[..n]);
            // GlobalUnlock reports "unlocked" as an error; nothing to do.
            let _ = GlobalUnlock(mem);
            Ok(text)
        };
        let result = read();
        let _ = CloseClipboard();
        Ok(Clipboard::fit(&from_windows(&result?), seq))
    }
}

#[cfg(windows)]
pub fn write(text: &str) -> Result<()> {
    use windows::Win32::Foundation::{GlobalFree, HANDLE};
    use windows::Win32::System::DataExchange::{CloseClipboard, EmptyClipboard, SetClipboardData};
    use windows::Win32::System::Memory::{GlobalAlloc, GlobalLock, GlobalUnlock, GMEM_MOVEABLE};
    use windows::Win32::System::Ole::CF_UNICODETEXT;

    let mut wide: Vec<u16> = to_windows(text).encode_utf16().collect();
    wide.push(0);
    unsafe {
        let mem = GlobalAlloc(GMEM_MOVEABLE, wide.len() * 2)?;
        let p = GlobalLock(mem) as *mut u16;
        if p.is_null() {
            let _ = GlobalFree(mem);
            anyhow::bail!("could not lock memory for the clipboard");
        }
        std::ptr::copy_nonoverlapping(wide.as_ptr(), p, wide.len());
        let _ = GlobalUnlock(mem);
        if let Err(e) = open() {
            let _ = GlobalFree(mem);
            return Err(e);
        }
        let placed = EmptyClipboard()
            .and_then(|()| SetClipboardData(u32::from(CF_UNICODETEXT.0), HANDLE(mem.0)));
        let _ = CloseClipboard();
        match placed {
            // The system owns the memory from here on.
            Ok(_) => Ok(()),
            Err(e) => {
                let _ = GlobalFree(mem);
                Err(e.into())
            }
        }
    }
}

/// Another program may hold the clipboard for a moment; try a few times.
#[cfg(windows)]
unsafe fn open() -> Result<()> {
    use windows::Win32::System::DataExchange::OpenClipboard;
    let mut last = None;
    for _ in 0..8 {
        match OpenClipboard(None) {
            Ok(()) => return Ok(()),
            Err(e) => {
                last = Some(e);
                std::thread::sleep(std::time::Duration::from_millis(25));
            }
        }
    }
    anyhow::bail!(
        "another program is holding the clipboard ({})",
        last.map(|e| e.to_string()).unwrap_or_default()
    )
}

/// One clipboard handle for the life of the process. On X11 the text
/// belongs to the `arboard::Clipboard` that set it: when the last one is
/// dropped the text goes to a clipboard manager if one takes it within
/// 100 ms, and is otherwise gone before anything on the desktop can paste
/// it. A handle whose connection failed is replaced on the next call.
#[cfg(not(windows))]
fn with_clipboard<T>(
    f: impl FnOnce(&mut arboard::Clipboard) -> Result<T, arboard::Error>,
) -> Result<T> {
    static CLIPBOARD: parking_lot::Mutex<Option<arboard::Clipboard>> =
        parking_lot::Mutex::new(None);
    let mut slot = CLIPBOARD.lock();
    let clipboard = match slot.as_mut() {
        Some(c) => c,
        None => slot.insert(arboard::Clipboard::new()?),
    };
    let result = f(clipboard);
    if result.is_err() {
        *slot = None;
    }
    Ok(result?)
}

#[cfg(not(windows))]
pub fn read() -> Result<Clipboard> {
    let text = with_clipboard(|c| match c.get_text() {
        // No text on the clipboard (an image, or nothing) reads as empty.
        Err(arboard::Error::ContentNotAvailable) => Ok(String::new()),
        other => other,
    })?;
    Ok(Clipboard {
        // write() below puts pasted text in the selection too.
        selection: cfg!(target_os = "linux"),
        ..Clipboard::fit(&text, text_seq(&text))
    })
}

/// On Linux the text also becomes the X selection (PRIMARY): a Mac sends
/// ⌘V to a Linux PC as Shift+Insert, which pastes the selection rather
/// than the clipboard in xterm and the VTE terminals (GNOME's, XFCE's).
#[cfg(not(windows))]
pub fn write(text: &str) -> Result<()> {
    with_clipboard(|c| {
        c.set_text(text.to_string())?;
        #[cfg(target_os = "linux")]
        {
            use arboard::{LinuxClipboardKind, SetExtLinux};
            c.set()
                .clipboard(LinuxClipboardKind::Primary)
                .text(text.to_string())?;
        }
        Ok(())
    })
}

/// macOS and X11 have no sequence number a program can read, so the text
/// stands in for one: it changes whenever different text is copied, which
/// is all a reader needs to tell a new copy from one it has seen. A fixed
/// `seq` made every copy on a Mac or Linux desktop look like the last.
#[cfg(any(not(windows), test))]
fn text_seq(text: &str) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut h = std::collections::hash_map::DefaultHasher::new();
    text.hash(&mut h);
    h.finish()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn line_endings_convert_both_ways_without_doubling() {
        assert_eq!(to_windows("a\nb"), "a\r\nb");
        assert_eq!(to_windows("a\r\nb"), "a\r\nb");
        assert_eq!(from_windows("a\r\nb\r\n"), "a\nb\n");
        assert_eq!(from_windows(&to_windows("x\ny\n")), "x\ny\n");
        assert_eq!(to_windows(""), "");
    }

    #[test]
    fn a_new_copy_gets_a_new_sequence_number() {
        assert_eq!(text_seq("hello"), text_seq("hello"));
        assert_ne!(text_seq("hello"), text_seq("hello!"));
        assert_ne!(text_seq(""), text_seq(" "));
    }
}
