//! The Sharing page: setup for sharing this machine, what it reports, the
//! devices paired with it, its options and diagnostics. The unified window
//! shows it as a tab (see `product.rs`); background threads keep
//! [`Shared`] current and the page reads it once per frame.

use crate::config::{self, HostConfig};
use crate::setup;
use crate::streamer::Api;
use eframe::egui;
use latch_core::api::Status;
use latch_core::http;
use latch_core::CONTROL_PORT;
use latch_ui::{self as ui, space, Kv, Tone};
use parking_lot::Mutex;
use semver::Version;
use std::cmp::Ordering;
use std::sync::Arc;
use std::time::{Duration, Instant};

/// How long the result of an action (an unpair, a driver install) stays.
const NOTE_FOR: Duration = Duration::from_secs(10);

/// What the background threads know, read by the UI every frame.
#[derive(Default)]
pub struct Shared {
    pub status: Option<Status>,
    pub service_error: Option<String>,
    pub clients: Vec<(String, String)>,
    pub gamepad_driver: Option<bool>,
    pub setup_running: bool,
    pub setup_result: Option<Result<(), String>>,
    pub setup_log: Vec<String>,
    /// A Mac whose owner turned sharing off: it only connects to the
    /// others, so the window neither starts nor restarts the service.
    pub viewer_only: bool,
    /// Moves whenever anything above changes, so a reader can skip copying
    /// what it already has.
    pub rev: u64,
}

impl Shared {
    fn touch(&mut self) {
        self.rev = self.rev.wrapping_add(1);
    }
}

/// A one-line result from a worker thread.
type Note = Arc<Mutex<Option<(Tone, String)>>>;

/// Which OS the page describes. Compiled in for the running app; set by
/// tests to render the other platforms' pages.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Os {
    Windows,
    Mac,
    Linux,
}

impl Os {
    pub const HERE: Os = if cfg!(windows) {
        Os::Windows
    } else if cfg!(target_os = "macos") {
        Os::Mac
    } else {
        Os::Linux
    };

    fn noun(self) -> &'static str {
        match self {
            Os::Windows => "PC",
            Os::Mac => "Mac",
            Os::Linux => "machine",
        }
    }

    fn tailscale_url(self) -> &'static str {
        match self {
            Os::Windows => "https://tailscale.com/download/windows",
            Os::Mac => "https://tailscale.com/download/mac",
            Os::Linux => "https://tailscale.com/download/linux",
        }
    }
}

pub struct HostApp {
    shared: Arc<Mutex<Shared>>,
    cfg: HostConfig,
    autostart: bool,
    dirty: bool,
    confirm_unpair: Option<String>,
    busy_since: Option<Instant>,
    /// The engine archive sits beside the exe, so setup needs no download.
    bundled_engine: bool,
    /// Actions reach the system: setup runs, devices unpair, the service
    /// stops. Off when the page is only rendered, in tests.
    live: bool,
    os: Os,
    note: Option<(Tone, String, Instant)>,
    pending: Option<Note>,
}

impl HostApp {
    pub fn new(cc: &eframe::CreationContext<'_>) -> Self {
        let cfg = HostConfig::load();
        let viewer_only = !crate::service::starts_with_window(&cfg);
        let shared = Arc::new(Mutex::new(Shared {
            viewer_only,
            // Down on purpose, as if stopped here: nothing restarts it.
            service_error: viewer_only.then(|| STOPPED.into()),
            ..Default::default()
        }));
        spawn_poller(shared.clone(), cc.egui_ctx.clone());
        Self {
            cfg,
            autostart: setup::starts_with_windows(),
            bundled_engine: std::env::current_exe().is_ok_and(|e| setup::bundled_engine(&e)),
            live: true,
            ..Self::headless(shared, Os::HERE)
        }
    }

    /// A page that only draws: default settings, no threads, and buttons
    /// that change what it shows but never touch the system.
    #[doc(hidden)]
    pub fn headless(shared: Arc<Mutex<Shared>>, os: Os) -> Self {
        Self {
            shared,
            cfg: HostConfig::default(),
            autostart: true,
            dirty: false,
            confirm_unpair: None,
            busy_since: None,
            bundled_engine: os == Os::Windows,
            live: false,
            os,
            note: None,
            pending: None,
        }
    }

    fn commit(&mut self) {
        if self.dirty {
            self.dirty = false;
            if !self.live {
                return;
            }
            if let Err(e) = self.cfg.save() {
                tracing::warn!("could not save host config: {e:#}");
                self.note = Some((
                    Tone::Danger,
                    format!("Couldn't save this setting: {e}."),
                    Instant::now(),
                ));
            }
        }
    }

    pub(crate) fn shared(&self) -> parking_lot::MutexGuard<'_, Shared> {
        self.shared.lock()
    }

    /// Run `job` off the UI thread and show what it returns under the
    /// page's header.
    fn later(
        &mut self,
        ctx: &egui::Context,
        job: impl FnOnce() -> (Tone, String) + Send + 'static,
    ) {
        let note: Note = Arc::default();
        let out = note.clone();
        let ctx = ctx.clone();
        std::thread::spawn(move || {
            *out.lock() = Some(job());
            ctx.request_repaint();
        });
        self.pending = Some(note);
    }

    fn poll_note(&mut self, ctx: &egui::Context) {
        if let Some(done) = self.pending.as_ref().and_then(|n| n.lock().take()) {
            self.note = Some((done.0, done.1, Instant::now()));
            self.pending = None;
        }
        if let Some((_, _, at)) = &self.note {
            match NOTE_FOR.checked_sub(at.elapsed()) {
                Some(left) => ctx.request_repaint_after(left),
                None => self.note = None,
            }
        }
    }

    /// The engine's login, for a worker thread.
    fn creds(&self) -> Option<(String, String)> {
        self.cfg.has_creds().then(|| {
            (
                self.cfg.sunshine_user.clone(),
                self.cfg.sunshine_pass.clone(),
            )
        })
    }

    /// Generate a Sunshine login if there is none, save it, and run the
    /// setup on a thread.
    fn start_setup(&mut self, ctx: &egui::Context, status: &Status) {
        let migrate = crate::migrate::uses_old_engine(&status.streamer.kind);
        let shared = self.shared.clone();
        {
            let mut s = shared.lock();
            s.setup_running = true;
            s.setup_result = None;
            s.setup_log.clear();
            s.touch();
        }
        if !self.live {
            return;
        }
        if !self.cfg.has_creds() && !migrate {
            self.cfg.sunshine_user = "latch".into();
            self.cfg.sunshine_pass = config::random_password();
            if let Err(e) = self.cfg.save() {
                tracing::warn!("could not save engine login: {e:#}");
            }
        }
        let cfg = self.cfg.clone();
        let ctx = ctx.clone();
        let install_engine = !status.streamer.installed
            || migrate
            || !status.streamer.running
            || crate::streamer::find().is_some_and(|i| crate::streamer::in_old_folder(&i));
        let adapter = status.wake_adapter.clone();
        let desc = status.wake_adapter_description.clone();
        std::thread::spawn(move || {
            let result = std::env::current_exe()
                .map_err(|e| e.to_string())
                .and_then(|exe| {
                    setup::run(&setup::Plan {
                        exe: &exe,
                        install_engine,
                        migrate,
                        dry_run: false,
                        sunshine_user: &cfg.sunshine_user,
                        sunshine_pass: &cfg.sunshine_pass,
                        adapter: &adapter,
                        adapter_description: &desc,
                    })
                    .map_err(|e| e.to_string())?;
                    // The service is what other machines need at logon; turn
                    // it on once setup has succeeded rather than asking.
                    let _ = setup::set_start_with_windows(true, &exe);
                    Ok(())
                });
            let mut s = shared.lock();
            s.setup_running = false;
            s.setup_log = read_setup_log();
            s.setup_result = Some(result);
            s.touch();
            drop(s);
            // The Machines page shows setup too, and it has no timer.
            ctx.request_repaint();
        });
    }

    fn set_autostart(&mut self, on: bool) {
        if !self.live {
            self.autostart = on;
            return;
        }
        if self.os == Os::Mac && on {
            // Loading the login item starts the service at once, and on a
            // Mac it exits unless it reads that this Mac is shared.
            let mut cfg = self.cfg.clone();
            cfg.start_with_windows = true;
            if let Err(e) = cfg.save() {
                tracing::warn!("could not save host config: {e:#}");
            }
        }
        let result = std::env::current_exe()
            .map_err(anyhow::Error::from)
            .and_then(|exe| setup::set_start_with_windows(on, &exe));
        match result {
            Ok(()) => {
                self.autostart = on;
                self.cfg.start_with_windows = on;
                self.dirty = true;
                if self.os == Os::Mac {
                    // On a Mac this is the switch between sharing and only
                    // connecting to the others: it takes effect now.
                    self.shared.lock().viewer_only = !on;
                    if on {
                        self.start_service();
                    } else {
                        self.stop_sharing();
                    }
                }
            }
            Err(e) => {
                tracing::warn!("autostart: {e:#}");
                self.note = Some((
                    Tone::Danger,
                    format!("Couldn't change whether Latch starts at login: {e}."),
                    Instant::now(),
                ));
            }
        }
    }

    fn stop_service(&mut self) {
        if self.live {
            // Off the UI thread: the service may take a moment to answer.
            std::thread::spawn(|| {
                let _ = http::request(
                    ("127.0.0.1", CONTROL_PORT),
                    "POST",
                    "/v1/quit",
                    None,
                    Duration::from_secs(2),
                );
            });
        }
        let mut s = self.shared.lock();
        s.status = None;
        s.service_error = Some(STOPPED.into());
        s.touch();
    }

    /// Stop the service and the engine it started: nothing is shared and
    /// nothing of Latch keeps running once the window closes.
    fn stop_sharing(&mut self) {
        self.stop_service();
        #[cfg(not(windows))]
        if self.live {
            std::thread::spawn(|| {
                // After the service has gone, or it starts the engine again.
                std::thread::sleep(Duration::from_millis(800));
                crate::unix_setup::stop_engine();
            });
        }
    }

    fn start_service(&mut self) {
        {
            let mut s = self.shared.lock();
            s.service_error = None;
            s.touch();
        }
        if self.live {
            std::thread::spawn(crate::service::ensure_service_running);
        }
    }

    fn open_log_folder(&mut self) {
        if !self.live {
            return;
        }
        let Ok(dir) = latch_core::config::data_dir() else {
            return;
        };
        let opener = match self.os {
            Os::Windows => "explorer",
            Os::Mac => "open",
            Os::Linux => "xdg-open",
        };
        if let Err(e) = std::process::Command::new(opener).arg(&dir).spawn() {
            self.note = Some((
                Tone::Danger,
                format!("Couldn't open {}: {e}.", dir.display()),
                Instant::now(),
            ));
        }
    }
}

/// What the service error says after the user stopped it from here.
const STOPPED: &str = "Stopped from this window.";

impl Shared {
    /// The service is down because someone pressed Stop, not by accident.
    pub(crate) fn stopped_by_user(&self) -> bool {
        self.service_error.as_deref() == Some(STOPPED)
    }
}

impl HostApp {
    /// Draw the page into `ui`, a column the window has already laid out.
    pub fn page(&mut self, ui: &mut egui::Ui) {
        let ctx = ui.ctx().clone();
        self.poll_note(&ctx);
        let shared = self.shared.lock();
        let status = shared.status.clone();
        let service_error = shared.service_error.clone();
        let clients = shared.clients.clone();
        let gamepad = shared.gamepad_driver;
        let setup_running = shared.setup_running;
        let setup_result = shared.setup_result.clone();
        let setup_log = shared.setup_log.clone();
        drop(shared);
        if setup_running {
            self.busy_since.get_or_insert_with(Instant::now);
            // The seconds counter under the spinner.
            ctx.request_repaint_after(Duration::from_secs(1));
        } else {
            self.busy_since = None;
        }

        let Some(s) = status else {
            self.no_service(ui, service_error.as_deref());
            return;
        };
        let (label, tone) = pill(Some(&s), None);
        ui::page_header(ui, &s.name, Some(&self.summary(&s)), |ui| {
            ui::status_text(ui, tone, label);
        });
        if let Some((tone, text, _)) = self.note.clone() {
            ui::notice(ui, tone, &text);
        }
        if s.version != env!("CARGO_PKG_VERSION") {
            ui::notice(
                ui,
                Tone::Neutral,
                &format!(
                    "The background service runs Latch {} and this window {}; Latch is restarting the older one.",
                    s.version,
                    env!("CARGO_PKG_VERSION")
                ),
            );
        }
        self.setup_card(ui, &s, setup_running, setup_result.as_ref(), &setup_log);
        self.overview(ui, &s, gamepad);
        self.paired(ui, &clients);
        self.options(ui);
        self.diagnostics(ui, &s, setup_running);
        self.commit();
    }

    /// One sentence under the machine's name: can others connect?
    fn summary(&self, s: &Status) -> String {
        let noun = self.os.noun();
        match pill(Some(s), None).0 {
            "Tailscale off" => format!(
                "Tailscale isn't running on this {noun}, so no other machine can reach it."
            ),
            "Needs setup" => format!(
                "Other machines can see this {noun} but can't connect until setup finishes."
            ),
            _ => format!(
                "Shared. Any machine signed in to your Tailscale account can connect to this {noun}."
            ),
        }
    }

    fn no_service(&mut self, ui: &mut egui::Ui, error: Option<&str>) {
        ui::page_header(ui, "Sharing", None, |_| {});
        if self.shared.lock().viewer_only {
            ui::banner(
                ui,
                Tone::Neutral,
                &format!("This {} is not shared", self.os.noun()),
                Some(&format!(
                    "It connects to your other machines, but they can't connect to it, and nothing of Latch runs once this window closes. Share this {} to let them in, also after a restart.",
                    self.os.noun()
                )),
                |ui| {
                    if ui::primary_button(ui, &format!("Share this {}", self.os.noun())).clicked() {
                        self.set_autostart(true);
                    }
                },
            );
            return;
        }
        if error == Some(STOPPED) {
            ui::banner(
                ui,
                Tone::Neutral,
                "The background service is stopped",
                Some(&format!(
                    "Other machines can't connect to this {} until it runs again. It also starts the next time this window opens.",
                    self.os.noun()
                )),
                |ui| {
                    if ui::primary_button(ui, "Start it now").clicked() {
                        self.start_service();
                    }
                },
            );
        } else {
            ui::card(ui, |ui| {
                ui::heading(ui, "Starting the background service", None);
                ui::empty_state(
                    ui,
                    "It shares this machine and answers the others. This takes a second or two.",
                    true,
                );
                if let Some(e) = error {
                    ui::small_print(ui, format!("Last attempt: {e}"));
                }
            });
        }
    }

    fn setup_card(
        &mut self,
        ui: &mut egui::Ui,
        s: &Status,
        running: bool,
        result: Option<&Result<(), String>>,
        log: &[String],
    ) {
        // Tailscale is the user's job (sign in), not the script's.
        let admin_items: Vec<&String> = s
            .setup
            .iter()
            .filter(|l| !l.starts_with("Tailscale"))
            .collect();
        let tailscale_item = s.setup.iter().find(|l| l.starts_with("Tailscale"));
        let done = admin_items.is_empty() && tailscale_item.is_none();
        if done && !running && !matches!(result, Some(Err(_))) {
            if result.is_some() {
                ui::notice(
                    ui,
                    Tone::Success,
                    &format!(
                        "Setup finished. Other machines can connect to this {} now.",
                        self.os.noun()
                    ),
                );
            }
            return;
        }
        let migrate = crate::migrate::uses_old_engine(&s.streamer.kind);
        let tone = if done && !matches!(result, Some(Err(_))) {
            Tone::Success
        } else if matches!(result, Some(Err(_))) {
            Tone::Danger
        } else {
            Tone::Warning
        };
        let (title, how) = match self.os {
            Os::Windows => (
                if migrate {
                    "Update the streaming engine"
                } else {
                    "Share this PC"
                },
                "One administrator prompt does everything listed here. Nothing else needs configuring.",
            ),
            Os::Mac => (
                "Share this Mac",
                "Latch installs the streaming engine for you. macOS then asks once to let it record the screen.",
            ),
            Os::Linux => (
                "Share this machine",
                "Latch installs the streaming engine for this user and keeps it running.",
            ),
        };
        let title = if done { "Sharing is set up" } else { title };
        ui::toned_card(ui, tone, |ui| {
            ui.spacing_mut().item_spacing.y = space::SM;
            ui::heading(ui, title, (!done).then_some(how));
            if let Some(t) = tailscale_item {
                let why = t
                    .trim_start_matches("Tailscale:")
                    .trim()
                    .trim_end_matches("Install it and sign in.")
                    .trim()
                    .trim_end_matches('.');
                // The link belongs to the item: it starts where its text does.
                ui::dot_item(ui, Tone::Danger, |ui| {
                    ui.spacing_mut().item_spacing.y = space::XXS;
                    ui.add(
                        egui::Label::new(egui::RichText::new(format!(
                            "{}. Install Tailscale and sign in with the account your other machines use.",
                            sentence_case(why)
                        )).color(ui::PALETTE.text))
                        .wrap(),
                    );
                    if ui::link(ui, "Get Tailscale").clicked() {
                        ui.ctx()
                            .open_url(egui::OpenUrl::new_tab(self.os.tailscale_url()));
                    }
                });
            }
            for item in &admin_items {
                ui::dot_label(ui, Tone::Warning, item);
            }
            ui.add_space(space::XS);
            if running {
                let secs = self.busy_since.map(|t| t.elapsed().as_secs()).unwrap_or(0);
                let what = match self.os {
                    Os::Windows => "Waiting for the administrator prompt and the setup steps",
                    _ => "Installing and starting the streaming engine",
                };
                ui::empty_state(ui, &format!("{what}… {secs} s"), true);
            } else if !done || result.is_some() {
                ui.horizontal_wrapped(|ui| {
                    ui.spacing_mut().item_spacing.x = space::MD;
                    let label = if migrate {
                        "Update the engine"
                    } else if admin_items.is_empty() {
                        "Run setup again"
                    } else if self.os == Os::Windows {
                        "Set up as administrator"
                    } else {
                        "Set up sharing"
                    };
                    let primary = !admin_items.is_empty() || migrate;
                    let clicked = if primary {
                        ui::primary_button(ui, label).clicked()
                    } else {
                        ui::secondary_button(ui, label).clicked()
                    };
                    if clicked {
                        self.start_setup(ui.ctx(), s);
                    }
                    let note = if migrate {
                        Some("Keeps this PC's pairings and web login.")
                    } else if !s.streamer.installed {
                        Some(match (self.os, self.bundled_engine) {
                            (Os::Windows, true) => "Installs the engine that ships with Latch.",
                            _ => "Downloads the streaming engine and installs it.",
                        })
                    } else {
                        None
                    };
                    if let Some(n) = note {
                        ui::small_print(ui, n);
                    }
                });
            }
            match result {
                Some(Ok(())) => {
                    ui::notice(
                        ui,
                        Tone::Success,
                        "Setup finished. The service is re-checking; the items above clear within a few seconds.",
                    );
                }
                Some(Err(e)) => {
                    let friendly = setup_error(e, self.os);
                    ui::banner(
                        ui,
                        Tone::Danger,
                        "Setup didn't finish",
                        Some(&friendly),
                        |ui| {
                            if !friendly.contains(e.trim_end_matches('.')) {
                                ui.add(
                                    egui::Label::new(
                                        egui::RichText::new(e)
                                            .font(ui::theme::mono(ui::theme::text::MONO - 1.0))
                                            .color(ui::PALETTE.text_tertiary),
                                    )
                                    .wrap(),
                                );
                            }
                        },
                    );
                }
                None => {}
            }
            if !log.is_empty() {
                ui::disclosure(
                    ui,
                    "setup_log",
                    "Setup log",
                    matches!(result, Some(Err(_))),
                    |ui| {
                        ui::log_view(ui, "setup_log_view", log, 180.0);
                    },
                );
            }
        });
    }

    fn overview(&mut self, ui: &mut egui::Ui, s: &Status, gamepad: Option<bool>) {
        let noun = self.os.noun();
        let (ts_tone, tailscale) = match (&s.tailscale_ip, &s.tailscale_login) {
            (Some(ip), Some(login)) => (Tone::Success, format!("{ip} · {login}")),
            (Some(ip), None) => (Tone::Success, ip.clone()),
            _ => (Tone::Danger, "Not running".into()),
        };
        let (engine_tone, engine) = if !s.streamer.installed {
            (Tone::Warning, "Not installed".to_string())
        } else if !s.streamer.running {
            (Tone::Warning, "Installed, not running".to_string())
        } else if !s.streamer.api_ok {
            (
                Tone::Warning,
                "Running, but Latch can't sign in to it".to_string(),
            )
        } else {
            let encoder = match s.streamer.encoder.as_str() {
                "" => String::new(),
                "software" => {
                    " · Software encoding: no GPU encoder worked, so streams use the CPU".into()
                }
                "nvenc" => " · NVIDIA encoder".into(),
                "amdvce" => " · AMD encoder".into(),
                "quicksync" => " · Intel encoder".into(),
                "videotoolbox" => " · Apple encoder".into(),
                e => format!(" · {e} encoder"),
            };
            let tone = if s.streamer.encoder == "software" {
                Tone::Warning
            } else {
                Tone::Success
            };
            (tone, format!("Running{encoder}"))
        };
        let network = match &s.nat {
            Some(n) => network_sentence(n, noun),
            None => "Checking…".into(),
        };
        let mut rows = vec![
            Kv::new("Tailscale", tailscale).tone(ts_tone),
            Kv::new("Streaming engine", engine).tone(engine_tone),
        ];
        if !s.streamer.audio_problem.is_empty() {
            rows.push(
                Kv::new(
                    "Sound",
                    format!(
                        "None to send: “{}”. Give this {noun} an audio output, even a virtual one.",
                        s.streamer.audio_problem
                    ),
                )
                .tone(Tone::Warning),
            );
        }
        rows.push(Kv::new("Network", network));
        if self.os == Os::Windows {
            let (tone, mut wake) = match (&s.mac, s.wake_ready) {
                (Some(mac), Some(true)) => {
                    (Tone::Success, format!("Ready · {} · {mac}", s.wake_adapter))
                }
                (Some(mac), Some(false)) => {
                    (Tone::Warning, format!("Off · {} · {mac}", s.wake_adapter))
                }
                (Some(mac), None) => (
                    Tone::Neutral,
                    format!("Unknown · {} · {mac}", s.wake_adapter),
                ),
                (None, _) => (Tone::Neutral, "No wired adapter found".into()),
            };
            if s.fast_startup == Some(true) {
                wake.push_str(" · Fast Startup is on");
            }
            if let Some(age) = s.wake_packet_age_secs {
                // The poller ignores this clock (only_aged), so it is kept
                // current here, and only while it is on screen.
                let (text, next) = match age {
                    0..60 => (format!("{age} s"), 1),
                    60..3600 => (format!("{} min", age / 60), 60 - age % 60),
                    _ => (format!("{} h", age / 3600), 3600 - age % 3600),
                };
                wake.push_str(&format!(" · Last wake packet {text} ago"));
                ui.ctx().request_repaint_after(Duration::from_secs(next));
            }
            rows.push(Kv::new("Wake-on-LAN", wake).tone(tone));
        }
        rows.push(Kv::new(
            "LAN address",
            s.lan_ip.clone().unwrap_or_else(|| "None".into()),
        ));
        if self.os == Os::Windows {
            let (tone, text) = match gamepad {
                Some(true) => (Tone::Success, "Virtual controller driver installed"),
                Some(false) => (Tone::Warning, "Virtual controller driver missing"),
                None => (Tone::Neutral, "Known once the streaming engine runs"),
            };
            rows.push(Kv::new("Game controllers", text).tone(tone));
        }
        let heading = format!("This {noun}");
        ui::section(ui, &heading, |ui| {
            ui.add_space(space::MD);
            ui::kv_grid(ui, &rows);
            if self.os == Os::Windows && gamepad == Some(false) {
                ui.add_space(space::SM);
                if ui::secondary_button(ui, "Install controller driver").clicked() {
                    self.install_gamepad_driver(ui.ctx());
                }
            }
            ui.add_space(space::MD);
        });
    }

    fn install_gamepad_driver(&mut self, ctx: &egui::Context) {
        let Some((user, pass)) = self.creds().filter(|_| self.live) else {
            return;
        };
        self.later(ctx, move || {
            let api = Api {
                user: &user,
                pass: &pass,
            };
            match api.install_gamepad_driver() {
                Ok(()) => (
                    Tone::Success,
                    "The controller driver is installing. Windows may ask to confirm.".into(),
                ),
                Err(e) => {
                    tracing::warn!("ViGEmBus install: {e:#}");
                    (
                        Tone::Danger,
                        format!("Couldn't install the controller driver: {e}."),
                    )
                }
            }
        });
    }

    fn paired(&mut self, ui: &mut egui::Ui, clients: &[(String, String)]) {
        ui::section(ui, "Paired devices", |ui| {
            if clients.is_empty() {
                ui.add_space(space::MD);
                ui::muted(ui, "None yet. A machine pairs itself the first time it connects; nothing needs typing here.");
                ui.add_space(space::MD);
                return;
            }
            for (i, (name, uuid)) in clients.iter().enumerate() {
                if i > 0 {
                    ui::row_separator(ui);
                }
                let id = format!("ID {}", &uuid[..uuid.len().min(8)]);
                ui::list_row(ui, None, name, &id, |ui| {
                    if self.confirm_unpair.as_deref() == Some(uuid) {
                        if ui::ghost_button(ui, "Keep").clicked() {
                            self.confirm_unpair = None;
                        }
                        if ui::destructive_button(ui, "Unpair").clicked() {
                            self.unpair(ui.ctx(), name, uuid);
                            self.confirm_unpair = None;
                        }
                    } else if ui::danger_button(ui, "Unpair…")
                        .on_hover_text("It pairs again by itself the next time it connects.")
                        .clicked()
                    {
                        self.confirm_unpair = Some(uuid.clone());
                    }
                });
            }
        });
    }

    fn unpair(&mut self, ctx: &egui::Context, name: &str, uuid: &str) {
        let Some((user, pass)) = self.creds().filter(|_| self.live) else {
            return;
        };
        let (name, uuid) = (name.to_string(), uuid.to_string());
        self.later(ctx, move || {
            let api = Api {
                user: &user,
                pass: &pass,
            };
            match api.unpair(&uuid) {
                Ok(()) => (
                    Tone::Success,
                    format!("Unpaired {name}. It pairs again the next time it connects."),
                ),
                Err(e) => {
                    tracing::warn!("unpair: {e:#}");
                    (Tone::Danger, format!("Couldn't unpair {name}: {e}."))
                }
            }
        });
    }

    fn options(&mut self, ui: &mut egui::Ui) {
        let noun = self.os.noun();
        ui::section(ui, "Options", |ui| {
            if ui::toggle_row(
                ui,
                &mut self.cfg.power_allowed,
                &format!("Let other machines sleep, restart or shut down this {noun}"),
                Some(&if self.os == Os::Windows {
                    format!("Any machine your Tailscale access rules allow can ask. Waking this {noun} works only from its own network.")
                } else {
                    "Any machine your Tailscale access rules allow can ask.".to_string()
                }),
            ) {
                self.dirty = true;
            }
            if self.os == Os::Windows {
                ui::row_separator(ui);
                if ui::toggle_row(
                    ui,
                    &mut self.cfg.stay_awake,
                    "Keep this PC awake while plugged in",
                    Some("Asleep, Tailscale is off too, so machines on other networks can't wake it. Sleep from the Start menu or another machine still works."),
                ) {
                    self.dirty = true;
                }
            }
            ui::row_separator(ui);
            let mut auto = self.autostart;
            let (title, detail) = if self.os == Os::Mac {
                (
                    "Share this Mac".to_string(),
                    "Others can connect whenever it is on, also after a restart. Off, this Mac only connects to your other machines and nothing of Latch runs once its window closes.".to_string(),
                )
            } else {
                (
                    "Start Latch when you log in".to_string(),
                    format!(
                        "So others can connect after this {noun} restarts, with nobody at the keyboard."
                    ),
                )
            };
            if ui::toggle_row(ui, &mut auto, &title, Some(&detail)) {
                self.set_autostart(auto);
            }
        });
    }

    fn diagnostics(&mut self, ui: &mut egui::Ui, s: &Status, setup_running: bool) {
        ui::section(ui, "Diagnostics", |ui| {
            ui.add_space(space::XS);
            ui::disclosure(ui, "host-diagnostics", "Service log", false, |ui| {
                ui::log_view(ui, "host_log", &s.log, 220.0);
                ui.add_space(space::XS);
                if ui::secondary_button(ui, "Copy log").clicked() {
                    ui.ctx().copy_text(s.log.join("\n"));
                }
            });
            ui::row_separator(ui);
            ui::setting_row(
                ui,
                "Repair sharing",
                Some("Runs setup again: reapplies display matching and clears stale bitrate limits. Paired devices stay paired."),
                |ui| {
                    let clicked = ui
                        .add_enabled_ui(!setup_running, |ui| {
                            ui::secondary_button(ui, "Run setup again")
                        })
                        .inner
                        .clicked();
                    if clicked {
                        self.start_setup(ui.ctx(), s);
                    }
                },
            );
            ui::row_separator(ui);
            ui::setting_row(
                ui,
                "Log files",
                Some(&format!(
                    "Control port TCP {CONTROL_PORT}. The service and window logs roll over at 4 MB."
                )),
                |ui| {
                    if ui::secondary_button(ui, "Open folder").clicked() {
                        self.open_log_folder();
                    }
                },
            );
            ui::row_separator(ui);
            ui::setting_row(
                ui,
                "Background service",
                Some("Stopping it stops sharing until this window opens again."),
                |ui| {
                    if ui::danger_button(ui, "Stop service").clicked() {
                        self.stop_service();
                    }
                },
            );
        });
    }
}

/// What this machine's NAT means for the others, in a sentence that names
/// the right kind of machine (the service log's version says "a Mac").
fn network_sentence(n: &latch_core::api::NatReport, noun: &str) -> String {
    let city = latch_core::tailscale::derp_city(&n.derp);
    if !n.udp {
        format!("UDP is blocked here, so other machines reach this {noun} only through a Tailscale relay.")
    } else if n.hard == Some(true) && !n.portmap {
        format!("Hard NAT without UPnP: machines on other networks reach this {noun} through the {city} relay, unless the router gets UPnP or a forwarded UDP port.")
    } else if n.hard == Some(true) {
        "Hard NAT, but the router maps ports, so direct connections should work.".into()
    } else if n.hard == Some(false) {
        format!("Easy NAT: direct connections should work. Nearest relay: {city}.")
    } else {
        format!("NAT type unknown. Nearest relay: {city}.")
    }
}

/// A setup failure in words: what happened, then what to do.
fn setup_error(e: &str, os: Os) -> String {
    let lower = e.to_lowercase();
    if os == Os::Windows && lower.contains("declined") {
        return "The administrator prompt was declined, or setup stopped part way. Try again and choose Yes; the setup log below shows the last step reached.".into();
    }
    format!(
        "{}. The setup log below shows the last step reached; run setup again once it is fixed.",
        sentence_case(e.trim_end_matches('.'))
    )
}

/// Capitalise the first letter.
fn sentence_case(s: &str) -> String {
    let mut c = s.chars();
    match c.next() {
        Some(f) => f.to_uppercase().collect::<String>() + c.as_str(),
        None => String::new(),
    }
}

/// The page's status in two words, and its tone.
fn pill(status: Option<&Status>, err: Option<&str>) -> (&'static str, Tone) {
    match status {
        None if err.is_some() => ("Service stopped", Tone::Neutral),
        None => ("Starting", Tone::Neutral),
        Some(s) if s.setup.iter().any(|l| l.starts_with("Tailscale")) => {
            ("Tailscale off", Tone::Danger)
        }
        Some(s) if !s.setup.is_empty() => ("Needs setup", Tone::Warning),
        Some(_) => ("Shared", Tone::Success),
    }
}

/// Last lines of the elevated script's transcript, BOM and blanks dropped.
fn read_setup_log() -> Vec<String> {
    let Some(path) = setup::log_path() else {
        return Vec::new();
    };
    let Ok(bytes) = std::fs::read(&path) else {
        return Vec::new();
    };
    let text = decode_log(&bytes);
    let lines: Vec<String> = text
        .lines()
        .map(str::trim_end)
        .filter(|l| !l.is_empty())
        .map(String::from)
        .collect();
    lines.into_iter().rev().take(40).rev().collect()
}

/// `Out-File -Encoding utf8` writes a BOM; older PowerShell writes UTF-16.
fn decode_log(bytes: &[u8]) -> String {
    if bytes.starts_with(&[0xFF, 0xFE]) {
        let u16s: Vec<u16> = bytes[2..]
            .as_chunks::<2>()
            .0
            .iter()
            .map(|c| u16::from_le_bytes(*c))
            .collect();
        String::from_utf16_lossy(&u16s)
    } else {
        String::from_utf8_lossy(bytes.strip_prefix(&[0xEF, 0xBB, 0xBF]).unwrap_or(bytes))
            .into_owned()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum UpdateAction {
    None,
    /// Service is older than this panel: quit it and start the file on disk.
    RestartService,
    /// Service is newer: this panel is the leftover window, replace it.
    RelaunchPanel,
}

fn update_action(service: &str, panel: &str) -> UpdateAction {
    let (Ok(s), Ok(p)) = (Version::parse(service), Version::parse(panel)) else {
        return UpdateAction::None;
    };
    match s.cmp(&p) {
        Ordering::Less => UpdateAction::RestartService,
        Ordering::Greater => UpdateAction::RelaunchPanel,
        Ordering::Equal => UpdateAction::None,
    }
}

fn relaunch_this_exe() -> bool {
    let Ok(exe) = std::env::current_exe() else {
        return false;
    };
    std::process::Command::new(exe)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .is_ok()
}

/// Poll the service every second; refresh Sunshine's client list now and
/// then; restart the service after an update. The window repaints only
/// when an answer differs from the last.
/// Whether `new` is `old` a little later: the wake packet's age only counts
/// up, and the Sharing page redraws that clock itself while it is shown.
fn only_aged(old: &Status, new: &Status) -> bool {
    let aged = match (old.wake_packet_age_secs, new.wake_packet_age_secs) {
        (Some(a), Some(b)) => b >= a,
        (a, b) => a == b,
    };
    let mut old = old.clone();
    old.wake_packet_age_secs = new.wake_packet_age_secs;
    aged && old == *new
}

fn spawn_poller(shared: Arc<Mutex<Shared>>, ctx: egui::Context) {
    std::thread::spawn(move || {
        let mut failures = 0u32;
        let mut tick = 0u64;
        let mut restarted_service = false;
        loop {
            // Repaint only when something the page shows has changed: an
            // idle window draws nothing.
            let mut changed = false;
            let r: Result<Status, _> = http::get_json(
                ("127.0.0.1", CONTROL_PORT),
                "/v1/status",
                Duration::from_millis(800),
            );
            match r {
                Ok(st) => {
                    failures = 0;
                    match update_action(&st.version, env!("CARGO_PKG_VERSION")) {
                        UpdateAction::RestartService if !restarted_service => {
                            // This panel is newer than the service (an
                            // update replaced the exe; current_exe still
                            // names it). Restart once; looping on != used
                            // to kill a *newer* service every second.
                            restarted_service = true;
                            let _ = http::request(
                                ("127.0.0.1", CONTROL_PORT),
                                "POST",
                                "/v1/quit",
                                None,
                                Duration::from_secs(2),
                            );
                            std::thread::sleep(Duration::from_secs(1));
                            crate::service::ensure_service_running();
                        }
                        UpdateAction::RelaunchPanel if relaunch_this_exe() => {
                            std::process::exit(0);
                        }
                        _ => {}
                    }
                    let cfg = HostConfig::load();
                    if st.streamer.api_ok && cfg.has_creds() && tick.is_multiple_of(10) {
                        let api = Api {
                            user: &cfg.sunshine_user,
                            pass: &cfg.sunshine_pass,
                        };
                        let clients = api.clients();
                        let gamepad = api.gamepad_driver();
                        let mut s = shared.lock();
                        changed |= s.clients != clients || s.gamepad_driver != gamepad;
                        s.clients = clients;
                        s.gamepad_driver = gamepad;
                    }
                    let mut s = shared.lock();
                    changed |= !s.status.as_ref().is_some_and(|old| only_aged(old, &st));
                    s.status = Some(st);
                    if s.service_error.as_deref().is_some_and(|e| e != STOPPED) {
                        s.service_error = None;
                        changed = true;
                    }
                }
                Err(e) => {
                    failures += 1;
                    let mut s = shared.lock();
                    let stopped_by_user =
                        s.service_error.as_deref() == Some(STOPPED) || s.viewer_only;
                    changed |= s.status.is_some();
                    s.status = None;
                    if !stopped_by_user {
                        let e = format!("{e}");
                        changed |= s.service_error.as_deref() != Some(e.as_str());
                        s.service_error = Some(e);
                    }
                    drop(s);
                    if failures % 4 == 2 && !stopped_by_user {
                        crate::service::ensure_service_running();
                    }
                }
            }
            if changed {
                shared.lock().touch();
                ctx.request_repaint();
            }
            tick += 1;
            std::thread::sleep(Duration::from_secs(1));
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn setup_log_decodes_utf16_and_utf8_with_boms() {
        let utf8 = [&[0xEF, 0xBB, 0xBF][..], "a\r\n\r\nb\n".as_bytes()].concat();
        assert_eq!(
            decode_log(&utf8)
                .lines()
                .filter(|l| !l.trim().is_empty())
                .count(),
            2
        );
        let mut utf16 = vec![0xFF, 0xFE];
        for c in "hi\n".encode_utf16() {
            utf16.extend_from_slice(&c.to_le_bytes());
        }
        assert_eq!(decode_log(&utf16), "hi\n");
    }

    #[test]
    fn a_newer_service_does_not_get_killed_by_the_old_panel() {
        assert_eq!(update_action("3.1.0", "3.1.0"), UpdateAction::None);
        assert_eq!(
            update_action("3.0.1", "3.1.0"),
            UpdateAction::RestartService
        );
        assert_eq!(update_action("3.1.0", "3.0.1"), UpdateAction::RelaunchPanel);
        assert_eq!(update_action("nope", "3.1.0"), UpdateAction::None);
    }

    #[test]
    fn pill_reflects_setup_state() {
        assert_eq!(pill(None, None).0, "Starting");
        let mut s = Status::default();
        assert_eq!(pill(Some(&s), None), ("Shared", Tone::Success));
        s.setup
            .push("The streaming engine is not installed.".into());
        assert_eq!(pill(Some(&s), None), ("Needs setup", Tone::Warning));
        s.setup.push("Tailscale: not running".into());
        assert_eq!(pill(Some(&s), None), ("Tailscale off", Tone::Danger));
    }

    #[test]
    fn a_declined_prompt_says_what_to_do() {
        let e = setup_error(
            "the administrator prompt was declined or setup failed",
            Os::Windows,
        );
        assert!(e.contains("choose Yes"), "{e}");
        assert!(!e.contains("setup failed"), "no echo of the raw error: {e}");
        let e = setup_error("curl: (6) Could not resolve host", Os::Mac);
        assert!(e.starts_with("Curl"), "{e}");
        assert!(e.contains("run setup again"), "{e}");
    }
}

/// The Sharing page inside the unified window, rendered without a PC:
///
/// ```text
/// cargo test -p latch-host snapshots -- --ignored
/// ```
///
/// Nothing here reads or writes real settings or reaches the service: the
/// window and the page are both built headless.
#[cfg(test)]
mod snapshots {
    use super::*;
    use crate::product::NodeApp;
    use egui_kittest::kittest::Queryable;
    use latch_client::app::Page;
    use latch_core::api::Streamer;

    const MIN: egui::Vec2 = egui::vec2(640.0, 420.0);
    const TYPICAL: egui::Vec2 = egui::vec2(1280.0, 800.0);
    const LARGE: egui::Vec2 = egui::vec2(1920.0, 1200.0);

    fn out_dir() -> std::path::PathBuf {
        let dir =
            std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../target/ui-snapshots");
        std::fs::create_dir_all(&dir).expect("create snapshot dir");
        dir
    }

    fn save(img: image::RgbaImage, name: &str) {
        let path = out_dir().join(name);
        img.save(&path).expect("write png");
        eprintln!("wrote {}", path.display());
    }

    fn ready_status(os: &str) -> Status {
        Status {
            app: latch_core::APP_ID.into(),
            version: env!("CARGO_PKG_VERSION").into(),
            name: if os == "windows" {
                "GAMING-PC".into()
            } else {
                "MacBook-Pro".into()
            },
            os: os.into(),
            tailscale_ip: Some("100.64.0.10".into()),
            tailscale_login: Some("user@example.com".into()),
            lan_ip: Some("192.168.1.10".into()),
            mac: Some("02:00:00:00:00:01".into()),
            wake_ready: Some(true),
            wake_adapter: "Ethernet".into(),
            wake_adapter_description: "Example NIC".into(),
            wake_packet_age_secs: Some(42),
            fast_startup: Some(false),
            streamer: Streamer {
                kind: latch_core::STREAMER_KIND.into(),
                installed: true,
                running: true,
                api_ok: true,
                encoder: if os == "windows" {
                    "nvenc".into()
                } else {
                    "videotoolbox".into()
                },
                audio_problem: String::new(),
                ..Default::default()
            },
            power_allowed: true,
            nat: Some(latch_core::api::NatReport {
                udp: true,
                ipv4: true,
                ipv6: false,
                hard: Some(true),
                portmap: false,
                derp: "tok".into(),
            }),
            setup: vec![],
            log: vec![
                "Latch 4.1.1 listening on TCP 47850".into(),
                "listening for wake packets on UDP 9".into(),
                "Tailscale up as user@example.com (100.64.0.10)".into(),
                "the streaming engine is running and Latch is signed in".into(),
                "Wake-on-LAN ready on Ethernet (02:00:00:00:00:01)".into(),
                "paired \"MacBook-Pro\"".into(),
            ],
        }
    }

    fn ready() -> Shared {
        Shared {
            status: Some(ready_status("windows")),
            clients: vec![
                ("MacBook-Pro".into(), "0000000000000001".into()),
                ("Studio".into(), "0000000000000002".into()),
            ],
            gamepad_driver: Some(false),
            ..Default::default()
        }
    }

    fn needs_setup() -> Shared {
        let mut st = ready_status("windows");
        st.streamer = Streamer::default();
        st.wake_ready = Some(false);
        st.fast_startup = Some(true);
        st.setup = vec![
            "The streaming engine is not installed.".into(),
            "Wake-on-LAN is off on Ethernet.".into(),
            "Fast Startup is on, so the PC cannot be woken after a shutdown.".into(),
        ];
        Shared {
            status: Some(st),
            gamepad_driver: None,
            ..Default::default()
        }
    }

    fn failed() -> Shared {
        let mut st = ready_status("windows");
        st.tailscale_ip = None;
        st.tailscale_login = None;
        st.streamer = Streamer::default();
        st.setup = vec![
            "Tailscale: Tailscale is not installed. Install it and sign in.".into(),
            "The streaming engine is not installed.".into(),
        ];
        Shared {
            status: Some(st),
            setup_result: Some(Err("the administrator prompt was declined or setup failed (see C:\\Users\\Example User\\AppData\\Local\\Latch\\setup.log)".into())),
            setup_log: vec![
                "[14:02:11] Latch setup started".into(),
                "[14:02:11] Downloading the streaming engine".into(),
                "  engine: curl: (6) Could not resolve host: api.github.com".into(),
            ],
            ..Default::default()
        }
    }

    fn states() -> Vec<(&'static str, Os, Shared)> {
        vec![
            ("ready", Os::Windows, ready()),
            ("setup", Os::Windows, needs_setup()),
            (
                "setup-running",
                Os::Windows,
                Shared {
                    setup_running: true,
                    ..needs_setup()
                },
            ),
            ("setup-failed", Os::Windows, failed()),
            (
                "setup-done",
                Os::Windows,
                Shared {
                    setup_result: Some(Ok(())),
                    ..ready()
                },
            ),
            (
                "mac-ready",
                Os::Mac,
                Shared {
                    status: Some(ready_status("macOS")),
                    ..Default::default()
                },
            ),
            (
                "mac-setup",
                Os::Mac,
                Shared {
                    status: Some({
                        let mut s = ready_status("macOS");
                        s.streamer = Streamer::default();
                        s.setup = vec!["The streaming engine is not installed.".into()];
                        s
                    }),
                    ..Default::default()
                },
            ),
            ("starting", Os::Windows, Shared::default()),
            (
                "stopped",
                Os::Windows,
                Shared {
                    service_error: Some(STOPPED.into()),
                    ..Default::default()
                },
            ),
            (
                "mac-viewer",
                Os::Mac,
                Shared {
                    service_error: Some(STOPPED.into()),
                    viewer_only: true,
                    ..Default::default()
                },
            ),
        ]
    }

    fn build(
        shared: Shared,
        os: Os,
        size: egui::Vec2,
        ppp: f32,
        gpu: bool,
    ) -> egui_kittest::Harness<'static, NodeApp> {
        let mut builder = egui_kittest::Harness::builder()
            .with_size(size)
            .with_pixels_per_point(ppp)
            .with_max_steps(8);
        if gpu {
            builder = builder.wgpu();
        }
        let mut harness = builder.build_eframe(move |cc| {
            let mut app = NodeApp::headless(cc, Default::default(), shared, os);
            app.client_mut().open_page(Page::Sharing);
            app
        });
        harness.run_steps(3);
        harness
    }

    /// Every widget sits inside the window, and no two controls overlap.
    fn assert_fits(h: &egui_kittest::Harness<'_, NodeApp>, width: f32) {
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

    #[test]
    fn every_sharing_state_fits_the_minimum_and_a_large_window() {
        for (name, os, shared) in states() {
            eprintln!("checking {name}");
            let h = build(shared, os, MIN, 1.0, false);
            assert_fits(&h, MIN.x);
        }
        for (name, os, shared) in states() {
            eprintln!("checking {name}");
            let h = build(shared, os, LARGE, 1.0, false);
            assert_fits(&h, LARGE.x);
        }
    }

    #[test]
    fn the_page_is_a_tab_of_the_window_and_the_machine_leads_the_list() {
        let mut h = build(ready(), Os::Windows, MIN, 1.0, false);
        assert!(h.query_by_label("GAMING-PC").is_some());
        h.get_by_label("Machines").click();
        h.run_steps(3);
        assert!(
            h.query_by_label("GAMING-PC").is_some(),
            "this PC leads the list"
        );
        assert!(h.query_by_label("Manage").is_some());
    }

    #[test]
    fn windows_only_rows_stay_off_a_mac() {
        let tall = egui::vec2(1280.0, 2400.0);
        let h = build(ready(), Os::Windows, tall, 1.0, false);
        assert!(h.query_by_label("Wake-on-LAN").is_some());
        assert!(h
            .query_all_by_label("Keep this PC awake while plugged in")
            .next()
            .is_some());
        let h = build(
            Shared {
                status: Some(ready_status("macOS")),
                ..Default::default()
            },
            Os::Mac,
            tall,
            1.0,
            false,
        );
        assert!(h.query_by_label("Wake-on-LAN").is_none());
        assert!(h.query_by_label("Game controllers").is_none());
        assert!(
            h.query_by_label_contains("Keep this").is_none(),
            "keeping awake is Windows-only; the switch would do nothing on a Mac"
        );
    }

    #[test]
    fn unpair_asks_first_and_a_headless_page_touches_nothing() {
        let tall = egui::vec2(1280.0, 2400.0);
        let mut h = build(ready(), Os::Windows, tall, 1.0, false);
        h.get_all_by_label("Unpair…").next().unwrap().click();
        h.run_steps(3);
        assert!(
            h.query_by_label("Unpair").is_some(),
            "the confirming button"
        );
        assert!(h.query_by_label("Keep").is_some());
        h.get_by_label("Run setup again").click();
        h.run_steps(3);
        assert!(
            h.query_by_label_contains("Waiting for the administrator prompt")
                .is_some(),
            "setup shows progress; headless, nothing ran"
        );
    }

    #[test]
    #[ignore = "renders with a GPU; run on demand to review the UI"]
    fn snapshots_sharing() {
        let key = ["ready", "setup", "setup-failed"];
        for (name, os, _) in states() {
            let sizes: Vec<(egui::Vec2, f32)> = if key.contains(&name) {
                vec![
                    (MIN, 1.0),
                    (MIN, 2.0),
                    (TYPICAL, 1.0),
                    (TYPICAL, 2.0),
                    (LARGE, 1.0),
                    (LARGE, 2.0),
                ]
            } else {
                vec![(MIN, 2.0), (TYPICAL, 2.0)]
            };
            for (size, ppp) in sizes {
                let shared = states().into_iter().find(|(n, ..)| *n == name).unwrap().2;
                let mut h = build(shared, os, size, ppp, true);
                save(
                    h.render().unwrap(),
                    &format!(
                        "sharing-{name}-{}x{}@{}x.png",
                        size.x as u32, size.y as u32, ppp as u32
                    ),
                );
            }
        }
        for (name, os, shared) in states() {
            let mut h = build(shared, os, egui::vec2(1280.0, 1800.0), 1.0, true);
            save(h.render().unwrap(), &format!("sharing-{name}-full.png"));
        }
        let mut h = build(ready(), Os::Windows, egui::vec2(1280.0, 1800.0), 1.0, true);
        h.get_by_label("Service log").click();
        h.run_steps(3);
        save(h.render().unwrap(), "sharing-log-open-full.png");
        let mut h = build(ready(), Os::Windows, TYPICAL, 2.0, true);
        h.get_by_label("Machines").click();
        h.run_steps(3);
        save(h.render().unwrap(), "sharing-lobby-row-1280x800@2x.png");
    }
}
