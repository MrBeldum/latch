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
use crate::input::{self, press_chord, Held};
use crate::path::Path;
use crate::session::Live;
use crate::video;
use egui::{
    Align2, Color32, CursorIcon, Event, Frame, Id, Margin, Pos2, Rect, RichText, Vec2,
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
const STATUS: f32 = 24.0;
const TOAST_FOR: Duration = Duration::from_secs(5);
/// How long the host-key hint stays up when a stream starts.
const HINT_FOR: Duration = Duration::from_secs(6);
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
    let os = os.to_ascii_lowercase();
    if os.starts_with("windows") {
        "PC"
    } else if os.starts_with("macos") || os.starts_with("darwin") {
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

#[derive(Debug, Clone, PartialEq)]
pub enum Action {
    Disconnect,
    RestartStream,
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
    stats: bool,
    settings: Option<StreamSettings>,
    held: Held,
    scroll: (f32, f32),
    motion: (f32, f32),
    /// Last measured toolbar height, so the notices sit under it.
    bar_h: f32,
    confirm: Option<PowerAction>,
    confirm_install: bool,
    poor: bool,
    poor_hinted: bool,
    toasts: Vec<Toast>,
    /// When the stream started, for the host-key hint.
    hint_since: Option<Instant>,
    /// Pastes seen through `clipboard.pastes_done()`.
    pastes_seen: u32,
    /// Ctrl+Alt was down when capture last toggled; ignore further edges
    /// until both keys are up. `release_all` clears `Held.modifiers`, so
    /// deriving "was down" from that retriggered the toggle every frame.
    capture_chord_held: bool,
    /// A video problem to show in place of the session's, for rendering
    /// the recovery overlay without a broken capture.
    #[cfg(test)]
    problem: Option<String>,
}

impl Default for View {
    fn default() -> Self {
        Self {
            captured: false,
            grabbed: false,
            bar_shown: false,
            stats: false,
            settings: None,
            held: Held::default(),
            scroll: (0.0, 0.0),
            motion: (0.0, 0.0),
            bar_h: size::TOOLBAR,
            confirm: None,
            confirm_install: false,
            poor: false,
            poor_hinted: false,
            toasts: Vec::new(),
            hint_since: None,
            pastes_seen: 0,
            capture_chord_held: false,
            #[cfg(test)]
            problem: None,
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
        if let Some(l) = live {
            self.held.release_all(&l.input);
        }
        self.set_captured(ctx, false);
        self.bar_shown = false;
        self.confirm = None;
        self.settings = None;
        self.confirm_install = false;
        self.poor = false;
        self.poor_hinted = false;
        self.toasts.clear();
        self.hint_since = None;
        self.capture_chord_held = false;
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
            ctx.send_viewport_cmd(ViewportCommand::CursorGrab(grab));
            // The cursor is hidden through `set_cursor_icon(None)` alone,
            // every frame the pointer is on the picture. A
            // `CursorVisible(true)` here would show it behind egui's
            // back: egui only re-applies an icon when it changes, so the
            // Mac cursor would stay on top of the PC's until the pointer
            // left the window — two pointers.
        }
    }

    pub fn show(&mut self, ctx: &egui::Context, env: &Env<'_>) -> Vec<Action> {
        let live = env.live;
        let mut actions = Vec::new();
        let screen = ctx.screen_rect();
        let stats = live.session.stats();
        let (vw, vh) = if stats.width > 0 {
            (stats.width as f32, stats.height as f32)
        } else {
            (live.requested.0 as f32, live.requested.1 as f32)
        };

        // The picture fills the window, in its own proportions; the
        // toolbar, when the host key has asked for it, lies over the top of
        // it. A menu or a question keeps it there until it is answered.
        let video = fit(screen, vw / vh);
        let bottom_gap = screen.bottom() - video.bottom();
        let popup = ctx.memory(|m| m.any_popup_open());
        let bar_visible = self.bar_shown || popup || self.confirm.is_some() || self.confirm_install;
        let mut bar_rect = bar_visible
            .then(|| Rect::from_min_size(screen.min, Vec2::new(screen.width(), self.bar_h)));
        let waiting = live.frames.seq() == 0;

        egui::CentralPanel::default()
            .frame(Frame::new().fill(Color32::BLACK))
            .show(ctx, |ui| {
                if !waiting {
                    ui.painter().add(egui::Shape::Callback(
                        egui_wgpu::Callback::new_paint_callback(
                            video,
                            video::Paint {
                                frames: live.frames.clone(),
                            },
                        ),
                    ));
                }
                if bottom_gap >= STATUS || self.stats {
                    self.status_line(ui, screen, video, env, &stats, bottom_gap >= STATUS);
                }
            });
        if waiting {
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
        if self.stats {
            self.diagnostics(ctx, env, below_bar, &stats);
        }
        self.input(ctx, live, env.cfg, video, bar_rect);
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
        let w = overlay_width(420.0, screen.width());
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
                    ui.add_space(space::SM);
                    if ui::secondary_button(ui, "Cancel").clicked() {
                        actions.push(Action::Disconnect);
                    }
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
                            "Round trip",
                            format!("{} ± {} ms", stats.rtt_ms, stats.rtt_var_ms),
                        ),
                        Kv::new("Packet loss", format!("{:.2}%", stats.loss_pct)),
                        Kv::new("Encode on host", format!("{:.2} ms", stats.host_ms)),
                        Kv::new("Frame assembly", format!("{:.2} ms", stats.assembly_ms)),
                        Kv::new("Decoder queue", format!("{:.2} ms", stats.queue_ms)),
                        Kv::new("Decode", format!("{:.2} ms", stats.decode_ms)),
                        Kv::new("Decoder", stats.decoder.to_string()),
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
            self.stats = false;
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
                                ui.label(
                                    RichText::new(format!(
                                        "{w}×{h} · {} fps · {}",
                                        live.requested.2, live.codec
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
                                }
                                if ui::ghost_button(ui, "Stream settings").clicked() {
                                    self.held.release_all(&live.input);
                                    self.set_captured(ctx, false);
                                    self.settings = Some(cfg.stream.clone());
                                }
                                toolbar_rule(ui);
                                if ui::danger_button(ui, "Disconnect").clicked() {
                                    actions.push(Action::Disconnect);
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
            if ui::menu_choice(ui, "Stream performance", self.stats).clicked() {
                self.stats = !self.stats;
            }
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
        if ui::ghost_button(ui, if self.stats { "Hide stats" } else { "Stats" })
            .on_hover_text("Frame rate, bitrate, round trip, packet loss and decode time")
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
            if ui::menu_choice(ui, "Command acts as Ctrl", cfg.cmd_is_ctrl)
                .on_hover_text("On: Command-C, V and Z copy, paste and undo there. Off: Command is the Windows key.")
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
            self.release(ctx, live);
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
                            if ui::secondary_button(ui, "Restart stream").clicked() {
                                actions.push(Action::RestartStream);
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

    fn status_line(
        &self,
        ui: &mut egui::Ui,
        screen: Rect,
        video: Rect,
        env: &Env<'_>,
        stats: &latch_stream::Stats,
        in_gap: bool,
    ) {
        let live = env.live;
        let secs = live.started.elapsed().as_secs();
        let text = format!(
            "{:02}:{:02}:{:02}   {:.0}/{} fps   {:.1}/{} Mbps   {} ms",
            secs / 3600,
            (secs / 60) % 60,
            secs % 60,
            stats.fps,
            live.requested.2,
            stats.mbps,
            live.settings.bitrate_kbps / 1000,
            stats.rtt_ms
        );
        let font = theme::mono(theme::text::LABEL + 0.5);
        if in_gap {
            let y = video.bottom() + (screen.bottom() - video.bottom()) / 2.0;
            ui.painter().text(
                Pos2::new(screen.left() + space::MD, y),
                Align2::LEFT_CENTER,
                text,
                font,
                P.text_tertiary,
            );
        } else {
            let galley = ui.painter().layout_no_wrap(text, font, P.text_secondary);
            let size = galley.size() + Vec2::new(2.0 * space::SM, space::SM);
            let rect = Rect::from_min_size(
                Pos2::new(
                    screen.left() + space::MD,
                    screen.bottom() - size.y - space::MD,
                ),
                size,
            );
            ui.painter().rect_filled(rect, theme::radius::SM, P.overlay);
            ui.painter().galley(
                rect.min + Vec2::new(space::SM, space::XS),
                galley,
                P.text_secondary,
            );
        }
    }

    /// Capture the mouse and take the toolbar away: the picture is all.
    fn grab(&mut self, ctx: &egui::Context, live: &Live) {
        self.held.release_all(&live.input);
        self.motion = (0.0, 0.0);
        self.set_captured(ctx, true);
        self.bar_shown = false;
    }

    /// Give the mouse back.
    fn release(&mut self, ctx: &egui::Context, live: &Live) {
        self.held.release_all(&live.input);
        self.set_captured(ctx, false);
    }

    /// Ctrl+Alt, the host key. From the picture it frees the mouse and
    /// drops the toolbar; from the toolbar it hides it and, when capture
    /// is on, takes the mouse back.
    fn host_key(&mut self, ctx: &egui::Context, live: &Live, cfg: &ClientConfig) {
        self.hint_since = None;
        if self.captured || !self.bar_shown {
            self.release(ctx, live);
            self.bar_shown = true;
        } else {
            self.bar_shown = false;
            if cfg.capture_mouse {
                self.grab(ctx, live);
            }
        }
    }

    /// The stream has just connected: take the mouse if the window has
    /// focus, and say once how to get it back.
    pub fn stream_started(&mut self, ctx: &egui::Context, live: &Live, capture: bool) {
        // Capture as soon as the stream connects, whenever the window has
        // focus, without waiting for the pointer to be over the picture. A
        // game that reads raw relative motion (Genshin's camera, say) only
        // gets it while captured; requiring the pointer to already sit on the
        // picture meant a connect with the pointer elsewhere left the game
        // deaf to the mouse until the user knew to click. Ctrl+Alt still frees
        // it. The click-to-capture path stays for re-capturing after a free.
        let focused = ctx.input(|i| i.focused);
        if capture && focused {
            self.grab(ctx, live);
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
        let input = &live.input;
        let (events, modifiers, pointer, focused) = ctx.input(|i| {
            (
                i.events.clone(),
                i.modifiers,
                i.pointer.latest_pos(),
                i.focused,
            )
        });
        let popup = ctx.memory(|m| m.any_popup_open());
        let overlay = self.confirm.is_some() || self.confirm_install || self.settings.is_some();
        let over_bar = pointer.is_some_and(|p| bar.is_some_and(|b| b.contains(p)));
        let over_overlay = pointer.is_some_and(|p| {
            ctx.layer_id_at(p)
                .is_some_and(|layer| layer.order > egui::Order::Background)
        });
        let over_video = pointer_on_stream(
            pointer.is_some_and(|p| video.contains(p)),
            over_bar,
            popup,
            overlay,
            over_overlay,
        );
        if self.captured && !popup && !overlay {
            ctx.memory_mut(|m| {
                if let Some(id) = m.focused() {
                    m.surrender_focus(id);
                }
            });
        }
        let keys_to_pc =
            focused && !ctx.wants_keyboard_input() && !popup && !overlay && input.connected();
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

        if !focused {
            if self.held.any_down() || self.captured {
                self.reset(ctx, Some(live));
            }
            return;
        }
        // Ctrl+Alt is Latch's host key. It is read before the keys-to-PC
        // gate so it works even if an overlay took focus, and while it is
        // held nothing is forwarded: the PC must not see a held chord after
        // the toolbar opens (games and the Start menu react to one).
        let ctrl_alt = modifiers.ctrl && modifiers.alt;
        if ctrl_alt {
            if !self.capture_chord_held && input.connected() {
                self.capture_chord_held = true;
                self.host_key(ctx, live, cfg);
            }
            return;
        }
        self.capture_chord_held = false;
        if keys_to_pc {
            self.held.modifiers(input, modifiers, cfg.cmd_is_ctrl);
            // A paste's chord releases the modifier it pressed once the text
            // has reached the PC. If ⌘ is still held here, press it again
            // on the PC, so ⌘V ⌘V in one hold pastes twice.
            let done = live.clipboard.pastes_done();
            if done != self.pastes_seen {
                self.pastes_seen = done;
                for vk in [input::VK_CONTROL, input::VK_LWIN] {
                    if self.held.is_down(vk) {
                        input.key(vk, true, self.held.mask());
                    }
                }
            }
        } else if self.held.any_down() {
            self.held.release_all(input);
        }

        if (over_video || self.captured) && input.connected() {
            ctx.set_cursor_icon(CursorIcon::None);
        }

        let mut position: Option<Pos2> = None;
        for ev in events {
            match ev {
                Event::Key {
                    physical_key,
                    key,
                    pressed,
                    repeat,
                    ..
                } if keys_to_pc && (pressed || !repeat) => {
                    // OS autorepeat arrives as extra Downs with `repeat`.
                    // Sunshine does not generate them, so holding a key
                    // would otherwise type once. Releases never repeat.
                    if let Some(vk) = physical_key.and_then(input::vk).or_else(|| input::vk(key)) {
                        self.held.key(input, vk, pressed);
                    }
                }
                Event::Copy if keys_to_pc => {
                    self.clipboard_chord(live, Edit::Copy, cfg);
                    live.clipboard.poke();
                }
                Event::Cut if keys_to_pc => {
                    self.clipboard_chord(live, Edit::Cut, cfg);
                    live.clipboard.poke();
                }
                Event::Paste(text) if keys_to_pc => {
                    // The Mac's text first, then the paste: the PC pastes
                    // what was copied here, not what it had. The chord is
                    // sent whole: by the time the text has crossed the
                    // network a quick tap has let go of ⌘.
                    let (keys, alone) = live.shortcuts.keys(
                        Edit::Paste,
                        self.clipboard_modifier(cfg),
                        live.clipboard.pastes_to_selection(),
                    );
                    let mask = self.held.mask();
                    let handle = input.clone();
                    let text = if clipboard::worth_sending(&text) {
                        text
                    } else {
                        String::new()
                    };
                    live.clipboard.push(text, move || {
                        if alone {
                            input::press_chord_alone(&handle, &keys, mask);
                        } else {
                            press_chord(&handle, &keys, mask);
                        }
                    });
                }
                Event::PointerMoved(p) if !self.captured && over_video => position = Some(p),
                Event::MouseMoved(d) if self.captured => {
                    // Deltas arrive in points; the PC moves in stream pixels.
                    // Scale by the picture's size here so a hand movement
                    // crosses the same share of the PC's desktop as of the
                    // picture, whatever the window size or the stream size.
                    self.motion.0 += d.x * motion_scale;
                    self.motion.1 += d.y * motion_scale;
                }
                Event::PointerButton {
                    button, pressed, ..
                } if self.captured || over_video || (!pressed && self.held.any_down()) => {
                    // A click on the picture puts the toolbar away and, when
                    // capture is on, takes the mouse. The click itself still
                    // goes to the PC: what was clicked on is what was meant.
                    if pressed && over_video && !self.captured {
                        self.bar_shown = false;
                        if cfg.capture_mouse {
                            self.grab(ctx, live);
                        }
                    }
                    if let Some(b) = input::button(button) {
                        self.held.button(input, b, pressed);
                    }
                }
                Event::MouseWheel { unit, delta, .. } if self.captured || over_video => {
                    let scale = match unit {
                        egui::MouseWheelUnit::Point => 3.0,
                        egui::MouseWheelUnit::Line => 120.0,
                        egui::MouseWheelUnit::Page => 1200.0,
                    };
                    self.scroll.0 += delta.x * scale;
                    self.scroll.1 += delta.y * scale;
                }
                Event::WindowFocused(false) => self.reset(ctx, Some(live)),
                _ => {}
            }
        }
        if let Some(p) = position {
            let x = ((p.x - video.left()) * ppp).round() as i16;
            let y = ((p.y - video.top()) * ppp).round() as i16;
            input.mouse_position(
                x,
                y,
                (video.width() * ppp).round() as i16,
                (video.height() * ppp).round() as i16,
            );
        }
        if self.captured && (self.motion.0.abs() >= 1.0 || self.motion.1.abs() >= 1.0) {
            let dx = self.motion.0.trunc();
            let dy = self.motion.1.trunc();
            self.motion.0 -= dx;
            self.motion.1 -= dy;
            input.mouse_move(dx as i16, dy as i16);
        }
        if self.scroll.0.abs() >= 1.0 || self.scroll.1.abs() >= 1.0 {
            let h = self.scroll.0.trunc();
            let v = self.scroll.1.trunc();
            self.scroll.0 -= h;
            self.scroll.1 -= v;
            input.scroll(
                v.clamp(-32000.0, 32000.0) as i16,
                h.clamp(-32000.0, 32000.0) as i16,
            );
        }
    }

    /// What ⌘ stands for in a clipboard shortcut.
    fn clipboard_modifier(&self, cfg: &ClientConfig) -> i16 {
        if cfg!(target_os = "macos") && !cfg.cmd_is_ctrl {
            input::VK_LWIN
        } else {
            input::VK_CONTROL
        }
    }

    /// egui swallows Cmd/Ctrl+C/X/V into clipboard events; replay them as
    /// the key chord the PC expects.
    fn clipboard_chord(&mut self, live: &Live, edit: Edit, cfg: &ClientConfig) {
        let (keys, alone) = live.shortcuts.keys(
            edit,
            self.clipboard_modifier(cfg),
            live.clipboard.pastes_to_selection(),
        );
        if alone {
            self.held.chord_alone(&live.input, &keys);
        } else {
            self.held.chord(&live.input, &keys);
        }
    }
}

/// Whether a pointer in the picture should be sent to the PC. A real egui
/// popup (More) sets `popup`; a hand-rolled `Area` at `Order::Background`
/// would not, and clicks would leak through.
fn pointer_on_stream(
    in_video: bool,
    over_bar: bool,
    popup: bool,
    overlay: bool,
    over_overlay: bool,
) -> bool {
    in_video && !over_bar && !popup && !overlay && !over_overlay
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

/// The quality profile a stream runs at, named as Settings names it.
fn profile_label(s: &StreamSettings) -> String {
    match (s.quality, s.preset()) {
        (crate::config::Quality::Auto, _) => "Recommended".into(),
        (_, Some(p)) => p.label().into(),
        _ => format!("Custom · {}", s.describe()),
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
    use super::*;
    use crate::config::ClientConfig;
    use crate::session::Live;
    use egui_kittest::kittest::Queryable;

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
            latch_stream::session::Server {
                address: "10.255.255.1".into(),
                app_version: "7.1.431.-1".into(),
                gfe_version: "3.23.0.74".into(),
                rtsp_url: "rtsp://10.255.255.1:48010".into(),
                codec_mode_support: 1,
            },
            latch_stream::Settings {
                width: 1920,
                height: 1080,
                fps: 60,
                bitrate_kbps: 50_000,
                hevc: true,
            },
            [0; 16],
            [0; 16],
            frames.clone(),
            tx,
            || {},
        );
        let ip: std::net::Ipv4Addr = "100.64.0.10".parse().unwrap();
        let input = session.input();
        Live {
            pc: "Gaming-PC".into(),
            node_id: "n1".into(),
            ip,
            session,
            input,
            frames,
            events: rx,
            started: Instant::now(),
            codec: "HEVC",
            requested: (1920, 1080, 60),
            desktop: None,
            settings,
            path,
            clipboard: crate::clipboard::Sync::spawn(ip, ctx.clone()),
            shortcuts: Default::default(),
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
        view.show(ctx, &env);
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
        for item in ["Ctrl+Alt+Delete", "Stream performance", "Sleep…"] {
            assert!(h.query_by_label(item).is_some(), "More lists {item}");
        }
        let popup_open = h.ctx.memory(|m| m.any_popup_open());
        assert!(!pointer_on_stream(true, false, popup_open, false, false));
        h.get_by_label("Stream performance").simulate_click();
        h.run_steps(2);
        assert!(
            h.state().view.stats,
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
        view.stats = true;
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
                    ..View::default()
                };
                let mut h = harness_with(size, ppp, true, fixture(view, extra.clone()));
                if let Some(label) = open {
                    h.get_by_label(label).simulate_click();
                    h.run_steps(3);
                }
                save(h.render().unwrap(), &file_name("stream", name, size, ppp));
            }
        }
    }
}
