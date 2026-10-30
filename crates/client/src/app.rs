//! The window: the machines on this tailnet, this machine's Sharing page,
//! settings, and the stream once connected.

use crate::config::ClientConfig;
#[cfg(test)]
use crate::config::{Codec, Resolution};
use crate::handover::{self, Handover};
use crate::path;
use crate::session::{
    self, Connect, Discovery, Live, Pc, PeerRelayServers, Progress, Step, Target,
};
use crate::share;
use crate::stream::{self, Action, Env};
use crate::update;
use eframe::egui;
use latch_core::api::PowerAction;
use latch_core::tailscale;
use latch_stream::Event;
use latch_ui::{self as ui, column, space, Icon, Tone, PALETTE as P};
use parking_lot::Mutex;
use semver::Version;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

const RELAY_NONE: &str = "When two machines can't reach each other directly, Tailscale's relays carry the stream. For a shorter detour, run your own relay with Latch's deploy kit.";
const RELAY_READY: &str =
    "Your relay carries streams between machines that can't connect directly.";
const RELAY_OFFLINE: &str = "Your relay is offline. Until it is back, Tailscale's relays carry streams between machines that can't connect directly.";
const RELAY_CHECKING: &str = "A relay is on this tailnet, but Latch couldn't confirm that this device may use it. That does not mean access is denied. Peer relays need Tailscale 1.86 or later on every device.";
const RELAY_UNAVAILABLE: &str = "A relay is on this tailnet but isn't available to this device yet. That does not mean access is denied: check that the relay is configured, and add this grant to the tailnet policy if it is missing.";
#[cfg(test)]
const RELAY_UNGRANTED: &str = "A relay node is online but this device is not granted access.";
const RELAY_GRANT: &str = "{\n  \"src\": [\"autogroup:member\"],\n  \"dst\": [\"tag:relay\"],\n  \"app\": {\n    \"tailscale.com/cap/relay\": []\n  }\n}";
const RELAY_DOCS: &str = "https://tailscale.com/docs/features/peer-relay";
const DOWNLOAD_URL: &str = "https://github.com/MrBeldum/latch/releases/latest";
const SOURCE_URL: &str = "https://github.com/MrBeldum/latch";

/// How long a one-line result stays on the machine list.
const NOTICE_FOR: Duration = Duration::from_secs(12);

/// A one-line result, filled in by a worker thread.
type Notice = Arc<Mutex<Option<(Tone, String)>>>;

/// A PC's fixed desktop size and frame-rate ceiling, from its last status.
type HostLimits = (Option<[u32; 2]>, u32);

/// A question about one machine, waiting for a second click. The machine
/// is its node id: names are not unique on a tailnet.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Confirm {
    node_id: String,
    name: String,
    action: Confirmable,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Confirmable {
    Power(PowerAction),
    /// Forget what Latch saved about it, its saved addresses included.
    Forget,
}

/// The window's pages, one per tab.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Page {
    Machines,
    /// This machine's sharing: setup and status.
    Sharing,
    Settings,
}

/// Give the window the stream's proportions, keeping its width, so the
/// picture fills it with no bar on any side. Only in a window: full screen
/// is the screen's shape, which Match screen already is.
fn fit_window_to(ctx: &egui::Context, width: u32, height: u32) {
    if width == 0 || height == 0 {
        return;
    }
    let size = ctx.screen_rect().size();
    let want = window_size_for(size, width as f32 / height as f32);
    if (want.y - size.y).abs() >= 1.0 {
        ctx.send_viewport_cmd(egui::ViewportCommand::InnerSize(want));
    }
}

/// `current` with its height changed to give `aspect`, never under the
/// window's minimum.
fn window_size_for(current: egui::Vec2, aspect: f32) -> egui::Vec2 {
    let width = current.x.max(640.0);
    egui::Vec2::new(width, (width / aspect).round().max(420.0))
}

/// "Command-," on a Mac, "Ctrl+," elsewhere: how a window shortcut is
/// written in a tooltip.
fn chord(key: &str) -> String {
    if cfg!(target_os = "macos") {
        format!("Command-{key}")
    } else {
        format!("Ctrl+{key}")
    }
}

pub struct ClientApp {
    cfg: ClientConfig,
    dirty: bool,
    discovery: Arc<Mutex<Discovery>>,
    progress: Arc<Mutex<Progress>>,
    live: Arc<Mutex<Option<Live>>>,
    view: stream::View,
    /// Tailscale's CLI is on this machine. Probed off the UI thread: when it
    /// is missing, looking for it runs a process.
    tailscale_ok: Arc<AtomicBool>,
    brand: ui::Brand,
    page: Page,
    /// Whether the window is full screen, as the system last reported it.
    fullscreen: bool,
    /// Full screen or not once streaming is over: what the window was when
    /// the stream first connected. Kept across a reconnect.
    fullscreen_after: Option<bool>,
    /// A restart, a shutdown or forgetting a machine waits for a second
    /// click.
    confirm: Option<Confirm>,
    notice: Option<(Tone, String, Instant)>,
    /// After a session ends, offer to sleep this PC.
    offer_sleep: Option<Pc>,
    /// A power action went to the machine of the current attempt: its end
    /// is no failure, and no reason to offer sleep. Cleared if refused.
    power_sent: Arc<AtomicBool>,
    ended_seen: bool,
    pending_notice: Option<Notice>,
    updates: Arc<Mutex<update::State>>,
    /// The PC last connected to, for a reconnect at another quality.
    last_pc: Option<Pc>,
    /// Connect again as soon as the current stream has stopped.
    reconnect: Option<Pc>,
    /// The last attempt reached the picture before it ended.
    streamed: bool,
    /// The last attempt failed in the stream itself (not while waking,
    /// starting the stream), where a lighter profile can help.
    stream_failed: bool,
    /// What the PC said about its black picture, and whether a fix is on
    /// its way. Asked for once per stream, by the thread that answers.
    video_help: Arc<Mutex<Option<crate::display::Help>>>,
    asked_about_video: bool,
    /// An install of Latch Host through the stream, while it runs and a
    /// little after.
    handover: Option<Handover>,
    display_at_connect: (u32, u32),
    display_change: Option<((u32, u32), Instant)>,
    /// Present when this process also shares this machine.
    pub local: Option<share::Slot>,
    share_page: Option<Box<dyn share::SharePage>>,
}

impl ClientApp {
    pub fn new(cc: &eframe::CreationContext<'_>) -> Self {
        let discovery = Arc::new(Mutex::new(Discovery::default()));
        session::spawn_discovery(discovery.clone(), cc.egui_ctx.clone());
        let app = Self::with_shared(cc, discovery.clone(), Arc::default(), ClientConfig::load());
        spawn_tailscale_probe(app.tailscale_ok.clone(), cc.egui_ctx.clone());
        update::spawn(
            app.updates.clone(),
            discovery,
            app.live.clone(),
            app.progress.clone(),
            cc.egui_ctx.clone(),
        );
        app
    }

    fn with_shared(
        cc: &eframe::CreationContext<'_>,
        discovery: Arc<Mutex<Discovery>>,
        progress: Arc<Mutex<Progress>>,
        cfg: ClientConfig,
    ) -> Self {
        ui::apply(&cc.egui_ctx);
        if let Some(rs) = &cc.wgpu_render_state {
            crate::video::install(rs);
        }
        Self {
            cfg,
            dirty: false,
            discovery,
            progress,
            live: Arc::default(),
            view: stream::View::default(),
            tailscale_ok: Arc::new(AtomicBool::new(true)),
            brand: ui::Brand::new(&cc.egui_ctx),
            page: Page::Machines,
            fullscreen: false,
            fullscreen_after: None,
            confirm: None,
            notice: None,
            offer_sleep: None,
            power_sent: Arc::default(),
            ended_seen: true,
            pending_notice: None,
            updates: Arc::default(),
            last_pc: None,
            reconnect: None,
            streamed: false,
            stream_failed: false,
            video_help: Arc::default(),
            asked_about_video: false,
            handover: None,
            display_at_connect: (0, 0),
            display_change: None,
            local: None,
            share_page: None,
        }
    }

    /// A window with no background threads and the given settings, for
    /// rendering screens in tests (this crate's and the host's).
    #[doc(hidden)]
    pub fn headless(
        cc: &eframe::CreationContext<'_>,
        discovery: Discovery,
        progress: Progress,
        cfg: ClientConfig,
    ) -> Self {
        Self::with_shared(
            cc,
            Arc::new(Mutex::new(discovery)),
            Arc::new(Mutex::new(progress)),
            cfg,
        )
    }

    /// Show this machine in the machine list and its Sharing page as a tab.
    pub fn with_local(mut self, local: share::Slot, page: Box<dyn share::SharePage>) -> Self {
        self.local = Some(local);
        self.share_page = Some(page);
        self
    }

    /// Switch tabs, as a click on one would.
    pub fn open_page(&mut self, page: Page) {
        self.page = if page == Page::Sharing && self.share_page.is_none() {
            Page::Machines
        } else {
            page
        };
    }

    fn commit(&mut self) {
        if self.dirty {
            self.dirty = false;
            if let Err(e) = self.cfg.save() {
                tracing::warn!("could not save settings: {e:#}");
                self.notice = Some((
                    Tone::Danger,
                    format!("Couldn't save settings: {e}. They apply until Latch quits."),
                    Instant::now(),
                ));
            }
        }
    }

    pub(crate) fn native_pixels(ctx: &egui::Context) -> (u32, u32) {
        let ppp = ctx
            .input(|i| i.viewport().native_pixels_per_point)
            .unwrap_or(ctx.pixels_per_point());
        let size = ctx
            .input(|i| i.viewport().monitor_size)
            .unwrap_or_else(|| ctx.screen_rect().size());
        let even = |v: f32| (((v * ppp).round() as u32) / 2) * 2;
        (even(size.x).max(640), even(size.y).max(400))
    }

    fn connect(&mut self, ctx: &egui::Context, pc: &Pc) {
        let Some(target) = Target::from_pc(pc) else {
            return;
        };
        self.ended_seen = false;
        self.offer_sleep = None;
        self.streamed = false;
        self.stream_failed = false;
        self.confirm = None;
        self.power_sent.store(false, Ordering::Relaxed);
        self.last_pc = Some(pc.clone());
        self.asked_about_video = false;
        // A new slot, so an answer still on its way about the last stream
        // lands in the old one.
        self.video_help = Arc::default();
        self.display_at_connect = Self::native_pixels(ctx);
        self.display_change = None;
        session::connect(Connect {
            target,
            settings: self.cfg.stream.clone(),
            native: Self::native_pixels(ctx),
            progress: self.progress.clone(),
            live: self.live.clone(),
            ctx: ctx.clone(),
        });
    }

    fn set_fullscreen(&mut self, ctx: &egui::Context, on: bool) {
        if self.fullscreen != on {
            self.fullscreen = on;
            ctx.send_viewport_cmd(egui::ViewportCommand::Fullscreen(on));
        }
    }

    /// Run `job` off the UI thread and show what it returns as the notice.
    fn notify_later(&mut self, job: impl FnOnce() -> (Tone, String) + Send + 'static) {
        let notice: Notice = Arc::default();
        let out = notice.clone();
        std::thread::spawn(move || {
            if let Ok(res) = std::panic::catch_unwind(std::panic::AssertUnwindSafe(job)) {
                *out.lock() = Some(res);
            }
        });
        self.pending_notice = Some(notice);
    }

    /// The first time a connected stream finds its picture black, ask the
    /// PC what it can see. Only then: a picture that stopped arriving or
    /// cannot be decoded is the network's or this Mac's doing, which the PC
    /// cannot explain. One question per stream: the answer does not change
    /// while the same capture keeps failing. Every way it ends leaves an
    /// answer, so "Asking the PC why…" never stays up.
    fn ask_why_black(&mut self, ctx: &egui::Context, live: &Live, os: &str) {
        let problem = live.session.stats().video_problem;
        if self.asked_about_video
            || !problem
                .as_deref()
                .is_some_and(crate::display::is_black_picture)
            || !live.session.connected()
        {
            return;
        }
        self.asked_about_video = true;
        let help = self.video_help.clone();
        if !crate::display::can_explain(os) {
            *help.lock() = Some(crate::display::no_answer(os));
            return;
        }
        let (ip, ctx, os) = (live.ip, ctx.clone(), os.to_string());
        std::thread::spawn(move || {
            let answer = match crate::display::ask(ip) {
                Ok(report) => crate::display::verdict(&report),
                Err(e) => {
                    tracing::warn!("display report: {e:#}");
                    Default::default()
                }
            };
            *help.lock() = Some(if answer.message.is_empty() {
                crate::display::no_answer(&os)
            } else {
                answer
            });
            ctx.request_repaint();
        });
    }

    /// Turn the PC's HDR desktop off, then start a fresh capture: the old
    /// one keeps producing the black frames it was already producing.
    fn turn_off_hdr(&mut self, _ctx: &egui::Context, ip: std::net::Ipv4Addr) {
        let help = self.video_help.clone();
        if help.lock().as_ref().is_some_and(|h| h.busy) {
            return;
        }
        if let Some(h) = help.lock().as_mut() {
            h.busy = true;
        }
        self.notify_later(move || {
            let told = match crate::display::set_hdr(ip, false) {
                Ok(state) => {
                    if crate::display::hdr_is_on(&serde_json::json!({"advanced_color": state})) {
                        (Tone::Danger, "The PC kept its HDR desktop on.".to_string())
                    } else {
                        (
                            Tone::Success,
                            "HDR is off on the PC. Starting a fresh capture…".to_string(),
                        )
                    }
                }
                Err(e) => (Tone::Danger, format!("Couldn't turn HDR off: {e}")),
            };
            if let Some(h) = help.lock().as_mut() {
                h.busy = false;
                h.hdr_is_on = told.0 != Tone::Success;
            }
            told
        });
        // The picture cannot recover on the capture that is already black.
        self.reconnect = self.last_pc.clone();
        self.disconnect();
    }

    /// Ask the machine at `ip` to sleep, restart or shut down. A stream from
    /// it, or a connection to it, ends once it has agreed (its host ends
    /// the stream itself first), so a refusal leaves the stream up. A
    /// connection to any other machine is none of this action's business.
    fn power(&mut self, ip: std::net::Ipv4Addr, name: &str, action: PowerAction) {
        let name = name.to_string();
        let ours = self.is_current_target(ip);
        if ours {
            self.power_sent.store(true, Ordering::Relaxed);
            if self.live.lock().is_some() {
                self.view.toast(
                    Tone::Neutral,
                    format!("Asking {name} to {}…", action.label().to_lowercase()),
                );
            }
        }
        let (live, progress, sent) = (
            self.live.clone(),
            self.progress.clone(),
            self.power_sent.clone(),
        );
        self.notify_later(move || match session::power(ip, action) {
            Ok(()) => {
                if ours {
                    end_connection_to(&live, &progress, ip);
                }
                (
                    Tone::Success,
                    format!("{name}: {} requested.", action.label().to_lowercase()),
                )
            }
            Err(e) => {
                if ours {
                    sent.store(false, Ordering::Relaxed);
                }
                (Tone::Danger, format!("{name}: {e}"))
            }
        });
    }

    /// The machine this window is streaming from or connecting to.
    fn is_current_target(&self, ip: std::net::Ipv4Addr) -> bool {
        let live_ip = self.live.lock().as_ref().map(|l| l.ip);
        targets(live_ip, &self.progress.lock(), ip)
    }

    fn test_wake(&mut self, pc: &Pc) {
        let pc = pc.clone();
        self.notify_later(move || match session::wake_test(&pc) {
            Ok(true) => (
                Tone::Success,
                format!("{} received the wake packet, so waking it from this network works.", pc.name),
            ),
            Ok(false) => (
                Tone::Warning,
                format!(
                    "The wake packet didn't reach {} from this network. Waking it from here needs its router to forward UDP port 9 to it.",
                    pc.name
                ),
            ),
            Err(e) => (Tone::Danger, format!("{}: {e}", pc.name)),
        });
    }

    fn disconnect(&mut self) {
        let mut p = self.progress.lock();
        if p.active() {
            p.cancel = true;
        }
        drop(p);
        if let Some(l) = self.live.lock().as_ref() {
            l.input.release_all();
            l.session.stop();
        }
    }

    /// Move the stream's events into the progress state; drop the stream once
    /// it has fully stopped.
    fn poll_live(&mut self, ctx: &egui::Context) {
        let mut fullscreen = None;
        {
            let mut guard = self.live.lock();
            let Some(live) = guard.as_mut() else { return };
            if live.codec.is_empty() {
                let s = live.session.stats();
                if !s.codec.is_empty() {
                    live.codec = s.codec;
                }
            }
            let mut prog = self.progress.lock();
            while let Ok(ev) = live.events.try_recv() {
                match ev {
                    Event::Stage(s) => {
                        if prog.step == Step::Connecting {
                            prog.detail = s;
                        }
                    }
                    Event::Connected => {
                        prog.step = Step::Streaming;
                        prog.since = Instant::now();
                        self.streamed = true;
                        self.fullscreen_after.get_or_insert(self.fullscreen);
                        if self.cfg.stream.fullscreen {
                            fullscreen = Some(true);
                        } else {
                            fit_window_to(ctx, live.requested.0, live.requested.1);
                        }
                        self.view.stream_started(ctx, live, self.cfg.capture_mouse);
                        if let Some(problem) = host_audio_problem(&self.discovery, &live.node_id) {
                            self.view.toast(
                                Tone::Warning,
                                format!(
                                    "{} has no sound to send: it reports “{problem}”. Its Sharing page in Latch says more.",
                                    live.pc
                                ),
                            );
                        }
                    }
                    Event::Failed { stage, message } => {
                        let cancelled = prog.cancel || self.power_sent.load(Ordering::Relaxed);
                        tracing::warn!(
                            "{}: the stream failed at the {stage} step: {message}{}",
                            live.pc,
                            if cancelled { " after a stop" } else { "" }
                        );
                        self.stream_failed = !cancelled;
                        prog.step = Step::Ended {
                            error: (!cancelled).then(|| stream_failure(&message)),
                        };
                    }
                    Event::Terminated { code, message } => {
                        let cancelled = prog.cancel || self.power_sent.load(Ordering::Relaxed);
                        let error = code != 0 && code != 1 && !cancelled;
                        if code == 1 && !cancelled {
                            self.notice = Some((Tone::Info, message.clone(), Instant::now()));
                        }
                        if code != 0 {
                            tracing::warn!(
                                "{}: the stream ended, code {code}: {message}{}",
                                live.pc,
                                if cancelled { " (after a stop)" } else { "" }
                            );
                        } else {
                            tracing::info!("{}: the stream ended: {message}", live.pc);
                        }
                        self.stream_failed = error;
                        prog.step = Step::Ended {
                            error: error.then_some(message),
                        };
                    }
                    Event::Notice(text) => self.view.toast(Tone::Warning, text),
                    Event::Poor(p) => self.view.set_poor(p),
                    Event::NoAudio(e) => self
                        .view
                        .toast(Tone::Warning, format!("No sound on this machine: {e}.")),
                }
            }
            // A stop we asked for ends without a Terminated event.
            let ended = matches!(prog.step, Step::Ended { .. });
            if live.session.finished() {
                if !ended {
                    prog.step = Step::Ended { error: None };
                }
                self.view.reset(ctx, Some(live));
                *guard = None;
                fullscreen =
                    fullscreen_when_finished(self.reconnect.is_some(), &mut self.fullscreen_after);
            } else if ended {
                live.session.stop();
            }
        }
        if let Some(on) = fullscreen {
            self.set_fullscreen(ctx, on);
        }
    }

    /// Reconnect when the window has moved to a display of another size
    /// (held for a second), if that changes what the stream asks for: it
    /// does for Match screen, and not for a fixed size or for a PC whose
    /// fixed desktop is smaller than both displays. `asked` is what the
    /// stream asked for; `limits` the PC's fixed desktop and frame-rate
    /// ceiling, when discovery knows them (without them, it reconnects).
    fn follow_display(
        &mut self,
        ctx: &egui::Context,
        (asked, limits): ((u32, u32, u32), Option<HostLimits>),
    ) {
        let size = Self::native_pixels(ctx);
        if size == self.display_at_connect || self.reconnect.is_some() {
            self.display_change = None;
            return;
        }
        match self.display_change {
            Some((candidate, at))
                if candidate == size && at.elapsed() >= Duration::from_secs(1) =>
            {
                let changes = limits.is_none_or(|(capture_size, max_fps)| {
                    session::request_for(&self.cfg.stream, size, capture_size, max_fps) != asked
                });
                if changes {
                    self.reconnect = self.last_pc.clone();
                    self.disconnect();
                } else {
                    // Nothing to ask for anew: this display is the one the
                    // stream is for now.
                    self.display_at_connect = size;
                    self.display_change = None;
                }
            }
            Some((candidate, _)) if candidate == size => {}
            _ => self.display_change = Some((size, Instant::now())),
        }
    }

    fn poll_notice(&mut self) {
        let (ready, done) = self.pending_notice.as_ref().map_or((None, false), |n| {
            let val = n.lock().take();
            let finished = val.is_some() || Arc::strong_count(n) == 1;
            (val, finished)
        });
        if let Some((tone, text)) = ready {
            // The machine list is out of sight while a stream runs: a
            // refused power action, say, has to be read over the picture.
            if self.live.lock().is_some() {
                self.view.toast(tone, text.clone());
            }
            self.notice = Some((tone, text, Instant::now()));
        }
        if done {
            self.pending_notice = None;
        }
    }

    fn tailscale_installed(&self) -> bool {
        self.tailscale_ok.load(Ordering::Relaxed)
    }

    /// Window shortcuts: Command-1/2/3 (Ctrl elsewhere) switch tabs,
    /// Command-, opens Settings, Escape goes back to the machine list or
    /// drops a pending question.
    fn shortcuts(&mut self, ctx: &egui::Context) {
        use egui::{Key, KeyboardShortcut, Modifiers};
        let shortcut = |key| KeyboardShortcut::new(Modifiers::COMMAND, key);
        let busy = ctx.memory(|m| m.any_popup_open()) || ctx.wants_keyboard_input();
        let (settings, one, two, three, escape) = ctx.input_mut(|i| {
            (
                i.consume_shortcut(&shortcut(Key::Comma)),
                i.consume_shortcut(&shortcut(Key::Num1)),
                i.consume_shortcut(&shortcut(Key::Num2)),
                i.consume_shortcut(&shortcut(Key::Num3)),
                !busy && i.key_pressed(Key::Escape),
            )
        });
        let has_sharing = self.share_page.is_some();
        if settings || three || (two && !has_sharing) {
            self.page = Page::Settings;
        } else if two {
            self.page = Page::Sharing;
        } else if one {
            self.page = Page::Machines;
        }
        if escape {
            if self.confirm.is_some() {
                self.confirm = None;
            } else {
                self.page = Page::Machines;
            }
        }
    }
}

/// Full screen or not once a stream has finished. A restart, new settings
/// or a new display reconnect at once, so the window stays as it is rather
/// than leaving full screen and coming back; otherwise it goes back to what
/// it was before the stream (`after`), if the stream ever showed.
fn fullscreen_when_finished(reconnecting: bool, after: &mut Option<bool>) -> Option<bool> {
    if reconnecting {
        None
    } else {
        after.take()
    }
}

/// Where to send a power action meant for the machine `node_id`: the
/// listed machine with that id, if it answers and allows it. Never another
/// machine of the same name.
fn power_target(disc: &Discovery, node_id: &str) -> Option<std::net::Ipv4Addr> {
    disc.pcs
        .iter()
        .find(|p| p.node_id == node_id && !p.remembered && p.online && p.power_allowed())
        .and_then(|p| p.ip)
}

/// What forgetting a machine does, for the question before it.
fn forget_question(disc: &Discovery, node_id: &str, name: &str) -> String {
    let listed = disc
        .pcs
        .iter()
        .any(|p| p.node_id == node_id && !p.remembered);
    if listed {
        format!("Latch forgets how to wake {name}. It stays in this list while it is on your Tailscale network; remove it in Tailscale's admin console to retire it.")
    } else {
        format!("Latch forgets {name}'s address and how to wake it. If it is still on your Tailscale network, it is listed again once Tailscale is back.")
    }
}

/// Whether `ip` is the machine of the stream (`live_ip`) or of the
/// connection under way.
fn targets(
    live_ip: Option<std::net::Ipv4Addr>,
    progress: &Progress,
    ip: std::net::Ipv4Addr,
) -> bool {
    live_ip == Some(ip) || (progress.active() && progress.ip == Some(ip))
}

/// End the stream from `ip`, or the connection to it, from a worker: what
/// a disconnect does, for that machine only. One lock at a time, as the
/// window takes them.
fn end_connection_to(
    live: &Mutex<Option<Live>>,
    progress: &Mutex<Progress>,
    ip: std::net::Ipv4Addr,
) {
    {
        let mut p = progress.lock();
        if p.active() && p.ip == Some(ip) {
            p.cancel = true;
        }
    }
    if let Some(l) = live.lock().as_ref().filter(|l| l.ip == ip) {
        l.session.stop();
    }
}

/// Look for Tailscale's CLI every few seconds on a thread of its own, and
/// repaint when the answer changes.
fn spawn_tailscale_probe(ok: Arc<AtomicBool>, ctx: egui::Context) {
    std::thread::spawn(move || loop {
        let found = tailscale::cli().is_some();
        if ok.swap(found, Ordering::Relaxed) != found {
            ctx.request_repaint();
        }
        std::thread::sleep(Duration::from_secs(if found { 30 } else { 3 }));
    });
}

impl eframe::App for ClientApp {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        // The window's green button (or Globe-F) changes full screen behind
        // the app's back: what the system reports is what is true.
        if let Some(on) = ctx.input(|i| i.viewport().fullscreen) {
            self.fullscreen = on;
        }
        self.poll_live(ctx);
        self.poll_notice();

        let streaming = self.live.lock().is_some();
        if streaming {
            // The stream repaints itself on every decoded frame.
            ctx.request_repaint_after(Duration::from_millis(250));
            let live = self.live.clone();
            let guard = live.lock();
            if let Some(l) = guard.as_ref() {
                // Text copied on the PC lands in this Mac's clipboard.
                if let Some(text) = l.clipboard.take_incoming() {
                    ctx.copy_text(text);
                }
                let disc = self.discovery.lock().clone();
                let pc_now = disc.pcs.iter().find(|p| p.node_id == l.node_id);
                let latest = self.updates.lock().latest.clone();
                let old_host = pc_now
                    .and_then(|p| p.host.as_ref())
                    .and_then(|h| stream::old_host(&h.version, latest.as_ref()));
                self.tend_handover(pc_now, &l.pc);
                let os = pc_now.map(|p| p.os.clone()).unwrap_or_default();
                self.ask_why_black(ctx, l, &os);
                // The PC's answer is about a black picture: it goes under
                // that headline and no other.
                let black = l
                    .session
                    .stats()
                    .video_problem
                    .as_deref()
                    .is_some_and(crate::display::is_black_picture);
                let help = self.video_help.lock().clone().filter(|_| black);
                let env = Env {
                    live: l,
                    cfg: &self.cfg,
                    fullscreen: self.fullscreen,
                    path: pc_now.map(|p| p.path.clone()),
                    os,
                    old_host,
                    handover: self.handover.as_ref().map(|h| h.status(&l.pc)),
                    video_help: help,
                    // Discovery's fresh word on it; without that, nothing.
                    power_allowed: pc_now.is_some_and(|p| p.power_allowed()),
                    can_sleep: pc_now.is_some_and(|p| p.can_sleep()),
                };
                let actions = self.view.show(ctx, &env);
                let (ip, name, input) = (l.ip, l.pc.clone(), l.input.clone());
                // What a reconnect for a new display would ask for, beside
                // what this stream asked for.
                let display_request = (
                    l.requested,
                    pc_now
                        .and_then(|p| p.host.as_ref())
                        .map(|h| (h.streamer.capture_size, h.streamer.max_fps)),
                );
                drop(guard);
                for a in actions {
                    match a {
                        Action::Disconnect => self.disconnect(),
                        Action::Power(action) => self.power(ip, &name, action),
                        Action::Fullscreen(on) => {
                            self.set_fullscreen(ctx, on);
                            self.cfg.stream.fullscreen = on;
                            self.dirty = true;
                        }
                        Action::ToggleCmd => {
                            self.cfg.cmd_is_ctrl = !self.cfg.cmd_is_ctrl;
                            self.dirty = true;
                        }
                        Action::MouseCapture(on) => {
                            self.cfg.capture_mouse = on;
                            self.dirty = true;
                        }
                        Action::ApplySettings(settings) => {
                            self.cfg.stream = settings;
                            self.dirty = true;
                            self.reconnect = self.last_pc.clone();
                            self.disconnect();
                        }
                        Action::TurnOffHdr => self.turn_off_hdr(ctx, ip),
                        Action::InstallHost => {
                            self.start_handover(ctx, ip, &name, input.clone(), &disc)
                        }
                    }
                }
                self.follow_display(ctx, display_request);
            }
            self.commit();
            return;
        }

        // Nothing here repaints on a timer when idle: discovery, the
        // updater, the local service and the connect worker each ask for a
        // repaint when what they know changes. Only the clocks below do.
        let disc = self.discovery.lock().clone();
        let prog = self.progress.lock().clone();
        if let Some(pc) = self.reconnect.take() {
            if prog.active() {
                self.reconnect = Some(pc);
                ctx.request_repaint_after(Duration::from_millis(250));
            } else {
                // Fresh details if discovery has them; the saved ones otherwise.
                let fresh = disc
                    .pcs
                    .iter()
                    .find(|p| p.node_id == pc.node_id)
                    .cloned()
                    .unwrap_or(pc);
                self.connect(ctx, &fresh);
                self.commit();
                return;
            }
        }
        if let Step::Ended { .. } = &prog.step {
            if !self.ended_seen {
                self.ended_seen = true;
                // A reconnect that failed before its stream came back: the
                // window leaves full screen as the stream's end would have.
                if let Some(on) = self.fullscreen_after.take() {
                    self.set_fullscreen(ctx, on);
                }
                if self.streamed && self.cfg.sleep_prompt && !self.power_sent.load(Ordering::Relaxed) {
                    self.offer_sleep = disc
                        .pcs
                        .iter()
                        .find(|p| p.node_id == prog.node_id && p.can_sleep())
                        .cloned();
                }
            }
        }
        if let Some((_, _, at)) = &self.notice {
            match NOTICE_FOR.checked_sub(at.elapsed()) {
                Some(left) => ctx.request_repaint_after(left),
                None => self.notice = None,
            }
        }
        if self.pending_notice.is_some() {
            ctx.request_repaint_after(Duration::from_millis(250));
        }
        self.shortcuts(ctx);

        ui::top_bar(ctx, "top", |ui| {
            self.brand.lockup(ui, "Latch");
            ui.add_space(space::XL);
            let mut pages = vec![(Page::Machines, "Machines", chord("1"))];
            if self.share_page.is_some() {
                pages.push((Page::Sharing, "Sharing", chord("2")));
            }
            pages.push((Page::Settings, "Settings", chord(",")));
            ui.scope(|ui| {
                ui::tabs(ui, &pages, &mut self.page);
            });
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                let (tone, label) = if !self.tailscale_installed() {
                    (Tone::Danger, "Tailscale not installed")
                } else if disc.error.is_some() {
                    (Tone::Danger, "Tailscale off")
                } else if prog.active() {
                    (Tone::Accent, "Connecting")
                } else if disc.refreshed.is_none() {
                    (Tone::Neutral, "Looking for machines")
                } else {
                    (Tone::Success, "Tailscale on")
                };
                ui::status_text(ui, tone, label);
            });
        });

        ui::bottom_bar(ctx, "bottom", |ui| {
            let mut line = format!("Latch {}", env!("CARGO_PKG_VERSION"));
            if !disc.login.is_empty() {
                line.push_str(&format!("  ·  {}", disc.login));
            }
            if let Some(v) = self.updates.lock().ready.clone() {
                line.push_str(&format!("  ·  {v} installs after this session"));
            }
            ui.add(egui::Label::new(line).truncate());
        });

        egui::CentralPanel::default()
            .frame(egui::Frame::new().fill(P.bg))
            .show(ctx, |ui| {
                egui::ScrollArea::vertical()
                    .id_salt(("page", self.page as u8))
                    .show(ui, |ui| {
                        ui.add_space(space::XL);
                        let width = match self.page {
                            Page::Machines => column::WIDE,
                            _ => column::NARROW,
                        };
                        ui::content_column(ui, width, |ui| {
                            ui.spacing_mut().item_spacing.y = space::LG;
                            match self.page {
                                Page::Machines => self.machines_page(ui, ctx, &disc, &prog),
                                Page::Sharing => match self.share_page.as_mut() {
                                    Some(page) => page.show(ui),
                                    None => self.page = Page::Machines,
                                },
                                Page::Settings => self.settings_page(ui, ctx, &disc, &prog),
                            }
                            ui.add_space(space::XL);
                        });
                    });
            });
        self.commit();
    }
}

impl ClientApp {
    // -----------------------------------------------------------------------
    // Machines
    // -----------------------------------------------------------------------

    fn machines_page(
        &mut self,
        ui: &mut egui::Ui,
        ctx: &egui::Context,
        disc: &Discovery,
        prog: &Progress,
    ) {
        let online = disc.pcs.iter().filter(|p| p.online).count();
        let summary = match disc.pcs.len() {
            0 => String::new(),
            1 => format!("1 machine · {online} online"),
            n => format!("{n} machines · {online} online"),
        };
        ui::page_header(ui, "Machines", None, |ui| {
            ui::small_print(ui, summary);
        });
        self.tailscale_banner(ui, disc);
        if let Some((tone, text, _)) = self.notice.clone() {
            ui::notice(ui, tone, &text);
        }
        if let Some((tone, text)) = self.updates.lock().notice.clone() {
            ui::notice(ui, tone, &text);
        }
        for (_, text) in key_expiry_warnings(disc)
            .iter()
            .filter(|(urgent, _)| *urgent)
        {
            ui::notice(ui, Tone::Danger, text);
        }
        if prog.active() {
            self.session_card(ui, prog);
        } else if let Step::Ended { error } = &prog.step {
            self.ended_card(ui, prog, error.as_deref());
        }
        self.confirm_banner(ui, disc);
        self.machine_list(ui, ctx, disc, prog);
        self.next_stream(ui, ctx);
        self.details(ui, disc);
    }

    fn tailscale_banner(&mut self, ui: &mut egui::Ui, disc: &Discovery) {
        if !self.tailscale_installed() {
            ui::banner(
                ui,
                Tone::Danger,
                "Tailscale isn't installed",
                Some("Latch finds your machines and connects to them through Tailscale. Install it and sign in with the account your other machines use; they appear here within seconds."),
                |ui| {
                    if ui::primary_button(ui, "Get Tailscale").clicked() {
                        let url = if cfg!(windows) {
                            "https://tailscale.com/download/windows"
                        } else if cfg!(target_os = "linux") {
                            "https://tailscale.com/download/linux"
                        } else {
                            "https://tailscale.com/download/mac"
                        };
                        ui.ctx().open_url(egui::OpenUrl::new_tab(url));
                    }
                },
            );
        } else if let Some(e) = &disc.error {
            ui::banner(
                ui,
                Tone::Danger,
                "Tailscale isn't connected",
                Some(&format!(
                    "{}. Open Tailscale and sign in with the account your other machines use. Machines seen before are listed, but can't be reached until then.",
                    sentence(e)
                )),
                |_| {},
            );
        }
    }

    fn confirm_banner(&mut self, ui: &mut egui::Ui, disc: &Discovery) {
        let Some(q) = self.confirm.clone() else {
            return;
        };
        let name = &q.name;
        let (title, body, button) = match q.action {
            Confirmable::Power(action) => (
                format!("{} {name}?", action.label()),
                match action {
                    PowerAction::Sleep => "Asleep, it can only be woken from its own network, or through a router that forwards wake packets to it.".to_string(),
                    _ => "Programs there close without asking, and anything unsaved is lost.".to_string(),
                },
                action.label(),
            ),
            Confirmable::Forget => (
                format!("Forget {name}?"),
                forget_question(disc, &q.node_id, name),
                "Forget",
            ),
        };
        ui::banner(ui, Tone::Danger, &title, Some(&body), |ui| {
            if ui::destructive_button(ui, button).clicked() {
                match q.action {
                    Confirmable::Power(action) => match power_target(disc, &q.node_id) {
                        Some(ip) => self.power(ip, name, action),
                        None => {
                            self.notice = Some((
                                Tone::Danger,
                                format!("Nothing was sent: {name} is offline, or no longer takes power actions from here."),
                                Instant::now(),
                            ))
                        }
                    },
                    Confirmable::Forget => self.forget(&q.node_id, name),
                }
                self.confirm = None;
            }
            if ui::ghost_button(ui, "Cancel").clicked() {
                self.confirm = None;
            }
        });
    }

    /// Remove what Latch saved about a machine, its saved addresses included. A
    /// remembered row goes at once; a machine still on the tailnet stays
    /// listed again.
    fn forget(&mut self, node_id: &str, name: &str) {
        let outcome = ClientConfig::update(|c| {
            c.forget(node_id);
        });
        self.cfg.forget(node_id);
        self.discovery
            .lock()
            .pcs
            .retain(|p| !(p.remembered && p.node_id == node_id));
        self.notice = Some(match outcome {
            Ok(()) => (Tone::Neutral, format!("Forgot {name}."), Instant::now()),
            Err(e) => (
                Tone::Danger,
                format!("Couldn't forget {name}: {e}"),
                Instant::now(),
            ),
        });
    }

    fn machine_list(
        &mut self,
        ui: &mut egui::Ui,
        ctx: &egui::Context,
        disc: &Discovery,
        prog: &Progress,
    ) {
        ui::group(ui, |ui| {
            let mut first = true;
            if let Some(slot) = self.local.clone() {
                if self.share_page.is_some() {
                    self.this_machine_row(ui, &slot);
                    first = false;
                }
            }
            if disc.pcs.is_empty() {
                if !first {
                    ui::row_separator(ui);
                }
                self.empty_list(ui, disc);
                return;
            }
            for pc in &disc.pcs {
                if !first {
                    ui::row_separator(ui);
                }
                first = false;
                let (tone, detail) = describe(pc);
                ui::list_row(ui, Some(tone), &pc.name, &detail, |ui| {
                    if pc.can_stream() && !pc.remembered {
                        let label = if pc.online {
                            "Connect"
                        } else {
                            "Wake and connect"
                        };
                        let clicked = ui
                            .add_enabled_ui(!prog.active(), |ui| {
                                ui::secondary_button(ui, label)
                                    .on_disabled_hover_text("Another connection is in progress.")
                            })
                            .inner
                            .clicked();
                        if clicked {
                            self.connect(ctx, pc);
                        }
                    }
                    self.row_menu(ui, pc);
                });
            }
        });
    }

    fn empty_list(&mut self, ui: &mut egui::Ui, disc: &Discovery) {
        ui.add_space(space::LG);
        if disc.error.is_some() || !self.tailscale_installed() {
            ui::muted(ui, "Your machines appear here once Tailscale is connected.");
        } else if disc.refreshed.is_none() {
            ui::empty_state(ui, "Looking for machines on your tailnet…", true);
        } else {
            ui::strong(ui, "No other machines yet");
            ui::caption(
                ui,
                "Install Latch on another computer and sign in to Tailscale there with the same account. It appears here within a few seconds.",
            );
            ui.add_space(space::XS);
            if ui::link(ui, "Download Latch").clicked() {
                ui.ctx().open_url(egui::OpenUrl::new_tab(DOWNLOAD_URL));
            }
        }
        ui.add_space(space::LG);
    }

    /// The first row of the list: this machine, and where its sharing is.
    fn this_machine_row(&mut self, ui: &mut egui::Ui, slot: &share::Slot) {
        let g = slot.lock();
        let running = g.setup_running;
        let stopped = g.service_stopped;
        let viewer_only = g.viewer_only;
        let status = g.status.clone();
        drop(g);
        let noun = this_noun(status.as_ref().map(|s| s.os.as_str()).unwrap_or_default());
        let name = status
            .as_ref()
            .map(|s| s.name.clone())
            .filter(|n| !n.is_empty())
            .unwrap_or_else(|| noun.to_string());
        let shared = status
            .as_ref()
            .is_some_and(|s| s.streamer.running && s.streamer.api_ok);
        let (tone, detail) = match &status {
            // Off on purpose, not a fault: Share this Mac is off.
            None if viewer_only => (
                Tone::Neutral,
                "Not shared · it only connects to your other machines".to_string(),
            ),
            None if stopped => (
                Tone::Neutral,
                "Not shared · the background service is stopped".to_string(),
            ),
            None => (
                Tone::Neutral,
                "Starting Latch's background service…".to_string(),
            ),
            Some(_) if running => (Tone::Accent, format!("{noun} · Setting up sharing…")),
            Some(s) if shared && s.setup.is_empty() => (
                Tone::Success,
                format!(
                    "{noun} · Shared{}",
                    s.tailscale_ip
                        .as_deref()
                        .map(|ip| format!(" at {ip}"))
                        .unwrap_or_default()
                ),
            ),
            Some(_) if shared => (
                Tone::Warning,
                format!("{noun} · Shared; setup needs attention"),
            ),
            Some(s) if s.streamer.installed => (
                Tone::Warning,
                format!("{noun} · Not shared: sharing isn't running"),
            ),
            Some(_) => (
                Tone::Neutral,
                format!("{noun} · Not shared, so others can't connect here"),
            ),
        };
        ui::list_row(ui, Some(tone), &name, &detail, |ui| {
            // Only ever the Sharing page: nothing starts from the list.
            let label = if shared {
                "Manage"
            } else if viewer_only && status.is_none() {
                "Share this Mac…"
            } else {
                "Set up sharing"
            };
            if ui::secondary_button(ui, label).clicked() {
                self.page = Page::Sharing;
            }
        });
    }

    /// The row's "…" menu: wake, power, copy the address, forget.
    fn row_menu(&mut self, ui: &mut egui::Ui, pc: &Pc) {
        let live = !pc.remembered;
        let wake = live && !pc.online && pc.can_wake();
        let test_wake = live && pc.online && pc.host.is_some() && pc.can_wake();
        let power = live && pc.online && pc.power_allowed();
        // Only a machine that is away: one that answers is re-learned
        // within seconds, and forgetting it would only forget its saved details.
        let forget = !pc.online && pc.known.is_some();
        if !(wake || test_wake || power || forget || pc.ip.is_some()) {
            return;
        }
        let ask = |action| Confirm {
            node_id: pc.node_id.clone(),
            name: pc.name.clone(),
            action,
        };
        ui::icon_menu(ui, Icon::More, &format!("More for {}", pc.name), |ui| {
            if wake && ui::menu_item(ui, "Wake").clicked() {
                self.notice = Some(match session::wake_only(pc) {
                    Ok(_) => (
                        Tone::Neutral,
                        format!(
                            "Wake packets sent to {}. It can take a minute to come online.",
                            pc.name
                        ),
                        Instant::now(),
                    ),
                    Err(e) => (Tone::Danger, format!("{}: {e}", pc.name), Instant::now()),
                });
            }
            if test_wake && ui::menu_item(ui, "Test waking it from this network").clicked() {
                self.test_wake(pc);
            }
            if power {
                if wake || test_wake {
                    ui::menu_separator(ui);
                }
                // Only where something can wake it again: never a Linux
                // VPS or a Mac, which would stay asleep.
                if pc.can_sleep() && ui::menu_item(ui, "Sleep").clicked() {
                    if let Some(ip) = pc.ip {
                        self.power(ip, &pc.name, PowerAction::Sleep);
                    }
                }
                if ui::menu_item(ui, "Restart…").clicked() {
                    self.confirm = Some(ask(Confirmable::Power(PowerAction::Restart)));
                }
                if ui::menu_item(ui, "Shut down…").clicked() {
                    self.confirm = Some(ask(Confirmable::Power(PowerAction::Shutdown)));
                }
            }
            if let Some(ip) = pc.ip {
                if wake || test_wake || power {
                    ui::menu_separator(ui);
                }
                if ui::menu_item(ui, "Copy Tailscale address").clicked() {
                    ui.ctx().copy_text(ip.to_string());
                    self.notice = Some((
                        Tone::Neutral,
                        format!("Copied {ip}, {}'s Tailscale address.", pc.name),
                        Instant::now(),
                    ));
                }
            }
            if forget {
                if wake || test_wake || power || pc.ip.is_some() {
                    ui::menu_separator(ui);
                }
                if ui::menu_item(ui, "Forget this machine…").clicked() {
                    self.confirm = Some(ask(Confirmable::Forget));
                }
            }
        });
    }

    /// What the next stream asks for, and a way to change it.
    fn next_stream(&mut self, ui: &mut egui::Ui, ctx: &egui::Context) {
        let s = path::effective(&self.cfg.stream);
        let (w, h) = s.resolution.pixels(Self::native_pixels(ctx));
        ui.horizontal_wrapped(|ui| {
            ui.spacing_mut().item_spacing = egui::vec2(space::SM, space::SM);
            ui::section_label(ui, "Next stream");
            ui.add_space(space::XS);
            ui::tag(ui, None, &format!("{w} × {h}"));
            ui::tag(ui, None, &format!("{} fps", s.fps));
            ui::tag(ui, None, &format!("{} Mbps", s.bitrate_kbps / 1000));
            ui.add_space(space::XS);
            if ui::link(ui, "Change").clicked() {
                self.page = Page::Settings;
            }
        });
    }

    /// Everything worth knowing about the paths and keys that is not
    /// urgent, folded away. Urgent key expiry is at the top of the page.
    fn details(&self, ui: &mut egui::Ui, disc: &Discovery) {
        let release = self.updates.lock().release.clone();
        let mut items: Vec<(Tone, String)> = path_warnings(disc, release.as_ref())
            .into_iter()
            .map(|t| (Tone::Warning, t))
            .collect();
        items.extend(
            key_expiry_warnings(disc)
                .into_iter()
                .filter(|(urgent, _)| !urgent)
                .map(|(_, t)| (Tone::Neutral, t)),
        );
        if items.is_empty() {
            return;
        }
        let title = format!("Connection details ({})", items.len());
        ui::collapsible(ui, "lobby-details", &title, false, |ui| {
            ui.spacing_mut().item_spacing.y = space::SM;
            for (tone, text) in &items {
                ui::notice(ui, *tone, text);
            }
        });
    }

    /// Start installing the newest Latch Host on the PC through the
    /// stream. See `handover.rs`.
    fn start_handover(
        &mut self,
        ctx: &egui::Context,
        ip: std::net::Ipv4Addr,
        name: &str,
        input: latch_stream::Input,
        disc: &Discovery,
    ) {
        let Some(mac_ip) = disc.self_ip else {
            self.view.toast(
                Tone::Danger,
                "Tailscale on this machine has no address to serve the update from.",
            );
            return;
        };
        let running = disc
            .pcs
            .iter()
            .find(|p| p.ip == Some(ip))
            .and_then(|p| p.host.as_ref())
            .and_then(|h| Version::parse(&h.version).ok())
            .unwrap_or_else(|| Version::new(0, 0, 0));
        let release = self.updates.lock().release.clone();
        if let Some(rel) = &release {
            if let Err(e) = handover::usable(rel, &running) {
                self.view.toast(Tone::Danger, format!("{name}: {e}."));
                return;
            }
        }
        // The token is looked up on the handover's own thread: git's
        // credential helper may wait on a Keychain prompt, and this thread
        // draws the stream.
        self.handover = Some(Handover::start(
            release,
            self.cfg.github_token.clone(),
            mac_ip,
            ip,
            input,
            ctx.clone(),
        ));
        self.view.toast(
            Tone::Neutral,
            format!("Installing Latch Host on {name} through the stream…"),
        );
    }

    /// Notice when the install through the stream has landed (the host
    /// reports the new version) or has given up, and let it go.
    fn tend_handover(&mut self, pc_now: Option<&Pc>, name: &str) {
        let Some(h) = &self.handover else { return };
        let p = h.progress();
        let landed = p.version.as_ref().is_some_and(|v| {
            pc_now
                .and_then(|pc| pc.host.as_ref())
                .and_then(|st| Version::parse(&st.version).ok())
                .is_some_and(|running| running >= *v)
        });
        if landed {
            self.view.toast(
                Tone::Success,
                format!(
                    "{name} now runs Latch Host {}. Updates arrive by themselves from here on.",
                    p.version.map(|v| v.to_string()).unwrap_or_default()
                ),
            );
            self.handover = None;
        } else if !h.active() && p.since.elapsed() > Duration::from_secs(20) {
            self.handover = None;
        }
    }

    fn session_card(&mut self, ui: &mut egui::Ui, prog: &Progress) {
        if prog.updating {
            ui::banner(
                ui,
                Tone::Accent,
                "Installing a Latch update",
                Some("Latch restarts itself in a moment. New connections wait until it has."),
                |_| {},
            );
            return;
        }
        let pc = &prog.pc;
        let (title, explain) = match &prog.step {
            Step::Waking => (
                format!("Waking {pc}"),
                "Latch sends wake packets every few seconds. A sleeping machine can take up to two minutes to come back.",
            ),
            Step::Waiting => (
                format!("Waiting for {pc}"),
                "It is awake; Latch is starting there.",
            ),
            _ => (format!("Connecting to {pc}"), ""),
        };
        ui::toned_card(ui, Tone::Accent, |ui| {
            ui::heading(ui, &title, None);
            if !explain.is_empty() {
                ui::caption(ui, explain);
            }
            ui::empty_state(
                ui,
                if prog.detail.is_empty() {
                    "Starting…"
                } else {
                    &prog.detail
                },
                true,
            );
            ui.add_space(space::XS);
            if ui::secondary_button(ui, "Cancel").clicked() {
                self.disconnect();
            }
        });
    }

    fn ended_card(&mut self, ui: &mut egui::Ui, prog: &Progress, error: Option<&str>) {
        let pc = prog.pc.clone();
        if let Some(e) = error {
            let title = if self.streamed {
                format!("{pc} disconnected")
            } else {
                format!("Couldn't connect to {pc}")
            };
            let lighter = self.stream_failed
                && self
                    .cfg
                    .stream
                    .preset()
                    .is_none_or(|p| p != crate::config::Preset::Smooth);
            ui::banner(ui, Tone::Danger, &title, Some(e), |ui| {
                if let Some(last) = self.last_pc.clone() {
                    if ui::primary_button(ui, "Try again").clicked() {
                        self.reconnect = Some(last.clone());
                        self.progress.lock().step = Step::Idle;
                    }
                    // The lighter profile is the likeliest to hold if the
                    // network, not the machine, ended the last one.
                    if lighter
                        && ui::secondary_button(ui, "Try at 1080p, 20 Mbps")
                            .on_hover_text("Switches Settings to the Smooth profile.")
                            .clicked()
                    {
                        self.cfg.stream.apply_preset(crate::config::Preset::Smooth);
                        self.dirty = true;
                        self.reconnect = Some(last);
                        self.progress.lock().step = Step::Idle;
                    }
                }
                if ui::ghost_button(ui, "Dismiss").clicked() {
                    self.offer_sleep = None;
                    self.progress.lock().step = Step::Idle;
                }
            });
        } else if let Some(target) = self.offer_sleep.clone() {
            ui::banner(
                ui,
                Tone::Neutral,
                &format!("Session with {pc} ended"),
                Some("Put it to sleep, or leave it on to connect again from anywhere. Asleep, it can only be woken from its own network."),
                |ui| {
                    if ui::primary_button(ui, &format!("Sleep {}", target.name)).clicked() {
                        if let Some(ip) = target.ip {
                            self.power(ip, &target.name, PowerAction::Sleep);
                        }
                        self.offer_sleep = None;
                        self.progress.lock().step = Step::Idle;
                    }
                    if ui::ghost_button(ui, "Leave it on").clicked() {
                        self.offer_sleep = None;
                        self.progress.lock().step = Step::Idle;
                    }
                },
            );
        }
    }

    // -----------------------------------------------------------------------
    // Settings
    // -----------------------------------------------------------------------

    fn settings_page(
        &mut self,
        ui: &mut egui::Ui,
        ctx: &egui::Context,
        disc: &Discovery,
        _prog: &Progress,
    ) {
        ui::page_header(
            ui,
            "Settings",
            Some("Stream changes apply the next time you connect."),
            |_| {},
        );
        let native = Self::native_pixels(ctx);
        ui::section(ui, "Stream", |ui| {
            if crate::settings::stream_controls(ui, &mut self.cfg.stream, native) {
                self.dirty = true;
            }
        });
        let (_, host_key) = stream::host_key();
        ui::section(ui, "Window and input", |ui| {
            if ui::toggle_row(
                ui,
                &mut self.cfg.stream.fullscreen,
                "Open streams full screen",
                Some("Otherwise the stream fills this window."),
            ) {
                self.dirty = true;
            }
            ui::row_separator(ui);
            if ui::toggle_row(
                ui,
                &mut self.cfg.capture_mouse,
                "Capture the mouse",
                Some(&format!(
                    "A click on the picture hides this cursor and sends raw movement, which games need. {host_key} releases it."
                )),
            ) {
                self.dirty = true;
            }
            if cfg!(target_os = "macos") {
                ui::row_separator(ui);
                if ui::toggle_row(
                    ui,
                    &mut self.cfg.cmd_is_ctrl,
                    "Command acts as Ctrl",
                    Some("So Command-C, V and Z copy, paste and undo on Windows and Linux. Off, Command is the Windows key. A Mac always gets Command."),
                ) {
                    self.dirty = true;
                }
            }
        });
        ui::section(ui, "Power", |ui| {
            if ui::toggle_row(
                ui,
                &mut self.cfg.sleep_prompt,
                "Offer to sleep a machine after a session",
                Some("Asleep, Tailscale is off there too, so it can only be woken from its own network."),
            ) {
                self.dirty = true;
            }
        });
        self.updates_section(ui);
        self.relay_section(ui, disc);
        self.about_section(ui);
    }

    fn updates_section(&mut self, ui: &mut egui::Ui) {
        ui::section(ui, "Updates", |ui| {
            if !cfg!(target_os = "macos") {
                // Only a Mac runs the updater (see update::spawn); here it
                // has left a line saying where new versions come from.
                let message = self.updates.lock().message.clone();
                ui::setting_row(ui, "New versions", Some(&message), |_| {});
                return;
            }
            if ui::toggle_row(
                ui,
                &mut self.cfg.auto_update,
                "Keep Latch up to date",
                Some("Checks GitHub every few hours, installs new versions of this app, and sends updates to your Windows PCs over Tailscale."),
            ) {
                self.dirty = true;
            }
            ui::row_separator(ui);
            let (message, checked) = {
                let st = self.updates.lock();
                (st.message.clone(), st.checked)
            };
            let hint = if !self.cfg.auto_update {
                "Off. This app and your other machines stay on their current versions.".to_string()
            } else if message.is_empty() {
                format!("Waiting for the first check · {}", update::ago(checked))
            } else {
                format!("{message} · {}", update::ago(checked))
            };
            ui::setting_row(ui, "Last check", Some(&hint), |ui| {
                let clicked = ui
                    .add_enabled_ui(self.cfg.auto_update, |ui| {
                        ui::secondary_button(ui, "Check now")
                    })
                    .inner
                    .clicked();
                if clicked {
                    self.updates.lock().check_now = true;
                }
            });
        });
    }

    fn relay_section(&mut self, ui: &mut egui::Ui, disc: &Discovery) {
        let state = relay_state(disc);
        ui::section(ui, "Relay", |ui| {
            ui.add_space(space::MD);
            ui.spacing_mut().item_spacing.y = space::SM;
            let (tone, title) = state.title();
            ui::status_text(ui, tone, &title);
            ui::caption(ui, state.sentence());
            if let RelayState::Unavailable { .. } = &state {
                ui::well(ui, |ui| {
                    ui.label(
                        egui::RichText::new(RELAY_GRANT)
                            .font(ui::theme::mono(ui::theme::text::MONO))
                            .color(P.text_secondary),
                    );
                });
            }
            ui.horizontal_wrapped(|ui| {
                ui.spacing_mut().item_spacing.x = space::LG;
                if let RelayState::Unavailable { .. } = &state {
                    if ui::secondary_button(ui, "Copy grant").clicked() {
                        ui.ctx().copy_text(RELAY_GRANT.to_string());
                        self.notice = Some((
                            Tone::Neutral,
                            "Copied the relay grant.".into(),
                            Instant::now(),
                        ));
                    }
                }
                if ui::link(ui, "How peer relays work").clicked() {
                    ui.ctx().open_url(egui::OpenUrl::new_tab(RELAY_DOCS));
                }
            });
            ui.add_space(space::XS);
        });
    }

    fn about_section(&mut self, ui: &mut egui::Ui) {
        ui::section(ui, "About", |ui| {
            ui::setting_row(
                ui,
                &format!("Latch {}", env!("CARGO_PKG_VERSION")),
                Some("Free software under the GNU GPL, version 3 or later. It uses the Geist typeface."),
                |ui| {
                    if ui::link(ui, "Source code").clicked() {
                        ui.ctx().open_url(egui::OpenUrl::new_tab(SOURCE_URL));
                    }
                },
            );
            ui::row_separator(ui);
            ui.add_space(space::XS);
            ui::disclosure(ui, "about-notices", "Open-source notices", false, |ui| {
                ui::well(ui, |ui| {
                    egui::ScrollArea::vertical()
                        .id_salt("open_source_notices")
                        .max_height(280.0)
                        .show(ui, |ui| {
                            ui.add(
                                egui::Label::new(
                                    egui::RichText::new(ui::NOTICES)
                                        .font(ui::theme::mono(11.5))
                                        .color(P.text_secondary),
                                )
                                .wrap(),
                            );
                        });
                });
            });
            ui.add_space(space::SM);
        });
    }
}

/// "Tailscale is stopped" → "Tailscale is stopped", with a capital and
/// without a trailing full stop, for building a sentence around it.
fn sentence(s: &str) -> String {
    let s = s.trim().trim_end_matches('.');
    let mut c = s.chars();
    match c.next() {
        Some(f) => f.to_uppercase().collect::<String>() + c.as_str(),
        None => String::new(),
    }
}

/// "This Mac", "This PC": what to call the machine Latch runs on.
fn this_noun(os: &str) -> &'static str {
    let os = if os.is_empty() {
        if cfg!(windows) {
            "windows"
        } else if cfg!(target_os = "macos") {
            "macOS"
        } else {
            "linux"
        }
    } else {
        os
    };
    match os_label(os) {
        "Windows" => "This PC",
        "macOS" => "This Mac",
        _ => "This machine",
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum RelayState {
    None,
    Offline {
        name: String,
    },
    Checking {
        name: String,
    },
    Unavailable {
        name: String,
    },
    Ready {
        name: String,
        ip: Option<std::net::Ipv4Addr>,
    },
}

impl RelayState {
    fn sentence(&self) -> &'static str {
        match self {
            Self::None => RELAY_NONE,
            Self::Offline { .. } => RELAY_OFFLINE,
            Self::Checking { .. } => RELAY_CHECKING,
            Self::Unavailable { .. } => RELAY_UNAVAILABLE,
            Self::Ready { .. } => RELAY_READY,
        }
    }

    /// The status line over the sentence.
    fn title(&self) -> (Tone, String) {
        match self {
            Self::None => (Tone::Neutral, "No relay of your own".into()),
            Self::Offline { name } => (Tone::Warning, format!("{name} is offline")),
            Self::Checking { name } => (Tone::Neutral, format!("Couldn't check {name}")),
            Self::Unavailable { name } => (
                Tone::Warning,
                format!("{name} isn't available to this device"),
            ),
            Self::Ready { name, ip } => (
                Tone::Success,
                match ip {
                    Some(ip) => format!("Using {name} ({ip})"),
                    None => format!("Using {name}"),
                },
            ),
        }
    }
}

fn ip_listed(servers: &[String], ip: Option<std::net::Ipv4Addr>) -> bool {
    ip.is_some_and(|ip| servers.iter().any(|s| s == &ip.to_string()))
}

/// Ready only when an online `tag:relay` node's Tailscale IP is in the debug
/// list. `Known([])` is not ACL denial. A nonempty list that matches no
/// tagged IP is not Ready.
fn relay_state(disc: &Discovery) -> RelayState {
    let Some(first) = disc.relays.first() else {
        return RelayState::None;
    };
    if let PeerRelayServers::Known(servers) = &disc.peer_relay_servers {
        if let Some(r) = disc
            .relays
            .iter()
            .find(|r| r.online && ip_listed(servers, r.ip))
        {
            return RelayState::Ready {
                name: r.name.clone(),
                ip: r.ip,
            };
        }
        if let Some(r) = disc
            .relays
            .iter()
            .find(|r| !r.online && ip_listed(servers, r.ip))
        {
            return RelayState::Offline {
                name: r.name.clone(),
            };
        }
    }
    let relay = disc.relays.iter().find(|r| r.online).unwrap_or(first);
    let name = relay.name.clone();
    if !disc.relays.iter().any(|r| r.online) {
        return RelayState::Offline { name };
    }
    match &disc.peer_relay_servers {
        PeerRelayServers::Unknown => RelayState::Checking { name },
        PeerRelayServers::Known(_) => RelayState::Unavailable { name },
    }
}

/// One line per machine whose Tailscale key expires, this one included.
/// A line for each Tailscale key that expires, and whether it needs doing
/// now: expired, or within 30 days.
fn key_expiry_warnings(disc: &Discovery) -> Vec<(bool, String)> {
    let urgent = |d: i64| d <= 30;
    let mut out = Vec::new();
    for pc in &disc.pcs {
        if let Some(d) = pc.key_expiry_days {
            out.push((
                urgent(d),
                if d <= 0 {
                    format!(
                        "{}'s Tailscale key has expired. It is off the tailnet until someone signs in to Tailscale on it.",
                        pc.name
                    )
                } else {
                    format!(
                        "{}'s Tailscale key expires in {d} days. Turn off key expiry for it in the Tailscale admin console (login.tailscale.com/admin/machines), or it will need a sign-in on that machine.",
                        pc.name
                    )
                },
            ));
        }
    }
    if let Some(d) = disc.self_key_days {
        out.push((
            urgent(d),
            if d <= 0 {
                "This machine's Tailscale key has expired. Sign in to Tailscale again.".to_string()
            } else {
                format!(
                    "This machine's Tailscale key expires in {d} days. Turn off key expiry for it in the admin console too."
                )
            },
        ));
    }
    out
}

fn stream_failure(message: &str) -> String {
    let message = message.trim().trim_end_matches(['.', '!', '?']);
    let mut chars = message.chars();
    let message = chars
        .next()
        .map(|c| c.to_lowercase().collect::<String>() + chars.as_str())
        .unwrap_or_else(|| "the host gave no reason".into());
    format!("The stream didn't start: {message}.")
}

/// "Windows", "macOS", "Linux".
fn os_label(os: &str) -> &'static str {
    let n = tailscale::Node {
        os: os.to_string(),
        ..Default::default()
    };
    n.os_label()
}

/// A machine's status dot and the line under its name. Kept short: the
/// line is cut, not wrapped, and shows whole on hover.
fn describe(pc: &Pc) -> (Tone, String) {
    let os = os_label(&pc.os);
    let at = pc.ip.map(|ip| format!(" · {ip}")).unwrap_or_default();
    // A phone on the tailnet is listed, but it will never share a desktop:
    // "Latch isn't installed" would send someone looking for an app.
    let lower = pc.os.to_ascii_lowercase();
    if lower == "ios" || lower == "android" || lower == "ipados" {
        let name = if lower == "android" { "Android" } else { "iOS" };
        return (
            Tone::Neutral,
            format!("{name}{at} · Phones and tablets can't share a desktop"),
        );
    }
    if pc.remembered {
        let seen = pc.known.as_ref().and_then(|k| k.last_seen_unix);
        return (
            Tone::Neutral,
            match seen {
                Some(t) => format!("{os} · Last seen {}", latch_core::dates::ymd(t)),
                None => format!("{os} · Not seen yet"),
            },
        );
    }
    if !pc.online {
        return if pc.can_wake() {
            (
                Tone::Neutral,
                format!("{os} · Asleep or off · Latch can wake it"),
            )
        } else {
            (
                Tone::Neutral,
                format!("{os} · Offline · Turn it on to connect"),
            )
        };
    }
    let path = if pc.path.direct.is_some() {
        format!(" · {}", pc.path.label())
    } else {
        String::new()
    };
    match &pc.host {
        Some(h) if !h.setup.is_empty() => (
            Tone::Warning,
            format!("{os}{at} · Sharing needs attention there"),
        ),
        Some(h) if h.streamer.protocol >= 1 => (Tone::Success, format!("{os}{at}{path}")),
        Some(_) => (
            Tone::Warning,
            format!("{os}{at} · Update Latch there to connect"),
        ),
        None => (Tone::Neutral, format!("{os}{at} · Latch isn't answering")),
    }
}

/// The host's audio failure on the PC with `node_id`, as its host last
/// reported it (3.1+ hosts); `None` when sound works or nothing is known.
fn host_audio_problem(discovery: &Mutex<Discovery>, node_id: &str) -> Option<String> {
    discovery
        .lock()
        .pcs
        .iter()
        .find(|p| p.node_id == node_id)
        .and_then(|p| p.host.as_ref())
        .map(|h| h.streamer.audio_problem.clone())
        .filter(|s| !s.is_empty())
}

/// One line per PC that is relayed, encodes in software, has no sound to
/// send, runs a host too old to update itself, or runs one this Mac cannot
/// update that is behind `release`.
fn path_warnings(disc: &Discovery, release: Option<&latch_core::update::Release>) -> Vec<String> {
    let mut out = Vec::new();
    for pc in disc.pcs.iter().filter(|p| !p.remembered && p.online) {
        let nat = pc.host.as_ref().and_then(|h| h.nat.as_ref());
        if let Some(t) = path::explain(&pc.name, &pc.path, nat, disc.self_nat.as_ref()) {
            out.push(t);
        }
        if let Some(h) = &pc.host {
            if h.streamer.encoder == "software" {
                out.push(format!(
                    "{} encodes video in software: it has no GPU encoder Latch can use, so every CPU core does the work. A still desktop holds the bitrate you set; fast motion may drop frames.",
                    pc.name
                ));
            }
            if !h.streamer.audio_problem.is_empty() {
                out.push(format!(
                    "{} has no sound to send: it reports “{}”. A machine with no speakers or monitor has no audio device to capture; add a virtual one (Steam Streaming Speakers or VB-CABLE) and make it the default output.",
                    pc.name, h.streamer.audio_problem
                ));
            }
            if let Ok(v) = Version::parse(&h.version) {
                if !latch_core::update::host_can_receive_update(&v) {
                    out.push(update::old_host_message(&pc.name, &v));
                }
                if let Some(t) =
                    release.and_then(|rel| update::manual_update_message(&pc.name, &h.os, &v, rel))
                {
                    out.push(t);
                }
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::KnownPc;
    use crate::session::Relay;

    #[test]
    fn stream_failures_are_sentences() {
        assert_eq!(
            stream_failure("No display is available"),
            "The stream didn't start: no display is available."
        );
        assert_eq!(
            stream_failure("the host refused.  "),
            "The stream didn't start: the host refused."
        );
        assert_eq!(
            stream_failure(""),
            "The stream didn't start: the host gave no reason."
        );
    }

    #[test]
    fn remembered_pcs_say_when_they_were_seen_and_keys_get_warnings() {
        let pc = Pc {
            name: "Gaming-PC".into(),
            remembered: true,
            known: Some(KnownPc {
                last_seen_unix: Some(1_788_739_200),
                ..Default::default()
            }),
            ..Default::default()
        };
        assert_eq!(
            describe(&pc),
            (Tone::Neutral, "Unknown · Last seen 2026-09-07".to_string())
        );
        let disc = Discovery {
            pcs: vec![
                Pc {
                    name: "Gaming-PC".into(),
                    key_expiry_days: Some(176),
                    ..Default::default()
                },
                Pc {
                    name: "Den".into(),
                    key_expiry_days: None,
                    ..Default::default()
                },
                Pc {
                    name: "Office".into(),
                    key_expiry_days: Some(-2),
                    ..Default::default()
                },
            ],
            self_key_days: Some(12),
            ..Default::default()
        };
        let w = key_expiry_warnings(&disc);
        assert_eq!(w.len(), 3, "{w:?}");
        assert!(!w[0].0 && w[0].1.contains("Gaming-PC") && w[0].1.contains("176 days"));
        assert!(w[1].0 && w[1].1.contains("Office") && w[1].1.contains("expired"));
        assert!(w[2].0 && w[2].1.starts_with("This machine") && w[2].1.contains("12 days"));
        assert!(key_expiry_warnings(&Discovery::default()).is_empty());
    }

    #[test]
    fn descriptions_cover_every_state() {
        let mut pc = Pc {
            name: "Gaming-PC".into(),
            ip: Some("203.0.113.10".parse().unwrap()),
            ..Default::default()
        };
        let line = |pc: &Pc| describe(pc).1;
        assert!(line(&pc).contains("Offline"));
        pc.known = Some(KnownPc {
            mac: Some("02:00:00:00:00:01".into()),
            ..Default::default()
        });
        assert!(line(&pc).contains("Asleep"));
        assert!(line(&pc).contains("can wake it"));
        pc.online = true;
        assert!(line(&pc).contains("Latch isn't answering"));
        assert_eq!(describe(&pc).0, Tone::Neutral);
        assert!(line(&pc).len() < 80, "{}", line(&pc));
        pc.host = Some(latch_core::api::Status {
            streamer: latch_core::api::Streamer {
                protocol: 1,
                ..Default::default()
            },
            ..Default::default()
        });
        assert_eq!(
            describe(&pc),
            (Tone::Success, "Unknown · 203.0.113.10".to_string())
        );
        pc.host.as_mut().unwrap().setup = vec!["Sharing is not set up.".into()];
        assert_eq!(describe(&pc).0, Tone::Warning, "set up there, not ready");
        pc.host.as_mut().unwrap().setup.clear();
        let phone = Pc {
            os: "iOS".into(),
            online: true,
            ..pc.clone()
        };
        assert!(
            line(&phone).contains("can't share a desktop"),
            "{}",
            line(&phone)
        );
        pc.path = crate::path::Path {
            direct: Some(false),
            relay: "tok".into(),
            rtt_ms: Some(210),
            ..Default::default()
        };
        assert_eq!(
            line(&pc),
            "Unknown · 203.0.113.10 · Relayed via Tokyo · 210 ms"
        );
    }

    #[test]
    fn the_lobby_warns_about_relays_software_encoders_and_old_hosts() {
        let mut gaming_pc = Pc {
            name: "Gaming-PC".into(),
            online: true,
            ip: Some("100.64.0.10".parse().unwrap()),
            path: crate::path::Path {
                direct: Some(false),
                relay: "tok".into(),
                rtt_ms: Some(200),
                ..Default::default()
            },
            host: Some(latch_core::api::Status {
                version: "3.0.0".into(),
                streamer: latch_core::api::Streamer {
                    encoder: "software".into(),
                    audio_problem:
                        "Unable to initialize audio capture. The stream will not have audio.".into(),
                    ..Default::default()
                },
                ..Default::default()
            }),
            ..Default::default()
        };
        let disc = Discovery {
            pcs: vec![gaming_pc.clone()],
            ..Default::default()
        };
        let w = path_warnings(&disc, None);
        assert_eq!(w.len(), 4, "{w:?}");
        assert!(w[0].contains("Tokyo relay"), "{}", w[0]);
        assert!(w[1].contains("software"), "{}", w[1]);
        assert!(
            w[2].contains("no sound") && w[2].contains("audio capture"),
            "{}",
            w[2]
        );
        assert!(
            w[3].contains("Install the latest Latch release"),
            "{}",
            w[3]
        );
        assert_eq!(
            host_audio_problem(&Mutex::new(disc.clone()), &gaming_pc.node_id).as_deref(),
            Some("Unable to initialize audio capture. The stream will not have audio.")
        );
        // Direct, GPU encoder, sound, current host: nothing to say.
        gaming_pc.path.direct = Some(true);
        let h = gaming_pc.host.as_mut().unwrap();
        h.version = "3.1.0".into();
        h.streamer.encoder = "nvenc".into();
        h.streamer.audio_problem.clear();
        let disc = Discovery {
            pcs: vec![gaming_pc.clone()],
            ..Default::default()
        };
        assert!(path_warnings(&disc, None).is_empty());
        // Nothing is said about a PC that is off.
        gaming_pc.online = false;
        gaming_pc.path.direct = Some(false);
        let disc = Discovery {
            pcs: vec![gaming_pc],
            ..Default::default()
        };
        assert!(path_warnings(&disc, None).is_empty());
    }

    fn power_pc(id: &str, os: &str, ip: &str, online: bool) -> Pc {
        Pc {
            node_id: id.into(),
            name: "latch-node".into(),
            os: os.into(),
            ip: Some(ip.parse().unwrap()),
            online,
            host: online.then(|| latch_core::api::Status {
                power_allowed: true,
                ..Default::default()
            }),
            ..Default::default()
        }
    }

    #[test]
    fn a_power_action_goes_to_the_machine_asked_for_never_its_namesake() {
        let disc = Discovery {
            pcs: vec![
                power_pc("old", "windows", "100.64.0.3", false),
                power_pc("a", "windows", "100.64.0.1", true),
                power_pc("b", "linux", "100.64.0.2", true),
            ],
            ..Default::default()
        };
        let ip = |s: &str| Some(s.parse().unwrap());
        assert_eq!(power_target(&disc, "a"), ip("100.64.0.1"));
        assert_eq!(power_target(&disc, "b"), ip("100.64.0.2"));
        assert_eq!(
            power_target(&disc, "old"),
            None,
            "asleep, so nothing to send to"
        );
        assert_eq!(power_target(&disc, "gone"), None);
        let mut refused = disc.clone();
        refused.pcs[1].host.as_mut().unwrap().power_allowed = false;
        assert_eq!(power_target(&refused, "a"), None, "its owner said no");
    }

    #[test]
    fn sleep_is_offered_only_where_a_wake_packet_brings_it_back() {
        assert!(power_pc("a", "windows", "100.64.0.1", true).can_sleep());
        for os in ["linux", "macOS", ""] {
            assert!(!power_pc("a", os, "100.64.0.1", true).can_sleep(), "{os:?}");
        }
        let mut pc = power_pc("a", "windows", "100.64.0.1", true);
        pc.host.as_mut().unwrap().power_allowed = false;
        assert!(!pc.can_sleep());
    }

    #[test]
    fn a_power_action_ends_only_the_connection_to_its_own_machine() {
        let (gaming, den) = (
            "100.64.0.10".parse().unwrap(),
            "100.64.0.31".parse().unwrap(),
        );
        let mut p = Progress {
            step: Step::Waking,
            ip: Some(gaming),
            ..Default::default()
        };
        assert!(targets(None, &p, gaming), "the machine being woken");
        assert!(
            !targets(None, &p, den),
            "another machine's sleep leaves it be"
        );
        assert!(targets(Some(den), &p, den), "the machine streaming");
        p.step = Step::Ended { error: None };
        assert!(!targets(None, &p, gaming), "nothing under way any more");
    }

    #[test]
    fn a_reconnect_keeps_full_screen_and_the_end_restores_the_window() {
        let mut after = Some(false);
        assert_eq!(fullscreen_when_finished(true, &mut after), None);
        assert_eq!(after, Some(false), "kept for the stream that follows");
        assert_eq!(fullscreen_when_finished(false, &mut after), Some(false));
        assert_eq!(after, None);
        assert_eq!(
            fullscreen_when_finished(false, &mut after),
            None,
            "a stream that never showed changes nothing"
        );
    }

    #[test]
    fn a_host_this_mac_cannot_update_is_named_once_a_release_is_out() {
        let mut stuzha = power_pc("s", "linux", "100.64.0.5", true);
        stuzha.name = "stuzha".into();
        let h = stuzha.host.as_mut().unwrap();
        h.version = "4.1.1".into();
        h.os = "linux".into();
        let disc = Discovery {
            pcs: vec![stuzha],
            ..Default::default()
        };
        let rel = latch_core::update::Release {
            version: Version::new(4, 1, 2),
            tag: "v4.1.2".into(),
            prerelease: false,
            assets: vec![],
        };
        assert!(path_warnings(&disc, None).is_empty());
        let w = path_warnings(&disc, Some(&rel));
        assert_eq!(w.len(), 1, "{w:?}");
        assert!(w[0].contains("stuzha runs Latch Host 4.1.1"), "{}", w[0]);
    }

    fn a_relay(online: bool) -> Relay {
        Relay {
            name: "relay-sj".into(),
            ip: Some("100.64.0.40".parse().unwrap()),
            online,
        }
    }

    fn disc_with(relays: Vec<Relay>, servers: PeerRelayServers) -> Discovery {
        Discovery {
            relays,
            peer_relay_servers: servers,
            ..Default::default()
        }
    }

    #[test]
    fn relay_card_copy_covers_the_three_states_and_does_not_invent_a_denial() {
        assert_eq!(RelayState::None.sentence(), RELAY_NONE);
        assert_eq!(
            RelayState::Ready {
                name: "relay-sj".into(),
                ip: None,
            }
            .sentence(),
            RELAY_READY
        );
        for s in [RELAY_NONE, RELAY_READY, RELAY_UNAVAILABLE] {
            assert!(!s.to_lowercase().contains("shared"));
            assert!(!s.contains("Latch relay"));
        }

        assert_eq!(
            relay_state(&Discovery::default()),
            RelayState::None,
            "no tagged node is no relay, even before the debug command runs"
        );
        assert_eq!(
            relay_state(&disc_with(
                vec![],
                PeerRelayServers::Known(vec!["100.64.0.40".into()]),
            )),
            RelayState::None,
            "a grant without a tagged node is not a relay on this network"
        );

        let online = vec![a_relay(true)];
        assert_eq!(
            relay_state(&disc_with(online.clone(), PeerRelayServers::Unknown)),
            RelayState::Checking {
                name: "relay-sj".into()
            }
        );
        assert_ne!(
            relay_state(&disc_with(online.clone(), PeerRelayServers::Unknown)).sentence(),
            RELAY_UNGRANTED,
            "Unknown is not a denial"
        );
        assert!(
            relay_state(&disc_with(online.clone(), PeerRelayServers::Unknown))
                .sentence()
                .contains("does not mean access is denied")
        );
        assert!(
            relay_state(&disc_with(online.clone(), PeerRelayServers::Unknown))
                .sentence()
                .contains("1.86")
        );

        let empty = relay_state(&disc_with(online.clone(), PeerRelayServers::Known(vec![])));
        assert_eq!(
            empty,
            RelayState::Unavailable {
                name: "relay-sj".into()
            }
        );
        assert_ne!(
            empty.sentence(),
            RELAY_UNGRANTED,
            "Known([]) is not ACL denial"
        );
        assert!(
            empty.sentence().contains("does not mean access is denied"),
            "{}",
            empty.sentence()
        );

        assert_eq!(
            relay_state(&disc_with(
                online.clone(),
                PeerRelayServers::Known(vec!["100.64.0.40".into()]),
            )),
            RelayState::Ready {
                name: "relay-sj".into(),
                ip: Some("100.64.0.40".parse().unwrap()),
            }
        );

        let mismatch = relay_state(&disc_with(
            online.clone(),
            PeerRelayServers::Known(vec!["100.64.0.99".into()]),
        ));
        assert_eq!(
            mismatch,
            RelayState::Unavailable {
                name: "relay-sj".into()
            },
            "a nonempty list that matches no tagged IP is not Ready"
        );
        assert_ne!(mismatch.sentence(), RELAY_READY);
        assert_ne!(mismatch.sentence(), RELAY_UNGRANTED);

        assert_eq!(
            relay_state(&disc_with(
                vec![a_relay(false)],
                PeerRelayServers::Known(vec![]),
            )),
            RelayState::Offline {
                name: "relay-sj".into()
            },
            "an offline node is not 'online but ungranted'"
        );
        assert_eq!(
            relay_state(&disc_with(
                vec![a_relay(false)],
                PeerRelayServers::Known(vec!["100.64.0.40".into()]),
            )),
            RelayState::Offline {
                name: "relay-sj".into()
            },
            "permitted server whose tagged node is offline is Offline, not Ready"
        );

        let decoy_online = Relay {
            name: "decoy".into(),
            ip: Some("100.64.0.40".parse().unwrap()),
            online: true,
        };
        let permitted_offline = Relay {
            name: "relay-sj".into(),
            ip: Some("100.64.0.41".parse().unwrap()),
            online: false,
        };
        assert_eq!(
            relay_state(&disc_with(
                vec![decoy_online, permitted_offline],
                PeerRelayServers::Known(vec!["100.64.0.41".into()]),
            )),
            RelayState::Offline {
                name: "relay-sj".into()
            },
            "must not Ready an unmatched online tagged node"
        );

        assert!(RELAY_GRANT.contains("autogroup:member"));
        assert!(RELAY_GRANT.contains("tag:relay"));
        assert!(RELAY_GRANT.contains("tailscale.com/cap/relay"));
        assert!(!RELAY_GRANT.contains("\"*\""));
        assert_eq!(RELAY_DOCS, "https://tailscale.com/docs/features/peer-relay");
    }
}

/// Screens rendered headlessly. The unignored tests check that every state
/// fits the minimum window with no control overlapping another; the
/// ignored ones write PNGs for review:
///
/// ```text
/// cargo test -p latch-client -p latch-host snapshots -- --ignored
/// ```
///
/// Files land in `target/ui-snapshots/` as
/// `client-<state>-<width>x<height>@<scale>x.png`. Nothing here reads or
/// writes the real settings: the window is built with defaults and never
/// saves.
#[cfg(test)]
pub(crate) mod snapshots {
    use super::*;
    use crate::config::KnownPc;
    use crate::session::Relay;
    use egui_kittest::kittest::Queryable;
    use latch_core::api::{Status, Streamer};

    pub const MIN: egui::Vec2 = egui::vec2(640.0, 420.0);
    pub const TYPICAL: egui::Vec2 = egui::vec2(1280.0, 800.0);
    pub const LARGE: egui::Vec2 = egui::vec2(1920.0, 1200.0);
    /// Every size at both scales.
    pub const MATRIX: [(egui::Vec2, f32); 6] = [
        (MIN, 1.0),
        (MIN, 2.0),
        (TYPICAL, 1.0),
        (TYPICAL, 2.0),
        (LARGE, 1.0),
        (LARGE, 2.0),
    ];
    /// The smallest and the usual window, for secondary states.
    pub const PAIR: [(egui::Vec2, f32); 2] = [(MIN, 2.0), (TYPICAL, 2.0)];

    pub fn out_dir() -> std::path::PathBuf {
        let dir =
            std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../target/ui-snapshots");
        std::fs::create_dir_all(&dir).expect("create snapshot dir");
        dir
    }

    pub fn save(img: image::RgbaImage, name: &str) {
        let path = out_dir().join(name);
        img.save(&path).expect("write png");
        eprintln!("wrote {}", path.display());
    }

    /// `client-lobby-1280x800@2x.png`
    pub fn file_name(prefix: &str, state: &str, size: egui::Vec2, ppp: f32) -> String {
        format!(
            "{prefix}-{state}-{}x{}@{}x.png",
            size.x as u32, size.y as u32, ppp as u32
        )
    }

    fn a_status(name: &str, os: &str) -> Status {
        Status {
            app: latch_core::APP_ID.into(),
            version: env!("CARGO_PKG_VERSION").into(),
            name: name.into(),
            os: os.into(),
            power_allowed: true,
            streamer: Streamer {
                kind: latch_core::STREAMER_KIND.into(),
                installed: true,
                running: true,
                api_ok: true,
                protocol: 1,
                encoder: "nvenc".into(),
                audio_problem: String::new(),
                ..Default::default()
            },
            ..Default::default()
        }
    }

    fn ready_pc(id: &str, name: &str, os: &str, ip: &str, rtt: u32, direct: bool) -> Pc {
        Pc {
            node_id: id.into(),
            name: name.into(),
            os: os.into(),
            ip: Some(ip.parse().unwrap()),
            online: true,
            host: Some(a_status(name, os)),
            path: crate::path::Path {
                direct: Some(direct),
                relay: if direct { String::new() } else { "tok".into() },
                rtt_ms: Some(rtt),
                ..Default::default()
            },
            known: Some(KnownPc {
                name: name.into(),
                mac: Some("02:00:00:00:00:01".into()),
                ..Default::default()
            }),
            ..Default::default()
        }
    }

    fn asleep_pc(id: &str, name: &str) -> Pc {
        Pc {
            node_id: id.into(),
            name: name.into(),
            os: "windows".into(),
            ip: Some("100.64.0.30".parse().unwrap()),
            known: Some(KnownPc {
                mac: Some("aa:bb:cc:dd:ee:01".into()),
                ..Default::default()
            }),
            ..Default::default()
        }
    }

    fn found(pcs: Vec<Pc>) -> Discovery {
        Discovery {
            login: "user@example.com".into(),
            pcs,
            refreshed: Some(Instant::now()),
            ..Default::default()
        }
    }

    /// The three machines most screens show: one ready and relayed, one
    /// asleep, one that needs Latch to be installed.
    pub fn pcs() -> Discovery {
        let mut gaming = ready_pc("n1", "Gaming-PC", "windows", "100.64.0.10", 210, false);
        gaming.key_expiry_days = Some(176);
        if let Some(h) = gaming.host.as_mut() {
            h.version = "3.0.0".into();
            h.streamer.encoder = "software".into();
        }
        let den = Pc {
            node_id: "n3".into(),
            name: "Den".into(),
            os: "linux".into(),
            ip: Some("100.64.0.31".parse().unwrap()),
            online: true,
            host: Some(a_status("Den", "linux")),
            ..Default::default()
        };
        let mut d = found(vec![gaming, asleep_pc("n2", "Office"), den]);
        d.self_nat = Some(latch_core::api::NatReport {
            udp: true,
            ipv4: true,
            ipv6: false,
            hard: Some(false),
            portmap: false,
            derp: "tok".into(),
        });
        d
    }

    fn one() -> Discovery {
        found(vec![ready_pc(
            "n1",
            "Studio",
            "macOS",
            "100.64.0.12",
            14,
            true,
        )])
    }

    /// A dozen machines in every state, some with long names.
    fn many() -> Discovery {
        let mut list = vec![
            ready_pc("a", "Gaming-PC", "windows", "100.64.0.10", 18, true),
            ready_pc("b", "Studio", "macOS", "100.64.0.12", 6, true),
            ready_pc(
                "c",
                "render-node-with-a-very-long-hostname-eu-central",
                "linux",
                "100.64.0.13",
                96,
                true,
            ),
            ready_pc("d", "vps-sanjose", "linux", "100.64.0.14", 160, false),
            asleep_pc("e", "Office"),
            asleep_pc("f", "Living-room-PC"),
        ];
        let mut setup = ready_pc("g", "Laptop", "windows", "100.64.0.16", 30, true);
        setup.host.as_mut().unwrap().setup = vec!["Sharing is not set up.".into()];
        list.push(setup);
        let mut not_shared = ready_pc("h", "Workstation", "linux", "100.64.0.17", 22, true);
        not_shared.host.as_mut().unwrap().streamer = Streamer::default();
        list.push(not_shared);
        list.push(Pc {
            node_id: "i".into(),
            name: "phone".into(),
            os: "iOS".into(),
            ip: Some("100.64.0.18".parse().unwrap()),
            online: true,
            ..Default::default()
        });
        list.push(Pc {
            node_id: "j".into(),
            name: "Old-Tower".into(),
            os: "windows".into(),
            ip: Some("100.64.0.19".parse().unwrap()),
            ..Default::default()
        });
        list.push(Pc {
            node_id: "k".into(),
            name: "Media-Server".into(),
            os: "linux".into(),
            ip: Some("100.64.0.20".parse().unwrap()),
            online: true,
            ..Default::default()
        });
        let mut d = found(list);
        d.login = "someone.with.a.long.address@example-mail-provider.com".into();
        d
    }

    fn remembered() -> Discovery {
        let pc = |id: &str, name: &str, seen| Pc {
            node_id: id.into(),
            name: name.into(),
            os: "windows".into(),
            ip: Some("100.64.0.10".parse().unwrap()),
            remembered: true,
            known: Some(KnownPc {
                name: name.into(),
                last_seen_unix: seen,
                ..Default::default()
            }),
            ..Default::default()
        };
        Discovery {
            error: Some("Tailscale is stopped".into()),
            pcs: vec![
                pc("n1", "Gaming-PC", Some(1_788_739_200)),
                pc("n2", "Office", None),
            ],
            refreshed: Some(Instant::now()),
            ..Default::default()
        }
    }

    fn relay_disc(online: bool, servers: PeerRelayServers) -> Discovery {
        Discovery {
            relays: vec![Relay {
                name: "relay-sj".into(),
                ip: Some("100.64.0.40".parse().unwrap()),
                online,
            }],
            peer_relay_servers: servers,
            ..found(vec![])
        }
    }

    /// A stand-in for the host's Sharing page.
    struct Placeholder;
    impl share::SharePage for Placeholder {
        fn show(&mut self, ui: &mut egui::Ui) {
            ui::page_header(ui, "This machine", None, |_| {});
            ui::muted(ui, "The host crate draws this page.");
        }
    }

    /// Everything a screen needs besides the window size.
    pub struct Setup {
        pub disc: Discovery,
        pub prog: Progress,
        pub page: Page,
        pub tailscale: bool,
        pub cfg: ClientConfig,
        pub local: Option<share::LocalShare>,
        pub tweak: fn(&mut ClientApp),
    }

    impl Setup {
        pub fn new(disc: Discovery) -> Self {
            Self {
                disc,
                prog: Progress::default(),
                page: Page::Machines,
                tailscale: true,
                cfg: ClientConfig::default(),
                local: None,
                tweak: |_| {},
            }
        }

        fn page(mut self, page: Page) -> Self {
            self.page = page;
            self
        }

        fn step(mut self, pc: &str, step: Step, detail: &str) -> Self {
            self.prog = Progress {
                pc: pc.into(),
                step,
                detail: detail.into(),
                ..Default::default()
            };
            self
        }

        fn tweak(mut self, f: fn(&mut ClientApp)) -> Self {
            self.tweak = f;
            self
        }

        fn local(mut self, status: Option<Status>) -> Self {
            self.local = Some(share::LocalShare {
                status,
                ..Default::default()
            });
            self
        }

        fn local_share(mut self, share: share::LocalShare) -> Self {
            self.local = Some(share);
            self
        }
    }

    pub fn build(
        setup: Setup,
        size: egui::Vec2,
        ppp: f32,
        gpu: bool,
    ) -> egui_kittest::Harness<'static, ClientApp> {
        let mut builder = egui_kittest::Harness::builder()
            .with_size(size)
            .with_pixels_per_point(ppp)
            .with_max_steps(8);
        if gpu {
            builder = builder.wgpu();
        }
        let Setup {
            disc,
            prog,
            page,
            tailscale,
            cfg,
            local,
            tweak,
        } = setup;
        let mut harness = builder.build_eframe(move |cc| {
            let mut app = ClientApp::headless(cc, disc, prog, cfg);
            if let Some(local) = local {
                app = app.with_local(Arc::new(Mutex::new(local)), Box::new(Placeholder));
            }
            app.tailscale_ok.store(tailscale, Ordering::Relaxed);
            app.open_page(page);
            tweak(&mut app);
            app
        });
        harness.run_steps(3);
        harness
    }

    /// Every widget sits inside the window, and no two controls overlap.
    pub fn assert_fits<S>(h: &egui_kittest::Harness<'_, S>, width: f32) {
        use egui::accesskit::Role;
        use egui_kittest::kittest::By;
        let mut controls = Vec::new();
        for node in h.query_all(By::new().predicate(|_| true)) {
            let Some(b) = node.raw_bounds() else { continue };
            let text = node.label().or_else(|| node.value()).unwrap_or_default();
            assert!(
                b.x0 >= -0.5 && b.x1 <= f64::from(width) + 0.5,
                "{:?} {text:?} runs past the {width}-wide window: {b:?}",
                node.role()
            );
            if matches!(
                node.role(),
                Role::Button | Role::CheckBox | Role::RadioButton | Role::ComboBox | Role::Link
            ) {
                controls.push((
                    text,
                    egui::Rect::from_min_max(
                        egui::pos2(b.x0 as f32, b.y0 as f32),
                        egui::pos2(b.x1 as f32, b.y1 as f32),
                    ),
                ));
            }
        }
        assert!(!controls.is_empty(), "no controls found");
        for (i, (a_name, a)) in controls.iter().enumerate() {
            for (b_name, b) in &controls[i + 1..] {
                let hit = a.intersect(*b);
                assert!(
                    hit.width() <= 1.0 || hit.height() <= 1.0,
                    "{a_name} {a:?} overlaps {b_name} {b:?}"
                );
            }
        }
    }

    /// Every named state, for the fit checks and the PNGs.
    fn states() -> Vec<(&'static str, Setup)> {
        let failed = |pc: &str, e: &str| {
            Setup::new(pcs()).step(
                pc,
                Step::Ended {
                    error: Some(e.into()),
                },
                "",
            )
        };
        vec![
            ("lobby", Setup::new(pcs()).local(Some(a_status("MacBook-Pro", "macOS")))),
            ("lobby-one", Setup::new(one())),
            (
                "lobby-many",
                Setup::new(many()).local(Some({
                    let mut s = a_status("MacBook-Pro", "macOS");
                    s.streamer = Streamer::default();
                    s
                })),
            ),
            ("lobby-looking", Setup::new(Discovery::default())),
            ("lobby-empty", Setup::new(found(vec![]))),
            (
                "lobby-no-tailscale",
                Setup {
                    tailscale: false,
                    ..Setup::new(Discovery::default())
                },
            ),
            ("lobby-tailscale-off", Setup::new(remembered())),
            (
                "lobby-service-starting",
                Setup::new(one()).local(None),
            ),
            (
                "waking",
                Setup::new(pcs()).step("Office", Step::Waking, "Waking Office… 12s"),
            ),
            (
                "waiting",
                Setup::new(pcs()).step("Office", Step::Waiting, "Waiting for Office… 4s"),
            ),
            (
                "connecting",
                Setup::new(pcs()).step("Gaming-PC", Step::Launching, "Measuring the path…"),
            ),
            (
                "confirm-forget",
                Setup::new(remembered()).tweak(|app| {
                    app.confirm = Some(Confirm {
                        node_id: "n2".into(),
                        name: "Office".into(),
                        action: Confirmable::Forget,
                    })
                }),
            ),
            (
                "lobby-mac-not-shared",
                Setup::new(pcs()).local_share(share::LocalShare {
                    service_stopped: true,
                    viewer_only: true,
                    ..Default::default()
                }),
            ),
            (
                "failed",
                failed("Office", "Office didn't wake up. A wake packet reaches it only from its own network, or through a router that forwards UDP port 9 to it. Its Sharing tab in Latch shows whether Wake-on-LAN is ready."),
            ),
            (
                "disconnected",
                failed("Gaming-PC", "The connection to the PC was lost.").tweak(|app| {
                    app.streamed = true;
                    app.stream_failed = true;
                    app.last_pc = Some(pcs().pcs[0].clone());
                }),
            ),
            (
                "ended-offer-sleep",
                Setup::new(pcs())
                    .step("Gaming-PC", Step::Ended { error: None }, "")
                    .tweak(|app| app.offer_sleep = Some(pcs().pcs[0].clone())),
            ),
            (
                "confirm-restart",
                Setup::new(pcs()).tweak(|app| {
                    app.confirm = Some(Confirm {
                        node_id: "n1".into(),
                        name: "Gaming-PC".into(),
                        action: Confirmable::Power(PowerAction::Restart),
                    })
                }),
            ),
            (
                "notice-and-update",
                Setup::new(pcs()).tweak(|app| {
                    app.notice = Some((
                        Tone::Success,
                        "Gaming-PC received the wake packet, so waking it from this network works.".into(),
                        Instant::now(),
                    ));
                    let mut u = app.updates.lock();
                    u.ready = Some(Version::new(4, 1, 0));
                    u.notice = Some((
                        Tone::Info,
                        "Latch 4.1.0 is downloaded and installs when no stream is running."
                            .into(),
                    ));
                }),
            ),
            (
                "updating",
                Setup::new(pcs()).tweak(|app| app.progress.lock().updating = true),
            ),
            (
                "key-expiring",
                Setup::new({
                    let mut d = pcs();
                    d.pcs[0].key_expiry_days = Some(6);
                    d.self_key_days = Some(90);
                    d
                })
            ),
            ("settings", Setup::new(pcs()).page(Page::Settings)),
            (
                "settings-custom",
                Setup::new(pcs()).page(Page::Settings).tweak(|app| {
                    app.cfg.stream.apply_preset(crate::config::Preset::Sharp);
                    app.cfg.stream.fps = 120;
                    app.cfg.auto_update = false;
                }),
            ),
            (
                "relay-unavailable",
                Setup::new(relay_disc(true, PeerRelayServers::Known(vec![]))).page(Page::Settings),
            ),
            ("sharing", Setup::new(pcs()).local(Some(a_status("MacBook-Pro", "macOS"))).page(Page::Sharing)),
        ]
    }

    #[test]
    fn every_state_fits_the_minimum_window() {
        for (name, setup) in states() {
            let h = build(setup, MIN, 1.0, false);
            // A name in the assertion would be nicer; kittest panics
            // inside, so say which state first.
            eprintln!("checking {name}");
            assert_fits(&h, MIN.x);
        }
    }

    #[test]
    fn every_state_fits_a_large_window() {
        for (name, setup) in states() {
            eprintln!("checking {name}");
            let h = build(setup, LARGE, 1.0, false);
            assert_fits(&h, LARGE.x);
        }
    }

    #[test]
    fn the_lobby_lists_machines_with_their_actions() {
        let h = build(Setup::new(pcs()), MIN, 1.0, false);
        assert_eq!(h.query_all_by_label("Connect").count(), 2);
        assert_eq!(h.query_all_by_label("Wake and connect").count(), 1);
        assert!(h.query_by_label("More for Gaming-PC").is_some());
        assert!(h.query_by_label("Settings").is_some());
        let mut disc = pcs();
        disc.login = "someone.with.a.long.name@example-mail-provider.com".into();
        let h = build(Setup::new(disc), MIN, 1.0, false);
        assert_fits(&h, 640.0);
    }

    #[test]
    fn the_row_menu_is_a_popup_with_the_machines_actions() {
        let mut h = build(Setup::new(pcs()), TYPICAL, 1.0, false);
        h.get_by_label("More for Gaming-PC").click();
        h.run_steps(2);
        assert!(h.ctx.memory(|m| m.any_popup_open()));
        for item in ["Sleep", "Restart…", "Shut down…", "Copy Tailscale address"] {
            assert!(h.query_by_label(item).is_some(), "{item}");
        }
        h.get_by_label("Restart…").click();
        h.run_steps(2);
        assert!(
            !h.ctx.memory(|m| m.any_popup_open()),
            "an item closes the menu"
        );
        assert_eq!(
            h.state().confirm,
            Some(Confirm {
                node_id: "n1".into(),
                name: "Gaming-PC".into(),
                action: Confirmable::Power(PowerAction::Restart),
            })
        );
        assert!(
            h.query_by_label("Restart").is_some(),
            "the confirming button"
        );
    }

    #[test]
    fn a_linux_machine_is_never_offered_sleep() {
        let mut h = build(Setup::new(many()), TYPICAL, 1.0, false);
        h.get_by_label("More for vps-sanjose").simulate_click();
        h.run_steps(2);
        assert!(h.query_by_label("Restart…").is_some());
        assert!(h.query_by_label("Sleep").is_none(), "nothing could wake it");
    }

    #[test]
    fn a_machine_that_is_away_can_be_forgotten_after_a_question() {
        // Asleep but still on the tailnet: offered, with what it costs.
        let mut h = build(Setup::new(pcs()), TYPICAL, 1.0, false);
        h.get_by_label("More for Office").simulate_click();
        h.run_steps(2);
        h.get_by_label("Forget this machine…").simulate_click();
        h.run_steps(2);
        assert_eq!(
            h.state().confirm,
            Some(Confirm {
                node_id: "n2".into(),
                name: "Office".into(),
                action: Confirmable::Forget,
            })
        );
        assert!(h.query_by_label("Forget Office?").is_some());
        assert!(h
            .query_by_label_contains("stays in this list while it is on your Tailscale network")
            .is_some());
        // A machine that answers is not offered it: it would only forget saved details.
        let mut h = build(Setup::new(pcs()), TYPICAL, 1.0, false);
        h.get_by_label("More for Gaming-PC").simulate_click();
        h.run_steps(2);
        assert!(h.query_by_label("Forget this machine…").is_none());
        // Remembered while Tailscale is off: offered too.
        let mut h = build(Setup::new(remembered()), TYPICAL, 1.0, false);
        h.get_by_label("More for Office").simulate_click();
        h.run_steps(2);
        assert!(h.query_by_label("Forget this machine…").is_some());
    }

    #[test]
    fn a_mac_with_sharing_off_is_not_reported_as_a_stopped_service() {
        let h = build(
            Setup::new(pcs()).local_share(share::LocalShare {
                service_stopped: true,
                viewer_only: true,
                ..Default::default()
            }),
            MIN,
            1.0,
            false,
        );
        assert!(h.query_by_label_contains("service is stopped").is_none());
        assert!(h
            .query_by_label_contains("only connects to your other machines")
            .is_some());
        assert!(h.query_by_label("Share this Mac…").is_some());
        // Someone pressed Stop: that is what it says.
        let h = build(
            Setup::new(pcs()).local_share(share::LocalShare {
                service_stopped: true,
                ..Default::default()
            }),
            MIN,
            1.0,
            false,
        );
        assert!(h.query_by_label_contains("service is stopped").is_some());
    }

    #[test]
    fn full_screen_is_what_the_system_says_it_is() {
        let mut h = build(Setup::new(pcs()), MIN, 1.0, false);
        assert!(!h.state().fullscreen);
        let root = |h: &mut egui_kittest::Harness<'_, ClientApp>, on| {
            h.input_mut()
                .viewports
                .entry(egui::ViewportId::ROOT)
                .or_default()
                .fullscreen = Some(on);
            h.run_steps(1);
        };
        root(&mut h, true);
        assert!(h.state().fullscreen, "the green button entered it");
        root(&mut h, false);
        assert!(!h.state().fullscreen, "and left it");
    }

    #[test]
    fn a_reconnect_that_fails_before_its_stream_leaves_full_screen() {
        let setup = Setup::new(pcs())
            .step(
                "Gaming-PC",
                Step::Ended {
                    error: Some("timed out".into()),
                },
                "",
            )
            .tweak(|app| {
                app.ended_seen = false;
                app.fullscreen = true;
                app.fullscreen_after = Some(false);
            });
        let h = build(setup, MIN, 1.0, false);
        assert_eq!(h.state().fullscreen_after, None, "the window went back");
    }

    #[test]
    fn a_notices_quiet_action_lines_up_with_its_text() {
        for state in ["failed", "confirm-restart"] {
            let setup = states().into_iter().find(|(n, _)| *n == state).unwrap().1;
            let h = build(setup, TYPICAL, 1.0, false);
            let (title, action) = match state {
                "failed" => ("Couldn't connect to Office", "Dismiss"),
                _ => ("Restart Gaming-PC?", "Restart"),
            };
            let text = h.get_by_label(title).raw_bounds().unwrap().x0;
            let button = h.get_by_label(action).raw_bounds().unwrap().x0;
            // A quiet button's label starts its padding in from its box;
            // a filled one's edge is what lines up.
            let label = if action == "Dismiss" {
                button + f64::from(space::MD)
            } else {
                button
            };
            assert!(
                (label - text).abs() <= 0.5,
                "{state}: {action} starts at {label}, the text at {text}"
            );
        }
    }

    #[test]
    fn row_menus_stay_inside_the_smallest_window() {
        // Gaming-PC's menu is the longest; Den's row is the lowest, so its
        // menu has to open upwards.
        for pc in ["Gaming-PC", "Office", "Den"] {
            let mut h = build(Setup::new(pcs()), MIN, 1.0, false);
            crate::stream::tests::assert_menu_fits(
                &mut h,
                MIN,
                &format!("More for {pc}"),
                "Copy Tailscale address",
            );
        }
        // A row near the bottom of the window, as the page first shows it:
        // its menu has no room below, so it opens upwards.
        let mut h = build(Setup::new(many()), MIN, 1.0, false);
        h.get_by_label("More for vps-sanjose").simulate_click();
        h.run_steps(3);
        let menu = ui::open_menu_rect(&h.ctx).expect("the menu is open");
        let trigger = h.get_by_label("More for vps-sanjose").raw_bounds().unwrap();
        assert!(
            f64::from(menu.bottom()) <= trigger.y0,
            "a low row's menu opens upwards: {menu:?}"
        );
        crate::stream::tests::assert_inside(
            "vps-sanjose's menu",
            menu,
            egui::Rect::from_min_size(egui::Pos2::ZERO, MIN),
        );
    }

    #[test]
    fn tab_reaches_a_rows_controls_in_reading_order() {
        use egui_kittest::kittest::By;
        let mut h = build(Setup::new(one()), MIN, 1.0, false);
        let mut order = Vec::new();
        for _ in 0..6 {
            h.press_key(egui::Key::Tab);
            h.run_steps(1);
            if let Some(n) = h.query_all(By::new().predicate(|n| n.is_focused())).next() {
                order.push(n.label().unwrap_or_default());
            }
        }
        let at = |label: &str| order.iter().position(|l| l == label);
        let (Some(connect), Some(more)) = (at("Connect"), at("More for Studio")) else {
            panic!("Tab never reached the row: {order:?}");
        };
        assert!(connect < more, "Connect comes before its menu: {order:?}");
        assert!(
            at("Machines").is_some_and(|m| m < connect),
            "the tabs come first: {order:?}"
        );
    }

    #[test]
    fn this_machine_leads_the_list_and_opens_sharing() {
        let mut h = build(
            Setup::new(pcs()).local(Some(a_status("MacBook-Pro", "macOS"))),
            MIN,
            1.0,
            false,
        );
        assert!(h.query_by_label("MacBook-Pro").is_some());
        h.get_by_label("Manage").click();
        h.run_steps(2);
        assert_eq!(h.state().page, Page::Sharing);
        assert!(h
            .query_by_label("The host crate draws this page.")
            .is_some());
    }

    #[test]
    fn the_standalone_viewer_has_no_sharing_tab() {
        let h = build(Setup::new(pcs()), MIN, 1.0, false);
        assert!(h.query_by_label("Sharing").is_none());
        let h = build(
            Setup::new(pcs()).local(Some(a_status("MacBook-Pro", "macOS"))),
            MIN,
            1.0,
            false,
        );
        assert!(h.query_by_label("Sharing").is_some());
    }

    #[test]
    fn tabs_switch_pages() {
        let mut h = build(Setup::new(pcs()), MIN, 1.0, false);
        assert_eq!(h.state().page, Page::Machines);
        assert!(h.query_by_label("How peer relays work").is_none());
        h.get_by_label("Settings").click();
        h.run_steps(3);
        assert_eq!(h.state().page, Page::Settings);
        assert!(h.query_by_label("How peer relays work").is_some());
        h.get_by_label("Machines").click();
        h.run_steps(3);
        assert_eq!(h.state().page, Page::Machines);
    }

    #[test]
    fn keyboard_shortcuts_switch_pages() {
        let mut h = build(Setup::new(pcs()), MIN, 1.0, false);
        h.press_key_modifiers(egui::Modifiers::COMMAND, egui::Key::Comma);
        h.run_steps(2);
        assert_eq!(h.state().page, Page::Settings);
        h.press_key(egui::Key::Escape);
        h.run_steps(2);
        assert_eq!(h.state().page, Page::Machines);
        h.press_key_modifiers(egui::Modifiers::COMMAND, egui::Key::Num2);
        h.run_steps(2);
        assert_eq!(
            h.state().page,
            Page::Settings,
            "with no Sharing tab, the second shortcut is Settings"
        );
    }

    #[test]
    fn open_source_notices_unfold_in_settings() {
        let mut h = build(
            Setup::new(pcs()).page(Page::Settings),
            egui::vec2(640.0, 3000.0),
            1.0,
            false,
        );
        assert!(h
            .query_by_label_contains("Geist Mono, SIL Open Font License")
            .is_none());
        h.get_by_label("Open-source notices").click();
        h.run_steps(3);
        assert!(
            h.query_by_label_contains("Geist Mono, SIL Open Font License")
                .is_some(),
            "the typeface's licence is listed"
        );
        assert_fits(&h, 640.0);
    }

    #[test]
    fn a_failure_offers_a_retry_and_a_lighter_profile_only_for_stream_failures() {
        let failed = |streamed: bool| {
            let mut setup = Setup::new(pcs()).step(
                "Gaming-PC",
                Step::Ended {
                    error: Some("The connection to the PC was lost.".into()),
                },
                "",
            );
            setup.tweak = if streamed {
                |app| {
                    app.streamed = true;
                    app.stream_failed = true;
                    app.last_pc = Some(pcs().pcs[0].clone());
                }
            } else {
                |app| app.last_pc = Some(pcs().pcs[0].clone())
            };
            build(setup, MIN, 1.0, false)
        };
        let h = failed(true);
        assert!(h.query_by_label("Gaming-PC disconnected").is_some());
        assert!(h.query_by_label("Try again").is_some());
        assert!(h.query_by_label("Try at 1080p, 20 Mbps").is_some());
        let h = failed(false);
        assert!(h.query_by_label("Couldn't connect to Gaming-PC").is_some());
        assert!(h.query_by_label("Try at 1080p, 20 Mbps").is_none());
    }

    #[test]
    fn failed_connect_never_offers_sleep() {
        let mut setup = Setup::new(pcs()).step(
            "Gaming-PC",
            Step::Ended { error: None },
            "",
        );
        setup.prog.node_id = "n1".into();
        setup.cfg.sleep_prompt = true;
        setup.tweak = |app| {
            app.ended_seen = false;
            app.streamed = false;
        };
        let h = build(setup, MIN, 1.0, false);
        assert!(h.query_by_label("Session with Gaming-PC ended").is_none());
        assert!(h.query_by_label("Sleep Gaming-PC").is_none());
        assert!(h.state().offer_sleep.is_none());
    }

    #[test]
    fn a_takeover_shows_the_hosts_message_without_a_connection_error() {
        let mut h = build(Setup::new(pcs()), MIN, 1.0, false);
        let (tx, rx) = std::sync::mpsc::channel();
        let mut live = crate::stream::tests::dummy_live(&h.ctx);
        live.events = rx;
        *h.state().live.lock() = Some(live);
        tx.send(Event::Terminated {
            code: 1,
            message: "Another device is watching Gaming-PC.".into(),
        })
        .unwrap();
        let ctx = h.ctx.clone();
        h.state_mut().poll_live(&ctx);
        assert!(!h.state().stream_failed);
        assert_eq!(h.state().progress.lock().step, Step::Ended { error: None });
        let notice = h.state().notice.as_ref().unwrap();
        assert_eq!(notice.0, Tone::Info);
        assert_eq!(notice.1, "Another device is watching Gaming-PC.");
    }

    #[test]
    fn panicking_notify_job_clears_pending_notice() {
        let mut h = build(Setup::new(pcs()), MIN, 1.0, false);
        h.state_mut().notify_later(|| {
            panic!("simulated panic in background job");
        });
        std::thread::sleep(Duration::from_millis(50));
        h.state_mut().poll_notice();
        assert!(h.state().pending_notice.is_none());
    }

    #[test]
    #[ignore = "renders changed streaming UI text with a GPU"]
    fn snapshots_client_migration() {
        for (name, setup, size) in [
            ("lobby", Setup::new(pcs()), TYPICAL),
            ("lobby-many", Setup::new(many()), TYPICAL),
            (
                "waiting",
                Setup::new(pcs()).step("Office", Step::Waiting, "Waiting for Office… 4s"),
                MIN,
            ),
            (
                "confirm-forget",
                Setup::new(remembered()).tweak(|app| {
                    app.confirm = Some(Confirm {
                        node_id: "n2".into(),
                        name: "Office".into(),
                        action: Confirmable::Forget,
                    })
                }),
                TYPICAL,
            ),
            (
                "settings-full",
                Setup::new(pcs()).page(Page::Settings),
                egui::vec2(1280.0, 2300.0),
            ),
        ] {
            let mut h = build(setup, size, 1.0, true);
            save(
                h.render().unwrap(),
                &file_name("migration", name, size, 1.0),
            );
        }
        let mut h = build(
            Setup::new(pcs()).page(Page::Settings),
            egui::vec2(1280.0, 3000.0),
            1.0,
            true,
        );
        h.get_by_label("Open-source notices").click();
        h.run_steps(3);
        save(h.render().unwrap(), "migration-open-source.png");
    }

    #[test]
    fn tagged_relay_nodes_stay_out_of_the_pc_list_and_in_discovery() {
        let disc = discovery_from_status(TAGGED_RELAY_STATUS);
        assert_eq!(
            disc.pcs.iter().map(|p| p.name.as_str()).collect::<Vec<_>>(),
            ["Gaming-PC", "tagged-pc"]
        );
        assert_eq!(
            disc.relays
                .iter()
                .map(|r| r.name.as_str())
                .collect::<Vec<_>>(),
            ["relay-pc", "sj-instance"]
        );
        let h = build(Setup::new(disc.clone()), MIN, 1.0, false);
        assert_fits(&h, 640.0);
        assert!(h.query_by_label("Gaming-PC").is_some());
        assert!(h.query_by_label("tagged-pc").is_some());
        assert!(h.query_by_label("sj-instance").is_none());
        assert!(h.query_by_label("relay-pc").is_none());
        assert_eq!(h.query_all_by_label("Connect").count(), 2);

        let h = build(Setup::new(disc).page(Page::Settings), MIN, 1.0, false);
        assert_fits(&h, 640.0);
        assert!(h.query_by_label("sj-instance").is_none());
        assert!(h.query_by_label("relay-pc").is_none());
    }

    const TAGGED_RELAY_STATUS: &str = r#"{"BackendState":"Running","Peer":{
      "nodekey:relay": {"ID":"nRELAY","HostName":"sj-instance","OS":"linux","Online":true,
                        "TailscaleIPs":["100.64.0.40"],"Tags":["tag:relay"]},
      "nodekey:winrelay": {"ID":"nWINREL","HostName":"relay-pc","OS":"windows","Online":true,
                           "TailscaleIPs":["100.64.0.42"],"Tags":["tag:relay"]},
      "nodekey:pc": {"ID":"nPC","HostName":"Gaming-PC","OS":"windows","Online":true,
                     "TailscaleIPs":["100.64.0.41"]},
      "nodekey:other": {"ID":"nOTHER","HostName":"tagged-pc","OS":"windows","Online":true,
                        "TailscaleIPs":["100.64.0.43"],"Tags":["tag:other"]}
    }}"#;

    fn discovery_from_status(json: &str) -> Discovery {
        let st = latch_core::tailscale::parse_status(json).expect("fixture");
        Discovery {
            login: "user@example.com".into(),
            refreshed: Some(Instant::now()),
            pcs: st
                .machine_peers()
                .into_iter()
                .map(|n| Pc {
                    node_id: n.id.clone(),
                    name: n.host_name.clone(),
                    os: n.os.clone(),
                    ip: n.ipv4(),
                    online: n.online,
                    host: n.online.then(|| a_status(&n.host_name, &n.os)),
                    ..Default::default()
                })
                .collect(),
            relays: st
                .relay_peers()
                .into_iter()
                .map(|n| Relay {
                    name: n.host_name.clone(),
                    ip: n.ipv4(),
                    online: n.online,
                })
                .collect(),
            ..Default::default()
        }
    }

    #[test]
    fn relay_section_fits_the_minimum_window_in_every_state() {
        let states = [
            found(vec![]),
            relay_disc(false, PeerRelayServers::Unknown),
            relay_disc(true, PeerRelayServers::Unknown),
            relay_disc(true, PeerRelayServers::Known(vec![])),
            relay_disc(true, PeerRelayServers::Known(vec!["100.64.0.99".into()])),
            relay_disc(true, PeerRelayServers::Known(vec!["100.64.0.40".into()])),
        ];
        for disc in states {
            let h = build(Setup::new(disc).page(Page::Settings), MIN, 1.0, false);
            assert_fits(&h, 640.0);
        }
        let tall = egui::vec2(640.0, 3000.0);
        let h = build(
            Setup::new(relay_disc(true, PeerRelayServers::Known(vec![]))).page(Page::Settings),
            tall,
            1.0,
            false,
        );
        assert!(h.query_by_label("Copy grant").is_some());
        assert!(h.query_by_label("How peer relays work").is_some());
        let h = build(
            Setup::new(found(vec![])).page(Page::Settings),
            tall,
            1.0,
            false,
        );
        assert!(h.query_by_label("Copy grant").is_none());
        assert!(h.query_by_label("How peer relays work").is_some());
        let h = build(
            Setup::new(relay_disc(
                true,
                PeerRelayServers::Known(vec!["100.64.0.40".into()]),
            ))
            .page(Page::Settings),
            tall,
            1.0,
            false,
        );
        assert!(h.query_by_label("Copy grant").is_none());
        assert!(h.query_by_label("Using relay-sj (100.64.0.40)").is_some());
    }

    /// The window never repaints on a timer while nothing is happening.
    #[test]
    fn an_idle_window_asks_for_no_repaint() {
        let mut h = build(Setup::new(pcs()), TYPICAL, 1.0, false);
        h.run_steps(4);
        let delay = h
            .output()
            .viewport_output
            .get(&egui::ViewportId::ROOT)
            .map(|v| v.repaint_delay)
            .unwrap_or(Duration::MAX);
        assert_eq!(delay, Duration::MAX, "the idle lobby keeps repainting");
    }

    #[test]
    #[ignore = "renders with a GPU; run on demand to review the UI"]
    fn snapshots_every_state() {
        let key = [
            "lobby",
            "lobby-many",
            "lobby-no-tailscale",
            "failed",
            "settings",
        ];
        for (name, _) in states() {
            let sizes: &[(egui::Vec2, f32)] = if key.contains(&name) { &MATRIX } else { &PAIR };
            for &(size, ppp) in sizes {
                let setup = states().into_iter().find(|(n, _)| *n == name).unwrap().1;
                let mut h = build(setup, size, ppp, true);
                save(h.render().unwrap(), &file_name("client", name, size, ppp));
            }
        }
    }

    /// Long pages whole, to review what scrolls.
    #[test]
    #[ignore = "renders with a GPU; run on demand to review the UI"]
    fn snapshots_full_pages() {
        for (name, setup, h) in [
            ("settings", Setup::new(pcs()).page(Page::Settings), 2300.0),
            (
                "relay-unavailable",
                Setup::new(relay_disc(true, PeerRelayServers::Known(vec![]))).page(Page::Settings),
                2400.0,
            ),
            ("lobby-many", Setup::new(many()), 1100.0),
            (
                "key-expiring",
                Setup::new({
                    let mut d = pcs();
                    d.pcs[0].key_expiry_days = Some(6);
                    d.self_key_days = Some(90);
                    d
                }),
                1000.0,
            ),
        ] {
            let size = egui::vec2(1280.0, h);
            let mut harness = build(setup, size, 1.0, true);
            save(
                harness.render().unwrap(),
                &format!("client-{name}-full.png"),
            );
        }
        let mut h = build(
            Setup::new(pcs()).page(Page::Settings),
            egui::vec2(1280.0, 3000.0),
            1.0,
            true,
        );
        h.get_by_label("Open-source notices").click();
        h.run_steps(3);
        save(h.render().unwrap(), "client-open-source-full.png");
        let mut h = build(Setup::new(pcs()), TYPICAL, 2.0, true);
        h.get_by_label("More for Gaming-PC").click();
        h.run_steps(3);
        save(h.render().unwrap(), "client-row-menu-1280x800@2x.png");
        // At the smallest size: Tab reaches the longest menu's last item,
        // and a low row's menu, opened where the page first shows it,
        // opens upwards.
        for ppp in [1.0, 2.0] {
            let mut h = build(Setup::new(pcs()), MIN, ppp, true);
            crate::stream::tests::assert_menu_fits(
                &mut h,
                MIN,
                "More for Gaming-PC",
                "Copy Tailscale address",
            );
            save(
                h.render().unwrap(),
                &file_name("client", "row-menu", MIN, ppp),
            );
            let mut h = build(Setup::new(many()), MIN, ppp, true);
            h.get_by_label("More for vps-sanjose").simulate_click();
            h.run_steps(3);
            let menu = ui::open_menu_rect(&h.ctx).expect("the menu is open");
            let trigger = h.get_by_label("More for vps-sanjose").raw_bounds().unwrap();
            assert!(
                f64::from(menu.bottom()) <= trigger.y0,
                "a low row's menu opens upwards: {menu:?}"
            );
            crate::stream::tests::assert_inside(
                "vps-sanjose's menu",
                menu,
                egui::Rect::from_min_size(egui::Pos2::ZERO, MIN),
            );
            save(
                h.render().unwrap(),
                &file_name("client", "row-menu-low-row", MIN, ppp),
            );
        }
        let mut h = build(Setup::new(pcs()), TYPICAL, 2.0, true);
        h.get_by_label_contains("Connection details").click();
        h.run_steps(3);
        save(h.render().unwrap(), "client-details-open-1280x800@2x.png");
    }

    /// Keyboard focus is visible on every kind of control.
    #[test]
    #[ignore = "renders with a GPU; run on demand to review the UI"]
    fn snapshots_keyboard_focus() {
        for (name, page, tabs) in [
            ("lobby", Page::Machines, 5),
            ("settings", Page::Settings, 5),
            ("settings-switch", Page::Settings, 13),
        ] {
            let mut h = build(Setup::new(pcs()).page(page), TYPICAL, 2.0, true);
            for _ in 0..tabs {
                h.press_key(egui::Key::Tab);
                h.run_steps(1);
            }
            h.run_steps(2);
            assert!(
                h.ctx.memory(|m| m.focused()).is_some(),
                "Tab reaches a control on {name}"
            );
            save(
                h.render().unwrap(),
                &format!("client-focus-{name}-1280x800@2x.png"),
            );
        }
    }

    /// The stream screen as the window shows it, before any picture has
    /// arrived: the session points at an address that never answers.
    #[test]
    #[ignore = "renders with a GPU; run on demand to review the UI"]
    fn snapshots_stream_connecting() {
        let mut h = build(Setup::new(pcs()), egui::vec2(1400.0, 860.0), 1.0, true);
        let ctx = h.ctx.clone();
        let live = crate::stream::tests::dummy_live(&ctx);
        *h.state().live.lock() = Some(live);
        h.state().progress.lock().step = Step::Streaming;
        h.run_steps(3);
        save(h.render().unwrap(), "client-stream-connecting.png");
        h.state_mut().disconnect();
    }
}

#[cfg(test)]
mod window_fit {
    use super::window_size_for;

    #[test]
    fn a_window_takes_the_streams_proportions_at_its_own_width() {
        let s = window_size_for(egui::Vec2::new(1100.0, 720.0), 3024.0 / 1964.0);
        assert_eq!(s.x, 1100.0);
        assert!((s.y - 714.0).abs() <= 1.0, "{}", s.y);
        let s = window_size_for(egui::Vec2::new(1100.0, 720.0), 16.0 / 9.0);
        assert!((s.y - 619.0).abs() <= 1.0, "{}", s.y);
        let tiny = window_size_for(egui::Vec2::new(300.0, 100.0), 16.0 / 9.0);
        assert_eq!(
            tiny,
            egui::Vec2::new(640.0, 420.0),
            "never under the minimum"
        );
    }
}

/// Streams from Latch on this machine into the window for a few
/// seconds and saves what the window shows:
/// `LATCH_DEV_LOCAL=1 cargo test -p latch-client stream_snapshot -- --ignored --nocapture`
#[cfg(test)]
mod live_snapshot {
    use super::*;

    #[test]
    #[ignore = "needs a Latch 5 host on this machine, completed protocol/codec implementations and a GPU"]
    fn stream_snapshot() {
        let _ = tracing_subscriber::fmt().with_target(false).try_init();
        let disc = Arc::new(Mutex::new(Discovery::default()));
        let prog = Arc::new(Mutex::new(Progress::default()));
        let mut harness = egui_kittest::Harness::builder()
            .wgpu()
            .with_size(egui::vec2(1512.0, 982.0))
            .with_pixels_per_point(2.0)
            .with_max_steps(8)
            .build_eframe({
                let (disc, prog) = (disc.clone(), prog.clone());
                move |cc| {
                    let mut app = ClientApp::with_shared(cc, disc, prog, ClientConfig::load());
                    app.cfg.stream.fullscreen = false;
                    if std::env::var_os("LATCH_TEST_PC").is_none() {
                        app.cfg.stream.resolution = Resolution::P1080;
                        app.cfg.stream.codec = Codec::H264;
                    }
                    if let Ok(codec) = std::env::var("LATCH_TEST_CODEC") {
                        app.cfg.stream.codec = match codec.as_str() {
                            "h264" => Codec::H264,
                            "hevc" => Codec::Hevc,
                            _ => panic!("LATCH_TEST_CODEC must be h264 or hevc"),
                        };
                    }
                    app
                }
            });
        harness.run_steps(2);
        let pc = if let Ok(name) = std::env::var("LATCH_TEST_PC") {
            let cfg = ClientConfig::load();
            let (id, known) = cfg
                .pcs
                .iter()
                .find(|(id, pc)| **id == name || pc.name == name)
                .expect("LATCH_TEST_PC must name a saved PC");
            Pc {
                node_id: id.clone(),
                name: known.name.clone(),
                ip: Some(known.tailscale_ip.as_ref().unwrap().parse().unwrap()),
                online: true,
                known: Some(known.clone()),
                ..Default::default()
            }
        } else {
            Pc {
                node_id: "local".into(),
                name: "GAMING-PC".into(),
                ip: Some("127.0.0.1".parse().unwrap()),
                online: true,
                ..Default::default()
            }
        };
        let ctx = harness.ctx.clone();
        harness.state_mut().connect(&ctx, &pc);
        let start = Instant::now();
        let mut shot = 0;
        let mut decoded = 0;
        let mut black = None;
        let mut problem = None;
        let mut sampled = 0;
        while start.elapsed() < Duration::from_secs(20) {
            harness.run_steps(1);
            let step = prog.lock().step.clone();
            if let Step::Ended { error } = step {
                panic!("ended: {error:?}");
            }
            if let Some(live) = harness.state().live.lock().as_ref() {
                decoded = live.frames.seq();
                let second = start.elapsed().as_secs();
                if second > sampled {
                    sampled = second;
                    eprintln!("sample {second}s: {:?}", live.session.stats());
                }
            }
            if step == Step::Streaming
                && decoded > 30
                && start.elapsed() > Duration::from_secs(15)
                && shot == 0
            {
                if let Some(live) = harness.state().live.lock().as_ref() {
                    problem = live.session.stats().video_problem;
                    if let Some(frame) = live.frames.take() {
                        black = Some(frame.is_black());
                        eprintln!("black picture: {black:?}; video problem: {problem:?}");
                        live.frames.publish(frame);
                    }
                }
                let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
                    .join("../../target/ui-snapshots");
                std::fs::create_dir_all(&dir).unwrap();
                harness
                    .render()
                    .unwrap()
                    .save(dir.join("client-stream.png"))
                    .unwrap();
                eprintln!("wrote client-stream.png");
                shot += 1;
            }
            std::thread::sleep(Duration::from_millis(16));
        }
        eprintln!("decoded {decoded} frames");
        let expect_black = std::env::var_os("LATCH_TEST_EXPECT_BLACK").is_some();
        harness.state_mut().disconnect();
        let t = Instant::now();
        while harness.state().live.lock().is_some() && t.elapsed() < Duration::from_secs(8) {
            harness.run_steps(1);
            std::thread::sleep(Duration::from_millis(20));
        }
        assert!(harness.state().live.lock().is_none(), "stream did not stop");
        assert_eq!(shot, 1, "never decoded video: {:?}", prog.lock().step);
        assert!(decoded > 60, "too few decoded frames: {decoded}");
        if expect_black {
            assert_eq!(black, Some(true));
            assert!(problem
                .as_deref()
                .is_some_and(|p| p.contains("black picture")));
        } else {
            assert_eq!(
                black,
                Some(false),
                "the host supplied no visible picture: {problem:?}"
            );
        }
    }
}
