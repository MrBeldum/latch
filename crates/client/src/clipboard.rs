//! The clipboard both ways while a stream runs, through Latch Host's
//! `/v1/clipboard` on the PC.
//!
//! The Moonlight protocol carries keys and pictures, not clipboards, so ⌘V
//! on the PC used to paste whatever the PC last copied, and text copied on
//! the PC never reached the Mac. Now a worker polls the PC's clipboard
//! every second or so and hands new text to the window, which puts it in
//! the Mac's clipboard; and a paste first sends the Mac's text to the PC,
//! then presses Ctrl+V there. Hosts before 3.1 have no such route; the
//! worker notices the 404 and says so once.
//!
//! A Linux desktop is sent the X11 shortcuts instead (see [`Shortcuts`]):
//! in its terminals Ctrl+C interrupts the running program and Ctrl+V types
//! a literal, so ⌘C and ⌘V did harm there instead of copying.

use crate::input::{VK_C, VK_CONTROL, VK_DELETE, VK_INSERT, VK_SHIFT, VK_V, VK_X};
use latch_core::api::{Ack, Clipboard, CLIPBOARD_PATH};
use latch_core::{http, CONTROL_PORT};
use parking_lot::Mutex;
use std::net::Ipv4Addr;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

const POLL: Duration = Duration::from_millis(1500);
/// After a ⌘C on the PC, Windows needs a moment before the clipboard has
/// the new text.
const POKE_DELAY: Duration = Duration::from_millis(400);
const TIMEOUT: Duration = Duration::from_secs(5);

/// A clipboard shortcut pressed on the Mac and replayed on the PC.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Edit {
    Copy,
    Cut,
    Paste,
}

/// How the PC spells copy, cut and paste.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Shortcuts {
    /// What ⌘ stands for there (Ctrl, or the Windows key) with C, X or V:
    /// Windows and macOS.
    #[default]
    Letters,
    /// Ctrl+Insert, Shift+Delete and Shift+Insert: the X11 shortcuts GTK,
    /// Qt, Firefox, Chromium, Electron and the terminals all honour. Ctrl+C
    /// in a terminal interrupts the running program and Ctrl+V types a
    /// literal. Shift+Insert pastes the selection in xterm and VTE
    /// terminals, which is why Latch Host puts pasted text in both (and
    /// why paste stays Ctrl+V with a host that does not say it does).
    Insert,
}

impl Shortcuts {
    /// From Tailscale's OS string for the PC.
    pub fn for_os(os: &str) -> Self {
        if os.eq_ignore_ascii_case("linux") {
            Shortcuts::Insert
        } else {
            Shortcuts::Letters
        }
    }

    /// The chord for `edit`, given what ⌘ stands for on the PC, and whether
    /// it must arrive without other held modifiers: ⌘ held as Ctrl would
    /// turn Shift+Insert into Ctrl+Shift+Insert. `selection` is whether the
    /// host puts pasted text in the X selection; a Linux host that does not
    /// (4.1.1 and older) keeps Ctrl+V for paste, since Shift+Insert in a
    /// terminal would paste whatever was last selected there.
    pub fn keys(self, edit: Edit, modifier: i16, selection: bool) -> (Vec<i16>, bool) {
        match (self, edit) {
            (Shortcuts::Insert, Edit::Paste) if !selection => (vec![modifier, VK_V], false),
            (Shortcuts::Letters, Edit::Copy) => (vec![modifier, VK_C], false),
            (Shortcuts::Letters, Edit::Cut) => (vec![modifier, VK_X], false),
            (Shortcuts::Letters, Edit::Paste) => (vec![modifier, VK_V], false),
            (Shortcuts::Insert, Edit::Copy) => (vec![VK_CONTROL, VK_INSERT], true),
            (Shortcuts::Insert, Edit::Cut) => (vec![VK_SHIFT, VK_DELETE], true),
            (Shortcuts::Insert, Edit::Paste) => (vec![VK_SHIFT, VK_INSERT], true),
        }
    }
}

#[derive(Default)]
struct Shared {
    /// Text the PC copied that the window has not taken yet.
    incoming: Option<String>,
    /// The last text either side saw, so nothing echoes back and forth.
    last: String,
    last_seq: Option<u64>,
    /// The host answered 404: older than 3.1.
    unsupported: bool,
    /// The host puts pasted text in the X selection too (`Clipboard::selection`).
    selection: bool,
    /// Something the window can show about the last exchange.
    note: Option<(String, Instant)>,
    pokes: u32,
}

pub struct Sync {
    ip: Ipv4Addr,
    shared: Arc<Mutex<Shared>>,
    stop: Arc<AtomicBool>,
    /// Pastes whose chord has been sent, so the window can put a modifier
    /// the user is still holding back down afterwards.
    done: Arc<AtomicU32>,
}

impl Sync {
    /// Start polling the PC at `ip`; `ctx` is repainted when text arrives.
    pub fn spawn(ip: Ipv4Addr, ctx: egui::Context) -> Self {
        let shared: Arc<Mutex<Shared>> = Arc::default();
        let stop = Arc::new(AtomicBool::new(false));
        {
            let shared = shared.clone();
            let stop = stop.clone();
            std::thread::Builder::new()
                .name("clipboard".into())
                .spawn(move || poll_loop(ip, shared, stop, ctx))
                .expect("spawn clipboard thread");
        }
        Self {
            ip,
            shared,
            stop,
            done: Arc::default(),
        }
    }

    /// How many pastes have had their chord sent. The chord releases the
    /// modifier it pressed; if ⌘ is still physically down when this moves,
    /// the window presses the modifier again.
    pub fn pastes_done(&self) -> u32 {
        self.done.load(Ordering::Acquire)
    }

    /// Text copied on the PC since the last call, once.
    pub fn take_incoming(&self) -> Option<String> {
        self.shared.lock().incoming.take()
    }

    pub fn unsupported(&self) -> bool {
        self.shared.lock().unsupported
    }

    /// Whether the host has said it puts pasted text in the X selection,
    /// which is what makes Shift+Insert paste it in a terminal.
    pub fn pastes_to_selection(&self) -> bool {
        self.shared.lock().selection
    }

    /// A short line for the toolbar, while it is fresh.
    pub fn note(&self) -> Option<String> {
        let sh = self.shared.lock();
        sh.note
            .as_ref()
            .filter(|(_, at)| at.elapsed() < Duration::from_secs(4))
            .map(|(t, _)| t.clone())
    }

    /// The PC's clipboard probably just changed (a ⌘C was sent): read it
    /// sooner than the next poll.
    pub fn poke(&self) {
        self.shared.lock().pokes += 1;
    }

    /// Put `text` in the PC's clipboard, then run `then` (the paste chord)
    /// on the worker thread. If the PC already has this text, `then` runs
    /// at once; if the host cannot take it, `then` still runs, so the PC
    /// pastes whatever it has rather than nothing.
    pub fn push(&self, text: String, then: impl FnOnce() + Send + 'static) {
        let (same, unsupported) = {
            let sh = self.shared.lock();
            (sh.last == text, sh.unsupported)
        };
        let done = self.done.clone();
        if same || unsupported || text.is_empty() {
            then();
            done.fetch_add(1, Ordering::AcqRel);
            return;
        }
        let shared = self.shared.clone();
        let ip = self.ip;
        std::thread::spawn(move || {
            match send(ip, &text) {
                Ok(()) => {
                    let mut sh = shared.lock();
                    sh.last = text;
                    sh.note = Some(("Clipboard sent to the PC".into(), Instant::now()));
                }
                Err(Fail::Unsupported) => shared.lock().unsupported = true,
                Err(Fail::Other(e)) => {
                    tracing::warn!("clipboard to PC: {e}");
                    shared.lock().note = Some((format!("Clipboard not sent: {e}"), Instant::now()));
                }
            }
            then();
            done.fetch_add(1, Ordering::AcqRel);
        });
    }

    pub fn stop(&self) {
        self.stop.store(true, Ordering::Relaxed);
    }
}

enum Fail {
    Unsupported,
    Other(String),
}

fn send(ip: Ipv4Addr, text: &str) -> Result<(), Fail> {
    let body =
        serde_json::to_string(&Clipboard::fit(text, 0)).map_err(|e| Fail::Other(e.to_string()))?;
    let r = http::request(
        (ip, CONTROL_PORT),
        "POST",
        CLIPBOARD_PATH,
        Some(&body),
        TIMEOUT,
    )
    .map_err(|e| Fail::Other(e.to_string()))?;
    match r.status {
        200 => Ok(()),
        404 => Err(Fail::Unsupported),
        _ => Err(Fail::Other(
            r.parse::<Ack>()
                .ok()
                .and_then(|a| a.error)
                .unwrap_or_else(|| format!("HTTP {}", r.status)),
        )),
    }
}

fn fetch(ip: Ipv4Addr) -> Result<Clipboard, Fail> {
    let r = http::request((ip, CONTROL_PORT), "GET", CLIPBOARD_PATH, None, TIMEOUT)
        .map_err(|e| Fail::Other(e.to_string()))?;
    match r.status {
        200 => r
            .parse::<Clipboard>()
            .map_err(|e| Fail::Other(e.to_string())),
        404 => Err(Fail::Unsupported),
        _ => Err(Fail::Other(format!("HTTP {}", r.status))),
    }
}

fn poll_loop(ip: Ipv4Addr, shared: Arc<Mutex<Shared>>, stop: Arc<AtomicBool>, ctx: egui::Context) {
    let mut failures = 0u32;
    let mut pokes_seen = 0u32;
    let mut primed = false;
    while !stop.load(Ordering::Relaxed) {
        // Sleep in small steps so a poke or a stop is noticed quickly.
        let mut waited = Duration::ZERO;
        while waited < POLL && !stop.load(Ordering::Relaxed) {
            std::thread::sleep(Duration::from_millis(100));
            waited += Duration::from_millis(100);
            if shared.lock().pokes != pokes_seen {
                std::thread::sleep(POKE_DELAY);
                break;
            }
        }
        if stop.load(Ordering::Relaxed) {
            break;
        }
        pokes_seen = shared.lock().pokes;
        match fetch(ip) {
            Ok(c) => {
                failures = 0;
                let mut sh = shared.lock();
                sh.selection = c.selection;
                if !primed {
                    // The PC's existing clipboard is not a copy that happened
                    // during this stream. Seed last/seq so we do not overwrite
                    // whatever the Mac was holding.
                    primed = true;
                    sh.last_seq = Some(c.seq);
                    if !c.text.is_empty() {
                        sh.last = c.text;
                    }
                    continue;
                }
                if sh.last_seq != Some(c.seq) {
                    sh.last_seq = Some(c.seq);
                    if !c.text.is_empty() && c.text != sh.last {
                        sh.last = c.text.clone();
                        sh.incoming = Some(c.text);
                        sh.note = Some((
                            if c.truncated {
                                "Copied from the PC (cut at 32 KB)".to_string()
                            } else {
                                "Copied from the PC".to_string()
                            },
                            Instant::now(),
                        ));
                        drop(sh);
                        ctx.request_repaint();
                    }
                }
            }
            Err(Fail::Unsupported) => {
                shared.lock().unsupported = true;
                tracing::info!("clipboard: the host has no /v1/clipboard; sync is off");
                return;
            }
            Err(Fail::Other(e)) => {
                failures += 1;
                if failures == 3 {
                    tracing::warn!("clipboard from PC: {e}");
                }
                // Back off while the host is unreachable.
                std::thread::sleep(POLL * failures.min(10));
            }
        }
    }
}

/// Whether the window should offer a paste at all: `Event::Paste` carries
/// text only when the Mac's clipboard holds some.
pub fn worth_sending(text: &str) -> bool {
    !text.is_empty()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_linux_pc_gets_the_insert_shortcuts_and_the_rest_keep_the_letters() {
        use crate::input::VK_LWIN;
        assert_eq!(Shortcuts::for_os("linux"), Shortcuts::Insert);
        assert_eq!(Shortcuts::for_os("windows"), Shortcuts::Letters);
        assert_eq!(Shortcuts::for_os("macOS"), Shortcuts::Letters);
        assert_eq!(Shortcuts::for_os(""), Shortcuts::Letters);
        let linux = Shortcuts::Insert;
        assert_eq!(
            linux.keys(Edit::Copy, VK_LWIN, true),
            (vec![VK_CONTROL, VK_INSERT], true)
        );
        assert_eq!(
            linux.keys(Edit::Cut, VK_CONTROL, true),
            (vec![VK_SHIFT, VK_DELETE], true)
        );
        assert_eq!(
            linux.keys(Edit::Paste, VK_CONTROL, true),
            (vec![VK_SHIFT, VK_INSERT], true)
        );
        // A host that leaves the selection alone: Shift+Insert would paste
        // the terminal's last selection, so paste stays Ctrl+V. Copy and
        // cut need nothing from the host.
        assert_eq!(
            linux.keys(Edit::Paste, VK_CONTROL, false),
            (vec![VK_CONTROL, VK_V], false)
        );
        assert_eq!(
            linux.keys(Edit::Copy, VK_CONTROL, false),
            (vec![VK_CONTROL, VK_INSERT], true)
        );
        let windows = Shortcuts::Letters;
        assert_eq!(
            windows.keys(Edit::Copy, VK_CONTROL, true),
            (vec![VK_CONTROL, VK_C], false)
        );
        assert_eq!(
            windows.keys(Edit::Paste, VK_LWIN, false),
            (vec![VK_LWIN, VK_V], false)
        );
    }

    #[test]
    fn a_push_of_the_same_text_runs_the_paste_at_once() {
        let s = Sync {
            ip: Ipv4Addr::LOCALHOST,
            shared: Arc::default(),
            stop: Arc::new(AtomicBool::new(true)),
            done: Arc::default(),
        };
        s.shared.lock().last = "hello".into();
        let ran = Arc::new(AtomicBool::new(false));
        let r = ran.clone();
        assert_eq!(s.pastes_done(), 0);
        s.push("hello".into(), move || r.store(true, Ordering::Relaxed));
        assert!(ran.load(Ordering::Relaxed));
        assert_eq!(s.pastes_done(), 1);
        assert!(worth_sending("x"));
        assert!(worth_sending("  \n"), "whitespace is valid clipboard text");
        assert!(!worth_sending(""));
        assert!(s.take_incoming().is_none());
        s.shared.lock().incoming = Some("from pc".into());
        assert_eq!(s.take_incoming().as_deref(), Some("from pc"));
        assert!(s.take_incoming().is_none());
    }
}
