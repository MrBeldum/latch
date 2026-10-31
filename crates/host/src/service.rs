//! The background control service: what the other machines on the tailnet talk to.
//!
//! One TCP listener on every interface, one rule for who gets an answer:
//! loopback (the host's own control panel), or a tailnet machine that
//! `tailscale whois` says belongs to the account this one is signed in as.
//! A tagged machine (the relay VPS) belongs to no account, so it answers
//! the members of its own tailnet. Everyone else gets a 403 and nothing
//! more: a machine shared in from someone else's tailnet can reach the
//! port, but may not stream, sleep the PC, read its clipboard or push it an
//! executable.
//!
//! Under userspace Tailscale (a container's tailscaled) every tailnet peer
//! arrives from loopback too, so there loopback alone proves nothing:
//! `tailscale whois` of the connection's own port says who is behind it.

use crate::clipboard;
use crate::config::HostConfig;
use crate::streamer;
use crate::update;
use crate::wake::{self, WakeInfo};
use anyhow::{Context, Result};
use latch_core::api::{
    Ack, Clipboard, DisplayRequest, NatReport, PowerRequest, Status, Streamer, CLIPBOARD_PATH,
    UPDATE_PATH,
};
use latch_core::http::{self, Request, Response};
use latch_core::tailscale::{self, is_tailnet_ip};
use latch_core::CONTROL_PORT;
use latch_engine::server::{Options, Server};
use latch_proto::control::{self, end, StreamRequest};
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
/// Ticks (of 5 s) between runs of `tailscale netcheck`: it takes seconds
/// and the network does not move often.
const NETCHECK_TICKS: u64 = 120;
/// The tick (of 5 s) at which a service lets go of what an update retired:
/// ten minutes in, long after the update's watcher has finished.
const TIDY_TICK: u64 = 120;

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
    server: Mutex<Option<Arc<Server>>>,
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
    /// Why host.toml could not be re-read, while it cannot.
    cfg_problem: Mutex<Option<String>>,
    /// A reinstall was noticed and handing over to it failed.
    reinstall_tried: AtomicBool,
    /// The machine is being kept from idle-sleeping (see `power::keep_awake`).
    kept_awake: AtomicBool,
    /// The macOS firewall turns away connections to this program.
    firewall_blocked: AtomicBool,
    /// macOS has the login item turned off (see `unix_setup`).
    login_item_off: AtomicBool,
    /// This PC's side of the NAT story, from `tailscale netcheck`.
    nat: Mutex<Option<NatReport>>,
    nat_running: AtomicBool,
    update_running: Arc<AtomicBool>,
    admission: Mutex<()>,
    power_pending: Arc<AtomicBool>,
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
            server: Mutex::new(None),
            log: Arc::new(Mutex::new(VecDeque::new())),
            auth: Mutex::new(HashMap::new()),
            whois_failed: Mutex::new(None),
            tun: Mutex::new(None),
            forwarded_logged: Mutex::new(None),
            cfg_problem: Mutex::new(None),
            reinstall_tried: AtomicBool::new(false),
            kept_awake: AtomicBool::new(false),
            firewall_blocked: AtomicBool::new(false),
            login_item_off: AtomicBool::new(false),
            nat: Mutex::new(None),
            nat_running: AtomicBool::new(false),
            update_running: Arc::new(AtomicBool::new(false)),
            admission: Mutex::new(()),
            power_pending: Arc::new(AtomicBool::new(false)),
        }
    }

    pub fn log(&self, msg: impl Into<String>) {
        push_log(&self.log, msg.into());
    }

    fn someone_watching(&self) -> bool {
        self.server.lock().as_ref().is_some_and(|s| s.busy())
    }

    fn stop_stream(&self) -> bool {
        let server = self.server.lock().clone();
        server.is_none_or(|server| server.stop(end::HOST_STOPPING, "This host is stopping."))
    }

    fn streaming_status(&self) -> Streamer {
        let server = self.server.lock();
        let status = server.as_ref().map(|s| s.status()).unwrap_or_default();
        let encoder = if status.encoder.is_empty() {
            streamer::encoder_description().to_string()
        } else {
            status.encoder
        };
        let cores = std::thread::available_parallelism().map_or(1, |n| n.get());
        Streamer {
            kind: latch_core::STREAMER_KIND.into(),
            installed: true,
            running: server.is_some(),
            api_ok: true,
            max_fps: streamer::max_fps(&encoder, cores),
            encoder,
            audio_problem: status.audio_problem,
            requested_bitrate_kbps: status.requested_bitrate_kbps,
            applied_bitrate_kbps: status.applied_bitrate_kbps,
            pacing_queue_ms: status.pacing_queue_ms,
            send_errors: status.send_errors,
            send_retries: status.send_retries,
            socket_send_bytes: status.socket_send_bytes,
            socket_receive_bytes: status.socket_receive_bytes,
            capture_size: streamer::capture_size(),
            viewer: status.viewer.is_some(),
            protocol: latch_proto::PROTOCOL,
        }
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
        if let Err(e) = crate::prep::migrate() {
            self.log(format!("migrating legacy configuration: {e:#}"));
        }
        *self.cfg.lock() = HostConfig::load();
        let owner = Arc::downgrade(&self);
        let log = self.log.clone();
        *self.server.lock() = Some(Server::bind(
            CONTROL_PORT,
            Options {
                prepare: Some(Box::new(move |req| {
                    let service = owner.upgrade().context("the host is stopping")?;
                    let cfg = service.cfg.lock().clone();
                    crate::prep::prepare(&cfg, req)
                })),
                log: Box::new(move |line| push_log(&log, line)),
            },
        )?);
        self.log(format!(
            "Latch Host {} listening on TCP and UDP {CONTROL_PORT}",
            env!("CARGO_PKG_VERSION")
        ));
        if let Ok(exe) = std::env::current_exe() {
            if replacing {
                self.log("updated: the previous version has been replaced");
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
        // Only a Windows PC learns its wake MAC (see `wake::probe`), and
        // without one every packet is ignored: elsewhere the listener only
        // held UDP 9 on every interface (a Mac) or failed to bind it and
        // said so at every start (a Linux user service).
        if wants_wake_listener() {
            let svc = self.clone();
            if let Err(e) = wake::listen(move |mac, from| svc.wake_packet(mac, from)) {
                self.log(format!("not listening for wake packets: {e:#}"));
            }
        }
        let handler = self.clone();
        let authorizer = self.clone();
        let body_authorizer = self.clone();
        http::serve_with_auth(
            listener,
            move |peer| authorizer.authorized(peer.ip()),
            move |peer, _path, _headers, _len| body_authorizer.caller(peer).allowed(),
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
        let prev = self.cfg.lock().clone();
        let (cfg, line) = next_config(prev, HostConfig::try_load(), &mut self.cfg_problem.lock());
        *self.cfg.lock() = cfg;
        if let Some(line) = line {
            self.log(line);
        }
        let stay_awake = self.cfg.lock().stay_awake;
        let awake = crate::power::keep_awake(stay_awake);
        // Said when it starts and stops being true, and only where it is:
        // a Linux server, or a laptop on battery, is not being kept awake.
        if awake != self.kept_awake.swap(awake, Ordering::AcqRel) {
            self.log(if awake {
                format!(
                    "keeping this {} awake while it is plugged in, so Tailscale stays reachable",
                    if cfg!(target_os = "macos") {
                        "Mac"
                    } else {
                        "PC"
                    }
                )
            } else {
                "no longer keeping this machine awake (on battery, or the setting is off)".into()
            });
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

        if !cfg!(windows) && tick % 12 == 11 {
            self.follow_reinstall();
        }
        #[cfg(target_os = "macos")]
        if tick.is_multiple_of(12) {
            if let Ok(exe) = std::env::current_exe() {
                let blocked = crate::unix_setup::firewall_blocks(&exe);
                if blocked && !self.firewall_blocked.swap(blocked, Ordering::AcqRel) {
                    self.log("the macOS firewall blocks incoming connections to Latch, so other machines cannot reach this Mac; run setup here");
                }
                self.firewall_blocked.store(blocked, Ordering::Release);
            }
            let off = crate::unix_setup::login_item_disabled();
            if off && !self.login_item_off.swap(off, Ordering::AcqRel) {
                self.log("macOS has Latch's login item turned off, so this Mac is not shared after a restart; run setup here");
            }
            self.login_item_off.store(off, Ordering::Release);
        }
        // What an update retired is kept until the new version has served
        // for a while: the update's watcher puts it back if it does not.
        if tick == TIDY_TICK {
            if let Ok(exe) = std::env::current_exe() {
                update::tidy(&exe);
            }
        }
    }

    /// A Mac or Linux install replaced under a running service (a new
    /// Latch.app copied over, a package upgrade) keeps serving the old code
    /// until something restarts it, and the window cannot tell when the
    /// version did not change. Restart into the new program once nobody
    /// watches. (Windows swaps its executable through `update`.)
    fn follow_reinstall(self: &Arc<Self>) {
        let Some(stamp) = started_from().filter(|s| s.replaced()) else {
            return;
        };
        if self.reinstall_tried.load(Ordering::Acquire) || self.someone_watching() {
            return;
        }
        #[cfg(not(windows))]
        if crate::unix_setup::supervised() {
            // launchd and systemd restart a service that failed, from the
            // path the new program is at.
            self.log("Latch was reinstalled; the service restarts into the new copy");
            self.stop_stream();
            std::process::exit(75);
        }
        // Once: a new copy that cannot start is not retried every minute.
        self.reinstall_tried.store(true, Ordering::Release);
        self.log(format!(
            "Latch was reinstalled at {}; restart it locally to use the new copy (no service supervisor is running)",
            stamp.path().display()
        ));
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
        let streamer = self.streaming_status();
        let mut setup = Vec::new();
        if let Err(e) = &*ts {
            setup.push(format!("Tailscale: {e}. Install it and sign in."));
        }
        #[cfg(target_os = "macos")]
        setup.extend(crate::unix_setup::permission_items());
        if wake.magic_packet == Some(false) {
            setup.push(format!("Wake-on-LAN is off on {}.", wake.adapter));
        }
        if wake.fast_startup == Some(true) {
            setup.push("Fast Startup is on, so the PC cannot be woken after a shutdown.".into());
        }
        if self.firewall_blocked.load(Ordering::Acquire) {
            setup.push("The macOS firewall blocks other machines from reaching Latch.".into());
        }
        if self.login_item_off.load(Ordering::Acquire) {
            setup.push(
                "macOS has Latch's login item turned off, so sharing stops at the next restart."
                    .into(),
            );
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

    /// One of this machine's own Tailscale addresses: a connection from it
    /// is a process on this machine, as loopback is.
    fn own_address(&self, ip: IpAddr) -> bool {
        self.tailscale.lock().as_ref().map_or(true, |st| {
            st.self_node.tailscale_ips.is_empty()
                || st
                    .self_node
                    .tailscale_ips
                    .iter()
                    .any(|a| a.parse::<IpAddr>() == Ok(ip))
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
        // Loopback, or this machine's own tailnet address, is any process
        // here, signed in as any OS user. An executable pushed from there
        // would run as the account that owns this service, and the
        // clipboard is that account's: both are for the other machines.
        if (req.path == UPDATE_PATH || req.path == CLIPBOARD_PATH)
            && (local || self.own_address(peer.ip()))
        {
            return Response::json(
                403,
                &Ack::err("updates and the clipboard are for the other machines on this Tailscale account"),
            );
        }
        match (req.method.as_str(), req.path.as_str()) {
            ("GET", "/v1/status") => Response::json(200, &self.status(local)),
            ("GET", "/v1/display") => Response::json(200, &self.display()),
            ("POST", "/v1/display") => self.set_display(peer.ip(), req),
            ("POST", p) if p == control::PATH => {
                // Connecting to our own tailnet address does not establish
                // which OS account is asking to control this desktop.
                if local || self.own_address(peer.ip()) {
                    Response::json(
                        403,
                        &Ack::err("streaming sessions must come from another authorized Tailscale machine"),
                    )
                } else {
                    self.stream(peer, req)
                }
            }
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
                if !self.stop_stream() {
                    return Response::json(409, &Ack::err("stream cleanup is still running"));
                }
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
                    "only a Windows PC takes a pushed latch-host.exe; a Mac updates with its app, and a Linux machine installs the release's latch-host-linux tarball (deploy/native/README.md)",
                ),
            );
        }
        if !self.cfg.lock().accept_updates {
            self.log(format!(
                "update from {from} refused: this PC takes no pushed updates (accept_updates = false in host.toml)"
            ));
            return Response::json(
                403,
                &Ack::err(
                    "the owner of this PC has turned pushed updates off; update it at the PC",
                ),
            );
        }
        let _admission = self.admission.lock();
        if self.update_running.swap(true, Ordering::AcqRel) {
            return Response::json(409, &Ack::err("an update is already being installed"));
        }
        let engine_running = self.server.lock().is_some();
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
                let sha = req
                    .header(latch_core::api::UPDATE_SHA256_HEADER)
                    .or_else(|| req.header(latch_core::legacy::UPDATE_SHA256_HEADER))
                    .unwrap_or("")
                    .trim()
                    .to_ascii_lowercase();
                self.log(format!(
                    "updating to {version}: {from} sent the new Latch Host (SHA-256 {sha})"
                ));
                let server = self.server.lock().clone();
                let running = self.update_running.clone();
                let log = self.log.clone();
                std::thread::spawn(move || {
                    std::thread::sleep(Duration::from_millis(500));
                    match update::apply(&exe) {
                        Ok(h) => {
                            if let Err(e) = update::start_probation(&exe, &h, &version.to_string())
                            {
                                tracing::warn!("{e:#}");
                            }
                            if let Some(server) = server {
                                server.stop(end::HOST_STOPPING, "This host is updating.");
                            }
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
                self.log(format!("update from {from} refused: {}", r.message()));
                Response::json(r.status(), &Ack::err(r.message()))
            }
        }
    }

    fn stream(&self, peer: SocketAddr, req: &Request) -> Response {
        let _admission = self.admission.lock();
        if self.update_running.load(Ordering::Acquire) || self.power_pending.load(Ordering::Acquire)
        {
            return Response::json(409, &Ack::err("the host is updating or stopping"));
        }
        if req.header("upgrade") != Some(control::UPGRADE) {
            return Response::json(400, &Ack::err("expected the Latch stream Upgrade header"));
        }
        let request = match req.json::<StreamRequest>() {
            Ok(request) => request,
            Err(_) => return Response::json(400, &Ack::err("expected a stream request")),
        };
        let Some(server) = self.server.lock().clone() else {
            return Response::json(503, &Ack::err("the stream server is not listening"));
        };
        let peer = if peer.ip().is_loopback() {
            media_peer(peer, tailscale::whois_addr(peer).ok().flatten())
        } else {
            peer
        };
        match server.prepare(request, peer) {
            Ok(prepared) => {
                Response::switching(control::UPGRADE, move |tcp| server.run(prepared, tcp))
            }
            Err(refusal) => Response::json(refusal.status, &Ack::err(refusal.message)),
        }
    }

    /// Everything that decides whether a capture can see the desktop.
    fn display(&self) -> serde_json::Value {
        let windows =
            crate::display::probe().unwrap_or_else(|e| serde_json::json!({"error": e.to_string()}));
        let color = crate::display::advanced_color()
            .unwrap_or_else(|e| serde_json::json!({"error": e.to_string()}));
        serde_json::json!({
            "windows": windows,
            "advanced_color": color,
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
        let action = p.action;
        let what = action.label().to_lowercase();
        // Refused before the stream is ended for it, and with the reason:
        // the Mac shows an answer that is not ok as it is. (Every Mac
        // client reads a 200's `ok`; another status only as "HTTP 409".)
        let name = self.name.lock().clone();
        let refused = crate::power::refusal(crate::power::kind(), action, &name).or_else(|| {
            crate::power::logind_refuses(action)
                .map(|answer| format!("this machine's login manager says {answer} to {what}"))
        });
        if let Some(why) = refused {
            self.log(format!("{what} requested by {from}, refused: {why}"));
            return Response::json(200, &Ack::err(why));
        }
        let _admission = self.admission.lock();
        if self.power_pending.swap(true, Ordering::AcqRel) {
            return Response::json(409, &Ack::err("a power action is already pending"));
        }
        self.log(format!("{what} requested by {from}"));
        let server = self.server.lock().clone();
        let log = self.log.clone();
        let pending = self.power_pending.clone();
        std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(400));
            if let Some(server) = server {
                if !server.stop(end::HOST_STOPPING, "This host is stopping.") {
                    push_log(
                        &log,
                        "power action cancelled: stream cleanup did not finish".into(),
                    );
                    pending.store(false, Ordering::Release);
                    return;
                }
            }
            if let Err(e) = crate::power::perform(action) {
                push_log(&log, format!("{what} failed: {e:#}"));
            }
            pending.store(false, Ordering::Release);
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

/// The settings to run on after re-reading host.toml: the file's when it
/// reads, else the last good ones, never defaults. A hand edit with a typo
/// (or a read mid-save) used to move the file aside and run with remote
/// power allowed and default settings until someone noticed. Returns the
/// line to log: once per problem, and once when it is fixed.
fn next_config(
    prev: HostConfig,
    loaded: Result<HostConfig>,
    problem: &mut Option<String>,
) -> (HostConfig, Option<String>) {
    match loaded {
        Ok(cfg) => {
            let fixed = problem
                .take()
                .map(|_| "host.toml reads again; using its settings".to_string());
            (cfg, fixed)
        }
        Err(e) => {
            let why = format!("{e:#}");
            let line = (problem.as_deref() != Some(why.as_str())).then(|| {
                format!("host.toml could not be read ({why}); keeping the settings from before")
            });
            *problem = Some(why);
            (prev, line)
        }
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

fn media_peer(peer: SocketAddr, who: Option<tailscale::WhoIs>) -> SocketAddr {
    let ip = who.and_then(|w| {
        w.node.addresses.into_iter().find_map(|s| {
            let ip: IpAddr = s.split('/').next()?.parse().ok()?;
            (ip.is_ipv4() && tailscale::is_tailnet_ip(ip)).then_some(ip)
        })
    });
    ip.map_or(peer, |ip| SocketAddr::new(ip, peer.port()))
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
    if tun == Some(true) {
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

/// Whether to listen for wake packets: only where `wake::probe` can find
/// this machine's MAC to match them against, which is Windows.
fn wants_wake_listener() -> bool {
    cfg!(windows)
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

/// Start the background service if nothing answers on loopback. Through
/// launchd or systemd when there is a login item, so it is supervised and
/// there is only ever one; a copy of its own only where there is none (a
/// Windows PC, or a Mac whose owner turned the login item off). A Mac that
/// is not shared starts nothing: the service would only exit again, and a
/// window that kept asking spawned one every four seconds for hours.
/// Returns whether it asked for a start.
pub fn ensure_service_running() -> bool {
    if !starts_with_window(&HostConfig::load()) {
        tracing::info!("not starting the background service: this Mac is not shared");
        return false;
    }
    if service_alive() {
        return false;
    }
    #[cfg(not(windows))]
    match crate::unix_setup::start_supervised() {
        Some(Ok(by)) => {
            tracing::info!("asked {by} to start the background service");
            return true;
        }
        Some(Err(e)) => {
            tracing::warn!("could not start the background service through its login item ({e:#}); starting it directly");
        }
        None => {}
    }
    spawn_service()
}

/// After an update: stop the service that answers (launchd's, or a copy an
/// older window started) and start it again from the file now on disk.
/// The supervisor would not restart it by itself, since /v1/quit exits 0.
pub fn restart_service() {
    let _ = http::request(
        ("127.0.0.1", CONTROL_PORT),
        "POST",
        "/v1/quit",
        None,
        Duration::from_secs(2),
    );
    // Until it has let go of the port: a copy started while it still holds
    // it would exit at once, and launchd would leave it at that.
    let deadline = Instant::now() + Duration::from_secs(5);
    while service_alive() && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(200));
    }
    std::thread::sleep(Duration::from_millis(300));
    ensure_service_running();
}

/// How soon after it started a copy that stops counts as one that could not
/// start at all.
const QUICK_EXIT: Duration = Duration::from_secs(5);

/// Why the last copy this window started stopped at once, for the page.
static QUICK_EXIT_NOTE: Mutex<Option<String>> = Mutex::new(None);

/// The reason the last copy this window started stopped as soon as it
/// started, once.
pub fn take_quick_exit() -> Option<String> {
    QUICK_EXIT_NOTE.lock().take()
}

/// Start a copy of `--background` as a child of this process.
fn spawn_service() -> bool {
    let Ok(exe) = std::env::current_exe() else {
        return false;
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
    let started = Instant::now();
    match c.spawn() {
        Ok(mut child) => {
            let pid = child.id();
            std::thread::spawn(move || {
                // Exit 0 at once is no failure: another copy already
                // answered, or a Mac turned out not to be shared.
                if let Ok(status) = child.wait() {
                    if !status.success() && started.elapsed() < QUICK_EXIT {
                        let why = format!(
                            "the background service stopped as it started ({status}); its log says why"
                        );
                        tracing::warn!("{why}");
                        *QUICK_EXIT_NOTE.lock() = Some(why);
                    }
                }
            });
            tracing::info!("launched the background service (pid {pid})");
            true
        }
        Err(e) => {
            tracing::error!("could not start the background service: {e}");
            false
        }
    }
}

/// The program file this process started from, as it was then: to tell
/// when an update or a reinstall has put another one at that path.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExeStamp {
    path: std::path::PathBuf,
    len: u64,
    modified: Option<std::time::SystemTime>,
}

impl ExeStamp {
    fn of(path: std::path::PathBuf) -> Option<Self> {
        let m = std::fs::metadata(&path).ok()?;
        Some(Self {
            len: m.len(),
            modified: m.modified().ok(),
            path,
        })
    }

    pub fn path(&self) -> &std::path::Path {
        &self.path
    }

    /// Another file sits at the path now: another size or time. `false`
    /// while the path cannot be read (gone, or held by a virus scanner).
    pub fn replaced(&self) -> bool {
        Self::of(self.path.clone())
            .is_some_and(|now| (now.len, now.modified) != (self.len, self.modified))
    }
}

/// This process's [`ExeStamp`], taken the first time it is asked for;
/// `main` asks first thing. The path is kept from then on: on Linux
/// `current_exe` follows the file an update renames away.
pub fn started_from() -> Option<&'static ExeStamp> {
    static STARTED: std::sync::OnceLock<Option<ExeStamp>> = std::sync::OnceLock::new();
    STARTED
        .get_or_init(|| std::env::current_exe().ok().and_then(ExeStamp::of))
        .as_ref()
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
    fn userspace_media_announcement_uses_the_whois_tailnet_address() {
        let peer = "127.0.0.1:1234".parse().unwrap();
        let who = serde_json::from_str(
            r#"{"Node":{"Addresses":["fd7a:115c:a1e0::1/128","100.64.0.3/32"]}}"#,
        )
        .unwrap();
        assert_eq!(
            media_peer(peer, Some(who)),
            "100.64.0.3:1234".parse().unwrap()
        );
        assert_eq!(media_peer(peer, None), peer);
    }

    #[test]
    fn pending_update_and_power_refuse_streams_before_preparation() {
        let svc = Service::new();
        let req = Request::default();
        let peer = "100.64.0.1:1234".parse().unwrap();
        svc.update_running.store(true, Ordering::Release);
        assert_eq!(svc.stream(peer, &req).status, 409);
        svc.update_running.store(false, Ordering::Release);
        svc.power_pending.store(true, Ordering::Release);
        assert_eq!(svc.stream(peer, &req).status, 409);
    }

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
            "a shared-in machine may not stream, sleep or update the PC"
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
        let found = |id, login: &str| {
            let w = whois(id, login);
            move || Ok(Some(w))
        };
        for tun_state in [Some(false), None] {
            assert_eq!(
                loopback_caller(tun_state, Some(&owner), found(21, "ada@example.com")),
                Caller::Peer { allowed: true },
                "the owner's Mac may ask, but is not the local window"
            );
            assert_eq!(
                loopback_caller(tun_state, Some(&owner), found(77, "bob@example.net")),
                Caller::Peer { allowed: false }
            );
            assert_eq!(
                loopback_caller(tun_state, None, found(21, "ada@example.com")),
                Caller::Peer { allowed: false },
                "nobody to compare with while Tailscale is down"
            );
            assert_eq!(
                loopback_caller(tun_state, Some(&owner), || Ok(None)),
                Caller::Local,
                "no peer behind the port: a process on this machine"
            );
            assert_eq!(
                loopback_caller(tun_state, Some(&owner), || anyhow::bail!("timed out")),
                Caller::Peer { allowed: false },
                "a failed lookup admits nobody"
            );
        }
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
    fn a_damaged_host_toml_keeps_the_last_good_settings() {
        let good = HostConfig {
            power_allowed: false,
            ..Default::default()
        };
        let mut problem = None;
        let (cfg, line) = next_config(
            good.clone(),
            Err(anyhow::anyhow!("expected `=`")),
            &mut problem,
        );
        assert_eq!(cfg, good, "power stays off");
        assert!(line.unwrap().contains("expected `=`"));
        // The same problem every 5 s is said once.
        let (cfg, line) = next_config(cfg, Err(anyhow::anyhow!("expected `=`")), &mut problem);
        assert_eq!(cfg, good);
        assert_eq!(line, None);
        // Fixed: the file's settings, and one line to say so.
        let edited = HostConfig {
            power_allowed: true,
            ..good.clone()
        };
        let (cfg, line) = next_config(cfg, Ok(edited.clone()), &mut problem);
        assert_eq!(cfg, edited);
        assert!(line.is_some());
        let (_, line) = next_config(edited.clone(), Ok(edited), &mut problem);
        assert_eq!(line, None);
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
    fn an_owner_can_turn_pushed_updates_off() {
        let svc = Service::new();
        svc.cfg.lock().accept_updates = false;
        let r = svc.update(LOOPBACK, &Request::default());
        if cfg!(windows) {
            assert_eq!(r.status, 403, "{}", r.body);
            assert!(r.body.contains("turned pushed updates off"), "{}", r.body);
        } else {
            assert_eq!(r.status, 400, "only a Windows PC takes pushes at all");
        }
        assert!(!svc.update_running.load(Ordering::Acquire));
    }

    #[test]
    fn only_a_pc_listens_for_wake_packets() {
        assert_eq!(wants_wake_listener(), cfg!(windows));
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
    fn stream_and_power_validate_their_bodies() {
        let svc = Service::new();
        let mut st = tailscale::Status::default();
        st.self_node.tailscale_ips = vec!["100.64.0.99".into()];
        *svc.tailscale.lock() = Ok(st);
        let local: SocketAddr = "127.0.0.1:5".parse().unwrap();
        let peer_addr: SocketAddr = "100.64.0.5:5".parse().unwrap();
        svc.auth
            .lock()
            .insert(peer_addr.ip(), (Instant::now(), true));
        let post = |path: &str, body: &str| Request {
            method: "POST".into(),
            path: path.into(),
            headers: json_header(),
            body: body.into(),
        };
        assert_eq!(svc.handle(local, &post(control::PATH, "{}")).status, 403);
        assert_eq!(
            svc.handle(peer_addr, &post(control::PATH, "{}")).status,
            400
        );
        let mut bad = post(control::PATH, "not json");
        bad.headers
            .push(("upgrade".into(), control::UPGRADE.into()));
        assert_eq!(svc.handle(peer_addr, &bad).status, 400);
        assert_eq!(
            svc.handle("192.168.1.11:5".parse().unwrap(), &bad).status,
            403
        );
        bad.headers.retain(|(key, _)| key != "upgrade");
        bad.headers.push(("upgrade".into(), "other/1".into()));
        assert_eq!(svc.handle(peer_addr, &bad).status, 400);
        assert_eq!(
            svc.handle(local, &post("/v1/power", r#"{"action":"nap"}"#))
                .status,
            400
        );
        assert_eq!(svc.handle(local, &post("/v1/nope", "")).status, 404);
        // An update without its headers is refused before anything is written.
        let r = svc.handle(peer_addr, &post(UPDATE_PATH, "MZ"));
        assert_eq!(r.status, 400, "{}", r.body);
        let r = svc.handle(
            peer_addr,
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

    #[test]
    fn local_accounts_cannot_access_remote_only_routes() {
        // Loopback, and this machine's own tailnet address, can be any OS
        // user signed in here. Updates, clipboard data and desktop control
        // belong to the account that owns this service.
        let svc = Service::new();
        let mut st = tailscale::Status::default();
        st.self_node.tailscale_ips = vec!["100.64.0.5".into(), "fd7a:115c:a1e0::5".into()];
        *svc.tailscale.lock() = Ok(st);
        let own: SocketAddr = "100.64.0.5:5".parse().unwrap();
        let own_v6: SocketAddr = "[fd7a:115c:a1e0::5]:5".parse().unwrap();
        let other: SocketAddr = "100.64.0.6:5".parse().unwrap();
        for ip in [own.ip(), own_v6.ip(), other.ip()] {
            svc.auth.lock().insert(ip, (Instant::now(), true));
        }
        let update = Request {
            method: "POST".into(),
            path: UPDATE_PATH.into(),
            headers: vec![
                ("x-latch-version".into(), "99.0.0".into()),
                ("content-type".into(), "application/octet-stream".into()),
            ],
            body: b"MZ".to_vec(),
        };
        let read = Request {
            method: "GET".into(),
            path: CLIPBOARD_PATH.into(),
            ..Default::default()
        };
        let write = Request {
            method: "POST".into(),
            path: CLIPBOARD_PATH.into(),
            headers: json_header(),
            body: b"not json".to_vec(),
        };
        let stream = Request {
            method: "POST".into(),
            path: control::PATH.into(),
            headers: json_header(),
            body: b"{}".to_vec(),
        };
        for from in [
            "127.0.0.1:5".parse().unwrap(),
            "[::1]:5".parse().unwrap(),
            own,
            own_v6,
        ] {
            for req in [&update, &read, &write, &stream] {
                let r = svc.handle(from, req);
                assert_eq!(r.status, 403, "{} {} from {from}", req.method, req.path);
            }
        }
        // Another machine on the account still gets through to the route.
        assert_ne!(svc.handle(other, &update).status, 403);
        assert_eq!(svc.handle(other, &write).status, 400);
        assert_eq!(svc.handle(other, &stream).status, 400);
        // A cached admission must not bypass the local-address check when
        // the next Tailscale status probe fails.
        *svc.tailscale.lock() = Err("status unavailable".into());
        for req in [&update, &read, &write, &stream] {
            assert_eq!(svc.handle(own, req).status, 403);
        }
        *svc.tailscale.lock() = Ok(tailscale::Status::default());
        assert_eq!(svc.handle(own, &stream).status, 403);
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
            for path in [
                "/v1/power",
                "/v1/quit",
                CLIPBOARD_PATH,
                UPDATE_PATH,
                control::PATH,
            ] {
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
        let mut st = tailscale::Status::default();
        st.self_node.tailscale_ips = vec!["100.64.0.99".into()];
        *svc.tailscale.lock() = Ok(st);
        let peer: SocketAddr = "100.64.0.5:5".parse().unwrap();
        svc.auth.lock().insert(peer.ip(), (Instant::now(), true));
        let r = svc.handle(
            peer,
            &Request {
                method: "POST".into(),
                path: CLIPBOARD_PATH.into(),
                headers: json_header(),
                body: b"not json".to_vec(),
            },
        );
        assert_eq!(r.status, 400, "{}", r.body);
        let r = svc.handle(
            peer,
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

    #[test]
    fn stream_route_refuses_plain_local_and_admits_another_tailnet_peer() {
        let svc = Service::new();
        let mut status = tailscale::Status::default();
        status.self_node.tailscale_ips = vec!["100.64.0.99".into()];
        *svc.tailscale.lock() = Ok(status);
        let local: SocketAddr = "127.0.0.1:5".parse().unwrap();
        let req = Request {
            method: "POST".into(),
            path: control::PATH.into(),
            headers: vec![("content-type".into(), "application/json".into())],
            body: b"{}".to_vec(),
        };

        // Plain local connection is refused with 403 Forbidden
        let r = svc.handle(local, &req);
        assert_eq!(
            r.status, 403,
            "plain local caller must not start stream: {}",
            r.body
        );

        // Unauthorized tailnet peer is refused with 403 Forbidden
        let peer: SocketAddr = "100.64.0.10:5".parse().unwrap();
        let r = svc.handle(peer, &req);
        assert_eq!(
            r.status, 403,
            "unauthorized tailnet peer must not start stream"
        );

        // Authorized tailnet peer is admitted (passes caller check and reaches body validation -> 400)
        svc.auth.lock().insert(peer.ip(), (Instant::now(), true));
        let r = svc.handle(peer, &req);
        assert_eq!(
            r.status, 400,
            "authorized tailnet peer is admitted: {}",
            r.body
        );
    }
}
