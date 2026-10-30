//! The clipboard both ways while a stream runs, through Latch Host's
//! `/v1/clipboard` on the PC.
//!
//! The stream carries keys and pictures; the control API carries clipboards, so ⌘V
//! on the PC used to paste whatever the PC last copied, and text copied on
//! the PC never reached the Mac. Now a worker polls the PC's clipboard
//! every second or so and hands new text to the window, which puts it in
//! the Mac's clipboard; and a paste first sends the Mac's text to the PC,
//! unless the PC has copied something newer since, then the window presses
//! Ctrl+V there, with anything typed meanwhile held back behind it. Hosts
//! before 3.1 have no such route; the worker notices the 404 and says so
//! once.
//!
//! A Linux desktop is sent the X11 shortcuts instead (see [`Shortcuts`]):
//! in its terminals Ctrl+C interrupts the running program and Ctrl+V types
//! a literal, so ⌘C and ⌘V did harm there instead of copying.

use crate::input::{VK_C, VK_CONTROL, VK_DELETE, VK_INSERT, VK_SHIFT, VK_V, VK_X};
use latch_core::api::{Ack, Clipboard, CLIPBOARD_PATH};
use latch_core::{http, CONTROL_PORT};
use parking_lot::Mutex;
use std::net::Ipv4Addr;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

const POLL: Duration = Duration::from_millis(1500);
/// After a ⌘C on the PC, Windows needs a moment before the clipboard has
/// the new text.
const POKE_DELAY: Duration = Duration::from_millis(400);
/// How long a paste right after a ⌘C waits for the PC's new clipboard to
/// be read, before sending the Mac's text anyway.
const POKE_WAIT: Duration = Duration::from_millis(1200);
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
    /// The last text both sides held, so nothing echoes back and forth.
    /// Empty once the PC has copied something that is not text.
    last: String,
    last_seq: Option<u64>,
    /// The host answered 404: older than 3.1.
    unsupported: bool,
    /// The host puts pasted text in the X selection too (`Clipboard::selection`).
    selection: bool,
    /// Something the window can show about the last exchange.
    note: Option<(String, Instant)>,
    /// ⌘C and ⌘X sent to the PC, and how many of them the poll had seen
    /// when its last read finished: while these differ, a copy may be on
    /// its way that a paste must not overwrite.
    pokes: u64,
    polled: u64,
    /// The PC has copied something (text, an image, files) that the
    /// window has not yet weighed against the Mac's clipboard.
    pc_copied: bool,
    /// A paste was sent to the PC; expect at most one echo/readback of that text.
    readback_expected: bool,
    /// The Mac clipboard's change count when the PC last copied something.
    /// While the Mac's count still reads this, the PC's clipboard is the
    /// newer of the two and a paste leaves it alone.
    pc_newer_than: Option<isize>,
    /// The newest paste whose text has reached the PC, or failed to.
    ready: u64,
    /// The newest paste the window stopped waiting for.
    gave_up: u64,
}

/// What a paste needs before the window presses its chord.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Paste {
    /// Nothing: the PC already has the right clipboard.
    Now,
    /// The Mac's text is on its way; [`Sync::paste_ready`] says when.
    Wait(u64),
}

pub struct Sync {
    ip: Ipv4Addr,
    shared: Arc<Mutex<Shared>>,
    stop: Arc<AtomicBool>,
    /// Repainted when a paste's text has arrived.
    ctx: egui::Context,
    pastes: AtomicU64,
}

impl Sync {
    /// Start polling the PC at `ip`; `ctx` is repainted when text arrives.
    pub fn spawn(ip: Ipv4Addr, ctx: egui::Context) -> Self {
        let shared: Arc<Mutex<Shared>> = Arc::default();
        let stop = Arc::new(AtomicBool::new(false));
        {
            let shared = shared.clone();
            let stop = stop.clone();
            let ctx = ctx.clone();
            std::thread::Builder::new()
                .name("clipboard".into())
                .spawn(move || poll_loop(ip, shared, stop, ctx))
                .expect("spawn clipboard thread");
        }
        Self {
            ip,
            shared,
            stop,
            ctx,
            pastes: AtomicU64::new(0),
        }
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

    /// Called every frame with a way to read the Mac clipboard's change
    /// count: when the PC has copied something since, that count is what
    /// the PC's copy is newer than. Read on the window's thread, after
    /// text from the PC has been handed over but before the Mac has
    /// written it, so that write counts as a change too (harmless: the
    /// Mac then holds the text the PC has).
    pub fn note_mac_clipboard(&self, count: impl FnOnce() -> Option<isize>) {
        let mut sh = self.shared.lock();
        if std::mem::take(&mut sh.pc_copied) {
            sh.pc_newer_than = count();
        }
    }

    /// Start a paste of `text`, the Mac's clipboard, whose change count is
    /// `mac_count`. The PC is sent the text first unless it already has it,
    /// or has copied something itself since the Mac last changed (an image
    /// or files there must not be replaced by older text from here), or the
    /// host cannot take it; then the PC pastes what it has. An empty
    /// `text` (an image or nothing on the Mac) is never sent.
    pub fn push(&self, text: String, mac_count: Option<isize>) -> Paste {
        let fitted = Clipboard::fit(&text, 0);
        let sh = self.shared.lock();
        if !sh.needs(&fitted, mac_count) {
            return Paste::Now;
        }
        let id = self.pastes.fetch_add(1, Ordering::Relaxed) + 1;
        // A ⌘C just sent may not have been read yet: wait for that read
        // before deciding, or the Mac's older text would replace it.
        let poke = (sh.polled != sh.pokes).then_some((sh.pokes, sh.last_seq));
        drop(sh);
        let (shared, stop, ctx, ip) = (
            self.shared.clone(),
            self.stop.clone(),
            self.ctx.clone(),
            self.ip,
        );
        std::thread::spawn(move || {
            if let Some((pokes, seq)) = poke {
                let until = Instant::now() + POKE_WAIT;
                while shared.lock().polled < pokes
                    && Instant::now() < until
                    && !stop.load(Ordering::Relaxed)
                {
                    std::thread::sleep(Duration::from_millis(50));
                }
                if shared.lock().last_seq != seq {
                    // The PC copied something after all: it pastes that.
                    shared.lock().finish(id);
                    ctx.request_repaint();
                    return;
                }
            }
            let before = shared.lock().sending(&fitted);
            let result = send(ip, &fitted);
            let mut sh = shared.lock();
            match result {
                Ok(()) => sh.sent(&fitted, id),
                Err(e) => {
                    sh.not_sent(&fitted, before);
                    match e {
                        Fail::Unsupported => sh.unsupported = true,
                        Fail::Other(e) => {
                            tracing::warn!("clipboard to PC: {e}");
                            sh.note = Some((format!("Clipboard not sent: {e}"), Instant::now()));
                        }
                    }
                }
            }
            sh.finish(id);
            drop(sh);
            ctx.request_repaint();
        });
        Paste::Wait(id)
    }

    /// Whether paste `id` is ready for its chord: its text has reached the
    /// PC, or failed to.
    pub fn paste_ready(&self, id: u64) -> bool {
        self.shared.lock().ready >= id
    }

    /// The window has stopped waiting for paste `id` and pressed its chord
    /// anyway (or dropped it).
    pub fn give_up(&self, id: u64) {
        let mut sh = self.shared.lock();
        sh.gave_up = sh.gave_up.max(id);
    }

    pub fn stop(&self) {
        self.stop.store(true, Ordering::Relaxed);
    }

    /// Mark paste `id` as arrived, as its worker would.
    #[cfg(test)]
    pub(crate) fn finish_for_test(&self, id: u64) {
        self.shared.lock().finish(id);
    }
}

impl Shared {
    /// Whether a paste of `fitted` has to send it to the PC first.
    fn needs(&self, fitted: &Clipboard, mac_count: Option<isize>) -> bool {
        let pc_newer = mac_count.is_some() && self.pc_newer_than == mac_count;
        !(fitted.text.is_empty()
            || self.unsupported
            || pc_newer
            || same_text(&self.last, &fitted.text))
    }

    /// `fitted` is about to be written on the PC: count it as the last text
    /// now, so the poll does not take the PC reading it back for a copy
    /// made there. Returns what `last` was, for [`Shared::not_sent`].
    fn sending(&mut self, fitted: &Clipboard) -> String {
        self.readback_expected = true;
        std::mem::replace(&mut self.last, fitted.text.clone())
    }

    fn sent(&mut self, fitted: &Clipboard, id: u64) {
        self.pc_newer_than = None;
        let text = if self.gave_up >= id {
            "The clipboard reached the PC after the paste: paste again for it"
        } else if fitted.truncated {
            "Sent the first 32 KB of the clipboard to the PC"
        } else {
            "Clipboard sent to the PC"
        };
        self.note = Some((text.into(), Instant::now()));
    }

    fn not_sent(&mut self, fitted: &Clipboard, before: String) {
        self.readback_expected = false;
        if self.last == fitted.text {
            self.last = before;
        }
    }

    fn finish(&mut self, id: u64) {
        self.ready = self.ready.max(id);
    }

    /// Take in one read of the PC's clipboard. True when there is
    /// something new for the window: text to put in the Mac's clipboard,
    /// or a copy of something else to weigh against it.
    fn observe(&mut self, c: Clipboard, primed: &mut bool) -> bool {
        self.selection = c.selection;
        if !*primed {
            // The PC's existing clipboard is not a copy that happened
            // during this stream. Seed last/seq so we do not overwrite
            // whatever the Mac was holding.
            *primed = true;
            self.last_seq = Some(c.seq);
            if !c.text.is_empty() {
                self.last = c.text;
            }
            return false;
        }
        if self.last_seq == Some(c.seq) {
            return false;
        }
        self.last_seq = Some(c.seq);
        if !c.text.is_empty() && self.readback_expected && same_text(&c.text, &self.last) {
            // Our own paste, read back.
            self.readback_expected = false;
            return false;
        }
        self.readback_expected = false;
        self.pc_copied = true;
        if c.text.is_empty() {
            // An image or files: nothing to bring over, but what the PC
            // holds is no longer the last text either side had.
            self.last.clear();
            return true;
        }
        self.last = c.text.clone();
        self.incoming = Some(c.text);
        self.note = Some((
            if c.truncated {
                "Copied from the PC (cut at 32 KB)".to_string()
            } else {
                "Copied from the PC".to_string()
            },
            Instant::now(),
        ));
        true
    }
}

/// The same text, whatever its line endings: a Windows PC reads back
/// what it was sent with CRLF turned into LF.
fn same_text(a: &str, b: &str) -> bool {
    a == b
        || ((a.contains('\r') || b.contains('\r'))
            && a.replace("\r\n", "\n") == b.replace("\r\n", "\n"))
}

enum Fail {
    Unsupported,
    Other(String),
}

fn send(ip: Ipv4Addr, fitted: &Clipboard) -> Result<(), Fail> {
    let body = serde_json::to_string(fitted).map_err(|e| Fail::Other(e.to_string()))?;
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
    let mut pokes_seen = 0u64;
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
        let result = fetch(ip);
        shared.lock().polled = pokes_seen;
        match result {
            Ok(c) => {
                failures = 0;
                if shared.lock().observe(c, &mut primed) {
                    ctx.request_repaint();
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
                sleep_interruptible(POLL * failures.min(10), &stop);
            }
        }
    }
}

fn sleep_interruptible(duration: Duration, stop: &AtomicBool) {
    let step = Duration::from_millis(100);
    let mut elapsed = Duration::ZERO;
    while elapsed < duration && !stop.load(Ordering::Relaxed) {
        let chunk = step.min(duration - elapsed);
        std::thread::sleep(chunk);
        elapsed += chunk;
    }
}

/// Whether a paste's text is worth sending. `Event::Paste` carries text
/// only when the Mac's clipboard holds some; a ⌘V with an image or
/// nothing there is seen by its V key's release instead (see the stream
/// view) and pastes with empty text, which is never sent: the PC pastes
/// what it has.
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

    fn sync() -> Sync {
        Sync {
            ip: Ipv4Addr::LOCALHOST,
            shared: Arc::default(),
            stop: Arc::new(AtomicBool::new(true)),
            ctx: egui::Context::default(),
            pastes: AtomicU64::new(0),
        }
    }

    fn pc(text: &str, seq: u64) -> Clipboard {
        Clipboard {
            text: text.into(),
            seq,
            ..Default::default()
        }
    }

    #[test]
    fn a_paste_the_pc_already_has_is_pressed_at_once() {
        let s = sync();
        s.shared.lock().last = "hello".into();
        assert_eq!(s.push("hello".into(), None), Paste::Now);
        assert_eq!(
            s.push(String::new(), None),
            Paste::Now,
            "an image on the Mac"
        );
        assert!(worth_sending("x"));
        assert!(worth_sending("  \n"), "whitespace is valid clipboard text");
        assert!(!worth_sending(""));
        assert!(s.take_incoming().is_none());
        s.shared.lock().incoming = Some("from pc".into());
        assert_eq!(s.take_incoming().as_deref(), Some("from pc"));
        assert!(s.take_incoming().is_none());
        // Paste ids count up; a paste is ready once its id is reached.
        s.shared.lock().finish(2);
        assert!(s.paste_ready(1) && s.paste_ready(2) && !s.paste_ready(3));
    }

    #[test]
    fn files_or_an_image_copied_on_the_pc_are_pasted_there_while_the_mac_is_unchanged() {
        let mut sh = Shared::default();
        let mut primed = false;
        assert!(!sh.observe(pc("old pc text", 1), &mut primed));
        // The PC copies files: no text, a new sequence number.
        assert!(sh.observe(pc("", 2), &mut primed));
        assert!(sh.incoming.is_none());
        assert!(sh.pc_copied);
        // The window weighs it against the Mac clipboard's change count.
        let s = sync();
        *s.shared.lock() = sh;
        s.note_mac_clipboard(|| Some(41));
        assert!(!s.shared.lock().pc_copied);
        let mac = Clipboard::fit("mac text", 0);
        let sh = s.shared.lock();
        assert!(
            !sh.needs(&mac, Some(41)),
            "the Mac has not changed: paste the files"
        );
        assert!(
            sh.needs(&mac, Some(42)),
            "the Mac copied since: send its text"
        );
        assert!(sh.needs(&mac, None), "no change count: as before");
        drop(sh);
        // Copying the old text again after the files is a copy too.
        let mut sh = s.shared.lock();
        assert!(sh.observe(pc("old pc text", 3), &mut primed));
        assert_eq!(sh.incoming.as_deref(), Some("old pc text"));
    }

    #[test]
    fn text_copied_on_the_pc_comes_over_once() {
        let mut sh = Shared::default();
        let mut primed = false;
        sh.observe(pc("", 1), &mut primed);
        assert!(sh.observe(pc("copied there", 2), &mut primed));
        assert_eq!(sh.incoming.take().as_deref(), Some("copied there"));
        assert_eq!(sh.last, "copied there");
        assert!(!sh.observe(pc("copied there", 2), &mut primed), "same seq");
        // The Mac now holds that text: a paste of it sends nothing.
        assert!(!sh.needs(&Clipboard::fit("copied there", 0), None));
    }

    #[test]
    fn the_pc_reading_back_a_paste_is_not_a_copy() {
        let mut sh = Shared::default();
        let mut primed = false;
        sh.observe(pc("before", 1), &mut primed);
        let fitted = Clipboard::fit("from the mac", 0);
        let before = sh.sending(&fitted);
        assert_eq!(before, "before");
        sh.sent(&fitted, 1);
        assert!(!sh.observe(pc("from the mac", 2), &mut primed));
        assert!(sh.incoming.is_none() && !sh.pc_copied);
        // A Windows PC reads CRLF back as LF.
        let crlf = Clipboard::fit("a\r\nb", 0);
        sh.sending(&crlf);
        sh.sent(&crlf, 2);
        assert!(!sh.observe(pc("a\nb", 3), &mut primed));
        assert!(sh.incoming.is_none());
        assert!(!sh.needs(&Clipboard::fit("a\nb", 0), None));
        // A send that failed puts the old text back, so the next paste of
        // the same text tries again.
        let fitted = Clipboard::fit("new", 0);
        let before = sh.sending(&fitted);
        sh.not_sent(&fitted, before);
        assert_eq!(sh.last, "a\r\nb");
        assert!(sh.needs(&fitted, None));
    }

    #[test]
    fn a_long_paste_is_cut_and_the_cut_copy_never_comes_back() {
        let long = "x".repeat(latch_core::api::CLIPBOARD_MAX_BYTES + 100);
        let fitted = Clipboard::fit(&long, 0);
        assert!(fitted.truncated);
        let mut sh = Shared::default();
        let mut primed = false;
        sh.observe(pc("", 1), &mut primed);
        sh.sending(&fitted);
        sh.sent(&fitted, 1);
        assert!(sh.note.as_ref().unwrap().0.contains("first 32 KB"));
        // The PC reads back the cut text under a new number.
        assert!(!sh.observe(pc(&fitted.text, 2), &mut primed));
        assert!(sh.incoming.is_none(), "the Mac keeps its whole text");
        assert!(!sh.pc_copied);
        // Pasting the same long text again sends nothing new.
        assert!(!sh.needs(&Clipboard::fit(&long, 0), None));
        // A subsequent copy on the PC with a new sequence (even if matching the truncated text)
        // must be recognized as a copy made on the PC so subsequent pastes do not overwrite it.
        assert!(sh.observe(pc(&fitted.text, 3), &mut primed));
        assert!(sh.pc_copied);
    }

    #[test]
    fn a_paste_the_window_gave_up_on_says_so_when_it_lands() {
        let s = sync();
        s.give_up(3);
        let mut sh = s.shared.lock();
        sh.sent(&Clipboard::fit("x", 0), 3);
        assert!(sh.note.as_ref().unwrap().0.contains("paste again"));
        sh.sent(&Clipboard::fit("x", 0), 4);
        assert_eq!(sh.note.as_ref().unwrap().0, "Clipboard sent to the PC");
    }

    #[test]
    fn poll_backoff_sleep_wakes_promptly_on_stop() {
        let stop = Arc::new(AtomicBool::new(false));
        let stop_clone = stop.clone();
        let start = Instant::now();
        let handle = std::thread::spawn(move || {
            sleep_interruptible(Duration::from_secs(5), &stop_clone);
        });
        std::thread::sleep(Duration::from_millis(30));
        stop.store(true, Ordering::Relaxed);
        handle.join().unwrap();
        assert!(
            start.elapsed() < Duration::from_millis(500),
            "backoff sleep must wake promptly on stop, took {:?}",
            start.elapsed()
        );
    }
}
