//! The stream screen: the other machine's picture filling the window, and
//! nothing else until the host key (Ctrl+Alt; Control-Option on a Mac) is
//! pressed. That frees the mouse and drops a toolbar over the top of the
//! picture, the way a hypervisor's host key does; a click on the picture
//! (or the host key again) hides it and captures the mouse back. Short
//! notices (clipboard, connection, an install in progress) appear under
//! the toolbar's place and go after a few seconds.
//!
//! Nothing here costs a frame while the toolbar is away: the bar, its
//! menus and the panels are only laid out while they show, and every
//! timed notice asks for a bounded, slow repaint only while it is up.

use crate::clipboard::{self, Edit};
use crate::config::{ClientConfig, StreamSettings};
use crate::input::{self, Held, Sink};
use crate::path::Path;
use crate::session::Live;
use crate::video;
use egui::{
    Align2, Color32, CursorIcon, Event, Frame, Id, Margin, Modifiers, Pos2, Rect, RichText, Vec2,
    ViewportCommand,
};
use latch_core::api::PowerAction;
use latch_ui::{self as ui, size, space, theme, Kv, Tone, PALETTE as P};
use semver::Version;
use std::time::{Duration, Instant};

/// Below this width the toolbar's menus fold into one More menu, so they
/// cannot overlap Disconnect or the machine's name.
const OVERFLOW_BELOW: f32 = 1040.0;
/// Below this width the toolbar drops the host-key hint.
const HINT_BELOW: f32 = 1200.0;
/// Below this width the toolbar drops the stream facts after the name.
const FACTS_BELOW: f32 = 1560.0;
const TOAST_FOR: Duration = Duration::from_secs(5);
/// How long the host-key hint stays up when a stream starts.
const HINT_FOR: Duration = Duration::from_secs(6);
/// How long a paste's chord, and what is typed after it, wait for its text
/// to reach the PC before they go anyway.
const PASTE_WAIT: Duration = Duration::from_secs(2);
/// Each side of an overlay frame (its inner margin) and the picture it
/// leaves visible beyond that.
const OVERLAY_PAD: f32 = 12.0;

/// Content width of an overlay: `max`, or what the screen leaves once the
/// frame's sides and a margin either side are taken off.
fn overlay_width(max: f32, screen_w: f32) -> f32 {
    max.min(screen_w - 4.0 * OVERLAY_PAD).max(0.0)
}

/// The host key as printed on this keyboard, and as written in a sentence.
pub fn host_key() -> (&'static [&'static str], &'static str) {
    if cfg!(target_os = "macos") {
        (&["control", "option"], "Control-Option")
    } else {
        (&["Ctrl", "Alt"], "Ctrl+Alt")
    }
}

/// What to call the other machine in the toolbar: its power menu reads
/// "PC" for Windows, as the lobby's messages expect.
fn machine_noun(os: &str) -> &'static str {
    if os.to_ascii_lowercase().starts_with("windows") {
        "PC"
    } else if is_mac_os(os) {
        "Mac"
    } else {
        "Machine"
    }
}

/// Whether the machine's menu has anything in it: power actions its owner
/// allows, or an update for an old host.
fn has_power_items(env: &Env<'_>) -> bool {
    env.power_allowed || env.old_host.is_some()
}

/// What a power action costs, said before it is sent to `pc`.
fn power_question(action: PowerAction, pc: &str) -> String {
    match action {
        PowerAction::Sleep => format!("The stream ends once {pc} agrees. Asleep, it can only be woken from its own network, or through a router that forwards wake packets to it."),
        PowerAction::Restart | PowerAction::Shutdown => format!("The stream ends once {pc} agrees. Programs there close without asking, and anything unsaved is lost."),
    }
}

/// Whether Tailscale's OS string names a Mac.
fn is_mac_os(os: &str) -> bool {
    let os = os.to_ascii_lowercase();
    os.starts_with("macos") || os.starts_with("darwin")
}

#[derive(Debug, Clone, PartialEq)]
pub enum Action {
    Disconnect,
    Reconnect,
    /// Ask the PC to leave HDR, which is what a black capture usually is.
    TurnOffHdr,
    Power(PowerAction),
    Fullscreen(bool),
    ToggleCmd,
    /// Install a new Latch Host on the PC through the stream.
    InstallHost,
    ApplySettings(StreamSettings),
    /// Whether a click on the picture captures the mouse.
    MouseCapture(bool),
}

/// An unexpected connection drop reconnection state machine.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReconnectState {
    pub attempt: u32,
    pub deadline: Instant,
    pub in_flight: bool,
}

pub fn delay_for_attempt(attempt: u32) -> Duration {
    match attempt {
        1 => Duration::from_secs(2),
        2 => Duration::from_secs(5),
        _ => Duration::from_secs(10),
    }
}

impl ReconnectState {
    pub fn new(now: Instant) -> Self {
        Self::with_attempt(1, now)
    }

    pub fn with_attempt(attempt: u32, now: Instant) -> Self {
        Self {
            attempt,
            deadline: now + delay_for_attempt(attempt),
            in_flight: false,
        }
    }

    pub fn remaining(&self, now: Instant) -> Duration {
        self.deadline.saturating_duration_since(now)
    }

    pub fn remaining_secs(&self, now: Instant) -> u64 {
        let rem = self.remaining(now);
        if rem.is_zero() {
            0
        } else {
            (rem.as_millis() as u64).div_ceil(1000)
        }
    }

    pub fn is_due(&self, now: Instant) -> bool {
        now >= self.deadline
    }

    pub fn next_attempt(&self, now: Instant) -> Option<Self> {
        if self.attempt >= 3 {
            None
        } else {
            Some(Self::with_attempt(self.attempt + 1, now))
        }
    }
}

/// An interruption in the live stream before dropping the stream window.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Interruption {
    Reconnecting(ReconnectState),
    Failed,
    TakenOver(String),
    HostSleeping,
}

impl Interruption {
    pub fn reconnecting(now: Instant) -> Self {
        Self::Reconnecting(ReconnectState::new(now))
    }
}

/// What the window knows that the stream screen should show.
pub struct Env<'a> {
    pub live: &'a Live,
    pub cfg: &'a ClientConfig,
    pub fullscreen: bool,
    /// The path as discovery sees it now; fresher than `live.path`.
    pub path: Option<Path>,
    /// The other machine's Tailscale OS string, for naming its menu.
    pub os: String,
    /// The PC's host version when it cannot take updates over the network,
    /// and the release that could be installed through the stream.
    pub old_host: Option<(Version, Option<Version>)>,
    /// An install through the stream, its status line.
    pub handover: Option<String>,
    /// What the PC said about a black picture, once it has answered.
    pub video_help: Option<crate::display::Help>,
    /// Latch Host there takes power actions from this Mac.
    pub power_allowed: bool,
    /// Sleep is offered: something can wake the machine again.
    pub can_sleep: bool,
}

struct Toast {
    tone: Tone,
    text: String,
    at: Instant,
}

pub struct View {
    /// The mouse is captured: hidden, held in place, raw movement sent.
    captured: bool,
    grabbed: bool,
    /// The host key dropped the toolbar over the picture.
    bar_shown: bool,
    /// The stats overlay over the picture's corner.
    stats: bool,
    /// The Stream performance panel, with everything and a copy button.
    details: bool,
    /// What the renderer has drawn, for the overlay.
    presented: Presented,
    /// Whether the mouse was captured and the toolbar up before the host
    /// key went down, so a stats chord (host key + S) can put both back.
    chord_before: Option<(bool, bool)>,
    settings: Option<StreamSettings>,
    held: Held,
    scroll: (f32, f32),
    motion: (f32, f32),
    /// Last measured toolbar height, so the notices sit under it.
    bar_h: f32,
    confirm: Option<PowerAction>,
    confirm_install: bool,
    pub(crate) confirm_disconnect: Option<Instant>,
    pub(crate) volume: u32,
    pub(crate) muted: bool,
    composing: bool,
    pub(crate) interrupted: Option<Interruption>,
    poor: bool,
    poor_hinted: bool,
    toasts: Vec<Toast>,
    /// When the stream started, for the host-key hint.
    hint_since: Option<Instant>,
    /// Ctrl+Alt was down when capture last toggled; ignore further edges
    /// until both keys are up. `release_all` clears `Held.modifiers`, so
    /// deriving "was down" from that retriggered the toggle every frame.
    capture_chord_held: bool,
    /// A paste whose text is still on its way to the PC.
    pending: Option<PendingPaste>,
    /// Where the pointer went in free mode, not yet sent.
    moved_to: Option<Pos2>,
    /// Since V was last let go: a paste was made, and ⌘ was down. A ⌘V
    /// egui swallowed (the Mac's clipboard holds no text) shows only as a V
    /// let go that the PC never saw pressed, with ⌘ seen and no paste.
    pasted_since_v_up: bool,
    cmd_since_v_up: bool,
    /// The other machine is a Mac: ⌘ is Command there, whatever the
    /// "Command acts as Ctrl" setting says for Windows and Linux.
    host_mac: bool,
    /// Texture handle and shape serial for rendering host cursor locally.
    cursor_texture: Option<(u32, egui::TextureHandle)>,
    /// Whether the host is currently told to draw its pointer into video.
    host_cursor_drawn: bool,
    /// The Mac's Caps Lock as the PC was last brought in line with it (see
    /// [`caps_tap`]); None until the stream first has the keys.
    caps: Option<bool>,
    /// Caps Lock as a test sets it, in place of the Mac's.
    #[cfg(test)]
    caps_now: Option<bool>,
    /// A video problem to show in place of the session's, for rendering
    /// the recovery overlay without a broken capture.
    #[cfg(test)]
    problem: Option<String>,
    /// Statistics to show in place of the session's, for rendering the
    /// overlay without a stream.
    #[cfg(test)]
    fake_stats: Option<latch_stream::Stats>,
    /// Where input goes in tests instead of the PC, and whether the
    /// stream counts as connected.
    #[cfg(test)]
    tap: Option<std::rc::Rc<input::Recorder>>,
}

/// A paste whose text is on its way to the PC's clipboard. Its chord, and
/// everything typed or clicked after it, wait for the text; a key let go
/// before the chord would otherwise run a command on the old line (⌘V
/// then Return, 300 ms away from a PC in Korea).
struct PendingPaste {
    id: u64,
    until: Instant,
    queued: Vec<Queued>,
}

/// Input held back behind a paste.
enum Queued {
    /// An event, with where it was going when it arrived.
    Event(Event, At),
    /// The modifiers held at the end of a frame: egui has no events for
    /// Shift, Ctrl, Option or ⌘ on their own.
    Modifiers(Modifiers),
    /// A key pressed and let go, for one egui never reports (Caps Lock).
    Tap(i16),
}

/// Where an event was going when it arrived.
#[derive(Debug, Clone, Copy)]
struct At {
    keys_to_pc: bool,
    over_stream: bool,
}

/// What this frame's input is sent through, and what it needs to know.
struct Link<'a, S: Sink + ?Sized> {
    ctx: &'a egui::Context,
    live: &'a Live,
    cfg: &'a ClientConfig,
    out: &'a S,
    connected: bool,
    video: Rect,
    ppp: f32,
}

impl Default for View {
    fn default() -> Self {
        Self {
            captured: false,
            grabbed: false,
            bar_shown: false,
            stats: false,
            details: false,
            presented: Presented::default(),
            chord_before: None,
            settings: None,
            held: Held::default(),
            scroll: (0.0, 0.0),
            motion: (0.0, 0.0),
            bar_h: size::TOOLBAR,
            confirm: None,
            confirm_install: false,
            confirm_disconnect: None,
            volume: 100,
            muted: false,
            composing: false,
            interrupted: None,
            poor: false,
            poor_hinted: false,
            toasts: Vec::new(),
            hint_since: None,
            capture_chord_held: false,
            pending: None,
            moved_to: None,
            pasted_since_v_up: false,
            cmd_since_v_up: false,
            host_mac: false,
            cursor_texture: None,
            host_cursor_drawn: false,
            caps: None,
            #[cfg(test)]
            caps_now: None,
            #[cfg(test)]
            problem: None,
            #[cfg(test)]
            fake_stats: None,
            #[cfg(test)]
            tap: None,
        }
    }
}

impl View {
    pub fn set_poor(&mut self, poor: bool) {
        if poor && !self.poor && !self.poor_hinted {
            self.poor_hinted = true;
            let (_, keys) = host_key();
            self.toast(
                Tone::Warning,
                format!("Video is arriving unevenly. {keys} opens the toolbar: Stats shows loss and decode time, and Stream settings can lower the bitrate."),
            );
        }
        self.poor = poor;
    }

    /// Show a short line under the toolbar for a few seconds.
    pub fn toast(&mut self, tone: Tone, text: impl Into<String>) {
        let text = text.into();
        if self.toasts.iter().any(|t| t.text == text) {
            return;
        }
        self.toasts.push(Toast {
            tone,
            text,
            at: Instant::now(),
        });
    }

    /// Release everything and let the cursor go; called when the stream
    /// ends or the window loses focus.
    pub fn reset(&mut self, ctx: &egui::Context, live: Option<&Live>) {
        self.reset_with(ctx, live, live.map(|l| &l.input));
    }

    fn reset_with<S: Sink + ?Sized>(
        &mut self,
        ctx: &egui::Context,
        live: Option<&Live>,
        out: Option<&S>,
    ) {
        // A paste still waiting goes, with what was held back behind it:
        // none of it has reached the PC, and pasting the PC's old clipboard
        // after the user has gone would be worse than nothing.
        if let Some(p) = self.pending.take() {
            if let Some(l) = live {
                l.clipboard.give_up(p.id);
            }
        }
        self.moved_to = None;
        self.pasted_since_v_up = false;
        self.cmd_since_v_up = false;
        self.composing = false;
        if let Some(out) = out {
            self.held.release_all(out);
        }
        crate::macos::stream_owns_keys(false);
        ctx.options_mut(|o| o.zoom_with_keyboard = true);
        self.set_captured(ctx, false);
        self.bar_shown = false;
        self.confirm = None;
        self.settings = None;
        self.confirm_install = false;
        self.confirm_disconnect = None;
        self.interrupted = None;
        self.poor = false;
        self.poor_hinted = false;
        self.toasts.clear();
        self.hint_since = None;
        self.capture_chord_held = false;
        if let Some(l) = live {
            if l.session.cursor_supported() && !self.host_cursor_drawn {
                self.host_cursor_drawn = true;
                l.session.draw_cursor(true);
            }
        }
    }

    fn set_captured(&mut self, ctx: &egui::Context, on: bool) {
        self.captured = on;
        if self.grabbed != on {
            self.grabbed = on;
            let grab = if on {
                if cfg!(target_os = "macos") {
                    egui::CursorGrab::Locked
                } else {
                    egui::CursorGrab::Confined
                }
            } else {
                egui::CursorGrab::None
            };
            if on {
                // The pointer is held where it is. Off the window (the
                // stream connected with it elsewhere) it would stay in
                // sight and its clicks reach another app: it goes to the
                // middle of the picture first.
                let inside = ctx.input(|i| {
                    i.pointer
                        .latest_pos()
                        .is_some_and(|p| i.screen_rect().contains(p))
                });
                if !inside {
                    let middle = ctx.screen_rect().center();
                    ctx.send_viewport_cmd(ViewportCommand::CursorPosition(middle));
                }
            }
            ctx.send_viewport_cmd(ViewportCommand::CursorGrab(grab));
            // A free pointer is hidden through `set_cursor_icon(None)`,
            // every frame it is over the picture. A `CursorVisible(true)`
            // here would show it behind egui's back: egui only re-applies
            // an icon when it changes, so the Mac cursor would stay on top
            // of the PC's until the pointer left the window — two pointers.
            // A captured one is hidden by AppKit as well: egui's icon is
            // only a cursor rectangle over the window, which macOS applies
            // when the pointer moves into it, and a held pointer never
            // moves — the Mac's arrow stayed in the picture beside the
            // PC's, or outside it when the window was not under it.
            crate::macos::hide_pointer(on);
        }
    }

    pub fn handle_interruption_at(&mut self, now: Instant) {
        match &mut self.interrupted {
            Some(Interruption::Reconnecting(state)) => {
                if let Some(next) = state.next_attempt(now) {
                    *state = next;
                } else {
                    self.interrupted = Some(Interruption::Failed);
                }
            }
            Some(Interruption::Failed)
            | Some(Interruption::TakenOver(_))
            | Some(Interruption::HostSleeping) => {}
            None => {
                self.interrupted = Some(Interruption::reconnecting(now));
            }
        }
    }

    pub fn handle_interruption(&mut self) {
        self.handle_interruption_at(Instant::now());
    }

    pub fn cancel_reconnect(&mut self) {
        self.interrupted = None;
    }

    pub fn show(&mut self, ctx: &egui::Context, env: &Env<'_>) -> Vec<Action> {
        let live = env.live;
        let mut actions = Vec::new();
        self.host_mac = is_mac_os(&env.os);
        let screen = ctx.screen_rect();
        let stats = live.session.stats();
        #[cfg(test)]
        let stats = self.fake_stats.clone().unwrap_or(stats);
        let (vw, vh) = if stats.width > 0 {
            (stats.width as f32, stats.height as f32)
        } else {
            (live.requested.0 as f32, live.requested.1 as f32)
        };

        // The picture fills the window, in its own proportions; the
        // toolbar, when the host key has asked for it, lies over the top of
        // it. A menu or a question keeps it there until it is answered.
        let video = fit(screen, vw / vh);
        let popup = ctx.memory(|m| m.any_popup_open());
        let bar_visible = self.bar_shown || popup || self.confirm.is_some() || self.confirm_install;
        if !bar_visible {
            self.confirm_disconnect = None;
        }
        let mut bar_rect = bar_visible
            .then(|| Rect::from_min_size(screen.min, Vec2::new(screen.width(), self.bar_h)));
        let waiting = live.frames.seq() == 0;
        let interruption = self.interrupted.clone();
        self.presented.frame(
            Instant::now(),
            live.frames.seq(),
            live.session.published_at(),
        );

        egui::CentralPanel::default()
            .frame(Frame::new().fill(Color32::BLACK))
            .show(ctx, |ui| {
                if !waiting {
                    ui.painter().add(egui::Shape::Callback(
                        egui_wgpu::Callback::new_paint_callback(
                            video,
                            video::Paint {
                                // Native video sits above egui's surface. Use
                                // the SDR tone map while controls cover it.
                                allow_native: !(bar_visible
                                    || self.stats
                                    || self.settings.is_some()
                                    || self.confirm_disconnect.is_some()
                                    || interruption.is_some()
                                    || !self.toasts.is_empty()
                                    || self.hint_since.is_some()),
                                rect: video,
                                frames: live.frames.clone(),
                            },
                        ),
                    ));
                }
            });
        if let Some(intr) = interruption.as_ref() {
            self.interruption_overlay(ctx, screen, env, intr, &mut actions);
        } else if waiting {
            self.waiting(ctx, screen, env, &mut actions);
        }

        if let Some(rect) = bar_rect {
            let measured = self.toolbar(ctx, rect, env, &mut actions);
            self.bar_h = measured.height().max(size::TOOLBAR);
            bar_rect = Some(Rect::from_min_size(
                screen.min,
                Vec2::new(screen.width(), self.bar_h),
            ));
        }
        #[cfg(test)]
        let stats = latch_stream::Stats {
            video_problem: stats.video_problem.clone().or(self.problem.clone()),
            ..stats
        };
        self.toasts(
            ctx,
            screen,
            bar_rect,
            env,
            stats.video_problem.as_deref(),
            &mut actions,
        );

        // Panels keep clear of the toolbar while it shows.
        let below_bar = bar_rect
            .map(|b| Rect::from_min_max(Pos2::new(screen.left(), b.bottom()), screen.max))
            .unwrap_or(screen);
        self.settings_panel(ctx, below_bar, &mut actions);
        if self.stats && !waiting {
            self.stats_overlay(ctx, video, below_bar.top(), env, &stats);
        }
        if self.details {
            self.diagnostics(ctx, env, below_bar, &stats);
        }
        if interruption.is_none() {
            self.input(ctx, live, env.cfg, video, bar_rect);
        } else {
            self.set_captured(ctx, false);
            self.held.release_all(&live.input);
        }
        actions
    }

    /// Before the first picture: what is happening, and a way out that does
    /// not need the host key.
    fn waiting(
        &mut self,
        ctx: &egui::Context,
        screen: Rect,
        env: &Env<'_>,
        actions: &mut Vec<Action>,
    ) {
        let live = env.live;
        let elapsed = live.started.elapsed();
        if elapsed < Duration::from_secs(10) {
            ctx.request_repaint_after(Duration::from_secs(10) - elapsed);
        }
        let w = overlay_width(440.0, screen.width());
        egui::Area::new(Id::new("stream-waiting"))
            .order(egui::Order::Middle)
            .anchor(Align2::CENTER_CENTER, Vec2::ZERO)
            .show(ctx, |ui| {
                ui.set_width(w);
                ui.vertical_centered(|ui| {
                    ui.spacing_mut().item_spacing.y = space::SM;
                    ui.add(ui::spinner(20.0, P.accent));
                    ui.add_space(space::XS);
                    ui.label(
                        RichText::new(if live.session.connected() {
                            format!("Waiting for the first picture from {}", live.pc)
                        } else {
                            format!("Connecting to {}", live.pc)
                        })
                        .font(theme::medium(theme::text::BODY + 1.0))
                        .color(P.text),
                    );
                    let path = env.path.as_ref().unwrap_or(&live.path);
                    ui.label(
                        RichText::new(format!("{} · {}", path.label(), live.settings.describe()))
                            .font(theme::mono(theme::text::MONO - 0.5))
                            .color(P.text_tertiary),
                    );
                    if elapsed >= Duration::from_secs(10) {
                        ui.add_space(space::SM);
                        ui.label(
                            RichText::new("Connecting is taking longer than expected.")
                                .font(theme::medium(theme::text::BODY))
                                .color(P.warning),
                        );
                        ui.label(
                            RichText::new(format!(
                                "Check that {} is awake and signed in to Tailscale.",
                                live.pc
                            ))
                            .font(theme::regular(theme::text::BODY - 1.0))
                            .color(P.text_secondary),
                        );
                        if path.relayed() {
                            ui.label(
                                RichText::new(
                                    "The connection is going through a Tailscale relay, which can be slow.",
                                )
                                .font(theme::regular(theme::text::BODY - 1.0))
                                .color(P.text_secondary),
                            );
                        }
                    }
                    ui.add_space(space::SM);
                    if ui::secondary_button(ui, "Cancel").clicked() {
                        actions.push(Action::Disconnect);
                    }
                });
            });
    }

    fn interruption_overlay(
        &mut self,
        ctx: &egui::Context,
        screen: Rect,
        env: &Env<'_>,
        interruption: &Interruption,
        actions: &mut Vec<Action>,
    ) {
        let live = env.live;
        let w = overlay_width(440.0, screen.width());
        egui::Area::new(Id::new("stream-interruption"))
            .order(egui::Order::Foreground)
            .anchor(Align2::CENTER_CENTER, Vec2::ZERO)
            .show(ctx, |ui| {
                ui::overlay_frame().show(ui, |ui| {
                    ui.set_width(w);
                    ui.vertical_centered(|ui| {
                        ui.spacing_mut().item_spacing.y = space::SM;
                        match interruption {
                            Interruption::Reconnecting(state) => {
                                let now = Instant::now();
                                let remaining_secs = state.remaining_secs(now);
                                ui.label(
                                    RichText::new("Connection lost")
                                        .font(theme::semibold(theme::text::TITLE))
                                        .color(P.text),
                                );
                                if remaining_secs > 0 && !state.in_flight {
                                    ui::muted(
                                        ui,
                                        format!(
                                            "Reconnecting in {remaining_secs} s · attempt {} of 3",
                                            state.attempt
                                        ),
                                    );
                                    let rem = state.remaining(now);
                                    let subsecond = rem.subsec_nanos();
                                    let next_tick = if subsecond == 0 {
                                        Duration::from_secs(1)
                                    } else {
                                        Duration::from_nanos(subsecond as u64)
                                    };
                                    ctx.request_repaint_after(next_tick);
                                } else {
                                    if !state.in_flight {
                                        if let Some(Interruption::Reconnecting(ref mut s)) =
                                            self.interrupted
                                        {
                                            s.in_flight = true;
                                        }
                                        actions.push(Action::Reconnect);
                                    }
                                    ui::muted(
                                        ui,
                                        format!("Attempting to restore stream to {}…", live.pc),
                                    );
                                }
                                ui.add_space(space::XS);
                                ui.horizontal(|ui| {
                                    if ui
                                        .add_enabled_ui(!state.in_flight, |ui| {
                                            ui::primary_button(ui, "Reconnect now")
                                        })
                                        .inner
                                        .clicked()
                                    {
                                        if let Some(Interruption::Reconnecting(ref mut s)) =
                                            self.interrupted
                                        {
                                            s.in_flight = true;
                                        }
                                        actions.push(Action::Reconnect);
                                    }
                                    if ui::ghost_button(ui, "Back to machines").clicked() {
                                        self.cancel_reconnect();
                                        actions.push(Action::Disconnect);
                                    }
                                });
                            }
                            Interruption::Failed => {
                                ui.label(
                                    RichText::new(format!("Couldn't reconnect to {}", live.pc))
                                        .font(theme::semibold(theme::text::TITLE))
                                        .color(P.text),
                                );
                                ui.add_space(space::XS);
                                ui.horizontal(|ui| {
                                    if ui::primary_button(ui, "Try again").clicked() {
                                        self.interrupted =
                                            Some(Interruption::Reconnecting(ReconnectState {
                                                attempt: 1,
                                                deadline: Instant::now(),
                                                in_flight: true,
                                            }));
                                        actions.push(Action::Reconnect);
                                    }
                                    if ui::ghost_button(ui, "Back to machines").clicked() {
                                        self.cancel_reconnect();
                                        actions.push(Action::Disconnect);
                                    }
                                });
                            }
                            Interruption::TakenOver(reason) => {
                                ui.label(
                                    RichText::new("Another device took over")
                                        .font(theme::semibold(theme::text::TITLE))
                                        .color(P.text),
                                );
                                ui::muted(ui, reason);
                                ui.add_space(space::XS);
                                ui.horizontal(|ui| {
                                    if ui::primary_button(ui, "Reconnect now").clicked() {
                                        actions.push(Action::Reconnect);
                                    }
                                    if ui::ghost_button(ui, "Back to machines").clicked() {
                                        self.cancel_reconnect();
                                        actions.push(Action::Disconnect);
                                    }
                                });
                            }
                            Interruption::HostSleeping => {
                                ui.label(
                                    RichText::new(format!("{} is going to sleep", live.pc))
                                        .font(theme::semibold(theme::text::TITLE))
                                        .color(P.text),
                                );
                                ui::muted(ui, "The remote machine was put to sleep.");
                                ui.add_space(space::XS);
                                ui.horizontal(|ui| {
                                    if ui::ghost_button(ui, "Back to machines").clicked() {
                                        self.cancel_reconnect();
                                        actions.push(Action::Disconnect);
                                    }
                                });
                            }
                        }
                    });
                });
            });
    }

    fn settings_panel(&mut self, ctx: &egui::Context, region: Rect, actions: &mut Vec<Action>) {
        let Some(settings) = self.settings.as_mut() else {
            return;
        };
        let mut apply = false;
        let mut close = false;
        let w = overlay_width(520.0, region.width()) - 2.0 * space::LG;
        egui::Window::new("Stream settings")
            .constrain_to(region)
            .title_bar(false)
            .frame(ui::panel_frame())
            .collapsible(false)
            .resizable(false)
            .default_width(w)
            .max_width(w)
            .anchor(Align2::CENTER_CENTER, Vec2::ZERO)
            .show(ctx, |ui| {
                ui.set_width(w);
                close = ui::panel_header(ui, "Stream settings");
                ui::clipped_scroll(
                    ui,
                    "stream-settings",
                    (region.height() - 190.0).max(120.0),
                    |ui| {
                        ui.spacing_mut().item_spacing.y = 0.0;
                        crate::settings::stream_controls(
                            ui,
                            settings,
                            crate::app::ClientApp::native_pixels(ctx),
                            crate::app::ClientApp::display_hz(),
                        );
                    },
                );
                ui.add_space(space::MD);
                ui.horizontal_wrapped(|ui| {
                    ui.spacing_mut().item_spacing.x = space::SM;
                    apply = ui::primary_button(ui, "Apply and reconnect").clicked();
                    if ui::ghost_button(ui, "Cancel").clicked() {
                        close = true;
                    }
                });
                ui::small_print(
                    ui,
                    "Applying restarts the stream with these settings, in a second or two.",
                );
            });
        if ctx.input(|i| i.key_pressed(egui::Key::Escape)) && !ctx.memory(|m| m.any_popup_open()) {
            close = true;
        }
        if apply {
            actions.push(Action::ApplySettings(settings.clone()));
        }
        if close || apply {
            self.settings = None;
        }
    }

    fn diagnostics(
        &mut self,
        ctx: &egui::Context,
        env: &Env<'_>,
        region: Rect,
        stats: &latch_stream::Stats,
    ) {
        let live = env.live;
        let mut close = false;
        egui::Window::new("Stream performance")
            .constrain_to(region)
            .title_bar(false)
            .frame(ui::panel_frame())
            .resizable(false)
            .collapsible(false)
            .default_pos(Pos2::new(16.0, self.bar_h + 16.0))
            .default_width(340.0)
            .max_width(340.0)
            .show(ctx, |ui| {
                ui.set_width(340.0);
                close = ui::panel_header(ui, "Stream performance");
                // Everything under the title scrolls when the window is too
                // short for it, so the panel always ends inside the window.
                let room = ui::room_below(ui, space::LG + 1.0);
                ui::clipped_scroll(ui, "stream-performance", room, |ui| {
                    let path = env.path.as_ref().unwrap_or(&live.path);
                    ui.horizontal_wrapped(|ui| {
                        ui::tag(ui, Some(path.tone()), &path.label());
                        ui::tag(ui, None, &profile_label(&live.settings));
                    });
                    ui.add_space(space::SM);
                    let rows = [
                        Kv::new("Resolution", format!("{} × {}", stats.width, stats.height)),
                        Kv::new(
                            "Requested resolution",
                            format!("{} × {}", live.requested.0, live.requested.1),
                        ),
                        Kv::new(
                            "Frame rate",
                            format!("{:.1} of {} fps", stats.fps, live.requested.2),
                        ),
                        Kv::new(
                            "Bitrate",
                            format!(
                                "{:.1} of {} Mbps",
                                stats.mbps,
                                live.settings.bitrate_kbps / 1000
                            ),
                        ),
                        Kv::new(
                            "Received",
                            format!("{:.1} Mbps with FEC and audio", stats.received_mbps),
                        ),
                        Kv::new(
                            "Round trip",
                            format!("{} ± {} ms", stats.rtt_ms, stats.rtt_var_ms),
                        ),
                        Kv::new("Packet loss", format!("{:.2}%", stats.loss_pct)),
                        Kv::new("Encode on host", format!("{:.2} ms", stats.host_ms)),
                        Kv::new("Frame assembly", format!("{:.2} ms", stats.assembly_ms)),
                        Kv::new("Decoder queue", format!("{:.2} ms", stats.queue_ms)),
                        Kv::new("Decode", format!("{:.2} ms", stats.decode_ms)),
                        Kv::new("Codec", stats.codec.to_string()),
                        Kv::new("Encoder on host", stats.encoder.clone()),
                        Kv::new("Decoder here", stats.decoder.to_string()),
                    ]
                    .map(Kv::mono);
                    ui::kv_grid(ui, &rows);
                    ui.add_space(space::SM);
                    ui::small_print(ui, "The encoder aims at the bitrate while the picture changes; a still screen needs less. Received bitrate falling well short during motion, or swinging, means packets are being lost on the way.");
                    if !stats.audio.is_empty() {
                        ui::small_print(ui, &stats.audio);
                    }
                    ui.add_space(space::XS);
                    if ui::secondary_button(ui, "Copy diagnostics").clicked() {
                        ctx.copy_text(format!("Latch {}\n{}\nRequested: {} × {}, {} fps, {} Mbps\n{stats:#?}",
                            env!("CARGO_PKG_VERSION"), path.label(), live.requested.0, live.requested.1,
                            live.requested.2, live.settings.bitrate_kbps / 1000));
                    }
                });
            });
        if close {
            self.details = false;
        }
    }

    /// The stats overlay, GeForce NOW style: what the picture is and how
    /// it is getting here, in the picture's top right corner (under the
    /// toolbar while it shows). Painted, never a window, so every click
    /// still goes to the PC.
    fn stats_overlay(
        &self,
        ctx: &egui::Context,
        video: Rect,
        top: f32,
        env: &Env<'_>,
        stats: &latch_stream::Stats,
    ) {
        let rows = overlay_rows(env.live, stats, &self.presented);
        let painter = ctx.layer_painter(egui::LayerId::new(
            egui::Order::Middle,
            Id::new("stats-overlay"),
        ));
        let font = theme::mono(theme::text::MONO - 1.0);
        let lay =
            |text: &str, colour| painter.layout_no_wrap(text.to_string(), font.clone(), colour);
        let laid: Vec<_> = rows
            .iter()
            .map(|r| {
                let colour = if r.warn { P.warning } else { P.text };
                (lay(r.label, P.text_tertiary), lay(&r.value, colour))
            })
            .collect();
        let label_w = laid.iter().map(|(l, _)| l.size().x).fold(0.0, f32::max);
        let value_w = laid.iter().map(|(_, v)| v.size().x).fold(0.0, f32::max);
        let row_h = laid.first().map_or(14.0, |(l, _)| l.size().y) + 2.0;
        let pad = Vec2::new(space::MD, space::SM);
        let size = Vec2::new(label_w + space::MD + value_w, row_h * laid.len() as f32) + 2.0 * pad;
        let corner = Pos2::new(
            video.right().min(ctx.screen_rect().right()) - space::MD,
            video.top().max(top) + space::MD,
        );
        let rect = Rect::from_min_size(Pos2::new(corner.x - size.x, corner.y), size);
        painter.rect(
            rect,
            theme::radius::MD,
            P.overlay,
            egui::Stroke::new(1.0_f32, P.hairline),
            egui::StrokeKind::Inside,
        );
        let mut y = rect.top() + pad.y;
        for (label, value) in laid {
            painter.galley(Pos2::new(rect.left() + pad.x, y), label, P.text_tertiary);
            let x = rect.right() - pad.x - value.size().x;
            painter.galley(Pos2::new(x, y), value, P.text);
            y += row_h;
        }
    }

    fn toolbar(
        &mut self,
        ctx: &egui::Context,
        rect: Rect,
        env: &Env<'_>,
        actions: &mut Vec<Action>,
    ) -> Rect {
        let live = env.live;
        let cfg = env.cfg;
        let overflow = rect.width() < OVERFLOW_BELOW;
        let facts = rect.width() >= FACTS_BELOW;
        let path = env.path.clone().unwrap_or_else(|| live.path.clone());
        let bar = egui::Area::new(Id::new("stream-toolbar"))
            .fixed_pos(rect.min)
            .order(egui::Order::Foreground)
            .interactable(true)
            .show(ctx, |ui| {
                ui.set_min_width(rect.width());
                ui.set_max_width(rect.width());
                Frame::new()
                    .fill(P.overlay)
                    .inner_margin(Margin::symmetric(space::MD as i8, 0))
                    .show(ui, |ui| {
                        ui.set_min_height(size::TOOLBAR);
                        ui.spacing_mut().interact_size.y = size::CONTROL_SM;
                        ui.horizontal_centered(|ui| {
                            ui.spacing_mut().item_spacing.x = space::SM;
                            let s = live.session.stats();
                            let tone = if !live.session.connected() {
                                Tone::Accent
                            } else if self.poor || s.video_problem.is_some() {
                                Tone::Warning
                            } else {
                                Tone::Success
                            };
                            ui::status_dot(ui, tone);
                            ui.add(
                                egui::Label::new(
                                    RichText::new(&live.pc)
                                        .font(theme::medium(theme::text::BODY))
                                        .color(P.text),
                                )
                                .truncate(),
                            );
                            if facts {
                                let (w, h) = if s.width > 0 {
                                    (s.width, s.height)
                                } else {
                                    (live.requested.0, live.requested.1)
                                };
                                let codec = if !live.codec.is_empty() {
                                    live.codec
                                } else if !s.codec.is_empty() {
                                    s.codec
                                } else {
                                    "Connecting…"
                                };
                                ui.label(
                                    RichText::new(format!(
                                        "{w}×{h} · {} fps · {codec}",
                                        live.requested.2
                                    ))
                                    .font(theme::mono(theme::text::MONO - 0.5))
                                    .color(P.text_tertiary),
                                );
                            }
                            if !overflow {
                                let tag = ui::tag(ui, Some(path.tone()), &path.label());
                                if path.relayed() {
                                    tag.on_hover_text("The route in use right now. A relay adds delay; Latch never lowers the bitrate for it.");
                                } else if path.direct == Some(true) {
                                    tag.on_hover_text("Packets go straight to the machine.");
                                }
                            }
                            if rect.width() >= HINT_BELOW {
                                ui.add_space(space::XS);
                                let (keys, _) = host_key();
                                ui::shortcut(ui, keys, "hides this bar");
                            }
                            ui::trailing(ui, |ui| {
                                ui.spacing_mut().item_spacing.x = space::XS;
                                if overflow {
                                    self.more_menu(ui, ctx, env, actions);
                                } else {
                                    self.mouse_menu(ui, ctx, live, cfg, actions);
                                    self.keys_menu(ui, live, cfg, actions);
                                    self.stats_button(ui);
                                    self.fullscreen_button(ui, env, actions);
                                    self.power_menu(ui, env);
                                    if rect.width() >= 1100.0 {
                                        self.volume_controls(ui, live);
                                    } else {
                                        ui::menu_button(ui, "Audio", |ui| {
                                            self.volume_controls(ui, live);
                                        });
                                    }
                                    pads_tag(ui, live);
                                }
                                if ui::ghost_button(ui, "Stream settings").clicked() {
                                    self.held.release_all(&live.input);
                                    self.set_captured(ctx, false);
                                    self.settings = Some(cfg.stream.clone());
                                    self.confirm_disconnect = None;
                                }
                                toolbar_rule(ui);
                                let armed = self
                                    .confirm_disconnect
                                    .filter(|&t| t.elapsed() < Duration::from_secs(3));
                                if self.confirm_disconnect.is_some() && armed.is_none() {
                                    self.confirm_disconnect = None;
                                }
                                let disc_label = if armed.is_some() {
                                    "Click again to disconnect"
                                } else {
                                    "Disconnect"
                                };
                                if let Some(t) = armed {
                                    let rem = Duration::from_secs(3).saturating_sub(t.elapsed());
                                    ctx.request_repaint_after(rem);
                                }
                                if ui::danger_button(ui, disc_label).clicked() {
                                    if armed.is_some() {
                                        actions.push(Action::Disconnect);
                                        self.confirm_disconnect = None;
                                    } else {
                                        self.confirm_disconnect = Some(Instant::now());
                                    }
                                }
                            });
                        });
                    });
                let r = ui.min_rect();
                ui.painter().hline(
                    r.x_range(),
                    r.bottom() - 0.5,
                    theme::stroke(1.0, Color32::from_white_alpha(28)),
                );
            });

        if let Some(action) = self.confirm {
            let w = overlay_width(420.0, rect.width());
            question(ctx, "stream-confirm", rect, w, |ui| {
                ui.label(
                    RichText::new(format!("{} {}?", action.label(), live.pc))
                        .font(theme::semibold(theme::text::TITLE))
                        .color(P.text),
                );
                ui::muted(ui, power_question(action, &live.pc));
                ui.add_space(space::XS);
                ui.horizontal(|ui| {
                    if ui::destructive_button(ui, action.label()).clicked() {
                        actions.push(Action::Power(action));
                        self.confirm = None;
                    }
                    if ui::ghost_button(ui, "Cancel").clicked() {
                        self.confirm = None;
                    }
                });
            });
        }
        if self.confirm_install {
            let (running, latest) = env
                .old_host
                .clone()
                .map(|(r, l)| (r.to_string(), l.map(|v| v.to_string())))
                .unwrap_or_default();
            let w = overlay_width(480.0, rect.width());
            question(ctx, "stream-install", rect, w, |ui| {
                ui.label(
                    RichText::new(format!(
                        "Install Latch Host {} on {}?",
                        latest.as_deref().unwrap_or("(latest)"),
                        live.pc
                    ))
                    .font(theme::semibold(theme::text::TITLE))
                    .color(P.text),
                );
                ui::muted(
                    ui,
                    format!(
                        "{} runs {running}, which can't take updates over the network. Latch presses Win+R on it, types one line that fetches the new version from this machine over Tailscale, and presses Enter. A PowerShell window opens there, swaps the file and restarts the service; the stream stays up.",
                        live.pc
                    ),
                );
                ui::small_print(ui, "Its desktop must be unlocked and in front.");
                ui.add_space(space::XS);
                ui.horizontal(|ui| {
                    if ui::primary_button(ui, "Install through the stream").clicked() {
                        actions.push(Action::InstallHost);
                        self.confirm_install = false;
                    }
                    if ui::ghost_button(ui, "Cancel").clicked() {
                        self.confirm_install = false;
                    }
                });
            });
        }
        bar.response.rect
    }

    /// The narrow toolbar's one menu: everything but Stream settings and
    /// Disconnect, in labelled groups, scrolling if the window is short.
    fn more_menu(
        &mut self,
        ui: &mut egui::Ui,
        ctx: &egui::Context,
        env: &Env<'_>,
        actions: &mut Vec<Action>,
    ) {
        let live = env.live;
        let cfg = env.cfg;
        ui::menu_button(ui, "More", |ui| {
            ui::menu_label(ui, "View");
            if ui::menu_choice(ui, "Full screen", env.fullscreen).clicked() {
                actions.push(Action::Fullscreen(!env.fullscreen));
            }
            if ui::menu_choice(ui, "Stats overlay", self.stats).clicked() {
                self.stats = !self.stats;
            }
            if ui::menu_choice(ui, "Stream performance", self.details).clicked() {
                self.details = !self.details;
            }
            ui::menu_label(ui, "Audio");
            self.volume_controls(ui, live);
            ui::menu_label(ui, "Mouse");
            self.mouse_items(ui, ctx, live, cfg, actions);
            ui::menu_label(ui, "Send keys");
            self.key_items(ui, live, cfg, actions);
            if has_power_items(env) {
                ui::menu_label(ui, machine_noun(&env.os));
                self.power_items(ui, env);
            }
        });
    }

    fn volume_controls(&mut self, ui: &mut egui::Ui, live: &Live) {
        let mute_label = if self.muted { "Unmute" } else { "Mute" };
        if ui::ghost_button(ui, mute_label).clicked() {
            self.muted = !self.muted;
            let gain = if self.muted {
                0.0
            } else {
                self.volume as f32 / 100.0
            };
            live.session.set_volume(gain);
        }
        let mut vol = self.volume;
        if ui::slider(ui, &mut vol, 0..=100, 60.0, "Volume").changed() {
            self.volume = vol;
            if self.muted && vol > 0 {
                self.muted = false;
            }
            let gain = if self.muted {
                0.0
            } else {
                self.volume as f32 / 100.0
            };
            live.session.set_volume(gain);
        }
    }

    fn fullscreen_button(&self, ui: &mut egui::Ui, env: &Env<'_>, actions: &mut Vec<Action>) {
        if ui::ghost_button(
            ui,
            if env.fullscreen {
                "Exit full screen"
            } else {
                "Full screen"
            },
        )
        .clicked()
        {
            actions.push(Action::Fullscreen(!env.fullscreen));
        }
    }

    fn stats_button(&mut self, ui: &mut egui::Ui) {
        let (_, keys) = host_key();
        if ui::ghost_button(ui, if self.stats { "Hide stats" } else { "Stats" })
            .on_hover_text(format!("Frame rate, bitrate, round trip, loss and latency over the picture ({keys}-S). More has the full Stream performance panel."))
            .clicked()
        {
            self.stats = !self.stats;
        }
    }

    fn power_menu(&mut self, ui: &mut egui::Ui, env: &Env<'_>) {
        if !has_power_items(env) {
            return;
        }
        ui::menu_button(ui, machine_noun(&env.os), |ui| self.power_items(ui, env));
    }

    /// Sleep, Restart and Shut down where the machine's owner allows them,
    /// each asked about first: a slip in this menu must not end the stream
    /// or put to sleep a machine that nothing here can wake. The stream
    /// stays up until the machine has agreed.
    fn power_items(&mut self, ui: &mut egui::Ui, env: &Env<'_>) {
        if env.power_allowed {
            if env.can_sleep && ui::menu_item(ui, "Sleep…").clicked() {
                self.confirm = Some(PowerAction::Sleep);
            }
            if ui::menu_item(ui, "Restart…").clicked() {
                self.confirm = Some(PowerAction::Restart);
            }
            if ui::menu_item(ui, "Shut down…").clicked() {
                self.confirm = Some(PowerAction::Shutdown);
            }
        }
        if let Some((running, _)) = &env.old_host {
            if env.power_allowed {
                ui::menu_separator(ui);
            }
            let label = format!("Update Latch Host… (runs {running})");
            if ui::menu_item_enabled(ui, env.handover.is_none(), &label)
                .on_hover_text("Installs the newest Latch Host through this stream, so later updates arrive by themselves.")
                .clicked()
            {
                self.confirm_install = true;
            }
        }
    }

    fn keys_menu(
        &mut self,
        ui: &mut egui::Ui,
        live: &Live,
        cfg: &ClientConfig,
        actions: &mut Vec<Action>,
    ) {
        ui::menu_button(ui, "Keys", |ui| {
            ui::menu_label(ui, "Send to the other machine");
            self.key_items(ui, live, cfg, actions);
            let (_, keys) = host_key();
            ui::menu_note(
                ui,
                &format!("Everything else you type goes there as pressed. {keys} shows or hides this bar."),
            );
            if live.clipboard.unsupported() {
                ui::menu_note(
                    ui,
                    "Clipboard sync needs Latch Host 3.1 or later there (PC menu → Update Latch Host).",
                );
            } else {
                ui::menu_note(
                    ui,
                    "Copy there and paste here, or the other way round: the clipboard follows you.",
                );
            }
        });
    }

    fn key_items(
        &mut self,
        ui: &mut egui::Ui,
        live: &Live,
        cfg: &ClientConfig,
        actions: &mut Vec<Action>,
    ) {
        let input = &live.input;
        for (label, keys) in [
            (
                "Ctrl+Alt+Delete",
                &[input::VK_CONTROL, input::VK_MENU, input::VK_DELETE][..],
            ),
            ("Windows key", &[input::VK_LWIN][..]),
            ("Alt+Tab", &[input::VK_MENU, input::VK_TAB][..]),
            ("Escape", &[input::VK_ESCAPE][..]),
            ("Print Screen", &[input::VK_SNAPSHOT][..]),
        ] {
            if ui::menu_item(ui, label).clicked() {
                self.held.chord(input, keys);
            }
        }
        if cfg!(target_os = "macos") {
            ui::menu_separator(ui);
            if self.host_mac {
                ui::menu_note(ui, "Command is Command on another Mac.");
            } else if ui::menu_choice(ui, "Command acts as Ctrl", cfg.cmd_is_ctrl)
                .on_hover_text("On: Command-C, V and Z copy, paste and undo there. Off: Command is the Windows key. A Mac there always gets Command.")
                .clicked()
            {
                actions.push(Action::ToggleCmd);
            }
        }
    }

    fn mouse_menu(
        &mut self,
        ui: &mut egui::Ui,
        ctx: &egui::Context,
        live: &Live,
        cfg: &ClientConfig,
        actions: &mut Vec<Action>,
    ) {
        ui::menu_button(ui, "Mouse", |ui| {
            self.mouse_items(ui, ctx, live, cfg, actions);
            let (_, keys) = host_key();
            ui::menu_note(
                ui,
                &format!(
                    "Captured: a click on the picture takes the mouse and {keys} gives it back."
                ),
            );
        });
    }

    fn mouse_items(
        &mut self,
        ui: &mut egui::Ui,
        ctx: &egui::Context,
        live: &Live,
        cfg: &ClientConfig,
        actions: &mut Vec<Action>,
    ) {
        if ui::menu_choice(ui, "Captured, for games (raw movement)", cfg.capture_mouse).clicked()
            && !cfg.capture_mouse
        {
            actions.push(Action::MouseCapture(true));
        }
        if ui::menu_choice(
            ui,
            "Free, for desktops (follows this cursor)",
            !cfg.capture_mouse,
        )
        .clicked()
            && cfg.capture_mouse
        {
            actions.push(Action::MouseCapture(false));
            self.release(ctx, &live.input);
        }
    }

    /// Short notices under the toolbar: the host-key hint, clipboard,
    /// connection, an install.
    fn toasts(
        &mut self,
        ctx: &egui::Context,
        screen: Rect,
        bar: Option<Rect>,
        env: &Env<'_>,
        video_problem: Option<&str>,
        actions: &mut Vec<Action>,
    ) {
        self.toasts.retain(|t| t.at.elapsed() < TOAST_FOR);
        let hint = self
            .hint_since
            .is_some_and(|t| t.elapsed() < HINT_FOR && !self.bar_shown);
        if !hint {
            self.hint_since = None;
        }
        let mut lines: Vec<(Tone, String)> = self
            .toasts
            .iter()
            .map(|t| (t.tone, t.text.clone()))
            .collect();
        if let Some(h) = &env.handover {
            lines.push((Tone::Neutral, h.clone()));
        }
        if let Some(n) = env.live.clipboard.note() {
            lines.push((Tone::Neutral, n));
        }
        if lines.is_empty() && video_problem.is_none() && !hint {
            return;
        }
        // Slow and bounded: enough to take a notice down on time.
        ctx.request_repaint_after(Duration::from_millis(500));
        let top = bar.map(|b| b.bottom()).unwrap_or(screen.top()) + space::MD;
        let w = overlay_width(480.0, screen.width());
        egui::Area::new(Id::new("stream-toasts"))
            .fixed_pos(Pos2::new(screen.center().x - w / 2.0 - OVERLAY_PAD, top))
            .order(egui::Order::Foreground)
            .interactable(video_problem.is_some())
            .show(ctx, |ui| {
                ui.set_width(w + 2.0 * OVERLAY_PAD);
                ui.spacing_mut().item_spacing.y = space::SM;
                if hint {
                    ui::overlay_frame().show(ui, |ui| {
                        ui.set_width(w);
                        let (keys, _) = host_key();
                        ui::shortcut(ui, keys, "releases the mouse and shows the toolbar");
                    });
                }
                if let Some(problem) = video_problem {
                    let help = env.video_help.as_ref();
                    // Once the machine has answered, "asking why" is stale.
                    let headline = match help {
                        Some(_) => problem.trim_end_matches("Asking the PC why…").trim_end(),
                        None => problem,
                    };
                    ui::overlay_frame().show(ui, |ui| {
                        ui.set_width(w);
                        ui.spacing_mut().item_spacing.y = space::XS;
                        ui.add(
                            egui::Label::new(
                                RichText::new(headline)
                                    .font(theme::medium(theme::text::BODY))
                                    .color(P.text),
                            )
                            .wrap(),
                        );
                        // The machine's own answer, once it has given one.
                        if let Some(help) = help {
                            ui::muted(ui, &help.message);
                        }
                        ui.add_space(space::XS);
                        ui.horizontal(|ui| {
                            if let Some(h) = help.filter(|h| h.hdr_is_on) {
                                let label = format!("Turn off {} there", h.mode);
                                if h.busy {
                                    ui::empty_state(ui, &format!("Turning {} off…", h.mode), true);
                                } else if ui::primary_button(ui, &label).clicked() {
                                    actions.push(Action::TurnOffHdr);
                                }
                            }
                        });
                    });
                }
                for (tone, text) in lines {
                    ui::overlay_frame().show(ui, |ui| {
                        ui.set_width(w);
                        ui::dot_label(ui, tone, &text);
                    });
                }
            });
    }

    /// Capture the mouse and take the toolbar away: the picture is all.
    fn grab(&mut self, ctx: &egui::Context, out: &(impl Sink + ?Sized)) {
        self.held.release_all(out);
        self.motion = (0.0, 0.0);
        // A captured mouse sends every click to the PC, while egui still
        // sees them at the frozen pointer: a panel, question or menu left
        // open would take them too ("Apply and reconnect", "Restart").
        // Closing them is Cancel.
        self.settings = None;
        self.confirm = None;
        self.confirm_install = false;
        ctx.memory_mut(|m| m.close_popup());
        self.set_captured(ctx, true);
        self.bar_shown = false;
    }

    /// Give the mouse back.
    fn release(&mut self, ctx: &egui::Context, out: &(impl Sink + ?Sized)) {
        self.held.release_all(out);
        self.set_captured(ctx, false);
    }

    /// Ctrl+Alt, the host key. From the picture it frees the mouse and
    /// drops the toolbar; from the toolbar it hides it and, when capture
    /// is on, takes the mouse back.
    fn host_key(&mut self, ctx: &egui::Context, cfg: &ClientConfig, out: &(impl Sink + ?Sized)) {
        self.hint_since = None;
        // Everything goes up first, whichever way this goes: while the
        // chord is down every event is swallowed, a key let go meanwhile
        // included, and it would stay down on the PC.
        self.held.release_all(out);
        if self.captured || !self.bar_shown {
            self.release(ctx, out);
            self.bar_shown = true;
        } else {
            self.bar_shown = false;
            if cfg.capture_mouse {
                self.grab(ctx, out);
            }
        }
    }

    /// The stream has just connected: take the mouse if the window has
    /// focus, and say once how to get it back.
    pub fn stream_started(&mut self, ctx: &egui::Context, live: &Live, capture: bool) {
        // A host that streams cursor shapes starts each session without
        // its pointer in the picture. The last session's reset left this
        // true, and a captured reconnect would then never ask for it.
        self.host_cursor_drawn = false;
        live.session.set_volume(if self.muted {
            0.0
        } else {
            self.volume as f32 / 100.0
        });
        // Capture as soon as the stream connects, whenever the window has
        // focus, without waiting for the pointer to be over the picture. A
        // game that reads raw relative motion (Genshin's camera, say) only
        // gets it while captured; requiring the pointer to already sit on the
        // picture meant a connect with the pointer elsewhere left the game
        // deaf to the mouse until the user knew to click. Ctrl+Alt still frees
        // it. The click-to-capture path stays for re-capturing after a free.
        let focused = ctx.input(|i| i.focused);
        if capture && focused {
            self.grab(ctx, &live.input);
        }
        self.hint_since = Some(Instant::now());
    }

    /// Forward this frame's keyboard and mouse events to the PC.
    fn input(
        &mut self,
        ctx: &egui::Context,
        live: &Live,
        cfg: &ClientConfig,
        video: Rect,
        bar: Option<Rect>,
    ) {
        #[cfg(test)]
        if let Some(tap) = self.tap.clone() {
            let connected = tap.connected.get();
            return self.forward(ctx, live, cfg, video, bar, &*tap, connected);
        }
        self.forward(
            ctx,
            live,
            cfg,
            video,
            bar,
            &live.input,
            live.input.connected(),
        );
    }

    /// [`View::input`], sending through `out`.
    #[allow(clippy::too_many_arguments)]
    fn forward<S: Sink + ?Sized>(
        &mut self,
        ctx: &egui::Context,
        live: &Live,
        cfg: &ClientConfig,
        video: Rect,
        bar: Option<Rect>,
        out: &S,
        connected: bool,
    ) {
        if self.interrupted.is_some() {
            self.held.release_all(out);
            self.set_captured(ctx, false);
            crate::macos::stream_owns_keys(false);
            return;
        }
        let (events, modifiers, pointer, focused) = ctx.input(|i| {
            (
                i.events.clone(),
                i.modifiers,
                i.pointer.latest_pos(),
                i.focused,
            )
        });
        let popup = ctx.memory(|m| m.any_popup_open());
        let overlay = self.confirm.is_some()
            || self.confirm_install
            || self.settings.is_some()
            || self.interrupted.is_some();
        let over_bar = pointer.is_some_and(|p| bar.is_some_and(|b| b.contains(p)));
        let over_overlay = pointer.is_some_and(|p| {
            ctx.layer_id_at(p)
                .is_some_and(|layer| layer.order > egui::Order::Background)
        });
        // The black bars are part of the stream too. Only Latch's controls
        // should take the pointer away from the remote machine.
        let over_stream = pointer_on_stream(
            pointer.is_some_and(|p| ctx.screen_rect().contains(p)),
            over_bar,
            popup,
            overlay,
            over_overlay,
        );
        // While the keys belong to the PC, egui must not hold a focus: Tab
        // would move it to the Stats panel or a notice's button, and every
        // key after would stay in Latch. With the toolbar up, Tab walks
        // the toolbar and its panels as it should; before the first
        // picture it reaches Cancel.
        let keys_belong_to_pc = self.captured || (bar.is_none() && live.frames.seq() > 0);
        if keys_belong_to_pc && !popup && !overlay {
            ctx.memory_mut(|m| {
                if let Some(id) = m.focused() {
                    m.surrender_focus(id);
                }
            });
        }
        let keys_to_pc = focused && !ctx.wants_keyboard_input() && !popup && !overlay && connected;
        // ⌘Q and ⌘H belong to the PC too while it has the keys, and so do
        // ⌘+, ⌘- and ⌘0: egui would zoom Latch's own window on them while
        // they zoomed the browser on the PC.
        crate::macos::stream_owns_keys(keys_to_pc);
        if ctx.options(|o| o.zoom_with_keyboard) == keys_to_pc {
            ctx.options_mut(|o| o.zoom_with_keyboard = !keys_to_pc);
        }
        let ppp = ctx.pixels_per_point();
        let motion_scale = {
            let stats = live.session.stats();
            let stream = if stats.width > 0 && stats.height > 0 {
                (stats.width, stats.height)
            } else {
                (live.requested.0, live.requested.1)
            };
            if video.width() >= 1.0 {
                stream.0 as f32 / video.width() * desktop_per_stream_pixel(live.desktop, stream)
            } else {
                ppp
            }
        };
        let link = Link {
            ctx,
            live,
            cfg,
            out,
            connected,
            video,
            ppp,
        };
        live.clipboard
            .note_mac_clipboard(crate::macos::pasteboard_changes);

        if !focused {
            if self.held.any_down() || self.captured || self.pending.is_some() {
                self.reset_with(ctx, Some(live), Some(out));
            }
            return;
        }
        // A paste whose text has arrived goes before anything this frame.
        self.settle_paste(&link, false);
        // Caps Lock, which egui never reports, as a tap of the PC's own.
        let (caps, tap) = caps_tap(self.caps, self.mac_caps(), keys_to_pc);
        self.caps = caps;
        if tap {
            self.dispatch(&link, Queued::Tap(input::VK_CAPITAL));
        }
        // Ctrl+Alt is Latch's host key. It is read before the keys-to-PC
        // gate so it works even if an overlay took focus, and while it is
        // held nothing is forwarded: the PC must not see a held chord after
        // the toolbar opens (games and the Start menu react to one), nor a
        // lone Alt when Control is let go first (Windows opens the menu bar).
        // Before the stream connects it can still free a captured mouse.
        let ctrl_alt = modifiers.ctrl && modifiers.alt;
        if ctrl_alt || (self.capture_chord_held && (modifiers.ctrl || modifiers.alt)) {
            if ctrl_alt && !self.capture_chord_held && (connected || self.captured) {
                self.capture_chord_held = true;
                self.settle_paste(&link, true);
                if connected {
                    self.chord_before = Some((self.captured, self.bar_shown));
                    self.host_key(ctx, cfg, out);
                } else {
                    self.release(ctx, out);
                }
            }
            // The host key and S (or F6, GeForce NOW's key) toggles the
            // stats overlay, and leaves the mouse and the toolbar as they
            // were before the host key went down.
            let stats_key = events.iter().any(|e| {
                matches!(
                    e,
                    Event::Key {
                        key: egui::Key::S | egui::Key::F6,
                        pressed: true,
                        repeat: false,
                        ..
                    }
                )
            });
            if ctrl_alt && stats_key {
                self.stats = !self.stats;
                if let Some((captured, bar)) = self.chord_before.take() {
                    if captured && !self.captured {
                        self.grab(ctx, out);
                    } else if !captured && self.captured {
                        self.release(ctx, out);
                    }
                    self.bar_shown = bar;
                }
            }
            return;
        }
        self.capture_chord_held = false;
        self.chord_before = None;
        if !keys_to_pc && self.held.any_down() {
            self.held.release_all(out);
        }

        if live.session.cursor_supported() && connected {
            let host_should_draw = self.captured || !over_stream;
            if self.host_cursor_drawn != host_should_draw {
                self.host_cursor_drawn = host_should_draw;
                live.session.draw_cursor(host_should_draw);
            }
        }

        if (over_stream || self.captured) && connected {
            ctx.set_cursor_icon(CursorIcon::None);
        }

        if live.session.cursor_supported() && !self.captured && over_stream && connected {
            if let Some(pos) = pointer {
                if let Some(cursor_state) = live.session.cursor() {
                    if cursor_state.visible {
                        if let Some(shape) = cursor_state.shape {
                            let texture = match &self.cursor_texture {
                                Some((s, h)) if *s == shape.serial => Some(h.clone()),
                                _ => {
                                    if let Ok(rgba) = latch_proto::base64::decode(&shape.rgba) {
                                        if rgba.len()
                                            == (shape.width as usize) * (shape.height as usize) * 4
                                        {
                                            let img = egui::ColorImage::from_rgba_unmultiplied(
                                                [shape.width as usize, shape.height as usize],
                                                &rgba,
                                            );
                                            let handle = ctx.load_texture(
                                                "host-cursor",
                                                img,
                                                egui::TextureOptions::LINEAR,
                                            );
                                            self.cursor_texture =
                                                Some((shape.serial, handle.clone()));
                                            Some(handle)
                                        } else {
                                            None
                                        }
                                    } else {
                                        None
                                    }
                                }
                            };
                            if let Some(tex) = texture {
                                let stats = live.session.stats();
                                let vw = if stats.width > 0 {
                                    stats.width
                                } else {
                                    live.requested.0
                                };
                                let cursor_rect = host_cursor_rect(
                                    pos,
                                    video,
                                    vw,
                                    shape.hot_x,
                                    shape.hot_y,
                                    shape.width,
                                    shape.height,
                                );
                                let painter = ctx.layer_painter(egui::LayerId::new(
                                    egui::Order::Middle,
                                    egui::Id::new("host_cursor"),
                                ));
                                painter.image(
                                    tex.id(),
                                    cursor_rect,
                                    Rect::from_min_max(Pos2::new(0.0, 0.0), Pos2::new(1.0, 1.0)),
                                    Color32::WHITE,
                                );
                            }
                        }
                    }
                }
            }
        }

        let at = At {
            keys_to_pc,
            over_stream,
        };
        let physical_text = events.iter().any(
            |ev| matches!(ev, Event::Key { key, pressed: true, .. } if input::vk(*key).is_some()),
        );
        let commits: Vec<_> = events
            .iter()
            .filter_map(|ev| match ev {
                Event::Ime(egui::ImeEvent::Commit(t)) => Some(t.clone()),
                _ => None,
            })
            .collect();
        // The keys typed before a commit composed it and stay here; keys
        // after it in the same frame are new typing and go to the PC.
        let last_commit = events
            .iter()
            .rposition(|ev| matches!(ev, Event::Ime(egui::ImeEvent::Commit(_))));
        for (i, ev) in events.into_iter().enumerate() {
            match &ev {
                Event::Ime(egui::ImeEvent::Enabled | egui::ImeEvent::Preedit(_)) => {
                    self.composing = true
                }
                Event::Ime(egui::ImeEvent::Disabled | egui::ImeEvent::Commit(_)) => {
                    self.composing = false
                }
                Event::Key {
                    pressed: true,
                    modifiers,
                    ..
                } if (self.composing || last_commit.is_some_and(|c| i < c))
                    && !modifiers.ctrl
                    && !modifiers.command =>
                {
                    continue
                }
                _ => {}
            }
            if matches!(&ev, Event::Text(t) if self.composing || physical_text || commits.contains(t))
            {
                continue;
            }
            match ev {
                // The pointer is never held back behind a paste.
                Event::PointerMoved(p) if !self.captured && over_stream => self.moved_to = Some(p),
                Event::MouseMoved(d) if self.captured => {
                    // Deltas arrive in points; the PC moves in stream pixels.
                    // Scale by the picture's size here so a hand movement
                    // crosses the same share of the PC's desktop as of the
                    // picture, whatever the window size or the stream size.
                    self.motion.0 += d.x * motion_scale;
                    self.motion.1 += d.y * motion_scale;
                }
                Event::MouseWheel { unit, delta, .. } if self.captured || over_stream => {
                    let (h, v) = wheel_to_host(unit, delta);
                    self.scroll.0 += h;
                    self.scroll.1 += v;
                }
                Event::WindowFocused(false) => self.reset_with(ctx, Some(live), Some(out)),
                ev => self.dispatch(&link, Queued::Event(ev, at)),
            }
        }
        // egui reports a modifier pressed or let go on its own only in its
        // state at the end of the frame.
        if keys_to_pc {
            self.dispatch(&link, Queued::Modifiers(modifiers));
        }
        if let Some(p) = self.moved_to.take() {
            self.send_position(&link, p);
        }
        self.flush_motion(out);
        if self.scroll.0.abs() >= 1.0 || self.scroll.1.abs() >= 1.0 {
            let h = self.scroll.0.trunc();
            let v = self.scroll.1.trunc();
            self.scroll.0 -= h;
            self.scroll.1 -= v;
            out.scroll(
                v.clamp(-32000.0, 32000.0) as i16,
                h.clamp(-32000.0, 32000.0) as i16,
            );
        }
        if let Some(p) = &self.pending {
            ctx.request_repaint_after(p.until.saturating_duration_since(Instant::now()));
        }
    }

    /// Send `item` now, or hold it behind a paste still on its way.
    fn dispatch<S: Sink + ?Sized>(&mut self, link: &Link<'_, S>, item: Queued) {
        match &mut self.pending {
            Some(p) => p.queued.push(item),
            None => self.handle(link, item),
        }
    }

    /// Send one event to the PC, as it stood when it arrived.
    fn handle<S: Sink + ?Sized>(&mut self, link: &Link<'_, S>, item: Queued) {
        let out = link.out;
        let cmd_is_ctrl = self.cmd_is_ctrl(link.cfg);
        let (ev, at) = match item {
            Queued::Modifiers(m) => {
                self.cmd_since_v_up |= m.command;
                self.held.modifiers(out, m, cmd_is_ctrl);
                return;
            }
            Queued::Tap(vk) => {
                self.held.key(out, vk, true);
                self.held.key(out, vk, false);
                return;
            }
            Queued::Event(ev, at) => (ev, at),
        };
        match ev {
            Event::Key {
                physical_key,
                key,
                pressed,
                repeat,
                modifiers: m,
            } if at.keys_to_pc && (pressed || !repeat) => {
                // The host key never reaches the PC, even when it was
                // pressed and let go inside one frame.
                if m.ctrl && m.alt {
                    return;
                }
                // Each key goes with the modifiers held when it was pressed:
                // a Shift or ⌘ let go in the same frame came after it.
                self.cmd_since_v_up |= m.command;
                self.held.modifiers(out, m, cmd_is_ctrl);
                // OS autorepeat arrives as extra Downs with `repeat`.
                // The host does not generate them, so holding a key
                // would otherwise type once. Releases never repeat.
                let Some(vk) = physical_key.and_then(input::vk).or_else(|| input::vk(key)) else {
                    return;
                };
                let seen = self.held.is_down(vk);
                if key == egui::Key::V && !pressed {
                    let pasted = std::mem::take(&mut self.pasted_since_v_up);
                    let cmd = std::mem::take(&mut self.cmd_since_v_up);
                    if swallowed_paste(seen, pasted, cmd) {
                        // A paste from an image or nothing on the Mac: the
                        // PC pastes what it has. It goes on the key's
                        // release, the first sign of it.
                        self.paste(link, String::new());
                    }
                }
                if !pressed && !seen {
                    // The PC never saw it go down: egui took the press
                    // for ⌘C, ⌘X or ⌘V, or it was let go already.
                    return;
                }
                self.held.key(out, vk, pressed);
            }
            Event::Copy if at.keys_to_pc => {
                self.clipboard_chord(link, Edit::Copy);
                link.live.clipboard.poke();
            }
            Event::Cut if at.keys_to_pc => {
                self.clipboard_chord(link, Edit::Cut);
                link.live.clipboard.poke();
            }
            Event::Paste(text) if at.keys_to_pc => self.paste(link, text),
            Event::Text(text) if at.keys_to_pc => {
                if !text.is_empty() {
                    out.text(&text);
                }
            }
            Event::Ime(egui::ImeEvent::Commit(text)) if at.keys_to_pc => {
                if !text.is_empty() {
                    out.text(&text);
                }
            }
            Event::PointerButton {
                pos,
                button,
                pressed,
                modifiers: m,
            } if self.captured || at.over_stream || (!pressed && self.held.any_down()) => {
                // The click lands where it was made: the pointer goes
                // there first, before a capture would zero the motion.
                if self.captured {
                    self.flush_motion(out);
                } else {
                    self.moved_to = None;
                    self.send_position(link, pos);
                }
                // A click on the picture puts the toolbar away and, when
                // capture is on, takes the mouse. We complete capture before
                // synchronizing modifiers and sending the click so grab's
                // release does not drop the click's modifiers (F09).
                let recaptured = pressed && at.over_stream && !self.captured && link.connected;
                if recaptured {
                    self.bar_shown = false;
                    if link.cfg.capture_mouse {
                        self.grab(link.ctx, out);
                    }
                }
                if at.keys_to_pc && !(m.ctrl && m.alt) {
                    self.held.modifiers(out, m, cmd_is_ctrl);
                }
                if let Some(b) = input::button(button) {
                    self.held.button(out, b, pressed);
                }
            }
            _ => {}
        }
    }

    /// The Mac's clipboard first, then the paste: the PC pastes what was
    /// copied here, not what it had. While the text travels, the chord and
    /// everything after it wait (see [`PendingPaste`]).
    fn paste<S: Sink + ?Sized>(&mut self, link: &Link<'_, S>, text: String) {
        self.pasted_since_v_up = true;
        let text = if clipboard::worth_sending(&text) {
            text
        } else {
            String::new()
        };
        match link
            .live
            .clipboard
            .push(text, crate::macos::pasteboard_changes())
        {
            clipboard::Paste::Now => self.clipboard_chord(link, Edit::Paste),
            clipboard::Paste::Wait(id) => {
                self.pending = Some(PendingPaste {
                    id,
                    until: Instant::now() + PASTE_WAIT,
                    queued: Vec::new(),
                });
            }
        }
    }

    /// Press a waiting paste's chord once its text is on the PC, or `now`,
    /// or once it has waited long enough (the PC then pastes what it has),
    /// and send what was held back behind it, in order.
    fn settle_paste<S: Sink + ?Sized>(&mut self, link: &Link<'_, S>, now: bool) {
        let Some(p) = &self.pending else { return };
        let ready = link.live.clipboard.paste_ready(p.id);
        if !(ready || now || Instant::now() >= p.until) {
            return;
        }
        let Some(p) = self.pending.take() else { return };
        if !ready {
            link.live.clipboard.give_up(p.id);
        }
        self.clipboard_chord(link, Edit::Paste);
        for item in p.queued {
            self.dispatch(link, item);
        }
    }

    /// Send a pointer position in the picture: over the black bars, the
    /// nearest pixel of the picture (see [`mouse_position_in_video`]).
    fn send_position<S: Sink + ?Sized>(&self, link: &Link<'_, S>, p: Pos2) {
        let (x, y, width, height) = mouse_position_in_video(p, link.video, link.ppp);
        link.out.position(x, y, width, height);
    }

    /// Send the whole pixels of captured movement, keeping the fraction.
    fn flush_motion(&mut self, out: &(impl Sink + ?Sized)) {
        if self.captured && (self.motion.0.abs() >= 1.0 || self.motion.1.abs() >= 1.0) {
            let dx = self.motion.0.trunc();
            let dy = self.motion.1.trunc();
            self.motion.0 -= dx;
            self.motion.1 -= dy;
            out.motion(dx as i16, dy as i16);
        }
    }

    /// Whether Caps Lock is on on this Mac (what a test set, in tests).
    fn mac_caps(&self) -> Option<bool> {
        #[cfg(test)]
        if self.tap.is_some() {
            return self.caps_now;
        }
        crate::macos::caps_lock()
    }

    /// Whether ⌘ goes to the PC as Ctrl. A Mac there gets Command whatever
    /// the setting, which is about Windows and Linux.
    fn cmd_is_ctrl(&self, cfg: &ClientConfig) -> bool {
        cfg.cmd_is_ctrl && !self.host_mac
    }

    /// What ⌘ stands for in a clipboard shortcut.
    fn clipboard_modifier(&self, cfg: &ClientConfig) -> i16 {
        if self.host_mac || (cfg!(target_os = "macos") && !cfg.cmd_is_ctrl) {
            input::VK_LWIN
        } else {
            input::VK_CONTROL
        }
    }

    /// egui swallows Cmd/Ctrl+C/X/V into clipboard events; replay them as
    /// the key chord the PC expects.
    fn clipboard_chord<S: Sink + ?Sized>(&mut self, link: &Link<'_, S>, edit: Edit) {
        let (keys, alone) = link.live.shortcuts.keys(
            edit,
            self.clipboard_modifier(link.cfg),
            link.live.clipboard.pastes_to_selection(),
        );
        if alone {
            self.held.chord_alone(link.out, &keys);
        } else {
            self.held.chord(link.out, &keys);
        }
    }
}

/// Whether a V let go was a ⌘V egui swallowed: the PC never saw V go
/// down, no paste came from it, and ⌘ was down since the last V.
fn swallowed_paste(v_seen_down: bool, pasted: bool, cmd: bool) -> bool {
    !v_seen_down && !pasted && cmd
}

/// Caps Lock kept in step with the Mac's: `seen` is the Mac's state the PC
/// was last brought in line with, `now` the Mac's. The PC's own state can't
/// be read, so the first one seen while the stream has the keys is taken as
/// matching; after that each change reaches the PC as one tap of Caps Lock,
/// including one made while Latch or another app had the keys, once the
/// stream has them again. Returns the new `seen`, and whether to tap.
fn caps_tap(seen: Option<bool>, now: Option<bool>, keys_to_pc: bool) -> (Option<bool>, bool) {
    match (seen, now) {
        (_, None) => (seen, false),
        _ if !keys_to_pc => (seen, false),
        (None, now) => (now, false),
        (Some(s), Some(n)) => (Some(n), s != n),
    }
}

/// A wheel or trackpad scroll as the PC takes it, in 1/120ths of a click:
/// (horizontal, vertical). egui's +x moves the content right, which is
/// scrolling left; Latch's horizontal scroll, like Windows' and
/// Linux's, is positive to the right. Vertical agrees: +y is up.
fn wheel_to_host(unit: egui::MouseWheelUnit, delta: Vec2) -> (f32, f32) {
    let scale = match unit {
        egui::MouseWheelUnit::Point => 3.0,
        egui::MouseWheelUnit::Line => 120.0,
        egui::MouseWheelUnit::Page => 1200.0,
    };
    (-delta.x * scale, delta.y * scale)
}

/// Position and bounding rectangle for the host cursor rendered locally in free mouse mode.
fn host_cursor_rect(
    pointer: Pos2,
    video: Rect,
    stream_w: u32,
    hot_x: u32,
    hot_y: u32,
    shape_w: u32,
    shape_h: u32,
) -> Rect {
    let scale = if stream_w > 0 {
        video.width() / stream_w as f32
    } else {
        1.0
    };
    let cw = shape_w as f32 * scale;
    let ch = shape_h as f32 * scale;
    let hx = hot_x as f32 * scale;
    let hy = hot_y as f32 * scale;
    let cursor_pos = Pos2::new(pointer.x - hx, pointer.y - hy);
    Rect::from_min_size(cursor_pos, Vec2::new(cw, ch))
}

/// Free mouse mode maps the bars to the nearest pixel in the picture.
fn mouse_position_in_video(p: Pos2, video: Rect, ppp: f32) -> (i16, i16, i16, i16) {
    let width = ((video.width() * ppp).round() as i16).max(2);
    let height = ((video.height() * ppp).round() as i16).max(2);
    // Rect's far edges are inclusive, but pixels are zero-based. Latch
    // sends width - 1 and height - 1 as the coordinate limits on the wire.
    let x = ((p.x - video.left()) * ppp)
        .round()
        .clamp(0.0, f32::from(width - 1)) as i16;
    let y = ((p.y - video.top()) * ppp)
        .round()
        .clamp(0.0, f32::from(height - 1)) as i16;
    (x, y, width, height)
}

/// Whether a pointer in the stream viewport should be sent to the PC. A real egui
/// popup (More) sets `popup`; a hand-rolled `Area` at `Order::Background`
/// would not, and clicks would leak through.
fn pointer_on_stream(
    in_viewport: bool,
    over_bar: bool,
    popup: bool,
    overlay: bool,
    over_overlay: bool,
) -> bool {
    in_viewport && !over_bar && !popup && !overlay && !over_overlay
}

/// Whether `host_version` is too old for `/v1/update`, with the release
/// that an install through the stream would put on it.
pub fn old_host(
    host_version: &str,
    latest: Option<&Version>,
) -> Option<(Version, Option<Version>)> {
    let v = Version::parse(host_version).ok()?;
    if latch_core::update::host_can_receive_update(&v) {
        return None;
    }
    Some((v, latest.cloned()))
}

/// The largest rect of the given aspect ratio centred in `area`.
pub fn fit(area: Rect, aspect: f32) -> Rect {
    let (w, h) = if area.width() / area.height() > aspect {
        (area.height() * aspect, area.height())
    } else {
        (area.width(), area.width() / aspect)
    };
    Rect::from_center_size(area.center(), Vec2::new(w.floor(), h.floor()))
}

/// How many of the PC's desktop pixels one stream pixel stands for. 1
/// unless the desktop is a fixed size the stream shrinks, keeping its
/// proportions, to fit inside; relative mouse movement lands in the
/// desktop's pixels, so it is scaled by this too.
fn desktop_per_stream_pixel(desktop: Option<(u32, u32)>, stream: (u32, u32)) -> f32 {
    match desktop {
        Some((dw, dh)) if stream.0 > 0 && stream.1 > 0 => {
            (dw as f32 / stream.0 as f32).max(dh as f32 / stream.1 as f32)
        }
        _ => 1.0,
    }
}

/// What the renderer has drawn of the frames the decoder published, a
/// second at a time.
#[derive(Debug, Default)]
struct Presented {
    seq: u64,
    since: Option<Instant>,
    shown: u32,
    skipped: u64,
    wait_us: u64,
    /// Pictures drawn a second, over the last whole second.
    fps: f32,
    /// Pictures published but replaced before they were drawn.
    not_shown: u64,
    /// From the decoder publishing a picture to this window drawing it.
    wait_ms: f32,
    /// This display's refresh rate; 0 when unknown.
    hz: u32,
}

impl Presented {
    /// A UI frame is being drawn with the slot at `seq`.
    fn frame(&mut self, now: Instant, seq: u64, published: Option<Instant>) {
        if seq > self.seq {
            if self.seq > 0 {
                self.skipped += seq - self.seq - 1;
            }
            self.seq = seq;
            self.shown += 1;
            if let Some(at) = published {
                self.wait_us += now.saturating_duration_since(at).as_micros() as u64;
            }
        }
        let since = *self.since.get_or_insert(now);
        let secs = now.saturating_duration_since(since).as_secs_f32();
        if secs >= 1.0 {
            self.fps = self.shown as f32 / secs;
            self.wait_ms = self.wait_us as f32 / self.shown.max(1) as f32 / 1000.0;
            self.not_shown = self.skipped;
            self.hz = crate::app::ClientApp::display_hz();
            (self.shown, self.skipped, self.wait_us) = (0, 0, 0);
            self.since = Some(now);
        }
    }
}

/// One line of the stats overlay.
#[derive(Debug, Clone, PartialEq)]
struct Row {
    label: &'static str,
    value: String,
    /// Worth a look: shown in the warning colour.
    warn: bool,
}

fn row(label: &'static str, value: String, warn: bool) -> Row {
    Row { label, value, warn }
}

/// The stats overlay's lines, from what the session and the renderer
/// measured. The latency is an estimate: the host's capture to send, half
/// the round trip, putting the frame together, waiting for and running the
/// decoder, waiting to be drawn, and half a refresh of this display on
/// average before the picture is on it.
fn overlay_rows(live: &Live, s: &latch_stream::Stats, p: &Presented) -> Vec<Row> {
    let (w, h) = if s.width > 0 {
        (s.width, s.height)
    } else {
        (live.requested.0, live.requested.1)
    };
    let asked = live.requested.2;
    let codec = if !s.codec.is_empty() {
        s.codec
    } else {
        live.codec
    };
    let ceiling = live.settings.bitrate_kbps / 1000;
    let target = if s.applied_bitrate_kbps > 0 {
        s.applied_bitrate_kbps / 1000
    } else {
        ceiling
    };
    let hz = if p.hz > 0 { p.hz } else { 60 };
    let display = 1000.0 / hz as f32 / 2.0;
    let network = s.rtt_ms as f32 / 2.0 + s.assembly_ms;
    let decode = s.queue_ms + s.decode_ms;
    let shown = p.wait_ms + display;
    let total = s.host_ms + network + decode + shown;
    let slow = |fps: f32| asked > 0 && fps < asked as f32 * 0.9;
    vec![
        row("Resolution", format!("{w} × {h}"), false),
        row("Codec", format!("{codec} · {}", s.decoder), false),
        row(
            "Frame rate",
            format!(
                "{:.0} in · {:.0} decoded · {:.0} shown / {asked}",
                s.received_fps, s.fps, p.fps
            ),
            slow(s.fps),
        ),
        row(
            "Bitrate",
            if target < ceiling {
                format!("{:.1} / {target} of {ceiling} Mbps", s.mbps)
            } else {
                format!("{:.1} / {ceiling} Mbps", s.mbps)
            },
            false,
        ),
        row(
            "Round trip",
            format!("{} ms ± {}", s.rtt_ms, s.rtt_var_ms),
            false,
        ),
        row("Jitter", format!("{:.1} ms", s.frame_jitter_ms), false),
        row(
            "Packet loss",
            format!("{:.2}% · {:.2}% after FEC", s.raw_loss_pct, s.loss_pct),
            s.loss_pct > 0.0,
        ),
        row(
            "Dropped",
            format!(
                "{} lost · {} skipped · {} not shown",
                s.frames_lost, s.frames_dropped, p.not_shown
            ),
            s.frames_lost + s.frames_dropped > 0,
        ),
        row("Decode", format!("{:.1} ms", s.decode_ms), false),
        row("Present", format!("{:.1} ms", p.wait_ms), false),
        row("Latency", format!("≈ {total:.0} ms"), false),
        row(
            "",
            format!(
                "host {:.1} · net {network:.1} · decode {decode:.1} · display {shown:.1}",
                s.host_ms
            ),
            false,
        ),
    ]
}

/// The quality profile a stream runs at, named as Settings names it.
fn profile_label(s: &StreamSettings) -> String {
    match s.quality {
        crate::config::Quality::Custom => format!("Custom · {}", s.describe()),
        q => q.label().into(),
    }
}

/// A question that floats under the toolbar, centred: restart, install.
fn question(ctx: &egui::Context, id: &str, bar: Rect, w: f32, add: impl FnOnce(&mut egui::Ui)) {
    egui::Area::new(Id::new(id))
        .fixed_pos(Pos2::new(
            bar.center().x - w / 2.0 - OVERLAY_PAD,
            bar.bottom() + space::SM,
        ))
        .order(egui::Order::Foreground)
        .show(ctx, |ui| {
            ui::overlay_frame().show(ui, |ui| {
                ui.set_width(w);
                // A long question on a short window scrolls inside its
                // frame instead of running off the bottom.
                let room = ui::room_below(ui, OVERLAY_PAD + 1.0);
                ui::clipped_scroll(ui, id, room, |ui| {
                    ui.set_width(w);
                    ui.spacing_mut().item_spacing.y = space::SM;
                    add(ui)
                });
            });
        });
}

/// The gamepads this machine sends, when there are any: how many, and on
/// hover which, or why the host takes none.
fn pads_tag(ui: &mut egui::Ui, live: &Live) {
    let pads = live.pads.connected();
    if pads.is_empty() {
        return;
    }
    let problem = live.session.pad_problem();
    let taken = live.input.pad_slots();
    let tone = if !problem.is_empty() {
        Tone::Warning
    } else if !live.session.connected() {
        Tone::Accent
    } else {
        Tone::Success
    };
    let label = match pads.len() {
        1 => "1 gamepad".to_string(),
        n => format!("{n} gamepads"),
    };
    let mut lines: Vec<String> = pads
        .iter()
        .map(|p| {
            let note = if problem.is_empty() && live.session.connected() && p.slot >= taken {
                format!(" — {} takes only {taken}", live.pc)
            } else {
                String::new()
            };
            format!("{}. {} ({}){note}", p.slot + 1, p.name, p.kind.name())
        })
        .collect();
    if !problem.is_empty() {
        lines.push(format!("Not sent to {}: {problem}", live.pc));
    }
    ui::tag(ui, Some(tone), &label).on_hover_text(lines.join("\n"));
}

/// A short vertical hairline between groups of toolbar controls.
fn toolbar_rule(ui: &mut egui::Ui) {
    let (rect, _) = ui.allocate_exact_size(Vec2::new(1.0, 16.0), egui::Sense::hover());
    ui.painter().vline(
        rect.center().x,
        rect.y_range(),
        theme::stroke(1.0, Color32::from_white_alpha(36)),
    );
}

#[cfg(test)]
pub(crate) mod tests {
    pub fn hold_test_key(view: &mut super::View, input: &latch_stream::Input) {
        view.held.key(input, 0x41, true);
        assert!(view.held.any_down());
    }
    pub fn assert_no_held_input(view: &super::View) {
        assert!(!view.held.any_down());
    }

    use super::*;
    use crate::config::ClientConfig;
    use crate::session::Live;
    use egui_kittest::kittest::Queryable;

    #[test]
    fn a_capture_puts_a_pointer_off_the_window_on_the_picture() {
        let commands = |pointer: Option<Pos2>| {
            let ctx = egui::Context::default();
            let mut input = egui::RawInput {
                screen_rect: Some(Rect::from_min_size(Pos2::ZERO, Vec2::new(800.0, 600.0))),
                ..Default::default()
            };
            if let Some(p) = pointer {
                input.events.push(Event::PointerMoved(p));
            }
            let mut view = View::default();
            let out = ctx.run(input, |ctx| view.set_captured(ctx, true));
            out.viewport_output[&egui::ViewportId::ROOT]
                .commands
                .clone()
        };
        // Held where it was clicked, on the picture.
        assert!(!commands(Some(Pos2::new(10.0, 20.0)))
            .iter()
            .any(|c| matches!(c, ViewportCommand::CursorPosition(_))));
        // Somewhere else: moved to the middle before it is held.
        let off = commands(None);
        let warp = off
            .iter()
            .position(|c| *c == ViewportCommand::CursorPosition(Pos2::new(400.0, 300.0)));
        let grab = off
            .iter()
            .position(|c| matches!(c, ViewportCommand::CursorGrab(_)));
        assert!(warp.is_some() && warp < grab, "{off:?}");
    }

    #[test]
    fn host_cursor_rect_unscaled_and_scaled() {
        let video = Rect::from_min_size(Pos2::new(100.0, 50.0), Vec2::new(1920.0, 1080.0));
        let pointer = Pos2::new(500.0, 400.0);

        // 1:1 scale: stream_w = 1920 matches video.width()
        let rect = host_cursor_rect(pointer, video, 1920, 5, 10, 32, 48);
        assert_eq!(rect.min, Pos2::new(495.0, 390.0));
        assert_eq!(rect.size(), Vec2::new(32.0, 48.0));

        // Half scale: video.width() is 960 (half of 1920)
        let half_video = Rect::from_min_size(Pos2::ZERO, Vec2::new(960.0, 540.0));
        let rect_half = host_cursor_rect(pointer, half_video, 1920, 10, 20, 40, 60);
        // scale = 0.5; hot_x = 5.0, hot_y = 10.0, cw = 20.0, ch = 30.0
        assert_eq!(rect_half.min, Pos2::new(495.0, 390.0));
        assert_eq!(rect_half.size(), Vec2::new(20.0, 30.0));

        // Fallback when stream_w is 0
        let rect_zero = host_cursor_rect(pointer, video, 0, 2, 3, 16, 16);
        assert_eq!(rect_zero.min, Pos2::new(498.0, 397.0));
        assert_eq!(rect_zero.size(), Vec2::new(16.0, 16.0));
    }

    #[test]
    fn fit_letterboxes_a_16_9_stream_on_a_16_10_screen() {
        let screen = Rect::from_min_size(Pos2::ZERO, Vec2::new(1512.0, 982.0));
        let v = fit(screen, 16.0 / 9.0);
        assert_eq!(v.width(), 1512.0);
        assert!((v.height() - 850.0).abs() <= 1.0);
        assert!(
            v.top() > 60.0,
            "a 16:9 picture on a 16:10 screen leaves a bar above: {}",
            v.top()
        );
        let tall = fit(
            Rect::from_min_size(Pos2::ZERO, Vec2::new(800.0, 900.0)),
            16.0 / 9.0,
        );
        assert_eq!(tall.width(), 800.0);
        let matched = fit(screen, 3024.0 / 1964.0);
        assert!((matched.height() - screen.height()).abs() < 0.1);
        assert!((matched.width() - screen.width()).abs() < 0.1);
    }

    #[test]
    fn mouse_movement_crosses_a_scaled_desktop_at_the_desktops_scale() {
        assert_eq!(desktop_per_stream_pixel(None, (1280, 720)), 1.0);
        // 720p of a fixed 1080p desktop: each stream pixel is 1.5 of its.
        assert_eq!(
            desktop_per_stream_pixel(Some((1920, 1080)), (1280, 720)),
            1.5
        );
        // A 16:10 desktop in a 16:9 stream is fitted by its height.
        let s = desktop_per_stream_pixel(Some((1920, 1200)), (1280, 720));
        assert!((s - 1200.0 / 720.0).abs() < 1e-6, "{s}");
        assert_eq!(desktop_per_stream_pixel(Some((1920, 1080)), (0, 0)), 1.0);
    }

    #[test]
    fn a_3_0_host_is_offered_an_install_through_the_stream() {
        let latest = Version::new(3, 1, 0);
        let (v, l) = old_host("3.0.0", Some(&latest)).unwrap();
        assert_eq!(v, Version::new(3, 0, 0));
        assert_eq!(l, Some(latest));
        assert!(old_host("3.1.0", None).is_none());
        assert!(old_host("soon", None).is_none());
        assert!(old_host("3.0.1", None).is_some());
    }

    #[test]
    fn overlay_width_clamps_to_the_screen() {
        assert_eq!(overlay_width(440.0, 640.0), 440.0);
        assert_eq!(overlay_width(500.0, 640.0), 500.0);
        assert_eq!(overlay_width(496.0, 640.0), 496.0);
        assert_eq!(overlay_width(440.0, 400.0), 400.0 - 4.0 * OVERLAY_PAD);
        assert!(overlay_width(500.0, 400.0) + 4.0 * OVERLAY_PAD <= 400.0);
    }

    #[test]
    fn a_popup_eats_clicks_that_would_otherwise_hit_the_picture() {
        assert!(pointer_on_stream(true, false, false, false, false));
        assert!(
            !pointer_on_stream(true, false, true, false, false),
            "More open: any_popup_open must block remote mouse"
        );
        assert!(!pointer_on_stream(true, true, false, false, false));
        assert!(!pointer_on_stream(true, false, false, true, false));
        assert!(!pointer_on_stream(true, false, false, false, true));
        assert!(
            pointer_on_stream(true, false, false, false, false),
            "a Background Area would leave popup=false and leak the click"
        );
    }

    #[test]
    fn clicks_in_letterbox_and_pillarbox_bars_capture_the_mouse() {
        for (size, points) in [
            (
                Vec2::new(1920.0, 1440.0),
                [Pos2::new(960.0, 90.0), Pos2::new(960.0, 1350.0)],
            ),
            (
                Vec2::new(2400.0, 1080.0),
                [Pos2::new(120.0, 600.0), Pos2::new(2280.0, 600.0)],
            ),
        ] {
            let video = fit(Rect::from_min_size(Pos2::ZERO, size), 16.0 / 9.0);
            for p in points {
                assert!(!video.contains(p), "the regression must hit a black bar");
                // Connected: a click before the stream connects takes nothing.
                let tap = std::rc::Rc::new(crate::input::Recorder::default());
                tap.connected.set(true);
                let view = View {
                    tap: Some(tap.clone()),
                    ..view_with_bar()
                };
                let extra = Extra {
                    os: "windows".into(),
                    ..Default::default()
                };
                let mut h = harness_with(size, 1.0, false, fixture(view, extra));
                h.input_mut().events.extend([
                    Event::PointerMoved(p),
                    Event::PointerButton {
                        pos: p,
                        button: egui::PointerButton::Primary,
                        pressed: true,
                        modifiers: egui::Modifiers::NONE,
                    },
                ]);
                h.run_steps(1);
                assert!(h.state().view.captured, "black bar at {p} must capture");
                assert!(!h.state().view.bar_shown);
                assert!(h.state().view.held.any_down(), "click must reach the PC");
                h.input_mut()
                    .events
                    .push(Event::MouseMoved(Vec2::new(3.25, -3.25)));
                h.run_steps(1);
                assert!(h.state().view.captured);
                assert_eq!(
                    h.state().view.motion,
                    (0.25, -0.25),
                    "raw movement must continue in the bars"
                );
            }
        }
    }

    #[test]
    fn mouse_coordinates_stop_at_the_last_pixel_on_every_edge() {
        for size in [Vec2::new(1512.0, 982.0), Vec2::new(2400.0, 1080.0)] {
            let screen = Rect::from_min_size(Pos2::new(17.0, 33.0), size);
            let video = fit(screen, 16.0 / 9.0);
            for ppp in [1.0, 1.5, 2.0] {
                let width = (video.width() * ppp).round() as i16;
                let height = (video.height() * ppp).round() as i16;
                for (p, expected) in [
                    (video.min, (0, 0)),
                    (video.max, (width - 1, height - 1)),
                    (Pos2::new(video.right(), video.top()), (width - 1, 0)),
                    (Pos2::new(video.left(), video.bottom()), (0, height - 1)),
                    (screen.min - Vec2::splat(100.0), (0, 0)),
                    (screen.max + Vec2::splat(100.0), (width - 1, height - 1)),
                ] {
                    let (x, y, w, h) = mouse_position_in_video(p, video, ppp);
                    assert_eq!((x, y), expected, "{size}, scale {ppp}, point {p}");
                    assert_eq!((w, h), (width, height));
                }
                let (x, y, _, _) = mouse_position_in_video(video.center(), video, ppp);
                assert_eq!(
                    (x, y),
                    (
                        (width as f32 / 2.0).round() as i16,
                        (height as f32 / 2.0).round() as i16
                    )
                );
            }
        }
    }

    #[test]
    fn the_power_menu_is_named_for_the_machine() {
        assert_eq!(machine_noun("windows"), "PC");
        assert_eq!(machine_noun("macOS"), "Mac");
        assert_eq!(machine_noun("linux"), "Machine");
        assert_eq!(machine_noun(""), "Machine");
    }

    /// What the window tells the view, besides the stream itself.
    #[derive(Default, Clone)]
    pub(crate) struct Extra {
        pub os: String,
        pub old_host: Option<(Version, Option<Version>)>,
        pub handover: Option<String>,
        pub video_help: Option<crate::display::Help>,
        pub power_allowed: bool,
        pub can_sleep: bool,
    }

    pub(crate) struct Fixture {
        pub view: View,
        pub live: Option<Live>,
        pub cfg: ClientConfig,
        pub fullscreen: bool,
        pub extra: Extra,
        pub actions: Vec<Action>,
        applied: bool,
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            if let Some(live) = self.live.take() {
                live.session.stop();
            }
        }
    }

    /// A session to an address that never answers, so the view shows its
    /// toolbar and overlays with no picture behind them.
    pub(crate) fn dummy_live(ctx: &egui::Context) -> Live {
        live_to(
            ctx,
            latch_stream::session::Server {
                address: "10.255.255.1".parse().unwrap(),
                port: latch_core::CONTROL_PORT,
            },
            (1920, 1080, 60),
        )
    }

    /// A session to `server` asking for `requested` (width, height, fps).
    fn live_to(
        ctx: &egui::Context,
        server: latch_stream::session::Server,
        requested: (u32, u32, u32),
    ) -> Live {
        let (tx, rx) = std::sync::mpsc::channel();
        let frames = std::sync::Arc::new(latch_stream::FrameSlot::default());
        let path = crate::path::Path {
            direct: Some(false),
            relay: "tok".into(),
            rtt_ms: Some(210),
            ..Default::default()
        };
        let settings = crate::config::StreamSettings::default();
        let session = latch_stream::Session::start(
            server,
            latch_stream::Settings {
                color: Default::default(),
                width: requested.0,
                height: requested.1,
                fps: requested.2,
                bitrate_kbps: 50_000,
                hevc: true,
                av1: true,
                audio: true,
                name: "UI test".into(),
                cursor: true,
                gamepads: false,
            },
            frames.clone(),
            tx,
            || {},
        );
        let ip: std::net::Ipv4Addr = "100.64.0.10".parse().unwrap();
        let input = session.input();
        let pads = crate::gamepads::Link::new(&session, input.clone());
        Live {
            generation: 0,
            pc: "Gaming-PC".into(),
            node_id: "n1".into(),
            ip,
            session,
            input,
            frames,
            events: rx,
            started: Instant::now(),
            codec: "HEVC",
            requested,
            desktop: None,
            settings,
            path,
            clipboard: crate::clipboard::Sync::spawn(ip, ctx.clone()),
            shortcuts: Default::default(),
            pads,
        }
    }

    fn paint(ctx: &egui::Context, f: &mut Fixture) {
        if !f.applied {
            latch_ui::apply(ctx);
            f.applied = true;
            return;
        }
        if f.live.is_none() {
            f.live = Some(dummy_live(ctx));
        }
        let Fixture {
            view,
            live,
            cfg,
            fullscreen,
            extra,
            ..
        } = f;
        let env = Env {
            live: live.as_ref().unwrap(),
            cfg,
            fullscreen: *fullscreen,
            path: None,
            os: extra.os.clone(),
            old_host: extra.old_host.clone(),
            handover: extra.handover.clone(),
            video_help: extra.video_help.clone(),
            power_allowed: extra.power_allowed,
            can_sleep: extra.can_sleep,
        };
        let actions = view.show(ctx, &env);
        if !actions.is_empty() {
            f.actions = actions;
        }
    }

    /// A view with the toolbar dropped, as the host key leaves it.
    fn view_with_bar() -> View {
        View {
            bar_shown: true,
            ..View::default()
        }
    }

    pub(crate) fn fixture(view: View, extra: Extra) -> Fixture {
        Fixture {
            view,
            live: None,
            cfg: ClientConfig::default(),
            fullscreen: false,
            extra,
            actions: Vec::new(),
            applied: false,
        }
    }

    fn harness_with(
        size: Vec2,
        ppp: f32,
        gpu: bool,
        f: Fixture,
    ) -> egui_kittest::Harness<'static, Fixture> {
        let mut b = egui_kittest::Harness::builder()
            .with_size(size)
            .with_pixels_per_point(ppp)
            .with_max_steps(2);
        if gpu {
            b = b.wgpu();
        }
        let mut h = b.build_state(paint, f);
        h.run_steps(3);
        h
    }

    fn harness(size: Vec2) -> egui_kittest::Harness<'static, Fixture> {
        harness_with(
            size,
            1.0,
            false,
            fixture(
                view_with_bar(),
                Extra {
                    os: "windows".into(),
                    power_allowed: true,
                    can_sleep: true,
                    ..Default::default()
                },
            ),
        )
    }

    fn bar_control_rects(h: &egui_kittest::Harness<'_, Fixture>) -> Vec<(String, Rect)> {
        let names = [
            "Disconnect",
            "More",
            "PC",
            "Full screen",
            "Exit full screen",
            "Stats",
            "Hide stats",
            "Keys",
            "Stream settings",
            "Mouse",
        ];
        let mut out = Vec::new();
        for name in names {
            for node in h.query_all_by_label(name) {
                let Some(b) = node.raw_bounds() else { continue };
                if b.y0 > f64::from(size::TOOLBAR + 8.0) {
                    continue;
                }
                out.push((
                    name.to_string(),
                    Rect::from_min_max(
                        Pos2::new(b.x0 as f32, b.y0 as f32),
                        Pos2::new(b.x1 as f32, b.y1 as f32),
                    ),
                ));
            }
        }
        out
    }

    fn assert_no_overlap(rects: &[(String, Rect)]) {
        for (i, (a_name, a)) in rects.iter().enumerate() {
            for (b_name, b) in rects.iter().skip(i + 1) {
                let hit = a.intersect(*b);
                assert!(
                    hit.width() <= 1.0 || hit.height() <= 1.0,
                    "{a_name} {} overlaps {b_name} {}",
                    a,
                    b
                );
            }
        }
    }

    #[test]
    fn toolbar_at_640_does_not_overlap_and_keeps_disconnect() {
        let h = harness(Vec2::new(640.0, 420.0));
        assert!(
            h.query_by_label("Disconnect").is_some(),
            "Disconnect must stay reachable"
        );
        assert!(
            h.query_by_label("More").is_some(),
            "narrow width must overflow into More"
        );
        assert!(
            h.query_by_label("Stream settings").is_some(),
            "Stream settings stays on the bar at every width"
        );
        let rects = bar_control_rects(&h);
        assert_no_overlap(&rects);
        assert!(
            h.state().view.bar_h >= size::TOOLBAR,
            "bar_rect is measured, got {}",
            h.state().view.bar_h
        );
        crate::app::snapshots::assert_fits(&h, 640.0);
    }

    #[test]
    fn more_is_a_real_egui_popup_and_consumes_clicks() {
        let mut h = harness(Vec2::new(640.0, 420.0));
        assert!(
            !h.ctx.memory(|m| m.any_popup_open()),
            "closed More is not a popup"
        );
        h.get_by_label("More").simulate_click();
        h.run_steps(2);
        assert!(
            h.ctx.memory(|m| m.any_popup_open()),
            "More must use egui::popup so any_popup_open is true; a Background Area would fail this"
        );
        for item in [
            "Ctrl+Alt+Delete",
            "Stats overlay",
            "Stream performance",
            "Sleep…",
        ] {
            assert!(h.query_by_label(item).is_some(), "More lists {item}");
        }
        let popup_open = h.ctx.memory(|m| m.any_popup_open());
        assert!(!pointer_on_stream(true, false, popup_open, false, false));
        h.get_by_label("Stream performance").simulate_click();
        h.run_steps(2);
        assert!(
            h.state().view.details,
            "click inside More is consumed by the popup, not forwarded"
        );
    }

    #[test]
    fn power_items_follow_the_owner_and_sleep_only_where_it_can_wake() {
        let wide = Vec2::new(1400.0, 860.0);
        let with = |power_allowed, can_sleep, os: &str| {
            harness_with(
                wide,
                1.0,
                false,
                fixture(
                    view_with_bar(),
                    Extra {
                        os: os.into(),
                        power_allowed,
                        can_sleep,
                        ..Default::default()
                    },
                ),
            )
        };
        // The owner turned power actions off: no menu at all.
        let h = with(false, false, "windows");
        assert!(h.query_by_label("PC").is_none(), "an empty menu is no menu");
        // A Linux VPS: restart and shut down, never sleep.
        let mut h = with(true, false, "linux");
        h.get_by_label("Machine").simulate_click();
        h.run_steps(2);
        assert!(h.query_by_label("Restart…").is_some());
        assert!(h.query_by_label("Sleep…").is_none());
        // A Windows PC: sleep too, and it asks first.
        let mut h = with(true, true, "windows");
        h.get_by_label("PC").simulate_click();
        h.run_steps(2);
        h.get_by_label("Sleep…").simulate_click();
        h.run_steps(2);
        assert_eq!(h.state().view.confirm, Some(PowerAction::Sleep));
        assert!(
            h.query_by_label_contains("woken from its own network")
                .is_some(),
            "the question says what sleep costs"
        );
        // The narrow toolbar's More menu follows the same rule.
        let mut h = harness_with(
            Vec2::new(640.0, 420.0),
            1.0,
            false,
            fixture(view_with_bar(), Extra::default()),
        );
        h.get_by_label("More").simulate_click();
        h.run_steps(2);
        for item in ["Sleep…", "Restart…", "Shut down…"] {
            assert!(h.query_by_label(item).is_none(), "{item}");
        }
    }

    #[test]
    fn toolbar_at_normal_width_keeps_controls_on_the_bar() {
        let h = harness(Vec2::new(1400.0, 860.0));
        assert!(h.query_by_label("Disconnect").is_some());
        assert!(h.query_by_label("More").is_none());
        assert!(h.query_by_label("Stream settings").is_some());
        assert!(h.query_by_label("Keys").is_some());
        assert!(
            h.query_by_label("PC").is_some(),
            "a Windows machine's menu reads PC"
        );
        assert_no_overlap(&bar_control_rects(&h));
        crate::app::snapshots::assert_fits(&h, 1400.0);
    }

    #[test]
    fn stats_toggle_controls_the_overlay_with_and_without_letterboxing() {
        for size in [Vec2::new(1920.0, 1200.0), Vec2::new(1920.0, 1080.0)] {
            let mut h = harness(size);
            h.state()
                .live
                .as_ref()
                .unwrap()
                .frames
                .publish(latch_stream::video::Frame::default());
            h.run_steps(2);
            let overlay = |h: &egui_kittest::Harness<'_, Fixture>| {
                h.output().shapes.iter().any(|s| {
                    matches!(&s.shape, egui::Shape::Text(t) if t.galley.job.text.contains("shown / 60"))
                })
            };
            assert!(!overlay(&h), "stats start hidden at {size}");
            h.get_by_label("Stats").simulate_click();
            h.run_steps(3);
            assert!(overlay(&h), "Stats shows the overlay at {size}");
            h.get_by_label("Hide stats").simulate_click();
            h.run_steps(3);
            assert!(!overlay(&h), "Hide stats removes the overlay at {size}");
        }
    }

    #[test]
    fn the_host_key_and_s_toggle_the_overlay_and_leave_the_mouse_alone() {
        let (mut h, _tap) = recording(View::default(), true);
        h.state_mut().view.captured = true;
        h.run_steps(1);
        let chord = Modifiers {
            ctrl: true,
            alt: true,
            ..Default::default()
        };
        frame(&mut h, vec![key(Key::S, true, chord)], chord);
        frame(&mut h, vec![key(Key::S, false, chord)], Modifiers::NONE);
        let v = &h.state().view;
        assert!(v.stats, "the overlay is on");
        assert!(
            v.captured && !v.bar_shown,
            "the mouse stays captured, no toolbar"
        );
        frame(&mut h, vec![], chord);
        frame(&mut h, vec![key(Key::F6, true, chord)], chord);
        frame(&mut h, vec![], Modifiers::NONE);
        let v = &h.state().view;
        assert!(!v.stats, "F6 toggles it too");
        assert!(v.captured && !v.bar_shown);
        // The host key alone still does what it did.
        frame(&mut h, vec![], chord);
        frame(&mut h, vec![], Modifiers::NONE);
        assert!(h.state().view.bar_shown && !h.state().view.captured);
    }

    #[test]
    fn the_overlay_counts_frames_drawn_and_never_drawn() {
        let mut p = Presented::default();
        let t = Instant::now();
        p.frame(t, 1, Some(t));
        for i in 1..=10u64 {
            // Every other picture is replaced before a frame draws it.
            let now = t + Duration::from_millis(i * 100);
            p.frame(now, 1 + i * 2, Some(now - Duration::from_millis(4)));
        }
        assert!((p.fps - 11.0).abs() < 0.01, "{}", p.fps);
        assert_eq!(p.not_shown, 10);
        assert!((p.wait_ms - 40.0 / 11.0).abs() < 0.01, "{}", p.wait_ms);
    }

    /// The overlay over a real stream: `LATCH_TEST_SERVE=127.0.0.1:47883`
    /// names a test host (`serve` with `LATCH_TEST_PATTERN=sweep`), and
    /// `LATCH_TEST_REQUEST=1280x720@300` what to ask it for. Renders the
    /// overlay after a few seconds of real statistics.
    #[test]
    #[ignore = "needs a test host and a GPU"]
    fn snapshot_live_stats_overlay() {
        use crate::app::snapshots::{file_name, save};
        let host: std::net::SocketAddr = std::env::var("LATCH_TEST_SERVE")
            .expect("LATCH_TEST_SERVE")
            .parse()
            .unwrap();
        let request = std::env::var("LATCH_TEST_REQUEST").unwrap_or("1920x1080@120".into());
        let (size, fps) = request.split_once('@').unwrap();
        let (w, h) = size.split_once('x').unwrap();
        let requested = (w.parse().unwrap(), h.parse().unwrap(), fps.parse().unwrap());
        let ctx = egui::Context::default();
        let live = live_to(
            &ctx,
            latch_stream::session::Server {
                address: host.ip(),
                port: host.port(),
            },
            requested,
        );
        let mut fixture = fixture(
            View {
                stats: true,
                ..View::default()
            },
            Extra::default(),
        );
        fixture.live = Some(live);
        let size = Vec2::new(1280.0, 800.0);
        let mut h = harness_with(size, 2.0, true, fixture);
        let until = Instant::now() + Duration::from_secs(5);
        while Instant::now() < until {
            h.run_steps(1);
            std::thread::sleep(Duration::from_millis(4));
        }
        let stats = h.state().live.as_ref().unwrap().session.stats();
        eprintln!("live stats: {stats:#?}");
        eprintln!("presented: {:?}", h.state().view.presented);
        save(
            h.render().unwrap(),
            &file_name("stream", "stats-overlay-live", size, 2.0),
        );
    }

    /// A 1440p HEVC stream at 120 fps on a good network.
    fn sample_stats() -> latch_stream::Stats {
        latch_stream::Stats {
            width: 2560,
            height: 1440,
            fps: 118.0,
            received_fps: 119.0,
            mbps: 41.5,
            rtt_ms: 12,
            rtt_var_ms: 2,
            frame_jitter_ms: 0.8,
            raw_loss_pct: 1.2,
            loss_pct: 0.0,
            frames_dropped: 0,
            decode_ms: 1.5,
            host_ms: 4.0,
            assembly_ms: 1.0,
            queue_ms: 0.5,
            codec: "HEVC",
            decoder: "VideoToolbox",
            applied_bitrate_kbps: 120_000,
            ..Default::default()
        }
    }

    #[test]
    fn the_overlay_lists_what_gfn_does_and_flags_trouble() {
        let ctx = egui::Context::default();
        let live = dummy_live(&ctx);
        let stats = latch_stream::Stats {
            width: 2560,
            height: 1440,
            fps: 118.0,
            received_fps: 119.0,
            mbps: 41.5,
            rtt_ms: 12,
            rtt_var_ms: 2,
            frame_jitter_ms: 0.8,
            raw_loss_pct: 1.2,
            loss_pct: 0.0,
            frames_dropped: 0,
            decode_ms: 1.5,
            host_ms: 4.0,
            assembly_ms: 1.0,
            queue_ms: 0.5,
            codec: "HEVC",
            decoder: "VideoToolbox",
            ..Default::default()
        };
        let p = Presented {
            fps: 117.0,
            wait_ms: 2.0,
            hz: 120,
            ..Default::default()
        };
        let rows = overlay_rows(&live, &stats, &p);
        let labels: Vec<_> = rows.iter().map(|r| r.label).collect();
        for want in [
            "Resolution",
            "Codec",
            "Frame rate",
            "Bitrate",
            "Round trip",
            "Jitter",
            "Packet loss",
            "Dropped",
            "Decode",
            "Present",
            "Latency",
        ] {
            assert!(labels.contains(&want), "{want} in {labels:?}");
        }
        let value = |label: &str| rows.iter().find(|r| r.label == label).unwrap();
        assert_eq!(value("Resolution").value, "2560 × 1440");
        assert_eq!(value("Packet loss").value, "1.20% · 0.00% after FEC");
        assert!(!value("Packet loss").warn, "FEC rebuilt it all");
        // 4 host + 6 + 1 network + 0.5 + 1.5 decode + 2 + 4.2 display.
        assert_eq!(value("Latency").value, "≈ 19 ms");
        let lossy = latch_stream::Stats {
            loss_pct: 0.5,
            frames_lost: 2,
            ..stats
        };
        let rows = overlay_rows(&live, &lossy, &p);
        assert!(rows.iter().filter(|r| r.warn).count() >= 2);
    }

    #[test]
    fn the_toolbar_fits_every_width_between_the_minimum_and_full_hd() {
        for w in [640.0, 800.0, 1039.0, 1040.0, 1100.0, 1299.0, 1300.0, 1920.0] {
            let h = harness(Vec2::new(w, 600.0));
            assert_no_overlap(&bar_control_rects(&h));
            crate::app::snapshots::assert_fits(&h, w);
        }
    }

    #[test]
    fn a_hidden_toolbar_puts_nothing_over_the_picture() {
        let h = harness_with(
            Vec2::new(1280.0, 800.0),
            1.0,
            false,
            fixture(View::default(), Extra::default()),
        );
        assert!(h.query_by_label("Disconnect").is_none());
        assert!(h.query_by_label("Stream settings").is_none());
    }

    #[test]
    fn the_settings_panel_applies_or_cancels() {
        let mut view = view_with_bar();
        view.settings = Some(ClientConfig::default().stream);
        let mut h = harness_with(
            Vec2::new(640.0, 420.0),
            1.0,
            false,
            fixture(view, Extra::default()),
        );
        assert!(h.query_by_label("Apply and reconnect").is_some());
        // The screen waiting for the first picture lies under the panel, on
        // a lower layer, with a Cancel of its own; `assert_fits` sees every
        // layer at once, so the panel is checked on its own.
        let screen = Rect::from_min_size(Pos2::ZERO, Vec2::new(640.0, 420.0));
        let panel = h
            .ctx
            .memory(|m| m.area_rect(Id::new("Stream settings")))
            .expect("the panel is showing");
        assert_inside("Stream settings", panel, below_bar(&h, screen));
        for control in ["Apply and reconnect", "Close Stream settings"] {
            assert_inside(control, node_rect(&h.get_by_label(control)), panel);
        }
        assert_cancel_inside(&h, panel);
        h.get_by_label("Close Stream settings").click();
        h.run_steps(2);
        assert!(h.state().view.settings.is_none());
    }

    fn node_rect(node: &egui_kittest::kittest::Node<'_>) -> Rect {
        let b = node.raw_bounds().expect("node has bounds");
        Rect::from_min_max(
            Pos2::new(b.x0 as f32, b.y0 as f32),
            Pos2::new(b.x1 as f32, b.y1 as f32),
        )
    }

    /// Press Tab until the control called `label` has keyboard focus.
    pub(crate) fn tab_to<S>(h: &mut egui_kittest::Harness<'_, S>, label: &str) {
        for _ in 0..60 {
            if h.query_by_label(label).is_some_and(|n| n.is_focused()) {
                h.run_steps(2);
                return;
            }
            h.press_key(egui::Key::Tab);
            h.run_steps(1);
        }
        panic!("Tab never reached {label:?}");
    }

    /// `inner` lies inside `outer`, give or take half a point of rounding.
    pub(crate) fn assert_inside(what: &str, inner: Rect, outer: Rect) {
        assert!(
            outer.expand(0.5).contains_rect(inner),
            "{what} {inner:?} runs outside {outer:?}"
        );
    }

    /// Open the menu behind `trigger` and check the whole menu is on
    /// screen and that Tab brings its last item, `last`, into view.
    pub(crate) fn assert_menu_fits<S>(
        h: &mut egui_kittest::Harness<'_, S>,
        size: Vec2,
        trigger: &str,
        last: &str,
    ) {
        let screen = Rect::from_min_size(Pos2::ZERO, size);
        h.get_by_label(trigger).simulate_click();
        h.run_steps(3);
        let menu = latch_ui::open_menu_rect(&h.ctx).expect("the menu is open");
        assert_inside(&format!("{trigger} menu at {size:?}"), menu, screen);
        tab_to(h, last);
        let menu = latch_ui::open_menu_rect(&h.ctx).expect("Tab keeps the menu open");
        assert_inside(&format!("{trigger} menu after Tab"), menu, screen);
        let item = node_rect(&h.get_by_label(last));
        assert_inside(&format!("{last:?} in the {trigger} menu"), item, menu);
    }

    fn windows_old_host() -> Extra {
        Extra {
            os: "windows".into(),
            old_host: Some((Version::new(3, 0, 0), Some(Version::new(4, 0, 2)))),
            power_allowed: true,
            can_sleep: true,
            ..Default::default()
        }
    }

    /// The last item each toolbar menu can show: the power items end the
    /// More menu, and with an old host the update item ends them.
    const LAST_POWER_ITEM: &str = "Update Latch Host… (runs 3.0.0)";

    #[test]
    fn every_toolbar_menu_fits_the_smallest_window_and_scrolls_to_its_end() {
        let min = Vec2::new(640.0, 420.0);
        let mut h = harness_with(
            min,
            1.0,
            false,
            fixture(view_with_bar(), windows_old_host()),
        );
        assert_menu_fits(&mut h, min, "More", LAST_POWER_ITEM);
        // The separate menus exist from this width up; at the minimum
        // height they must fit too.
        let short = Vec2::new(OVERFLOW_BELOW, 420.0);
        let last_key = if cfg!(target_os = "macos") {
            "Command acts as Ctrl"
        } else {
            "Print Screen"
        };
        for (trigger, last) in [
            ("Keys", last_key),
            ("Mouse", "Free, for desktops (follows this cursor)"),
            ("PC", LAST_POWER_ITEM),
        ] {
            let mut h = harness_with(
                short,
                1.0,
                false,
                fixture(view_with_bar(), windows_old_host()),
            );
            assert_menu_fits(&mut h, short, trigger, last);
        }
    }

    #[test]
    fn the_stream_panels_and_questions_fit_the_smallest_window() {
        let min = Vec2::new(640.0, 420.0);
        let screen = Rect::from_min_size(Pos2::ZERO, min);
        let area = |h: &egui_kittest::Harness<'_, Fixture>, id: &str| {
            h.ctx
                .memory(|m| m.area_rect(Id::new(id)))
                .unwrap_or_else(|| panic!("{id} is showing"))
        };

        let mut view = view_with_bar();
        view.details = true;
        let mut h = harness_with(min, 1.0, false, fixture(view, windows_old_host()));
        tab_to(&mut h, "Copy diagnostics");
        let below = below_bar(&h, screen);
        assert_inside("Stream performance", area(&h, "Stream performance"), below);
        let copy = node_rect(&h.get_by_label("Copy diagnostics"));
        assert_inside("Copy diagnostics", copy, area(&h, "Stream performance"));

        let mut view = view_with_bar();
        view.settings = Some(ClientConfig::default().stream);
        let mut h = harness_with(min, 1.0, false, fixture(view, windows_old_host()));
        tab_to(&mut h, "H.264");
        let panel = area(&h, "Stream settings");
        assert_inside("Stream settings", panel, below_bar(&h, screen));
        for control in ["H.264", "Apply and reconnect"] {
            assert_inside(control, node_rect(&h.get_by_label(control)), panel);
        }
        assert_cancel_inside(&h, panel);

        for (id, view, action) in [
            (
                "stream-confirm",
                View {
                    confirm: Some(PowerAction::Shutdown),
                    ..view_with_bar()
                },
                PowerAction::Shutdown.label(),
            ),
            (
                "stream-install",
                View {
                    confirm_install: true,
                    ..view_with_bar()
                },
                "Install through the stream",
            ),
        ] {
            let mut h = harness_with(min, 1.0, false, fixture(view, windows_old_host()));
            tab_to(&mut h, action);
            assert_inside(id, area(&h, id), below_bar(&h, screen));
            assert_inside(action, node_rect(&h.get_by_label(action)), area(&h, id));
            assert_cancel_inside(&h, area(&h, id));
        }
    }

    /// The window below the toolbar: where panels and questions go.
    fn below_bar(h: &egui_kittest::Harness<'_, Fixture>, screen: Rect) -> Rect {
        Rect::from_min_max(Pos2::new(0.0, h.state().view.bar_h), screen.max)
    }

    /// The Cancel that ends a panel or question lies inside it. (The
    /// screen waiting for the first picture has a Cancel of its own.)
    fn assert_cancel_inside(h: &egui_kittest::Harness<'_, Fixture>, area: Rect) {
        assert!(
            h.query_all_by_label("Cancel")
                .any(|n| area.expand(0.5).contains_rect(node_rect(&n))),
            "no Cancel inside {area:?}"
        );
    }

    #[test]
    #[ignore = "renders with a GPU; run on demand to review the UI"]
    fn snapshots_stream_menus_at_the_smallest_size() {
        use crate::app::snapshots::{file_name, save};
        let min = Vec2::new(640.0, 420.0);
        let short = Vec2::new(OVERFLOW_BELOW, 420.0);
        let last_key = if cfg!(target_os = "macos") {
            "Command acts as Ctrl"
        } else {
            "Print Screen"
        };
        for ppp in [1.0, 2.0] {
            for (name, size, trigger, last) in [
                ("more-end", min, "More", LAST_POWER_ITEM),
                ("keys-end", short, "Keys", last_key),
                (
                    "mouse-end",
                    short,
                    "Mouse",
                    "Free, for desktops (follows this cursor)",
                ),
                ("power-end", short, "PC", LAST_POWER_ITEM),
            ] {
                let mut h = harness_with(
                    size,
                    ppp,
                    true,
                    fixture(view_with_bar(), windows_old_host()),
                );
                assert_menu_fits(&mut h, size, trigger, last);
                save(h.render().unwrap(), &file_name("stream", name, size, ppp));
            }
            for (name, view, last) in [
                (
                    "stats-end",
                    View {
                        stats: true,
                        ..view_with_bar()
                    },
                    "Copy diagnostics",
                ),
                (
                    "settings-end",
                    View {
                        settings: Some(ClientConfig::default().stream),
                        ..view_with_bar()
                    },
                    "H.264",
                ),
                (
                    "confirm-shutdown",
                    View {
                        confirm: Some(PowerAction::Shutdown),
                        ..view_with_bar()
                    },
                    PowerAction::Shutdown.label(),
                ),
                (
                    "install-host",
                    View {
                        confirm_install: true,
                        ..view_with_bar()
                    },
                    "Install through the stream",
                ),
            ] {
                let mut h = harness_with(min, ppp, true, fixture(view, windows_old_host()));
                tab_to(&mut h, last);
                save(h.render().unwrap(), &file_name("stream", name, min, ppp));
            }
            // A dropdown in the settings panel opens where it has room.
            let view = View {
                settings: Some(ClientConfig::default().stream),
                ..view_with_bar()
            };
            let mut h = harness_with(min, ppp, true, fixture(view, windows_old_host()));
            // The frame rate select comes just before the bitrate slider.
            use egui::accesskit::Role;
            use egui_kittest::kittest::By;
            let focused = |h: &egui_kittest::Harness<'_, Fixture>, role: Role| {
                h.query_all(By::new().predicate(|n| n.is_focused()))
                    .any(|n| n.role() == role)
            };
            for _ in 0..30 {
                if focused(&h, Role::Slider) {
                    break;
                }
                h.press_key(egui::Key::Tab);
                h.run_steps(1);
            }
            h.press_key_modifiers(egui::Modifiers::SHIFT, egui::Key::Tab);
            h.run_steps(2);
            assert!(focused(&h, Role::ComboBox), "Shift-Tab reaches the select");
            h.query_all(By::new().predicate(|n| n.is_focused()))
                .next()
                .unwrap()
                .simulate_click();
            h.run_steps(3);
            let screen = Rect::from_min_size(Pos2::ZERO, min);
            let list = latch_ui::open_menu_rect(&h.ctx).expect("the list is open");
            assert_inside("the frame rate list", list, screen);
            tab_to(&mut h, "240 fps");
            let list = latch_ui::open_menu_rect(&h.ctx).expect("Tab keeps the list open");
            assert_inside("240 fps", node_rect(&h.get_by_label("240 fps")), list);
            save(
                h.render().unwrap(),
                &file_name("stream", "fps-open", min, ppp),
            );
            // The resolution list, the first select in the panel, reaches 4K.
            let view = View {
                settings: Some(ClientConfig::default().stream),
                ..view_with_bar()
            };
            let mut h = harness_with(min, ppp, true, fixture(view, windows_old_host()));
            for _ in 0..30 {
                if focused(&h, Role::ComboBox) {
                    break;
                }
                h.press_key(egui::Key::Tab);
                h.run_steps(1);
            }
            assert!(focused(&h, Role::ComboBox), "Tab reaches the select");
            h.query_all(By::new().predicate(|n| n.is_focused()))
                .next()
                .unwrap()
                .simulate_click();
            h.run_steps(3);
            let list = latch_ui::open_menu_rect(&h.ctx).expect("the list is open");
            assert_inside("the resolution list", list, screen);
            let last = "4K · 3840 × 2160";
            tab_to(&mut h, last);
            let list = latch_ui::open_menu_rect(&h.ctx).expect("Tab keeps the list open");
            assert_inside(last, node_rect(&h.get_by_label(last)), list);
            save(
                h.render().unwrap(),
                &file_name("stream", "resolution-open", min, ppp),
            );
        }
    }

    #[test]
    #[ignore = "renders with a GPU; run on demand to review the UI"]
    fn snapshots_stream_overlay() {
        use crate::app::snapshots::{file_name, save, MATRIX, PAIR};
        let windows = || Extra {
            os: "windows".into(),
            power_allowed: true,
            can_sleep: true,
            ..Default::default()
        };
        for &(size, ppp) in &MATRIX {
            let mut h = harness_with(size, ppp, true, fixture(view_with_bar(), windows()));
            save(
                h.render().unwrap(),
                &file_name("stream", "toolbar", size, ppp),
            );
        }
        let shots: Vec<(&str, View, Extra, Option<&str>)> = vec![
            ("waiting", View::default(), windows(), None),
            (
                "hint",
                View {
                    hint_since: Some(Instant::now()),
                    ..View::default()
                },
                windows(),
                None,
            ),
            ("more-open", view_with_bar(), windows(), Some("More")),
            ("keys-open", view_with_bar(), windows(), Some("Keys")),
            ("mouse-open", view_with_bar(), windows(), Some("Mouse")),
            (
                "power-open",
                view_with_bar(),
                Extra {
                    old_host: Some((Version::new(3, 0, 0), Some(Version::new(4, 0, 2)))),
                    ..windows()
                },
                Some("PC"),
            ),
            (
                "confirm-restart",
                View {
                    confirm: Some(PowerAction::Restart),
                    ..view_with_bar()
                },
                windows(),
                None,
            ),
            (
                "install-host",
                View {
                    confirm_install: true,
                    ..view_with_bar()
                },
                Extra {
                    old_host: Some((Version::new(3, 0, 0), Some(Version::new(4, 0, 2)))),
                    ..windows()
                },
                None,
            ),
            (
                "settings",
                View {
                    settings: Some(ClientConfig::default().stream),
                    ..View::default()
                },
                windows(),
                None,
            ),
            (
                "stats",
                View {
                    stats: true,
                    ..view_with_bar()
                },
                windows(),
                None,
            ),
            (
                "stats-overlay",
                View {
                    stats: true,
                    fake_stats: Some(sample_stats()),
                    ..View::default()
                },
                windows(),
                None,
            ),
            (
                "stats-overlay-bar",
                View {
                    stats: true,
                    fake_stats: Some(sample_stats()),
                    ..view_with_bar()
                },
                windows(),
                None,
            ),
            (
                "black-picture",
                View {
                    problem: Some("The PC is sending a black picture: the connection and the video are healthy, but every frame is blank. Asking the PC why…".into()),
                    ..View::default()
                },
                Extra {
                    video_help: Some(crate::display::Help {
                        message: "The PC's desktop is composed in HDR. The capture converts it to an ordinary picture, and on this PC that conversion comes out black. Turning HDR off fixes it.".into(),
                        hdr_is_on: true,
                        mode: "HDR".into(),
                        busy: false,
                    }),
                    ..windows()
                },
                None,
            ),
            (
                "toasts",
                {
                    let mut v = View::default();
                    v.toast(Tone::Warning, "Gaming-PC has no sound to send: it reports “Unable to initialize audio capture”. Its Sharing page in Latch says more.");
                    v.set_poor(true);
                    v
                },
                Extra {
                    handover: Some("Installing Latch Host 4.1.1 on Gaming-PC: waiting for it to fetch the file…".into()),
                    ..windows()
                },
                None,
            ),
        ];
        for (name, view, extra, open) in shots {
            let sizes: &[(Vec2, f32)] = if name == "more-open" {
                &[
                    (Vec2::new(640.0, 420.0), 2.0),
                    (Vec2::new(960.0, 640.0), 2.0),
                ]
            } else if name == "settings" {
                &PAIR
            } else {
                &[(Vec2::new(1280.0, 800.0), 2.0)]
            };
            for &(size, ppp) in sizes {
                let view = View {
                    settings: view.settings.clone(),
                    stats: view.stats,
                    bar_shown: view.bar_shown,
                    confirm: view.confirm,
                    confirm_install: view.confirm_install,
                    hint_since: view.hint_since,
                    toasts: view
                        .toasts
                        .iter()
                        .map(|t| Toast {
                            tone: t.tone,
                            text: t.text.clone(),
                            at: t.at,
                        })
                        .collect(),
                    poor: view.poor,
                    poor_hinted: view.poor_hinted,
                    problem: view.problem.clone(),
                    fake_stats: view.fake_stats.clone(),
                    ..View::default()
                };
                let overlay = view.fake_stats.is_some();
                let mut h = harness_with(size, ppp, true, fixture(view, extra.clone()));
                if overlay {
                    let mut frame = latch_stream::video::Frame::default();
                    let y: Vec<u8> = (0..64 * 64).map(|i| (40 + (i % 64) * 2) as u8).collect();
                    frame.fill((64, 64), (&y, 64), (&[128; 64 * 32], 64), false);
                    h.state().live.as_ref().unwrap().frames.publish(frame);
                    h.run_steps(3);
                }
                if let Some(label) = open {
                    h.get_by_label(label).simulate_click();
                    h.run_steps(3);
                }
                save(h.render().unwrap(), &file_name("stream", name, size, ppp));
            }
        }
    }

    #[test]
    #[ignore = "renders changed streaming UI text with a GPU"]
    fn snapshots_stream_migration() {
        use crate::app::snapshots::{file_name, save};
        for (name, view) in [
            ("toolbar", view_with_bar()),
            ("stats", View { stats: true, ..view_with_bar() }),
            ("black-picture", View { problem: Some("The PC is sending a black picture: the connection and the video are healthy, but every frame is blank. Asking the PC why…".into()), ..View::default() }),
        ] {
            let size = Vec2::new(1280.0, 800.0);
            let mut h = harness_with(size, 1.0, true, fixture(view, Extra { os: "windows".into(), ..Default::default() }));
            save(h.render().unwrap(), &file_name("migration-stream", name, size, 1.0));
        }
    }

    // What the PC is sent: the order, the signs and the masks.

    use crate::input::{
        Recorder, Sent, VK_CAPITAL, VK_CONTROL, VK_LWIN, VK_MENU, VK_RETURN, VK_SHIFT, VK_TAB, VK_V,
    };
    use egui::{Key, PointerButton};
    use latch_stream::input::{BUTTON_LEFT, MOD_CTRL, MOD_SHIFT};
    use std::rc::Rc;

    const VK_T: i16 = 0x54;
    const VK_W: i16 = 0x57;
    const VK_Y: i16 = 0x59;
    const VK_Z: i16 = 0x5A;
    const CMD: Modifiers = Modifiers {
        mac_cmd: true,
        command: true,
        ..Modifiers::NONE
    };
    const CTRL_ALT: Modifiers = Modifiers {
        ctrl: true,
        alt: true,
        ..Modifiers::NONE
    };
    /// On the picture, clear of the card shown before the first picture.
    const ON_PICTURE: Pos2 = Pos2::new(20.0, 200.0);

    /// A stream view at the smallest window whose input goes to a
    /// recording instead of the PC, with the other machine running `os`.
    fn recording_on(
        view: View,
        connected: bool,
        os: &str,
    ) -> (egui_kittest::Harness<'static, Fixture>, Rc<Recorder>) {
        let tap = Rc::new(Recorder::default());
        tap.connected.set(connected);
        let view = View {
            tap: Some(tap.clone()),
            ..view
        };
        let extra = Extra {
            os: os.into(),
            ..Default::default()
        };
        let h = harness_with(Vec2::new(640.0, 420.0), 1.0, false, fixture(view, extra));
        tap.take();
        (h, tap)
    }

    fn recording(
        view: View,
        connected: bool,
    ) -> (egui_kittest::Harness<'static, Fixture>, Rc<Recorder>) {
        recording_on(view, connected, "windows")
    }

    fn key(key: Key, pressed: bool, modifiers: Modifiers) -> Event {
        Event::Key {
            key,
            physical_key: Some(key),
            pressed,
            repeat: false,
            modifiers,
        }
    }

    fn click(pos: Pos2, pressed: bool) -> Event {
        Event::PointerButton {
            pos,
            button: PointerButton::Primary,
            pressed,
            modifiers: Modifiers::NONE,
        }
    }

    /// One frame with these events, ending with `end` held.
    fn frame(h: &mut egui_kittest::Harness<'_, Fixture>, events: Vec<Event>, end: Modifiers) {
        h.input_mut().events.extend(events);
        h.input_mut().modifiers = end;
        h.run_steps(1);
    }

    #[test]
    fn caps_lock_follows_the_mac_once_the_stream_has_the_keys() {
        // Unknown on the Mac: nothing to do.
        assert_eq!(caps_tap(None, None, true), (None, false));
        // Not the stream's keys yet: nothing is taken as matching.
        assert_eq!(caps_tap(None, Some(true), false), (None, false));
        // The first state seen with the keys is the baseline.
        assert_eq!(caps_tap(None, Some(true), true), (Some(true), false));
        assert_eq!(caps_tap(Some(true), Some(true), true), (Some(true), false));
        // A change is one tap; one made while Latch had the keys waits.
        assert_eq!(caps_tap(Some(true), Some(false), true), (Some(false), true));
        assert_eq!(
            caps_tap(Some(false), Some(true), false),
            (Some(false), false)
        );
        assert_eq!(caps_tap(Some(false), Some(true), true), (Some(true), true));
    }

    #[test]
    fn caps_lock_on_the_mac_taps_caps_lock_on_the_pc() {
        let (mut h, tap) = recording(View::default(), true);
        h.state_mut().view.caps_now = Some(false);
        frame(&mut h, vec![], Modifiers::NONE);
        assert_eq!(tap.keys(), vec![], "the first state is the baseline");
        h.state_mut().view.caps_now = Some(true);
        frame(
            &mut h,
            vec![
                key(Key::A, true, Modifiers::NONE),
                key(Key::A, false, Modifiers::NONE),
            ],
            Modifiers::NONE,
        );
        assert_eq!(
            tap.keys(),
            vec![
                (VK_CAPITAL, true),
                (VK_CAPITAL, false),
                (0x41, true),
                (0x41, false)
            ],
            "Caps Lock goes before the frame's keys"
        );
        frame(&mut h, vec![], Modifiers::NONE);
        assert_eq!(tap.keys(), vec![], "once per change");
    }

    #[test]
    fn a_shift_let_go_in_the_same_frame_still_capitalises() {
        let (mut h, tap) = recording(View::default(), true);
        frame(
            &mut h,
            vec![
                key(Key::T, true, Modifiers::SHIFT),
                key(Key::T, false, Modifiers::SHIFT),
            ],
            Modifiers::NONE,
        );
        assert_eq!(
            tap.take(),
            [
                Sent::Key(VK_SHIFT, true, MOD_SHIFT),
                Sent::Key(VK_T, true, MOD_SHIFT),
                Sent::Key(VK_T, false, MOD_SHIFT),
                Sent::Key(VK_SHIFT, false, 0),
            ]
        );
        // A key pressed before Shift in the same frame stays lowercase.
        frame(
            &mut h,
            vec![key(Key::Y, true, Modifiers::NONE)],
            Modifiers::SHIFT,
        );
        assert_eq!(tap.keys(), [(VK_Y, true), (VK_SHIFT, true)]);
    }

    #[test]
    fn command_z_with_command_let_go_in_the_same_frame_is_still_undo() {
        let (mut h, tap) = recording(View::default(), true);
        frame(&mut h, vec![key(Key::Z, true, CMD)], Modifiers::NONE);
        assert_eq!(
            tap.take(),
            [
                Sent::Key(VK_CONTROL, true, MOD_CTRL),
                Sent::Key(VK_Z, true, MOD_CTRL),
                Sent::Key(VK_CONTROL, false, 0),
            ]
        );
    }

    #[test]
    fn a_mac_there_gets_command_as_command() {
        let (mut h, tap) = recording_on(View::default(), true, "macOS");
        assert!(h.state().cfg.cmd_is_ctrl, "the default, for Windows");
        frame(&mut h, vec![key(Key::Z, true, CMD)], CMD);
        assert_eq!(tap.keys(), [(VK_LWIN, true), (VK_Z, true)]);
        // Copy is the Command chord there too.
        frame(&mut h, vec![Event::Copy], CMD);
        assert_eq!(tap.keys(), [(input::VK_C, true), (input::VK_C, false)]);
        assert!(is_mac_os("macOS") && is_mac_os("darwin") && is_mac_os("MacOS"));
        assert!(!is_mac_os("windows") && !is_mac_os("linux") && !is_mac_os(""));
    }

    #[test]
    fn command_plus_zooms_the_pc_not_latch() {
        let (mut h, tap) = recording(View::default(), true);
        frame(&mut h, vec![key(Key::Equals, true, CMD)], CMD);
        // A new zoom takes effect on the next frame.
        h.run_steps(1);
        assert_eq!(h.ctx.zoom_factor(), 1.0);
        assert!(tap.keys().contains(&(0xBB, true)));
        // Without the stream's keys, egui's zoom is back.
        h.state_mut().view.settings = Some(ClientConfig::default().stream);
        h.run_steps(1);
        frame(&mut h, vec![key(Key::Equals, true, CMD)], CMD);
        h.run_steps(1);
        assert!(h.ctx.zoom_factor() > 1.0);
    }

    #[test]
    fn the_host_key_never_reaches_the_pc_even_inside_one_frame() {
        let (mut h, tap) = recording(View::default(), true);
        // Ctrl+Alt+T pressed and Alt let go before the frame was drawn.
        frame(&mut h, vec![key(Key::T, true, CTRL_ALT)], Modifiers::CTRL);
        let keys = tap.keys();
        assert!(!keys.contains(&(VK_T, true)), "{keys:?}");
        assert!(!keys.iter().any(|&(k, _)| k == VK_MENU), "{keys:?}");
    }

    #[test]
    fn scrolling_sideways_scrolls_the_pc_the_same_way() {
        use egui::MouseWheelUnit::{Line, Page, Point};
        assert_eq!(wheel_to_host(Line, Vec2::new(1.0, 0.0)), (-120.0, 0.0));
        assert_eq!(wheel_to_host(Line, Vec2::new(0.0, 1.0)), (0.0, 120.0));
        assert_eq!(wheel_to_host(Point, Vec2::new(-2.0, -2.0)), (6.0, -6.0));
        assert_eq!(wheel_to_host(Page, Vec2::new(0.5, 0.0)), (-600.0, 0.0));
        let (mut h, tap) = recording(View::default(), true);
        frame(
            &mut h,
            vec![
                Event::PointerMoved(ON_PICTURE),
                Event::MouseWheel {
                    unit: Line,
                    delta: Vec2::new(1.0, 0.0),
                    modifiers: Modifiers::NONE,
                },
            ],
            Modifiers::NONE,
        );
        let sent = tap.take();
        assert!(sent.contains(&Sent::Scroll(0, -120)), "{sent:?}");
    }

    #[test]
    fn a_click_goes_where_the_pointer_was_when_it_was_made() {
        let (mut h, tap) = recording(View::default(), true);
        h.state_mut().cfg.capture_mouse = false;
        // 1920×1080 in 640×420: the picture is 640×360, 30 points down.
        let elsewhere = Pos2::new(300.0, 100.0);
        frame(
            &mut h,
            vec![
                Event::PointerMoved(ON_PICTURE),
                click(ON_PICTURE, true),
                Event::PointerMoved(elsewhere),
                click(elsewhere, false),
            ],
            Modifiers::NONE,
        );
        assert_eq!(
            tap.take(),
            [
                Sent::Position(20, 170, 640, 360),
                Sent::Button(BUTTON_LEFT, true),
                Sent::Position(300, 70, 640, 360),
                Sent::Button(BUTTON_LEFT, false),
            ]
        );
        assert!(!h.state().view.captured, "free mode");
    }

    #[test]
    fn a_click_that_recaptures_the_mouse_preserves_modifiers() {
        let (mut h, tap) = recording(View::default(), true);
        h.state_mut().cfg.capture_mouse = true;
        assert!(!h.state().view.captured);

        let shift = Modifiers {
            shift: true,
            ..Modifiers::NONE
        };
        frame(
            &mut h,
            vec![Event::PointerButton {
                pos: ON_PICTURE,
                button: PointerButton::Primary,
                pressed: true,
                modifiers: shift,
            }],
            shift,
        );

        let sent = tap.take();
        assert!(
            h.state().view.captured,
            "mouse must be captured after click"
        );
        let shift_down_pos = sent
            .iter()
            .position(|s| matches!(s, Sent::Key(k, true, _) if *k == input::VK_SHIFT));
        let button_down_pos = sent
            .iter()
            .position(|s| matches!(s, Sent::Button(BUTTON_LEFT, true)));
        assert!(
            shift_down_pos.is_some(),
            "shift down must be sent: {sent:?}"
        );
        assert!(
            button_down_pos.is_some(),
            "button down must be sent: {sent:?}"
        );
        assert!(
            shift_down_pos.unwrap() < button_down_pos.unwrap(),
            "shift down must precede button down: {sent:?}"
        );
        let shift_up_between = sent[shift_down_pos.unwrap()..button_down_pos.unwrap()]
            .iter()
            .any(|s| matches!(s, Sent::Key(k, false, _) if *k == input::VK_SHIFT));
        assert!(
            !shift_up_between,
            "shift must not be released before button down: {sent:?}"
        );
    }

    #[test]
    fn a_captured_click_comes_after_the_movement_before_it() {
        let view = View {
            captured: true,
            grabbed: true,
            ..View::default()
        };
        let (mut h, tap) = recording(view, true);
        // 1920 stream pixels across 640 points: three per point.
        frame(
            &mut h,
            vec![
                Event::MouseMoved(Vec2::new(3.5, 0.0)),
                click(ON_PICTURE, true),
                Event::MouseMoved(Vec2::new(1.0, 1.0)),
            ],
            Modifiers::NONE,
        );
        assert_eq!(
            tap.take(),
            [
                Sent::Motion(10, 0),
                Sent::Button(BUTTON_LEFT, true),
                Sent::Motion(3, 3),
            ]
        );
    }

    #[test]
    fn a_click_that_captures_the_mouse_sends_its_position_first() {
        let (mut h, tap) = recording(View::default(), true);
        frame(
            &mut h,
            vec![Event::PointerMoved(ON_PICTURE), click(ON_PICTURE, true)],
            Modifiers::NONE,
        );
        assert_eq!(
            tap.take(),
            [
                Sent::Position(20, 170, 640, 360),
                Sent::ReleaseAll,
                Sent::Button(BUTTON_LEFT, true),
            ]
        );
        assert!(h.state().view.captured);
    }

    #[test]
    fn a_click_before_the_stream_connects_does_not_take_the_mouse() {
        let (mut h, tap) = recording(View::default(), false);
        frame(
            &mut h,
            vec![
                Event::PointerMoved(ON_PICTURE),
                click(ON_PICTURE, true),
                click(ON_PICTURE, false),
            ],
            Modifiers::NONE,
        );
        h.run_steps(1);
        assert!(!h.state().view.captured && !h.state().view.grabbed);
        assert!(h.query_by_label("Cancel").is_some());
        // A capture left over from a dropped connection: the host key
        // still lets the mouse go.
        h.state_mut().view.captured = true;
        frame(&mut h, vec![], CTRL_ALT);
        assert!(!h.state().view.captured);
        let sent = tap.take();
        assert!(
            !sent.iter().any(|s| matches!(s, Sent::Button(..))),
            "{sent:?}"
        );
    }

    #[test]
    fn the_host_key_lets_go_of_what_was_held() {
        let (mut h, tap) = recording(view_with_bar(), true);
        h.state_mut().cfg.capture_mouse = false;
        frame(
            &mut h,
            vec![key(Key::W, true, Modifiers::NONE)],
            Modifiers::NONE,
        );
        assert_eq!(tap.keys(), [(VK_W, true)]);
        // The bar hides; W is let go while the chord is still down.
        frame(&mut h, vec![key(Key::W, false, CTRL_ALT)], CTRL_ALT);
        assert_eq!(tap.keys(), [(VK_W, false)]);
        assert!(!h.state().view.held.any_down());
        assert!(!h.state().view.bar_shown);
        // Control let go before Option: no lone Option reaches the PC.
        frame(&mut h, vec![], Modifiers::ALT);
        frame(&mut h, vec![], Modifiers::NONE);
        assert!(tap.keys().is_empty());
    }

    #[test]
    fn the_host_key_never_captures_the_mouse_under_an_open_panel() {
        let view = View {
            settings: Some(ClientConfig::default().stream),
            ..view_with_bar()
        };
        let (mut h, _tap) = recording(view, true);
        frame(&mut h, vec![], CTRL_ALT);
        let v = &h.state().view;
        assert!(v.captured && !v.bar_shown);
        assert!(v.settings.is_none(), "closed, as Cancel");
        let view = View {
            confirm: Some(PowerAction::Restart),
            ..view_with_bar()
        };
        let (mut h, _tap) = recording(view, true);
        frame(&mut h, vec![], CTRL_ALT);
        assert!(h.state().view.confirm.is_none() && h.state().view.captured);
    }

    #[test]
    fn a_paste_goes_before_what_was_typed_after_it() {
        // Nothing to send first (an image on the Mac): at once, in order.
        let (mut h, tap) = recording(View::default(), true);
        frame(
            &mut h,
            vec![
                Event::Paste(String::new()),
                key(Key::Enter, true, Modifiers::NONE),
            ],
            Modifiers::NONE,
        );
        assert_eq!(
            tap.keys(),
            [
                (VK_CONTROL, true),
                (VK_V, true),
                (VK_V, false),
                (VK_CONTROL, false),
                (VK_RETURN, true),
            ]
        );
        // Text on its way: Return waits for it, then follows the paste.
        let (mut h, tap) = recording(View::default(), true);
        frame(&mut h, vec![], CMD);
        assert_eq!(tap.keys(), [(VK_CONTROL, true)]);
        h.state_mut().view.pending = Some(PendingPaste {
            id: 1,
            until: Instant::now() + Duration::from_secs(60),
            queued: Vec::new(),
        });
        // ⌘ let go, then Return, while the text travels.
        frame(&mut h, vec![], Modifiers::NONE);
        frame(
            &mut h,
            vec![
                key(Key::Enter, true, Modifiers::NONE),
                key(Key::Enter, false, Modifiers::NONE),
            ],
            Modifiers::NONE,
        );
        assert!(tap.take().is_empty(), "nothing overtakes the paste");
        h.state()
            .live
            .as_ref()
            .unwrap()
            .clipboard
            .finish_for_test(1);
        frame(&mut h, vec![], Modifiers::NONE);
        assert_eq!(
            tap.keys(),
            [
                (VK_V, true),
                (VK_V, false),
                (VK_CONTROL, false),
                (VK_RETURN, true),
                (VK_RETURN, false),
            ]
        );
        assert!(h.state().view.pending.is_none());
    }

    #[test]
    fn a_paste_that_waits_too_long_goes_once_and_losing_focus_drops_it() {
        let (mut h, tap) = recording(View::default(), true);
        h.state_mut().view.pending = Some(PendingPaste {
            id: 7,
            until: Instant::now(),
            queued: vec![Queued::Event(
                key(Key::Enter, true, Modifiers::NONE),
                At {
                    keys_to_pc: true,
                    over_stream: false,
                },
            )],
        });
        frame(&mut h, vec![], Modifiers::NONE);
        assert_eq!(
            tap.keys(),
            [
                (VK_CONTROL, true),
                (VK_V, true),
                (VK_V, false),
                (VK_CONTROL, false),
                (VK_RETURN, true),
            ]
        );
        frame(&mut h, vec![], Modifiers::NONE);
        assert_eq!(tap.keys(), [], "and only once");
        // Focus goes while a paste waits: it goes, unsent, with what was
        // held behind it; what the PC holds is let go.
        h.state_mut().view.pending = Some(PendingPaste {
            id: 8,
            until: Instant::now() + Duration::from_secs(60),
            queued: Vec::new(),
        });
        frame(
            &mut h,
            vec![key(Key::A, true, Modifiers::NONE)],
            Modifiers::NONE,
        );
        frame(&mut h, vec![Event::WindowFocused(false)], Modifiers::NONE);
        assert!(h.state().view.pending.is_none());
        assert_eq!(tap.keys(), [(VK_RETURN, false)]);
    }

    #[test]
    fn command_v_pastes_even_with_no_text_on_the_mac() {
        // egui swallows ⌘V's press when the Mac's clipboard has no text;
        // only the V let go shows.
        let (mut h, tap) = recording(View::default(), true);
        frame(&mut h, vec![], CMD);
        assert_eq!(tap.keys(), [(VK_CONTROL, true)]);
        frame(&mut h, vec![key(Key::V, false, CMD)], CMD);
        assert_eq!(tap.keys(), [(VK_V, true), (VK_V, false)]);
        // With text, the paste event comes and the V let go adds nothing.
        frame(
            &mut h,
            vec![Event::Paste(String::new()), key(Key::V, false, CMD)],
            CMD,
        );
        assert_eq!(tap.keys(), [(VK_V, true), (VK_V, false)]);
        // A plain V is typed as ever.
        frame(&mut h, vec![], Modifiers::NONE);
        tap.take();
        frame(
            &mut h,
            vec![
                key(Key::V, true, Modifiers::NONE),
                key(Key::V, false, Modifiers::NONE),
            ],
            Modifiers::NONE,
        );
        assert_eq!(tap.keys(), [(VK_V, true), (VK_V, false)]);
        for (seen, pasted, cmd) in [
            (true, false, true),
            (false, true, true),
            (false, false, false),
        ] {
            assert!(!swallowed_paste(seen, pasted, cmd));
        }
        assert!(swallowed_paste(false, false, true));
    }

    #[test]
    fn tab_in_free_mode_goes_to_the_pc_with_the_stats_panel_open() {
        let view = View {
            stats: true,
            details: true,
            ..View::default()
        };
        let (mut h, tap) = recording(view, true);
        h.state_mut().cfg.capture_mouse = false;
        h.state()
            .live
            .as_ref()
            .unwrap()
            .frames
            .publish(latch_stream::video::Frame::default());
        h.run_steps(2);
        tap.take();
        for _ in 0..3 {
            h.press_key(Key::Tab);
            h.run_steps(1);
            assert!(h.ctx.memory(|m| m.focused()).is_none());
        }
        let tabs = tap
            .keys()
            .into_iter()
            .filter(|&k| k == (VK_TAB, true))
            .count();
        assert_eq!(tabs, 3);
        // Focus taken in the Stats panel while the toolbar showed goes
        // when the toolbar does.
        h.state_mut().view.bar_shown = true;
        h.run_steps(2);
        tab_to(&mut h, "Copy diagnostics");
        h.state_mut().view.bar_shown = false;
        h.run_steps(2);
        assert!(h.ctx.memory(|m| m.focused()).is_none());
    }

    #[test]
    fn review_physical_text_and_ime_are_not_typed_twice() {
        let (mut h, tap) = recording(View::default(), true);
        frame(
            &mut h,
            vec![
                key(Key::A, true, Modifiers::NONE),
                Event::Text("a".into()),
                key(Key::A, false, Modifiers::NONE),
            ],
            Modifiers::NONE,
        );
        assert!(
            !tap.take().iter().any(|s| matches!(s, Sent::Text(_))),
            "physical key already types a"
        );
        frame(
            &mut h,
            vec![
                Event::Ime(egui::ImeEvent::Commit("世界".into())),
                Event::Text("世界".into()),
            ],
            Modifiers::NONE,
        );
        assert_eq!(
            tap.take()
                .iter()
                .filter(|s| matches!(s, Sent::Text(_)))
                .count(),
            1
        );
    }

    #[test]
    fn review_composition_keys_stay_local_until_commit() {
        let (mut h, tap) = recording(View::default(), true);
        frame(
            &mut h,
            vec![Event::Ime(egui::ImeEvent::Preedit("n".into()))],
            Modifiers::NONE,
        );
        frame(
            &mut h,
            vec![
                key(Key::N, true, Modifiers::NONE),
                Event::Text("n".into()),
                key(Key::N, false, Modifiers::NONE),
            ],
            Modifiers::NONE,
        );
        assert!(!tap
            .take()
            .iter()
            .any(|s| matches!(s, Sent::Key(_, true, _) | Sent::Text(_))));
        frame(
            &mut h,
            vec![Event::Ime(egui::ImeEvent::Commit("你".into()))],
            Modifiers::NONE,
        );
        assert_eq!(
            tap.take()
                .iter()
                .filter(|s| matches!(s, Sent::Text(_)))
                .count(),
            1
        );
    }

    #[test]
    fn keys_after_a_commit_in_the_same_frame_reach_the_pc() {
        let (mut h, tap) = recording(View::default(), true);
        frame(
            &mut h,
            vec![
                Event::Ime(egui::ImeEvent::Commit("你".into())),
                key(Key::A, true, Modifiers::NONE),
                Event::Text("a".into()),
                key(Key::A, false, Modifiers::NONE),
            ],
            Modifiers::NONE,
        );
        let sent = tap.take();
        assert!(sent.contains(&Sent::Text("你".into())), "{sent:?}");
        assert!(sent.contains(&Sent::Key(0x41, true, 0)), "{sent:?}");
        assert!(sent.contains(&Sent::Key(0x41, false, 0)), "{sent:?}");
    }

    #[test]
    fn review_new_session_restores_mute() {
        let (mut h, _) = recording(
            View {
                muted: true,
                volume: 35,
                ..Default::default()
            },
            true,
        );
        let ctx = h.ctx.clone();
        let f = h.state_mut();
        f.view.stream_started(&ctx, f.live.as_ref().unwrap(), false);
        assert_eq!(f.live.as_ref().unwrap().session.volume(), 0.0);
    }

    #[test]
    fn text_and_ime_commit_events_are_forwarded_to_pc() {
        let (mut h, tap) = recording(View::default(), true);
        frame(
            &mut h,
            vec![
                Event::Text("hello".into()),
                Event::Ime(egui::ImeEvent::Commit("世界".into())),
            ],
            Modifiers::NONE,
        );
        let sent = tap.take();
        assert!(
            sent.iter()
                .any(|s| matches!(s, Sent::Text(t) if t == "hello")),
            "Event::Text must be forwarded to PC: {sent:?}"
        );
        assert!(
            sent.iter()
                .any(|s| matches!(s, Sent::Text(t) if t == "世界")),
            "Event::Ime Commit must be forwarded to PC: {sent:?}"
        );
    }

    #[test]
    fn waiting_shows_troubleshooting_guidance_after_10_seconds() {
        let (mut h, _) = recording(View::default(), false);
        let live = h.state_mut().live.as_mut().unwrap();
        live.started = Instant::now() - Duration::from_secs(11);
        live.path = crate::path::Path {
            direct: Some(true),
            relay: String::new(),
            peer_relay: String::new(),
            rtt_ms: Some(25),
        };
        h.run_steps(1);
        assert!(
            h.query_by_label("Connecting is taking longer than expected.")
                .is_some(),
            "10s timeout guidance must be shown"
        );
        assert!(
            h.query_by_label_contains("signed in to Tailscale")
                .is_some(),
            "Tailscale sign-in advice must be shown"
        );
        assert!(
            h.query_by_label(
                "The connection is going through a Tailscale relay, which can be slow."
            )
            .is_none(),
            "relay warning should not be shown when direct"
        );

        // When the lobby/discovery path is relayed:
        let (mut h_relayed, _) = recording(View::default(), false);
        h_relayed.state_mut().live.as_mut().unwrap().started =
            Instant::now() - Duration::from_secs(11);
        h_relayed.state_mut().live.as_mut().unwrap().path = crate::path::Path {
            direct: Some(false),
            relay: "tok".into(),
            peer_relay: String::new(),
            rtt_ms: Some(150),
        };
        h_relayed.run_steps(1);
        assert!(
            h_relayed
                .query_by_label(
                    "The connection is going through a Tailscale relay, which can be slow."
                )
                .is_some(),
            "relay warning must be shown when relayed"
        );
    }

    #[test]
    fn disconnect_requires_confirmation_in_toolbar() {
        let mut h = harness(Vec2::new(1400.0, 860.0));
        assert!(h.query_by_label("Disconnect").is_some());
        assert!(h.query_by_label("Click again to disconnect").is_none());

        // First click requests confirmation
        h.get_by_label("Disconnect").simulate_click();
        h.run_steps(2);
        assert!(
            h.query_by_label("Click again to disconnect").is_some(),
            "Disconnect click must switch to Click again to disconnect"
        );
        assert!(h.state().view.confirm_disconnect.is_some());

        // Second click confirms and resets
        h.get_by_label("Click again to disconnect").simulate_click();
        h.run_steps(2);
        assert!(h.state().view.confirm_disconnect.is_none());

        // Armed state expires after 3 seconds:
        h.get_by_label("Disconnect").simulate_click();
        h.run_steps(2);
        assert!(h.query_by_label("Click again to disconnect").is_some());
        // Fast-forward armed timestamp by 4s without sleeping
        h.state_mut().view.confirm_disconnect = Some(Instant::now() - Duration::from_secs(4));
        h.run_steps(2);
        assert!(
            h.query_by_label("Click again to disconnect").is_none(),
            "armed state must expire back to Disconnect after 3s"
        );
        assert!(h.query_by_label("Disconnect").is_some());
    }

    #[test]
    fn volume_slider_and_mute_control_audio_gain() {
        let mut h = harness(Vec2::new(1400.0, 860.0));
        assert!(h.query_by_label("Mute").is_some());
        assert!(h.query_by_label("Volume").is_some());

        // Initial state is 100% volume, unmuted
        assert_eq!(h.state().view.volume, 100);
        assert!(!h.state().view.muted);
        assert!((h.state().live.as_ref().unwrap().session.volume() - 1.0).abs() < 1e-4);

        // Click Mute
        h.get_by_label("Mute").simulate_click();
        h.run_steps(2);
        assert!(h.state().view.muted);
        assert_eq!(h.state().live.as_ref().unwrap().session.volume(), 0.0);
        assert!(h.query_by_label("Unmute").is_some());

        // Click Unmute
        h.get_by_label("Unmute").simulate_click();
        h.run_steps(2);
        assert!(!h.state().view.muted);
        assert!((h.state().live.as_ref().unwrap().session.volume() - 1.0).abs() < 1e-4);
    }

    #[test]
    fn each_session_starts_with_the_host_pointer_off() {
        // The last session's reset leaves the host drawing; a new session
        // starts with it off, so a captured reconnect must ask again.
        let ctx = egui::Context::default();
        let live = dummy_live(&ctx);
        let mut view = View {
            host_cursor_drawn: true,
            ..Default::default()
        };
        view.stream_started(&ctx, &live, false);
        assert!(!view.host_cursor_drawn);
    }

    #[test]
    fn in_stream_reconnecting_overlay_appears_when_connection_interrupted() {
        let mut view = view_with_bar();
        let live = dummy_live(&egui::Context::default());
        live.frames.publish(latch_stream::video::Frame::default());
        assert!(live.frames.seq() > 0);

        // Using explicit interrupted state to avoid sleeping
        view.interrupted = Some(Interruption::reconnecting(Instant::now()));

        let mut h = harness_with(
            Vec2::new(1400.0, 860.0),
            1.0,
            false,
            fixture(view, Extra::default()),
        );
        h.state_mut().live = Some(live);
        h.run_steps(2);

        assert!(
            h.query_by_label("Connection lost").is_some(),
            "Connection lost overlay must appear when disconnected during stream"
        );
        assert!(h.query_by_label("Reconnect now").is_some());
        assert!(h.query_by_label("Back to machines").is_some());

        // Clicking Back to machines sends Disconnect
        h.get_by_label("Back to machines").simulate_click();
        h.run_steps(2);
        assert!(h.state().actions.contains(&Action::Disconnect));
    }

    #[test]
    fn review_countdown_dispatches_once_and_back_cancels_in_flight() {
        let (mut h, _) = recording(View::default(), false);
        h.state_mut().view.interrupted = Some(Interruption::Reconnecting(ReconnectState {
            attempt: 1,
            deadline: Instant::now() - Duration::from_secs(1),
            in_flight: false,
        }));
        h.run_steps(1);
        assert_eq!(
            h.state()
                .actions
                .iter()
                .filter(|a| **a == Action::Reconnect)
                .count(),
            1
        );
        h.state_mut().actions.clear();
        h.run_steps(1);
        assert!(!h.state().actions.contains(&Action::Reconnect));
        h.get_by_label("Reconnect now").simulate_click();
        h.run_steps(2);
        assert!(
            !h.state().actions.contains(&Action::Reconnect),
            "in-flight retry is disabled"
        );
        frame(&mut h, vec![Event::WindowFocused(false)], Modifiers::NONE);
        assert!(
            h.state().view.interrupted.is_some(),
            "focus loss must retain retry overlay"
        );
        h.get_by_label("Back to machines").simulate_click();
        h.run_steps(2);
        assert!(h.state().actions.contains(&Action::Disconnect));
        assert!(h.state().view.interrupted.is_none());
    }

    #[test]
    fn reconnect_state_transitions_countdown_and_attempts() {
        let mut view = View::default();
        let t0 = Instant::now();

        // 1. Initial interruption: attempt 1 scheduled after 2 s
        view.handle_interruption_at(t0);
        let Some(Interruption::Reconnecting(ref s1)) = view.interrupted else {
            panic!("expected Reconnecting state");
        };
        assert_eq!(s1.attempt, 1);
        assert_eq!(s1.deadline, t0 + Duration::from_secs(2));
        assert_eq!(s1.remaining_secs(t0), 2);
        assert_eq!(s1.remaining_secs(t0 + Duration::from_millis(1500)), 1);
        assert_eq!(s1.remaining_secs(t0 + Duration::from_secs(2)), 0);
        assert!(s1.is_due(t0 + Duration::from_secs(2)));

        // 2. Attempt 1 fails: attempt 2 scheduled after 5 s
        let t1 = t0 + Duration::from_secs(2);
        view.handle_interruption_at(t1);
        let Some(Interruption::Reconnecting(ref s2)) = view.interrupted else {
            panic!("expected Reconnecting state for attempt 2");
        };
        assert_eq!(s2.attempt, 2);
        assert_eq!(s2.deadline, t1 + Duration::from_secs(5));
        assert_eq!(s2.remaining_secs(t1), 5);
        assert_eq!(s2.remaining_secs(t1 + Duration::from_secs(5)), 0);
        assert!(s2.is_due(t1 + Duration::from_secs(5)));

        // 3. Attempt 2 fails: attempt 3 scheduled after 10 s
        let t2 = t1 + Duration::from_secs(5);
        view.handle_interruption_at(t2);
        let Some(Interruption::Reconnecting(ref s3)) = view.interrupted else {
            panic!("expected Reconnecting state for attempt 3");
        };
        assert_eq!(s3.attempt, 3);
        assert_eq!(s3.deadline, t2 + Duration::from_secs(10));
        assert_eq!(s3.remaining_secs(t2), 10);
        assert_eq!(s3.remaining_secs(t2 + Duration::from_secs(10)), 0);
        assert!(s3.is_due(t2 + Duration::from_secs(10)));

        // 4. Attempt 3 fails: exhausted retries transitions to Failed
        let t3 = t2 + Duration::from_secs(10);
        view.handle_interruption_at(t3);
        assert_eq!(view.interrupted, Some(Interruption::Failed));
    }

    #[test]
    fn reconnect_state_cancel_stops_retries() {
        let mut view = View::default();
        let t0 = Instant::now();
        view.handle_interruption_at(t0);
        assert!(view.interrupted.is_some());

        // Cancelling stops pending retries and clears state
        view.cancel_reconnect();
        assert!(view.interrupted.is_none());
    }

    #[test]
    fn reconnect_state_takeover_does_not_retry() {
        let mut view = View {
            interrupted: Some(Interruption::TakenOver("Another device connected".into())),
            ..Default::default()
        };

        // Interruption handler must not overwrite or retry takeover
        let t0 = Instant::now();
        view.handle_interruption_at(t0);
        assert_eq!(
            view.interrupted,
            Some(Interruption::TakenOver("Another device connected".into()))
        );
    }

    #[test]
    fn in_stream_reconnecting_overlay_countdown_and_exhaustion_ui() {
        let mut view = view_with_bar();
        let live = dummy_live(&egui::Context::default());
        live.frames.publish(latch_stream::video::Frame::default());

        let t0 = Instant::now();
        view.handle_interruption_at(t0);

        let mut h = harness_with(
            Vec2::new(1400.0, 860.0),
            1.0,
            false,
            fixture(view, Extra::default()),
        );
        h.state_mut().live = Some(live);
        h.run_steps(1);

        // Shows countdown
        assert!(h.query_by_label("Connection lost").is_some());
        assert!(h.query_by_label_contains("Reconnecting in").is_some());
        assert!(h.query_by_label_contains("attempt 1 of 3").is_some());
        assert!(h.query_by_label("Reconnect now").is_some());

        // Primary click triggers Action::Reconnect
        h.get_by_label("Reconnect now").simulate_click();
        h.run_steps(1);
        assert!(h.state().actions.contains(&Action::Reconnect));

        // When exhausted, Failed state shows "Couldn't reconnect to {pc}" with Try again
        h.state_mut().actions.clear();
        h.state_mut().view.interrupted = Some(Interruption::Failed);
        h.run_steps(1);
        assert!(h.query_by_label_contains("Couldn't reconnect to").is_some());
        assert!(h.query_by_label("Try again").is_some());
        assert!(h.query_by_label("Back to machines").is_some());

        // "Try again" initiates Reconnect
        h.get_by_label("Try again").simulate_click();
        h.run_steps(1);
        assert!(h.state().actions.contains(&Action::Reconnect));
    }
}
