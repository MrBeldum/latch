//! The background control service: what the other machines on the tailnet talk to.
//!
//! One TCP listener on every interface, one rule for who gets an answer:
//! loopback (the host's own control panel), or a tailnet machine that
//! `tailscale whois` says belongs to the account this one is signed in as.
//! A tagged machine (the relay VPS) belongs to no account, so it answers
//! the members of its own tailnet. Everyone else gets a 403 and nothing
//! more: a machine shared in from someone else's tailnet can reach the
//! port, but may not pair, sleep the PC, read its clipboard or push it an
//! executable.
//!
//! Under userspace Tailscale (a container's tailscaled) every tailnet peer
//! arrives from loopback too, so there loopback alone proves nothing:
//! `tailscale whois` of the connection's own port says who is behind it.

use crate::clipboard;
use crate::config::HostConfig;
use crate::streamer::{self, Api, Install};
use crate::update;
use crate::wake::{self, WakeInfo};
use anyhow::{Context, Result};
use latch_core::api::{
    Ack, Clipboard, DisplayRequest, NatReport, PinRequest, PowerRequest, Status, Streamer,
    CLIPBOARD_PATH, UPDATE_PATH,
};
use latch_core::http::{self, Request, Response};
use latch_core::tailscale::{self, is_tailnet_ip};
use latch_core::CONTROL_PORT;
use parking_lot::Mutex;
use std::collections::{HashMap, VecDeque};
use std::net::{IpAddr, Ipv4Addr, SocketAddr, TcpListener};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

const LOG_LINES: usize = 80;
/// How long a `whois` answer is trusted before asking again.
const AUTH_TTL: Duration = Duration::from_secs(60);
/// How long an answer is remembered, so the same peer is not logged again.
const AUTH_REMEMBER: Duration = Duration::from_secs(3600);
const AUTH_CACHE_LIMIT: usize = 256;
/// How long a PIN the engine took counts as entered for the Mac's retries:
/// longer than the engine waits for the handshake.
const PIN_TAKEN_FOR: Duration = Duration::from_secs(30);
/// Ticks (of 5 s) between runs of `tailscale netcheck`: it takes seconds
/// and the network does not move often.
const NETCHECK_TICKS: u64 = 120;

pub struct Service {
    cfg: Mutex<HostConfig>,
    /// This machine's name, re-read now and then: asking macOS or Linux
    /// runs a program, which every status request used to do.
    name: Mutex<String>,
    tailscale: Mutex<Result<tailscale::Status, String>>,
    wake: Mutex<WakeInfo>,
    /// When a magic packet for this PC last arrived.
    wake_seen: Mutex<Option<Instant>>,
    /// Whether the virtual display lists every screen size a viewer can ask for;
    /// `None` on a PC without one. See `virtual_display`.
    virtual_display: Mutex<Option<bool>>,
    install: Mutex<Option<Install>>,
    streamer: Mutex<Streamer>,
    log: Arc<Mutex<VecDeque<String>>>,
    /// `whois` answers for recent peers: when, and whether they may ask.
    auth: Mutex<HashMap<IpAddr, (Instant, bool)>>,
    /// The last `whois` failure logged.
    whois_failed: Mutex<Option<String>>,
    /// Whether Tailscale here has a kernel TUN device, as last reported.
    /// Kept when a later `status` fails: `Some(false)` (userspace) is what
    /// makes loopback a peer's address too, and must not lapse.
    tun: Mutex<Option<bool>>,
    /// The last line logged about a peer forwarded through loopback.
    forwarded_logged: Mutex<Option<String>>,
    /// This PC's side of the NAT story, from `tailscale netcheck`.
    nat: Mutex<Option<NatReport>>,
    nat_running: AtomicBool,
    update_running: Arc<AtomicBool>,
    /// A PIN the engine took and is pairing with, and when: the Mac's
    /// retries for it are answered without asking the engine again, which
    /// is busy with that very handshake.
    pin_taken: Mutex<Option<(String, Instant)>>,
    /// The last PIN refusal logged.
    pin_refused: Mutex<Option<String>>,
}

impl Service {
    pub fn new() -> Self {
        Self {
            cfg: Mutex::new(HostConfig::load()),
            name: Mutex::new(latch_core::config::machine_name()),
            tailscale: Mutex::new(Err("not checked yet".into())),
            wake: Mutex::new(WakeInfo::default()),
            wake_seen: Mutex::new(None),
            virtual_display: Mutex::new(None),
            install: Mutex::new(None),
            streamer: Mutex::new(Streamer::default()),
            log: Arc::new(Mutex::new(VecDeque::new())),
            auth: Mutex::new(HashMap::new()),
            whois_failed: Mutex::new(None),
            tun: Mutex::new(None),
            forwarded_logged: Mutex::new(None),
            nat: Mutex::new(None),
            nat_running: AtomicBool::new(false),
            update_running: Arc::new(AtomicBool::new(false)),
            pin_taken: Mutex::new(None),
            pin_refused: Mutex::new(None),
        }
    }

    pub fn log(&self, msg: impl Into<String>) {
        push_log(&self.log, msg.into());
    }

    /// A viewer is connected right now. A Desktop left running for the
    /// viewer to resume does not count: see [`streamer::Session`].
    fn someone_watching(&self) -> bool {
        let install = self.install.lock().clone();
        streamer::session(install.as_ref()).watched()
    }

    pub fn run_arc(self, replacing: bool) -> Result<()> {
        Arc::new(self).run(replacing)
    }

    /// Bind, then serve forever. Fails only when the port is taken, which
    /// means another copy is already running. `replacing` is the service an
    /// update just started: the old one is still answering its last request,
    /// so wait for the port rather than give up.
    pub fn run(self: Arc<Self>, replacing: bool) -> Result<()> {
        if !starts_with_window(&self.cfg.lock()) {
            // A Mac that is only a viewer: an older window (or anything else)
            // that starts the service must not share it. Exit 0, so launchd
            // does not try again.
            self.log("this Mac is not shared (Share this Mac is off); the service exits");
            return Ok(());
        }
        let addr = SocketAddr::from((Ipv4Addr::UNSPECIFIED, CONTROL_PORT));
        let listener = if replacing {
            let deadline = Instant::now() + Duration::from_secs(60);
            loop {
                match TcpListener::bind(addr) {
                    Ok(l) => break l,
                    Err(e) if Instant::now() < deadline => {
                        tracing::info!("waiting for the old service to stop: {e}");
                        std::thread::sleep(Duration::from_millis(500));
                    }
                    Err(e) => return Err(e).context("bind after an update"),
                }
            }
        } else {
            match TcpListener::bind(addr) {
                Ok(l) => l,
                Err(e) if service_alive() => {
                    // launchd, systemd or the window started this copy while
                    // another already serves. Exit 0: a supervisor that
                    // restarts on failure then leaves the running one alone
                    // instead of respawning this one every few seconds.
                    self.log(format!(
                        "another Latch service already answers on TCP {CONTROL_PORT}; this copy exits ({e})"
                    ));
                    return Ok(());
                }
                Err(e) => return Err(e).context(format!("bind TCP {CONTROL_PORT}")),
            }
        };
        self.log(format!(
            "Latch Host {} listening on TCP {CONTROL_PORT}",
            env!("CARGO_PKG_VERSION")
        ));
        if let Ok(exe) = std::env::current_exe() {
            if replacing {
                self.log("updated: the previous version has been replaced");
                update::tidy(&exe);
            }
            self.ensure_autostart(&exe);
        }
        // Before the first request: under userspace Tailscale every tailnet
        // peer arrives from loopback, so that has to be known first.
        if let Ok(st) = tailscale::status() {
            self.note_tun(&st);
        }
        let refresher = self.clone();
        std::thread::spawn(move || refresher.refresh_loop());
        // (refresh_loop takes the Arc so netcheck can run on its own thread.)
        let svc = self.clone();
        if let Err(e) = wake::listen(move |mac, from| svc.wake_packet(mac, from)) {
            self.log(format!("not listening for wake packets: {e:#}"));
        }
        let handler = self.clone();
        let authorizer = self.clone();
        http::serve_with_peer_check(
            listener,
            move |peer| authorizer.authorized(peer.ip()),
            move |peer, req| handler.handle(peer, req),
        );
        Ok(())
    }

    /// A PC nobody can reach in person has to come back by itself after a
    /// restart, so the logon entry is kept unless the owner turned it off.
    fn ensure_autostart(&self, exe: &std::path::Path) {
        if !self.cfg.lock().start_with_windows {
            return;
        }
        // Rewrite even when a Run value already exists: install-host.ps1
        // may have moved the exe after the first setup, and the old path
        // would start nothing at logon. Written only: this is the service,
        // so it must not stop itself or start a second copy.
        if let Err(e) = crate::setup::register_autostart(exe) {
            self.log(format!("could not register start at logon: {e:#}"));
        }
    }

    fn refresh_loop(self: Arc<Self>) {
        let mut tick: u64 = 0;
        loop {
            self.refresh(tick);
            tick += 1;
            std::thread::sleep(Duration::from_secs(5));
        }
    }

    /// Cheap checks every tick; the slower probes on a longer cadence.
    fn refresh(self: &Arc<Self>, tick: u64) {
        *self.cfg.lock() = HostConfig::load();
        let stay_awake = self.cfg.lock().stay_awake;
        crate::power::keep_awake(stay_awake);
        if tick == 0 && stay_awake {
            self.log("keeping this PC awake so Tailscale stays reachable");
        }

        let ts = tailscale::status().map_err(|e| e.to_string());
        if let Ok(st) = &ts {
            self.note_tun(st);
        }
        let came_up;
        {
            let mut cur = self.tailscale.lock();
            came_up = matches!((&*cur, &ts), (Err(_), Ok(_)));
            match (&*cur, &ts) {
                (Ok(_), Err(e)) => self.log(format!("Tailscale: {e}")),
                (Err(_), Ok(s)) => self.log(format!(
                    "Tailscale up as {} ({})",
                    s.self_login().unwrap_or("?"),
                    s.self_node
                        .ipv4()
                        .map(|ip| ip.to_string())
                        .unwrap_or_default()
                )),
                _ => {}
            }
            *cur = ts;
        }
        if self.tailscale.lock().is_ok() && (came_up || tick % NETCHECK_TICKS == 1) {
            self.spawn_netcheck();
        }

        let install = streamer::find();
        let running = install.is_some() && streamer::running();
        let cfg = self.cfg.lock().clone();
        let api_ok = if running
            && cfg.has_creds()
            && (tick.is_multiple_of(6) || !self.streamer.lock().api_ok)
        {
            Api {
                user: &cfg.sunshine_user,
                pass: &cfg.sunshine_pass,
            }
            .ok()
        } else {
            running && self.streamer.lock().api_ok
        };
        // Apply the streaming profile on any engine Latch can log into,
        // after any old app has ended. The marker survives panel/config saves;
        // v4 drops Linux's frame-rate floor (see setup::LINUX_ENGINE_CONF).
        if tick.is_multiple_of(6) && api_ok && install.is_some() {
            if let Ok(dir) = latch_core::config::data_dir() {
                let marker = dir.join("stream-profile-v4");
                if !marker.exists() {
                    let api = Api {
                        user: &cfg.sunshine_user,
                        pass: &cfg.sunshine_pass,
                    };
                    match api.upgrade_stream_profile(install.as_ref()) {
                        Ok(true) => {
                            if let Err(e) = std::fs::write(&marker, "1") {
                                self.log(format!("could not save stream profile version: {e}"));
                            }
                            self.log(if cfg!(target_os = "linux") {
                                "stream profile ready: constant bitrate, the viewer's frame rate"
                            } else {
                                "stream profile ready: match client display, constant bitrate, 60 fps"
                            });
                        }
                        Ok(false) => {} // Retried once nobody watches.
                        Err(e) => self.log(format!("stream profile could not be applied: {e:#}")),
                    }
                }
            }
        }
        // The encoder Sunshine picked and whether it could capture audio,
        // from its log; re-read now and then since Sunshine restarts on
        // its own after setup and every session tries the audio device.
        // The X screen it captures is re-read with them: a VPS desktop can
        // be restarted at another size.
        let (encoder, audio_problem, capture_size) =
            if tick.is_multiple_of(12) || self.streamer.lock().encoder.is_empty() {
                let log = install.as_ref().and_then(streamer::log_text);
                (
                    log.as_deref()
                        .and_then(streamer::encoder_in)
                        .unwrap_or_default(),
                    log.as_deref()
                        .and_then(streamer::audio_problem_in)
                        .unwrap_or_default(),
                    streamer::capture_size(),
                )
            } else {
                let cur = self.streamer.lock();
                (
                    cur.encoder.clone(),
                    cur.audio_problem.clone(),
                    cur.capture_size,
                )
            };
        let cores = std::thread::available_parallelism().map_or(1, |n| n.get());
        let max_fps = streamer::max_fps(&encoder, cores);
        let st = Streamer {
            kind: install
                .as_ref()
                .map(|i| i.kind.to_string())
                .unwrap_or_default(),
            installed: install.is_some(),
            running,
            api_ok,
            encoder,
            audio_problem,
            capture_size,
            max_fps,
        };
        {
            let mut cur = self.streamer.lock();
            if (cur.installed, cur.running, cur.api_ok) != (st.installed, st.running, st.api_ok) {
                self.log(match (&st.installed, &st.running, &st.api_ok) {
                    (false, _, _) => "the streaming engine is not installed".to_string(),
                    (true, false, _) => "Latch is installed but not sharing yet".to_string(),
                    (true, true, false) => {
                        "Latch is running; the streaming engine has no working login yet".into()
                    }
                    (true, true, true) => "Latch is sharing this machine".to_string(),
                });
            }
            if cur.encoder != st.encoder && !st.encoder.is_empty() {
                self.log(if st.encoder == "software" {
                    "encoding in software (no GPU encoder); using every CPU core".into()
                } else {
                    format!("encoding with {}", st.encoder)
                });
            }
            if cur.audio_problem != st.audio_problem {
                self.log(if st.audio_problem.is_empty() {
                    "audio capture is working again".into()
                } else {
                    format!(
                        "no sound to send: {}. A PC with no monitor or speakers needs a virtual audio device",
                        st.audio_problem
                    )
                });
            }
            *cur = st;
        }
        *self.install.lock() = install;
        if tick.is_multiple_of(6) {
            let install = self.install.lock().clone();
            if let Some(install) = install {
                let audio_problem = self.streamer.lock().audio_problem.clone();
                if !streamer::running() {
                    if let Err(e) = streamer::start(&install) {
                        self.log(format!("streaming engine: {e:#}"));
                    }
                } else if cfg!(windows) && !audio_problem.is_empty() {
                    let _ = crate::audio::take_over_engine();
                }
            }
        }

        if tick.is_multiple_of(12) {
            *self.name.lock() = latch_core::config::machine_name();
            let w = wake::probe();
            let mut cur = self.wake.lock();
            if *cur != w {
                match (&w.mac, w.magic_packet) {
                    (Some(mac), Some(true)) => self.log(format!("Wake-on-LAN ready on {} ({mac})", w.adapter)),
                    (Some(_), Some(false)) => self.log(format!("Wake-on-LAN is off on {}; run setup", w.adapter)),
                    (Some(_), None) => self.log(format!("Wake-on-LAN state on {} is unknown", w.adapter)),
                    (None, _) => self.log("no wired adapter with a MAC found; other machines will not be able to wake this one"),
                }
                *cur = w;
            }
            let v = crate::virtual_display::probe();
            let mut cur = self.virtual_display.lock();
            if *cur != v {
                match v {
                    Some(true) => {
                        self.log("the virtual display lists every screen size a viewer can ask for")
                    }
                    Some(false) => self
                        .log("the virtual display is missing screen sizes a viewer can ask for; run setup"),
                    None => {}
                }
                *cur = v;
            }
        }
    }

    /// `tailscale netcheck` on its own thread, once at a time.
    fn spawn_netcheck(self: &Arc<Self>) {
        if self.nat_running.swap(true, Ordering::AcqRel) {
            return;
        }
        let svc = self.clone();
        std::thread::spawn(move || {
            match tailscale::netcheck() {
                Ok(n) => {
                    let home = match &*svc.tailscale.lock() {
                        Ok(st) => st.self_node.relay.clone(),
                        Err(_) => String::new(),
                    };
                    let report = n.report_near(&home);
                    let mut cur = svc.nat.lock();
                    if cur.as_ref().is_none_or(|c| nat_changed(c, &report)) {
                        svc.log(describe_nat(&report));
                    }
                    *cur = Some(report);
                }
                Err(e) => tracing::info!("netcheck: {e}"),
            }
            svc.nat_running.store(false, Ordering::Release);
        });
    }

    /// A magic packet arrived while awake: remember when, for the status.
    fn wake_packet(&self, mac: latch_core::wake::MacAddr, from: SocketAddr) {
        let own = self.wake.lock().mac.clone();
        if own.as_deref() != Some(mac.to_string().as_str()) {
            return;
        }
        let mut seen = self.wake_seen.lock();
        if seen.is_none_or(|t| t.elapsed() > Duration::from_secs(5)) {
            self.log(format!("wake packet received from {from}"));
        }
        *seen = Some(Instant::now());
    }

    pub fn status(&self, with_log: bool) -> Status {
        let cfg = self.cfg.lock().clone();
        let ts = self.tailscale.lock();
        let wake = self.wake.lock().clone();
        let streamer = self.streamer.lock().clone();
        let mut setup = Vec::new();
        if let Err(e) = &*ts {
            setup.push(format!("Tailscale: {e}. Install it and sign in."));
        }
        if !streamer.installed {
            setup.push("The streaming engine is not installed.".into());
        } else if !streamer.running {
            setup.push("The streaming engine is installed but not running.".into());
        } else if !streamer.api_ok {
            setup.push("Latch has no working login for the streaming engine.".into());
        } else if cfg!(windows)
            && latch_core::legacy::is_own_streamer_kind(&streamer.kind)
            && !crate::streamer::find()
                .is_some_and(|install| crate::brand::is_branded_cached(&install.dir))
        {
            setup.push(
                "The streaming engine still shows its upstream name and icon in Task Manager."
                    .into(),
            );
        }
        if wake.magic_packet == Some(false) {
            setup.push(format!("Wake-on-LAN is off on {}.", wake.adapter));
        }
        if wake.fast_startup == Some(true) {
            setup.push("Fast Startup is on, so the PC cannot be woken after a shutdown.".into());
        }
        if *self.virtual_display.lock() == Some(false) {
            setup.push(
                "The virtual display does not list every screen size a viewer can ask for.".into(),
            );
        }
        Status {
            app: latch_core::APP_ID.into(),
            version: env!("CARGO_PKG_VERSION").into(),
            name: self.name.lock().clone(),
            os: ts
                .as_ref()
                .ok()
                .map(|s| s.self_node.os.clone())
                .filter(|s| !s.is_empty())
                .unwrap_or_else(|| {
                    if cfg!(windows) {
                        "windows".into()
                    } else if cfg!(target_os = "macos") {
                        "macOS".into()
                    } else if cfg!(target_os = "linux") {
                        "linux".into()
                    } else {
                        std::env::consts::OS.into()
                    }
                }),
            tailscale_ip: ts
                .as_ref()
                .ok()
                .and_then(|s| s.self_node.ipv4())
                .map(|ip| ip.to_string()),
            tailscale_login: ts
                .as_ref()
                .ok()
                .and_then(|s| s.self_login().map(str::to_string)),
            lan_ip: wake.lan_ip.map(|ip| ip.to_string()),
            mac: wake.mac.clone(),
            wake_ready: wake.magic_packet,
            wake_adapter: wake.adapter.clone(),
            wake_adapter_description: wake.description.clone(),
            wake_packet_age_secs: self.wake_seen.lock().map(|t| t.elapsed().as_secs()),
            fast_startup: wake.fast_startup,
            streamer,
            power_allowed: cfg.power_allowed,
            nat: self.nat.lock().clone(),
            setup,
            log: if with_log {
                self.log.lock().iter().cloned().collect()
            } else {
                Vec::new()
            },
        }
    }

    fn note_tun(&self, st: &tailscale::Status) {
        if let Some(tun) = st.tun {
            *self.tun.lock() = Some(tun);
        }
    }

    /// Who sent a request, once it has been read. A loopback connection is
    /// a process on this machine, unless Tailscale runs in userspace mode:
    /// then it may be a tailnet peer that tailscaled relayed, which must
    /// pass [`Owner::admits`] like any other and never counts as local.
    /// Asked after the request is read, when tailscaled has certainly
    /// recorded which peer the connection is from (it does so only after
    /// it has connected to this port). Never cached: every relayed peer
    /// shares 127.0.0.1.
    fn caller(&self, peer: SocketAddr) -> Caller {
        if !peer.ip().is_loopback() {
            return Caller::Peer {
                allowed: self.authorized(peer.ip()),
            };
        }
        let tun = *self.tun.lock();
        let owner = self.tailscale.lock().as_ref().ok().map(Owner::of);
        loopback_caller(tun, owner.as_ref(), || {
            let found = tailscale::whois_addr(peer);
            let line = match &found {
                Ok(Some(w)) => Some(format!(
                    "{} ({}) asked through userspace Tailscale{}",
                    w.node.computed_name,
                    w.user_profile.login_name,
                    if owner.as_ref().is_some_and(|o| o.admits(w)) {
                        ""
                    } else {
                        ": not this account, refused"
                    }
                )),
                Ok(None) => None,
                Err(e) => Some(format!("whois {peer}: {e}; refused")),
            };
            if let Some(line) = line {
                let mut last = self.forwarded_logged.lock();
                if last.as_deref() != Some(line.as_str()) {
                    self.log(line.clone());
                    *last = Some(line);
                }
            }
            found
        })
    }

    /// Loopback may connect (see [`Service::caller`] for what it may do);
    /// a tailnet peer must pass [`Owner::admits`]. A `whois` answer is
    /// remembered for [`AUTH_TTL`], a refusal included; a failed `whois` is
    /// not remembered.
    fn authorized(&self, ip: IpAddr) -> bool {
        if ip.is_loopback() {
            return true;
        }
        if !is_tailnet_ip(ip) {
            return false;
        }
        let now = Instant::now();
        let previous = self.auth.lock().get(&ip).copied();
        if let Some((at, allowed)) = previous {
            if now - at < AUTH_TTL {
                return allowed;
            }
        }
        // Copied out so the lock is not held across the `whois` call.
        let owner = match &*self.tailscale.lock() {
            Ok(st) => Owner::of(st),
            Err(_) => return false,
        };
        let allowed = match tailscale::whois(ip) {
            Ok(w) => {
                let allowed = owner.admits(&w);
                // Once per peer, and again if the answer changes or it has
                // been away an hour: a Mac polling every few seconds used
                // to put a line here every minute, 96% of a Mac's log.
                if previous.map(|(_, a)| a) != Some(allowed) {
                    self.log(format!(
                        "{} ({}) asked{}",
                        w.node.computed_name,
                        w.user_profile.login_name,
                        if allowed {
                            ""
                        } else {
                            ": not this account, refused"
                        }
                    ));
                }
                allowed
            }
            Err(e) => {
                // Not remembered, so asked again on the next request; say
                // so once rather than at every poll.
                let line = format!("whois {ip}: {e}");
                let mut last = self.whois_failed.lock();
                if last.as_deref() != Some(line.as_str()) {
                    self.log(line.clone());
                    *last = Some(line);
                }
                return false;
            }
        };
        let mut cache = self.auth.lock();
        cache.retain(|_, (at, _)| at.elapsed() < AUTH_REMEMBER);
        if cache.len() >= AUTH_CACHE_LIMIT {
            if let Some(oldest) = cache
                .iter()
                .min_by_key(|(_, (at, _))| *at)
                .map(|(ip, _)| *ip)
            {
                cache.remove(&oldest);
            }
        }
        cache.insert(ip, (Instant::now(), allowed));
        allowed
    }

    fn handle(&self, peer: SocketAddr, req: &Request) -> Response {
        // The API is for native clients. Deny browser requests even when
        // DNS rebinding makes a malicious page appear same-origin.
        if req.header("origin").is_some()
            || req.header("sec-fetch-site").is_some()
            || req.header("host").is_some_and(|host| !control_host(host))
        {
            return Response::json(
                403,
                &Ack::err("browser access to the control API is disabled"),
            );
        }
        let caller = self.caller(peer);
        if !caller.allowed() {
            return Response::json(
                403,
                &Ack::err("not a machine on this PC's Tailscale account"),
            );
        }
        // A web page open in a browser on an authorised machine (this PC,
        // or the Mac) can POST here without asking: a cross-origin form
        // post needs no preflight, and sleep, quit or a clipboard write
        // happen whether or not the page can read the reply. Latch's own
        // callers send JSON or an executable, neither of which a browser
        // can send cross-origin without a preflight that nothing here
        // answers.
        if req.method == "POST" && !sent_by_latch(req) {
            return Response::json(
                403,
                &Ack::err("a Latch request carries JSON or an executable"),
            );
        }
        // The service log and /v1/quit are for this machine's own window.
        let local = caller == Caller::Local;
        match (req.method.as_str(), req.path.as_str()) {
            ("GET", "/v1/status") => Response::json(200, &self.status(local)),
            ("GET", "/v1/display") => Response::json(200, &self.display()),
            ("POST", "/v1/display") => self.set_display(peer.ip(), req),
            ("POST", "/v1/pin") => self.pin(peer.ip(), req),
            ("POST", "/v1/power") => self.power(peer.ip(), req),
            ("POST", p) if p == UPDATE_PATH => self.update(peer.ip(), req),
            ("GET", p) if p == CLIPBOARD_PATH => match clipboard::read() {
                Ok(c) => Response::json(200, &c),
                Err(e) => Response::json(500, &Ack::err(format!("clipboard: {e}"))),
            },
            ("POST", p) if p == CLIPBOARD_PATH => match req.json::<Clipboard>() {
                Ok(c) => match clipboard::write(&c.text) {
                    Ok(()) => Response::json(200, &Ack::ok()),
                    Err(e) => Response::json(500, &Ack::err(format!("clipboard: {e}"))),
                },
                Err(_) => Response::json(400, &Ack::err("expected {\"text\"}")),
            },
            ("POST", "/v1/quit") if local => {
                self.log("control panel asked the service to stop");
                std::thread::spawn(|| {
                    std::thread::sleep(Duration::from_millis(300));
                    std::process::exit(0);
                });
                Response::json(200, &Ack::ok())
            }
            _ => Response::json(404, &Ack::err("no such route")),
        }
    }

    /// A newer `latch-host.exe` from the Mac: stage it, answer, then swap
    /// it in and hand over. See [`crate::update`].
    fn update(&self, from: IpAddr, req: &Request) -> Response {
        if !cfg!(windows) {
            // A Mac replaces its own app from GitHub and a container is
            // rebuilt; swapping a Windows executable in here would only
            // fail to start and be retried.
            return Response::json(
                400,
                &Ack::err(
                    "only a Windows PC takes a pushed latch-host.exe; this machine updates itself",
                ),
            );
        }
        if self.update_running.swap(true, Ordering::AcqRel) {
            return Response::json(409, &Ack::err("an update is already being installed"));
        }
        let engine_running = self.streamer.lock().running;
        if update::refuse_self_update(engine_running, self.someone_watching()) {
            self.update_running.store(false, Ordering::Release);
            self.log("update deferred: a stream is running");
            return Response::json(
                409,
                &Ack::err("a stream is running; the update is retried after it ends"),
            );
        }
        let exe = match std::env::current_exe() {
            Ok(e) => e,
            Err(e) => {
                self.update_running.store(false, Ordering::Release);
                return Response::json(500, &Ack::err(format!("own path unknown: {e}")));
            }
        };
        match update::stage(req, &exe) {
            Ok(version) => {
                self.log(format!(
                    "updating to {version}: {from} sent the new Latch Host"
                ));
                let running = self.update_running.clone();
                let log = self.log.clone();
                std::thread::spawn(move || {
                    std::thread::sleep(Duration::from_millis(500));
                    match update::apply(&exe) {
                        Ok(()) => {
                            tracing::info!("handing over to {version}");
                            std::process::exit(0);
                        }
                        Err(e) => {
                            push_log(&log, format!("update to {version} failed: {e:#}"));
                            running.store(false, Ordering::Release);
                        }
                    }
                });
                Response::json(200, &Ack::ok())
            }
            Err(r) => {
                self.update_running.store(false, Ordering::Release);
                self.log(format!("update refused: {}", r.message()));
                Response::json(r.status(), &Ack::err(r.message()))
            }
        }
    }

    fn pin(&self, from: IpAddr, req: &Request) -> Response {
        let Ok(p) = req.json::<PinRequest>() else {
            return Response::json(400, &Ack::err("expected {\"pin\",\"name\"}"));
        };
        if p.pin.len() != 4 || !p.pin.chars().all(|c| c.is_ascii_digit()) {
            return Response::json(400, &Ack::err("the PIN is four digits"));
        }
        let cfg = self.cfg.lock().clone();
        if !cfg.has_creds() || !self.streamer.lock().api_ok {
            return Response::json(
                502,
                &Ack::err(
                    "Latch cannot log in to the streaming engine on this PC; run setup there",
                ),
            );
        }
        if self
            .pin_taken
            .lock()
            .as_ref()
            .is_some_and(|(pin, at)| *pin == p.pin && at.elapsed() < PIN_TAKEN_FOR)
        {
            return Response::json(200, &Ack::ok());
        }
        let api = Api {
            user: &cfg.sunshine_user,
            pass: &cfg.sunshine_pass,
        };
        match api.submit_pin(&p.pin, &p.name, from) {
            Ok(streamer::Pin::Paired) => {
                self.log(format!("paired \"{}\"", p.name));
                Response::json(200, &Ack::ok())
            }
            Ok(streamer::Pin::Taken) => {
                *self.pin_taken.lock() = Some((p.pin.clone(), Instant::now()));
                self.log(format!(
                    "PIN from \"{}\" entered; it finishes pairing itself",
                    p.name
                ));
                Response::json(200, &Ack::ok())
            }
            Ok(streamer::Pin::NotYet) => {
                // Not an error: the Mac retries within a second. A 200 with
                // the reason is what it shows while it waits; any other
                // status reads to it as "Latch isn't answering".
                Response::json(
                    200,
                    &Ack::err(
                        "waiting for this machine's pairing request to reach the streaming engine",
                    ),
                )
            }
            Err(e) => {
                // The Mac retries every second for over a minute; one line
                // per reason, not one per retry.
                let line = format!("PIN from \"{}\" refused: {e:#}", p.name);
                let mut last = self.pin_refused.lock();
                if last.as_deref() != Some(line.as_str()) {
                    self.log(line.clone());
                    *last = Some(line);
                }
                Response::json(502, &Ack::err(e.to_string()))
            }
        }
    }

    /// Everything that decides whether a capture can see the desktop.
    fn display(&self) -> serde_json::Value {
        let cfg = self.cfg.lock().clone();
        let windows =
            crate::display::probe().unwrap_or_else(|e| serde_json::json!({"error": e.to_string()}));
        let color = crate::display::advanced_color()
            .unwrap_or_else(|e| serde_json::json!({"error": e.to_string()}));
        let sunshine = Api {
            user: &cfg.sunshine_user,
            pass: &cfg.sunshine_pass,
        }
        .display_diagnostics();
        serde_json::json!({
            "windows": windows,
            "advanced_color": color,
            "sunshine": sunshine,
        })
    }

    /// Turn the PC's HDR desktop off (or back on). This is the one display
    /// setting the Mac can change: an HDR desktop on a PC with no monitor
    /// captures as black, and nobody can reach the PC's settings to fix it
    /// when the picture is the thing that is broken.
    fn set_display(&self, from: IpAddr, req: &Request) -> Response {
        let Ok(want) = req.json::<DisplayRequest>() else {
            return Response::json(400, &Ack::err("expected {\"advanced_color\": true|false}"));
        };
        match crate::display::set_advanced_color(want.advanced_color) {
            Ok(state) => {
                self.log(format!(
                    "{from} turned the HDR desktop {}",
                    if want.advanced_color { "on" } else { "off" }
                ));
                Response::json(200, &state)
            }
            Err(e) => Response::json(500, &Ack::err(format!("advanced colour: {e:#}"))),
        }
    }

    fn power(&self, from: IpAddr, req: &Request) -> Response {
        let Ok(p) = req.json::<PowerRequest>() else {
            return Response::json(
                400,
                &Ack::err("expected {\"action\": sleep|restart|shutdown}"),
            );
        };
        let cfg = self.cfg.lock().clone();
        if !cfg.power_allowed {
            return Response::json(
                403,
                &Ack::err("the owner of this PC has turned remote power actions off"),
            );
        }
        if cfg.has_creds() {
            Api {
                user: &cfg.sunshine_user,
                pass: &cfg.sunshine_pass,
            }
            .close_app();
        }
        self.log(format!(
            "{} requested by {from}",
            p.action.label().to_lowercase()
        ));
        // Reply first: sleep can suspend the machine before the bytes leave.
        let action = p.action;
        std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(400));
            if let Err(e) = crate::power::perform(action) {
                tracing::error!("{}: {e:#}", action.label());
            }
        });
        Response::json(200, &Ack::ok())
    }
}

impl Default for Service {
    fn default() -> Self {
        Self::new()
    }
}

/// One log line about what `tailscale netcheck` found.
pub fn describe_nat(n: &NatReport) -> String {
    let city = tailscale::derp_city(&n.derp);
    if !n.udp {
        "network: UDP is blocked here, so other machines reach this one only through a Tailscale relay"
            .into()
    } else if n.hard == Some(true) && !n.portmap {
        format!(
            "network: hard NAT with no UPnP; a machine on another network reaches this one through the {city} relay unless the router gets UPnP or a forwarded UDP port"
        )
    } else if n.hard == Some(true) {
        "network: hard NAT, but the router maps ports; direct connections should work".into()
    } else if n.hard == Some(false) {
        format!("network: easy NAT; direct connections should work (nearest relay {city})")
    } else {
        format!("network: NAT type unknown (nearest relay {city})")
    }
}

/// Who sent a request.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Caller {
    /// A process on this machine: its own window, or a script.
    Local,
    /// A tailnet machine, and whether this machine's owner admits it.
    Peer { allowed: bool },
}

impl Caller {
    fn allowed(self) -> bool {
        match self {
            Caller::Local => true,
            Caller::Peer { allowed } => allowed,
        }
    }
}

/// Who is behind a loopback connection. Only with userspace networking
/// (`TUN` false) can it be a tailnet peer; then `whois` of the connection's
/// address says which, and an address Tailscale knows nothing about is a
/// process on this machine. A lookup that fails admits nobody.
fn loopback_caller(
    tun: Option<bool>,
    owner: Option<&Owner>,
    whois: impl FnOnce() -> Result<Option<tailscale::WhoIs>>,
) -> Caller {
    if tun != Some(false) {
        return Caller::Local;
    }
    match whois() {
        Ok(None) => Caller::Local,
        Ok(Some(w)) => Caller::Peer {
            allowed: owner.is_some_and(|o| o.admits(&w)),
        },
        Err(_) => Caller::Peer { allowed: false },
    }
}

/// Whose machine this is, for deciding who may use the control API.
#[derive(Debug, Clone, PartialEq)]
struct Owner {
    /// The Tailscale user this machine is signed in as; for a tagged
    /// machine, Tailscale's shared "tagged-devices" user.
    user: u64,
    /// The machine carries ACL tags, so no person owns it.
    tagged: bool,
    /// `CurrentTailnet.Name`: the owner's login for a personal tailnet,
    /// the domain for an organisation's.
    tailnet: String,
}

impl Owner {
    fn of(st: &tailscale::Status) -> Self {
        Self {
            user: st.self_node.user_id,
            tagged: !st.self_node.tags.is_empty(),
            tailnet: st
                .current_tailnet
                .as_ref()
                .map(|t| t.name.clone())
                .unwrap_or_default(),
        }
    }

    /// The same account may ask. A tagged machine has no account of its
    /// own, so it takes the members of its tailnet: the owner of a
    /// personal tailnet, the users of an organisation's domain. A machine
    /// shared in from another tailnet is neither, even though `whois`
    /// knows it and the tailnet's rules let it reach this port.
    fn admits(&self, peer: &tailscale::WhoIs) -> bool {
        let user = peer.user_profile.id;
        if user == 0 {
            return false;
        }
        if !self.tagged {
            return user == self.user;
        }
        // Every tagged machine is the "tagged-devices" user, and nothing
        // says that user's ID differs between tailnets, so a matching ID
        // proves nothing here: only a person's login does.
        if self.tailnet.is_empty() {
            return false;
        }
        let login = peer.user_profile.login_name.as_str();
        login.eq_ignore_ascii_case(&self.tailnet)
            || login
                .rsplit_once('@')
                .is_some_and(|(_, domain)| domain.eq_ignore_ascii_case(&self.tailnet))
    }
}

/// Whether the NAT itself changed. The nearest relay region moves between
/// runs on its own (Paris, then Chicago, then Paris again on the same
/// network), and logging each move pushed useful lines out of the 80 the
/// window shows.
fn nat_changed(old: &NatReport, new: &NatReport) -> bool {
    (old.udp, old.ipv4, old.ipv6, old.hard, old.portmap)
        != (new.udp, new.ipv4, new.ipv6, new.hard, new.portmap)
}

/// True when a service answers on loopback.
pub fn service_alive() -> bool {
    http::request(
        ("127.0.0.1", CONTROL_PORT),
        "GET",
        "/v1/status",
        None,
        Duration::from_millis(600),
    )
    .map(|r| r.status == 200)
    .unwrap_or(false)
}

/// Whether opening the window starts the background service. A PC must
/// stay reachable, so on Windows and Linux it always does. A Mac does only
/// while it is shared ("Share this Mac", stored as `start_with_windows`):
/// off, it is a viewer, and nothing of Latch outlives its window.
pub fn starts_with_window(cfg: &HostConfig) -> bool {
    starts_with_window_on(cfg!(target_os = "macos"), cfg)
}

fn starts_with_window_on(mac: bool, cfg: &HostConfig) -> bool {
    !mac || cfg.start_with_windows
}

/// Start `--background` if nothing answers on loopback.
pub fn ensure_service_running() {
    if service_alive() {
        return;
    }
    let Ok(exe) = std::env::current_exe() else {
        return;
    };
    let mut c = std::process::Command::new(exe);
    c.arg("--background")
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        c.creation_flags(0x0000_0008 | 0x0800_0000); // DETACHED_PROCESS | CREATE_NO_WINDOW
    }
    match c.spawn() {
        Ok(mut child) => {
            std::thread::spawn(move || {
                let _ = child.wait();
            });
            tracing::info!("started the background service");
        }
        Err(e) => tracing::error!("could not start the background service: {e}"),
    }
}

/// The `Content-Type` Latch's own callers send: JSON, or the executable
/// on the update route. A browser cannot send either cross-origin without
/// a CORS preflight, so this keeps web pages from driving the service.
fn sent_by_latch(req: &Request) -> bool {
    let ct = req
        .header("content-type")
        .unwrap_or("")
        .trim()
        .to_ascii_lowercase();
    matches!(
        ct.split(';').next().unwrap_or("").trim(),
        "application/json" | "application/octet-stream"
    )
}

/// Native callers use numeric addresses (the Windows installer also uses
/// localhost). A page hosted on an attacker-controlled name must never be
/// able to rebind that name to the control service.
fn control_host(host: &str) -> bool {
    if host.eq_ignore_ascii_case(&format!("localhost:{CONTROL_PORT}")) {
        return true;
    }
    host.parse::<SocketAddr>().is_ok_and(|addr| {
        addr.port() == CONTROL_PORT && (addr.ip().is_loopback() || is_tailnet_ip(addr.ip()))
    })
}

/// One line in the service log the window shows (and in the log file).
fn push_log(log: &Mutex<VecDeque<String>>, msg: String) {
    tracing::info!("{msg}");
    let mut log = log.lock();
    if log.len() >= LOG_LINES {
        log.pop_front();
    }
    log.push_back(msg);
}
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_a_mac_that_is_not_shared_opens_without_the_service() {
        let shared = HostConfig::default();
        let viewer = HostConfig {
            start_with_windows: false,
            ..Default::default()
        };
        assert!(starts_with_window_on(true, &shared));
        assert!(!starts_with_window_on(true, &viewer));
        // A PC nobody can reach in person keeps its service either way.
        assert!(starts_with_window_on(false, &viewer));
        assert!(starts_with_window_on(false, &shared));
    }

    fn whois(id: u64, login: &str) -> tailscale::WhoIs {
        tailscale::WhoIs {
            user_profile: tailscale::User {
                id,
                login_name: login.into(),
                ..Default::default()
            },
            ..Default::default()
        }
    }

    /// The shapes of a real tailnet: a PC and a Mac signed in as one
    /// person, a relay VPS tagged `tag:relay`, and a friend's machine
    /// shared in from their own tailnet.
    #[test]
    fn only_the_account_or_for_a_tagged_machine_its_tailnet_may_ask() {
        const ADA: u64 = 21;
        const TAGGED: u64 = 18;
        let status = |user: u64, tags: &str| {
            tailscale::parse_status(&format!(
                r#"{{"BackendState":"Running","Self":{{"UserID":{user},"Tags":[{tags}]}},
                    "CurrentTailnet":{{"Name":"ada@example.com"}}}}"#
            ))
            .unwrap()
        };
        let mac = whois(ADA, "ada@example.com");
        let relay = whois(TAGGED, "tagged-devices");
        let friend = whois(77, "bob@example.net");

        let pc = Owner::of(&status(ADA, ""));
        assert!(!pc.tagged);
        assert!(pc.admits(&mac), "the owner's Mac");
        assert!(
            !pc.admits(&friend),
            "a shared-in machine may not pair, sleep or update the PC"
        );
        assert!(
            !pc.admits(&relay),
            "an internet-facing relay may not push the PC an executable"
        );
        assert!(!pc.admits(&whois(0, "")), "whois without a user");

        let vps = Owner::of(&status(TAGGED, r#""tag:relay""#));
        assert!(vps.tagged);
        assert!(vps.admits(&mac), "a tagged VPS serves its tailnet's owner");
        assert!(
            !vps.admits(&relay),
            "a tagged ID is no account: it may be a tagged machine anywhere"
        );
        assert!(!vps.admits(&friend));

        // An organisation's tailnet is named after its domain.
        let org = Owner {
            tailnet: "example.com".into(),
            ..vps.clone()
        };
        assert!(org.admits(&whois(5, "ada@EXAMPLE.com")));
        assert!(!org.admits(&whois(6, "eve@example.com.evil")));
        // Without a tailnet name a tagged machine takes nobody.
        let unnamed = Owner {
            tailnet: String::new(),
            ..vps
        };
        assert!(!unnamed.admits(&mac));
        assert!(!unnamed.admits(&relay));
    }

    /// A Docker node's tailscaled in userspace mode relays every tailnet
    /// peer from 127.0.0.1: only `whois` of the port tells them apart from
    /// the node's own processes.
    #[test]
    fn a_peer_relayed_through_loopback_is_judged_like_any_other() {
        let owner = Owner {
            user: 21,
            tagged: false,
            tailnet: "ada@example.com".into(),
        };
        let never = || -> Result<Option<tailscale::WhoIs>> { panic!("no whois needed") };
        // A kernel TUN, or a Tailscale that does not say: loopback is local.
        assert_eq!(
            loopback_caller(Some(true), Some(&owner), never),
            Caller::Local
        );
        assert_eq!(loopback_caller(None, Some(&owner), never), Caller::Local);
        // Userspace: whoever whois finds behind the port.
        let found = |id, login: &str| {
            let w = whois(id, login);
            move || Ok(Some(w))
        };
        assert_eq!(
            loopback_caller(Some(false), Some(&owner), found(21, "ada@example.com")),
            Caller::Peer { allowed: true },
            "the owner's Mac may ask, but is not the local window"
        );
        assert_eq!(
            loopback_caller(Some(false), Some(&owner), found(77, "bob@example.net")),
            Caller::Peer { allowed: false }
        );
        assert_eq!(
            loopback_caller(Some(false), None, found(21, "ada@example.com")),
            Caller::Peer { allowed: false },
            "nobody to compare with while Tailscale is down"
        );
        assert_eq!(
            loopback_caller(Some(false), Some(&owner), || Ok(None)),
            Caller::Local,
            "no peer behind the port: a process on this machine"
        );
        assert_eq!(
            loopback_caller(Some(false), Some(&owner), || anyhow::bail!("timed out")),
            Caller::Peer { allowed: false },
            "a failed lookup admits nobody"
        );
        assert!(!Caller::Peer { allowed: false }.allowed());
        assert!(Caller::Local.allowed());

        // The window keeps working, and loopback never enters the cache.
        let svc = Service::new();
        let req = Request {
            method: "GET".into(),
            path: "/v1/status".into(),
            ..Default::default()
        };
        assert_eq!(svc.handle("127.0.0.1:5".parse().unwrap(), &req).status, 200);
        assert!(svc.auth.lock().is_empty());
    }

    #[test]
    fn browsers_and_rebound_hostnames_cannot_read_or_write_the_api() {
        let svc = Service::new();
        let local = "127.0.0.1:5".parse().unwrap();
        for header in [
            ("origin", "http://example.com"),
            ("sec-fetch-site", "same-origin"),
            ("host", "attacker.example:47850"),
            ("host", "127.0.0.1:80"),
        ] {
            let req = Request {
                method: "GET".into(),
                path: CLIPBOARD_PATH.into(),
                headers: vec![(header.0.into(), header.1.into())],
                ..Default::default()
            };
            assert_eq!(svc.handle(local, &req).status, 403);
        }
        for host in [
            "127.0.0.1:47850",
            "localhost:47850",
            "100.64.0.10:47850",
            "[::1]:47850",
            "[fd7a:115c:a1e0::1]:47850",
        ] {
            assert!(control_host(host), "{host}");
        }
        for host in ["8.8.8.8:47850", "[2001:db8::1]:47850", "100.64.0.10:80"] {
            assert!(!control_host(host), "{host}");
        }
        let req = Request {
            headers: vec![("content-type".into(), "application/json-not-really".into())],
            ..Default::default()
        };
        assert!(!sent_by_latch(&req));
    }

    #[test]
    fn updates_are_serialized_and_a_rejected_upload_releases_the_slot() {
        let svc = Service::new();
        let req = Request::default();
        svc.update_running.store(true, Ordering::Release);
        if cfg!(windows) {
            assert_eq!(svc.update(LOOPBACK, &req).status, 409);
        } else {
            // Not a Windows PC: refused before the slot is even looked at.
            let r = svc.update(LOOPBACK, &req);
            assert_eq!(r.status, 400);
            assert!(r.body.contains("Windows PC"), "{}", r.body);
        }
        svc.update_running.store(false, Ordering::Release);
        assert_eq!(svc.update(LOOPBACK, &req).status, 400);
        assert!(!svc.update_running.load(Ordering::Acquire));
    }

    #[test]
    fn wake_evidence_requires_a_known_matching_adapter() {
        let svc = Service::new();
        let mac = latch_core::wake::MacAddr::parse("02:00:00:00:00:01").unwrap();
        let from = "192.168.1.2:9".parse().unwrap();
        svc.wake_packet(mac, from);
        assert!(svc.wake_seen.lock().is_none());
        svc.wake.lock().mac = Some(mac.to_string());
        svc.wake_packet(mac, from);
        assert!(svc.wake_seen.lock().is_some());
    }

    #[test]
    fn strangers_get_403_and_loopback_gets_status() {
        let svc = Service::new();
        let req = Request {
            method: "GET".into(),
            path: "/v1/status".into(),
            ..Default::default()
        };
        let r = svc.handle("192.168.1.11:5".parse().unwrap(), &req);
        assert_eq!(r.status, 403);
        // A tailnet address with Tailscale down is refused too: nobody can vouch for it.
        let r = svc.handle("100.64.0.30:5".parse().unwrap(), &req);
        assert_eq!(r.status, 403);
        let r = svc.handle("127.0.0.1:5".parse().unwrap(), &req);
        assert_eq!(r.status, 200);
        let st: Status = r.parse().unwrap();
        assert_eq!(st.app, "latch");
        assert!(!st.name.is_empty());
    }

    #[test]
    fn pin_and_power_validate_their_bodies() {
        let svc = Service::new();
        let local: SocketAddr = "127.0.0.1:5".parse().unwrap();
        let post = |path: &str, body: &str| Request {
            method: "POST".into(),
            path: path.into(),
            headers: json_header(),
            body: body.into(),
        };
        assert_eq!(svc.handle(local, &post("/v1/pin", "{}")).status, 400);
        assert_eq!(
            svc.handle(local, &post("/v1/pin", r#"{"pin":"12","name":"m"}"#))
                .status,
            400
        );
        // Valid shape, but no Sunshine login on this box: a 502, not a crash.
        assert_eq!(
            svc.handle(local, &post("/v1/pin", r#"{"pin":"1234","name":"m"}"#))
                .status,
            502
        );
        assert_eq!(
            svc.handle(local, &post("/v1/power", r#"{"action":"nap"}"#))
                .status,
            400
        );
        assert_eq!(svc.handle(local, &post("/v1/nope", "")).status, 404);
        // An update without its headers is refused before anything is written.
        let r = svc.handle(local, &post(UPDATE_PATH, "MZ"));
        assert_eq!(r.status, 400, "{}", r.body);
        let r = svc.handle(
            local,
            &Request {
                method: "POST".into(),
                path: UPDATE_PATH.into(),
                headers: vec![
                    ("x-latch-version".into(), "0.0.1".into()),
                    ("content-type".into(), "application/octet-stream".into()),
                ],
                body: b"MZ".to_vec(),
            },
        );
        // An older version is refused on Windows; any version is elsewhere.
        assert_eq!(
            r.status,
            if cfg!(windows) { 409 } else { 400 },
            "{}",
            r.body
        );
    }

    const LOOPBACK: IpAddr = IpAddr::V4(Ipv4Addr::LOCALHOST);

    fn json_header() -> Vec<(String, String)> {
        vec![("content-type".into(), "application/json".into())]
    }

    #[test]
    fn a_web_page_cannot_post_from_an_authorised_machine() {
        // A cross-origin form post from a browser on this PC arrives from
        // loopback with a form or text content type and no preflight. It
        // must not sleep the PC, stop the service or touch the clipboard.
        let svc = Service::new();
        let local: SocketAddr = "127.0.0.1:5".parse().unwrap();
        for ct in [
            None,
            Some("text/plain"),
            Some("application/x-www-form-urlencoded"),
            Some("multipart/form-data; boundary=x"),
        ] {
            for path in ["/v1/power", "/v1/quit", CLIPBOARD_PATH, UPDATE_PATH] {
                let mut headers = Vec::new();
                if let Some(ct) = ct {
                    headers.push(("content-type".to_string(), ct.to_string()));
                }
                let r = svc.handle(
                    local,
                    &Request {
                        method: "POST".into(),
                        path: path.into(),
                        headers,
                        body: br#"{"action":"sleep","text":"x"}"#.to_vec(),
                    },
                );
                assert_eq!(r.status, 403, "{path} with {ct:?}: {}", r.body);
            }
        }
        // Latch's own callers are unaffected, whatever the case of the header.
        let r = svc.handle(
            local,
            &Request {
                method: "POST".into(),
                path: "/v1/power".into(),
                headers: vec![(
                    "content-type".into(),
                    "Application/JSON; charset=utf-8".into(),
                )],
                body: br#"{"action":"nap"}"#.to_vec(),
            },
        );
        assert_eq!(r.status, 400, "{}", r.body);
        // GETs carry no side effect and stay open to the panel.
        let r = svc.handle(
            local,
            &Request {
                method: "GET".into(),
                path: "/v1/status".into(),
                ..Default::default()
            },
        );
        assert_eq!(r.status, 200);
    }

    #[test]
    fn a_new_nearest_relay_is_not_a_new_nat() {
        let paris = NatReport {
            udp: true,
            hard: Some(true),
            derp: "par".into(),
            ..Default::default()
        };
        let chicago = NatReport {
            derp: "ord".into(),
            ..paris.clone()
        };
        assert!(!nat_changed(&paris, &chicago));
        let mapped = NatReport {
            portmap: true,
            ..paris.clone()
        };
        assert!(nat_changed(&paris, &mapped));
    }

    #[test]
    fn the_nat_line_names_the_problem() {
        let hard = NatReport {
            udp: true,
            hard: Some(true),
            portmap: false,
            derp: "tok".into(),
            ..Default::default()
        };
        let t = describe_nat(&hard);
        assert!(
            t.contains("hard NAT with no UPnP") && t.contains("Tokyo"),
            "{t}"
        );
        let mapped = NatReport {
            portmap: true,
            ..hard.clone()
        };
        assert!(describe_nat(&mapped).contains("maps ports"));
        let easy = NatReport {
            udp: true,
            hard: Some(false),
            ..Default::default()
        };
        assert!(describe_nat(&easy).contains("easy NAT"));
        let blocked = NatReport::default();
        assert!(describe_nat(&blocked).contains("UDP is blocked"));
        // The clipboard routes exist and answer JSON, whatever the platform
        // says about the clipboard itself.
        let svc = Service::new();
        let local: SocketAddr = "127.0.0.1:5".parse().unwrap();
        let r = svc.handle(
            local,
            &Request {
                method: "POST".into(),
                path: CLIPBOARD_PATH.into(),
                headers: json_header(),
                body: b"not json".to_vec(),
            },
        );
        assert_eq!(r.status, 400, "{}", r.body);
        let r = svc.handle(
            local,
            &Request {
                method: "GET".into(),
                path: CLIPBOARD_PATH.into(),
                ..Default::default()
            },
        );
        assert!(r.status == 200 || r.status == 500, "{}", r.body);
    }

    #[test]
    fn power_is_refused_when_the_owner_says_so() {
        let svc = Service::new();
        svc.cfg.lock().power_allowed = false;
        let r = svc.handle(
            "127.0.0.1:5".parse().unwrap(),
            &Request {
                method: "POST".into(),
                path: "/v1/power".into(),
                headers: json_header(),
                body: r#"{"action":"sleep"}"#.into(),
            },
        );
        assert_eq!(r.status, 403);
    }
}
